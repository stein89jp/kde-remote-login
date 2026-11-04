/* PipeWire consumer for KWin's zkde_screencast_unstable_v1 output stream.
 *
 * One pw_thread_loop per stream, connected with pw_context_connect_fd to the
 * session's private PipeWire (the broker's fd). The Rust capture worker polls
 * an eventfd and takes the newest content buffer and the latest cursor state.
 *
 * KWin offers only 2..4 buffers per stream (screencaststream.cpp), and skips
 * a frame when it cannot dequeue one, so buffers are never kept for long:
 * - a newer content buffer replaces (and requeues) an untaken one, with the
 *   damage of both merged;
 * - cursor-only buffers (SPA_CHUNK_FLAG_CORRUPTED, full chunk size) are
 *   parsed for SPA_META_Cursor and requeued at once;
 * - a taken buffer stays dequeued until Rust releases it: MemFd pixels are
 *   copied out by the worker right away; a DmaBuf is held by the GPU frame
 *   ticket until the encoder dropped it.
 * MemFd memory is mapped here (not with PW_STREAM_FLAG_MAP_BUFFERS), and
 * every fd is duplicated, so a buffer that PipeWire removes during a
 * renegotiation while Rust holds it stays valid until it is released. */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <errno.h>
#include <fcntl.h>
#include <pipewire/pipewire.h>
#include <pthread.h>
#include <spa/buffer/meta.h>
#include <spa/param/video/format-utils.h>
#include <spa/utils/result.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define KDESC_DAMAGE_MAX 16
#define KDESC_CURSOR_MAX 256
#define KDESC_MODIFIERS_MAX 64
#define KDESC_BUFFERS_MAX 32
#define CURSOR_META_SIZE(w, h) (sizeof(struct spa_meta_cursor) + sizeof(struct spa_meta_bitmap) + (w) * (h) * 4)

struct kdesc_rect { int32_t x, y; uint32_t width, height; };

/* What the Rust side receives for one content frame. */
struct kdesc_frame {
    void *buffer;            /* release exactly once with kdesc_release */
    uint32_t width, height, stride, offset, size;
    uint32_t drm_format;     /* DRM fourcc of the pixel layout */
    uint64_t modifier;       /* DmaBuf only */
    int32_t dmabuf;          /* 1: fd is a DMA-BUF; 0: data maps MemFd pixels */
    int32_t fd;              /* borrowed while the buffer is held */
    const uint8_t *data;     /* MemFd: first pixel (offset applied) */
    uint64_t sequence;
    int32_t damage_full;     /* damage unknown or too large: everything */
    uint32_t n_damage;
    struct kdesc_rect damage[KDESC_DAMAGE_MAX];
};

struct kdesc_cursor {
    uint64_t serial;         /* bumps with every cursor metadata update */
    uint32_t id;             /* 0: hidden or not on this output */
    int32_t x, y, hot_x, hot_y;
    uint64_t bitmap_serial;  /* bumps with every new bitmap */
    uint32_t bitmap_width, bitmap_height, bitmap_stride, bitmap_format;
    /* Copied only when the caller's known bitmap serial differs. */
    uint8_t bitmap[KDESC_CURSOR_MAX * KDESC_CURSOR_MAX * 4];
};

struct kdesc_buffer {
    struct kdesc *owner;
    struct pw_buffer *pw;    /* NULL once PipeWire removed it */
    int rust;                /* held by a Rust frame ticket */
    int type;
    int fd;
    void *map;
    size_t map_size;
    uint32_t mapoffset;
};

struct kdesc {
    atomic_int refs;         /* the owner plus every Rust-held buffer */
    struct pw_thread_loop *loop;
    struct pw_context *context;
    struct pw_core *core;
    struct spa_hook core_listener;
    struct pw_stream *stream;
    struct spa_hook stream_listener;
    int event_fd;
    int closed;
    /* Negotiation. */
    uint32_t width, height;  /* requested size */
    int fixed_size;
    int want_dmabuf;
    uint64_t modifiers[KDESC_MODIFIERS_MAX];
    uint32_t n_modifiers;
    struct spa_video_info_raw format;
    int have_format;
    uint32_t buffers;        /* currently added */
    /* Status. */
    int state;               /* 0 connecting, 1 paused, 2 streaming, -1 error */
    char error[256];
    uint64_t format_serial;
    /* The newest untaken content buffer and the damage merged since the last take. */
    struct kdesc_buffer *pending;
    struct spa_chunk pending_chunk;
    uint64_t sequence;
    int damage_full;
    uint32_t n_damage;
    struct kdesc_rect damage[KDESC_DAMAGE_MAX];
    struct kdesc_cursor cursor;
    struct kdesc_buffer *all[KDESC_BUFFERS_MAX];
    /* Counters for diagnostics. */
    uint64_t content_frames, cursor_frames, merged_frames;
};

