#!/bin/sh
# Install or upgrade Donut from the latest GitHub release. Settings in ~/.config/donut are kept.
# DONUT_INSTALL_DIR picks the folder for the executables (default ~/.local/bin).
# DONUT_INSTALL_SOURCE installs a local release archive instead of downloading one.
set -eu
printf 'Installing or upgrading Donut…\n'
bin_dir=${DONUT_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) printf 'Donut has no release for %s. Use install.ps1 on Windows.\n' "$(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) printf 'Donut has no release for the %s processor.\n' "$(uname -m)" >&2; exit 1 ;;
esac
target="$arch-$os"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
source=${DONUT_INSTALL_SOURCE:-}
if [ -z "$source" ]; then
  base=https://github.com/ihawn/Donut/releases/latest/download
  curl -fsSL "$base/donut-$target.tar.gz" -o "$work/donut.tar.gz"
  curl -fsSL "$base/SHA256SUMS" -o "$work/SHA256SUMS"
  expected=$(grep " donut-$target.tar.gz\$" "$work/SHA256SUMS" | cut -d' ' -f1)
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$work/donut.tar.gz" | cut -d' ' -f1)
  else
    actual=$(shasum -a 256 "$work/donut.tar.gz" | cut -d' ' -f1)
  fi
  if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    printf 'The downloaded archive does not match its checksum. Nothing was installed.\n' >&2
    exit 1
  fi
  source="$work/donut.tar.gz"
fi
# Remove the earlier Python install first, so its launcher does not hide this one.
if command -v uv >/dev/null 2>&1 && uv tool list 2>/dev/null | grep -q '^donut-cli '; then
  uv tool uninstall donut-cli >/dev/null 2>&1 || true
fi

mkdir -p "$work/unpacked" "$bin_dir"
tar -xzf "$source" -C "$work/unpacked"
for name in donut qwen-build-next; do
  found=$(find "$work/unpacked" -type f -name "$name" | head -n 1)
  if [ -z "$found" ]; then
    printf 'The archive has no %s executable. Nothing was installed.\n' "$name" >&2
    exit 1
  fi
  # Replace by rename, so a running Donut keeps its old file.
  cp "$found" "$bin_dir/.$name.new"
  chmod 755 "$bin_dir/.$name.new"
  mv -f "$bin_dir/.$name.new" "$bin_dir/$name"
done

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) printf '\nAdd %s to your PATH, for example in ~/.profile:\n  export PATH="%s:$PATH"\n' "$bin_dir" "$bin_dir" ;;
esac
"$bin_dir/donut" welcome
