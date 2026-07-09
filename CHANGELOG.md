# Changelog

## 0.3.0

- `rm` / `rmSync`: `fs.rm`-parity removal (`recursive` / `force` / `maxRetries`
  / `retryDelay`) with a safe-delete guard that refuses the cwd, an ancestor, or
  the filesystem root unless `force` overrides.
- `compressFile` / `compressFileSync`: compress an existing file in place,
  transparently.
- macOS: LZVN block compression runs in parallel across cores — large addons
  compress several times faster on write.
- Node bindings surface `fs`-shaped errors (Node `code` / `errno` / `syscall`)
  across `write` / `copy` / `copyFile` / `rm`.
- Windows: `detect()` handles directories (opens with `FILE_FLAG_BACKUP_SEMANTICS`).
- Ship TypeScript declarations (`index.d.ts`) for the addon.

## 0.2.0

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
