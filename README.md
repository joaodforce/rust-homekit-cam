# rust-homekit-cam

Put an RTSP **doorbell or camera** in Apple Home as a native HomeKit accessory: ring
notifications, live video and audio, two-way audio, snapshots, motion and **HomeKit Secure
Video**. Video is **never transcoded**. Each camera runs as one small static binary (~2 MB):
no Node, no ffmpeg, no Docker.

It works with any camera that serves **H.264 over RTSP**. ONVIF cameras and UniFi Protect get
extras through optional backends:

| Feature | Any H.264 RTSP camera | ONVIF | UniFi Protect |
|---|---|---|---|
| Live video (LAN and remote) | ✓ up to three streams, picked by bitrate | ✓ streams found automatically | ✓ High / Medium / Low |
| Camera audio | ✓ if the stream has Opus | ✓ if the stream has Opus | ✓ |
| Two-way audio | – | – (needs decoding) | ✓ |
| Doorbell ring | ✓ `POST /ring` | ✓ + event topics | ✓ from Protect |
| Motion sensor | ✓ `POST /motion` | ✓ camera's motion events | ✓ smart detections |
| HomeKit Secure Video | ✓ | ✓ | ✓ |
| Snapshots | ✓ JPEG URL | ✓ ONVIF or JPEG URL | ✓ Protect API |

Any camera can be a doorbell (`doorbell = true`). Tested with a UniFi Protect Doorbell Lite and
an Intelbras (Dahua) ONVIF camera, iPhones and home hubs, in a 256 MB Debian LXC.

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

Fill in the name and how to reach the camera:
- **ONVIF cameras**: just the camera's address and login in `[onvif]`; `rust-homekit-cam
  onvif-discover` finds them ([docs/onvif.md](docs/onvif.md)).
- **UniFi Protect**: `rust-homekit-cam protect-cameras` prints a ready-made block for every
  camera ([docs/unifi-protect.md](docs/unifi-protect.md)).
- **Anything else**: the RTSP URL ([docs/generic-cameras.md](docs/generic-cameras.md)).

Check it with `rust-homekit-cam probe <config>` (the streams) and `rust-homekit-cam snapshot
<config> test.jpg`. Every option is in
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
- [ONVIF cameras](docs/onvif.md): discovery, streams, snapshots and motion from ONVIF
- [UniFi Protect](docs/unifi-protect.md): API key, camera setup helper, two-way audio, events
- [Other cameras](docs/generic-cameras.md): RTSP and snapshot URLs, rings and motion from Home Assistant
- [Troubleshooting](docs/troubleshooting.md)
- [How it works](docs/how-it-works.md): HAP, streaming without transcoding, HKSV
- [Development](docs/development.md): building, testing, project layout

## License

GPL-3.0-only.
