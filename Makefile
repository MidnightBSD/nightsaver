# nightsaver Makefile -- works with BSD make and GNU make.
#
#   make                  build release binaries with cargo
#   make test             run cargo test
#   make install          install binaries, man pages, sample config, desktop files
#   make install-pam      install the PAM policy only (install already does this)
#   make install-compat-links   xscreensaver -> nightsaver symlinks
#   make uninstall
#
# Variables (override on the command line, e.g. make install PREFIX=/opt DESTDIR=/stage):
#   PREFIX    install prefix            (default /usr/local)
#   DESTDIR   staging root              (default empty)
#   CARGO     cargo executable          (default cargo)
#   LOCALBASE where libxcb etc. live    (default /usr/local; read by build.rs)
#   MANDIR    man page root             (default $(PREFIX)/man)

PREFIX?=	/usr/local
DESTDIR?=
CARGO?=		cargo
LOCALBASE?=	/usr/local
MANDIR?=	$(PREFIX)/man
INSTALL?=	install

BINDIR=		$(PREFIX)/bin
ETCDIR=		$(PREFIX)/etc
APPDIR=		$(PREFIX)/share/applications
AUTOSTARTDIR=	$(PREFIX)/etc/xdg/autostart
PAMDIR=		$(PREFIX)/etc/pam.d

all:
	LOCALBASE="$(LOCALBASE)" $(CARGO) build --release

test:
	LOCALBASE="$(LOCALBASE)" $(CARGO) test

clean:
	$(CARGO) clean

install:
	$(INSTALL) -d $(DESTDIR)$(BINDIR)
	$(INSTALL) -m 0755 target/release/nightsaver $(DESTDIR)$(BINDIR)/nightsaver
	$(INSTALL) -m 0755 target/release/nightsaver-command $(DESTDIR)$(BINDIR)/nightsaver-command
	$(INSTALL) -d $(DESTDIR)$(MANDIR)/man1 $(DESTDIR)$(MANDIR)/man5
	$(INSTALL) -m 0644 man/nightsaver.1 $(DESTDIR)$(MANDIR)/man1/nightsaver.1
	$(INSTALL) -m 0644 man/nightsaver-command.1 $(DESTDIR)$(MANDIR)/man1/nightsaver-command.1
	$(INSTALL) -m 0644 man/nightsaver.toml.5 $(DESTDIR)$(MANDIR)/man5/nightsaver.toml.5
	$(INSTALL) -d $(DESTDIR)$(ETCDIR)
	$(INSTALL) -m 0644 etc/nightsaver.toml.sample $(DESTDIR)$(ETCDIR)/nightsaver.toml.sample
	$(INSTALL) -d $(DESTDIR)$(APPDIR)
	$(INSTALL) -m 0644 etc/nightsaver.desktop $(DESTDIR)$(APPDIR)/nightsaver.desktop
	$(INSTALL) -m 0644 etc/nightsaver-lock.desktop $(DESTDIR)$(APPDIR)/nightsaver-lock.desktop
	$(INSTALL) -d $(DESTDIR)$(AUTOSTARTDIR)
	$(INSTALL) -m 0644 etc/autostart/nightsaver.desktop $(DESTDIR)$(AUTOSTARTDIR)/nightsaver.desktop
	@$(MAKE) install-pam

# PAM policy (also run by install); used automatically when the xscreensaver policy is absent.
# Never overwrites an existing file.
install-pam:
	$(INSTALL) -d $(DESTDIR)$(PAMDIR)
	@if [ -e $(DESTDIR)$(PAMDIR)/nightsaver ]; then \
		echo "$(DESTDIR)$(PAMDIR)/nightsaver exists; not overwriting"; \
	else \
		$(INSTALL) -m 0644 etc/pam.d/nightsaver $(DESTDIR)$(PAMDIR)/nightsaver; \
	fi

# Optional: make nightsaver answer to the xscreensaver names. Refuses to replace a real
# file (i.e. the installed xscreensaver package).
install-compat-links:
	@$(INSTALL) -d $(DESTDIR)$(BINDIR); \
	rc=0; \
	for pair in xscreensaver:nightsaver xscreensaver-command:nightsaver-command; do \
		link=$${pair%%:*}; target=$${pair##*:}; \
		path=$(DESTDIR)$(BINDIR)/$$link; \
		if [ -e "$$path" ] && [ ! -L "$$path" ]; then \
			echo "$$path is a real file (the xscreensaver package?); not replacing it."; \
			echo "  Stop it with 'xscreensaver-command -exit', remove the package"; \
			echo "  (e.g. 'mport delete xscreensaver'), then re-run 'make install-compat-links'."; \
			rc=1; \
		else \
			ln -sf $$target "$$path" && echo "$$path -> $$target"; \
		fi; \
	done; \
	exit $$rc

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/nightsaver $(DESTDIR)$(BINDIR)/nightsaver-command
	rm -f $(DESTDIR)$(MANDIR)/man1/nightsaver.1 $(DESTDIR)$(MANDIR)/man1/nightsaver-command.1
	rm -f $(DESTDIR)$(MANDIR)/man5/nightsaver.toml.5
	rm -f $(DESTDIR)$(ETCDIR)/nightsaver.toml.sample
	rm -f $(DESTDIR)$(APPDIR)/nightsaver.desktop $(DESTDIR)$(APPDIR)/nightsaver-lock.desktop
	rm -f $(DESTDIR)$(AUTOSTARTDIR)/nightsaver.desktop
	@for link in xscreensaver xscreensaver-command; do \
		if [ -L $(DESTDIR)$(BINDIR)/$$link ]; then rm -f $(DESTDIR)$(BINDIR)/$$link; fi; \
	done
	@if cmp -s etc/pam.d/nightsaver $(DESTDIR)$(PAMDIR)/nightsaver 2>/dev/null; then \
		rm -f $(DESTDIR)$(PAMDIR)/nightsaver; \
	fi

.PHONY: all test clean install install-pam install-compat-links uninstall
