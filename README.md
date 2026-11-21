# rust-homekit-cam

Put an RTSP **doorbell or camera** in Apple Home as a native HomeKit accessory: ring
notifications, live video and audio, two-way audio, snapshots, motion and **HomeKit Secure
Video**. Video is **never transcoded** (camera audio can be, if you ask). Each camera runs as one small static binary (~3.5 MB):
no Node, no ffmpeg, no Docker.

It works with any camera that serves **H.264 over RTSP**. ONVIF cameras and UniFi Protect get
extras through optional backends:

| Feature | Any H.264 RTSP camera | ONVIF | UniFi Protect |
|---|---|---|---|
| Live video (LAN and remote) | ✓ up to three streams, picked by bitrate | ✓ streams found automatically | ✓ High / Medium / Low |
| Camera audio | ✓ Opus as is; G.711/AAC with `transcode_audio` | ✓ Opus as is; G.711/AAC with `transcode_audio` | ✓ |
| Two-way audio | – | – (needs decoding) | ✓ |
| Doorbell ring | ✓ `POST /ring` | ✓ + event topics | ✓ from Protect |
| Motion sensor | ✓ `POST /motion` | ✓ camera's motion events | ✓ smart detections |
| HomeKit Secure Video | ✓ | ✓ | ✓ |
| Snapshots | ✓ JPEG URL, or ffmpeg (optional) | ✓ ONVIF or JPEG URL | ✓ Protect API |

Any camera can be a doorbell (`doorbell = true`). Tested with a UniFi Protect Doorbell Lite and
an Intelbras (Dahua) ONVIF camera, iPhones and home hubs, in a 256 MB Debian LXC.

## Requirements

