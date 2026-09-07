#!/bin/sh
# Give locally built macOS executables an ad-hoc signature before Cargo runs
# them. Some managed Macs quarantine a fresh unsigned test or coverage binary.
# This is local execution signing, not the identity-based release signature.
#
# STAYS .sh, NOT .mts. Cargo invokes this as its `runner`, execing it directly
# with the target binary as argv — it wants a plain executable, and a Rust
# toolchain has no reason to carry node. Routing a cargo test through a
# TypeScript wrapper would make every `cargo test` depend on the JS runtime.
set -eu

if [ "$#" -lt 1 ]; then
  echo "usage: $0 <binary> [arguments...]" >&2
  exit 64
fi

binary=$1
shift

if [ "$(uname -s)" != 'Darwin' ]; then
  echo "sign-and-run is a macOS-only Cargo runner" >&2
  exit 69
fi

if ! command -v codesign >/dev/null 2>&1; then
  echo "codesign is required to run locally built macOS binaries" >&2
  exit 69
fi

# `--timestamp=none` keeps local ad-hoc signing offline and deterministic.
# A signing failure is fatal: never silently execute the unsigned binary.
codesign --force --sign - --timestamp=none "$binary"
exec "$binary" "$@"
