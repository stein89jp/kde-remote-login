CC ?= cc
CFLAGS ?= -O2 -g
CPPFLAGS += -D_FORTIFY_SOURCE=3
WARNINGS = -std=c17 -Wall -Wextra -Werror -Wformat=2 -Wshadow -Wconversion
LDLIBS = -lpam -lsystemd
# Tools run through config/session-containers in remote desktops.
CONTAINER_TOOLS = podman buildah toolbox docker

all: kde-rdp-broker
kde-rdp-broker: broker.c
	$(CC) $(CPPFLAGS) $(CFLAGS) $(WARNINGS) $< $(LDFLAGS) $(LDLIBS) -o $@
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
	install -Dm755 kde-rdp-broker "$(DESTDIR)/usr/lib/kde-rdp/kde-rdp-broker"
	install -Dm755 config/session-worker "$(DESTDIR)/usr/lib/kde-rdp/session-worker"
	install -Dm755 config/session-audio "$(DESTDIR)/usr/lib/kde-rdp/session-audio"
	install -Dm755 config/session-microphone "$(DESTDIR)/usr/lib/kde-rdp/session-microphone"
	install -Dm755 config/session-plasma "$(DESTDIR)/usr/lib/kde-rdp/session-plasma"
	install -Dm755 config/session-portals "$(DESTDIR)/usr/lib/kde-rdp/session-portals"
	install -Dm755 config/session-environment "$(DESTDIR)/usr/lib/kde-rdp/session-environment"
	install -Dm755 config/session-autostart "$(DESTDIR)/usr/lib/kde-rdp/session-autostart"
	install -Dm755 config/session-input-method "$(DESTDIR)/usr/lib/kde-rdp/session-input-method"
	install -Dm755 config/session-containers "$(DESTDIR)/usr/lib/kde-rdp/session-containers"
	install -dm755 "$(DESTDIR)/usr/lib/kde-rdp/containers"
	for tool in $(CONTAINER_TOOLS); do ln -sfn ../session-containers "$(DESTDIR)/usr/lib/kde-rdp/containers/$$tool" || exit 1; done
	install -Dm644 config/kde-rdp-portals.conf "$(DESTDIR)/usr/share/xdg-desktop-portal/kde-rdp-portals.conf"
	install -Dm644 config/kde-rdp.pam "$(DESTDIR)/etc/pam.d/kde-rdp"
	install -Dm644 config/renderer.conf "$(DESTDIR)/etc/kde-rdp/renderer.conf"
	install -Dm644 config/broker.conf "$(DESTDIR)/etc/kde-rdp/broker.conf"
	install -Dm644 config/kde-rdp-broker.service "$(DESTDIR)/usr/lib/systemd/system/kde-rdp-broker.service"
.PHONY: all check clean install
