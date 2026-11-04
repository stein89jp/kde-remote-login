/* Shared helpers for the KWin lab probes. */
#pragma once
#define _GNU_SOURCE
#include <errno.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>

static inline double now_s(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static inline void logf_(const char *tag, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    fprintf(stdout, "[%s %.3f] ", tag, now_s());
    vfprintf(stdout, fmt, ap);
    fputc('\n', stdout);
    fflush(stdout);
    va_end(ap);
}

static inline void die(const char *msg)
{
    fprintf(stderr, "fatal: %s (%s)\n", msg, strerror(errno));
    exit(1);
}

/* Connect: WAYLAND_SOCKET (fd handoff) is honoured by wl_display_connect. */
static inline struct wl_display *connect_display(void)
{
    struct wl_display *d = wl_display_connect(NULL);
    if (!d)
        die("wl_display_connect");
    return d;
}

/* Report protocol errors after a roundtrip. Returns nonzero on error. */
static inline int check_display(struct wl_display *d, const char *tag)
{
    int err = wl_display_get_error(d);
    if (!err)
        return 0;
    const struct wl_interface *iface = NULL;
    uint32_t id = 0;
    uint32_t code = wl_display_get_protocol_error(d, &iface, &id);
    logf_(tag, "DISPLAY ERROR errno=%d protocol_code=%u iface=%s id=%u", err, code,
          iface ? iface->name : "-", id);
    return 1;
}

static inline int make_shm_fd(size_t size)
{
    int fd = memfd_create("probe-shm", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, size) < 0)
        die("memfd");
    return fd;
}
