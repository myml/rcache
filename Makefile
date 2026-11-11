# rcache install rules.
#
#   make            build the release binary (cargo)
#   make test       run the test suite
#   make install    install binary, systemd unit and sample config
#   make uninstall  remove what `install` put down

PREFIX      ?= /usr
SBINDIR     ?= $(PREFIX)/sbin
SYSTEMD     ?= $(PREFIX)/lib/systemd/system
CONFDIR     ?= /etc/rcache
MODPROBE    ?= $(PREFIX)/lib/modules-load.d

BIN         := target/release/rcache

.PHONY: all build test clean install uninstall fmt lint

all: build

build:
	cargo build --release

test:
	cargo test --release

lint:
	cargo clippy --release --all-targets -- -D warnings

fmt:
	cargo fmt

clean:
	cargo clean

install: build
	install -d -m 0755 $(DESTDIR)$(SBINDIR)
	install -m 0755 $(BIN) $(DESTDIR)$(SBINDIR)/rcache
	install -d -m 0755 $(DESTDIR)$(SYSTEMD)
	install -m 0644 contrib/rcache@.service $(DESTDIR)$(SYSTEMD)/rcache@.service
	install -d -m 0755 $(DESTDIR)$(CONFDIR)
	@if [ -e $(DESTDIR)$(CONFDIR)/0.toml ]; then \
		echo "keeping existing $(CONFDIR)/0.toml"; \
	else \
		install -m 0644 contrib/rcache.toml.example $(DESTDIR)$(CONFDIR)/0.toml; \
		echo "installed sample config to $(CONFDIR)/0.toml -- edit it before enabling"; \
	fi
	install -d -m 0755 $(DESTDIR)$(MODPROBE)
	echo "ublk_drv" > $(DESTDIR)$(MODPROBE)/rcache.conf
	@if command -v systemctl >/dev/null 2>&1; then systemctl daemon-reload || true; fi
	@echo
	@echo "next: edit $(CONFDIR)/0.toml, then"
	@echo "      systemctl enable --now rcache@0.service"
	@echo "      rcache status --id 0"

uninstall:
	rm -f $(DESTDIR)$(SBINDIR)/rcache
	rm -f $(DESTDIR)$(SYSTEMD)/rcache@.service
	rm -f $(DESTDIR)$(MODPROBE)/rcache.conf
	@echo "config in $(CONFDIR) left in place; remove it manually if unwanted"
