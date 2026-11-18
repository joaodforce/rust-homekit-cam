# Static x86_64 Linux binary from the Mac (cargo-zigbuild), deployed over SSH.
#   make deploy HOST=root@<lxc ip>
# rustup (with the musl target) ahead of Homebrew's rust
export PATH := /opt/homebrew/opt/rustup/bin:$(PATH)
TARGET := x86_64-unknown-linux-musl
BIN    := target/$(TARGET)/release/rust-homekit-cam
HOST   ?= root@rust-homekit-cam

build:
	cargo zigbuild --release --target $(TARGET)

deploy: build
	scp $(BIN) $(HOST):/usr/local/bin/rust-homekit-cam.new
	scp deploy/rust-homekit-cam.service $(HOST):/etc/systemd/system/rust-homekit-cam.service
	ssh $(HOST) 'mv /usr/local/bin/rust-homekit-cam.new /usr/local/bin/rust-homekit-cam && systemctl daemon-reload && systemctl enable rust-homekit-cam && systemctl restart rust-homekit-cam'

logs:
	ssh $(HOST) journalctl -u rust-homekit-cam -f

.PHONY: build deploy logs
