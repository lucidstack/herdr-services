#!/usr/bin/env bash
# Build hook: fetch a prebuilt release binary for this platform, falling back
# to `cargo build --release` when no release matches (or when offline).
set -euo pipefail

cd "$(dirname "$0")/.."
version="$(sed -n 's/^version = "\(.*\)"/\1/p' herdr-plugin.toml | head -n1)"
os="$(uname -s)"
arch="$(uname -m)"
case "${os}-${arch}" in
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64) target="aarch64-unknown-linux-gnu" ;;
  *) target="" ;;
esac

mkdir -p bin
url="https://github.com/lucidstack/herdr-services/releases/download/v${version}/herdr-services-${target}"
if [ -n "$target" ] && [ "${HERDR_SERVICES_NO_DOWNLOAD:-}" != "1" ] \
  && curl -fsSL --max-time 60 -o bin/herdr-services.tmp "$url" 2>/dev/null; then
  mv bin/herdr-services.tmp bin/herdr-services
  chmod +x bin/herdr-services
  echo "herdr-services: installed release v${version} (${target})"
  exit 0
fi
rm -f bin/herdr-services.tmp

if command -v cargo >/dev/null 2>&1; then
  echo "herdr-services: no release binary for ${os}-${arch}; building with cargo"
  cargo build --release --quiet
  cp target/release/herdr-services bin/herdr-services
  chmod +x bin/herdr-services
  echo "herdr-services: built from source"
  exit 0
fi

echo "herdr-services: no release binary and cargo is not installed" >&2
exit 1
