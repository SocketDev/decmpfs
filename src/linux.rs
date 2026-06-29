//! Linux backend — btrfs (Stage 1); bcachefs later. ext4/xfs/ZFS cannot be done.
//!
//! Detection is an allowlist: only btrfs carries the per-file `FS_COMPR_FL` we set,
//! so every other f_type (incl. overlay/NFS) reports Unsupported. Apply uses the
//! SYNCHRONOUS copy+flag path (create a sibling, request the codec BEFORE writing
//! so the kernel compresses the bytes as they land, then atomic-rename over the
//! target) — NOT `BTRFS_IOC_DEFRAG_RANGE`, whose async completion races a
//! post-apply `st_blocks` check. The rename also gives a fresh inode: the
//! copy-break that isolates any pnpm CAS hardlink siblings.
//!
//! Codec preference is zstd → lzo → zlib (speed-over-ratio: zstd decodes faster
//! than zlib and tunes ratio, lzo is the fastest fallback). We request it the way
//! `btrfs property set <file> compression <algo>` does — a write to the
//! `btrfs.compression` xattr — and walk the list because an old kernel rejects an
//! unsupported algo (zstd pre-4.14, lzo pre-3.14) with EINVAL. If none take we
//! fall back to the bare `FS_COMPR_FL` flag (the mount's default algorithm).

use std::os::fd::AsRawFd;
use std::path::Path;

use crate::{cstring, io, Error, Support, UnsupportedReason};

// Allowlist magic (statfs f_type). Only btrfs from the Stage-1 matrix.
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

// FS_IOC_{GET,SET}FLAGS are _IOR/_IOW('f', 1|2, long). The request value is a u32;
// `as _` at the call site casts it to whatever `libc::ioctl` expects per target
// (c_ulong on glibc, c_int on musl) — same 32 bits either way.
const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
// FS_IOC_FIEMAP = _IOWR('f', 11, struct fiemap). Used to read whether the data
// extents are ENCODED (compressed) — btrfs reports st_blocks as the LOGICAL size,
// so the on-disk win is invisible to stat/du and only FIEMAP/compsize reveals it.
const FS_IOC_FIEMAP: u32 = 0xC020_660B;
const FIEMAP_FLAG_SYNC: u32 = 0x0001;
const FIEMAP_EXTENT_ENCODED: u32 = 0x0008;
// uapi/linux/fs.h: FS_COMPR_FL is 0x4 (0x20 is FS_APPEND_FL — append-only, which
// makes writes and renames EPERM).
const FS_COMPR_FL: libc::c_long = 0x0000_0004;

fn statfs_type(path: &Path) -> Result<i64, Error> {
  let cpath = cstring(path)?;
  let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
  if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
    return Err(io("statfs"));
  }
  Ok(buf.f_type as i64)
}

fn get_flags(fd: libc::c_int) -> Result<libc::c_long, Error> {
  let mut flags: libc::c_long = 0;
  if unsafe { libc::ioctl(fd, FS_IOC_GETFLAGS as _, &mut flags) } != 0 {
    return Err(io("FS_IOC_GETFLAGS"));
  }
  Ok(flags)
}

// Per-file codecs in preference order (speed-over-ratio). zstd: fast decode, good
// ratio, tunable. lzo: fastest decode, weakest ratio. zlib: legacy fallback.
const PREFERRED_ALGOS: [&[u8]; 3] = [b"zstd", b"lzo", b"zlib"];

// Request a codec the way `btrfs property set <file> compression <algo>` does — by
// writing the `btrfs.compression` xattr — trying most-preferred first. A kernel
// without a given algo rejects it (EINVAL), so we walk the list; if none take we
// fall back to the bare FS_COMPR_FL flag (the mount's default codec). Called on the
// empty temp file so the bytes compress as they're written.
fn request_codec(fd: libc::c_int) -> Result<(), Error> {
  for algo in PREFERRED_ALGOS {
    let rc = unsafe {
      libc::fsetxattr(
        fd,
        c"btrfs.compression".as_ptr(),
        algo.as_ptr().cast(),
        algo.len(),
        0,
      )
    };
    if rc == 0 {
      return Ok(());
    }
  }
  let flags = get_flags(fd)? | FS_COMPR_FL;
  if unsafe { libc::ioctl(fd, FS_IOC_SETFLAGS as _, &flags) } != 0 {
    return Err(io("FS_IOC_SETFLAGS"));
  }
  Ok(())
}