static pthread_once_t pw_once = PTHREAD_ONCE_INIT;
static void pw_global_init(void) { pw_init(NULL, NULL); }

static void kdesc_signal(struct kdesc *s)
{
    uint64_t one = 1;
    ssize_t r = write(s->event_fd, &one, sizeof one);
    (void)r; /* EAGAIN: a wake is already pending */
}

static void set_error(struct kdesc *s, const char *what, const char *detail)
{
    if (s->state != -1)
        snprintf(s->error, sizeof s->error, "%s%s%s", what, detail ? ": " : "", detail ? detail : "");
    s->state = -1;
    kdesc_signal(s);
}

static uint32_t drm_format_of(uint32_t spa)
{
    switch (spa) {
    case SPA_VIDEO_FORMAT_BGRx: return 0x34325258; /* XR24 */
    case SPA_VIDEO_FORMAT_BGRA: return 0x34325241; /* AR24 */
    default: return 0;
    }
}

/* EnumFormat: BGRx/BGRA, the requested size as default (fixed for resize). */
static const struct spa_pod *enum_format(struct kdesc *s, struct spa_pod_builder *b, int with_modifiers)
{
    struct spa_pod_frame f[2];
    struct spa_rectangle def = SPA_RECTANGLE(s->width, s->height);
    struct spa_rectangle min = s->fixed_size ? def : SPA_RECTANGLE(1, 1);
    struct spa_rectangle max = s->fixed_size ? def : SPA_RECTANGLE(8192, 8192);
    spa_pod_builder_push_object(b, &f[0], SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat);
    spa_pod_builder_add(b, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video), SPA_FORMAT_mediaSubtype,
                        SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), SPA_FORMAT_VIDEO_format,
                        SPA_POD_CHOICE_ENUM_Id(3, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRA), 0);
    if (with_modifiers) {
        spa_pod_builder_prop(b, SPA_FORMAT_VIDEO_modifier, SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE);
        spa_pod_builder_push_choice(b, &f[1], SPA_CHOICE_Enum, 0);
        spa_pod_builder_long(b, (int64_t)s->modifiers[0]);
        for (uint32_t i = 0; i < s->n_modifiers; i++)
            spa_pod_builder_long(b, (int64_t)s->modifiers[i]);
        spa_pod_builder_pop(b, &f[1]);
    }
    spa_pod_builder_add(b, SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(&def, &min, &max),
                        SPA_FORMAT_VIDEO_framerate, SPA_POD_Fraction(&SPA_FRACTION(0, 1)),
                        SPA_FORMAT_VIDEO_maxFramerate,
                        SPA_POD_CHOICE_RANGE_Fraction(&SPA_FRACTION(60, 1), &SPA_FRACTION(0, 1), &SPA_FRACTION(1000, 1)),
                        0);
    return spa_pod_builder_pop(b, &f[0]);
}

static uint32_t enum_formats(struct kdesc *s, struct spa_pod_builder *b, const struct spa_pod **params)
{
    uint32_t n = 0;
    if (s->want_dmabuf && s->n_modifiers)
        params[n++] = enum_format(s, b, 1);
    params[n++] = enum_format(s, b, 0);
    return n;
}

/* ---------- buffers ---------- */

static void buffer_free(struct kdesc_buffer *kb)
{
    struct kdesc *s = kb->owner;
    for (int i = 0; i < KDESC_BUFFERS_MAX; i++)
        if (s->all[i] == kb)
            s->all[i] = NULL;
    if (kb->map)
        munmap(kb->map, kb->map_size);
    if (kb->fd >= 0)
        close(kb->fd);
    free(kb);
}

