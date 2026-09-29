#!/usr/bin/env bash
# Build rsc_driver against the installed nightly's rustc-dev (no cargo needed).
# rustc auto-locates `extern crate rustc_*` from the sysroot via -L.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
# Immune to directory rust-toolchain overrides: this driver needs the nightly
# that actually has the rustc-dev component.
export RUSTUP_TOOLCHAIN=nightly
SYSROOT="$(rustc --print sysroot)"
LIB="$SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/lib"
mkdir -p target
rustc --edition 2021 --crate-name rsc_driver driver.rs \
  -L "dependency=$LIB" \
  -L "$SYSROOT/lib" \
  -o target/rsc_driver
echo "built target/rsc_driver"
