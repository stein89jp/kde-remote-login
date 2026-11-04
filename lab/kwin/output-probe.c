/* output-probe: list kde_output_device_v2 state, optionally resize the
 * (virtual) output at runtime through kde_output_management_v2.
 *   output-probe [--resize W H [REFRESH_MHZ]] [--mode-only W H]
 * --resize: set_custom_modes([W x H @ refresh]) + apply, then select the new
 *           mode with configuration.mode + apply (two configurations).
 * --mode-only: select an already-listed mode by size (no custom mode).
 * Uses generic dispatchers so every event of protocol v26 is accepted and
 * the interesting ones are printed. */
#include "common.h"
#include "kde-output-device-v2-client.h"
#include "kde-output-management-v2-client.h"

struct mode {
    struct kde_output_device_mode_v2 *p;
    int w, h, refresh, removed;
};
static struct mode modes[64];
static int nmodes;
static struct kde_output_device_v2 *dev;
static struct kde_output_management_v2 *mgmt;
static uint32_t mgmt_ver, reg_ver;
static int done_seen, cfg_result; /* 1 applied, -1 failed */

static struct mode *find_mode(void *p)
{
    for (int i = 0; i < nmodes; i++)
        if ((void *)modes[i].p == p)
            return &modes[i];
    return NULL;
}

static int mode_dispatch(const void *impl, void *target, uint32_t op, const struct wl_message *msg, union wl_argument *a)
{
    struct mode *m = find_mode(target);
    if (!m)
        return 0;
    if (!strcmp(msg->name, "size")) {
        m->w = a[0].i;
        m->h = a[1].i;
    } else if (!strcmp(msg->name, "refresh"))
        m->refresh = a[0].i;
    else if (!strcmp(msg->name, "removed")) {
        m->removed = 1;
        logf_("output", "mode removed %dx%d@%d", m->w, m->h, m->refresh);
    } else if (!strcmp(msg->name, "flags"))
        logf_("output", "mode %p flags 0x%x", target, a[0].u);
    else if (!strcmp(msg->name, "preferred"))
        logf_("output", "mode %p preferred", target);
    return 0;
}

static int dev_dispatch(const void *impl, void *target, uint32_t op, const struct wl_message *msg, union wl_argument *a)
{
    const char *n = msg->name;
    if (!strcmp(n, "mode")) {
        struct mode *m = &modes[nmodes++];
        m->p = (struct kde_output_device_mode_v2 *)a[0].o;
        wl_proxy_add_dispatcher((struct wl_proxy *)m->p, mode_dispatch, NULL, NULL);
    } else if (!strcmp(n, "current_mode")) {
        struct mode *m = find_mode(a[0].o);
        logf_("output", "current_mode %dx%d@%d", m ? m->w : -1, m ? m->h : -1, m ? m->refresh : -1);
    } else if (!strcmp(n, "name"))
        logf_("output", "name \"%s\"", a[0].s);
    else if (!strcmp(n, "geometry"))
        logf_("output", "geometry x=%d y=%d phys=%dx%dmm make=\"%s\" model=\"%s\" transform=%d", a[0].i, a[1].i,
              a[2].i, a[3].i, a[5].s, a[6].s, a[7].i);
    else if (!strcmp(n, "scale"))
        logf_("output", "scale %.3f", wl_fixed_to_double(a[0].f));
    else if (!strcmp(n, "enabled"))
        logf_("output", "enabled %d", a[0].i);
    else if (!strcmp(n, "capabilities"))
        logf_("output", "capabilities 0x%x (custom_modes=%s disable=%s)", a[0].u, (a[0].u & 0x2000) ? "yes" : "no", (a[0].u & 0x20000) ? "yes" : "no");
    else if (!strcmp(n, "uuid"))
        logf_("output", "uuid %s", a[0].s);
    else if (!strcmp(n, "serial_number") || !strcmp(n, "eisa_id"))
        logf_("output", "%s \"%s\"", n, a[0].s);
    else if (!strcmp(n, "done")) {
        done_seen++;
        for (int i = 0; i < nmodes; i++)
            if (!modes[i].removed)
                logf_("output", "  mode[%d] %dx%d@%dmHz", i, modes[i].w, modes[i].h, modes[i].refresh);
        logf_("output", "done");
    } else if (!strcmp(n, "removed"))
        logf_("output", "device removed");
    return 0;
}

static int reg_dispatch(const void *impl, void *target, uint32_t op, const struct wl_message *msg, union wl_argument *a)
{
    if (!strcmp(msg->name, "output") && !dev) {
        dev = (struct kde_output_device_v2 *)a[0].o;
        wl_proxy_add_dispatcher((struct wl_proxy *)dev, dev_dispatch, NULL, NULL);
        logf_("output", "registry announced kde_output_device_v2 v%u", wl_proxy_get_version((struct wl_proxy *)dev));
    }
    return 0;
}

