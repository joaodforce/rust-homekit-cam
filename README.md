# rust-homekit-cam

Put an RTSP **doorbell or camera** in Apple Home as a native HomeKit accessory: ring
notifications, live video and audio, two-way audio, snapshots, motion and **HomeKit Secure
Video**. Video is **never transcoded**. Each camera runs as one small static binary (~2 MB):
no Node, no ffmpeg, no Docker.

It works with any camera that serves **H.264 over RTSP**. UniFi Protect gets extras through an
optional backend:

| Feature | Any H.264 RTSP camera | UniFi Protect |
|---|---|---|
| Live video (LAN and remote) | ✓ one URL, or up to three picked by bitrate | ✓ High / Medium / Low channels |
| Camera audio | ✓ if the camera streams Opus | ✓ Protect's Opus track |
| Two-way audio | – (planned: RTSP back-channel) | ✓ talk-back session |
| Doorbell ring | ✓ `POST /ring` from anything (HA, Node-RED…) | ✓ from Protect's events |
| Motion sensor | ✓ `POST /motion` from anything | ✓ from Protect's smart detections |
| HomeKit Secure Video | ✓ (audio if the camera has AAC) | ✓ |
| Snapshots | ✓ from the camera's JPEG URL | ✓ from the Protect API |

Tested with a UniFi Protect Doorbell Lite, iPhones and home hubs, in a 256 MB Debian LXC.

## Requirements

- A camera (or NVR) with an **H.264** RTSP stream (H.265 won't work: iPhones need H.264 here)
- A Linux host with systemd **on the same network (L2) as your home hubs**, since HomeKit finds
  accessories by mDNS
- A home hub (Apple TV or HomePod) for remote access, notifications and HomeKit Secure Video

## Quick start

**1. Install.** Build on a Mac and deploy over SSH (`cargo-zigbuild` builds a static x86_64
binary):

```bash
make deploy HOST=root@<linux-host>
```

or build on the host itself (`cargo build --release`, then copy `target/release/rust-homekit-cam`
to `/usr/local/bin/` and `deploy/rust-homekit-cam@.service` to `/etc/systemd/system/`).

**2. Configure one camera.** Each camera is a file named after it:

```bash
install -d -m 700 /etc/rust-homekit-cam
cp config.example.toml /etc/rust-homekit-cam/frontdoor.toml
chmod 600 /etc/rust-homekit-cam/frontdoor.toml
```

Fill in the name and RTSP URL. UniFi Protect users: `rust-homekit-cam protect-cameras` prints a
ready-made block for every camera (see [docs/unifi-protect.md](docs/unifi-protect.md)). For other
cameras see [docs/generic-cameras.md](docs/generic-cameras.md). Every option is in
[docs/configuration.md](docs/configuration.md).

**3. Start it.**

```bash
systemctl enable --now rust-homekit-cam@frontdoor
rust-homekit-cam code /etc/rust-homekit-cam/frontdoor.toml     # the setup code
```

**4. Add it in the Home app.** Tap + → Add Accessory → More options… → your camera → enter
the setup code. For recordings, open the camera's settings → Recording Options → Stream &
Allow Recording.

**More cameras:** another file in `/etc/rust-homekit-cam/` with its own `hap_port` (and its
own `ring_listen` port if you use one), then `systemctl enable --now rust-homekit-cam@<name>`.
Each camera is its own process and its own accessory.

## Docs

- [Configuration reference](docs/configuration.md)
- [UniFi Protect](docs/unifi-protect.md): API key, camera setup helper, two-way audio, events
- [Other cameras](docs/generic-cameras.md): RTSP and snapshot URLs, rings and motion from Home Assistant
- [Troubleshooting](docs/troubleshooting.md)
- [How it works](docs/how-it-works.md): HAP, streaming without transcoding, HKSV
- [Development](docs/development.md): building, testing, project layout

## License

GPL-3.0-only.
