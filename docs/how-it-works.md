# How it works

```
iPhone / home hub ──HAP (HTTP, ChaCha20-Poly1305)──► rust-homekit-cam ◄── events: Protect WebSocket, POST /ring, /motion
     ▲   │   ▲                                         │   ▲
     │   │   └── HomeKit Data Stream (recordings) ◄────┤   └── RTSP/TCP from the camera / NVR
     │   └──── SRTP: microphone (Opus) ─────────────►  │       (up to 3 streams, picked by bitrate)
     └──────── SRTP: H.264 + Opus ◄────────────────────┘──► talk-back (Protect: camera UDP 7004)
```

One process per camera, plain threads, no async runtime. Nothing is decoded or re-encoded:
video and audio are only re-packaged.

## HomeKit Accessory Protocol

Own implementation (hap-rs is unmaintained and has no camera support): SRP pair-setup,
pair-verify (X25519 + Ed25519), the encrypted session (ChaCha20-Poly1305 frames), `/accessories`,
`/characteristics` with write-responses and events, `/pairings`, `/resource` (snapshots). The
mDNS advert uses an IPv4-only host name of its own: an IPv6 address in the advert can hang
"Adding to Home…". Re-advertising (after pairing) updates the record in place; restarting the
responder sends goodbye packets that can make a controller lose the accessory right after
pair-setup.

One mDNS responder per host: with several cameras (one process each) on one machine, separate
responders would all bind UDP 5353, and Linux delivers a unicast query to just one of them,
which can't answer for the others. So the instances elect a leader through the abstract Unix
socket `@rust-homekit-cam-mdns` (no files, works under the systemd sandbox): the first to bind
it runs the only responder; the others connect and send their advert as JSON lines. A follower
that stops is withdrawn when its connection closes; if the leader stops, the followers elect a
new one within a second. The log says which role each instance has (`mdns: …`). Off Linux each
process is its own responder.

A doorbell is category 18 (Video Doorbell), a camera 17. Services: Accessory Information,
Camera RTP Stream Management, Microphone, Speaker, Doorbell (doorbells), Motion Sensor, and for
HomeKit Secure Video Camera Operating Mode, Camera Event Recording Management and Data Stream
Transport Management.

## Live view

The controller writes `SetupEndpoints` (its address, ports and SRTP keys); we answer with our
own ports, keys and SSRCs. `SelectedRTPStreamConfiguration` then starts the stream with the size,
bitrate, payload types and MTU it wants. We pick the RTSP stream whose bitrate fits, pull its RTP
over TCP, reassemble the H.264 NAL units, cut them again into packets that fit the MTU (FU-A,
1378 bytes on IPv4), put SPS/PPS in front of each IDR, and send them with SRTP
(AES_CM_128_HMAC_SHA1_80), plus RTCP sender reports. The advertised resolutions are only what
the iPhone may ask for; it decodes the real size from the SPS.

## Audio

HomeKit live view takes Opus or AAC-ELD. Only Opus is offered (with AAC-ELD listed, iPhones pick
it, and it would need encoding); a camera's Opus track is passed through with a new RTP header.
iPhones on the LAN ask for 20 ms packets, home hubs relaying over the internet for 60 ms: then
three 20 ms Opus frames go into one packet (code 3, RFC 6716), still the same audio data.

With `transcode_audio = true` a G.711 (PCMU/PCMA) or AAC-LC track is converted instead: decoded
(G.711 by table, AAC with Symphonia), downmixed to mono, linearly resampled to the controller's
rate and encoded with libopus (VoIP mode, 24 kbps) in frames of the requested packet time. The
RTP timestamps count samples at the requested rate. Off by default.

Timestamps: Opus over RTP normally runs on a 48 kHz clock (RFC 7587), but HomeKit times it by
the sample rate it requested (RFC 3550 style): at 16 kHz a 20 ms packet is 320 ticks, not 960.
Passing 48 kHz timestamps through makes every packet look three times as long, and LAN audio
stutters. They are rescaled to the requested rate, keeping the camera's own timing. (go2rtc and
Scrypted do the same.)

The jitter in the iPhone's RTCP reports (logged as `rtcp audio/video`) stays at a few hundred
ms even when playback is smooth; loss, NACKs and keyframe requests are the useful numbers.

Two-way audio: the iPhone's microphone arrives as SRTP (Opus) on our audio port; we decrypt it
and hand the RTP packets to the talk-back backend. Protect's integration API opens a talk-back
session that points at the camera itself (Opus over RTP, UDP 7004).

## HomeKit Secure Video

The home hub decides when to record, based on the Motion Sensor (and the doorbell). While
recording is enabled one RTSP connection keeps a prebuffer: the last ~4 s from a keyframe, as
H.264 access units and AAC frames in memory. On a trigger the hub:

1. writes `SetupDataStreamTransport` with its salt; we answer with a TCP port and our salt.
   Keys: HKDF-SHA512 over the HAP session secret with both salts
   (`HDS-Read-Encryption-Key` / `HDS-Write-Encryption-Key`);
2. connects to the HomeKit Data Stream: ChaCha20-Poly1305 frames carrying messages in Apple's
   binary "opack" encoding; `control/hello`, then `dataSend/open` for an
   `ipcamera.recording`;
3. receives `dataSend/data` events: the fMP4 init segment (H.264 `avc1` + AAC `mp4a`), then
   fragments (`moof` + `mdat`) cut at each keyframe, prebuffer first, then live;
4. closes the stream when it is done.

The camera's H.264 and AAC go into the MP4 boxes as they come; the size in the init segment is
read from the SPS.

## Events

Rings and motion have pluggable sources that end in the same two calls: `POST /ring` and
`POST /motion` (bearer token), and Protect's integration event stream (a WebSocket, pinged every
30 s, reconnecting with backoff). Rings from several sources within 3 s count once; motion stays
on while a detection lasts and `motion_hold` seconds after.
