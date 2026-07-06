# decmpfs

![coverage score](assets/coverage-score.svg) [![Socket score](assets/socket-score.svg)](https://socket.dev/cargo/package/decmpfs)

Apply the operating system's **transparent per-file filesystem compression** to a
file — smaller on disk, byte-identical on read, decompressed by the kernel at
near-native speed. macOS APFS (decmpfs/LZVN), Linux btrfs (zstd→lzo→zlib), Windows
NTFS (LZNT1).

## Why this repo exists

Disk-heavy artifacts (native addons, bundled binaries, package stores) compress
40–60% with the compression the OS already ships, but every runtime writes them
uncompressed and no portable API exists to fix that. decmpfs is that API:

- **One pass.** `compress_bytes` writes bytes straight to an OS-compressed file —
  never write-then-recompress.
- **Outcome, never a surprise error.** Every call returns an `Outcome` —
  `Compressed`, `NoGain` (incompressible / sub-cluster), `AlreadyCompressed`,
  `Unsupported` (ext4, xfs, ZFS, ReFS, FAT, tmpfs, network mounts), or `Skipped`
  (permission, lock, gate). `Err` is reserved for genuine I/O failures.
- **Compression-preserving copy.** A plain byte copy silently re-inflates a
  compressed file; `copy_file` clones (macOS `clonefile`, Linux `FICLONE`) or
  recompresses so the savings survive the copy. Node's own `fs.copyFile` cannot
  do this on macOS — libuv has no `clonefile` path (`COPYFILE_FICLONE` falls back
  to a byte copy, `COPYFILE_FICLONE_FORCE` throws `ENOSYS`).
- **Speed-first codecs** (a file is written once, read on load): LZVN on macOS,
  zstd→lzo→zlib on btrfs, LZNT1 on NTFS (survives a reinstall's open-for-write,
  unlike WOF).

## Install

```sh
cargo add decmpfs
```

```sh
npm install decmpfs
```

The core crate is dependency-light (`libc` / `windows-sys` only). The optional
`addon` feature pulls `zstd` + `sha2` to unwrap a napi `--compress` hybrid `.node`
back to the raw addon before compressing.

## Usage

Rust:

```rust
use decmpfs::{compress_bytes, compress_file, copy_file, try_clone_file, Gate};

// Write `content` straight to an OS-compressed file (single pass).
let outcome = compress_bytes(path, &content, &Gate::any())?;

// Or compress a file that already exists, in place.
let outcome = compress_file(path)?;

// Copy without losing the compression: clone when the FS can, recompress when
// it can't, plain-copy only when the source wasn't compressed.
let copied = copy_file(src, dest)?;

// Reflink-or-decline: true when the OS cloned, false to fall back yourself.
let cloned = try_clone_file(src, dest)?;
```

The `Gate` decides which files to compress by glob and/or size:

```rust
let gate = Gate::new(Some("**/*.node"), Some(">= 1MB"))?;
```

Node (an N-API binding in [`napi/`](napi/), async + `Sync` variants of each):

- `writeDecmpfsFile(path, data)` — `fs.writeFile`-shaped, atomic by default,
  lands the bytes already compressed.
- `copyDecmpfsFile(src, dest, { force, errorOnExist })` — `fs.cp`-shaped
  compression-preserving copy.
- `copyFile(src, dest, mode)` — `fsPromises.copyFile` signature, including
  `COPYFILE_EXCL` / `COPYFILE_FICLONE` / `COPYFILE_FICLONE_FORCE`, backed by the
  clone-first copy libuv lacks on macOS.

## Development

```sh
cargo test --workspace
npm test --prefix napi/decmpfs
node scripts/check-versions.mjs
```

The napi addon rebuilds with `npm run build` in `napi/decmpfs/`. The coverage
badge (`assets/coverage-score.svg`) is regenerated from `cargo llvm-cov` output.

## License

MIT