/* Give a buffer back to KWin, or free it when PipeWire already removed it.
 * The loop lock is held. */
static void buffer_return(struct kdesc *s, struct kdesc_buffer *kb)
{
    if (!kb->pw)
        buffer_free(kb);
    else if (s->stream)
        pw_stream_queue_buffer(s->stream, kb->pw);
}

static void on_add_buffer(void *data, struct pw_buffer *b)
{
    struct kdesc *s = data;
    struct spa_data *d = &b->buffer->datas[0];
    struct kdesc_buffer *kb = calloc(1, sizeof *kb);
    if (!kb) {
        set_error(s, "screencast buffer allocation failed", NULL);
        return;
    }
    kb->owner = s;
    kb->pw = b;
    kb->type = d->type;
    kb->fd = -1;
    if ((d->type == SPA_DATA_MemFd || d->type == SPA_DATA_DmaBuf) && d->fd >= 0)
        kb->fd = fcntl((int)d->fd, F_DUPFD_CLOEXEC, 3);
    if (d->type == SPA_DATA_MemFd && kb->fd >= 0) {
        kb->map_size = (size_t)d->mapoffset + d->maxsize;
        kb->map = mmap(NULL, kb->map_size, PROT_READ, MAP_SHARED, kb->fd, 0);
        if (kb->map == MAP_FAILED)
            kb->map = NULL;
        kb->mapoffset = d->mapoffset;
    }
    for (int i = 0; i < KDESC_BUFFERS_MAX; i++)
        if (!s->all[i]) {
            s->all[i] = kb;
            break;
        }
    b->user_data = kb;
    s->buffers++;
}

static void on_remove_buffer(void *data, struct pw_buffer *b)
{
    struct kdesc *s = data;
    struct kdesc_buffer *kb = b->user_data;
    b->user_data = NULL;
    if (s->buffers)
        s->buffers--;
    if (!kb)
        return;
    kb->pw = NULL;
    if (s->pending == kb) {
        s->pending = NULL;
        s->damage_full = 1;
    }
    if (!kb->rust)
        buffer_free(kb);
}

/* ---------- metadata ---------- */

static void merge_damage(struct kdesc *s, struct spa_buffer *sb)
{
    struct spa_meta *meta = spa_buffer_find_meta(sb, SPA_META_VideoDamage);
    if (!meta) {
        s->damage_full = 1;
        return;
    }
    struct spa_meta_region *r;
    spa_meta_for_each(r, meta)
    {
        if (!spa_meta_region_is_valid(r))
            break;
        if (s->damage_full)
            continue;
        if (s->n_damage == KDESC_DAMAGE_MAX) {
            s->damage_full = 1;
            continue;
        }
        s->damage[s->n_damage++] = (struct kdesc_rect){r->region.position.x, r->region.position.y,
                                                        r->region.size.width, r->region.size.height};
    }
}

static void read_cursor(struct kdesc *s, struct spa_buffer *sb)
{
    struct spa_meta_cursor *mc = spa_buffer_find_meta_data(sb, SPA_META_Cursor, sizeof *mc);
    if (!mc)
        return;
    struct kdesc_cursor *c = &s->cursor;
    int changed = c->id != mc->id;
    if (mc->id != 0) {
        changed |= c->x != mc->position.x || c->y != mc->position.y || c->hot_x != mc->hotspot.x ||
                   c->hot_y != mc->hotspot.y;
        c->x = mc->position.x;
        c->y = mc->position.y;
        c->hot_x = mc->hotspot.x;
        c->hot_y = mc->hotspot.y;
        struct spa_meta *meta = spa_buffer_find_meta(sb, SPA_META_Cursor);
        if (mc->bitmap_offset >= sizeof *mc && meta && mc->bitmap_offset + sizeof(struct spa_meta_bitmap) <= meta->size) {
            struct spa_meta_bitmap *bm = SPA_PTROFF(mc, mc->bitmap_offset, struct spa_meta_bitmap);
            uint32_t w = bm->size.width, h = bm->size.height, stride = (uint32_t)bm->stride;
            if (bm->format != 0 && w > 0 && h > 0 && w <= KDESC_CURSOR_MAX && h <= KDESC_CURSOR_MAX &&
                stride >= w * 4 && (uint64_t)mc->bitmap_offset + bm->offset + (uint64_t)stride * h <= meta->size) {
                const uint8_t *src = SPA_PTROFF(bm, bm->offset, uint8_t);
                for (uint32_t y = 0; y < h; y++)
                    memcpy(c->bitmap + (size_t)y * w * 4, src + (size_t)y * stride, (size_t)w * 4);
                c->bitmap_width = w;
                c->bitmap_height = h;
                c->bitmap_stride = w * 4;
                c->bitmap_format = bm->format;
                c->bitmap_serial++;
                changed = 1;
            }
        }
    }
    c->id = mc->id;
    if (changed)
        c->serial++;
}

