/* screencast: zkde_screencast_unstable_v1 -> PipeWire node -> libpipewire
 * consumer connected with pw_context_connect_fd (production mirror).
 *   screencast [--seconds N] [--cursor hidden|embedded|metadata]
 *              [--size WxH] [--renegotiate SECONDS WxH]
 *              [--save PREFIX] [--save-seq N] [--dmabuf [--mods 0x..,0x..]] [--virtual W H] [--quiet]
 * PipeWire fd: $KDE_RDP_PW_FD (fd-handoff --pipewire) or a socket this
 * process connects to $PIPEWIRE_RUNTIME_DIR|$XDG_RUNTIME_DIR/pipewire-0.
 * Prints the negotiated format, per-buffer metadata summary and a
 * per-second report: "RATE t=.. frames=.. with_data=.. cursor_only=..". */
#include "common.h"
#include "zkde-screencast-unstable-v1-client.h"
#include <linux/dma-buf.h>
#include <pipewire/pipewire.h>
#include <spa/debug/types.h>
#include <spa/param/video/format-utils.h>
#include <spa/param/video/type-info.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <zlib.h>

static struct zkde_screencast_unstable_v1 *sc;
static struct wl_output *output;
static int stream_state;
static uint32_t node_id;
static unsigned long long node_serial; /* zkde v6 "serial" = PipeWire object.serial */
static struct wl_display *wl;

static struct pw_main_loop *loop;
static struct pw_stream *stream;
static struct spa_video_info_raw fmt;
static int have_fmt, want_dmabuf, quiet;
static long long mods[32] = {0, 0x00ffffffffffffffLL}; /* LINEAR, INVALID */
static int nmods = 2;
static const char *save_prefix;
static int saved;
static unsigned save_seq; /* also save buffer #N (any content) */
static double t_start;
static unsigned n_total, n_data, n_cursor_only, n_damage_meta, n_cursor_meta, n_corrupt;
static unsigned s_total, s_data, s_cursor_only;
static double seconds = 20;
static int req_w = 1280, req_h = 720, fixed_size; /* --size: fixed size request */
static double reneg_at; static int reneg_w, reneg_h; /* --renegotiate T WxH */
static const struct spa_pod *enum_format(struct spa_pod_builder *b, int with_mod);

/* ---------- wayland ---------- */
static void global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v)
{
    if (!strcmp(iface, zkde_screencast_unstable_v1_interface.name))
        sc = wl_registry_bind(r, name, &zkde_screencast_unstable_v1_interface, v < 6 ? v : 6);
    else if (!strcmp(iface, "wl_output") && !output)
        output = wl_registry_bind(r, name, &wl_output_interface, 1);
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) {}
static const struct wl_registry_listener reg_l = {global, global_remove};
static void s_closed(void *d, struct zkde_screencast_stream_unstable_v1 *s)
{
    logf_("sc", "wayland stream closed");
    stream_state = -1;
}
static void s_created(void *d, struct zkde_screencast_stream_unstable_v1 *s, uint32_t node)
{
    node_id = node;
    stream_state = 1;
}
static void s_failed(void *d, struct zkde_screencast_stream_unstable_v1 *s, const char *e)
{
    logf_("sc", "wayland stream failed: %s", e);
    stream_state = -1;
}
static void s_serial(void *d, struct zkde_screencast_stream_unstable_v1 *s, uint32_t hi, uint32_t lo)
{
    node_serial = ((unsigned long long)hi << 32) | lo;
    logf_("sc", "stream serial %llu", node_serial);
}
static const struct zkde_screencast_stream_unstable_v1_listener s_l = {s_closed, s_created, s_failed, s_serial};

