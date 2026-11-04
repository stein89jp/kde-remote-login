/* rdp-probe: displayless FreeRDP 3 client for the bridge integration lab.
 *   rdp-probe PORT WxH SECONDS SAVE.ppm   (credentials: $RDP_USER / $RDP_PASS)
 * Connects with NLA (the bridge's disposable mode), software GDI and no GFX
 * (plain bitmap updates), counts painted updates, moves the pointer across
 * the desktop and saves the final decoded desktop. Prints one JSON line. */
#define _POSIX_C_SOURCE 200809L
#include <freerdp/error.h>
#include <freerdp/freerdp.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/input.h>
#include <freerdp/settings.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <winpr/synch.h>
#include <winpr/sysinfo.h>

typedef struct {
    rdpContext context;
    BOOL initialized;
    unsigned paints, painted_pixels_k;
    unsigned resizes;
} ProbeContext;

static BOOL begin_paint(rdpContext *context)
{
    context->gdi->primary->hdc->hwnd->invalid->null = TRUE;
    context->gdi->primary->hdc->hwnd->ninvalid = 0;
    return TRUE;
}

static BOOL end_paint(rdpContext *context)
{
    ProbeContext *p = (ProbeContext *)context;
    HGDI_WND hwnd = context->gdi->primary->hdc->hwnd;
    if (!hwnd->invalid->null) {
        p->paints++;
        p->painted_pixels_k += (unsigned)((long)hwnd->invalid->w * hwnd->invalid->h / 1000);
    }
    return TRUE;
}

static BOOL desktop_resize(rdpContext *context)
{
    ProbeContext *p = (ProbeContext *)context;
    p->resizes++;
    return gdi_resize(context->gdi, freerdp_settings_get_uint32(context->settings, FreeRDP_DesktopWidth),
                      freerdp_settings_get_uint32(context->settings, FreeRDP_DesktopHeight));
}

static BOOL post_connect(freerdp *instance)
{
    ProbeContext *p = (ProbeContext *)instance->context;
    if (!gdi_init(instance, PIXEL_FORMAT_BGRA32))
        return FALSE;
    p->initialized = TRUE;
    instance->context->update->BeginPaint = begin_paint;
    instance->context->update->EndPaint = end_paint;
    instance->context->update->DesktopResize = desktop_resize;
    return TRUE;
}

static BOOL refuse_prompt(freerdp *instance, char **user, char **password, char **domain, rdp_auth_reason reason)
{
    (void)instance; (void)user; (void)password; (void)domain; (void)reason;
    return FALSE;
}

static void save(rdpGdi *gdi, const char *path, unsigned long *nonblack)
{
    FILE *f = fopen(path, "wb");
    *nonblack = 0;
    if (f)
        fprintf(f, "P6\n%u %u\n255\n", gdi->width, gdi->height);
    for (UINT32 y = 0; y < gdi->height; y++) {
        const BYTE *row = gdi->primary_buffer + (size_t)y * gdi->stride;
        for (UINT32 x = 0; x < gdi->width; x++) {
            const BYTE *px = row + x * 4;
            if (px[0] | px[1] | px[2])
                (*nonblack)++;
            if (f) {
                BYTE rgb[3] = {px[2], px[1], px[0]};
                fwrite(rgb, 1, 3, f);
            }
        }
    }
    if (f)
        fclose(f);
}

static int pump(freerdp *instance, ULONGLONG until)
{
    while (GetTickCount64() < until) {
        HANDLE events[64];
        DWORD count = freerdp_get_event_handles(instance->context, events, 64);
        if (!count || WaitForMultipleObjects(count, events, FALSE, 50) == WAIT_FAILED ||
            !freerdp_check_event_handles(instance->context))
            return 0;
    }
    return 1;
}

