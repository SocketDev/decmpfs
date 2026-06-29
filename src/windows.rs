//! Windows backend — NTFS LZNT1 via `FSCTL_SET_COMPRESSION`.
//!
//! Codec choice is LZNT1, not WOF XPRESS/LZX, even though WOF-XPRESS decodes faster
//! and packs tighter. WOF is "write-once": opening the file for write strips the
//! compression back to plain, so every package-manager **reinstall** that rewrites
//! the `.node` would silently lose it — exactly the workload this targets. LZNT1
//! compresses the existing file IN PLACE (no copy), survives open-for-write, and
//! hardlink siblings share the file record so they compress together (same content;
//! acceptable). For a load-once addon the LZNT1-vs-XPRESS decode delta is marginal,
//! so reinstall-survival wins. Reversal condition: if a consumer re-applies
//! compression on every install (so write-strip is harmless), switch to WOF-XPRESS
//! (`FSCTL_SET_EXTERNAL_BACKING`) for the better ratio + faster decode.
//!
//! Detection gates on the volume's per-file-compression capability, which ReFS/FAT
//! lack.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use std::io::Write;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
  CreateFileW, GetFileAttributesW, GetVolumeInformationByHandleW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::{io, Error, Support, UnsupportedReason};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const OPEN_EXISTING: u32 = 3;
const CREATE_ALWAYS: u32 = 2;
const FSCTL_SET_COMPRESSION: u32 = 0x0009_C040;
const COMPRESSION_FORMAT_DEFAULT: u16 = 1; // LZNT1
const FILE_FILE_COMPRESSION: u32 = 0x0000_0010; // volume supports per-file compression
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x0000_0800;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

fn wide(path: &Path) -> Vec<u16> {
  path
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .collect()
}

/// Owning wrapper so the handle is always closed, even on the error paths.
struct Handle(HANDLE);

impl Drop for Handle {
  fn drop(&mut self) {
    unsafe {
      CloseHandle(self.0);
    }
  }
}

fn open(path: &Path, access: u32) -> Result<Handle, Error> {
  open_with(path, access, OPEN_EXISTING)
}

fn open_with(path: &Path, access: u32, disposition: u32) -> Result<Handle, Error> {
  let wpath = wide(path);
  let handle = unsafe {
    CreateFileW(
      wpath.as_ptr(),
      access,
      FILE_SHARE_READ,
      std::ptr::null(),
      disposition,
      0,
      std::ptr::null_mut(),
    )
  };
  if handle == INVALID_HANDLE_VALUE || handle.is_null() {
    return Err(io("CreateFileW"));
  }
  Ok(Handle(handle))
}

/// Set LZNT1 compression on an open handle (the empty fresh file or an existing
/// one). Shared by `apply_inplace` and `apply_bytes`.
fn set_compression(handle: HANDLE) -> Result<(), Error> {
  let format: u16 = COMPRESSION_FORMAT_DEFAULT;
  let mut returned: u32 = 0;
  let ok = unsafe {
    DeviceIoControl(
      handle,
      FSCTL_SET_COMPRESSION,
      (&format as *const u16).cast(),
      std::mem::size_of::<u16>() as u32,
      std::ptr::null_mut(),
      0,
      &mut returned,
      std::ptr::null_mut(),
    )
  };
  if ok == 0 {
    return Err(io("FSCTL_SET_COMPRESSION"));
  }
  Ok(())
}

pub(crate) fn detect(path: &Path) -> Result<Support, Error> {
  let handle = open(path, GENERIC_READ)?;
  let mut flags: u32 = 0;
  let ok = unsafe {
    GetVolumeInformationByHandleW(
      handle.0,
      std::ptr::null_mut(),
      0,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      &mut flags,
      std::ptr::null_mut(),
      0,
    )
  };
  if ok == 0 {
    return Err(io("GetVolumeInformationByHandleW"));
  }
  if flags & FILE_FILE_COMPRESSION != 0 {
    Ok(Support::Supported)
  } else {
    Ok(Support::Unsupported(UnsupportedReason::Filesystem))
  }
}

pub(crate) fn is_already_compressed(path: &Path) -> Result<bool, Error> {
  let attrs = unsafe { GetFileAttributesW(wide(path).as_ptr()) };
  if attrs == INVALID_FILE_ATTRIBUTES {
    return Err(io("GetFileAttributesW"));
  }
  Ok(attrs & FILE_ATTRIBUTE_COMPRESSED != 0)
}

pub(crate) fn apply_inplace(path: &Path) -> Result<(), Error> {
  let handle = open(path, GENERIC_READ | GENERIC_WRITE)?;
  set_compression(handle.0)
}

/// Write `content` to `path` as a fresh NTFS-compressed file in ONE pass: create
/// the file, FSCTL_SET_COMPRESSION on the EMPTY handle (so writes compress on the
/// way in — never a write-then-recompress), then write the bytes through the same
/// handle. `mode` is unused on Windows (no POSIX bits). Shared by `compress_bytes`.
pub(crate) fn apply_bytes(
  path: &Path,
  content: &[u8],
  _mode: Option<std::fs::Permissions>,
) -> Result<(), Error> {
  let handle = open_with(path, GENERIC_READ | GENERIC_WRITE, CREATE_ALWAYS)?;
  set_compression(handle.0)?;
  // Write through the same handle the compression flag was set on, so every block
  // lands compressed. WriteFile via a std File would need a second open; instead
  // borrow the raw handle into a File for the write, then forget it so Drop on the
  // Handle (not the File) does the single CloseHandle.
  use std::os::windows::io::FromRawHandle;
  let mut file = unsafe { std::fs::File::from_raw_handle(handle.0 as _) };
  let res = file.write_all(content).and_then(|()| file.flush());
  // Don't let File's Drop close the handle — the Handle wrapper owns it.
  std::mem::forget(file);
  res.map_err(|source| Error::Io {
    context: "write",
    source,
  })
}

/// No FS-specific on-disk signal — apply_guarded falls back to the generic
/// allocated-bytes measurement (st_blocks / GetCompressedFileSizeW), which DOES
/// reflect the win on APFS and NTFS.
pub(crate) fn compressed_on_disk(_path: &Path) -> Result<Option<bool>, Error> {
  Ok(None)
}