/* ---------- stream events ---------- */

static void on_state(void *data, enum pw_stream_state old, enum pw_stream_state state, const char *error)
{
    struct kdesc *s = data;
    (void)old;
    switch (state) {
    case PW_STREAM_STATE_ERROR: set_error(s, "screencast stream error", error); return;
    case PW_STREAM_STATE_UNCONNECTED:
        if (!s->closed)
            set_error(s, "screencast stream disconnected", error);
        return;
    case PW_STREAM_STATE_PAUSED: s->state = 1; break;
    case PW_STREAM_STATE_STREAMING: s->state = 2; break;
    default: break;
    }
    kdesc_signal(s);
}

static void on_param_changed(void *data, uint32_t id, const struct spa_pod *param)
{
    struct kdesc *s = data;
    if (id != SPA_PARAM_Format || !param)
        return;
    struct spa_video_info_raw info = {0};
    uint32_t media_type, media_subtype;
    if (spa_format_parse(param, &media_type, &media_subtype) < 0 || spa_format_video_raw_parse(param, &info) < 0) {
        set_error(s, "unparsable screencast format", NULL);
        return;
    }
    s->format = info;
    s->have_format = 1;
    s->format_serial++;
    int dmabuf = s->want_dmabuf && (info.flags & SPA_VIDEO_FLAG_MODIFIER);
    uint8_t buffer[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof buffer);
    const struct spa_pod *params[4];
    params[0] = spa_pod_builder_add_object(
        &b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers, SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 4),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1), SPA_PARAM_BUFFERS_dataType,
        SPA_POD_CHOICE_FLAGS_Int(dmabuf ? 1 << SPA_DATA_DmaBuf : 1 << SPA_DATA_MemFd));
    params[1] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta, SPA_PARAM_META_type,
                                           SPA_POD_Id(SPA_META_Header), SPA_PARAM_META_size,
                                           SPA_POD_Int(sizeof(struct spa_meta_header)));
    params[2] = spa_pod_builder_add_object(
        &b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta, SPA_PARAM_META_type, SPA_POD_Id(SPA_META_VideoDamage),
        SPA_PARAM_META_size,
        SPA_POD_CHOICE_RANGE_Int(sizeof(struct spa_meta_region) * KDESC_DAMAGE_MAX, sizeof(struct spa_meta_region),
                                 sizeof(struct spa_meta_region) * KDESC_DAMAGE_MAX));
    params[3] = spa_pod_builder_add_object(
        &b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta, SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Cursor),
        SPA_PARAM_META_size,
        SPA_POD_CHOICE_RANGE_Int(CURSOR_META_SIZE(64, 64), CURSOR_META_SIZE(1, 1),
                                 CURSOR_META_SIZE(KDESC_CURSOR_MAX, KDESC_CURSOR_MAX)));
    pw_stream_update_params(s->stream, params, 4);
    /* A new format invalidates merged damage. */
    s->damage_full = 1;
    kdesc_signal(s);
}

