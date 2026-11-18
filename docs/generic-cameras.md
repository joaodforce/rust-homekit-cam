# Other cameras

Any camera (or NVR) with an **H.264** RTSP stream works. If it speaks ONVIF (most do), see
[onvif.md](onvif.md) first: it finds the streams and motion events for you. What you get depends on the stream:

| The camera has | You get |
|---|---|
| H.264 video | live view, HomeKit Secure Video (with a motion source) |
| an Opus audio track | camera audio in live view |
| an AAC audio track | audio in recordings |
| a JPEG snapshot URL, or ffmpeg installed | pictures in tiles and notifications |

Two-way audio needs a vendor backend; only UniFi Protect has one so far (ONVIF back-channels
usually need audio conversion, which isn't done).

## RTSP and snapshot URLs

Typical paths; check your camera's manual, and set the stream codec to H.264 in its settings.
Use the main stream as `high` and the sub stream as `low`.

| Brand | RTSP (main / sub) | Snapshot |
|---|---|---|
| Dahua, Intelbras, Amcrest | `rtsp://user:pass@ip:554/cam/realmonitor?channel=1&subtype=0` / `subtype=1` | `http://user:pass@ip/cgi-bin/snapshot.cgi` |
| Hikvision | `rtsp://user:pass@ip:554/Streaming/Channels/101` / `102` | `http://user:pass@ip/ISAPI/Streaming/channels/101/picture` |
| Reolink | `rtsp://user:pass@ip:554/h264Preview_01_main` / `_sub` | see the camera's API docs |

`rust-homekit-cam probe <config>` connects to each URL and reports the tracks and frames it
receives, which is the quickest way to check a URL.

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

## Rings and motion from Home Assistant

Without a vendor backend, events come in over HTTP. Set `ring_listen` and `ring_token` (and
`motion = true` for the Motion Sensor), then call the endpoints from anything. In Home
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
