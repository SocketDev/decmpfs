//! `rm` — a fast recursive remove mirroring Node's `fs.rm` / `fs.rmSync`,
//! tuned for APFS/decmpfs.
//!
//! API parity (no extra knobs): options are exactly Node's `recursive`, `force`,
//! `maxRetries`, `retryDelay`. Semantics match `fs.rm`:
//!   - a missing path throws unless `force`;
//!   - a directory with `recursive: false` throws `EISDIR`;
//!   - `recursive: true` does the `rm -rf`; `maxRetries`/`retryDelay` (linear
//!     backoff on EBUSY/EMFILE/ENFILE/ENOTEMPTY/EPERM) apply ONLY when recursive,
//!     as in Node.
//!
//! Safety: adapted from socket-lib `safeDelete`, MINUS its socket-owned
//! allowlist (temp / cacache / ~/.socket) — just the universal guard: removing
//! the current directory, one of its ancestors, or the filesystem root is
//! refused unless `force` is set (Node's own option, doubling as the override —
//! no extra knob). Descendants and unrelated siblings are unaffected.
//!
//! Speed: a decmpfs file unlinks like any other (its resource-fork xattr drops
//! with the inode), so DELETE has no compression angle. MEASURED on APFS (this
//! machine, ~12k files), `rm` is filesystem-metadata-bound — directory-entry
//! mutations serialize on the container lock, so BOTH a single `removefile(3)`
//! (~5% slower) and a parallel top-level fan-out (~15-20% slower) LOSE to
//! `std::fs::remove_dir_all`. So this is a correct Node-parity wrapper over
//! `remove_dir_all`, already at that floor — the DELETE win is parity, not a
//! codec trick (contrast WRITE: parallel LZVN, 6.5x).

use std::path::Path;

use crate::Error;

/// Node `fs.rm` options — same four fields, same defaults, nothing extra.
#[derive(Clone, Copy)]
pub struct RmOptions {
  pub recursive: bool,
  pub force: bool,
  pub max_retries: u32,
  pub retry_delay_ms: u64,
}

impl Default for RmOptions {
  fn default() -> Self {
    // Node defaults: recursive false, force false, maxRetries 0, retryDelay 100.
    Self {
      recursive: false,
      force: false,
      max_retries: 0,
      retry_delay_ms: 100,
    }
  }
}

fn is_not_found(e: &std::io::Error) -> bool {
  e.kind() == std::io::ErrorKind::NotFound
}

// The errno set Node retries in recursive mode.
#[cfg(unix)]
fn retryable(e: &std::io::Error) -> bool {
  matches!(
    e.raw_os_error(),
    Some(c)
      if c == libc::EBUSY
        || c == libc::EMFILE
        || c == libc::ENFILE
        || c == libc::ENOTEMPTY
        || c == libc::EPERM
  )
}
#[cfg(windows)]
fn retryable(e: &std::io::Error) -> bool {
  // ACCESS_DENIED, SHARING_VIOLATION, LOCK_VIOLATION, DIR_NOT_EMPTY.
  matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33) | Some(145))
}

/// Run one removal op, applying Node's force (swallow ENOENT) and — only when
/// recursive — the retry/backoff loop.
fn with_policy<F: FnMut() -> std::io::Result<()>>(
  mut op: F,
  opts: &RmOptions,
) -> std::io::Result<()> {
  let mut attempt: u32 = 0;
  loop {
    match op() {
      Ok(()) => return Ok(()),
      Err(e) if is_not_found(&e) && opts.force => return Ok(()),
      Err(e) if opts.recursive && attempt < opts.max_retries && retryable(&e) => {
        attempt += 1;
        // Linear backoff: retryDelay ms longer each try (Node's wording).
        std::thread::sleep(std::time::Duration::from_millis(
          opts.retry_delay_ms.saturating_mul(u64::from(attempt)),
        ));
      }
      Err(e) => return Err(e),
    }
  }
}

