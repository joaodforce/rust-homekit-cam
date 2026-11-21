# Development

```bash
cargo test          # unit tests: HAP crypto, TLV, SRTP (RFC 3711 vectors), RTSP/SDP, H.264,
                    # Opus packing, MP4 boxes, HDS/opack, Protect events
cargo build --release
```

Rust plus `ring` (TLS crypto), which needs a C compiler for the target. Run a dev instance on a Mac or Linux box on the same network as
your home hubs:

```bash
rust-homekit-cam serve dev/config.toml        # dev/ is gitignored: keep configs and state there
```

(set `state_dir = "dev/state"` in a dev config). Stop it before running the same identity
anywhere else.

## Building from source on Linux

With [rustup](https://rustup.rs) (Rust 1.85 or newer, for edition 2024) a C compiler and CMake
(`apt install build-essential cmake`; CMake builds the bundled libopus):

```bash
git clone https://github.com/joaodforce/rust-homekit-cam && cd rust-homekit-cam
cargo build --release --locked
sudo install -m 755 target/release/rust-homekit-cam /usr/local/bin/
sudo install -m 644 deploy/rust-homekit-cam@.service /etc/systemd/system/
sudo install -D -m 644 config.example.toml /usr/local/share/rust-homekit-cam/config.example.toml
sudo install -d -m 700 /etc/rust-homekit-cam
sudo systemctl daemon-reload
```

This build links against the system's glibc; the release binaries are static (musl), see below.

## Deploying from a Mac (development)

For quick iterations on a test host. `make deploy HOST=root@<host>` builds a static x86_64 musl binary with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) (`brew install zig
cargo-zigbuild cmake`, `rustup target add x86_64-unknown-linux-musl`), copies it and the systemd
template unit over SSH and restarts every `rust-homekit-cam@*` instance. The Makefile puts
rustup's toolchain ahead of Homebrew's Rust.

## CI and releases

GitHub Actions ([.github/workflows/ci.yml](../.github/workflows/ci.yml)) runs the tests on every
push and pull request, then builds static musl binaries with cargo-zigbuild for x86_64, aarch64
and armv7 (downloadable from the run's artifacts).

To release: bump `version` in `Cargo.toml`, commit, then tag and push:

```bash
git tag v0.2.0 && git push origin v0.2.0
```

The tag must match `Cargo.toml`. CI then publishes a GitHub release with one `.tar.gz` per
architecture (binary, unit file, example config, README, license) and `SHA256SUMS`;
[install.sh](../install.sh) installs the latest one.

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
| `src/events.rs`, `src/protect.rs`, `src/onvif.rs` | rings and motion; UniFi Protect integration API; ONVIF (SOAP, events, discovery) |
| `src/snapshot.rs`, `src/httpc.rs` | snapshots; small HTTP client |
| `deploy/rust-homekit-cam@.service` | systemd template unit, one instance per camera |

The HAP and pairing code comes from the author's crtd project (a HomeKit TV accessory for an office CRT).

## A fake camera for testing

`scripts/test-stream.sh <variant>` runs an RTSP server (MediaMTX) and an ffmpeg publisher in
Docker: `rtsp://127.0.0.1:8554/test` (1280x720) and `/test_low` (640x360), looping `./stream.mp4`
(any clip you drop there; gitignored) or a test pattern. Variants pick the audio codec: `opus`,
`opus-stereo`, `pcmu`, `pcma`, `aac8`, `aac16`, `aac44`, `aac48-stereo`, `g722`, `none`; `stop`
removes the containers. `KBPS` / `LOW_KBPS` set the two bitrates, `SLICES=1` one slice per frame
(ffmpeg's low-latency default cuts frames into several), `BEEP=1` a 1 s beep instead of the clip's
audio. Point a dev config's `[rtsp]` at it, then:

- `rust-homekit-cam audio-test <config>`: transcodes each stream's audio as a live view would
  (Opus 16 kHz / 20 ms and 24 kHz / 60 ms), decodes it back and prints packet rate, timestamp
  steps and level;
- `rust-homekit-cam record-test <config> out.mp4`: what a recording would contain.

Lessons it caught: HomeKit drops frames cut into many slices, and a home hub relaying a remote
viewer asks for ≤132 kbps and loses most packets of a bigger `low` stream.
