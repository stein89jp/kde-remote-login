/* target-window: a fullscreen xdg_toplevel that logs every wl_pointer and
 * wl_keyboard event it receives (surface-local == global logical
 * coordinates because it is fullscreen on the only output).
 *   target-window [--animate START STOP] [--seconds N]
 * --animate repaints with a changing colour every frame between START and
 * STOP seconds after mapping (used to drive the screencast frame rate). */
#include "common.h"
#include "xdg-shell-client.h"
#include <linux/input-event-codes.h>
#include <poll.h>

static struct wl_compositor *comp;
static struct wl_shm *shm;
static struct xdg_wm_base *wm;
static struct wl_seat *seat;
static struct wl_surface *surf;
static int W = 0, H = 0, configured, frame_pending;
static double t0, anim_start = -1, anim_stop = -1;
static unsigned frames_drawn;

static void draw(void)
{
    if (!W || !H)
        return;
    int stride = W * 4;
    size_t size = (size_t)stride * H;
    int fd = make_shm_fd(size);
    uint32_t *px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    double t = now_s() - t0;
    uint8_t v = (uint8_t)(frames_drawn * 7);
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            /* gradient + moving stripe so a saved frame is recognisable */
            uint32_t r = x * 255 / W, g = y * 255 / H, b = ((x + (int)(t * 200)) / 40 % 2) ? 200 : v;
            px[y * W + x] = 0xff000000u | r << 16 | g << 8 | b;
        }
    munmap(px, size);
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
    struct wl_buffer *buf = wl_shm_pool_create_buffer(pool, 0, W, H, stride, WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    wl_surface_attach(surf, buf, 0, 0);
    wl_surface_damage_buffer(surf, 0, 0, W, H);
    frames_drawn++;
}

static void frame_done(void *d, struct wl_callback *cb, uint32_t t);
static const struct wl_callback_listener frame_l = {frame_done};
static void request_frame(void)
{
    struct wl_callback *cb = wl_surface_frame(surf);
    wl_callback_add_listener(cb, &frame_l, NULL);
    frame_pending = 1;
}
static int animating(void)
{
    double t = now_s() - t0;
    return anim_start >= 0 && t >= anim_start && t < anim_stop;
}
static void frame_done(void *d, struct wl_callback *cb, uint32_t t)
{
    wl_callback_destroy(cb);
    frame_pending = 0;
    if (animating()) {
        draw();
        request_frame();
        wl_surface_commit(surf);
    }
}

/* pointer */
static void p_enter(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *sf, wl_fixed_t x, wl_fixed_t y)
{ logf_("target", "pointer enter %.2f %.2f", wl_fixed_to_double(x), wl_fixed_to_double(y)); }
static void p_leave(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *sf) { logf_("target", "pointer leave"); }
static void p_motion(void *d, struct wl_pointer *p, uint32_t t, wl_fixed_t x, wl_fixed_t y)
{ logf_("target", "pointer motion %.2f %.2f", wl_fixed_to_double(x), wl_fixed_to_double(y)); }
static void p_button(void *d, struct wl_pointer *p, uint32_t s, uint32_t t, uint32_t b, uint32_t st)
{ logf_("target", "pointer button 0x%x %s", b, st ? "pressed" : "released"); }
static void p_axis(void *d, struct wl_pointer *p, uint32_t t, uint32_t a, wl_fixed_t v)
{ logf_("target", "pointer axis %u %.2f", a, wl_fixed_to_double(v)); }
static void p_frame(void *d, struct wl_pointer *p) {}
static void p_src(void *d, struct wl_pointer *p, uint32_t s) {}
static void p_stop(void *d, struct wl_pointer *p, uint32_t t, uint32_t a) {}
static void p_disc(void *d, struct wl_pointer *p, uint32_t a, int32_t v) { logf_("target", "pointer axis_discrete %u %d", a, v); }
static const struct wl_pointer_listener ptr_l = {p_enter, p_leave, p_motion, p_button, p_axis, p_frame, p_src, p_stop, p_disc};

/* keyboard */
static void k_keymap(void *d, struct wl_keyboard *k, uint32_t f, int fd, uint32_t sz)
{ logf_("target", "keyboard keymap format=%u size=%u", f, sz); close(fd); }
static void k_enter(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *sf, struct wl_array *keys)
{ logf_("target", "keyboard enter"); }
static void k_leave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *sf) { logf_("target", "keyboard leave"); }
static void k_key(void *d, struct wl_keyboard *k, uint32_t s, uint32_t t, uint32_t key, uint32_t st)
{ logf_("target", "keyboard key %u %s", key, st ? "pressed" : "released"); }
static void k_mods(void *d, struct wl_keyboard *k, uint32_t s, uint32_t dep, uint32_t lat, uint32_t lock, uint32_t g)
{ logf_("target", "keyboard modifiers dep=0x%x lat=0x%x lock=0x%x group=%u", dep, lat, lock, g); }
static void k_rep(void *d, struct wl_keyboard *k, int32_t r, int32_t dl) {}
static const struct wl_keyboard_listener kbd_l = {k_keymap, k_enter, k_leave, k_key, k_mods, k_rep};

