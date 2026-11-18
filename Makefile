# Static x86_64 Linux binary (cargo-zigbuild), deployed over SSH to a systemd host.
#   make deploy HOST=root@<host>    binary + unit, restarts every camera instance
#   make logs HOST=root@<host> CAM=<name>
# rustup (with the musl target) ahead of Homebrew's rust
export PATH := /opt/homebrew/opt/rustup/bin:$(PATH)
TARGET := x86_64-unknown-linux-musl
BIN    := target/$(TARGET)/release/rust-homekit-cam
HOST   ?= root@rust-homekit-cam
CAM    ?= *

build:
	cargo zigbuild --release --target $(TARGET)

deploy: build
	scp $(BIN) $(HOST):/usr/local/bin/rust-homekit-cam.new
	scp deploy/rust-homekit-cam@.service $(HOST):/etc/systemd/system/
	ssh $(HOST) 'mv /usr/local/bin/rust-homekit-cam.new /usr/local/bin/rust-homekit-cam && install -d -m 700 /etc/rust-homekit-cam && systemctl daemon-reload && systemctl restart "rust-homekit-cam@*"'

logs:
	ssh $(HOST) 'journalctl -f -u "rust-homekit-cam@$(CAM)"'

.PHONY: build deploy logs