static void on_process(void *data)
{
    struct kdesc *s = data;
    struct pw_buffer *b;
    int signal = 0;
    while ((b = pw_stream_dequeue_buffer(s->stream))) {
        struct kdesc_buffer *kb = b->user_data;
        struct spa_buffer *sb = b->buffer;
        uint64_t cursor_serial = s->cursor.serial;
        read_cursor(s, sb);
        signal |= cursor_serial != s->cursor.serial;
        struct spa_data *d = &sb->datas[0];
        struct spa_meta_header *header = spa_buffer_find_meta_data(sb, SPA_META_Header, sizeof *header);
        int corrupted = (d->chunk->flags & SPA_CHUNK_FLAG_CORRUPTED) ||
                        (header && (header->flags & SPA_META_HEADER_FLAG_CORRUPTED));
        if (!kb || corrupted || d->chunk->size == 0 || (kb->type == SPA_DATA_MemFd && !kb->map) || kb->fd < 0) {
            /* Cursor-only (or unusable) buffer: KWin may reuse it at once. */
            s->cursor_frames++;
            pw_stream_queue_buffer(s->stream, b);
            continue;
        }
        s->content_frames++;
        merge_damage(s, sb);
        if (s->pending) {
            s->merged_frames++;
            buffer_return(s, s->pending);
        }
        s->pending = kb;
        s->pending_chunk = *d->chunk;
        s->sequence++;
        signal = 1;
    }
    if (signal)
        kdesc_signal(s);
}

static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS,
    .state_changed = on_state,
    .param_changed = on_param_changed,
    .add_buffer = on_add_buffer,
    .remove_buffer = on_remove_buffer,
    .process = on_process,
};

static void on_core_error(void *data, uint32_t id, int seq, int res, const char *message)
{
    struct kdesc *s = data;
    (void)seq;
    if (id == PW_ID_CORE)
        set_error(s, res == -EPIPE ? "PipeWire connection closed" : "PipeWire error", message);
}

static const struct pw_core_events core_events = {
    PW_VERSION_CORE_EVENTS,
    .error = on_core_error,
};

/* ---------- public API ---------- */

static void kdesc_unref(struct kdesc *s)
{
    if (atomic_fetch_sub(&s->refs, 1) != 1)
        return;
    /* The stream and core are gone; only the loop and context remain. */
    pw_thread_loop_stop(s->loop);
    if (s->context)
        pw_context_destroy(s->context);
    pw_thread_loop_destroy(s->loop);
    close(s->event_fd);
    free(s);
}

/* Create and connect the consumer stream for KWin's node. Loop lock held. */
static int start_stream(struct kdesc *s, uint64_t serial, uint32_t node, char *error, size_t length)
{
    struct pw_properties *props = pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture",
                                                    PW_KEY_MEDIA_ROLE, "Screen", NULL);
    /* KWin's object.serial is not its node id; the id breaks linking. */
    if (serial)
        pw_properties_setf(props, PW_KEY_TARGET_OBJECT, "%llu", (unsigned long long)serial);
    s->stream = pw_stream_new(s->core, "kde-rdp-bridge", props);
    if (!s->stream) {
        snprintf(error, length, "PipeWire stream creation failed");
        return -1;
    }
    pw_stream_add_listener(s->stream, &s->stream_listener, &stream_events, s);
    uint8_t buffer[4096];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof buffer);
    const struct spa_pod *params[2];
    uint32_t n = enum_formats(s, &b, params);
    int result = pw_stream_connect(s->stream, PW_DIRECTION_INPUT, serial ? PW_ID_ANY : node,
                                   PW_STREAM_FLAG_AUTOCONNECT, params, n);
    if (result < 0) {
        snprintf(error, length, "PipeWire stream connect failed: %s", spa_strerror(result));
        return -1;
    }
    return 0;
}

/* Destroy the stream; buffers Rust holds stay valid until released. Loop
 * lock held. */
static void detach_stream(struct kdesc *s)
{
    int closed = s->closed;
    s->closed = 1;
    if (s->pending) {
        struct kdesc_buffer *pending = s->pending;
        s->pending = NULL;
        buffer_return(s, pending);
    }
    if (s->stream)
        pw_stream_destroy(s->stream);
    s->stream = NULL;
    /* Orphan whatever remove_buffer did not report. */
    for (int i = 0; i < KDESC_BUFFERS_MAX; i++) {
        struct kdesc_buffer *kb = s->all[i];
        if (!kb)
            continue;
        if (kb->pw)
            kb->pw->user_data = NULL;
        kb->pw = NULL;
        if (!kb->rust)
            buffer_free(kb);
    }
    s->buffers = 0;
    s->closed = closed;
}

