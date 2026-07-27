use super::*;

/// `fs.copyFile` mode flags — values match Node's `fs.constants`.
#[napi]
pub const COPYFILE_EXCL: u32 = 1;
#[napi]
pub const COPYFILE_FICLONE: u32 = 2;
#[napi]
pub const COPYFILE_FICLONE_FORCE: u32 = 4;

/// Options for [`copyDecmpfsFile`] / [`copyDecmpfsFileSync`]. All optional.
#[napi(object)]
pub struct CopyDecmpfsOptions {
  /// Replace an existing file at `dest`. Default `true` (like `fs.cp`).
  pub force: Option<bool>,
  /// With `force: false`, reject (throw) if `dest` already exists. Default `false`.
  pub error_on_exist: Option<bool>,
}

/// Allocated on-disk bytes for `path` (falls back to the logical size where
/// the platform has no block count).
pub(crate) fn allocated(_path: &Path, logical: usize) -> i64 {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if let Ok(meta) = std::fs::metadata(_path) {
      return (meta.blocks() * 512) as i64;
    }
  }
  logical as i64
}

pub(crate) fn copy_outcome_to_result(
  outcome: decmpfs::CopyOutcome,
  dest: &Path,
  logical: usize,
) -> DecmpfsResult {
  use decmpfs::CopyOutcome;
  match outcome {
    CopyOutcome::Cloned { compressed } => DecmpfsResult {
      compressed,
      before: logical as i64,
      after: allocated(dest, logical),
      reason: "Cloned".to_string(),
    },
    CopyOutcome::CopiedCompressed { before, after } => DecmpfsResult {
      compressed: true,
      before: before as i64,
      after: after as i64,
      reason: "CopiedCompressed".to_string(),
    },
    CopyOutcome::CopiedPlain { skipped } => DecmpfsResult {
      compressed: false,
      before: logical as i64,
      after: logical as i64,
      reason: match skipped {
        Some(reason) => format!("CopiedPlain:{reason:?}"),
        None => "CopiedPlain".to_string(),
      },
    },
  }
}

pub(crate) fn src_logical(src: &Path, path: &str) -> std::result::Result<usize, NapiFail> {
  std::fs::metadata(src)
    .map(|meta| meta.len() as usize)
    .map_err(|e| NapiFail::Fs(fs_err_io(&e, "stat", path)))
}

// The shared logic for both cp-shaped copy entry points.
pub(crate) fn run_copy(
  src: &str,
  dest: &str,
  options: Option<CopyDecmpfsOptions>,
) -> std::result::Result<DecmpfsResult, NapiFail> {
  let (force, error_on_exist) = match options {
    Some(o) => (o.force.unwrap_or(true), o.error_on_exist.unwrap_or(false)),
    None => (true, false),
  };
  let src_path = Path::new(src);
  let dest_path = Path::new(dest);
  let logical = src_logical(src_path, src)?;
  if dest_path.exists() {
    if error_on_exist {
      return Err(NapiFail::Fs(FsErr {
        code: "EEXIST",
        errno: -17,
        detail: "file already exists".to_string(),
        syscall: "copyfile",
        path: dest.to_string(),
      }));
    }
    if !force {
      // Don't replace — report a skip rather than throw.
      return Ok(DecmpfsResult {
        compressed: false,
        before: logical as i64,
        after: logical as i64,
        reason: "ExistsNoForce".to_string(),
      });
    }
  }
  let outcome = decmpfs::copy_file(src_path, dest_path)
    .map_err(|e| NapiFail::Fs(fs_err_decmpfs(&e, "copyfile", dest)))?;
  Ok(copy_outcome_to_result(outcome, dest_path, logical))
}

/// Synchronously copy `src` to `dest`, preserving OS filesystem compression —
/// the clone-first copy `fs.cp` should do (a plain byte copy re-inflates a
/// compressed file).
#[napi]
pub fn copy_decmpfs_file_sync(
  env: Env,
  src: String,
  dest: String,
  options: Option<CopyDecmpfsOptions>,
) -> Result<DecmpfsResult> {
  run_copy(&src, &dest, options).map_err(|f| f.into_error(&env))
}

/// The async task backing [`copyDecmpfsFile`] — runs the copy on the libuv pool.
pub struct CopyTask {
  src: String,
  dest: String,
  force: Option<bool>,
  error_on_exist: Option<bool>,
  fail: Option<NapiFail>,
}

#[napi]
impl Task for CopyTask {
  type Output = DecmpfsResult;
  type JsValue = DecmpfsResult;

  fn compute(&mut self) -> Result<Self::Output> {
    match run_copy(
      &self.src,
      &self.dest,
      Some(CopyDecmpfsOptions {
        force: self.force,
        error_on_exist: self.error_on_exist,
      }),
    ) {
      Ok(output) => Ok(output),
      Err(f) => {
        self.fail = Some(f);
        Err(Error::from_reason("fs error"))
      }
    }
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }

  fn reject(&mut self, env: Env, err: Error) -> Result<Self::JsValue> {
    match self.fail.take() {
      Some(f) => Err(f.into_error(&env)),
      None => Err(err),
    }
  }
}

/// Asynchronously copy `src` to `dest`, preserving OS filesystem compression.
#[napi]
pub fn copy_decmpfs_file(
  src: String,
  dest: String,
  options: Option<CopyDecmpfsOptions>,
) -> AsyncTask<CopyTask> {
  let (force, error_on_exist) = match options {
    Some(o) => (o.force, o.error_on_exist),
    None => (None, None),
  };
  AsyncTask::new(CopyTask {
    src,
    dest,
    force,
    error_on_exist,
    fail: None,
  })
}

// The shared logic for both `fs.copyFile`-parity entry points. Mode flags match
// Node's: COPYFILE_EXCL rejects an existing `dest`; COPYFILE_FICLONE_FORCE
// requires a copy-on-write clone and throws where one is impossible (Node's own
// FICLONE_FORCE always throws ENOSYS on macOS — libuv has no clonefile path);
// 0 and COPYFILE_FICLONE both take the clone-first, compression-preserving
// copy (this binding never does a compression-dropping plain byte copy).
pub(crate) fn run_copy_file(
  src: &str,
  dest: &str,
  mode: Option<u32>,
) -> std::result::Result<DecmpfsResult, NapiFail> {
  let mode = mode.unwrap_or(0);
  let src_path = Path::new(src);
  let dest_path = Path::new(dest);
  let logical = src_logical(src_path, src)?;
  if mode & COPYFILE_EXCL != 0 && dest_path.exists() {
    return Err(NapiFail::Fs(FsErr {
      code: "EEXIST",
      errno: -17,
      detail: "file already exists".to_string(),
      syscall: "copyfile",
      path: dest.to_string(),
    }));
  }
  if mode & COPYFILE_FICLONE_FORCE != 0 {
    let cloned = decmpfs::try_clone_file(src_path, dest_path)
      .map_err(|e| NapiFail::Fs(fs_err_decmpfs(&e, "copyfile", dest)))?;
    if !cloned {
      return Err(NapiFail::Fs(FsErr {
        code: "ENOTSUP",
        errno: -45,
        detail: "cannot copy-on-write clone (existing destination, cross-volume, or a filesystem without clone support)".to_string(),
        syscall: "copyfile",
        path: dest.to_string(),
      }));
    }
    return Ok(DecmpfsResult {
      compressed: decmpfs::probe(dest_path)
        .map(|s| matches!(s, decmpfs::Support::AlreadyCompressed))
        .unwrap_or(false),
      before: logical as i64,
      after: allocated(dest_path, logical),
      reason: "Cloned".to_string(),
    });
  }
  let outcome = decmpfs::copy_file(src_path, dest_path)
    .map_err(|e| NapiFail::Fs(fs_err_decmpfs(&e, "copyfile", dest)))?;
  Ok(copy_outcome_to_result(outcome, dest_path, logical))
}

/// Synchronous `fs.copyFileSync` parity, decmpfs-aware. See [`copyFile`].
#[napi]
pub fn copy_file_sync(
  env: Env,
  src: String,
  dest: String,
  mode: Option<u32>,
) -> Result<DecmpfsResult> {
  run_copy_file(&src, &dest, mode).map_err(|f| f.into_error(&env))
}

/// The async task backing [`copyFile`].
pub struct CopyFileTask {
  src: String,
  dest: String,
  mode: Option<u32>,
  fail: Option<NapiFail>,
}

#[napi]
impl Task for CopyFileTask {
  type Output = DecmpfsResult;
  type JsValue = DecmpfsResult;

  fn compute(&mut self) -> Result<Self::Output> {
    match run_copy_file(&self.src, &self.dest, self.mode) {
      Ok(output) => Ok(output),
      Err(f) => {
        self.fail = Some(f);
        Err(Error::from_reason("fs error"))
      }
    }
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }

  fn reject(&mut self, env: Env, err: Error) -> Result<Self::JsValue> {
    match self.fail.take() {
      Some(f) => Err(f.into_error(&env)),
      None => Err(err),
    }
  }
}

/// `fsPromises.copyFile(src, dest[, mode])` parity, decmpfs-aware — the copy
/// Node can't do: on macOS, Node's COPYFILE_FICLONE silently degrades to a
/// byte copy that re-inflates a compressed file, and COPYFILE_FICLONE_FORCE
/// always throws ENOSYS. Here both clone via `clonefile(2)`.
#[napi]
pub fn copy_file(src: String, dest: String, mode: Option<u32>) -> AsyncTask<CopyFileTask> {
  AsyncTask::new(CopyFileTask {
    src,
    dest,
    mode,
    fail: None,
  })
}
