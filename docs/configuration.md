# Configuration

One TOML file per camera, `/etc/rust-homekit-cam/<name>.toml`, mode 0600 (it holds secrets),
run by `rust-homekit-cam@<name>.service`. [`config.example.toml`](../config.example.toml) is a
starting point.

## Top level

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Name in the Home app |
| `doorbell` | `true` | `true`: a Video Doorbell (ring notification, HomePod chime); works for any camera, rings come from `POST /ring` or a backend; `false`: a plain camera |
| `manufacturer`, `model` | `rust-homekit-cam`, `Video Doorbell` / `Camera` | Shown in the accessory details |
| `hap_port` | `51830` | TCP port HomeKit talks to; **different for each camera** on one host |
| `state_dir` | see below | Pairing identity, setup code, recording settings |
| `motion` | on with `[protect]` | Adds the Motion Sensor (the trigger for HomeKit Secure Video); `true` to feed it with `POST /motion` |
| `motion_hold` | `10` | Seconds the sensor stays on after the last motion |
| `hksv` | on with the Motion Sensor | HomeKit Secure Video services; `false` hides them |
| `talkback` | `true` | `false` turns two-way audio off (e.g. when the camera's speaker misbehaves) |
| `ring_listen` | none | `"0.0.0.0:8780"`: listen for the event endpoints below; a different port per camera |
| `ring_token` | required with `ring_listen` | Bearer token for those requests |
| `snapshot_url` | Protect's snapshot API with `[protect]` | `http://[user:pass@]host/path` returning a JPEG (Basic or Digest) |

`state_dir` defaults to systemd's `$STATE_DIRECTORY` (`/var/lib/rust-homekit-cam/<name>`) when
run by the unit. Run from a shell (`code`, `reset`), it defaults to
`/var/lib/rust-homekit-cam/<config file name>`, so both find the same place. Keep this
directory: losing it means removing and re-adding the camera in the Home app.

## `[rtsp]`: the video

| Key | Meaning |
|---|---|
| `high`, `medium`, `low` | RTSP URLs of the camera's streams. One is enough. With several, live view picks the one that fits the bitrate the iPhone asks for (≤ 500 kbps: `low`; otherwise `medium`), and HomeKit Secure Video records `medium` (or the closest) |
| `allow_high` | `true`: use `high` when the iPhone asks for ≥ 4 Mbps (by default `high` is only a fallback) |

Credentials go in the URL: `rtsp://user:password@host:554/path` (Basic or Digest). Encode
special characters in the password (`@` → `%40`).

Requirements: H.264 video. For camera audio in live view the stream needs an **Opus** track; for
audio in recordings, an **AAC** track (`mpeg4-generic`). Without them you get video only.

## `[protect]`: UniFi Protect backend (optional)

| Key | Meaning |
|---|---|
| `token` | Integration API key |
| `camera_id` | The camera's id (`rust-homekit-cam protect-cameras` lists them) |
| `host` | The console's address; default: the host of the RTSP URLs |
| `cert_sha256` | The console's TLS certificate fingerprint (`AB:CD:…`), pinned. Logged on first use if unset |
| `motion` | `"smart"` (default): person, vehicle, animal and package detections; `"any"`: every motion event |

With `[protect]` the camera gets: rings and motion from Protect's events, snapshots from the
Protect API, and two-way audio (talk-back). See [unifi-protect.md](unifi-protect.md).

## Event endpoints (`ring_listen`)

All `POST`, with `Authorization: Bearer <ring_token>` (or `?token=<ring_token>`):

| Path | Effect |
|---|---|
| `/ring` | Rings the doorbell (repeats within 3 s count once) |
| `/motion` | Motion pulse: the sensor is on for `motion_hold` seconds (again from now on each call) |
| `/motion/start` | Motion on until `/motion/end` (at most 10 minutes, in case the end is lost) |
| `/motion/end` | The sensor goes off `motion_hold` seconds from now |

## Commands

```
rust-homekit-cam serve <config>            run (the systemd unit does this)
rust-homekit-cam code <config>             the setup code, and whether it is paired
rust-homekit-cam reset <config>            forget all controllers (then restart the unit)
rust-homekit-cam probe <config>            pull 3 s from each RTSP URL and report what arrives
rust-homekit-cam record-test <config> <out.mp4>   ~12 s of what HomeKit Secure Video gets
rust-homekit-cam protect-cameras <config>  list Protect cameras with config blocks to paste
```
