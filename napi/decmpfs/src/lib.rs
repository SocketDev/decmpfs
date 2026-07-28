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
fn run(path: &str, data: &[u8], r: &Resolved) -> std::result::Result<DecmpfsResult, NapiFail> {
  let target = Path::new(path);
  let exists = target.exists();
  if exists && r.error_on_exist {
    return Err(NapiFail::Fs(FsErr {
      code: "EEXIST",
      errno: -17,
      detail: "file already exists".to_string(),
      syscall: "open",
      path: path.to_string(),
    }));
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
    .map_err(|e| NapiFail::Arg(format!("invalid gate: {e}")))?;

  // Direct write: compress_bytes applies the gate to `target` itself — correct.
  if !r.atomic {
    let outcome = decmpfs::compress_bytes(target, data, &gate)
      .map_err(|e| NapiFail::Fs(fs_err_decmpfs(&e, "open", path)))?;
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
      NapiFail::Fs(fs_err_decmpfs(&e, "open", path))
    })?;
    to_result(outcome, data.len())
  } else {
    std::fs::write(&tmp, data).map_err(|e| {
      let _ = std::fs::remove_file(&tmp);
      NapiFail::Fs(fs_err_io(&e, "open", path))
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
    NapiFail::Fs(fs_err_io(&e, "rename", path))
  })?;
  Ok(result)
}

/// Synchronously write `data` to `path` as an OS-FS-compressed file.
#[napi]
pub fn write_decmpfs_file_sync(
  env: Env,
  path: String,
  data: Buffer,
  options: Option<WriteDecmpfsOptions>,
) -> Result<DecmpfsResult> {
  run(&path, &data, &resolve(options)).map_err(|f| f.into_error(&env))
}

/// The async task backing [`writeDecmpfsFile`] — runs the write on the libuv pool.
pub struct WriteTask {
  path: String,
  data: Vec<u8>,
  opts: Resolved,
  fail: Option<NapiFail>,
}

#[napi]
impl Task for WriteTask {
  type Output = DecmpfsResult;
  type JsValue = DecmpfsResult;

  fn compute(&mut self) -> Result<Self::Output> {
    match run(&self.path, &self.data, &self.opts) {
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
    fail: None,
  })
}

/// Options for the incremental Node Writable adapter.
#[napi(object)]
pub struct StreamDecmpfsOptions {
  /// Replace an existing file at `path`. Default `true`.
  pub force: Option<bool>,
  /// With `force: false`, reject if `path` already exists. Default `false`.
  pub error_on_exist: Option<bool>,
  /// Gate glob (e.g. `**/*.node`). Default: match any path.
  pub glob: Option<String>,
  /// Gate size predicate (e.g. `>= 1MB`). Default: no size floor.
  pub min_size: Option<String>,
}

/// Native state behind `createDecmpfsWriteStream`. JavaScript owns the
/// `Writable` protocol and forwards each chunk into this bounded-memory writer.
#[napi]
pub struct DecmpfsWriteHandle {
  path: String,
  expected_len: usize,
  writer: Option<decmpfs::DecmpfsWriter>,
  exists_no_force: bool,
}

#[napi]
impl DecmpfsWriteHandle {
  #[napi(constructor)]
  pub fn new(
    env: Env,
    path: String,
    size: i64,
    options: Option<StreamDecmpfsOptions>,
  ) -> Result<Self> {
    if size < 0 {
      return Err(Error::new(
        Status::InvalidArg,
        "stream size must be a non-negative safe integer".to_string(),
      ));
    }
    let expected_len = usize::try_from(size).map_err(|_| {
      Error::new(
        Status::InvalidArg,
        "stream size does not fit this platform".to_string(),
      )
    })?;
    let opts = options.unwrap_or(StreamDecmpfsOptions {
      force: None,
      error_on_exist: None,
      glob: None,
      min_size: None,
    });
    let force = opts.force.unwrap_or(true);
    let error_on_exist = opts.error_on_exist.unwrap_or(false);
    let target = Path::new(&path);
    let exists = target.exists();
    if exists && error_on_exist {
      return Err(
        NapiFail::Fs(FsErr {
          code: "EEXIST",
          errno: -17,
          detail: "file already exists".to_string(),
          syscall: "open",
          path,
        })
        .into_error(&env),
      );
    }
    if exists && !force {
      return Ok(Self {
        path,
        expected_len,
        writer: None,
        exists_no_force: true,
      });
    }
    let gate = decmpfs::Gate::new(opts.glob.as_deref(), opts.min_size.as_deref())
      .map_err(|error| Error::new(Status::InvalidArg, format!("invalid gate: {error}")))?;
    let writer = decmpfs::DecmpfsWriter::create(target, expected_len as u64, &gate)
      .map_err(|error| throw_decmpfs(&env, &error, "open", &path))?;
    Ok(Self {
      path,
      expected_len,
      writer: Some(writer),
      exists_no_force: false,
    })
  }

  #[napi]
  pub fn abort(&mut self, env: Env) -> Result<()> {
    if let Some(writer) = self.writer.take() {
      writer
        .abort()
        .map_err(|error| throw_decmpfs(&env, &error, "unlink", &self.path))?;
    }
    Ok(())
  }