int main(int argc, char **argv)
{
    const char *user = getenv("RDP_USER"), *password = getenv("RDP_PASS");
    unsigned width = 1024, height = 768;
    if (argc != 5 || !user || !password || sscanf(argv[2], "%ux%u", &width, &height) != 2) {
        fprintf(stderr, "usage: RDP_USER=.. RDP_PASS=.. rdp-probe PORT WxH SECONDS SAVE.ppm\n");
        return 2;
    }
    freerdp *instance = freerdp_new();
    instance->ContextSize = sizeof(ProbeContext);
    instance->PostConnect = post_connect;
    instance->AuthenticateEx = refuse_prompt;
    if (!freerdp_context_new(instance))
        return 2;
    rdpSettings *s = instance->context->settings;
    BOOL ok = freerdp_settings_set_string(s, FreeRDP_ServerHostname, "127.0.0.1") &&
              freerdp_settings_set_uint32(s, FreeRDP_ServerPort, (UINT32)atoi(argv[1])) &&
              freerdp_settings_set_string(s, FreeRDP_Username, user) &&
              freerdp_settings_set_string(s, FreeRDP_Password, password) &&
              freerdp_settings_set_string(s, FreeRDP_Domain, "") &&
              freerdp_settings_set_bool(s, FreeRDP_NlaSecurity, TRUE) &&
              freerdp_settings_set_bool(s, FreeRDP_TlsSecurity, FALSE) &&
              freerdp_settings_set_bool(s, FreeRDP_RdpSecurity, FALSE) &&
              freerdp_settings_set_bool(s, FreeRDP_IgnoreCertificate, TRUE) &&
              freerdp_settings_set_bool(s, FreeRDP_SoftwareGdi, TRUE) &&
              freerdp_settings_set_bool(s, FreeRDP_SupportGraphicsPipeline, FALSE) &&
              freerdp_settings_set_bool(s, FreeRDP_DesktopResize, TRUE) &&
              freerdp_settings_set_uint32(s, FreeRDP_DesktopWidth, width) &&
              freerdp_settings_set_uint32(s, FreeRDP_DesktopHeight, height) &&
              freerdp_settings_set_uint32(s, FreeRDP_ColorDepth, 32) &&
              freerdp_settings_set_uint32(s, FreeRDP_TcpConnectTimeout, 5000);
    int result = 1;
    if (ok && freerdp_connect(instance)) {
        ProbeContext *p = (ProbeContext *)instance->context;
        ULONGLONG start = GetTickCount64(), end = start + (ULONGLONG)atoi(argv[3]) * 1000;
        pump(instance, start + 1500);
        unsigned first_second = p->paints;
        for (unsigned i = 0; i < 20 && GetTickCount64() < end; i++) {
            freerdp_input_send_mouse_event(instance->context->input, PTR_FLAGS_MOVE, (UINT16)(i * width / 20),
                                           (UINT16)(i * height / 20));
            pump(instance, GetTickCount64() + 50);
        }
        pump(instance, end);
        rdpGdi *gdi = instance->context->gdi;
        unsigned long nonblack = 0;
        save(gdi, argv[4], &nonblack);
        printf("{\"probe\":\"rdp\",\"width\":%u,\"height\":%u,\"paints_first_1500ms\":%u,\"paints\":%u,"
               "\"painted_kpixels\":%u,\"desktop_resizes\":%u,\"nonblack_pixels\":%lu}\n",
               gdi->width, gdi->height, first_second, p->paints, p->painted_pixels_k, p->resizes, nonblack);
        result = p->paints > 0 && nonblack > 0 && gdi->width == width && gdi->height == height ? 0 : 1;
        freerdp_disconnect(instance);
    } else {
        fprintf(stderr, "connection failed: %s\n",
                freerdp_get_last_error_name(freerdp_get_last_error(instance->context)));
    }
    if (((ProbeContext *)instance->context)->initialized)
        gdi_free(instance);
    freerdp_context_free(instance);
    freerdp_free(instance);
    return result;
}
