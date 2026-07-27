# Changelog

## [0.1.3](https://github.com/SocketDev/decmpfs/releases/tag/v0.1.3) - 2026-07-27

### Fixed

- **`release`** — generator re-pins the loader's optionalDependencies to its own version
- **`release`** — fetch tags in the npm-publish checkout so first-publish bump derivation can anchor
- **`deps`** — sync -stable catalog aliases to their base versions
- **`fleet`** — restore fetch-fleet-bundle to the v1.0.14 manifest bytes
- **`npm`** — point repository metadata at the real SocketDev owner
- **`deps`** — override js-yaml to 5.2.2 for GHSA-pm4m-ph32-ghv5

## 0.1.2

- Maintenance release — no API or behavior changes; platform packages rebuilt.

## 0.1.1

- `rm` / `rmSync`: `fs.rm`-parity removal (`recursive` / `force` / `maxRetries`
  / `retryDelay`) with a safe-delete guard that refuses the cwd, an ancestor, or
  the filesystem root unless `force` overrides.
- `compressFile` / `compressFileSync`: compress an existing file in place,
  transparently.
- macOS: LZVN block compression runs in parallel across cores — large addons
  compress several times faster on write.
- Smaller shipped addon: fat LTO + a single codegen unit + symbol stripping and
  per-platform CPU baselines (x86-64-v2, apple-m1) trim the `.node` ~20%.
- Node bindings surface `fs`-shaped errors (Node `code` / `errno` / `syscall`)
  across `write` / `copy` / `copyFile` / `rm`.
- Windows: `detect()` handles directories (opens with `FILE_FLAG_BACKUP_SEMANTICS`).
- Ship TypeScript declarations (`index.d.ts`) for the addon.
- `copy_file` / `try_clone_file`: compression-preserving copy — clone when the
  filesystem can (macOS `clonefile`, Linux `FICLONE`), recompress when it
  can't, plain-copy only when the source wasn't compressed.
- Node: `copyDecmpfsFile{,Sync}` (`fs.cp`-shaped `force` / `errorOnExist`) and
  `copyFile{,Sync}` (`fsPromises.copyFile` signature with `COPYFILE_EXCL` /
  `COPYFILE_FICLONE` / `COPYFILE_FICLONE_FORCE`).
- Self-replacing executable packing (`exe` feature): `pack_executable_with_stub`
  injects a compressed payload into the `decmpfs-stub` binary; on first run the
  packed file materializes the payload FS-compressed, replaces itself, and execs
  it. Node `packExecutable{,Sync}` exposes the packer.

## 0.1.0

- Initial release: one-pass transparent filesystem compression (APFS decmpfs /
  btrfs / NTFS) via `compress_bytes` / `compress_file`, fail-soft `Outcome`
  vocabulary, `Gate` glob/size filtering, and the Node `writeDecmpfsFile`
  bindings.
