//! macOS backend — APFS/HFS+ decmpfs transparent compression.
//!
//! decmpfs is an undocumented kernel ABI; afsctool and `ditto --hfsCompression` are
//! the references. We write the LZVN resource-fork variant (compression type 8): the
//! kernel decompresses on read(), so the file keeps its logical size + stays a
//! loadable native binary. LZVN comes from the system libcompression framework — no
//! Rust codec dep, so the macOS stub stays tiny.
//!
//! Codec choice is LZVN, not LZFSE (type 12) or ZLIB (type 4): for a load-once
//! `.node` we favor decode speed over ratio, and LZVN is the fastest of the three
//! to decompress (LZFSE wins ~10-15% on size but decodes slower). LZVN has shipped
//! in libcompression since 10.11, so there's no availability fallback to make — the
//! only fallback path is the ephemeral cache when decmpfs can't be applied at all.
//!
//! Layout written (verified by the kernel-roundtrip test):
//!   xattr com.apple.decmpfs      = [magic u32 LE][type=8 LZVN u32 LE][rawSize u64 LE]
//!   xattr com.apple.ResourceFork = [(numBlocks+1) u32 LE offset table][LZVN blocks]
//! Built on a sibling temp (empty data fork + those xattrs + UF_COMPRESSED), then
//! atomically renamed over the original — never an in-place truncate, so a crash
//! can't leave a 0-byte file.

use std::os::fd::AsRawFd;
use std::path::Path;

use crate::{cstring, io, Error, Support, UnsupportedReason};

const UF_COMPRESSED: u32 = 0x0000_0020;
const DECMPFS_MAGIC: u32 = 0x636d_7066; // 'cmpf' (XNU sys/decmpfs.h); LE on disk = "fpmc"
                                        // Type 8 = LZVN-in-resource-fork — what `ditto --hfsCompression` writes and the
                                        // kernel reliably reads. The resource fork is a flat offset table (NOT the zlib
                                        // type-4 resource-fork-with-map format).
const CMP_LZVN_RESOURCE_FORK: u32 = 8;
const BLOCK: usize = 0x1_0000; // 64 KiB
const XATTR_NOFOLLOW: libc::c_int = 0x0001;
// decmpfs resource-fork offsets cap at u32; stay well under 4 GiB so the offsets
// and the kernel's own checks never overflow.
const MAX_RAW: u64 = 3_900_000_000;
const COMPRESSION_LZVN: i32 = 0x900;

#[link(name = "compression")]
extern "C" {
  fn compression_encode_buffer(
    dst_buffer: *mut u8,
    dst_size: usize,
    src_buffer: *const u8,
    src_size: usize,
    scratch_buffer: *mut u8,
    algorithm: i32,
  ) -> usize;
  fn compression_encode_scratch_buffer_size(algorithm: i32) -> usize;
}

/// Reject files past the decmpfs u32-offset ceiling (→ Skipped(TooLarge) via
/// safety::classify_skip on EFBIG). Pure, so the limit is testable without a 4 GiB
/// file.
fn within_decmpfs_limit(len: u64) -> Result<(), Error> {
  if len > MAX_RAW {
    return Err(Error::Io {
      context: "file too large for decmpfs",
      source: std::io::Error::from_raw_os_error(libc::EFBIG),
    });
  }
  Ok(())
}

fn statfs(path: &Path) -> Result<libc::statfs, Error> {
  let cpath = cstring(path)?;
  let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
  if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
    return Err(io("statfs"));
  }
  Ok(buf)
}

/// Local APFS or HFS+ only — the two filesystems with the decmpfs path. A network
/// or non-local mount reports Unsupported (the signal isn't ours to trust).
pub(crate) fn detect(path: &Path) -> Result<Support, Error> {
  let buf = statfs(path)?;
  // f_fstypename is a NUL-padded C string ("apfs", "hfs").
  let name: Vec<u8> = buf
    .f_fstypename
    .iter()
    .take_while(|&&c| c != 0)
    .map(|&c| c as u8)
    .collect();
  Ok(classify_fs(
    buf.f_flags & (libc::MNT_LOCAL as u32) != 0,
    &name,
  ))
}