- A camera (or NVR) with an **H.264** RTSP stream (H.265 won't work: iPhones need H.264 here)
- A Linux host with **systemd** (x86_64, arm64 or 32-bit ARM: a VM, an LXC container, a
  Raspberry Pi) **on the same network (L2) as your home hubs**, since HomeKit finds accessories
  by mDNS. 256 MB of RAM is plenty for several cameras.
- A home hub (Apple TV or HomePod) for remote access, notifications and HomeKit Secure Video

## Install

The release binaries are static (no runtime dependencies) and checksummed. The install script
picks the right one for the machine, verifies it, and installs it:

```bash
curl -fsSL https://raw.githubusercontent.com/joaodforce/rust-homekit-cam/main/install.sh -o install.sh
# no curl (minimal Debian)? wget -O install.sh https://raw.githubusercontent.com/joaodforce/rust-homekit-cam/main/install.sh
less install.sh            # read it first: it's short
sudo sh install.sh         # or: sudo VERSION=v0.1.0 sh install.sh
```

<details>
<summary>Or install by hand</summary>

Pick your architecture (`x86_64`, `aarch64` or `armv7`) and a release from
[Releases](https://github.com/joaodforce/rust-homekit-cam/releases):

```bash
V=v0.1.0 A=x86_64
curl -fsSLO https://github.com/joaodforce/rust-homekit-cam/releases/download/$V/rust-homekit-cam-$A-linux.tar.gz
curl -fsSLO https://github.com/joaodforce/rust-homekit-cam/releases/download/$V/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
tar xzf rust-homekit-cam-$A-linux.tar.gz && cd rust-homekit-cam-$A-linux
sudo install -m 755 rust-homekit-cam /usr/local/bin/
sudo install -m 644 rust-homekit-cam@.service /etc/systemd/system/
sudo install -D -m 644 config.example.toml /usr/local/share/rust-homekit-cam/config.example.toml
sudo install -d -m 700 /etc/rust-homekit-cam
sudo systemctl daemon-reload
```

To build from source instead, see [docs/development.md](docs/development.md).
</details>

What ends up where:

| Path | What |
|---|---|
| `/usr/local/bin/rust-homekit-cam` | the binary |
| `/etc/systemd/system/rust-homekit-cam@.service` | the unit template: one instance per camera |
| `/usr/local/share/rust-homekit-cam/config.example.toml` | the example config |
| `/etc/rust-homekit-cam/<camera>.toml` | your configs (root only, `0600`: they hold passwords) |
| `/var/lib/rust-homekit-cam/<camera>/` | each camera's pairing keys and HKSV settings |

Each camera runs as its own sandboxed service: a throwaway system user (`DynamicUser`), a
read-only system, no access to `/home`. The config reaches it as a systemd credential, so it
stays readable by root only.

## Set up a camera

**1. Write its config.** One file per camera, named after it:

```bash
sudo cp /usr/local/share/rust-homekit-cam/config.example.toml /etc/rust-homekit-cam/frontdoor.toml
sudo chmod 600 /etc/rust-homekit-cam/frontdoor.toml
sudo nano /etc/rust-homekit-cam/frontdoor.toml
```

Fill in the name and how to reach the camera:
- **ONVIF cameras**: just the camera's address and login in `[onvif]`; `rust-homekit-cam
  onvif-discover` finds them ([docs/onvif.md](docs/onvif.md)).
- **UniFi Protect**: `rust-homekit-cam protect-cameras` prints a ready-made block for every
  camera ([docs/unifi-protect.md](docs/unifi-protect.md)).
- **Anything else**: the RTSP URL ([docs/generic-cameras.md](docs/generic-cameras.md)).

Every option is in [docs/configuration.md](docs/configuration.md).

**2. Check it** before going live: the streams, then a snapshot.

```bash
sudo rust-homekit-cam probe /etc/rust-homekit-cam/frontdoor.toml
sudo rust-homekit-cam snapshot /etc/rust-homekit-cam/frontdoor.toml /tmp/test.jpg
```

**3. Start it** (also at every boot) and get the setup code:

```bash
sudo systemctl enable --now rust-homekit-cam@frontdoor
sudo rust-homekit-cam code /etc/rust-homekit-cam/frontdoor.toml
```

**4. Add it in the Home app.** Tap + → Add Accessory → More options… → your camera → enter
the setup code. For recordings, open the camera's settings → Recording Options → Stream &
Allow Recording.

**More cameras:** another config with its own `hap_port` (and its own `ring_listen` port if you
use one), then `systemctl enable --now rust-homekit-cam@<name>`. Each camera is its own process
and its own accessory, so they restart and fail independently.

## Running it

| | |
|---|---|
| Status | `systemctl status 'rust-homekit-cam@*'` |
| Logs | `journalctl -u rust-homekit-cam@frontdoor -f` |
| Config change | `systemctl restart rust-homekit-cam@frontdoor` |
| Upgrade | run `install.sh` again: it replaces the binary and restarts running cameras; configs and pairings stay |
| Re-pair from scratch | `systemctl stop …`, `rust-homekit-cam reset <config>`, start, add it again |

**Back up** `/etc/rust-homekit-cam/` and `/var/lib/private/rust-homekit-cam/` (the pairing keys;
`/var/lib/rust-homekit-cam` links there). Without the keys, a reinstalled camera has to be
removed and added to Home again.

**Firewall:** HomeKit needs mDNS (UDP 5353) and each camera's `hap_port` (TCP). Live video,
two-way audio and HKSV use ports picked at each session, so a host firewall must allow
traffic from your Apple devices and hubs in general, not single ports. The service has to
reach the cameras' RTSP (and ONVIF/HTTP) ports.

**Uninstall:**

```bash
sudo systemctl disable --now 'rust-homekit-cam@*'
sudo rm /usr/local/bin/rust-homekit-cam /etc/systemd/system/rust-homekit-cam@.service
sudo rm -r /usr/local/share/rust-homekit-cam
sudo rm -r /etc/rust-homekit-cam /var/lib/private/rust-homekit-cam   # configs and pairings
```

and remove the cameras in the Home app.

## Docs

- [Configuration reference](docs/configuration.md)
- [ONVIF cameras](docs/onvif.md): discovery, streams, snapshots and motion from ONVIF
- [UniFi Protect](docs/unifi-protect.md): API key, camera setup helper, two-way audio, events
- [Other cameras](docs/generic-cameras.md): RTSP and snapshot URLs, rings and motion from Home Assistant
- [Troubleshooting](docs/troubleshooting.md)
- [How it works](docs/how-it-works.md): HAP, streaming without transcoding, HKSV
- [Development](docs/development.md): building from source, deploying from a Mac, CI and releases, layout

## License

GPL-3.0-only.
