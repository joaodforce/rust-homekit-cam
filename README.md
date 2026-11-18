# rust-homekit-cam

A small Rust service that puts a UniFi Protect **doorbell** (and later, regular cameras) in Apple
Home as a native HomeKit accessory: ring notifications, live video and audio, two-way audio —
**without video transcoding**. One static binary, no Node, no ffmpeg for the video path.

Status: early. Pairing, the doorbell ring, live video and camera audio work (tested with a
Doorbell Lite and an iPhone). Two-way audio is next.

## What it exposes

One HAP accessory per camera; a doorbell is category 18 (Video Doorbell), a camera 17.

| Service | Purpose |
|---|---|
| Accessory Information | name, model, serial, firmware |
| Doorbell (0x121), doorbells only | `ProgrammableSwitchEvent` = 0 on ring → notification + chime on HomePods |
| Camera RTP Stream Management (0x110) | live view: `SupportedVideo/Audio/RTPConfiguration`, `SetupEndpoints`, `SelectedRTPStreamConfiguration`, `StreamingStatus`, `Active` |
| Microphone (0x112) | camera audio (mute) |
| Speaker (0x113) | two-way audio (mute, volume) |

Later: Motion Sensor (Protect motion / smart detections), snapshots, and **HomeKit Secure Video**
(Camera Operating Mode, Camera Event Recording Management, Data Stream: fMP4 fragments of the same
H.264, still no transcoding).

## Architecture

```
iPhone / Home hub ──HAP (HTTP, ChaCha20-Poly1305)──► rust-homekit-cam ◄── POST /ring (HA, webhook…)
        ▲   │                                              │   ▲
        │   └── SRTP talk-back audio ───────────────────►  │   └── RTSP/TCP: Protect channel
        └──────────── SRTP: H.264 + Opus ◄─────────────────┘        High / Medium / Low
                                                           └──► Protect talk-back (camera speaker)
```

- **HAP**: own code (hap-rs is unmaintained and has no camera support), from the author's crtd
  project: SRP pair-setup, pair-verify, encrypted sessions, `/accessories`, `/characteristics`
  (incl. write-responses), events, `/pairings`, `mdns-sd` advert with an IPv4-only host name (an
  IPv6 address in the advert can hang "Adding to Home…"). Plain std threads, no async runtime.
- **Stream handshake**: the controller writes `SetupEndpoints` (session id, its address and
  ports, SRTP keys); we answer with our address/ports, our SRTP keys and SSRCs. Then
  `SelectedRTPStreamConfiguration` starts / reconfigures / stops the stream with the size,
  max bitrate, payload type and MTU it wants.
- **Video, no transcoding**: Protect serves H.264 over RTSP. Per stream we pull the RTP H.264
  packets, rewrite PT/SSRC/seq/timestamp, re-fragment NALs to the controller's MTU (FU-A; 1378
  on IPv4), put SPS/PPS in front of each IDR, and send them SRTP-protected
  (AES_CM_128_HMAC_SHA1_80). RTCP sender reports keep the stream alive.
- **Channel choice**: we pick the Protect channel whose bitrate fits the request, e.g. for a
  Doorbell Lite:

  | Channel | Size | Bitrate | Used when the request is |
  |---|---|---|---|
  | Low | 336×448 | 225 kbps | ≤ ~300 kbps (cellular, Watch, notification preview) |
  | Medium | 672×896 | 2 Mbps | default (Wi-Fi live view) |
  | High | 1920×2560 | 10 Mbps | opt-in |

  The advertised sizes are only what the iPhone may ask for; it decodes the real size from the
  SPS (as Scrypted's "copy" mode does).
- **Audio**: Protect's RTSP carries an **Opus** track next to AAC, and HomeKit accepts Opus, so
  camera audio is passed through (only the RTP header is rewritten). Only Opus is offered:
  with AAC-ELD listed the iPhone picks AAC-ELD. iOS keeps the audio configuration it saw when
  the accessory was added, so after changing the offer the accessory has to be removed and
  added again.
- **Two-way audio**: the iPhone sends its microphone as SRTP (Opus) to our audio port; we forward
  it to the camera's speaker through a Protect talk-back session (Opus where the camera supports
  it, otherwise an audio-only transcode).
- **Ring**: pluggable sources that all end in `Ringer::ring()`. Today: `POST /ring` with a bearer
  token (from Home Assistant or a Protect alarm-manager webhook). Later possibly Protect's own
  event websocket.

## Run

```sh
cargo build --release
cp config.example.toml /etc/rust-homekit-cam.toml   # fill in; chmod 600
rust-homekit-cam serve /etc/rust-homekit-cam.toml
rust-homekit-cam code  /etc/rust-homekit-cam.toml   # setup code for the Home app
rust-homekit-cam reset /etc/rust-homekit-cam.toml   # forget all controllers
```

Home app → + → Add Accessory → More options… → the camera's name → setup code.
If adding hangs at "Adding to Home…", the iPhone may now hide that accessory id: stop the
service, move `state_dir` away (new id and code) and add it again.
Test a ring: `curl -X POST -H "Authorization: Bearer <token>" http://<host>:8780/ring`.

The config holds secrets (RTSP alias, ring token): never commit it. State (setup code, pairings,
Ed25519 key) lives in `state_dir`, mode 0600.

Deploy: one static binary plus a systemd unit, on a host in the same L2 network as the home hubs
(mDNS). From a Mac: `make deploy HOST=root@<host>` (cargo-zigbuild, x86_64 musl, ~2 MB), with
`/etc/rust-homekit-cam.toml` (0600, `state_dir` left at its default) on the host. Two-way
audio needs the host to reach the camera on UDP 7004: Protect's talk-back session points at
the camera, it does not relay.

## Milestones

1. Pairs in the Home app as a doorbell; `POST /ring` rings it.
2. Live view (video).
3. Camera audio (Opus passthrough).
4. Two-way audio (Protect talk-back).
5. Deploy as a service.
6. Optional: motion sensor, snapshots, more cameras, HomeKit Secure Video.

## License

GPL-3.0-only.