/* ---------- PNG writer (no external deps besides zlib) ---------- */
static void be32(uint8_t *p, uint32_t v) { p[0] = v >> 24; p[1] = v >> 16; p[2] = v >> 8; p[3] = v; }
static void chunk(FILE *f, const char *type, const uint8_t *data, uint32_t len)
{
    uint8_t h[8];
    be32(h, len);
    memcpy(h + 4, type, 4);
    fwrite(h, 1, 8, f);
    if (len)
        fwrite(data, 1, len, f);
    uLong crc = crc32(0, (const Bytef *)type, 4);
    crc = crc32(crc, data, len);
    be32(h, crc);
    fwrite(h, 1, 4, f);
}
static void save_png(const char *path, const uint8_t *src, int w, int h, int stride, uint32_t format)
{
    size_t rawlen = (size_t)(w * 3 + 1) * h;
    uint8_t *raw = malloc(rawlen);
    for (int y = 0; y < h; y++) {
        uint8_t *o = raw + (size_t)y * (w * 3 + 1);
        *o++ = 0;
        const uint8_t *s = src + (size_t)y * stride;
        for (int x = 0; x < w; x++, s += 4) {
            switch (format) { /* memory byte order */
            case SPA_VIDEO_FORMAT_BGRx: case SPA_VIDEO_FORMAT_BGRA: o[0] = s[2]; o[1] = s[1]; o[2] = s[0]; break;
            case SPA_VIDEO_FORMAT_RGBx: case SPA_VIDEO_FORMAT_RGBA: o[0] = s[0]; o[1] = s[1]; o[2] = s[2]; break;
            case SPA_VIDEO_FORMAT_xRGB: case SPA_VIDEO_FORMAT_ARGB: o[0] = s[1]; o[1] = s[2]; o[2] = s[3]; break;
            default: /* xBGR/ABGR */ o[0] = s[3]; o[1] = s[2]; o[2] = s[1]; break;
            }
            o += 3;
        }
    }
    uLongf zlen = compressBound(rawlen);
    uint8_t *z = malloc(zlen);
    compress2(z, &zlen, raw, rawlen, 6);
    FILE *f = fopen(path, "wb");
    if (!f) {
        logf_("sc", "cannot write %s", path);
        goto out;
    }
    fwrite("\x89PNG\r\n\x1a\n", 1, 8, f);
    uint8_t ihdr[13];
    be32(ihdr, w);
    be32(ihdr + 4, h);
    ihdr[8] = 8; ihdr[9] = 2; ihdr[10] = ihdr[11] = ihdr[12] = 0;
    chunk(f, "IHDR", ihdr, 13);
    chunk(f, "IDAT", z, zlen);
    chunk(f, "IEND", NULL, 0);
    fclose(f);
    logf_("sc", "saved %s (%dx%d)", path, w, h);
out:
    free(raw);
    free(z);
}

/* ---------- PipeWire ---------- */
static const char *fmt_name(uint32_t f) { return spa_debug_type_find_short_name(spa_type_video_format, f); }

static void on_state(void *d, enum pw_stream_state old, enum pw_stream_state st, const char *err)
{
    logf_("sc", "pw stream state %s -> %s%s%s", pw_stream_state_as_string(old), pw_stream_state_as_string(st),
          err ? " err=" : "", err ? err : "");
}

static void on_param_changed(void *d, uint32_t id, const struct spa_pod *param)
{
    if (id != SPA_PARAM_Format || !param)
        return;
    uint32_t mt, mst;
    spa_format_parse(param, &mt, &mst);
    struct spa_video_info_raw nf = {0};
    spa_format_video_raw_parse(param, &nf);
    logf_("sc", "FORMAT %s %ux%u framerate=%u/%u max_framerate=%u/%u modifier=%s0x%llx%s", fmt_name(nf.format),
          nf.size.width, nf.size.height, nf.framerate.num, nf.framerate.denom, nf.max_framerate.num,
          nf.max_framerate.denom, (nf.flags & SPA_VIDEO_FLAG_MODIFIER) ? "" : "(none) ",
          (unsigned long long)nf.modifier, have_fmt ? "  [RENEGOTIATED]" : "");
    have_fmt = 1;
    fmt = nf;
    uint8_t buf[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
    const struct spa_pod *p[4];
    int types = (1 << SPA_DATA_MemFd) | (1 << SPA_DATA_MemPtr);
    if (want_dmabuf && (nf.flags & SPA_VIDEO_FLAG_MODIFIER))
        types = 1 << SPA_DATA_DmaBuf;
    p[0] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
                                      SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 16),
                                      SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
                                      SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(types));
    p[1] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta, SPA_PARAM_META_type,
                                      SPA_POD_Id(SPA_META_Header), SPA_PARAM_META_size,
                                      SPA_POD_Int(sizeof(struct spa_meta_header)));
    p[2] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta, SPA_PARAM_META_type,
                                      SPA_POD_Id(SPA_META_VideoDamage), SPA_PARAM_META_size,
                                      SPA_POD_CHOICE_RANGE_Int(sizeof(struct spa_meta_region) * 16,
                                                               sizeof(struct spa_meta_region) * 1,
                                                               sizeof(struct spa_meta_region) * 16));
