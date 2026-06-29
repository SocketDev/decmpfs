# decmpfs

![coverage score](assets/coverage-score.svg) [![Socket score](assets/socket-score.svg)](https://socket.dev/cargo/package/decmpfs)

Apply the operating system's **transparent per-file filesystem compression** to a
file — smaller on disk, byte-identical on read, decompressed by the kernel at
near-native speed. macOS APFS (decmpfs/LZVN), Linux btrfs (zstd→lzo→zlib), Windows
NTFS (LZNT1).

The core does the bytes-to-compressed-file write in **one pass** — it never writes a
file then reads it back to recompress.

```rust
use decmpfs::{compress_bytes, compress_file, Gate};

// Write `content` straight to an OS-compressed file (single pass).
let outcome = compress_bytes(path, &content, &Gate::any())?;

// Or compress a file that already exists, in place.
let outcome = compress_file(path)?;
```

## Outcome, never a surprise error

Every call returns an `Outcome` — `Compressed`, `NoGain` (incompressible /
sub-cluster), `AlreadyCompressed`, `Unsupported` (the FS has no per-file
compression — ext4, xfs, ZFS, ReFS, FAT, tmpfs, network mounts), or `Skipped`
(permission, lock, gate). An unsupported filesystem or a permission issue is a
non-fatal `Outcome`, not an `Err`; `Err` is reserved for genuine I/O failures.

## Gate

`Gate` decides which files to compress by glob and/or size:

```rust
let gate = Gate::new(Some("**/*.node"), Some(">= 1MB"))?;
```

## Codecs (speed over ratio — a file is written once, read on load)

- **macOS:** LZVN (the fastest decmpfs codec to decompress).
- **Linux btrfs:** zstd, falling back to lzo then zlib on older kernels.
- **Windows NTFS:** LZNT1 (survives a reinstall's open-for-write, unlike WOF).

## Features

The core is dependency-light (`libc` / `windows-sys` only). The optional `addon`
feature pulls `zstd` + `sha2` to unwrap a napi `--compress` hybrid `.node` back to
the raw addon before compressing.

## Node

An N-API binding (`writeDecmpfsFile` / `writeDecmpfsFileSync`, fs.writeFile-shaped,
atomic by default) lives in [`node/`](node/).

## License

MIT
