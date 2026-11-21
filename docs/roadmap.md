# Roadmap

## Next major version: a web GUI for configuring cameras

Today a camera is a TOML file plus `probe` / `snapshot` / `code` on the command line. The next
major version should add a small web UI (served by the binary itself, off by default, LAN-only,
password-protected) to:

- add a camera: ONVIF discovery or Protect's camera list, paste an RTSP URL, pick the streams;
- show live checks per stream (what `probe` does) and a snapshot;
- show the setup code / QR and pairing state; reset pairing;
- edit settings with explanations, and restart the camera's service;
- show the **smart warnings** below, each with what to change and where (ideally with a
  "fix it" button when we can do it ourselves, e.g. turning on `transcode_audio`).

### Smart warnings to surface

Things that work but disappoint, learned the hard way. `probe` already prints the ones marked ✓.

**Streams and bitrate**
- ✓ `low` well above ~150 kbps: remote viewers through a home hub ask for ≤ 132 kbps and lose
  most packets of a bigger stream (video freezes every few seconds, audio survives).
- Only one stream configured: every viewer, remote ones too, gets it (covered by the above when
  it's big).
- Main stream configured as `high` with nothing as `medium`: LAN live view falls back to `low`
  (looks blurry). Suggest moving it to `medium`.
- `medium` far above what iPhones ask on the LAN (~300 kbps–2 Mbps): works, but costs the hub
  and Wi-Fi; suggest a lower bitrate or the third stream.
- Keyframe interval long (> 4 s): live view takes that long to start; suggest 1–2 s.
- H.265 / "smart codec" / Enhanced Encoding on: no H.264 track, nothing plays (probe errors).
- Camera's stream orientation vs what we detected (portrait doorbells): offered sizes follow
  the SPS; if the SPS isn't readable at start-up both orientations are offered.

**Video encoding**
- ✓ Pictures cut into many slices (seen: 6 fails, 2 plays): HomeKit doesn't decode them; suggest
  one slice per frame in the camera's encoder settings. Open question: where exactly the limit
  is, and whether we can merge slices ourselves.

**Audio**
- ✓ No Opus and `transcode_audio` off: live view has no sound (G.711/AAC cameras).
- ✓ Recording stream without AAC and `transcode_audio` off: recordings are silent.
- ✓ AAC on another stream but not the one recordings use: set AAC there and recordings copy it
  instead of encoding (saves CPU).
- G.722 or other codecs we can't convert: say so plainly.
- AAC encoding for recordings costs ~2% of an M-series core, ~10% of a Pi 4 core, per camera,
  while recording is enabled: show the cost, prefer AAC from the camera.

**Events and recording**
- No motion source (`[protect]`, `[onvif]` or `ring_listen`): the Motion Sensor never fires,
  so HKSV never records (logged at start-up).
- No snapshot source: blank tiles in the Home app, and the camera may show "unavailable" more
  often; suggest `snapshot_url` or `snapshot_ffmpeg`.
- ONVIF camera without GetSnapshotUri (e.g. Intelbras): suggest the brand's snapshot URL.

**Network and host**
- HAP port clash between cameras on one host; mDNS responder status (who leads).
- Firewall: live view, talk-back and HKSV use ports picked per session; Protect talk-back needs
  UDP 7004 to the camera.
- Protect API rate limiting (HTTP 429) and RTSP disabled per camera in Protect.

## Other ideas

- Switch streams mid-view when HomeKit reconfigures to a lower bitrate (now logged and ignored).
- Encode recording audio only while recording (keep raw PCM in the prebuffer): zero idle cost.
- ONVIF two-way audio (back-channel) for non-Protect cameras.
- Upstream the oxideav-aac speed patches (fork: joaodforce/oxideav-aac, branch
  `rust-homekit-cam-speed`) and drop the `[patch]`.
