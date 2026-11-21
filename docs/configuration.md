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
| `motion_hold` | `10` | Seconds the sensor stays on after the last motion |
| `hksv` | `true` | HomeKit Secure Video services; `false` hides them. The Motion Sensor (its trigger) is always present, fed by `[protect]`, `[onvif]` or `POST /motion` |
| `talkback` | `true` | `false` turns two-way audio off (e.g. when the camera's speaker misbehaves) |
| `ring_listen` | none | `"0.0.0.0:8780"`: listen for the event endpoints below; a different port per camera |
| `ring_token` | required with `ring_listen` | Bearer token for those requests |
| `motion_webhook` | unset | An `http://…` URL, or a list of them, that gets each Motion Sensor change and ring as a JSON POST: `{"camera": "Garagem", "event": "motion", "state": "start"}` (`"end"` when it clears; rings: `"event": "ring", "state": "pressed"`), plus `"source"` (`protect`, `onvif`, `http`, …). For Home Assistant: a webhook trigger, so HA gets every camera's motion without talking to the cameras |
| `motion_webhook_snapshot` | unset | `{ width = 320, quality = 60 }`: each motion start also carries a small JPEG (base64, `"snapshot"`) from the camera's snapshot source, e.g. for a receiver that shows it as the event's thumbnail (such as a UniFi Protect event bridge). Fetched once per event, in the background |
| `snapshot_url` | from `[protect]` or `[onvif]` | `http://[user:pass@]host/path` returning a JPEG (Basic or Digest); overrides the backends |
| `snapshot_ffmpeg` | unset | Grab snapshots from the RTSP stream with ffmpeg (which you install): `"ffmpeg"` from PATH, or its full path. Used when `snapshot_url` isn't set; overrides the backends. See [generic-cameras.md](generic-cameras.md#snapshots-from-the-stream-ffmpeg) |
| `snapshot_resize` | `false` | Shrink snapshots when the Home app asks for a much smaller image (the Apple Watch asks for ~320x240, through a home hub): decoded at 1/2–1/8 scale and re-encoded, a few ms each, a few KB instead of a few hundred. Only when the request is at most half the source's size; otherwise the original is sent. Works with every snapshot source |
| `transcode_audio` | `false` | Convert camera audio to what HomeKit takes (video is never touched): G.711/AAC → Opus for live view, G.711/Opus → AAC for recordings. Needed for sound from most non-Protect cameras. See [generic-cameras.md](generic-cameras.md#no-audio-in-live-view-or-recordings) |

`state_dir` defaults to systemd's `$STATE_DIRECTORY` (`/var/lib/rust-homekit-cam/<name>`) when
run by the unit. Run from a shell (`code`, `reset`), it defaults to
`/var/lib/rust-homekit-cam/<config file name>`, so both find the same place. Keep this
directory: losing it means removing and re-adding the camera in the Home app.

## `[rtsp]`: the video

| Key | Meaning |
|---|---|
| `high`, `medium`, `low` | RTSP URLs of the camera's streams. One is enough. With several, live view picks the one that fits the bitrate the iPhone asks for (≤ 150 kbps, a remote viewer through a home hub: `low`; otherwise `medium`), and HomeKit Secure Video records `medium` (or the closest). Two-stream cameras: main as `medium`, sub (~128 kbps) as `low` |
| `allow_high` | `true`: use `high` when the iPhone asks for ≥ 4 Mbps (by default `high` is only a fallback) |

Credentials go in the URL: `rtsp://user:password@host:554/path` (Basic or Digest). Encode
special characters in the password (`@` → `%40`).

Requirements: H.264 video. For camera audio in live view the stream needs an **Opus** track, or a
G.711/AAC track with `transcode_audio = true`; for audio in recordings, an **AAC** track
(`mpeg4-generic`), or a G.711/Opus track with `transcode_audio = true`. Otherwise you get video only.

## `[onvif]`: ONVIF backend (optional)

`url`, `user`, `password`, and optionally `motion_topics`, `ring_topics`, `snapshot`. Supplies
the streams (when `[rtsp]` is empty), the snapshot URL and motion/ring events. See
[onvif.md](onvif.md).

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
rust-homekit-cam snapshot <config> <out.jpg>      save the picture the Home app would get
rust-homekit-cam protect-cameras <config>  list Protect cameras with config blocks to paste
rust-homekit-cam onvif-info <config>       an ONVIF camera's profiles, URIs and 30 s of events
rust-homekit-cam onvif-discover            ONVIF cameras on the LAN
```
