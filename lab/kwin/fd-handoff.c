/* fd-handoff: mirror of the production broker pattern.
 *   fd-handoff [--pipewire] [--linger SECONDS] -- program args...
 * --linger keeps the connector alive (not reaped) for SECONDS after sending.
 * A forked "connector" child connects to $XDG_RUNTIME_DIR/wayland-0 (and,
 * with --pipewire, pipewire-0), sends the fd(s) over SCM_RIGHTS on a
 * socketpair and _exits. The parent reaps it (so its pid no longer exists),
 * then execs the receiver with WAYLAND_SOCKET=<fd> (and KDE_RDP_PW_FD=<fd>).
 * The receiver is a different executable from the connector. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>

static int connect_unix(const char *name)
{
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path, sizeof a.sun_path, "%s/%s", getenv("XDG_RUNTIME_DIR"), name);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0); /* no CLOEXEC: receiver inherits */
    if (fd < 0 || connect(fd, (struct sockaddr *)&a, sizeof a) < 0) {
        perror(a.sun_path);
        _exit(2);
    }
    return fd;
}

int main(int argc, char **argv)
{
    int pw = 0, i = 1;
    double linger = 0;
    for (; i < argc; i++) {
        if (!strcmp(argv[i], "--pipewire"))
            pw = 1;
        else if (!strcmp(argv[i], "--linger") && i + 1 < argc)
            linger = atof(argv[++i]);
        else
            break;
    }
    if (i < argc && !strcmp(argv[i], "--"))
        i++;
    if (i >= argc) {
        fprintf(stderr, "usage: fd-handoff [--pipewire] -- prog args\n");
        return 2;
    }
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0)
        return perror("socketpair"), 1;
    pid_t child = fork();
    if (child == 0) {
        close(sv[0]);
        int fds[2] = {connect_unix("wayland-0"), pw ? connect_unix("pipewire-0") : -1};
        int n = pw ? 2 : 1;
        char cbuf[CMSG_SPACE(sizeof(int) * 2)] = {0};
        char b = 'x';
        struct iovec io = {&b, 1};
        struct msghdr m = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = CMSG_SPACE(sizeof(int) * n)};
        struct cmsghdr *c = CMSG_FIRSTHDR(&m);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int) * n);
        memcpy(CMSG_DATA(c), fds, sizeof(int) * n);
        if (sendmsg(sv[1], &m, 0) < 0)
            _exit(3);
        fprintf(stderr, "connector pid=%d connected and sent %d fd(s); exiting after %.1fs\n", getpid(), n, linger);
        if (linger > 0)
            usleep((useconds_t)(linger * 1e6));
        _exit(0);
    }
    close(sv[1]);
    char cbuf[CMSG_SPACE(sizeof(int) * 2)];
    char b;
    struct iovec io = {&b, 1};
    struct msghdr m = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf};
    if (recvmsg(sv[0], &m, 0) <= 0)
        return perror("recvmsg"), 1;
    int status = 0;
    if (linger <= 0)
        waitpid(child, &status, 0);
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    int fds[2] = {-1, -1};
    memcpy(fds, CMSG_DATA(c), c->cmsg_len - CMSG_LEN(0));
    char s[16];
    snprintf(s, sizeof s, "%d", fds[0]);
    setenv("WAYLAND_SOCKET", s, 1);
    if (pw) {
        snprintf(s, sizeof s, "%d", fds[1]);
        setenv("KDE_RDP_PW_FD", s, 1);
    }
    fprintf(stderr, "receiver pid=%d: connector %d reaped (exit %d, /proc/%d %s); exec %s\n", getpid(), child,
            WEXITSTATUS(status), child, access("/proc/self", F_OK) == 0 && kill(child, 0) < 0 ? "gone" : "present", argv[i]);
    execvp(argv[i], argv + i);
    perror("execvp");
    return 1;
}
