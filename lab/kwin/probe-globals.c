/* probe-globals: list advertised globals, then bind and exercise the
 * restricted ones (org_kde_kwin_fake_input, zkde_screencast_unstable_v1).
 * Honours WAYLAND_SOCKET, so it can run as the receiver of fd-handoff.
 * Output lines "RESULT <name> <verdict>" are machine-checkable. */
#include "common.h"
#include "fake-input-client.h"
#include "zkde-screencast-unstable-v1-client.h"

static struct org_kde_kwin_fake_input *fake;
static struct zkde_screencast_unstable_v1 *sc;
static struct wl_output *output;
static uint32_t fake_ver, sc_ver;
static int restricted_seen;
static int stream_state; /* 0 pending, 1 created, -1 failed/closed */
static uint32_t node_id;

static void global(void *data, struct wl_registry *reg, uint32_t name, const char *iface, uint32_t ver)
{
    printf("global %-45s v%u\n", iface, ver);
    if (!strcmp(iface, org_kde_kwin_fake_input_interface.name)) {
        fake_ver = ver;
        fake = wl_registry_bind(reg, name, &org_kde_kwin_fake_input_interface, ver < 6 ? ver : 6);
        restricted_seen++;
    } else if (!strcmp(iface, zkde_screencast_unstable_v1_interface.name)) {
        sc_ver = ver;
        sc = wl_registry_bind(reg, name, &zkde_screencast_unstable_v1_interface, ver < 6 ? ver : 6);
        restricted_seen++;
    } else if (!strcmp(iface, "wl_output") && !output) {
        output = wl_registry_bind(reg, name, &wl_output_interface, 1);
    }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) {}
static const struct wl_registry_listener reg_l = {global, global_remove};

static void s_closed(void *d, struct zkde_screencast_stream_unstable_v1 *s) { stream_state = -1; logf_("globals", "stream closed"); }
static void s_created(void *d, struct zkde_screencast_stream_unstable_v1 *s, uint32_t node) { node_id = node; stream_state = 1; }
static void s_failed(void *d, struct zkde_screencast_stream_unstable_v1 *s, const char *e) { stream_state = -1; logf_("globals", "stream failed: %s", e); }
static void s_serial(void *d, struct zkde_screencast_stream_unstable_v1 *s, uint32_t hi, uint32_t lo) {}
static const struct zkde_screencast_stream_unstable_v1_listener s_l = {s_closed, s_created, s_failed, s_serial};

int main(int argc, char **argv)
{
    const char *ws = getenv("WAYLAND_SOCKET");
    printf("probe-globals pid=%d uid=%d via=%s\n", getpid(), getuid(), ws ? "WAYLAND_SOCKET(fd handoff)" : "socket path");
    struct wl_display *d = connect_display();
    struct wl_registry *reg = wl_display_get_registry(d);
    wl_registry_add_listener(reg, &reg_l, NULL);
    wl_display_roundtrip(d);
    printf("RESULT fake_input_advertised %s v%u\n", fake ? "yes" : "no", fake_ver);
    printf("RESULT screencast_advertised %s v%u\n", sc ? "yes" : "no", sc_ver);
    if (fake) {
        org_kde_kwin_fake_input_authenticate(fake, "kde-rdp probe", "lab authorization test");
        /* A harmless use: move the pointer. No error event exists, so the
         * evidence of use is the absence of a protocol error plus the
         * target-window test. */
        org_kde_kwin_fake_input_pointer_motion_absolute(fake, wl_fixed_from_int(10), wl_fixed_from_int(10));
        wl_display_roundtrip(d);
        printf("RESULT fake_input_use %s\n", check_display(d, "globals") ? "protocol-error" : "accepted");
    }
    if (sc && output) {
        struct zkde_screencast_stream_unstable_v1 *s =
            zkde_screencast_unstable_v1_stream_output(sc, output, ZKDE_SCREENCAST_UNSTABLE_V1_POINTER_METADATA);
        zkde_screencast_stream_unstable_v1_add_listener(s, &s_l, NULL);
        double end = now_s() + 5;
        while (stream_state == 0 && now_s() < end && wl_display_roundtrip(d) >= 0)
            ;
        if (stream_state == 1)
            printf("RESULT screencast_use created node=%u\n", node_id);
        else
            printf("RESULT screencast_use %s\n", stream_state < 0 ? "failed" : "timeout");
        zkde_screencast_stream_unstable_v1_close(s);
        wl_display_roundtrip(d);
    }
    return 0;
}
