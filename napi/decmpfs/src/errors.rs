use super::*;

// ── Node-shaped fs errors ────────────────────────────────────────────────────
// napi-rs maps a returned error to `error.code = <status>` only; to be drop-in
// for Node's fs we build the JS Error ourselves with { code, errno, syscall,
// path } and a Node-format message ("ENOENT: no such file or directory, rm
// '/x'"), then throw it — matching fs.rm / fs.rmSync.

/// The parts of a Node-shaped fs error, carried so an async `Task` (whose
/// `compute` has no `Env`) can rebuild the JS error in `reject`.
pub(crate) struct FsErr {
  pub(crate) code: &'static str,
  pub(crate) errno: i32,
  pub(crate) detail: String,
  pub(crate) syscall: &'static str,
  pub(crate) path: String,
}

/// A binding failure: a Node fs-shaped error, or an argument-validation error (an
/// invalid gate). Both are realized against an `Env` at the FFI boundary.
pub(crate) enum NapiFail {
  Fs(FsErr),
  Arg(String),
}

impl NapiFail {
  pub(crate) fn into_error(self, env: &Env) -> Error {
    match self {
      NapiFail::Fs(fe) => build_fs_error(env, &fe),
      NapiFail::Arg(msg) => Error::new(Status::InvalidArg, msg),
    }
  }
}

// POSIX errno → Node `code`. Shared across macOS + Linux, except ENOTEMPTY
// (macOS 66, Linux 39).
#[cfg(not(windows))]
pub(crate) fn errno_name(raw: i32) -> &'static str {
  match raw {
    1 => "EPERM",
    2 => "ENOENT",
    13 => "EACCES",
    16 => "EBUSY",
    17 => "EEXIST",
    20 => "ENOTDIR",
    21 => "EISDIR",
    23 => "ENFILE",
    24 => "EMFILE",
    30 => "EROFS",
    #[cfg(target_os = "macos")]
    66 => "ENOTEMPTY",
    #[cfg(not(target_os = "macos"))]
    39 => "ENOTEMPTY",
    _ => "UNKNOWN",
  }
}

// (code, errno) for a positive OS error number, per platform. Windows
// `raw_os_error()` is the Win32 GetLastError space, which does NOT coincide with
// POSIX — map it to the Node `code` a caller checks (`err.code === 'EBUSY'`) with
// the negated POSIX-equivalent errno for consistency.
#[cfg(not(windows))]
pub(crate) fn os_errno(raw: i32) -> (&'static str, i32) {
  (errno_name(raw), -raw)
}
#[cfg(windows)]
pub(crate) fn os_errno(raw: i32) -> (&'static str, i32) {
  match raw {
    2 | 3 => ("ENOENT", -2),     // FILE_NOT_FOUND / PATH_NOT_FOUND
    5 => ("EACCES", -13),        // ACCESS_DENIED
    19 => ("EROFS", -30),        // WRITE_PROTECT
    32 | 33 => ("EBUSY", -16),   // SHARING_VIOLATION / LOCK_VIOLATION
    80 | 183 => ("EEXIST", -17), // FILE_EXISTS / ALREADY_EXISTS
    145 => ("ENOTEMPTY", -39),   // DIR_NOT_EMPTY
    _ => ("UNKNOWN", -raw),
  }
}

// uv-style lowercase strerror ("no such file or directory"), stripped of the
// Rust "(os error N)" suffix.
pub(crate) fn os_strerror(raw: i32) -> String {
  let full = std::io::Error::from_raw_os_error(raw).to_string();
  full
    .split(" (os error")
    .next()
    .unwrap_or(&full)
    .to_lowercase()
}

// (code, errno, detail) for a raw io::Error. An error with no OS errno was built
// from an ErrorKind; map the common kinds, and for anything unmapped keep the
// error's OWN message as the detail rather than rendering "undefined error: 0".
pub(crate) fn io_parts(err: &std::io::Error) -> (&'static str, i32, String) {
  match err.raw_os_error() {
    Some(raw) if raw > 0 => {
      let (code, errno) = os_errno(raw);
      (code, errno, os_strerror(raw))
    }
    _ => match err.kind() {
      std::io::ErrorKind::PermissionDenied => ("EACCES", -13, "permission denied".to_string()),
      std::io::ErrorKind::NotFound => ("ENOENT", -2, "no such file or directory".to_string()),
      std::io::ErrorKind::AlreadyExists => ("EEXIST", -17, "file exists".to_string()),
      _ => ("UNKNOWN", 0, err.to_string()),
    },
  }
}

// (code, errno, detail) for a decmpfs error — NotFound is ENOENT; an Io defers to
// io_parts, but when the source has no OS errno the decmpfs Error's own Display
// (context + cause) is the detail, so the known cause is never discarded.
pub(crate) fn fs_parts(err: &decmpfs::Error) -> (&'static str, i32, String) {
  match err {
    decmpfs::Error::NotFound(_) => ("ENOENT", -2, "no such file or directory".to_string()),
    decmpfs::Error::Io { source, .. } => {
      let (code, errno, detail) = io_parts(source);
      if errno == 0 {
        (code, errno, err.to_string())
      } else {
        (code, errno, detail)
      }
    }
  }
}

pub(crate) fn fs_err_decmpfs(err: &decmpfs::Error, syscall: &'static str, path: &str) -> FsErr {
  let (code, errno, detail) = fs_parts(err);
  FsErr {
    code,
    errno,
    detail,
    syscall,
    path: path.to_string(),
  }
}

pub(crate) fn fs_err_io(err: &std::io::Error, syscall: &'static str, path: &str) -> FsErr {
  let (code, errno, detail) = io_parts(err);
  FsErr {
    code,
    errno,
    detail,
    syscall,
    path: path.to_string(),
  }
}

// Build a Node-shaped fs Error with { code, errno, syscall, path } + a Node-format
// message. Returned through the Err channel — napi THROWS it for a sync fn and
// REJECTS the promise with it for an async Task, so both deliver the same error.
// (env.throw would fire OUTSIDE the promise on the async path → uncaught.)
pub(crate) fn build_fs_error(env: &Env, fe: &FsErr) -> Error {
  let message = format!("{}: {}, {} '{}'", fe.code, fe.detail, fe.syscall, fe.path);
  match env.create_error(Error::new(Status::GenericFailure, message)) {
    Ok(mut obj) => {
      let _ = obj.set_named_property("code", fe.code);
      let _ = obj.set_named_property("errno", fe.errno);
      let _ = obj.set_named_property("syscall", fe.syscall);
      let _ = obj.set_named_property("path", fe.path.as_str());
      Error::from(obj.to_unknown())
    }
    Err(e) => e,
  }
}

pub(crate) fn throw_decmpfs(
  env: &Env,
  err: &decmpfs::Error,
  syscall: &'static str,
  path: &str,
) -> Error {
  build_fs_error(env, &fs_err_decmpfs(err, syscall, path))
}
