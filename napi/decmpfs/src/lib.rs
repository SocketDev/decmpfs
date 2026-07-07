//! N-API binding for the `decmpfs` core.
//!
//! Mirrors `fs.writeFile` / `fs.writeFileSync`: write bytes straight to an
//! OS-FS-compressed file in ONE pass (`decmpfs::compress_bytes` — no write-then-
//! rewrite). Atomic by default (sibling temp + rename, the applesauce /
//! write-file-atomic pattern); `{ atomic: false }` opts into a direct write.
//! cp-shaped replace semantics: `{ force = true, errorOnExist = false }`. Fail-soft
//! — an unsupported FS or a skipped gate is a returned result, never a throw.

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::path::Path;

/// Options for [`writeDecmpfsFile`] / [`writeDecmpfsFileSync`]. All optional.
#[napi(object)]
pub struct WriteDecmpfsOptions {
  /// Replace an existing file at `path`. Default `true` (like `fs.cp`).
  pub force: Option<bool>,
  /// With `force: false`, reject (throw) if `path` already exists. Default `false`.
  pub error_on_exist: Option<bool>,
  /// Write atomically via a sibling temp + rename. Default `true`. `false` writes
  /// `path` directly (faster, but a crash can leave a partial file).
  pub atomic: Option<bool>,
  /// Gate glob (e.g. `**/*.node`). Default: match any path.
  pub glob: Option<String>,
  /// Gate size predicate (e.g. `>= 1MB`). Default: no size floor.
  pub min_size: Option<String>,
}

/// The result of a write — a SUCCESS shape; never thrown for an unsupported FS.
#[napi(object)]
pub struct DecmpfsResult {
  /// Whether the file landed OS-compressed (false = wrote plain: unsupported FS,
  /// incompressible, or gate skip).
  pub compressed: bool,
  /// Logical size of the content written.
  pub before: i64,
  /// On-disk allocated size after the write.
  pub after: i64,
  /// The outcome category (`Compressed` / `NoGain` / `AlreadyCompressed` /
  /// `Unsupported:*` / `Skipped:*` / `ExistsNoForce`).
  pub reason: String,
}

struct Resolved {
  force: bool,
  error_on_exist: bool,
  atomic: bool,
  glob: Option<String>,
  min_size: Option<String>,
}

fn resolve(options: Option<WriteDecmpfsOptions>) -> Resolved {
  match options {
    Some(o) => Resolved {
      force: o.force.unwrap_or(true),
      error_on_exist: o.error_on_exist.unwrap_or(false),
      atomic: o.atomic.unwrap_or(true),
      glob: o.glob,
      min_size: o.min_size,
    },
    None => Resolved {
      force: true,
      error_on_exist: false,
      atomic: true,
      glob: None,
      min_size: None,
    },
  }
}

fn to_result(outcome: decmpfs::Outcome, raw_len: usize) -> DecmpfsResult {
  use decmpfs::Outcome;
  match outcome {
    Outcome::Compressed { before, after } => DecmpfsResult {
      compressed: true,
      before: before as i64,
      after: after as i64,
      reason: "Compressed".to_string(),
    },
    Outcome::NoGain { before, after } => DecmpfsResult {
      compressed: false,
      before: before as i64,
      after: after as i64,
      reason: "NoGain".to_string(),
    },
    Outcome::AlreadyCompressed { before } => DecmpfsResult {
      compressed: true,
      before: before as i64,
      after: before as i64,
      reason: "AlreadyCompressed".to_string(),
    },
    Outcome::Unsupported { reason } => DecmpfsResult {
      compressed: false,
      before: raw_len as i64,
      after: raw_len as i64,
      reason: format!("Unsupported:{reason:?}"),
    },
    Outcome::Skipped { reason } => DecmpfsResult {
      compressed: false,
      before: raw_len as i64,
      after: raw_len as i64,
      reason: format!("Skipped:{reason:?}"),
    },
  }
}