#define CURSOR_META_SIZE(w, h) (sizeof(struct spa_meta_cursor) + sizeof(struct spa_meta_bitmap) + (w) * (h) * 4)
    p[3] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta, SPA_PARAM_META_type,
                                      SPA_POD_Id(SPA_META_Cursor), SPA_PARAM_META_size,
                                      SPA_POD_CHOICE_RANGE_Int(CURSOR_META_SIZE(64, 64), CURSOR_META_SIZE(1, 1),
                                                               CURSOR_META_SIZE(256, 256)));
    pw_stream_update_params(stream, p, 4);
}

static void on_add_buffer(void *d, struct pw_buffer *b)
{
    struct spa_data *sd = &b->buffer->datas[0];
    static int logged;
    if (logged++ < 1)
        logf_("sc", "add_buffer type=%s fd=%lld maxsize=%u mapped=%s flags=0x%x n_metas=%u",
              spa_debug_type_find_short_name(spa_type_data_type, sd->type), (long long)sd->fd, sd->maxsize,
              sd->data ? "yes" : "no", sd->flags, b->buffer->n_metas);
}

static void on_process(void *d)
{
    struct pw_buffer *b;
    while ((b = pw_stream_dequeue_buffer(stream))) {
        struct spa_buffer *sb = b->buffer;
        struct spa_data *sd = &sb->datas[0];
        n_total++;
        s_total++;
        int has_data = sd->chunk->size > 0;
        if (sd->chunk->flags & SPA_CHUNK_FLAG_CORRUPTED) {
            n_corrupt++;
            has_data = 0;
        }
        struct spa_meta_header *hdr = spa_buffer_find_meta_data(sb, SPA_META_Header, sizeof *hdr);
        if (hdr && (hdr->flags & SPA_META_HEADER_FLAG_CORRUPTED))
            has_data = 0;
        if (has_data) {
            n_data++;
            s_data++;
        } else {
            n_cursor_only++;
            s_cursor_only++;
        }
        int ndmg = 0;
        struct spa_meta *dm = spa_buffer_find_meta(sb, SPA_META_VideoDamage);
        char dmgs[160] = "";
        if (dm) {
            struct spa_meta_region *r;
            spa_meta_for_each(r, dm)
            {
                if (!spa_meta_region_is_valid(r))
                    break;
                if (ndmg < 3) {
                    size_t l = strlen(dmgs);
                    snprintf(dmgs + l, sizeof dmgs - l, " %d,%d %ux%u", r->region.position.x, r->region.position.y,
                             r->region.size.width, r->region.size.height);
                }
                ndmg++;
            }
            if (ndmg)
                n_damage_meta++;
        }
        char cur[160] = "none";
        struct spa_meta_cursor *mc = spa_buffer_find_meta_data(sb, SPA_META_Cursor, sizeof *mc);
        if (mc && spa_meta_cursor_is_valid(mc)) {
            n_cursor_meta++;
            int bw = 0, bh = 0;
            uint32_t bfmt = 0;
            if (mc->bitmap_offset) {
                struct spa_meta_bitmap *bm = SPA_PTROFF(mc, mc->bitmap_offset, struct spa_meta_bitmap);
                bw = bm->size.width;
                bh = bm->size.height;
                bfmt = bm->format;
            }
            snprintf(cur, sizeof cur, "id=%u pos=%d,%d hot=%d,%d bitmap=%dx%d %s", mc->id, mc->position.x,
                     mc->position.y, mc->hotspot.x, mc->hotspot.y, bw, bh, bw ? fmt_name(bfmt) : "-");
        } else if (mc)
            snprintf(cur, sizeof cur, "present-but-invalid(id=0)");
        if (!quiet && (n_total <= 12 || n_total % 50 == 0))
            logf_("sc", "buf#%u type=%s size=%u stride=%d off=%u seq=%llu damage=%d[%s ] cursor=%s", n_total,
                  spa_debug_type_find_short_name(spa_type_data_type, sd->type), sd->chunk->size, sd->chunk->stride,
                  sd->chunk->offset, hdr ? (unsigned long long)hdr->seq : 0ULL, ndmg, dmgs, cur);
        int seq_hit = save_seq && n_total == save_seq;
        if (save_prefix && ((has_data && saved < 2 && n_data >= (saved ? 30u : 3u)) || seq_hit)) {
            void *ptr = sd->data;
            void *m = NULL;
            if (!ptr && sd->type == SPA_DATA_DmaBuf) {
                struct dma_buf_sync s = {DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ};
                int r = ioctl(sd->fd, DMA_BUF_IOCTL_SYNC, &s);
                m = mmap(NULL, sd->maxsize + sd->mapoffset, PROT_READ, MAP_SHARED, sd->fd, 0);
                logf_("sc", "dmabuf mmap %s (sync ioctl %d)", m == MAP_FAILED ? strerror(errno) : "ok", r);
                if (m != MAP_FAILED)
                    ptr = (uint8_t *)m + sd->mapoffset;
                else
                    m = NULL;
            }
            if (ptr) {
                char path[512];
                if (seq_hit)
                    snprintf(path, sizeof path, "%s-buf%u.png", save_prefix, n_total);
                else
                    snprintf(path, sizeof path, "%s-%d.png", save_prefix, saved);
                save_png(path, (uint8_t *)ptr + sd->chunk->offset, fmt.size.width, fmt.size.height,
                         sd->chunk->stride, fmt.format);
            }
            if (m) {
                struct dma_buf_sync s = {DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ};
                ioctl(sd->fd, DMA_BUF_IOCTL_SYNC, &s);
                munmap(m, sd->maxsize + sd->mapoffset);
            }
            if (!seq_hit)
                saved++;
        }
        pw_stream_queue_buffer(stream, b);
    }
}

