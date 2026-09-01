#!/usr/bin/env bash
# Cross-compile a static aarch64 binary for the Jetson TX2.
#
# musl static rather than glibc: JetPack 4.6 is Ubuntu 18.04 with glibc 2.27,
# and a statically linked binary sidesteps that entirely — nothing to install on
# the device.
set -euo pipefail

TARGET=aarch64-unknown-linux-musl
OUT="target/$TARGET/release/rustclaw"

command -v cargo-zigbuild >/dev/null || {
  echo "cargo-zigbuild not found. Install it with:" >&2
  echo "  brew install rustup zig && cargo install cargo-zigbuild" >&2
  exit 1
}

rustup target add "$TARGET" 2>/dev/null || true

echo "building $TARGET ..."
cargo zigbuild --release --target "$TARGET" "$@"

echo
file "$OUT"
echo "size: $(du -h "$OUT" | cut -f1)"
echo
echo "deploy with:"
echo "  scp $OUT nvidia@<jetson>:~/rustclaw"
echo "  ssh nvidia@<jetson> './rustclaw config --init && ./rustclaw repl'"