pub(crate) fn detect(path: &Path) -> Result<Support, Error> {
  if statfs_type(path)? == BTRFS_SUPER_MAGIC {
    Ok(Support::Supported)
  } else {
    Ok(Support::Unsupported(UnsupportedReason::Filesystem))
  }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FiemapExtent {
  fe_logical: u64,
  fe_physical: u64,
  fe_length: u64,
  fe_reserved64: [u64; 2],
  fe_flags: u32,
  fe_reserved: [u32; 3],
}

#[repr(C)]
struct FiemapHeader {
  fm_start: u64,
  fm_length: u64,
  fm_flags: u32,
  fm_mapped_extents: u32,
  fm_extent_count: u32,
  fm_reserved: u32,
}

const _: () = assert!(std::mem::size_of::<FiemapHeader>() == 32);
const _: () = assert!(std::mem::size_of::<FiemapExtent>() == 56);

/// True if any data extent carries FIEMAP_EXTENT_ENCODED (i.e. is compressed on
/// disk). This is the only reliable btrfs win signal — st_blocks reflects the
/// logical size. FIEMAP_FLAG_SYNC flushes pending writes before mapping.
fn compressed_via_fiemap(path: &Path) -> Result<bool, Error> {
  const COUNT: usize = 64;
  let file = std::fs::File::open(path).map_err(|source| Error::Io {
    context: "open for fiemap",
    source,
  })?;
  let mut buf =
    vec![0u8; std::mem::size_of::<FiemapHeader>() + COUNT * std::mem::size_of::<FiemapExtent>()];
  // SAFETY: buf is sized for the header + COUNT extents; the kernel fills it.
  unsafe {
    let header = buf.as_mut_ptr().cast::<FiemapHeader>();
    (*header).fm_start = 0;
    (*header).fm_length = u64::MAX;
    (*header).fm_flags = FIEMAP_FLAG_SYNC;
    (*header).fm_extent_count = COUNT as u32;
    (*header).fm_mapped_extents = 0;
    (*header).fm_reserved = 0;
    if libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, buf.as_mut_ptr()) != 0 {
      return Err(io("FS_IOC_FIEMAP"));
    }
    let mapped = (*header).fm_mapped_extents as usize;
    let extents = std::slice::from_raw_parts(
      buf
        .as_ptr()
        .add(std::mem::size_of::<FiemapHeader>())
        .cast::<FiemapExtent>(),
      mapped.min(COUNT),
    );
    Ok(
      extents
        .iter()
        .any(|e| e.fe_flags & FIEMAP_EXTENT_ENCODED != 0),
    )
  }
}

/// btrfs win measurement is authoritative via FIEMAP, not st_blocks.
pub(crate) fn compressed_on_disk(path: &Path) -> Result<Option<bool>, Error> {
  Ok(Some(compressed_via_fiemap(path)?))
}

pub(crate) fn is_already_compressed(path: &Path) -> Result<bool, Error> {
  let file = std::fs::File::open(path).map_err(|source| Error::Io {
    context: "open",
    source,
  })?;
  Ok(get_flags(file.as_raw_fd())? & FS_COMPR_FL != 0)
}

pub(crate) fn apply_inplace(path: &Path) -> Result<(), Error> {
  let data = std::fs::read(path).map_err(|source| Error::Io {
    context: "read",
    source,
  })?;
  let mode = std::fs::metadata(path).map(|m| m.permissions()).ok();
  apply_bytes(path, &data, mode)
}

/// Write `content` to `path` as a fresh btrfs-compressed file in ONE pass: create
/// the sibling temp, request the codec on the EMPTY file (so the bytes compress as
/// they land — never a write-then-recompress), write, fsync, then atomic-rename
/// over `path`. The rename gives a fresh inode, the copy-break from any pnpm CAS
/// hardlink siblings. Shared by `compress_bytes` (no original) and `apply_inplace`.
pub(crate) fn apply_bytes(
  path: &Path,
  content: &[u8],
  mode: Option<std::fs::Permissions>,
) -> Result<(), Error> {
  use std::io::Write;

  let dir = path.parent().ok_or_else(|| Error::Io {
    context: "no parent dir",
    source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
  })?;
  let name = path.file_name().map_or_else(
    || std::borrow::Cow::Borrowed("addon"),
    |n| n.to_string_lossy(),
  );
  let tmp = dir.join(format!(".{name}.decmpfs-{}.tmp", std::process::id()));

  let result = (|| {
    let mut file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(true)
      .open(&tmp)
      .map_err(|source| Error::Io {
        context: "create temp",
        source,
      })?;
    let fd = file.as_raw_fd();
    // Request the codec on the empty file FIRST so writes compress on the way in.
    request_codec(fd)?;
    file.write_all(content).map_err(|source| Error::Io {
      context: "write temp",
      source,
    })?;
    file.sync_all().map_err(|source| Error::Io {
      context: "fsync temp",
      source,
    })?;
    Ok(())
  })();

  if result.is_ok() {
    if let Some(perm) = mode {
      let _ = std::fs::set_permissions(&tmp, perm);
    }
  }

  match result {
    Ok(()) => std::fs::rename(&tmp, path).map_err(|source| {
      let _ = std::fs::remove_file(&tmp);
      Error::Io {
        context: "rename",
        source,
      }
    }),
    Err(err) => {
      let _ = std::fs::remove_file(&tmp);
      Err(err)
    }
  }
}