/* Connect to PipeWire over `fd` (always consumed) and start consuming KWin's
 * node `serial` (object.serial; falls back to `node` when serial is 0). */
struct kdesc *kdesc_new(int fd, uint64_t serial, uint32_t node, uint32_t width, uint32_t height,
                        const uint64_t *modifiers, uint32_t n_modifiers, int want_dmabuf, char *error, size_t length)
{
    pthread_once(&pw_once, pw_global_init);
    struct kdesc *s = calloc(1, sizeof *s);
    if (!s) {
        close(fd);
        snprintf(error, length, "out of memory");
        return NULL;
    }
    atomic_init(&s->refs, 1);
    s->width = width;
    s->height = height;
    s->want_dmabuf = want_dmabuf;
    s->n_modifiers = n_modifiers > KDESC_MODIFIERS_MAX ? KDESC_MODIFIERS_MAX : n_modifiers;
    if (s->n_modifiers)
        memcpy(s->modifiers, modifiers, s->n_modifiers * sizeof *modifiers);
    s->damage_full = 1;
    s->event_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    s->loop = pw_thread_loop_new("kde-rdp-screencast", NULL);
    if (s->event_fd < 0 || !s->loop) {
        close(fd);
        snprintf(error, length, "screencast loop creation failed");
        goto fail;
    }
    s->context = pw_context_new(pw_thread_loop_get_loop(s->loop), NULL, 0);
    if (!s->context) {
        close(fd);
        snprintf(error, length, "PipeWire context creation failed");
        goto fail;
    }
    if (pw_thread_loop_start(s->loop) < 0) {
        close(fd);
        snprintf(error, length, "PipeWire thread start failed");
        goto fail;
    }
    pw_thread_loop_lock(s->loop);
    s->core = pw_context_connect_fd(s->context, fd, NULL, 0);
    if (!s->core) {
        snprintf(error, length, "PipeWire connection failed: %s", strerror(errno));
        pw_thread_loop_unlock(s->loop);
        goto fail;
    }
    pw_core_add_listener(s->core, &s->core_listener, &core_events, s);
    int started = start_stream(s, serial, node, error, length);
    pw_thread_loop_unlock(s->loop);
    if (started < 0)
        goto fail;
    return s;
fail:
    if (s->loop) {
        pw_thread_loop_lock(s->loop);
        s->closed = 1;
        if (s->stream)
            pw_stream_destroy(s->stream);
        s->stream = NULL;
        if (s->core)
            pw_core_disconnect(s->core);
        s->core = NULL;
        pw_thread_loop_unlock(s->loop);
        kdesc_unref(s);
    } else {
        if (s->event_fd >= 0)
            close(s->event_fd);
        free(s);
    }
    return NULL;
}

int kdesc_event_fd(struct kdesc *s) { return s->event_fd; }

void kdesc_clear_event(struct kdesc *s)
{
    uint64_t value;
    ssize_t r = read(s->event_fd, &value, sizeof value);
    (void)r;
}

/* State: 0 connecting, 1 paused, 2 streaming, -1 failed (message in error). */
int kdesc_status(struct kdesc *s, char *error, size_t length, uint64_t *format_serial, uint32_t *buffers)
{
    pw_thread_loop_lock(s->loop);
    int state = s->state;
    if (state < 0 && length)
        snprintf(error, length, "%s", s->error);
    if (format_serial)
        *format_serial = s->format_serial;
    if (buffers)
        *buffers = s->buffers;
    pw_thread_loop_unlock(s->loop);
    return state;
}

