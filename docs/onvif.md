# ONVIF cameras

Most IP cameras speak ONVIF. With an `[onvif]` section the camera's own ONVIF service supplies
the RTSP streams, the snapshot and motion events, so the config can be just a login:

```toml
name = "Garden"
doorbell = false
hap_port = 51831

[onvif]
url = "http://192.168.1.50"     # the camera (add :port if ONVIF isn't on 80, e.g. :8899)
user = "admin"
password = "…"
```

Tested with an Intelbras VIP-1130-B (Dahua-based): live view, snapshots, motion and HomeKit
Secure Video.

## Finding cameras and checking what they offer

```bash
rust-homekit-cam onvif-discover                  # ONVIF cameras on the LAN (WS-Discovery)
rust-homekit-cam onvif-info /etc/rust-homekit-cam/garden.toml
```

`onvif-info` logs in and prints the device, its profiles (codec, size, bitrate) with stream and
snapshot URIs, then 30 s of live events: walk in front of the camera to see which event topics
it sends.

## What comes from ONVIF

| | From ONVIF | Override |
|---|---|---|
| Streams | the H.264 profiles: the largest becomes `high` (and `medium`), the smallest `low`; the login is added to the URLs | set `[rtsp]` yourself |
| Snapshots | the profile's snapshot URI | `snapshot_url` (top level), or `snapshot = false` |
| Motion | events whose topic contains one of `motion_topics` (default `["Motion"]`): `VideoSource/MotionAlarm`, `RuleEngine/CellMotionDetector/Motion`… true turns the Motion Sensor on, false off | `motion_topics = [...]`, `motion = false` (top level) |
| Ring | events whose topic contains one of `ring_topics` with a true value (none by default; e.g. a doorbell's `Device/Trigger/DigitalInput`) | `ring_topics = [...]` |

Switch the camera's streams to **H.264** in its web UI; H.265 profiles are skipped.

## `[onvif]` keys

| Key | Default | Meaning |
|---|---|---|
| `url` | required | `http://<camera>[:port]`, or the full device service URL |
| `user`, `password` | required | An ONVIF user (some cameras keep ONVIF users separate from web users) |
| `motion_topics` | `["Motion"]` | Topic substrings that count as motion |
| `ring_topics` | `[]` | Topic substrings that ring the doorbell |
| `snapshot` | `true` | Use the ONVIF snapshot URI |

## Brand notes

- **Intelbras / Dahua**: ONVIF on port 80. Their ONVIF snapshot URI is missing or broken; use
  `snapshot_url = "http://user:pass@<camera>/cgi-bin/snapshot.cgi"` (Digest, handled). Motion
  arrives as `VideoSource/MotionAlarm`.
- The camera's clock may be off: the login uses the camera's own time, so that's fine.

## Two-way audio

Not yet for ONVIF cameras. The ONVIF back-channel (Profile T) usually wants G.711 or AAC, and the
iPhone talks Opus; converting needs audio decoding, which this project doesn't do (yet).
`talkback = false` is the default outcome for ONVIF cameras.
