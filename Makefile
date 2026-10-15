# healthd — build, lint, test and install.
#
# healthd is a *per-user* daemon (it talks to your D-Bus session for desktop
# notifications and keeps its data under $HOME), so everything installs into
# your home directory by default and no root / doas is needed.
#
#   make build            optimised build of all three binaries
#   make web              rebuild the dashboard in web/ (needs Node; see below)
#   make web-check        type-check and unit-test the dashboard
#   make web-verify       fail if the committed web/dist is out of date
#   make install          install binaries, dinit user service, example config
#   make uninstall        remove what `install` put in place
#   make lint             rustfmt --check + clippy with warnings denied
#   make fmt              reformat the code in place
#   make test             run every unit test
#
# The dashboard is a Svelte app in web/ whose *built* output, web/dist, is
# committed and compiled into the daemon (include_bytes!). So `make build` needs
# only Rust; Node is needed only when you change something under web/src.
#
# Override paths on the command line, e.g.  make install PREFIX=/usr/local
# (use DESTDIR=... for staged/package builds).

CARGO       ?= cargo
NPM         ?= npm
PREFIX      ?= $(HOME)/.local
BINDIR      ?= $(PREFIX)/bin
CONFIG_HOME ?= $(or $(XDG_CONFIG_HOME),$(HOME)/.config)
# Where the *user* dinit instance looks for service descriptions.
DINIT_DIR   ?= $(CONFIG_HOME)/dinit.d
CONF_DIR    ?= $(CONFIG_HOME)/healthd
TARGET_DIR  ?= $(or $(CARGO_TARGET_DIR),target)

BINS := healthd healthctl health-tui

.PHONY: all build install uninstall lint fmt test clean web web-check web-verify

all: build

# --locked: build exactly what Cargo.lock says, so builds are reproducible.
build:
	$(CARGO) build --release --workspace --locked

test:
	$(CARGO) test --workspace --locked

# `npm ci` installs exactly what package-lock.json says (the npm counterpart of
# --locked) and re-runs only when the lockfile changes.
web/node_modules: web/package-lock.json
	cd web && $(NPM) ci
	touch web/node_modules

# Rebuild web/dist. Commit the result: the daemon embeds it at compile time.
web: web/node_modules
	cd web && $(NPM) run build

web-check: web/node_modules
	cd web && $(NPM) run check
	cd web && $(NPM) test

# Builds into a scratch directory and compares, so a forgotten `make web` after
# editing web/src is caught before it ships a stale dashboard.
web-verify: web/node_modules
	rm -rf web/.verify
	cd web && $(NPM) run build -- --outDir .verify
	diff -r web/dist web/.verify || { rm -rf web/.verify; echo "web/dist is out of date: run 'make web'" >&2; exit 1; }
	rm -rf web/.verify

# Lint = formatting check + clippy. Any warning fails the build.
lint:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

fmt:
	$(CARGO) fmt --all

# The service file is a template: @BINDIR@ becomes the real install path, since
# dinit needs an absolute `command`. config.toml itself is never installed or
# overwritten: with no file the daemon runs on its built-in defaults, and the
# example is there to copy from.
install: build
	install -d $(DESTDIR)$(BINDIR) $(DESTDIR)$(DINIT_DIR) $(DESTDIR)$(CONF_DIR)
	for b in $(BINS); do install -m 755 $(TARGET_DIR)/release/$$b $(DESTDIR)$(BINDIR)/$$b; done
	sed 's|@BINDIR@|$(BINDIR)|g' dinit/healthd.in > $(DESTDIR)$(DINIT_DIR)/healthd
	chmod 644 $(DESTDIR)$(DINIT_DIR)/healthd
	install -m 644 config/healthd.toml $(DESTDIR)$(CONF_DIR)/config.toml.example
	@echo
	@echo "Installed. Next: dinitctl enable healthd && dinitctl start healthd"
	@echo "(see README.md for the doas rule NVMe SMART data needs)"

# Leaves your config.toml and the database alone; they are your data.
uninstall:
	for b in $(BINS); do rm -f $(DESTDIR)$(BINDIR)/$$b; done
	rm -f $(DESTDIR)$(DINIT_DIR)/healthd
	rm -f $(DESTDIR)$(CONF_DIR)/config.toml.example
	@echo "Removed. Kept $(CONF_DIR)/config.toml and ~/.local/share/healthd/health.db if present."

clean:
	$(CARGO) clean
