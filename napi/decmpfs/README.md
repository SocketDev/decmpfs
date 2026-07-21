<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/SocketDev/decmpfs/main/assets/repo/decmpfs-for-npm-dark.svg">
    <img alt="decmpfs — by socket labs" src="https://raw.githubusercontent.com/SocketDev/decmpfs/main/assets/repo/decmpfs-for-npm-light.svg" width="360">
  </picture>
</p>

[![Socket Badge](https://badge.socket.dev/npm/package/decmpfs)](https://socket.dev/npm/package/decmpfs)

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
