# UniFi Protect

Protect cameras work without any Protect settings beyond RTSP. The optional `[protect]` backend
adds rings and motion from Protect's own detections, snapshots and two-way audio.

## 1. An API key

In the console's web UI (the local one, `https://<console>`): **Settings → Control Plane →
Integrations → Create API Key**. On some versions it is in the **Network** app's settings
(Control Plane → Integrations). The key covers Protect's integration API; it is not a
cloud (unifi.ui.com) key.

## 2. Camera blocks, generated

Write a minimal file with just the key and the console's address:

```toml
name = "setup"
[protect]
token = "<API key>"
host = "192.168.1.1"
```

and run:

```bash
rust-homekit-cam protect-cameras setup.toml
```

It lists every camera with a block to paste into `/etc/rust-homekit-cam/<name>.toml`: the RTSP
URLs (plain RTSP on port 7447) and `camera_id`. The URLs contain the stream's alias, which works
like a password: keep them out of git and chats. A camera whose RTSP is off says so; turn it on
in Protect (camera → Settings → Advanced → RTSP, all three qualities).

The first request logs the console's certificate fingerprint; put it in `cert_sha256` to pin it.

## Video codec

Protect can stream H.264 or H.265 per camera. This service needs **H.264**: turn off
**Enhanced Encoding** (H.265) in the camera's settings in Protect; newer models (G5 Turret
Ultra, G6…) often ship with it on. The change only takes effect after the camera restarts.
`rust-homekit-cam probe <config>` names the codecs each stream carries.

If `protect-cameras` shows "couldn't read the RTSP streams: … HTTP 429", the console is
rate-limiting; requests are retried, so running it again usually works.

## What the backend does

- **Rings and motion**: one WebSocket to the integration API's event stream. A ring (`ring`
  event) rings the doorbell. Smart detections (`smartDetectZone` / `smartDetectLine`: person,
  vehicle, animal, package) turn the Motion Sensor on until they end. With `motion = "any"`,
  plain motion events count too. Some cameras (the Doorbell Lite) send only smart detections.
  Talking (`smartAudioDetect`) is ignored.
- **Snapshots**: the integration API's snapshot. Its size depends on the camera (a Doorbell Lite
  gives 336×448).
- **Two-way audio**: each live view opens a talk-back session; Protect answers with the camera's
  own address (`rtp://<camera>:7004`, Opus) and the iPhone's microphone is forwarded there
  unchanged. **The service's host must reach the camera on UDP 7004.** If cameras sit on their
  own VLAN with inter-VLAN traffic blocked, allow exactly that (service host → camera, UDP 7004).
  Protect does not relay the audio.
- **Camera audio**: Protect's RTSP carries an Opus track next to AAC: Opus goes to live view,
  AAC into HomeKit Secure Video recordings, both untouched.

## Channels

Protect's three RTSP qualities map to `high`, `medium`, `low`. For a Doorbell Lite:

| Channel | Size | Bitrate | Used for |
|---|---|---|---|
| Low | 336×448 | 225 kbps | live view on cellular, Apple Watch, previews |
| Medium | 672×896 | 2 Mbps | live view on Wi-Fi, recordings |
| High | 1920×2560 | 10 Mbps | only with `allow_high = true` |
