# decmpfs

Apply the operating system's **transparent per-file filesystem compression** to a
file — smaller on disk, byte-identical on read, decompressed by the kernel at
near-native speed. macOS APFS (decmpfs/LZVN), Linux btrfs (zstd→lzo→zlib),
Windows NTFS (LZNT1).

This is the Node addon for [decmpfs](https://github.com/decmpfs/decmpfs). The
prebuilt native binary ships as an optional dependency per platform
(`@decmpfs/<triple>`); pnpm installs only the one matching the host.

## Install

```sh
npm install decmpfs
```

## Usage

```js
const { writeDecmpfsFile, copyDecmpfsFile } = require('decmpfs')

// Write bytes straight to an OS-compressed file — one pass, no recompress.
const result = writeDecmpfsFile('data.bin', bytes)

// Compression-preserving copy (clonefile / FICLONE, or recompress).
copyDecmpfsFile('data.bin', 'copy.bin')
```

See the [repository](https://github.com/decmpfs/decmpfs) for the full API and the
Rust crate.

## License

MIT
