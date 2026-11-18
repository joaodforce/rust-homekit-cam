# Development

```bash
cargo test          # unit tests: HAP crypto, TLV, SRTP (RFC 3711 vectors), RTSP/SDP, H.264,
                    # Opus packing, MP4 boxes, HDS/opack, Protect events
cargo build --release
```

Pure Rust, no C dependencies. Run a dev instance on a Mac or Linux box on the same network as
your home hubs:

```bash
rust-homekit-cam serve dev/config.toml        # dev/ is gitignored: keep configs and state there
```

(set `state_dir = "dev/state"` in a dev config). Stop it before running the same identity
anywhere else.

## Deploying from a Mac

`make deploy HOST=root@<host>` builds a static x86_64 musl binary with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) (`brew install zig
cargo-zigbuild`, `rustup target add x86_64-unknown-linux-musl`), copies it and the systemd
template unit over SSH and restarts every `rust-homekit-cam@*` instance. The Makefile puts
rustup's toolchain ahead of Homebrew's Rust.

## Layout

| Path | What |
|---|---|
| `src/main.rs` | config, commands, wiring |
| `src/hap/` | HomeKit Accessory Protocol: pairing, encrypted sessions, HTTP, mDNS |
| `src/camera.rs` | the accessory: services, live-view TLVs |
| `src/stream.rs` | one live-view stream: RTSP → SRTP, audio, talk-back |
| `src/rtsp.rs`, `src/h264.rs` | RTSP client (Basic/Digest), H.264 RTP depacketizing / packetizing |
| `src/srtp.rs` | SRTP/SRTCP sender and receiver |
| `src/hksv.rs`, `src/recording.rs`, `src/mp4.rs`, `src/hds.rs` | HomeKit Secure Video: services, prebuffer, fMP4, HomeKit Data Stream |
| `src/events.rs`, `src/protect.rs` | rings and motion; UniFi Protect integration API |
| `src/snapshot.rs`, `src/httpc.rs` | snapshots; small HTTP client |
| `deploy/rust-homekit-cam@.service` | systemd template unit, one instance per camera |

The HAP and pairing code comes from the author's crtd project (a HomeKit TV accessory for an office CRT).