static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS,
    .state_changed = on_state,
    .param_changed = on_param_changed,
    .add_buffer = on_add_buffer,
    .process = on_process,
};

static void on_timer(void *d, uint64_t exp)
{
    double t = now_s() - t_start;
    logf_("sc", "RATE t=%.0f frames=%u with_data=%u cursor_only=%u", t, s_total, s_data, s_cursor_only);
    s_total = s_data = s_cursor_only = 0;
    if (reneg_at > 0 && t >= reneg_at) {
        reneg_at = 0;
        req_w = reneg_w;
        req_h = reneg_h;
        fixed_size = 1;
        uint8_t buf[2048];
        struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
        const struct spa_pod *params[2];
        int np = 0;
        if (want_dmabuf)
            params[np++] = enum_format(&b, 1);
        params[np++] = enum_format(&b, 0);
        logf_("sc", "RENEGOTIATE: update_params EnumFormat size %dx%d -> %d", req_w, req_h,
              pw_stream_update_params(stream, params, np));
    }
    if (t >= seconds)
        pw_main_loop_quit(loop);
}

static void on_wl(void *d, int fd, uint32_t mask)
{
    if (wl_display_dispatch(wl) < 0 || stream_state < 0) {
        logf_("sc", "wayland connection/stream ended");
        pw_main_loop_quit(loop);
    }
}

