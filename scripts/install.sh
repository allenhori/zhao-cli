#!/usr/bin/env sh
# Installs the `zhao` CLI binary for the current platform.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/allenhori/zhao-cli/master/scripts/install.sh | sh
#
# Environment variables:
#   ZHAO_VERSION    A release tag to install (e.g. "v0.1.0" or "nightly").
#                    Defaults to "latest" (the newest stable release).
#   ZHAO_INSTALL_DIR Where to place the binary. Defaults to "$HOME/.zhao/bin".
#
# This script only downloads and unpacks a pre-built binary from
# https://github.com/allenhori/zhao-cli/releases -- it doesn't need a Rust
# toolchain, and it never asks for elevated privileges.

set -eu

REPO="allenhori/zhao-cli"
VERSION="${ZHAO_VERSION:-latest}"
INSTALL_DIR="${ZHAO_INSTALL_DIR:-$HOME/.zhao/bin}"

say() { printf '%s\n' "$1"; }
die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        arm64) echo "aarch64-apple-darwin" ;;
        x86_64) echo "x86_64-apple-darwin" ;;
        *) die "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64) echo "x86_64-unknown-linux-gnu" ;;
        *) die "unsupported Linux architecture: $arch (only x86_64 has a released binary today -- build from source instead: cargo install --git https://github.com/$REPO)" ;;
      esac
      ;;
    *)
      die "unsupported OS: $os (Windows users: download zhao-x86_64-pc-windows-msvc.zip directly from https://github.com/$REPO/releases)"
      ;;
  esac
}

main() {
  command -v curl >/dev/null 2>&1 || die "curl is required"
  command -v tar >/dev/null 2>&1 || die "tar is required"

  target="$(detect_target)"
  say "Detected platform: $target"

  if [ "$VERSION" = "latest" ]; then
    url="https://github.com/$REPO/releases/latest/download/zhao-$target.tar.gz"
  else
    url="https://github.com/$REPO/releases/download/$VERSION/zhao-$target.tar.gz"
  fi

  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT

  say "Downloading $url"
  curl -fsSL "$url" -o "$tmp_dir/zhao.tar.gz" \
    || die "download failed -- check that $VERSION is a real release tag at https://github.com/$REPO/releases"

  tar -xzf "$tmp_dir/zhao.tar.gz" -C "$tmp_dir"

  mkdir -p "$INSTALL_DIR"
  mv "$tmp_dir/zhao" "$INSTALL_DIR/zhao"
  chmod +x "$INSTALL_DIR/zhao"

  say "Installed zhao to $INSTALL_DIR/zhao"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      say ""
      say "$INSTALL_DIR isn't on your PATH yet. Add it, e.g.:"
      say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc   # or ~/.zshrc"
      ;;
  esac

  say ""
  say "Run 'zhao --help' to get started (after adding it to your PATH, or via $INSTALL_DIR/zhao directly)."
}

main
