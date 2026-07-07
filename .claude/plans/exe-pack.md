# decmpfs exe-pack (0.2.0, unpublished)

Compressed **self-replacing executable** support: pack a real executable `E`
into a stub `E'` that, on first run, materializes `E` back onto disk
**FS-compressed** (decmpfs), atomically replaces itself, and `execve`s it — so
every later run is native-speed off a smaller-on-disk binary with zero stub
overhead.

## Why a section, not an EOF trailer

The napi-compress addon path proved (fork commit `dc14bab`) that appended
trailer bytes fail macOS strict codesign validation and Gatekeeper. An
executable is judged harder than a dylib, so macOS packing MUST inject a
**signable named section** (`SMOL/__DECMPFS`) and ad-hoc re-sign, exactly like
the addon injector. ELF and PE loaders don't enforce a signature to `execve`, so
those targets may append `[payload][footer]` after the image — cheaper, no
section-table surgery.

## Wire format (one source of truth)

Reuse the section ABI verbatim from the fork:
`[MAGIC "NAPCSECT"][content_hash u64 LE][zstd payload]`. `content_hash` = FNV-1a
of the raw executable (names the temp/rename target, verifies the decompress).
ELF/PE footer = `[payload][content_hash u64][payload_len u64][MAGIC]` at EOF, so
the runtime seeks the tail, reads back `payload_len`, and validates the magic.

## Module layout (decmpfs crate) — file boundaries locked so stages don't collide

- `src/exe/mod.rs` — `pub mod` wiring + the public API surface:
  - `pub fn pack_executable(src, dest, gate) -> Result<PackOutcome>` (host packer)
  - `pub fn self_replace_and_exec(args) -> Result<Never>` (runtime entry the stub calls)
  - `PackOutcome { Packed { before, after }, SkippedTooSmall, … }`
- `src/exe/section.rs` — port of the fork's `section.rs`: `build_section_payload`,
  `read_self_section`, per-OS `find_section` (cfg-gated), synthetic-object test helper.
- `src/exe/inject.rs` — port of the fork's `inject.rs`: Mach-O segment splice +
  `resign` (apple-codesign, macOS-only, behind the `exe` feature), ELF/PE append.
- `src/exe/replace.rs` — the runtime: resolve self path → read payload → zstd
  decompress → `compress_bytes` the bytes to a temp on the same fs → atomic
  rename over `argv[0]` (unix) / swap-on-next-start (Windows, since a running
  image can't be replaced) → macOS re-sign the materialized binary →
  `execve(argv[0], argv, envp)`.
- `lib.rs` — add `#[cfg(feature = "exe")] pub mod exe;` (the ONLY lib.rs edit).

## Feature gating

New `exe` cargo feature pulls `zstd` + `sha2` + `apple-codesign` (macOS). The
dep-free core stays untouched — same posture as the existing `addon` feature.

## napi surface (napi/decmpfs)

- `packExecutable(src, dest, options?) -> PackResult` (sync + async).
- The runtime `self_replace_and_exec` is NOT exposed to Node (it's for a Rust
  stub binary); Node only produces packed executables.

## Stages (sequential — one crate, no parallel edits to shared files)

1. **Design freeze** (this doc) + `src/exe/` skeleton with signatures, `mod`
   wired, `cargo build --features exe` compiles (empty bodies `todo!()`-free,
   return `Unsupported`).
2. **section.rs** port + unit tests (synthetic-object round-trip).
3. **inject.rs** port (Mach-O splice + resign, ELF/PE append) + tests.
4. **replace.rs** runtime (decompress → compress_bytes → rename → execve; Windows
   deferred-swap) + tests (a tiny packed shell/`true` round-trip on the host OS).
5. **pack_executable** public API tying section+inject+gate together + tests.
6. **napi** `packExecutable` binding + `test/exe.test.mjs`.
7. **Review** (big-brain) → floor follow-up → `npm run check` + `cargo test` green.

## Verification

`cargo test --features exe --workspace`; a host-OS end-to-end test packs a
trivial executable, runs the packed stub, asserts (a) it produced identical
output to the original, (b) the on-disk file is now FS-compressed
(`blocks*512 < logical`), (c) a second run execs directly (no re-materialize).
CHANGELOG 0.2.0 gains an exe-pack bullet; version stays 0.2.0 (unpublished).