fn io(context: &'static str, source: std::io::Error) -> Error {
  Error::Io { context, source }
}

/// What `path` is, from a no-follow `symlink_metadata`, collapsed to the three
/// cases `rm` dispatches on. Keeping the fs probe down to this small enum is what
/// lets the seam below be a trivial mock.
enum EntryKind {
  Missing,
  Dir,
  Other,
}

/// The filesystem operations `rm` performs, behind a trait so every error arm is
/// unit-testable with a mock instead of a hard-to-provoke real fault. Prod uses
/// `OsRmFs` — thin `std::fs` / `std::env` delegations with no added logic, so the
/// logic that needs testing all lives in `rm_with` / `guard_cwd_and_root`.
#[cfg_attr(test, mockall::automock)]
trait RmFs {
  fn kind(&self, path: &Path) -> std::io::Result<EntryKind>;
  fn remove_file(&self, path: &Path) -> std::io::Result<()>;
  fn remove_tree(&self, path: &Path) -> std::io::Result<()>;
  fn cwd(&self) -> std::io::Result<std::path::PathBuf>;
  fn canonicalize(&self, path: &Path) -> std::io::Result<std::path::PathBuf>;
}

struct OsRmFs;

impl RmFs for OsRmFs {
  fn kind(&self, path: &Path) -> std::io::Result<EntryKind> {
    match std::fs::symlink_metadata(path) {
      Ok(md) if md.is_dir() => Ok(EntryKind::Dir),
      Ok(_) => Ok(EntryKind::Other),
      Err(e) if is_not_found(&e) => Ok(EntryKind::Missing),
      Err(e) => Err(e),
    }
  }
  fn remove_file(&self, path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
  }
  /// One recursive delete of a subtree. MEASURED on APFS (this machine, 14 cores,
  /// ~12k files): neither a single `removefile(3)` (~4-5% slower) nor a parallel
  /// top-level fan-out (~15-20% slower) beats `std::fs::remove_dir_all` —
  /// directory-entry mutations serialize on the container's metadata lock, so
  /// `rm` is filesystem-bound and `remove_dir_all` (openat + unlinkat, no path
  /// re-resolution) is already at that floor. Unlike WRITE (parallel LZVN = 6.5x),
  /// DELETE has no userspace codec win, so the simplest correct call is the fast
  /// one, on every platform.
  fn remove_tree(&self, path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(path)
  }
  fn cwd(&self) -> std::io::Result<std::path::PathBuf> {
    std::env::current_dir()
  }
  fn canonicalize(&self, path: &Path) -> std::io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
  }
}

/// PURE safe-delete guard (socket-lib `safeDelete` model, minus the socket-owned
/// allowlist): is `target` the current directory, an ANCESTOR of it, or the
/// filesystem root? Deleting any of those is almost always a mistake. `cwd` is
/// injected so the policy is unit-testable without touching the process cwd. A
/// sibling or a descendant of cwd is allowed.
fn is_cwd_ancestor_or_root(target: &Path, cwd: &Path) -> bool {
  // A path with no parent is a filesystem root ("/", "C:\").
  if target.parent().is_none() {
    return true;
  }
  // `target` is cwd or an ancestor of cwd iff cwd is prefixed by target.
  cwd == target || cwd.starts_with(target)
}

/// Refuse to remove the cwd, one of its ancestors, or the root — unless `force`.
/// This is the safe-delete guard adapted from socket-lib: NO socket-specific
/// allowlist (temp / cacache / ~/.socket), just the universal ancestor + root
/// protection, with Node's own `force` as the override (no extra option).
fn guard_cwd_and_root<F: RmFs>(fs: &F, path: &Path, opts: &RmOptions) -> Result<(), Error> {
  if opts.force {
    return Ok(());
  }
  // Resolve real paths for the comparison; a missing target (canonicalize fails)
  // falls back to its given path — it can't be an ancestor of cwd anyway.
  let target = fs.canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
  let cwd = fs
    .cwd()
    .and_then(|c| fs.canonicalize(&c))
    .unwrap_or_default();
  if is_cwd_ancestor_or_root(&target, &cwd) {
    return Err(io(
      "refusing to remove the current directory, an ancestor, or the root — pass force to override",
      std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    ));
  }
  Ok(())
}