// The shared logic for both the sync and async entry points.
fn run(path: &str, data: &[u8], r: &Resolved) -> Result<DecmpfsResult> {
  let target = Path::new(path);
  let exists = target.exists();
  if exists && r.error_on_exist {
    return Err(Error::new(
      Status::GenericFailure,
      format!("file already exists: {path}"),
    ));
  }
  if exists && !r.force {
    // Don't replace — report a skip rather than throw.
    return Ok(DecmpfsResult {
      compressed: false,
      before: data.len() as i64,
      after: data.len() as i64,
      reason: "ExistsNoForce".to_string(),
    });
  }
  let gate = decmpfs::Gate::new(r.glob.as_deref(), r.min_size.as_deref())
    .map_err(|e| Error::new(Status::InvalidArg, format!("invalid gate: {e}")))?;

  // Direct write: compress_bytes applies the gate to `target` itself — correct.
  if !r.atomic {
    let outcome = decmpfs::compress_bytes(target, data, &gate)
      .map_err(|e| Error::new(Status::GenericFailure, format!("write: {e}")))?;
    return Ok(to_result(outcome, data.len()));
  }

  // Atomic: write a sibling temp then rename over `target`. The gate's glob must be
  // judged against the REAL target path, NOT the temp name (which ends in `.tmp` and
  // would wrongly fail a `**/*.node`-style glob). So pre-decide here, then compress
  // the temp unconditionally with Gate::any(); rename carries the compression over
  // (same FS → same inode/extents).
  let normalized = target.to_string_lossy().replace('\\', "/");
  let dir = target.parent().unwrap_or_else(|| Path::new("."));
  let name = target
    .file_name()
    .and_then(|n| n.to_str())
    .unwrap_or("decmpfs-out");
  let tmp = dir.join(format!(".{name}.decmpfs-{}.tmp", std::process::id()));
  let result = if gate.matches(&normalized, data.len() as u64) {
    let outcome = decmpfs::compress_bytes(&tmp, data, &decmpfs::Gate::any()).map_err(|e| {
      let _ = std::fs::remove_file(&tmp);
      Error::new(Status::GenericFailure, format!("write: {e}"))
    })?;
    to_result(outcome, data.len())
  } else {
    std::fs::write(&tmp, data).map_err(|e| {
      let _ = std::fs::remove_file(&tmp);
      Error::new(Status::GenericFailure, format!("write: {e}"))
    })?;
    DecmpfsResult {
      compressed: false,
      before: data.len() as i64,
      after: data.len() as i64,
      reason: "Skipped:GateExcluded".to_string(),
    }
  };
  std::fs::rename(&tmp, target).map_err(|e| {
    let _ = std::fs::remove_file(&tmp);
    Error::new(Status::GenericFailure, format!("rename: {e}"))
  })?;
  Ok(result)
}

/// Synchronously write `data` to `path` as an OS-FS-compressed file.
#[napi]
pub fn write_decmpfs_file_sync(
  path: String,
  data: Buffer,
  options: Option<WriteDecmpfsOptions>,
) -> Result<DecmpfsResult> {
  run(&path, &data, &resolve(options))
}

/// The async task backing [`writeDecmpfsFile`] — runs the write on the libuv pool.
pub struct WriteTask {
  path: String,
  data: Vec<u8>,
  opts: Resolved,
}

#[napi]
impl Task for WriteTask {
  type Output = DecmpfsResult;
  type JsValue = DecmpfsResult;