/// The pure detect policy — split from the statfs syscall so the network/non-APFS
/// branches are unit-testable without a network mount or an exotic filesystem.
fn classify_fs(is_local: bool, fstype: &[u8]) -> Support {
  if !is_local {
    return Support::Unsupported(UnsupportedReason::NetworkOrOverlay);
  }
  if fstype == b"apfs" || fstype == b"hfs" {
    Support::Supported
  } else {
    Support::Unsupported(UnsupportedReason::Filesystem)
  }
}

fn st_flags(path: &Path) -> Result<u32, Error> {
  let cpath = cstring(path)?;
  let mut st: libc::stat = unsafe { std::mem::zeroed() };
  if unsafe { libc::lstat(cpath.as_ptr(), &mut st) } != 0 {
    return Err(io("lstat"));
  }
  Ok(st.st_flags)
}

pub(crate) fn is_already_compressed(path: &Path) -> Result<bool, Error> {
  Ok(st_flags(path)? & UF_COMPRESSED != 0)
}

/// On macOS, UF_COMPRESSED is the authoritative win signal (st_blocks also drops,
/// but the flag is unambiguous and what we set).
pub(crate) fn compressed_on_disk(path: &Path) -> Result<Option<bool>, Error> {
  Ok(Some(is_already_compressed(path)?))
}

/// LZVN-encode `src` into a kernel-decodable block. libcompression emits a valid
/// frame even for incompressible input (slightly larger than `src`), so every
/// block decodes the same way — there is no bare "stored" block. Returns `None`
/// only if encoding fails outright (treated as a hard error upstream).
fn compress_block(src: &[u8], scratch: &mut [u8]) -> Option<Vec<u8>> {
  // Headroom for the worst case (incompressible data expands a little).
  let mut dst = vec![0u8; src.len() + src.len() / 16 + 1024];
  let n = unsafe {
    compression_encode_buffer(
      dst.as_mut_ptr(),
      dst.len(),
      src.as_ptr(),
      src.len(),
      scratch.as_mut_ptr(),
      COMPRESSION_LZVN,
    )
  };
  if n == 0 {
    return None;
  }
  dst.truncate(n);
  Some(dst)
}

/// Build the com.apple.ResourceFork blob for `raw` in the LZVN/LZFSE decmpfs
/// layout (what `ditto` writes): `(numBlocks+1)` u32 LE offsets, then the blocks.
/// `offset[0]` = table size; `offset[i+1]` = end of block i; last = total size.
fn build_resource_fork(raw: &[u8]) -> Option<Vec<u8>> {
  let num_blocks = raw.len().div_ceil(BLOCK).max(1);
  let scratch_len = unsafe { compression_encode_scratch_buffer_size(COMPRESSION_LZVN) };

  // The 64 KiB LZVN blocks are independent and the encode IS the write cost, so
  // fan them across cores — each worker keeps its OWN scratch (the libcompression
  // scratch can't be shared concurrently). Contiguous byte regions (each a whole
  // number of blocks) mean per-worker outputs are already in block order, so
  // concatenation needs no re-sort. Stay serial for a handful of blocks, where
  // thread setup would cost more than it saves. No thread-pool dep — std scoped
  // threads keep the macOS stub dependency-free.
  let blocks: Vec<Vec<u8>> = {
    // DECMPFS_SERIAL forces the single-thread path — a deterministic escape hatch
    // (and the A/B baseline for the parallel win).
    let workers = if std::env::var_os("DECMPFS_SERIAL").is_some() {
      1
    } else {
      std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(num_blocks)
    };
    if workers <= 1 || num_blocks < 8 {
      let mut scratch = vec![0u8; scratch_len];
      raw
        .chunks(BLOCK)
        .map(|chunk| compress_block(chunk, &mut scratch))
        .collect::<Option<Vec<Vec<u8>>>>()?
    } else {
      let bytes_per_worker = num_blocks.div_ceil(workers) * BLOCK;
      let parts: Vec<Option<Vec<Vec<u8>>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = raw
          .chunks(bytes_per_worker)
          .map(|region| {
            scope.spawn(move || {
              let mut scratch = vec![0u8; scratch_len];
              region
                .chunks(BLOCK)
                .map(|chunk| compress_block(chunk, &mut scratch))
                .collect::<Option<Vec<Vec<u8>>>>()
            })
          })
          .collect();
        handles
          .into_iter()
          .map(|h| h.join().ok().flatten())
          .collect()
      });
      let mut out = Vec::with_capacity(num_blocks);
      for part in parts {
        out.extend(part?);
      }
      out
    }
  };

  let table_len = (num_blocks + 1) * 4;
  let mut out = Vec::with_capacity(table_len + blocks.iter().map(Vec::len).sum::<usize>());
  // Offset table: numBlocks+1 entries. offset[i] is where block i starts.
  let mut offset = table_len as u32;
  out.extend_from_slice(&offset.to_le_bytes());
  for block in &blocks {
    offset += block.len() as u32;
    out.extend_from_slice(&offset.to_le_bytes());
  }
  for block in &blocks {
    out.extend_from_slice(block);
  }
  Some(out)
}