/* Take the newest content frame. 1: frame filled (release it); 0: none. */
int kdesc_take(struct kdesc *s, struct kdesc_frame *frame)
{
    pw_thread_loop_lock(s->loop);
    struct kdesc_buffer *kb = s->pending;
    if (!kb || !s->have_format) {
        pw_thread_loop_unlock(s->loop);
        return 0;
    }
    s->pending = NULL;
    memset(frame, 0, sizeof *frame);
    kb->rust = 1;
    atomic_fetch_add(&s->refs, 1);
    const struct spa_data *d = &kb->pw->buffer->datas[0];
    frame->buffer = kb;
    frame->width = s->format.size.width;
    frame->height = s->format.size.height;
    frame->stride = s->pending_chunk.stride > 0 ? (uint32_t)s->pending_chunk.stride : s->format.size.width * 4;
    frame->offset = s->pending_chunk.offset;
    frame->size = s->pending_chunk.size;
    frame->drm_format = drm_format_of(s->format.format);
    frame->modifier = s->format.modifier;
    frame->dmabuf = kb->type == SPA_DATA_DmaBuf;
    frame->fd = kb->fd;
    if (!frame->dmabuf) {
        uint64_t end = (uint64_t)kb->mapoffset + frame->offset + (uint64_t)frame->stride * frame->height;
        frame->data = end <= kb->map_size ? (const uint8_t *)kb->map + kb->mapoffset + frame->offset : NULL;
    } else {
        /* DMA-BUF planes carry their own offset; chunk offsets are 0 for KWin. */
        frame->offset = d->chunk ? d->chunk->offset : 0;
    }
    frame->sequence = s->sequence;
    frame->damage_full = s->damage_full;
    frame->n_damage = s->damage_full ? 0 : s->n_damage;
    memcpy(frame->damage, s->damage, sizeof frame->damage);
    s->damage_full = 0;
    s->n_damage = 0;
    pw_thread_loop_unlock(s->loop);
    return 1;
}

/* Return a taken buffer to KWin. Safe from any thread, after kdesc_free too. */
void kdesc_release(void *buffer)
{
    struct kdesc_buffer *kb = buffer;
    struct kdesc *s = kb->owner;
    pw_thread_loop_lock(s->loop);
    kb->rust = 0;
    buffer_return(s, kb);
    pw_thread_loop_unlock(s->loop);
    kdesc_unref(s);
}

/* Copy the cursor state. The bitmap is copied only when its serial differs
 * from `known_bitmap`. Returns 1 when the bitmap was copied. */
int kdesc_cursor(struct kdesc *s, struct kdesc_cursor *out, uint64_t known_bitmap)
{
    pw_thread_loop_lock(s->loop);
    int copy = s->cursor.bitmap_serial != known_bitmap && s->cursor.bitmap_serial != 0;
    out->serial = s->cursor.serial;
    out->id = s->cursor.id;
    out->x = s->cursor.x;
    out->y = s->cursor.y;
    out->hot_x = s->cursor.hot_x;
    out->hot_y = s->cursor.hot_y;
    out->bitmap_serial = s->cursor.bitmap_serial;
    out->bitmap_width = s->cursor.bitmap_width;
    out->bitmap_height = s->cursor.bitmap_height;
    out->bitmap_stride = s->cursor.bitmap_stride;
    out->bitmap_format = s->cursor.bitmap_format;
    if (copy)
        memcpy(out->bitmap, s->cursor.bitmap, (size_t)s->cursor.bitmap_stride * s->cursor.bitmap_height);
    pw_thread_loop_unlock(s->loop);
    return copy;
}

/* Renegotiate: a fixed size (resize) and/or DmaBuf on or off. */
int kdesc_renegotiate(struct kdesc *s, uint32_t width, uint32_t height, int want_dmabuf)
{
    pw_thread_loop_lock(s->loop);
    s->width = width;
    s->height = height;
    s->fixed_size = 1;
    s->want_dmabuf = want_dmabuf;
    uint8_t buffer[4096];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof buffer);
    const struct spa_pod *params[2];
    uint32_t n = enum_formats(s, &b, params);
    int result = s->stream ? pw_stream_update_params(s->stream, params, n) : -EPIPE;
    pw_thread_loop_unlock(s->loop);
    return result;
}

void kdesc_counters(struct kdesc *s, uint64_t *content, uint64_t *cursor, uint64_t *merged)
{
    pw_thread_loop_lock(s->loop);
    *content = s->content_frames;
    *cursor = s->cursor_frames;
    *merged = s->merged_frames;
    pw_thread_loop_unlock(s->loop);
}

/* Disconnect; buffers still held by Rust stay valid until released. */
void kdesc_free(struct kdesc *s)
{
    if (!s)
        return;
    pw_thread_loop_lock(s->loop);
    s->closed = 1;
    detach_stream(s);
    if (s->core)
        pw_core_disconnect(s->core);
    s->core = NULL;
    pw_thread_loop_unlock(s->loop);
    kdesc_unref(s);
}