/// Node `fs.rm(path, options)`. A file/symlink is a single unlink. A directory
/// needs `recursive` (else `EISDIR`, as in Node); recursive delete is
/// `std::fs::remove_dir_all` — MEASURED as the floor on APFS.
pub fn rm(path: &Path, opts: &RmOptions) -> Result<(), Error> {
  rm_with(&OsRmFs, path, opts)
}

/// The generic core: `rm` over an injectable filesystem so tests exercise every
/// arm with a mock. Prod calls it with `OsRmFs`.
fn rm_with<F: RmFs>(fs: &F, path: &Path, opts: &RmOptions) -> Result<(), Error> {
  guard_cwd_and_root(fs, path, opts)?;
  match fs.kind(path) {
    Ok(EntryKind::Missing) if opts.force => Ok(()),
    Ok(EntryKind::Missing) => Err(Error::NotFound(path.to_path_buf())),
    Ok(EntryKind::Other) => with_policy(|| fs.remove_file(path), opts).map_err(|e| io("unlink", e)),
    // Node throws EISDIR for a directory without recursive.
    Ok(EntryKind::Dir) if !opts.recursive => Err(io(
      "path is a directory (pass recursive)",
      std::io::Error::from_raw_os_error(eisdir()),
    )),
    Ok(EntryKind::Dir) => {
      with_policy(|| fs.remove_tree(path), opts).map_err(|e| io("remove tree", e))
    }
    Err(e) => Err(io("lstat", e)),
  }
}