fn setxattr(path: &std::ffi::CStr, name: &std::ffi::CStr, value: &[u8]) -> Result<(), Error> {
  let rc = unsafe {
    libc::setxattr(
      path.as_ptr(),
      name.as_ptr(),
      value.as_ptr().cast(),
      value.len(),
      0,
      XATTR_NOFOLLOW,
    )
  };
  if rc != 0 {
    return Err(io("setxattr"));
  }
  Ok(())
}

pub(crate) fn apply_inplace(path: &Path) -> Result<(), Error> {
  let raw = std::fs::read(path).map_err(|source| Error::Io {
    context: "read",
    source,
  })?;

  // Fail-soft: skip if we can't write the original (by mode or ownership) — the
  // temp+rename below would otherwise replace even a file we can't open for write.
  let cpath = cstring(path)?;
  if unsafe { libc::access(cpath.as_ptr(), libc::W_OK) } != 0 {
    return Err(io("access"));
  }

  let mode = std::fs::metadata(path).map(|m| m.permissions()).ok();
  apply_bytes(path, &raw, mode)
}

/// Write `content` to `path` as a fresh decmpfs-compressed file in ONE pass — no
/// write-then-read-back. The decmpfs is built directly from `content`, dropped on a
/// sibling temp (empty data fork + the two xattrs + UF_COMPRESSED), then atomically
/// renamed over `path`. A crash can only leave the original or the finished file;
/// the rename also gives a fresh inode, the copy-break from any pnpm CAS hardlink
/// siblings. This is the one-pass core both `compress_bytes` (no original) and
/// `apply_inplace` (read first) share.
pub(crate) fn apply_bytes(
  path: &Path,
  content: &[u8],
  mode: Option<std::fs::Permissions>,
) -> Result<(), Error> {
  within_decmpfs_limit(content.len() as u64)?;

  let mut header = Vec::with_capacity(16);
  header.extend_from_slice(&DECMPFS_MAGIC.to_le_bytes());
  header.extend_from_slice(&CMP_LZVN_RESOURCE_FORK.to_le_bytes());
  header.extend_from_slice(&(content.len() as u64).to_le_bytes());
  let resource_fork = build_resource_fork(content).ok_or_else(|| Error::Io {
    context: "lzvn encode",
    source: std::io::Error::from(std::io::ErrorKind::InvalidData),
  })?;

  let dir = path.parent().ok_or_else(|| io("parent"))?;
  let name = path
    .file_name()
    .ok_or_else(|| io("file_name"))?
    .to_string_lossy();
  let tmp = dir.join(format!(".{name}.decmpfs-{}.tmp", std::process::id()));

  let build = (|| -> Result<(), Error> {
    let file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create_new(true)
      .open(&tmp)
      .map_err(|source| Error::Io {
        context: "create temp",
        source,
      })?;
    let ctmp = cstring(&tmp)?;
    setxattr(&ctmp, c"com.apple.decmpfs", &header)?;
    setxattr(&ctmp, c"com.apple.ResourceFork", &resource_fork)?;
    if unsafe { libc::fchflags(file.as_raw_fd(), UF_COMPRESSED) } != 0 {
      return Err(io("fchflags"));
    }
    Ok(())
  })();

  if let Err(e) = build {
    let _ = std::fs::remove_file(&tmp);
    return Err(e);
  }
  if let Some(perm) = mode {
    let _ = std::fs::set_permissions(&tmp, perm);
  }
  std::fs::rename(&tmp, path).map_err(|source| {
    let _ = std::fs::remove_file(&tmp);
    Error::Io {
      context: "rename",
      source,
    }
  })
}