  fn compute(&mut self) -> Result<Self::Output> {
    run(&self.path, &self.data, &self.opts)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Asynchronously write `data` to `path` as an OS-FS-compressed file.
#[napi]
pub fn write_decmpfs_file(
  path: String,
  data: Buffer,
  options: Option<WriteDecmpfsOptions>,
) -> AsyncTask<WriteTask> {
  AsyncTask::new(WriteTask {
    path,
    data: data.to_vec(),
    opts: resolve(options),
  })
}

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
fn allocated(path: &Path, logical: usize) -> i64 {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if let Ok(meta) = std::fs::metadata(path) {
      return (meta.blocks() * 512) as i64;
    }
  }
  logical as i64
}

fn copy_outcome_to_result(
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

fn src_logical(src: &Path) -> Result<usize> {
  std::fs::metadata(src)
    .map(|meta| meta.len() as usize)
    .map_err(|e| Error::new(Status::GenericFailure, format!("stat source: {e}")))
}

// The shared logic for both cp-shaped copy entry points.
fn run_copy(src: &str, dest: &str, options: Option<CopyDecmpfsOptions>) -> Result<DecmpfsResult> {
  let (force, error_on_exist) = match options {
    Some(o) => (o.force.unwrap_or(true), o.error_on_exist.unwrap_or(false)),
    None => (true, false),
  };
  let src_path = Path::new(src);
  let dest_path = Path::new(dest);
  let logical = src_logical(src_path)?;
  if dest_path.exists() {
    if error_on_exist {
      return Err(Error::new(
        Status::GenericFailure,
        format!("file already exists: {dest}"),
      ));
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
    .map_err(|e| Error::new(Status::GenericFailure, format!("copy: {e}")))?;
  Ok(copy_outcome_to_result(outcome, dest_path, logical))
}

/// Synchronously copy `src` to `dest`, preserving OS filesystem compression —
/// the clone-first copy `fs.cp` should do (a plain byte copy re-inflates a
/// compressed file).
#[napi]
pub fn copy_decmpfs_file_sync(
  src: String,
  dest: String,
  options: Option<CopyDecmpfsOptions>,
) -> Result<DecmpfsResult> {
  run_copy(&src, &dest, options)
}

/// The async task backing [`copyDecmpfsFile`] — runs the copy on the libuv pool.
pub struct CopyTask {
  src: String,
  dest: String,
  force: Option<bool>,
  error_on_exist: Option<bool>,
}

#[napi]
impl Task for CopyTask {
  type Output = DecmpfsResult;
  type JsValue = DecmpfsResult;

  fn compute(&mut self) -> Result<Self::Output> {
    run_copy(
      &self.src,
      &self.dest,
      Some(CopyDecmpfsOptions {
        force: self.force,
        error_on_exist: self.error_on_exist,
      }),
    )
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
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
  })
}

// The shared logic for both `fs.copyFile`-parity entry points. Mode flags match
// Node's: COPYFILE_EXCL rejects an existing `dest`; COPYFILE_FICLONE_FORCE
// requires a copy-on-write clone and throws where one is impossible (Node's own
// FICLONE_FORCE always throws ENOSYS on macOS — libuv has no clonefile path);
// 0 and COPYFILE_FICLONE both take the clone-first, compression-preserving
// copy (this binding never does a compression-dropping plain byte copy).
fn run_copy_file(src: &str, dest: &str, mode: Option<u32>) -> Result<DecmpfsResult> {
  let mode = mode.unwrap_or(0);
  let src_path = Path::new(src);
  let dest_path = Path::new(dest);
  let logical = src_logical(src_path)?;
  if mode & COPYFILE_EXCL != 0 && dest_path.exists() {
    return Err(Error::new(
      Status::GenericFailure,
      format!("EEXIST: file already exists, copyfile -> {dest}"),
    ));
  }
  if mode & COPYFILE_FICLONE_FORCE != 0 {
    let cloned = decmpfs::try_clone_file(src_path, dest_path)
      .map_err(|e| Error::new(Status::GenericFailure, format!("copy: {e}")))?;
    if !cloned {
      return Err(Error::new(
        Status::GenericFailure,
        format!("ENOTSUP: cannot copy-on-write clone, copyfile {src} -> {dest} (existing destination, cross-volume, or a filesystem without clone support)"),
      ));
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
    .map_err(|e| Error::new(Status::GenericFailure, format!("copy: {e}")))?;
  Ok(copy_outcome_to_result(outcome, dest_path, logical))
}

/// Synchronous `fs.copyFileSync` parity, decmpfs-aware. See [`copyFile`].
#[napi]
pub fn copy_file_sync(src: String, dest: String, mode: Option<u32>) -> Result<DecmpfsResult> {
  run_copy_file(&src, &dest, mode)
}

/// The async task backing [`copyFile`].
pub struct CopyFileTask {
  src: String,
  dest: String,
  mode: Option<u32>,
}

#[napi]
impl Task for CopyFileTask {
  type Output = DecmpfsResult;
  type JsValue = DecmpfsResult;

  fn compute(&mut self) -> Result<Self::Output> {
    run_copy_file(&self.src, &self.dest, self.mode)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// `fsPromises.copyFile(src, dest[, mode])` parity, decmpfs-aware — the copy
/// Node can't do: on macOS, Node's COPYFILE_FICLONE silently degrades to a
/// byte copy that re-inflates a compressed file, and COPYFILE_FICLONE_FORCE
/// always throws ENOSYS. Here both clone via `clonefile(2)`.
#[napi]
pub fn copy_file(src: String, dest: String, mode: Option<u32>) -> AsyncTask<CopyFileTask> {
  AsyncTask::new(CopyFileTask { src, dest, mode })
}

/// Options for [`packExecutable`] / [`packExecutableSync`].
#[napi(object)]
pub struct PackExeOptions {
  /// Path to the self-replacing stub binary the payload is injected into — a
  /// decmpfs-stub build (`cargo build --features exe`, target `decmpfs-stub`)
  /// or any executable whose `main` calls `decmpfs::exe::self_replace_and_exec`.
  /// REQUIRED: the Node host is not a self-replacing runtime, so there is no
  /// sensible default — a packed file built on a stub without that runtime just
  /// runs the stub and never materializes the payload.
  pub stub: String,
  /// Gate glob (e.g. `**/*.node`). Default: match any path.
  pub gate_glob: Option<String>,
  /// Gate size predicate (e.g. `>= 1MB`). Default: no size floor.
  pub gate_size: Option<String>,
}

/// The result of packing an executable — a SUCCESS shape; never thrown for a
/// gate miss.
#[napi(object)]
pub struct PackExeResult {
  /// Whether the executable was packed (`false` = the gate excluded it).
  pub packed: bool,
  /// Logical size of the source executable (`0` on a gate miss).
  pub before: i64,
  /// On-disk size of the packed stub (`0` on a gate miss).
  pub after: i64,
  /// Whether the gate rejected the input — nothing was read or written.
  pub skipped_gate: bool,
}

fn pack_gate(options: &PackExeOptions) -> Result<decmpfs::Gate> {
  decmpfs::Gate::new(options.gate_glob.as_deref(), options.gate_size.as_deref())
    .map_err(|e| Error::new(Status::InvalidArg, format!("invalid gate: {e}")))
}

fn pack_outcome_to_result(outcome: decmpfs::exe::PackOutcome) -> PackExeResult {
  use decmpfs::exe::PackOutcome;
  match outcome {
    PackOutcome::Packed { before, after } => PackExeResult {
      packed: true,
      before: before as i64,
      after: after as i64,
      skipped_gate: false,
    },
    PackOutcome::SkippedGate => PackExeResult {
      packed: false,
      before: 0,
      after: 0,
      skipped_gate: true,
    },
  }
}

// The shared logic for both the sync and async pack entry points. Injects the
// payload into the caller-supplied `options.stub` — the Node host is not a
// self-replacing runtime, so there is no `current_exe()` default.
fn run_pack(src: &str, dest: &str, options: PackExeOptions) -> Result<PackExeResult> {
  let gate = pack_gate(&options)?;
  let outcome = decmpfs::exe::pack_executable_with_stub(
    Path::new(&options.stub),
    Path::new(src),
    Path::new(dest),
    &gate,
  )
  .map_err(|e| Error::new(Status::GenericFailure, format!("pack: {e}")))?;
  Ok(pack_outcome_to_result(outcome))
}

/// Synchronously pack `src` into a self-replacing executable at `dest`, using
/// `options.stub` as the runtime stub. On first run the packed `dest`
/// decompresses `src` back to disk FS-compressed, swaps itself out for it, and
/// execs it; every later run is the plain materialized executable.
#[napi]
pub fn pack_executable_sync(
  src: String,
  dest: String,
  options: PackExeOptions,
) -> Result<PackExeResult> {
  run_pack(&src, &dest, options)
}

/// The async task backing [`packExecutable`] — runs the pack on the libuv pool.
pub struct PackExeTask {
  src: String,
  dest: String,
  options: PackExeOptions,
}

#[napi]
impl Task for PackExeTask {
  type Output = PackExeResult;
  type JsValue = PackExeResult;

  fn compute(&mut self) -> Result<Self::Output> {
    run_pack(
      &self.src,
      &self.dest,
      PackExeOptions {
        stub: self.options.stub.clone(),
        gate_glob: self.options.gate_glob.clone(),
        gate_size: self.options.gate_size.clone(),
      },
    )
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Asynchronously pack `src` into a self-replacing executable at `dest` using
/// `options.stub`. See [`packExecutableSync`].
#[napi]
pub fn pack_executable(
  src: String,
  dest: String,
  options: PackExeOptions,
) -> AsyncTask<PackExeTask> {
  AsyncTask::new(PackExeTask { src, dest, options })
}