static void seat_caps(void *d, struct wl_seat *s, uint32_t caps)
{
    static struct wl_pointer *p;
    static struct wl_keyboard *k;
    logf_("target", "seat caps 0x%x", caps);
    if ((caps & WL_SEAT_CAPABILITY_POINTER) && !p) {
        p = wl_seat_get_pointer(s);
        wl_pointer_add_listener(p, &ptr_l, NULL);
    }
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !k) {
        k = wl_seat_get_keyboard(s);
        wl_keyboard_add_listener(k, &kbd_l, NULL);
    }
}
static void seat_name(void *d, struct wl_seat *s, const char *n) {}
static const struct wl_seat_listener seat_l = {seat_caps, seat_name};

static void wm_ping(void *d, struct xdg_wm_base *w, uint32_t s) { xdg_wm_base_pong(w, s); }
static const struct xdg_wm_base_listener wm_l = {wm_ping};
static void xs_conf(void *d, struct xdg_surface *xs, uint32_t s)
{
    xdg_surface_ack_configure(xs, s);
    configured = 1;
    draw();
    if (!frame_pending)
        request_frame();
    wl_surface_commit(surf);
}
static const struct xdg_surface_listener xs_l = {xs_conf};
static void tl_conf(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *st)
{
    if (w && h && (w != W || h != H)) {
        logf_("target", "toplevel configure %dx%d", w, h);
        W = w;
        H = h;
    }
    if (!W) {
        W = 640;
        H = 480;
    }
}
static void tl_close(void *d, struct xdg_toplevel *t) { exit(0); }
static void tl_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h) {}
static void tl_caps(void *d, struct xdg_toplevel *t, struct wl_array *c) {}
static const struct xdg_toplevel_listener tl_l = {tl_conf, tl_close, tl_bounds, tl_caps};

static void global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t ver)
{
    if (!strcmp(iface, "wl_compositor"))
        comp = wl_registry_bind(r, name, &wl_compositor_interface, 4);
    else if (!strcmp(iface, "wl_shm"))
        shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
    else if (!strcmp(iface, "xdg_wm_base")) {
        wm = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(wm, &wm_l, NULL);
    } else if (!strcmp(iface, "wl_seat") && !seat) {
        seat = wl_registry_bind(r, name, &wl_seat_interface, 5);
        wl_seat_add_listener(seat, &seat_l, NULL);
    }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) {}
static const struct wl_registry_listener reg_l = {global, global_remove};

int main(int argc, char **argv)
{
    double seconds = 30;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--animate") && i + 2 < argc) {
            anim_start = atof(argv[++i]);
            anim_stop = atof(argv[++i]);
        } else if (!strcmp(argv[i], "--seconds") && i + 1 < argc)
            seconds = atof(argv[++i]);
    }
    struct wl_display *d = connect_display();
    struct wl_registry *reg = wl_display_get_registry(d);
    wl_registry_add_listener(reg, &reg_l, NULL);
    wl_display_roundtrip(d);
    surf = wl_compositor_create_surface(comp);
    struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wm, surf);
    xdg_surface_add_listener(xs, &xs_l, NULL);
    struct xdg_toplevel *tl = xdg_surface_get_toplevel(xs);
    xdg_toplevel_add_listener(tl, &tl_l, NULL);
    xdg_toplevel_set_title(tl, "kde-rdp target");
    xdg_toplevel_set_app_id(tl, "kde-rdp-target");
    xdg_toplevel_set_fullscreen(tl, NULL);
    wl_surface_commit(surf);
    t0 = now_s();
    double end = t0 + seconds;
    while (now_s() < end) {
        while (wl_display_prepare_read(d) != 0)
            wl_display_dispatch_pending(d);
        wl_display_flush(d);
        struct pollfd pfd = {wl_display_get_fd(d), POLLIN};
        if (poll(&pfd, 1, 50) > 0)
            wl_display_read_events(d);
        else
            wl_display_cancel_read(d);
        wl_display_dispatch_pending(d);
        if (check_display(d, "target"))
            return 1;
        /* kick the animation if it was idle when the window started */
        if (configured && animating() && !frame_pending) {
            draw();
            request_frame();
            wl_surface_commit(surf);
        }
    }
    logf_("target", "exit, frames drawn=%u", frames_drawn);
    return 0;
}