/// Copy-on-write clone via `clonefile(2)` — shares the extents AND the decmpfs
/// state, so a compressed source stays compressed at zero cost. `Ok(false)`
/// means "cannot clone here" (cross-volume, unsupported FS, …) and the caller
/// falls back to a byte copy; a failed clonefile never leaves a partial
/// destination.
pub(crate) fn clone_file(src: &Path, dest: &Path) -> Result<bool, Error> {
  let csrc = cstring(src)?;
  let cdest = cstring(dest)?;
  Ok(unsafe { libc::clonefile(csrc.as_ptr(), cdest.as_ptr(), 0) } == 0)
}

#[cfg(test)]
mod tests {
  use super::*;

  // The kernel-roundtrip oracle. decmpfs is undocumented — the only proof the
  // format is right is that a normal read() returns identical bytes after apply.
  #[test]
  fn kernel_roundtrips_decmpfs() {
    let dir = std::env::temp_dir().join(format!("decmpfs-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.bin");
    // > 1 block (64 KiB) of compressible data, so the offset table + LZVN blocks
    // are both exercised.
    let mut raw = Vec::new();
    let pat = b"the quick brown fox decmpfs lzvn resource-fork oracle line ";
    while raw.len() < 2_000_000 {
      raw.extend_from_slice(pat);
    }
    std::fs::write(&path, &raw).unwrap();

    assert!(
      matches!(detect(&path).unwrap(), Support::Supported),
      "temp dir is local APFS/HFS+"
    );
    apply_inplace(&path).unwrap();
    assert!(is_already_compressed(&path).unwrap(), "UF_COMPRESSED set");
    assert_eq!(
      compressed_on_disk(&path).unwrap(),
      Some(true),
      "reports compressed"
    );
    // THE ORACLE: the kernel decompresses our resource fork on read().
    assert_eq!(
      std::fs::read(&path).unwrap(),
      raw,
      "kernel read-back must equal the original bytes"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  // Opt-in perf probe (ignored in CI — timing is machine-specific). Reports the
  // decmpfs write time for a ~40 MiB addon; run serial vs parallel with
  //   cargo test -p decmpfs write_time -- --ignored --nocapture
  //   DECMPFS_SERIAL=1 cargo test -p decmpfs write_time -- --ignored --nocapture
  #[test]
  #[ignore]
  fn write_time_probe() {
    let dir = std::env::temp_dir().join(format!("decmpfs-time-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("addon.node");
    let mut raw: Vec<u8> = Vec::with_capacity(40 << 20);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    while raw.len() < (40 << 20) {
      x ^= x << 13;
      x ^= x >> 7;
      x ^= x << 17;
      raw.extend_from_slice(&x.to_le_bytes());
      raw.extend_from_slice(b"native addon .node text segment padding ");
    }
    if !matches!(detect(&dir), Ok(Support::Supported)) {
      std::fs::remove_dir_all(&dir).ok();
      return;
    }
    let cores = std::thread::available_parallelism()
      .map(|n| n.get())
      .unwrap_or(1);
    let serial = std::env::var_os("DECMPFS_SERIAL").is_some();
    let start = std::time::Instant::now();
    apply_bytes(&path, &raw, None).unwrap();
    let ms = start.elapsed().as_secs_f64() * 1e3;
    eprintln!(
      "decmpfs write {}MiB — {} ({} cores): {:.1} ms",
      raw.len() >> 20,
      if serial { "serial" } else { "parallel" },
      cores,
      ms,
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn detect_and_flags_error_on_a_missing_path() {
    let p = std::path::Path::new("/no/such/decmpfs/path/x.bin");
    assert!(detect(p).is_err(), "statfs of a missing path errors");
    assert!(
      is_already_compressed(p).is_err(),
      "lstat of a missing path errors"
    );
  }

  #[test]
  fn apply_inplace_errors_when_the_file_cannot_be_read() {
    // A 0-perm file: apply_inplace's initial read fails before any apply. Root
    // bypasses mode bits, so skip there.
    if unsafe { libc::geteuid() } == 0 {
      return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("decmpfs-noread-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.bin");
    std::fs::write(&path, b"\x7fELF unreadable").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = apply_inplace(&path);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
    assert!(matches!(
      out,
      Err(Error::Io {
        context: "read",
        ..
      })
    ));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn setxattr_errors_on_a_missing_path() {
    let out = setxattr(c"/no/such/decmpfs/path", c"com.apple.decmpfs", b"x");
    assert!(matches!(
      out,
      Err(Error::Io {
        context: "setxattr",
        ..
      })
    ));
  }

  #[test]
  fn compress_block_returns_none_for_empty_input() {
    // libcompression encodes zero bytes to nothing → the n == 0 guard returns None.
    let scratch_len = unsafe { compression_encode_scratch_buffer_size(COMPRESSION_LZVN) };
    let mut scratch = vec![0u8; scratch_len];
    assert!(compress_block(b"", &mut scratch).is_none());
  }

  #[test]
  fn cstring_rejects_an_interior_nul() {
    use std::os::unix::ffi::OsStrExt;
    let p = std::path::Path::new(std::ffi::OsStr::from_bytes(b"a\0b"));
    assert!(cstring(p).is_err());
  }

  #[test]
  fn detect_rejects_a_non_apfs_filesystem() {
    // /dev is devfs (local, but not apfs/hfs) → Unsupported(Filesystem).
    assert!(matches!(
      detect(std::path::Path::new("/dev")),
      Ok(Support::Unsupported(UnsupportedReason::Filesystem))
    ));
  }

  #[test]
  fn classify_fs_covers_every_branch() {
    // Non-local (e.g. a network mount) — no real mount needed.
    assert!(matches!(
      classify_fs(false, b"nfs"),
      Support::Unsupported(UnsupportedReason::NetworkOrOverlay)
    ));
    assert!(matches!(classify_fs(true, b"apfs"), Support::Supported));
    assert!(matches!(classify_fs(true, b"hfs"), Support::Supported));
    assert!(matches!(
      classify_fs(true, b"ext4"),
      Support::Unsupported(UnsupportedReason::Filesystem)
    ));
  }

  #[test]
  fn within_decmpfs_limit_rejects_oversized() {
    // No 4 GiB file needed — the predicate is pure.
    assert!(within_decmpfs_limit(MAX_RAW).is_ok());
    match within_decmpfs_limit(MAX_RAW + 1).unwrap_err() {
      Error::Io { source, .. } => assert_eq!(source.raw_os_error(), Some(libc::EFBIG)),
      other => panic!("expected EFBIG Io, got {other:?}"),
    }
  }

  // Incompressible data → blocks are stored verbatim (compress_block returns the
  // chunk). The kernel must still read them back identically.
  #[test]
  fn kernel_roundtrips_incompressible_blocks() {
    let dir = std::env::temp_dir().join(format!("decmpfs-raw-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.bin");
    let mut raw = Vec::new();
    let mut x: u32 = 0x9e37_79b9;
    while raw.len() < 200_000 {
      x ^= x << 13;
      x ^= x >> 17;
      x ^= x << 5;
      raw.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(&path, &raw).unwrap();
    if matches!(detect(&path).unwrap(), Support::Supported) {
      apply_inplace(&path).unwrap();
      assert_eq!(
        std::fs::read(&path).unwrap(),
        raw,
        "verbatim blocks read back"
      );
    }
    std::fs::remove_dir_all(&dir).ok();
  }
}
