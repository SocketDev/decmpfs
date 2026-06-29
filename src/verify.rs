//! Platform-agnostic measurement + loadability surface.

use std::io::Read;
use std::path::Path;

use crate::Error;

/// On-disk *allocated* bytes — `st_blocks * 512` on POSIX, `GetCompressedFileSizeW`
/// on Windows. Never `st_size`: transparent compression holds the logical size
/// constant, so only the allocation reveals the win.
pub(crate) fn on_disk_bytes(path: &Path) -> Result<u64, Error> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(|source| Error::Io {
      context: "stat",
      source,
    })?;
    Ok(meta.blocks().saturating_mul(512))
  }
  #[cfg(windows)]
  {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    let wide: Vec<u16> = path
      .as_os_str()
      .encode_wide()
      .chain(std::iter::once(0))
      .collect();
    let mut high: u32 = 0;
    // Returns the actual allocated size (post-NTFS-compression), low dword as the
    // return value + high dword via the out-param. INVALID_FILE_SIZE (u32::MAX) is
    // only an error if GetLastError is non-zero (it can also be a legit low dword).
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == u32::MAX {
      let err = std::io::Error::last_os_error();
      if err.raw_os_error().unwrap_or(0) != 0 {
        return Err(Error::Io {
          context: "GetCompressedFileSizeW",
          source: err,
        });
      }
    }
    Ok(((high as u64) << 32) | low as u64)
  }
  #[cfg(not(any(unix, windows)))]
  {
    let meta = std::fs::metadata(path).map_err(|source| Error::Io {
      context: "stat",
      source,
    })?;
    Ok(meta.len())
  }
}

/// First 4 bytes — the native-binary magic (ELF `7f454c46`, Mach-O `cffaedfe`/…,
/// PE `4d5a`). Compared before/after apply to assert the file still loads, in
/// place of a content hash that transparent compression would render vacuous.
pub(crate) fn magic_prefix(path: &Path) -> Result<[u8; 4], Error> {
  let mut file = std::fs::File::open(path).map_err(|source| Error::Io {
    context: "open",
    source,
  })?;
  let mut buf = [0u8; 4];
  file.read(&mut buf).map_err(|source| Error::Io {
    context: "read",
    source,
  })?;
  Ok(buf)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn measures_allocation_and_reads_magic() {
    let dir = std::env::temp_dir().join(format!("decmpfs-verify-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f");
    std::fs::write(&path, vec![0x7f; 9000]).unwrap();
    assert!(
      on_disk_bytes(&path).unwrap() > 0,
      "allocated bytes reported"
    );
    assert_eq!(magic_prefix(&path).unwrap(), [0x7f; 4]);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn errors_on_a_missing_path() {
    let p = std::path::Path::new("/no/such/verify/x");
    assert!(on_disk_bytes(p).is_err());
    assert!(magic_prefix(p).is_err());
  }

  // Opening a directory succeeds on unix, but read() on its fd fails (EISDIR) —
  // exercising the read-error arm distinct from the open-error arm above.
  #[cfg(unix)]
  #[test]
  fn magic_prefix_errors_when_the_read_fails_after_a_successful_open() {
    let dir = std::env::temp_dir().join(format!("decmpfs-readfail-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(magic_prefix(&dir).is_err(), "read of a directory fd errors");
    std::fs::remove_dir_all(&dir).ok();
  }
}