  #[napi]
  pub fn finish(&mut self, env: Env) -> Result<DecmpfsResult> {
    if self.exists_no_force {
      self.exists_no_force = false;
      return Ok(DecmpfsResult {
        compressed: false,
        before: self.expected_len as i64,
        after: self.expected_len as i64,
        reason: "ExistsNoForce".to_string(),
      });
    }
    let writer = self.writer.take().ok_or_else(|| {
      Error::new(
        Status::InvalidArg,
        "decmpfs stream is already finished or aborted".to_string(),
      )
    })?;
    let outcome = writer
      .finish()
      .map_err(|error| throw_decmpfs(&env, &error, "close", &self.path))?;
    Ok(to_result(outcome, self.expected_len))
  }

  #[napi]
  pub fn write(&mut self, env: Env, data: Buffer) -> Result<()> {
    if self.exists_no_force {
      return Ok(());
    }
    let writer = self.writer.as_mut().ok_or_else(|| {
      Error::new(
        Status::InvalidArg,
        "decmpfs stream is already finished or aborted".to_string(),
      )
    })?;
    writer
      .write_chunk(&data)
      .map_err(|error| throw_decmpfs(&env, &error, "write", &self.path))
  }
}

mod copy;
pub use copy::*;

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

fn pack_gate(options: &PackExeOptions) -> std::result::Result<decmpfs::Gate, NapiFail> {
  decmpfs::Gate::new(options.gate_glob.as_deref(), options.gate_size.as_deref())
    .map_err(|e| NapiFail::Arg(format!("invalid gate: {e}")))
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
fn run_pack(
  src: &str,
  dest: &str,
  options: PackExeOptions,
) -> std::result::Result<PackExeResult, NapiFail> {
  let gate = pack_gate(&options)?;
  let outcome = decmpfs::exe::pack_executable_with_stub(
    Path::new(&options.stub),
    Path::new(src),
    Path::new(dest),
    &gate,
  )
  .map_err(|e| {
    NapiFail::Fs(FsErr {
      code: "UNKNOWN",
      errno: 0,
      detail: format!("pack: {e}"),
      syscall: "pack",
      path: src.to_string(),
    })
  })?;
  Ok(pack_outcome_to_result(outcome))
}

/// Synchronously pack `src` into a self-replacing executable at `dest`, using
/// `options.stub` as the runtime stub. On first run the packed `dest`
/// decompresses `src` back to disk FS-compressed, swaps itself out for it, and
/// execs it; every later run is the plain materialized executable.
#[napi]
pub fn pack_executable_sync(
  env: Env,
  src: String,
  dest: String,
  options: PackExeOptions,
) -> Result<PackExeResult> {
  run_pack(&src, &dest, options).map_err(|f| f.into_error(&env))
}

/// The async task backing [`packExecutable`] — runs the pack on the libuv pool.
pub struct PackExeTask {
  src: String,
  dest: String,
  options: PackExeOptions,
  fail: Option<NapiFail>,
}

#[napi]
impl Task for PackExeTask {
  type Output = PackExeResult;
  type JsValue = PackExeResult;

  fn compute(&mut self) -> Result<Self::Output> {
    match run_pack(
      &self.src,
      &self.dest,
      PackExeOptions {
        stub: self.options.stub.clone(),
        gate_glob: self.options.gate_glob.clone(),
        gate_size: self.options.gate_size.clone(),
      },
    ) {
      Ok(output) => Ok(output),
      Err(f) => {
        self.fail = Some(f);
        Err(Error::from_reason("pack error"))
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

/// Asynchronously pack `src` into a self-replacing executable at `dest` using
/// `options.stub`. See [`packExecutableSync`].
#[napi]
pub fn pack_executable(
  src: String,
  dest: String,
  options: PackExeOptions,
) -> AsyncTask<PackExeTask> {
  AsyncTask::new(PackExeTask {
    src,
    dest,
    options,
    fail: None,
  })
}

mod errors;
use errors::*;

// ── rm / rmSync (Node fs.rm parity) ──────────────────────────────────────────

/// Options for [`rm`] / [`rmSync`] — exactly Node's `fs.rm` options.
#[napi(object)]
pub struct RmOptions {
  /// Recursive removal (`rm -rf` with `force`). Default `false`.
  pub recursive: Option<bool>,
  /// Ignore a missing path AND bypass the safe-delete guard (cwd/ancestor/root).
  /// Default `false`.
  pub force: Option<bool>,
  /// Retries on EBUSY/EMFILE/ENFILE/ENOTEMPTY/EPERM — recursive mode only.
  /// Default `0`.
  pub max_retries: Option<u32>,
  /// Milliseconds between retries with linear backoff — recursive mode only.
  /// Default `100`.
  pub retry_delay: Option<u32>,
}

fn to_rm_opts(o: Option<RmOptions>) -> decmpfs::RmOptions {
  match o {
    Some(o) => decmpfs::RmOptions {
      recursive: o.recursive.unwrap_or(false),
      force: o.force.unwrap_or(false),
      max_retries: o.max_retries.unwrap_or(0),
      retry_delay_ms: u64::from(o.retry_delay.unwrap_or(100)),
    },
    None => decmpfs::RmOptions::default(),
  }
}

/// `fs.rmSync(path, options)` — decmpfs-aware, with the safe-delete guard.
#[napi]
pub fn rm_sync(env: Env, path: String, options: Option<RmOptions>) -> Result<()> {
  decmpfs::rm(Path::new(&path), &to_rm_opts(options))
    .map_err(|e| throw_decmpfs(&env, &e, "rm", &path))
}

/// The async task backing [`rm`]. Carries the Node error parts across the
/// threadpool boundary — `compute` has no `Env`, so the JS error is built in
/// `reject` where one is available.
pub struct RmTask {
  path: String,
  opts: decmpfs::RmOptions,
  err: Option<FsErr>,
}

#[napi]
impl Task for RmTask {
  type Output = ();
  type JsValue = ();

  fn compute(&mut self) -> Result<()> {
    match decmpfs::rm(Path::new(&self.path), &self.opts) {
      Ok(()) => Ok(()),
      Err(e) => {
        self.err = Some(fs_err_decmpfs(&e, "rm", &self.path));
        Err(Error::from_reason("fs error"))
      }
    }
  }

  fn resolve(&mut self, _env: Env, _output: ()) -> Result<()> {
    Ok(())
  }

  fn reject(&mut self, env: Env, err: Error) -> Result<()> {
    match self.err.take() {
      Some(fe) => Err(build_fs_error(&env, &fe)),
      None => Err(err),
    }
  }
}

/// `fsPromises.rm(path, options)` — decmpfs-aware, with the safe-delete guard.
#[napi]
pub fn rm(path: String, options: Option<RmOptions>) -> AsyncTask<RmTask> {
  AsyncTask::new(RmTask {
    path,
    opts: to_rm_opts(options),
    err: None,
  })
}

// ── compressFile / compressFileSync (chmod-like: make an existing file compfs) ─

fn file_len(path: &str) -> usize {
  std::fs::metadata(path)
    .map(|m| m.len() as usize)
    .unwrap_or(0)
}

/// Turn an existing file into an OS-FS-compressed file IN PLACE (atomic rewrite
/// — read, write compressed, rename). The `chmod`-for-compression op: the file's
/// bytes are unchanged to every reader, only its on-disk representation changes.
#[napi]
pub fn compress_file_sync(env: Env, path: String) -> Result<DecmpfsResult> {
  match decmpfs::compress_file(Path::new(&path)) {
    Ok(outcome) => Ok(to_result(outcome, file_len(&path))),
    Err(e) => Err(throw_decmpfs(&env, &e, "open", &path)),
  }
}

/// The async task backing [`compressFile`].
pub struct CompressFileTask {
  path: String,
  err: Option<FsErr>,
}

#[napi]
impl Task for CompressFileTask {
  type Output = DecmpfsResult;
  type JsValue = DecmpfsResult;

  fn compute(&mut self) -> Result<DecmpfsResult> {
    match decmpfs::compress_file(Path::new(&self.path)) {
      Ok(outcome) => Ok(to_result(outcome, file_len(&self.path))),
      Err(e) => {
        self.err = Some(fs_err_decmpfs(&e, "open", &self.path));
        Err(Error::from_reason("fs error"))
      }
    }
  }

  fn resolve(&mut self, _env: Env, output: DecmpfsResult) -> Result<DecmpfsResult> {
    Ok(output)
  }

  fn reject(&mut self, env: Env, err: Error) -> Result<DecmpfsResult> {
    match self.err.take() {
      Some(fe) => Err(build_fs_error(&env, &fe)),
      None => Err(err),
    }
  }
}

/// Async in-place compress — see [`compressFileSync`].
#[napi]
pub fn compress_file(path: String) -> AsyncTask<CompressFileTask> {
  AsyncTask::new(CompressFileTask { path, err: None })
}

/// The FS-compression state of a path — returned by [`decmpfsStat`].
#[napi(object)]
pub struct DecmpfsStat {
  /// Whether the file is stored OS-FS-compressed on disk.
  pub compressed: bool,
  /// Logical (apparent) size in bytes — constant regardless of compression.
  pub logical: i64,
  /// Physical, on-disk allocated size in bytes — where the win shows.
  pub physical: i64,
}

/// Inspect a path's FS-compression state (`{ compressed, logical, physical }`).
/// Sync-only by design: it is a single metadata read, so — unlike the
/// compress/copy/pack ops — there is no expensive work to offload to a task.
#[napi]
pub fn decmpfs_stat(env: Env, path: String) -> Result<DecmpfsStat> {
  match decmpfs::stat(Path::new(&path)) {
    Ok(s) => Ok(DecmpfsStat {
      compressed: s.compressed,
      logical: s.logical as i64,
      physical: s.physical as i64,
    }),
    Err(e) => Err(throw_decmpfs(&env, &e, "stat", &path)),
  }
}
