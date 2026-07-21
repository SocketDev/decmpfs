#!/usr/bin/env bash
# Grep-level lint: every `unsafe` on the UNTRUSTED-INPUT decode/parse surface of
# decmpfs must carry a `// FUZZ: <target>` annotation naming the fuzz target that
# covers the path, so no `unsafe` byte-parser ships without a fuzz target
# exercising it. The annotation appears on the same line as the `unsafe` token or
# within the 3 preceding lines. (Mirrors envrypt/fuzz/no-unsafe-without-fuzz.sh.)
#
# decmpfs's ONLY `unsafe` today is OS SYSCALL / FFI glue — statfs/lstat/xattr/
# chflags/clonefile/libcompression on macOS, the btrfs ioctls on Linux, the Win32
# file-attribute + GetCompressedFileSizeW calls, and a test-only `geteuid`. That
# code is fed paths + kernel handles, NOT the attacker-controlled `&[u8]` a fuzz
# target drives, and it is covered by the crate's kernel-roundtrip integration
# tests, not byte fuzzing. So the platform-backend files below are EXCLUDED from
# this gate; its remit is the hand-rolled byte parsers (addon.rs hybrid decoder,
# exe/ Mach-O/ELF/PE section walkers) — where a NEW `unsafe` with no fuzz target
# is exactly what must be caught.
#
# Exit 0 = clean (the current state: zero `unsafe` on the parse surface). Exit 1
# lists every unannotated offender. Run from anywhere.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/crates/decmpfs/src"

# Platform-backend / FFI-syscall files: audited kernel glue, not byte parsers.
# Excluded from the fuzz-coverage requirement (see header).
is_excluded() {
  case "$1" in
    */macos.rs | */linux.rs | */windows.rs | */unsupported.rs | */verify.rs | */lib.rs) return 0 ;;
    *) return 1 ;;
  esac
}

offenders=0
# `-n` line numbers, word-boundary `unsafe`; skip if none.
while IFS= read -r hit; do
  [ -n "$hit" ] || continue
  file="${hit%%:*}"
  rest="${hit#*:}"
  line="${rest%%:*}"
  content="${rest#*:}"
  is_excluded "$file" && continue
  # Skip comment-only lines: a real `unsafe` block is never a `//`-led line, so the
  # only such matches are doc-comment word mentions.
  case "${content#"${content%%[![:space:]]*}"}" in
  //*) continue ;;
  esac
  start=$((line > 3 ? line - 3 : 1))
  # The annotation may be on the unsafe line or up to 3 lines above it.
  if sed -n "${start},${line}p" "$file" | grep -q '// FUZZ:'; then
    continue
  fi
  echo "::error file=${file#"$ROOT"/},line=${line}::unsafe on the decode/parse surface without a '// FUZZ: <target>' annotation"
  offenders=$((offenders + 1))
done < <(grep -rn --include='*.rs' -E '(^|[^A-Za-z_])unsafe([^A-Za-z_]|$)' "$SRC" 2>/dev/null || true)

if [ "$offenders" -gt 0 ]; then
  echo "no-unsafe-without-fuzz: $offenders unannotated unsafe block(s) on the decmpfs parse surface" >&2
  exit 1
fi
echo "no-unsafe-without-fuzz: OK (no unfuzzed unsafe on the decmpfs decode/parse surface)"
