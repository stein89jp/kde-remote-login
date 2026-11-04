/* fake-input: authenticate and inject a scripted sequence via
 * org_kde_kwin_fake_input. Honours WAYLAND_SOCKET (fd handoff).
 *   fake-input [--no-auth] [--cursor-wiggle SECONDS]
 * Default sequence: absolute motion to (100,200) and (640.5,360.25),
 * relative motion (+10,+5), left click, vertical axis 15, horizontal axis -5,
 * key KEY_A press/release (evdev 30), keysym 'b' (0x62), keysym U+00E9
 * eacute (0xe9) and keysym U+263A (0x100263a, not in the keymap). */
#include "common.h"
#include "fake-input-client.h"
#include <linux/input-event-codes.h>

static struct org_kde_kwin_fake_input *fake;
static uint32_t ver;

static void global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v)
{
    if (!strcmp(iface, org_kde_kwin_fake_input_interface.name)) {
        ver = v;
        fake = wl_registry_bind(r, name, &org_kde_kwin_fake_input_interface, v < 6 ? v : 6);
    }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) {}
static const struct wl_registry_listener reg_l = {global, global_remove};

static void step(struct wl_display *d, const char *what)
{
    wl_display_roundtrip(d);
    logf_("inject", "%s", what);
    if (check_display(d, "inject"))
        exit(1);
    usleep(150 * 1000);
}

int main(int argc, char **argv)
{
    int auth = 1;
    double wiggle = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--no-auth"))
            auth = 0;
        else if (!strcmp(argv[i], "--cursor-wiggle") && i + 1 < argc)
            wiggle = atof(argv[++i]);
    }
    struct wl_display *d = connect_display();
    struct wl_registry *reg = wl_display_get_registry(d);
    wl_registry_add_listener(reg, &reg_l, NULL);
    wl_display_roundtrip(d);
    if (!fake) {
        logf_("inject", "org_kde_kwin_fake_input NOT advertised");
        return 1;
    }
    logf_("inject", "bound org_kde_kwin_fake_input v%u auth=%d", ver, auth);
    if (auth)
        org_kde_kwin_fake_input_authenticate(fake, "kde-rdp", "remote desktop input");
    if (wiggle > 0) {
        /* cursor-only movement for the screencast cursor-metadata test */
        /* one monotonic sweep: a back-and-forth wiggle triggers KWin's
         * shake-to-find-cursor effect, which renders an enlarged cursor
         * into the frames instead of SPA_META_Cursor */
        double start = now_s(), end = start + wiggle;
        while (now_s() < end) {
            double f = (now_s() - start) / wiggle;
            org_kde_kwin_fake_input_pointer_motion_absolute(fake, wl_fixed_from_double(300 + 600 * f),
                                                            wl_fixed_from_double(300 + 200 * f));
            wl_display_flush(d);
            usleep(10 * 1000);
        }
        wl_display_roundtrip(d);
        logf_("inject", "wiggle done");
        return 0;
    }
    org_kde_kwin_fake_input_pointer_motion_absolute(fake, wl_fixed_from_int(100), wl_fixed_from_int(200));
    step(d, "sent motion_absolute 100,200");
    org_kde_kwin_fake_input_pointer_motion_absolute(fake, wl_fixed_from_double(640.5), wl_fixed_from_double(360.25));
    step(d, "sent motion_absolute 640.5,360.25");
    org_kde_kwin_fake_input_pointer_motion(fake, wl_fixed_from_int(10), wl_fixed_from_int(5));
    step(d, "sent pointer_motion +10,+5");
    org_kde_kwin_fake_input_button(fake, BTN_LEFT, WL_POINTER_BUTTON_STATE_PRESSED);
    org_kde_kwin_fake_input_button(fake, BTN_LEFT, WL_POINTER_BUTTON_STATE_RELEASED);
    step(d, "sent button BTN_LEFT press/release");
    org_kde_kwin_fake_input_axis(fake, WL_POINTER_AXIS_VERTICAL_SCROLL, wl_fixed_from_int(15));
    step(d, "sent axis vertical 15");
    org_kde_kwin_fake_input_axis(fake, WL_POINTER_AXIS_HORIZONTAL_SCROLL, wl_fixed_from_int(-5));
    step(d, "sent axis horizontal -5");
    if (ver >= 4) {
        org_kde_kwin_fake_input_keyboard_key(fake, KEY_A, WL_KEYBOARD_KEY_STATE_PRESSED);
        org_kde_kwin_fake_input_keyboard_key(fake, KEY_A, WL_KEYBOARD_KEY_STATE_RELEASED);
        step(d, "sent keyboard_key KEY_A(30) press/release");
        org_kde_kwin_fake_input_keyboard_key(fake, KEY_LEFTSHIFT, WL_KEYBOARD_KEY_STATE_PRESSED);
        org_kde_kwin_fake_input_keyboard_key(fake, KEY_A, WL_KEYBOARD_KEY_STATE_PRESSED);
        org_kde_kwin_fake_input_keyboard_key(fake, KEY_A, WL_KEYBOARD_KEY_STATE_RELEASED);
        org_kde_kwin_fake_input_keyboard_key(fake, KEY_LEFTSHIFT, WL_KEYBOARD_KEY_STATE_RELEASED);
        step(d, "sent keyboard_key Shift+A");
    }
    if (ver >= 6) {
        uint32_t syms[] = {0x62, 0xe9, 0x100263a};
        const char *names[] = {"b", "eacute", "U+263A"};
        for (int i = 0; i < 3; i++) {
            org_kde_kwin_fake_input_keyboard_keysym(fake, syms[i], WL_KEYBOARD_KEY_STATE_PRESSED);
            org_kde_kwin_fake_input_keyboard_keysym(fake, syms[i], WL_KEYBOARD_KEY_STATE_RELEASED);
            char msg[64];
            snprintf(msg, sizeof msg, "sent keyboard_keysym 0x%x (%s)", syms[i], names[i]);
            step(d, msg);
        }
    }
    logf_("inject", "done");
    return 0;
}