static const struct spa_pod *enum_format(struct spa_pod_builder *b, int with_mod)
{
    struct spa_pod_frame f[2];
    spa_pod_builder_push_object(b, &f[0], SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat);
    spa_pod_builder_add(b, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video), SPA_FORMAT_mediaSubtype,
                        SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), SPA_FORMAT_VIDEO_format,
                        SPA_POD_CHOICE_ENUM_Id(5, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRA,
                                               SPA_VIDEO_FORMAT_RGBx, SPA_VIDEO_FORMAT_RGBA),
                        0);
    if (with_mod) {
        /* default: DRM_FORMAT_MOD_LINEAR + DRM_FORMAT_MOD_INVALID; --mods overrides */
        spa_pod_builder_prop(b, SPA_FORMAT_VIDEO_modifier, SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE);
        spa_pod_builder_push_choice(b, &f[1], SPA_CHOICE_Enum, 0);
        spa_pod_builder_long(b, mods[0]); /* choice default */
        for (int i = 0; i < nmods; i++)
            spa_pod_builder_long(b, mods[i]);
        spa_pod_builder_pop(b, &f[1]);
    }
    spa_pod_builder_add(b, SPA_FORMAT_VIDEO_size,
                        SPA_POD_CHOICE_RANGE_Rectangle(&SPA_RECTANGLE(req_w, req_h),
                                                       fixed_size ? &SPA_RECTANGLE(req_w, req_h) : &SPA_RECTANGLE(1, 1),
                                                       fixed_size ? &SPA_RECTANGLE(req_w, req_h) : &SPA_RECTANGLE(8192, 8192)),
                        SPA_FORMAT_VIDEO_framerate, SPA_POD_Fraction(&SPA_FRACTION(0, 1)),
                        SPA_FORMAT_VIDEO_maxFramerate,
                        SPA_POD_CHOICE_RANGE_Fraction(&SPA_FRACTION(60, 1), &SPA_FRACTION(0, 1), &SPA_FRACTION(1000, 1)),
                        0);
    return spa_pod_builder_pop(b, &f[0]);
}

