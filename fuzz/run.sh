#!/usr/bin/env bash
# decmpfs fuzz runner (fleet property-and-fuzz lane; mirrors envrypt/fuzz).
# Single source of truth for the per-target libFuzzer flags so a local acceptance
# run and the CI job match exactly.
#
#   fuzz/run.sh <target> [max_total_time_seconds]   # default 600 = 10 min/target
#   fuzz/run.sh all       [max_total_time_seconds]   # each target in turn
#
# Requires a nightly toolchain (cargo-fuzz sets the sanitizer flags + `--cfg
# fuzzing`). Invoked via `cargo +nightly fuzz run` when cargo is the rustup shim,
# or `rustup run nightly cargo fuzz run` otherwise (both handled below).
#
# The build target defaults to the HOST triple (cargo-fuzz's own default) so the
# same script works on a macOS dev box and a Linux CI runner; override with
# FUZZ_TARGET_TRIPLE=x86_64-unknown-linux-gnu to pin CI.
set -euo pipefail

FUZZ_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DICT="$FUZZ_DIR/fuzz.dict"
DURATION="${2:-600}"
TARGETS=(decode_pressed_data unwrap_if_hybrid)

# Per-target libFuzzer flags, calibrated for the ASan + coverage instrumentation
# cargo-fuzz builds with:
#   * `-timeout=10` — absorbs the ~10x ASan slowdown (a genuine hang is unbounded
#     and still caught).
#   * `-rss_limit_mb=2048` — ASan shadow memory + libFuzzer's accumulating coverage
#     counters push the baseline RSS past the 512 MB default over a long run; a real
#     unbounded allocation still trips 2048 (the addon decoder caps a claimed
#     decompressed size at 512 MiB, so a per-exec blowup is already bounded).
#   * `-max_len` bounds a single input to a realistic pressed-section size.
target_flags() {
  case "$1" in
    decode_pressed_data) echo "-timeout=10 -rss_limit_mb=2048 -max_len=65536" ;;
    unwrap_if_hybrid)    echo "-timeout=10 -rss_limit_mb=2048 -max_len=65536" ;;
    *) echo "unknown target: $1" >&2; return 1 ;;
  esac
}

# Pick the invocation that handles `+nightly` in this environment.
run_fuzz() {
  if cargo +nightly --version >/dev/null 2>&1; then
    cargo +nightly fuzz "$@"
  else
    rustup run nightly cargo fuzz "$@"
  fi
}

run_one() {
  local t="$1"
  echo "===== fuzz: $t (max_total_time=${DURATION}s) ====="
  local target_args=()
  if [ -n "${FUZZ_TARGET_TRIPLE:-}" ]; then
    target_args=(--target "$FUZZ_TARGET_TRIPLE")
  fi
  # shellcheck disable=SC2046
  # `${arr[@]+"${arr[@]}"}` guards the empty-array case under `set -u` on bash 3.2.
  run_fuzz run ${target_args[@]+"${target_args[@]}"} "$t" -- \
    -max_total_time="$DURATION" -dict="$DICT" -print_final_stats=1 \
    $(target_flags "$t")
}

case "${1:-all}" in
  all)
    for t in "${TARGETS[@]}"; do run_one "$t"; done ;;
  decode_pressed_data|unwrap_if_hybrid)
    run_one "$1" ;;
  *)
    echo "usage: $0 <decode_pressed_data|unwrap_if_hybrid|all> [seconds]" >&2
    exit 2 ;;
esac