/* Replace the stream with one for a new KWin node on the same connection
 * (a new zkde stream, e.g. after a cursor mode change), at a fixed size. */
int kdesc_retarget(struct kdesc *s, uint64_t serial, uint32_t node, uint32_t width, uint32_t height,
                   int want_dmabuf, char *error, size_t length)
{
    pw_thread_loop_lock(s->loop);
    s->closed = 1;
    detach_stream(s);
    s->closed = 0;
    s->state = 0;
    s->error[0] = 0;
    s->have_format = 0;
    s->damage_full = 1;
    s->n_damage = 0;
    s->width = width;
    s->height = height;
    s->fixed_size = 1;
    s->want_dmabuf = want_dmabuf;
    /* The new stream resends the bitmap; until then the cursor is hidden. */
    s->cursor.id = 0;
    s->cursor.serial++;
    int result = s->core ? start_stream(s, serial, node, error, length) : -1;
    if (!s->core)
        snprintf(error, length, "PipeWire connection closed");
    pw_thread_loop_unlock(s->loop);
    kdesc_signal(s);
    return result;
}

/* ---------- EGL modifier query ---------- */

/* video_gpu.c's reference-counted EGL display initialization: EGL shares one
 * display per device with the encoder. */
int kde_rdp_egl_display_acquire(EGLDisplay display);
void kde_rdp_egl_display_release(EGLDisplay display);

/* DMA-BUF modifiers the render node `node` can import for `fourcc`, without
 * external-only ones. Returns the count, or -1 with `error` set. */
int kdesc_query_modifiers(const char *node, uint32_t fourcc, uint64_t *out, int max, char *error, size_t length)
{
    PFNEGLQUERYDEVICESEXTPROC query_devices = (void *)eglGetProcAddress("eglQueryDevicesEXT");
    PFNEGLQUERYDEVICESTRINGEXTPROC query_string = (void *)eglGetProcAddress("eglQueryDeviceStringEXT");
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform_display = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC query_modifiers = (void *)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
    if (!query_devices || !query_string || !platform_display || !query_modifiers) {
        snprintf(error, length, "EGL lacks device or DMA-BUF modifier query extensions");
        return -1;
    }
    struct stat wanted;
    if (stat(node, &wanted) != 0) {
        snprintf(error, length, "render node %s unavailable: %s", node, strerror(errno));
        return -1;
    }
    EGLDeviceEXT devices[16];
    EGLint count = 0;
    if (!query_devices(16, devices, &count)) {
        snprintf(error, length, "EGL device query failed (%#x)", eglGetError());
        return -1;
    }
    EGLDisplay display = EGL_NO_DISPLAY;
    for (EGLint i = 0; i < count; i++) {
        const char *path = query_string(devices[i], EGL_DRM_RENDER_NODE_FILE_EXT);
        struct stat st;
        if (path && stat(path, &st) == 0 && st.st_rdev == wanted.st_rdev) {
            display = platform_display(EGL_PLATFORM_DEVICE_EXT, devices[i], NULL);
            break;
        }
    }
    if (display == EGL_NO_DISPLAY) {
        snprintf(error, length, "no EGL device matches %s", node);
        return -1;
    }
    if (!kde_rdp_egl_display_acquire(display)) {
        snprintf(error, length, "EGL initialization failed (%#x)", eglGetError());
        return -1;
    }
    EGLuint64KHR modifiers[KDESC_MODIFIERS_MAX];
    EGLBoolean external[KDESC_MODIFIERS_MAX];
    EGLint n = 0;
    EGLBoolean queried = query_modifiers(display, (EGLint)fourcc, KDESC_MODIFIERS_MAX, modifiers, external, &n);
    EGLint query_error = eglGetError();
    kde_rdp_egl_display_release(display);
    if (!queried) {
        snprintf(error, length, "EGL modifier query failed (%#x)", query_error);
        return -1;
    }
    int written = 0;
    for (EGLint i = 0; i < n && written < max; i++)
        if (!external[i] && modifiers[i] != 0x00ffffffffffffffULL)
            out[written++] = modifiers[i];
    return written;
}
