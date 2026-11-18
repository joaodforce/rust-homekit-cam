#!/bin/sh
# Install or upgrade rust-homekit-cam from the GitHub releases (static binary, any distro with
# systemd): x86_64, arm64 (aarch64) or 32-bit ARM (armv7).
#
#   curl -fsSL https://raw.githubusercontent.com/joaodforce/rust-homekit-cam/main/install.sh | sudo sh
#   ... | sudo VERSION=v0.1.0 sh      # a specific release
#
# Installs /usr/local/bin/rust-homekit-cam and the rust-homekit-cam@.service template, creates
# /etc/rust-homekit-cam/ and restarts cameras that are already running. Configs are never touched.
set -eu

REPO=joaodforce/rust-homekit-cam
VERSION=${VERSION:-latest}

die() { echo "install.sh: $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root (sudo)"
command -v systemctl >/dev/null || die "systemd is required"
case "$(uname -s)" in Linux) ;; *) die "Linux only" ;; esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  armv7l | armv7*) arch=armv7 ;;
  *) die "no build for $(uname -m)" ;;
esac

if [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi
name="rust-homekit-cam-$arch-linux"

fetch() {
  if command -v curl >/dev/null; then curl -fsSL -o "$2" "$1"
  elif command -v wget >/dev/null; then wget -qO "$2" "$1"
  else die "curl or wget is required"; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading $name ($VERSION)"
fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" || die "download failed: $base/$name.tar.gz"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
(cd "$tmp" && grep " $name.tar.gz\$" SHA256SUMS | sha256sum -c - >/dev/null) || die "checksum mismatch"
tar -C "$tmp" -xzf "$tmp/$name.tar.gz"

install -m 755 "$tmp/$name/rust-homekit-cam" /usr/local/bin/rust-homekit-cam.new
mv /usr/local/bin/rust-homekit-cam.new /usr/local/bin/rust-homekit-cam
install -m 644 "$tmp/$name/rust-homekit-cam@.service" /etc/systemd/system/
install -d -m 755 /usr/local/share/rust-homekit-cam
install -m 644 "$tmp/$name/config.example.toml" /usr/local/share/rust-homekit-cam/
install -d -m 700 /etc/rust-homekit-cam
systemctl daemon-reload
running=$(systemctl list-units --plain --no-legend --state=running 'rust-homekit-cam@*' | awk '{print $1}')
[ -z "$running" ] || systemctl restart $running

echo "installed: $(/usr/local/bin/rust-homekit-cam --version 2>/dev/null || echo rust-homekit-cam)"
[ -z "$running" ] && cat <<EOF

Next: one config per camera, then start it.
  cp /usr/local/share/rust-homekit-cam/config.example.toml /etc/rust-homekit-cam/frontdoor.toml
  chmod 600 /etc/rust-homekit-cam/frontdoor.toml     # then edit it
  systemctl enable --now rust-homekit-cam@frontdoor
  rust-homekit-cam code /etc/rust-homekit-cam/frontdoor.toml
EOF
[ -n "$running" ] && echo "restarted: $running"
exit 0
