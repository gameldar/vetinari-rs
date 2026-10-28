# Build and install vetinari, vetinarid and the systemd user units.
#
#   make install          everything below, then restart vetinarid
#   make install-tui      vetinari
#   make install-daemon   the web page, then vetinarid, which embeds it
#   make install-units    the unit files, then daemon-reload
#   make restart          restart vetinarid
#
# Installing the units overwrites the copies in UNIT_DIR; keep local
# changes in a drop-in (systemctl --user edit vetinarid), which survives.
# The autognomus dev stack's unit is installed but never restarted here:
# a restart forgets every run.

CARGO ?= cargo
TRUNK ?= trunk
SYSTEMCTL ?= systemctl --user
UNIT_DIR ?= $(HOME)/.config/systemd/user
UNITS := $(wildcard docs/systemd/*.service)

.PHONY: install install-tui install-daemon install-units web restart

install: install-tui install-daemon install-units restart

install-tui:
	$(CARGO) install --locked --path crates/vetinari-tui

install-daemon: web
	$(CARGO) install --locked --path crates/vetinari-daemon

web:
	cd crates/vetinari-web && $(TRUNK) build --release

install-units:
	install -d $(UNIT_DIR)
	install -m 644 $(UNITS) $(UNIT_DIR)/
	$(SYSTEMCTL) daemon-reload

restart:
	$(SYSTEMCTL) restart vetinarid