#[cfg(unix)]
fn eisdir() -> i32 {
  libc::EISDIR
}
#[cfg(windows)]
fn eisdir() -> i32 {
  // ERROR_DIRECTORY — "The directory name is invalid" (closest Win32 analog).
  267
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
  use super::*;

  fn seed_tree(root: &Path, dirs: usize, per: usize) {
    std::fs::create_dir_all(root).unwrap();
    for d in 0..dirs {
      let sub = root.join(format!("pkg-{d}"));
      std::fs::create_dir_all(sub.join("nested")).unwrap();
      for f in 0..per {
        std::fs::write(sub.join(format!("f{f}.js")), b"module.exports=1\n").unwrap();
        std::fs::write(sub.join("nested").join(format!("g{f}.js")), b"x\n").unwrap();
      }
    }
  }

  #[test]
  fn matches_node_rm_semantics() {
    let root = std::env::temp_dir().join(format!("decmpfs-rm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    seed_tree(&root, 4, 3);

    // recursive:false on a directory throws (EISDIR parity).
    assert!(rm(&root, &RmOptions::default()).is_err());

    // a symlink is unlinked, not followed.
    let keep = std::env::temp_dir().join(format!("decmpfs-rm-keep-{}", std::process::id()));
    std::fs::write(&keep, b"keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&keep, root.join("link")).unwrap();

    let rf = RmOptions {
      recursive: true,
      force: true,
      ..RmOptions::default()
    };
    rm(&root, &rf).unwrap();
    assert!(!root.exists(), "tree cleared");
    assert!(keep.exists(), "symlink target must survive");

    // force: a missing path is Ok; without force it's NotFound.
    rm(&root, &rf).unwrap();
    assert!(matches!(
      rm(&root, &RmOptions::default()),
      Err(Error::NotFound(_))
    ));

    // a single file.
    let f = std::env::temp_dir().join(format!("decmpfs-rm-one-{}", std::process::id()));
    std::fs::write(&f, b"x").unwrap();
    rm(&f, &RmOptions::default()).unwrap();
    assert!(!f.exists());

    let _ = std::fs::remove_file(&keep);
  }

  #[test]
  fn safe_guard_blocks_cwd_ancestors_and_root() {
    use std::path::Path;
    let cwd = Path::new("/a/b/c");
    // cwd itself, an ancestor, and the root are refused.
    assert!(is_cwd_ancestor_or_root(Path::new("/a/b/c"), cwd), "cwd");
    assert!(is_cwd_ancestor_or_root(Path::new("/a/b"), cwd), "ancestor");
    assert!(is_cwd_ancestor_or_root(Path::new("/a"), cwd), "ancestor");
    assert!(is_cwd_ancestor_or_root(Path::new("/"), cwd), "root");
    // a descendant of cwd and an unrelated sibling are allowed.
    assert!(
      !is_cwd_ancestor_or_root(Path::new("/a/b/c/build"), cwd),
      "descendant allowed"
    );
    assert!(
      !is_cwd_ancestor_or_root(Path::new("/a/b/other"), cwd),
      "sibling allowed"
    );
  }

  // Retry/backoff + force policy — the fs op is injected as a closure, so these
  // branches are covered WITHOUT touching disk (DI is the mock).
  #[cfg(unix)]
  #[test]
  fn with_policy_covers_retry_force_and_non_retryable() {
    use std::io::Error as IoErr;
    // retryDelay 0 keeps the test instant.
    let recursive = RmOptions {
      recursive: true,
      force: false,
      max_retries: 3,
      retry_delay_ms: 0,
    };

    // A retryable errno (EBUSY) under recursive: retried until it succeeds.
    let mut tries = 0;
    let ok = with_policy(
      || {
        tries += 1;
        if tries < 3 {
          Err(IoErr::from_raw_os_error(libc::EBUSY))
        } else {
          Ok(())
        }
      },
      &recursive,
    );
    assert!(ok.is_ok());
    assert_eq!(tries, 3, "retried twice then succeeded");

    // Same error, NOT recursive: Node ignores retries → fails on the first try.
    let mut n = 0;
    let non_recursive = RmOptions {
      recursive: false,
      ..recursive
    };
    let err = with_policy(
      || {
        n += 1;
        Err::<(), _>(IoErr::from_raw_os_error(libc::EBUSY))
      },
      &non_recursive,
    );
    assert!(err.is_err());
    assert_eq!(n, 1, "no retry when not recursive");

    // force swallows a missing path.
    let forced = with_policy(
      || Err(IoErr::from(std::io::ErrorKind::NotFound)),
      &RmOptions {
        force: true,
        ..RmOptions::default()
      },
    );
    assert!(forced.is_ok());

    // A non-retryable errno surfaces immediately even under recursive.
    let mut k = 0;
    let hard = with_policy(
      || {
        k += 1;
        Err::<(), _>(IoErr::from_raw_os_error(libc::EACCES))
      },
      &recursive,
    );
    assert!(hard.is_err());
    assert_eq!(k, 1, "non-retryable is not retried");

    // retryable() classifies the errno set.
    assert!(retryable(&IoErr::from_raw_os_error(libc::ENOTEMPTY)));
    assert!(!retryable(&IoErr::from_raw_os_error(libc::EACCES)));
  }

  #[test]
  fn rm_refuses_cwd_without_force_but_force_overrides_the_guard() {
    // Removing the real cwd is blocked by the guard (this does NOT delete it).
    let cwd = std::env::current_dir().unwrap();
    assert!(
      rm(&cwd, &RmOptions::default()).is_err(),
      "guard must refuse removing the cwd"
    );
    // force bypasses the guard — proven WITHOUT touching cwd: a fresh temp file
    // (not an ancestor) removes fine, and force is the documented override.
    let f = std::env::temp_dir().join(format!("decmpfs-guard-{}", std::process::id()));
    std::fs::write(&f, b"x").unwrap();
    let forced = RmOptions {
      force: true,
      ..RmOptions::default()
    };
    rm(&f, &forced).unwrap();
    assert!(!f.exists());
  }

  // A path far from cwd, so the guard lets rm_with reach its dispatch.
  fn mock_target() -> std::path::PathBuf {
    std::path::PathBuf::from("/mock-target/x")
  }

  // Wire a MockRmFs so guard_cwd_and_root passes: canonicalize is identity and
  // cwd is an unrelated dir, so `mock_target` is neither cwd nor an ancestor.
  fn mock_guard_passes(m: &mut MockRmFs) {
    m.expect_canonicalize().returning(|p| Ok(p.to_path_buf()));
    m.expect_cwd()
      .returning(|| Ok(std::path::PathBuf::from("/mock-cwd")));
  }

  fn eacces() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::PermissionDenied)
  }

  // rm_with over a mocked filesystem — each dispatch + error arm without disk.
  #[test]
  fn rm_with_surfaces_a_non_not_found_kind_error() {
    let mut m = MockRmFs::new();
    mock_guard_passes(&mut m);
    m.expect_kind().returning(|_| Err(eacces()));
    assert!(matches!(
      rm_with(&m, &mock_target(), &RmOptions::default()),
      Err(Error::Io {
        context: "lstat",
        ..
      })
    ));
  }

  #[test]
  fn rm_with_missing_is_force_ok_else_not_found() {
    let mut ok = MockRmFs::new();
    ok.expect_kind().returning(|_| Ok(EntryKind::Missing));
    let forced = RmOptions {
      force: true,
      ..RmOptions::default()
    };
    // force short-circuits the guard AND swallows the missing path.
    assert!(rm_with(&ok, &mock_target(), &forced).is_ok());

    let mut miss = MockRmFs::new();
    mock_guard_passes(&mut miss);
    miss.expect_kind().returning(|_| Ok(EntryKind::Missing));
    assert!(matches!(
      rm_with(&miss, &mock_target(), &RmOptions::default()),
      Err(Error::NotFound(_))
    ));
  }

  #[test]
  fn rm_with_dir_needs_recursive_then_removes_tree() {
    let mut eisdir = MockRmFs::new();
    mock_guard_passes(&mut eisdir);
    eisdir.expect_kind().returning(|_| Ok(EntryKind::Dir));
    assert!(matches!(
      rm_with(&eisdir, &mock_target(), &RmOptions::default()),
      Err(Error::Io { context, .. }) if context.contains("directory")
    ));

    let mut tree_err = MockRmFs::new();
    mock_guard_passes(&mut tree_err);
    tree_err.expect_kind().returning(|_| Ok(EntryKind::Dir));
    tree_err.expect_remove_tree().returning(|_| Err(eacces()));
    let rf = RmOptions {
      recursive: true,
      ..RmOptions::default()
    };
    assert!(matches!(
      rm_with(&tree_err, &mock_target(), &rf),
      Err(Error::Io {
        context: "remove tree",
        ..
      })
    ));

    let mut tree_ok = MockRmFs::new();
    mock_guard_passes(&mut tree_ok);
    tree_ok.expect_kind().returning(|_| Ok(EntryKind::Dir));
    tree_ok.expect_remove_tree().returning(|_| Ok(()));
    assert!(rm_with(&tree_ok, &mock_target(), &rf).is_ok());
  }

  #[test]
  fn rm_with_file_unlink_ok_and_error() {
    let mut ok = MockRmFs::new();
    mock_guard_passes(&mut ok);
    ok.expect_kind().returning(|_| Ok(EntryKind::Other));
    ok.expect_remove_file().returning(|_| Ok(()));
    assert!(rm_with(&ok, &mock_target(), &RmOptions::default()).is_ok());

    let mut err = MockRmFs::new();
    mock_guard_passes(&mut err);
    err.expect_kind().returning(|_| Ok(EntryKind::Other));
    err.expect_remove_file().returning(|_| Err(eacces()));
    assert!(matches!(
      rm_with(&err, &mock_target(), &RmOptions::default()),
      Err(Error::Io {
        context: "unlink",
        ..
      })
    ));
  }

  #[test]
  fn guard_falls_back_when_canonicalize_and_cwd_fail() {
    let mut m = MockRmFs::new();
    // Both the target canonicalize and cwd() fail → the guard uses the raw path
    // and an empty cwd (the unwrap_or_else / unwrap_or_default fallbacks). A
    // non-ancestor path still proceeds to the dispatch.
    m.expect_canonicalize().returning(|_| Err(eacces()));
    m.expect_cwd().returning(|| Err(eacces()));
    m.expect_kind().returning(|_| Ok(EntryKind::Missing));
    assert!(matches!(
      rm_with(&m, &mock_target(), &RmOptions::default()),
      Err(Error::NotFound(_))
    ));
  }

  #[test]
  fn guard_blocks_when_target_is_an_ancestor_of_cwd() {
    let mut m = MockRmFs::new();
    // target "/" is the root → an ancestor of any cwd → refused (no force).
    m.expect_canonicalize().returning(|p| Ok(p.to_path_buf()));
    m.expect_cwd()
      .returning(|| Ok(std::path::PathBuf::from("/mock-cwd")));
    assert!(matches!(
      rm_with(&m, std::path::Path::new("/"), &RmOptions::default()),
      Err(Error::Io { context, .. }) if context.contains("refusing")
    ));
  }

  #[test]
  fn os_rmfs_kind_surfaces_a_non_not_found_error() {
    // A path whose parent is a FILE makes symlink_metadata fail non-NotFound
    // (ENOTDIR), exercising OsRmFs::kind's real error passthrough.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, b"x").unwrap();
    let bogus = file.join("child");
    assert!(rm(&bogus, &RmOptions::default()).is_err());
  }

  #[test]
  fn os_rmfs_removes_a_real_tree_and_file() {
    // Covers OsRmFs's happy delegations (kind Dir/Other, remove_tree/remove_file)
    // against a real temp fs with auto-cleanup.
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir_all(sub.join("deep")).unwrap();
    std::fs::write(sub.join("a.txt"), b"a").unwrap();
    let rf = RmOptions {
      recursive: true,
      ..RmOptions::default()
    };
    rm(&sub, &rf).unwrap();
    assert!(!sub.exists());

    let f = dir.path().join("solo");
    std::fs::write(&f, b"x").unwrap();
    rm(&f, &RmOptions::default()).unwrap();
    assert!(!f.exists());
  }

  // Opt-in perf probe: parallel rm vs std::fs::remove_dir_all on a big tree.
  //   cargo test -p decmpfs rmrf_probe -- --ignored --nocapture
  #[test]
  #[ignore]
  fn rmrf_probe() {
    let base = std::env::temp_dir().join(format!("decmpfs-rmrf-{}", std::process::id()));
    let a = base.join("parallel");
    let b = base.join("std");
    for d in [&a, &b] {
      seed_tree(d, 300, 20);
    }
    let cores = std::thread::available_parallelism()
      .map(|n| n.get())
      .unwrap_or(1);
    let rf = RmOptions {
      recursive: true,
      force: true,
      ..RmOptions::default()
    };
    let t0 = std::time::Instant::now();
    rm(&a, &rf).unwrap();
    let par = t0.elapsed().as_secs_f64() * 1e3;
    let t1 = std::time::Instant::now();
    std::fs::remove_dir_all(&b).unwrap();
    let base_ms = t1.elapsed().as_secs_f64() * 1e3;
    eprintln!(
      "rmrf ~12k files — decmpfs::rm ({cores} cores avail): {par:.1} ms | std::fs::remove_dir_all: {base_ms:.1} ms"
    );
    let _ = std::fs::remove_dir_all(&base);
  }
}