static int cfg_dispatch(const void *impl, void *target, uint32_t op, const struct wl_message *msg, union wl_argument *a)
{
    if (!strcmp(msg->name, "applied"))
        cfg_result = 1;
    else if (!strcmp(msg->name, "failed"))
        cfg_result = -1;
    else if (!strcmp(msg->name, "failure_reason"))
        logf_("output", "failure_reason: %s", a[0].s);
    return 0;
}

static void global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v)
{
    if (!strcmp(iface, "kde_output_device_registry_v2")) {
        reg_ver = v;
        struct kde_output_device_registry_v2 *reg = wl_registry_bind(r, name, &kde_output_device_registry_v2_interface, v < 26 ? v : 26);
        wl_proxy_add_dispatcher((struct wl_proxy *)reg, reg_dispatch, NULL, NULL);
    } else if (!strcmp(iface, "kde_output_management_v2")) {
        mgmt_ver = v;
        mgmt = wl_registry_bind(r, name, &kde_output_management_v2_interface, v < 22 ? v : 22);
    } else if (!strcmp(iface, "kde_output_device_v2"))
        logf_("output", "NOTE: legacy per-output kde_output_device_v2 global v%u", v);
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) {}
static const struct wl_registry_listener reg_l = {global, global_remove};

static int apply(struct wl_display *d, struct kde_output_configuration_v2 *c, const char *what)
{
    cfg_result = 0;
    wl_proxy_add_dispatcher((struct wl_proxy *)c, cfg_dispatch, NULL, NULL);
    kde_output_configuration_v2_apply(c);
    double end = now_s() + 5;
    while (!cfg_result && now_s() < end && wl_display_roundtrip(d) >= 0)
        ;
    logf_("output", "%s -> %s", what, cfg_result > 0 ? "APPLIED" : cfg_result < 0 ? "FAILED" : "timeout");
    kde_output_configuration_v2_destroy(c);
    wl_display_roundtrip(d);
    return cfg_result;
}

static struct mode *mode_by_size(int w, int h)
{
    for (int i = nmodes - 1; i >= 0; i--)
        if (!modes[i].removed && modes[i].w == w && modes[i].h == h)
            return &modes[i];
    return NULL;
}

int main(int argc, char **argv)
{
    int rw = 0, rh = 0, rr = 60000, mode_only = 0;
    for (int i = 1; i < argc; i++) {
        if ((!strcmp(argv[i], "--resize") || !strcmp(argv[i], "--mode-only")) && i + 2 < argc) {
            mode_only = !strcmp(argv[i], "--mode-only");
            rw = atoi(argv[++i]);
            rh = atoi(argv[++i]);
            if (i + 1 < argc && argv[i + 1][0] != '-')
                rr = atoi(argv[++i]);
        }
    }
    struct wl_display *d = connect_display();
    struct wl_registry *r = wl_display_get_registry(d);
    wl_registry_add_listener(r, &reg_l, NULL);
    wl_display_roundtrip(d);
    wl_display_roundtrip(d);
    wl_display_roundtrip(d);
    logf_("output", "kde_output_device_registry_v2 v%u, kde_output_management_v2 v%u", reg_ver, mgmt_ver);
    if (!dev || !mgmt)
        return 1;
    if (!rw)
        return 0;
    if (!mode_only) {
        struct kde_mode_list_v2 *ml = kde_output_management_v2_create_mode_list(mgmt);
        kde_mode_list_v2_set_resolution(ml, rw, rh);
        kde_mode_list_v2_set_refresh_rate(ml, rr);
        kde_mode_list_v2_add_mode(ml);
        struct kde_output_configuration_v2 *c = kde_output_management_v2_create_configuration(mgmt);
        kde_output_configuration_v2_set_custom_modes(c, dev, ml);
        apply(d, c, "set_custom_modes");
        kde_mode_list_v2_destroy(ml);
        wl_display_roundtrip(d);
    }
    struct mode *m = mode_by_size(rw, rh);
    if (!m) {
        logf_("output", "no mode %dx%d listed", rw, rh);
        return 1;
    }
    struct kde_output_configuration_v2 *c = kde_output_management_v2_create_configuration(mgmt);
    kde_output_configuration_v2_mode(c, dev, m->p);
    int ok = apply(d, c, "select mode");
    wl_display_roundtrip(d);
    return ok > 0 ? 0 : 1;
}
