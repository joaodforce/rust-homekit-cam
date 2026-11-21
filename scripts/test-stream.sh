#!/bin/sh
# Fake RTSP camera for testing (Docker: MediaMTX + ffmpeg), e.g. audio transcoding.
#
#   scripts/test-stream.sh pcma          # (re)start publishing with G.711 A-law audio
#   scripts/test-stream.sh stop          # stop both containers
#
# Publishes rtsp://<this host>:8554/test (1280x720 25 fps, 2.5 Mbps, keyframe every 2 s) and
# rtsp://<this host>:8554/test_low (640x360, 400 kbps), like a camera's main and sub stream.
# Video: VIDEO=file (default ./stream.mp4; encoded once per setting into .test-stream/ next to it,
# then looped without re-encoding), or a live-encoded test pattern when it's missing.
# Audio: the file's audio converted to the variant's codec; BEEP=1 uses a 1 s beep instead
# (for checking A/V sync against the pattern's counter).
# KBPS=2500 / LOW_KBPS=400 set the main / sub stream's bitrate; SLICES=1 makes ffmpeg send one slice per frame
# (by default its low-latency mode splits every frame into several, like some cameras do).
set -eu

variant=${1:-}
net=rhc-test
cd "$(dirname "$0")/.."
VIDEO=${VIDEO:-stream.mp4}

case "$variant" in
  opus)         a="-c:a libopus -ar 48000 -ac 1 -b:a 32k" ;;
  opus-stereo)  a="-c:a libopus -ar 48000 -ac 2 -b:a 64k" ;;
  pcmu)         a="-c:a pcm_mulaw -ar 8000 -ac 1" ;;
  pcma)         a="-c:a pcm_alaw -ar 8000 -ac 1" ;;
  aac8)         a="-c:a aac -ar 8000 -ac 1 -b:a 24k" ;;
  aac16)        a="-c:a aac -ar 16000 -ac 1 -b:a 32k" ;;
  aac44)        a="-c:a aac -ar 44100 -ac 1 -b:a 64k" ;;
  aac48-stereo) a="-c:a aac -ar 48000 -ac 2 -b:a 96k" ;;
  g722)         a="-c:a g722 -ar 16000 -ac 1" ;;  # unsupported: expect "no usable audio track"
  none)         a="-an" ;;
  stop)
    docker rm -f rhc-publisher rhc-mediamtx >/dev/null 2>&1 || true
    echo "stopped"; exit 0 ;;
  *)
    echo "usage: $0 opus|opus-stereo|pcmu|pcma|aac8|aac16|aac44|aac48-stereo|g722|none|stop" >&2
    exit 2 ;;
esac

docker network inspect $net >/dev/null 2>&1 || docker network create $net >/dev/null
if ! docker ps -q -f name=^rhc-mediamtx$ | grep -q .; then
  docker rm -f rhc-mediamtx >/dev/null 2>&1 || true
  docker run -d --name rhc-mediamtx --network $net -p 8554:8554 \
    -e MTX_RTSPTRANSPORTS=tcp -e MTX_RTMP=no -e MTX_HLS=no -e MTX_WEBRTC=no -e MTX_SRT=no \
    bluenviron/mediamtx:1.21.4 >/dev/null
  sleep 1
fi
docker rm -f rhc-publisher >/dev/null 2>&1 || true

x264=""
[ "${SLICES:-0}" = 1 ] && x264="-x264-params sliced-threads=0"
kbps=${KBPS:-2500}
low=${LOW_KBPS:-400}
v="$x264 -c:v libx264 -preset veryfast -tune zerolatency -profile:v main -pix_fmt yuv420p -r 25 -g 50 -keyint_min 50 -sc_threshold 0"

if [ -f "$VIDEO" ]; then
  # encode the clip once per setting (cached), then loop it without re-encoding the video:
  # only the audio is converted live
  dir=$(cd "$(dirname "$VIDEO")" && pwd)
  cache="$dir/.test-stream"
  mkdir -p "$cache"
  base=$(basename "$VIDEO" | sed 's/\.[^.]*$//')
  hi="$base-720p-${kbps}k-s${SLICES:-0}.mp4"
  lo="$base-360p-${low}k-s${SLICES:-0}.mp4"
  for f in "$hi:1280:720:$kbps" "$lo:640:360:$low"; do
    IFS=: read -r name w h rate <<EOF2
$f
EOF2
    if [ ! -f "$cache/$name" ]; then
      echo "encoding $name once (cached in $cache)"
      # shellcheck disable=SC2086
      docker run --rm -v "$dir":/media lscr.io/linuxserver/ffmpeg:latest -hide_banner -loglevel error -y \
        -i "/media/$(basename "$VIDEO")" -vf "scale=$w:$h" $v -b:v ${rate}k -maxrate ${rate}k -bufsize ${rate}k \
        -c:a copy "/media/.test-stream/$name"
    fi
  done
  mount="-v $dir:/media:ro"
  [ "${BEEP:-0}" = 1 ] && aud="2:a" || aud="0:a:0"
  # shellcheck disable=SC2086
  docker run -d --name rhc-publisher --network $net $mount lscr.io/linuxserver/ffmpeg:latest \
    -hide_banner -loglevel warning \
    -re -stream_loop -1 -i "/media/.test-stream/$hi" -re -stream_loop -1 -i "/media/.test-stream/$lo" \
    -f lavfi -i "sine=frequency=880:beep_factor=4:sample_rate=48000" \
    -map 0:v -map "$aud" -c:v copy $a -f rtsp -rtsp_transport tcp rtsp://rhc-mediamtx:8554/test \
    -map 1:v -map "$aud" -c:v copy $a -f rtsp -rtsp_transport tcp rtsp://rhc-mediamtx:8554/test_low >/dev/null
  echo "publishing $variant: rtsp://127.0.0.1:8554/test and /test_low (logs: docker logs -f rhc-publisher)"
  exit 0
fi

if true; then
  src="-re -f lavfi -i testsrc=size=1280x720:rate=25"
  mount=""
  aud="1:a"
fi
# shellcheck disable=SC2086
docker run -d --name rhc-publisher --network $net $mount lscr.io/linuxserver/ffmpeg:latest \
  -hide_banner -loglevel warning $src \
  -f lavfi -i "sine=frequency=880:beep_factor=4:sample_rate=48000" \
  -filter_complex "[0:v]scale=1280:720,split[hi][x];[x]scale=640:360[lo]" \
  -map "[hi]" -map "$aud" $v -b:v ${kbps}k -maxrate ${kbps}k -bufsize ${kbps}k $a \
  -f rtsp -rtsp_transport tcp rtsp://rhc-mediamtx:8554/test \
  -map "[lo]" -map "$aud" $v -b:v ${low}k -maxrate ${low}k -bufsize ${low}k $a \
  -f rtsp -rtsp_transport tcp rtsp://rhc-mediamtx:8554/test_low >/dev/null

echo "publishing $variant: rtsp://127.0.0.1:8554/test and /test_low (logs: docker logs -f rhc-publisher)"