int main(int argc, char **argv)
{
    uint32_t cursor = ZKDE_SCREENCAST_UNSTABLE_V1_POINTER_METADATA;
    int vw = 0, vh = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--seconds") && i + 1 < argc)
            seconds = atof(argv[++i]);
        else if (!strcmp(argv[i], "--cursor") && i + 1 < argc) {
            const char *m = argv[++i];
            cursor = !strcmp(m, "hidden") ? 1 : !strcmp(m, "embedded") ? 2 : 4;
        } else if (!strcmp(argv[i], "--save") && i + 1 < argc)
            save_prefix = argv[++i];
        else if (!strcmp(argv[i], "--save-seq") && i + 1 < argc)
            save_seq = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--dmabuf"))
            want_dmabuf = 1;
        else if (!strcmp(argv[i], "--mods") && i + 1 < argc) {
            nmods = 0;
            for (char *t = strtok(argv[++i], ","); t && nmods < 32; t = strtok(NULL, ","))
                mods[nmods++] = strtoll(t, NULL, 0);
        }
        else if (!strcmp(argv[i], "--size") && i + 1 < argc) {
            sscanf(argv[++i], "%dx%d", &req_w, &req_h);
            fixed_size = 1;
        } else if (!strcmp(argv[i], "--renegotiate") && i + 2 < argc) {
            reneg_at = atof(argv[++i]);
            sscanf(argv[++i], "%dx%d", &reneg_w, &reneg_h);
        } else if (!strcmp(argv[i], "--quiet"))
            quiet = 1;
        else if (!strcmp(argv[i], "--virtual") && i + 2 < argc) {
            vw = atoi(argv[++i]);
            vh = atoi(argv[++i]);
        }
    }
    wl = connect_display();
    struct wl_registry *reg = wl_display_get_registry(wl);
    wl_registry_add_listener(reg, &reg_l, NULL);
    wl_display_roundtrip(wl);
    if (!sc || !output) {
        logf_("sc", "zkde_screencast or wl_output missing");
        return 1;
    }
    struct zkde_screencast_stream_unstable_v1 *s =
        vw ? zkde_screencast_unstable_v1_stream_virtual_output(sc, "kde-rdp-virtual", vw, vh, wl_fixed_from_int(1), cursor)
           : zkde_screencast_unstable_v1_stream_output(sc, output, cursor);
    zkde_screencast_stream_unstable_v1_add_listener(s, &s_l, NULL);
    while (stream_state == 0 && wl_display_roundtrip(wl) >= 0)
        ;
    if (stream_state != 1)
        return 1;
    logf_("sc", "zkde stream %s created: PipeWire node %u (cursor mode %u)", vw ? "virtual_output" : "output", node_id, cursor);

    pw_init(&argc, &argv);
    loop = pw_main_loop_new(NULL);
    struct pw_context *ctx = pw_context_new(pw_main_loop_get_loop(loop), NULL, 0);
    int pwfd = getenv("KDE_RDP_PW_FD") ? atoi(getenv("KDE_RDP_PW_FD")) : -1;
    if (pwfd < 0) {
        const char *dir = getenv("PIPEWIRE_RUNTIME_DIR") ? getenv("PIPEWIRE_RUNTIME_DIR") : getenv("XDG_RUNTIME_DIR");
        struct sockaddr_un a = {.sun_family = AF_UNIX};
        snprintf(a.sun_path, sizeof a.sun_path, "%s/pipewire-0", dir);
        pwfd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        if (connect(pwfd, (struct sockaddr *)&a, sizeof a) < 0)
            die("connect pipewire-0");
        logf_("sc", "connected own socket to %s", a.sun_path);
    } else
        logf_("sc", "using handed-off PipeWire fd %d", pwfd);
    struct pw_core *core = pw_context_connect_fd(ctx, pwfd, NULL, 0);
    if (!core)
        die("pw_context_connect_fd");
    /* target.object must be the object.serial (or node.name), NOT the id;
     * without the v6 serial event fall back to the deprecated target id. */
    struct pw_properties *props = pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture",
                                                    PW_KEY_MEDIA_ROLE, "Screen", NULL);
    if (node_serial)
        pw_properties_setf(props, PW_KEY_TARGET_OBJECT, "%llu", node_serial);
    stream = pw_stream_new(core, "kde-rdp-probe", props);
    struct spa_hook hook;
    pw_stream_add_listener(stream, &hook, &stream_events, NULL);
    uint8_t buf[2048];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
    const struct spa_pod *params[2];
    int np = 0;
    if (want_dmabuf)
        params[np++] = enum_format(&b, 1);
    params[np++] = enum_format(&b, 0);
    if (pw_stream_connect(stream, PW_DIRECTION_INPUT, node_id, PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
                          params, np) < 0)
        die("pw_stream_connect");
    pw_loop_add_io(pw_main_loop_get_loop(loop), wl_display_get_fd(wl), SPA_IO_IN, false, on_wl, NULL);
    struct spa_source *timer = pw_loop_add_timer(pw_main_loop_get_loop(loop), on_timer, NULL);
    struct timespec one = {1, 0};
    pw_loop_update_timer(pw_main_loop_get_loop(loop), timer, &one, &one, false);
    t_start = now_s();
    wl_display_flush(wl);
    pw_main_loop_run(loop);
    logf_("sc", "SUMMARY frames=%u with_data=%u cursor_only=%u with_damage_meta=%u with_cursor_meta=%u corrupted=%u",
          n_total, n_data, n_cursor_only, n_damage_meta, n_cursor_meta, n_corrupt);
    pw_stream_destroy(stream);
    zkde_screencast_stream_unstable_v1_close(s);
    wl_display_roundtrip(wl);
    return 0;
}
