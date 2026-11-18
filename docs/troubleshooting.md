# Troubleshooting

Logs: `journalctl -u rust-homekit-cam@<name> -f` (from a Mac: `make logs HOST=root@<host> CAM=<name>`).

## Adding to Home

**The camera doesn't show up under "More options…".** The host must be on the same network
segment as the iPhone and the home hubs (mDNS doesn't cross VLANs without a reflector). Check the
log for `advertised (… sf=1)`; `sf=1` means "not paired yet".

**Adding hangs at "Adding to Home…".** Afterwards the iPhone may hide that accessory for good.
Give it a fresh identity: stop the unit, move its state directory away
(`/var/lib/private/rust-homekit-cam/<name>`), start it again and add it with the new code
(`rust-homekit-cam code …`).

**Start over with the same identity**: remove it in the Home app (that unpairs it here too), or
`rust-homekit-cam reset <config>` and restart the unit.

## Changes that need a remove + re-add

iOS remembers what an accessory offered when it was added. After enabling HomeKit Secure Video
(`hksv`, or `motion`) on a camera that is already in a home, or after a change to the audio
codecs it offers, remove it from the Home app and add it again (same code). Otherwise the
Recording Options don't appear, or live view keeps asking for the old audio codec.

## Live view

- **Spinner, no video**: `rust-homekit-cam probe <config>` should report H.264 frames. "no H.264
  track" means the camera streams H.265: switch it to H.264.
- **No audio**: live view audio needs an Opus track (the log shows `audio selected: codec 3` and
  `audio flowing`). Remotely, home hubs ask for 60 ms packets; that is handled.
- **Two-way audio silent at the camera** (Protect): the log should show
  `talkback: session to <camera>:7004` and packets forwarded. If so, the host can't reach the
  camera on UDP 7004 (firewall between VLANs).

## Recordings (HomeKit Secure Video)

- Recording Options missing in the Home app: see "remove + re-add" above.
- The log shows `hksv: recording enabled` and `recording: prebuffering` once recording is on in
  the Home app.
- Each trigger should log `hds: recording stream … open` and, when the hub is done, the number
  of fragments sent. No `open` after a motion: check that motion reaches the service
  (`motion (protect)` / `motion (http)`); test with `POST /motion`.
- `rust-homekit-cam record-test <config> clip.mp4` writes what the hub would get; it should play
  in QuickTime.
