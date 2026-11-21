# Other cameras

Any camera (or NVR) with an **H.264** RTSP stream works. If it speaks ONVIF (most do), see
[onvif.md](onvif.md) first: it finds the streams and motion events for you. What you get depends on the stream:

| The camera has | You get |
|---|---|
| H.264 video | live view, HomeKit Secure Video (with a motion source) |
| an Opus audio track | camera audio in live view |
| a G.711 (PCMU/PCMA) or AAC audio track | camera audio in live view with `transcode_audio = true` (see [below](#no-audio-in-live-view-or-recordings)) |
| an AAC audio track | audio in recordings (G.711 or Opus: with `transcode_audio = true`) |
| a JPEG snapshot URL, or ffmpeg installed | pictures in tiles and notifications |

Two-way audio needs a vendor backend; only UniFi Protect has one so far (ONVIF back-channels
usually need audio conversion, which isn't done).

## RTSP and snapshot URLs

Typical paths; check your camera's manual, and set the stream codec to H.264 in its settings.
Use the main stream as `medium` and the sub stream as `low`, and set the sub stream to ~128 kbps
in the camera: remote viewers (through a home hub) ask for ≤ 132 kbps, and a bigger stream
freezes for them. Prefer one slice per frame and, for recordings, AAC audio on the main stream.
`probe` warns about each of these.

| Brand | RTSP (main / sub) | Snapshot |
|---|---|---|
| Dahua, Intelbras, Amcrest | `rtsp://user:pass@ip:554/cam/realmonitor?channel=1&subtype=0` / `subtype=1` | `http://user:pass@ip/cgi-bin/snapshot.cgi` |
| Hikvision | `rtsp://user:pass@ip:554/Streaming/Channels/101` / `102` | `http://user:pass@ip/ISAPI/Streaming/channels/101/picture` |
| Reolink | `rtsp://user:pass@ip:554/h264Preview_01_main` / `_sub` | see the camera's API docs |

`rust-homekit-cam probe <config>` connects to each URL and reports the tracks and frames it
receives, which is the quickest way to check a URL, then warns about settings HomeKit handles
badly:

```
warning: remote viewers (via a home hub, which asks for ≤132 kbps) get the low stream at 512 kbps: expect freezes; set a sub stream to ~128 kbps as `low`
warning: low: pictures come in up to 6 slices; HomeKit may not decode them (one slice per frame is safest: check the camera's encoder settings)
warning: recordings use the medium stream: its audio ["pcma"] isn't AAC, so they'll be silent: set transcode_audio = true, or AAC in the camera
```

## Snapshots from the stream (ffmpeg)

If the camera has no snapshot URL, rust-homekit-cam can grab one frame from the RTSP stream with
**ffmpeg**. ffmpeg isn't bundled or required: install it only if you need this.

```bash
apt install ffmpeg          # Debian/Ubuntu (brew install ffmpeg on a Mac)
```

```toml
snapshot_ffmpeg = "ffmpeg"  # from PATH, or the full path, e.g. "/usr/bin/ffmpeg"
```

Prefer not to install it? Any executable that takes ffmpeg's arguments works, such as a wrapper
around a container (needs Docker; adds about half a second):

```sh
#!/bin/sh
exec docker run --rm -i lscr.io/linuxserver/ffmpeg:latest "$@"
```

Each snapshot runs `ffmpeg` once on the sub stream (`low`, else `medium`/`high`), which takes
about a second or two depending on the camera's keyframe interval. Pictures are cached for 5 s,
and a ring or motion fetches a new one. Check it with `rust-homekit-cam snapshot <config>
test.jpg`. A working `snapshot_url` is still faster and lighter.

## No audio in live view or recordings

HomeKit's live view plays **Opus** (or AAC-ELD) and its recordings take **AAC**, while most
cameras send **G.711** (PCMU/PCMA) or **AAC-LC**. By default audio is passed through untouched,
so the log says when a camera's audio can't be used:

```
stream: camera audio is pcma, HomeKit plays Opus: set transcode_audio = true for sound; video only
recording: camera audio is pcma, recordings take AAC: set transcode_audio = true for sound
```

Either switch the camera's audio codec to Opus (few offer it), or let rust-homekit-cam convert it:

```toml
transcode_audio = true     # top level of the camera's config
```

The audio (only the audio) is then decoded and re-encoded inside the binary, no ffmpeg:

| Camera audio | Live view | Recordings |
|---|---|---|
| Opus | passed through | → AAC-LC 16 kHz |
| G.711 (PCMU/PCMA) | → Opus | → AAC-LC 16 kHz |
| AAC-LC | → Opus | copied as is |

Live view costs well under 1% of a CPU core per viewer. For recordings the AAC encoding runs
while recording is enabled (the few seconds before a motion are kept ready): about 2% of a core
on an Apple M-series, roughly 10% on a Raspberry Pi 4. A camera that sends AAC needs none of it
for recordings. Check with `journalctl -u rust-homekit-cam@NAME | grep -E "stream:|recording:"`:

```
recording: encoding audio, PCMA 8000 Hz → AAC-LC 16000 Hz
stream: transcoding audio, PCMA 8000 Hz → Opus 16000 Hz, 20 ms
stream: audio flowing, Opus config 9 stereo 0 (52 bytes)
```

To see which audio codec a camera sends, `sudo rust-homekit-cam probe /etc/rust-homekit-cam/NAME.toml`
lists the tracks (`pcmu`, `pcma`, `mpeg4-generic` = AAC, `opus`). If the camera can send AAC,
that's the cheapest choice: recordings copy it and live view converts it.

## Rings and motion from Home Assistant

Without a vendor backend, events come in over HTTP. Set `ring_listen` and `ring_token` (they feed
the doorbell and the Motion Sensor, which HomeKit Secure Video records on), then call the endpoints from anything. In Home
Assistant (`configuration.yaml`):

```yaml
rest_command:
  frontdoor_ring:
    url: http://<service-host>:8780/ring
    method: POST
    headers:
      Authorization: !secret frontdoor_ring_auth   # "Bearer <ring_token>" in secrets.yaml
  frontdoor_motion:
    url: http://<service-host>:8780/motion
    method: POST
    headers:
      Authorization: !secret frontdoor_ring_auth
```

and call `rest_command.frontdoor_ring` / `rest_command.frontdoor_motion` from automations
(e.g. when the camera's motion `binary_sensor` turns on). Each `POST /motion` keeps the sensor on
for `motion_hold` seconds. If your source knows when motion ends, use `/motion/start` and
`/motion/end` instead (e.g. on the `binary_sensor` turning on and off). Rings within 3 s count
once. Any camera can be a doorbell (`doorbell = true`): a button, an intercom or a Zigbee
switch can ring it through `/ring`.

Test from a shell:

```bash
curl -X POST -H "Authorization: Bearer <ring_token>" http://<service-host>:8780/ring
```

## Motion into Home Assistant (without HA touching the cameras)

`motion_webhook` sends each camera's Motion Sensor changes (and rings) on as JSON, whatever the
source (Protect, ONVIF, HTTP), so Home Assistant needs no camera integration, credentials or
firewall access to the cameras. One webhook can serve every camera:

```toml
motion_webhook = "http://homeassistant.local:8123/api/webhook/camera-events"
```

```yaml
# configuration.yaml: one binary sensor per camera, driven by the webhook
template:
  - trigger:
      - platform: webhook
        webhook_id: camera-events
        local_only: true
    binary_sensor:
      - name: "Garagem motion"
        device_class: motion
        state: >-
          {% if trigger.json.camera == 'Garagem' and trigger.json.event == 'motion' %}{{ trigger.json.state == 'start' }}
          {% else %}{{ this.state == 'on' }}{% endif %}
```

The body is `{"camera": "Garagem", "event": "motion", "state": "start" | "end", "source": "onvif"}`,
or `"event": "ring", "state": "pressed"` for a doorbell press.
