#!/bin/sh
# Runs the unit tests of LazyTO mode's pure modules on a PC. The firmware
# itself builds only for the ESP32-S3, so `cargo test` cannot run them; these
# files import nothing from the crate, so plain rustc can.
#
#     sh tools/lazyto_host_tests.sh
#
# Needs a host Rust toolchain (rustup's stable; rust-toolchain.toml's "esp"
# channel is overridden with +stable).
set -eu
cd "$(dirname "$0")/.."
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
for f in src/lazyto/hmac.rs src/lazyto/wire.rs; do
    name=$(basename "$f" .rs)
    rustc +stable --edition 2021 --test -o "$out/$name" "$f"
    "$out/$name" --quiet
done
