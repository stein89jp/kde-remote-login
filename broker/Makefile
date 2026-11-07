CC ?= cc
CFLAGS ?= -O2 -g
CPPFLAGS += -D_FORTIFY_SOURCE=3
WARNINGS = -std=c17 -Wall -Wextra -Werror -Wformat=2 -Wshadow -Wconversion
LDLIBS = -lpam -lsystemd
# Tools run through config/session-containers in remote desktops.
CONTAINER_TOOLS = podman buildah toolbox docker
# Install layout. The defaults are the Arch package's. The Fedora/Bazzite RPM
# passes LIBEXECDIR=/usr/libexec/kde-rdp (bin_t under SELinux) and its own PAM
# stack. The helpers and the broker unit name each other by absolute path, so
# install rewrites the Arch prefix to LIBEXECDIR (a no-op for the defaults).
LIBEXECDIR ?= /usr/lib/kde-rdp
UNITDIR ?= /usr/lib/systemd/system
PAM_FILE ?= config/kde-rdp.pam
SESSION_HELPERS = session-worker session-audio session-microphone session-plasma \
	session-portals session-environment session-autostart session-input-method \
	session-containers
SUBSTITUTE = sed -e 's|/usr/lib/kde-rdp|$(LIBEXECDIR)|g'

all: kde-rdp-broker
kde-rdp-broker: broker.c
	$(CC) $(CPPFLAGS) -DWORKER='"$(LIBEXECDIR)/session-worker"' $(CFLAGS) $(WARNINGS) $< $(LDFLAGS) $(LDLIBS) -o $@
broker-tests: tests.c broker.c
	$(CC) $(CPPFLAGS) $(CFLAGS) $(WARNINGS) -Wno-unused-function tests.c $(LDFLAGS) $(LDLIBS) -o $@
check: broker-tests
	rm -rf broker-test-root
	./broker-tests
clean:
	rm -f kde-rdp-broker broker-tests
	rm -rf broker-test-root
install: kde-rdp-broker
	@case "$(DESTDIR)" in /*) ;; *) printf '%s\n' 'Set an absolute staging DESTDIR; direct host installation is disabled.' >&2; exit 1 ;; esac
	install -Dm755 kde-rdp-broker "$(DESTDIR)$(LIBEXECDIR)/kde-rdp-broker"
	install -dm755 "$(DESTDIR)$(LIBEXECDIR)"
	for helper in $(SESSION_HELPERS); do \
		$(SUBSTITUTE) "config/$$helper" > "$(DESTDIR)$(LIBEXECDIR)/$$helper" && \
		chmod 755 "$(DESTDIR)$(LIBEXECDIR)/$$helper" || exit 1; \
	done
	install -dm755 "$(DESTDIR)$(LIBEXECDIR)/containers"
	for tool in $(CONTAINER_TOOLS); do ln -sfn ../session-containers "$(DESTDIR)$(LIBEXECDIR)/containers/$$tool" || exit 1; done
	install -Dm644 config/kde-rdp-portals.conf "$(DESTDIR)/usr/share/xdg-desktop-portal/kde-rdp-portals.conf"
	install -Dm644 "$(PAM_FILE)" "$(DESTDIR)/etc/pam.d/kde-rdp"
	install -Dm644 config/renderer.conf "$(DESTDIR)/etc/kde-rdp/renderer.conf"
	install -Dm644 config/broker.conf "$(DESTDIR)/etc/kde-rdp/broker.conf"
	install -dm755 "$(DESTDIR)$(UNITDIR)"
	$(SUBSTITUTE) config/kde-rdp-broker.service > "$(DESTDIR)$(UNITDIR)/kde-rdp-broker.service"
	chmod 644 "$(DESTDIR)$(UNITDIR)/kde-rdp-broker.service"
.PHONY: all check clean install
