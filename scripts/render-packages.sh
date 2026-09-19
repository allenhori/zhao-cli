#!/usr/bin/env bash
# Renders the Homebrew formula and Scoop manifest for a release, from the
# release's own archives (their sha256 sums are computed here, never
# hand-edited).
#
#   scripts/render-packages.sh <version> <archive-dir> <out-dir>
#
# <version> has no leading "v" (e.g. 0.5.3). <archive-dir> holds the four
# zhao-<target>.{tar.gz,zip} archives release.yml builds. Writes
# <out-dir>/zhao.rb (for allenhori/homebrew-zhao's Formula/) and
# <out-dir>/zhao.json (for allenhori/zhao-scoop's bucket/).
set -euo pipefail

version="${1:?usage: render-packages.sh <version> <archive-dir> <out-dir>}"
dir="${2:?missing <archive-dir>}"
out="${3:?missing <out-dir>}"

repo="allenhori/zhao-cli"
desc="Deterministic, offline change-review and CI gate for data transformation projects."
base="https://github.com/${repo}/releases/download/v${version}"

sha() {
  local file="$dir/$1"
  [ -f "$file" ] || { echo "missing archive: $file" >&2; exit 1; }
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | cut -d' ' -f1
  else
    shasum -a 256 "$file" | cut -d' ' -f1
  fi
}

mac_arm="zhao-aarch64-apple-darwin.tar.gz"
mac_x64="zhao-x86_64-apple-darwin.tar.gz"
linux_x64="zhao-x86_64-unknown-linux-gnu.tar.gz"
win_x64="zhao-x86_64-pc-windows-msvc.zip"

mkdir -p "$out"

cat > "$out/zhao.rb" <<RUBY
class Zhao < Formula
  desc "${desc}"
  homepage "https://github.com/${repo}"
  version "${version}"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "${base}/${mac_arm}"
      sha256 "$(sha "$mac_arm")"
    end
    on_intel do
      url "${base}/${mac_x64}"
      sha256 "$(sha "$mac_x64")"
    end
  end

  on_linux do
    on_intel do
      url "${base}/${linux_x64}"
      sha256 "$(sha "$linux_x64")"
    end
  end

  def install
    bin.install "zhao"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/zhao --version")
  end
end
RUBY

cat > "$out/zhao.json" <<JSON
{
    "version": "${version}",
    "description": "${desc}",
    "homepage": "https://github.com/${repo}",
    "license": "Apache-2.0",
    "architecture": {
        "64bit": {
            "url": "${base}/${win_x64}",
            "hash": "$(sha "$win_x64")"
        }
    },
    "bin": "zhao.exe"
}
JSON
