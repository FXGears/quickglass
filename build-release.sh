#!/usr/bin/env bash
# Builds both release binaries from a Linux host.
#
# Windows: cross-built for x86_64-pc-windows-gnu (mingw-w64) with the same
# size-optimized nightly std as build-release.ps1. Linux: stable release build.
#
# Needs: mingw-w64, rustup toolchain nightly + rust-src + the windows-gnu target
#   rustup toolchain install nightly --profile minimal -c rust-src -t x86_64-pc-windows-gnu
#
# Output: target/x86_64-pc-windows-gnu/release/quickglass.exe
#         target/release/quickglass
set -euo pipefail
cd "$(dirname "$0")"

RUSTFLAGS='-Zunstable-options -Cpanic=immediate-abort' \
    cargo +nightly build --release --target x86_64-pc-windows-gnu \
    -Zbuild-std=std,panic_abort -Zbuild-std-features=optimize_for_size
cargo build --release

ls -l target/x86_64-pc-windows-gnu/release/quickglass.exe target/release/quickglass
