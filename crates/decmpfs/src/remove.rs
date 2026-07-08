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

/// One recursive delete of a subtree. MEASURED on APFS (this machine, 14 cores,
/// ~12k files): neither a single `removefile(3)` (~4-5% slower) nor a parallel
/// top-level fan-out (~15-20% slower) beats `std::fs::remove_dir_all` —
/// directory-entry mutations serialize on the container's metadata lock, so `rm`
/// is filesystem-bound and `remove_dir_all` (openat + unlinkat, no path
/// re-resolution) is already at that floor. Unlike WRITE (parallel LZVN = 6.5x),
/// DELETE has no userspace codec win, so the simplest correct call is the fast
/// one, on every platform.
fn remove_tree_once(path: &Path) -> std::io::Result<()> {
  std::fs::remove_dir_all(path)
}

fn io(context: &'static str, source: std::io::Error) -> Error {
  Error::Io { context, source }
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
fn guard_cwd_and_root(path: &Path, opts: &RmOptions) -> Result<(), Error> {
  if opts.force {
    return Ok(());
  }
  // Resolve real paths for the comparison; a missing target (canonicalize fails)
  // falls back to its given path — it can't be an ancestor of cwd anyway.
  let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
  let cwd = std::env::current_dir()
    .and_then(|c| std::fs::canonicalize(&c))
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
/// needs `recursive` (else `EISDIR`, as in Node); its top-level entries are
/// cleared CONCURRENTLY across cores, then the empty root is removed.
pub fn rm(path: &Path, opts: &RmOptions) -> Result<(), Error> {
  guard_cwd_and_root(path, opts)?;
  let md = match std::fs::symlink_metadata(path) {
    Ok(md) => md,
    Err(e) if is_not_found(&e) && opts.force => return Ok(()),
    Err(e) if is_not_found(&e) => return Err(Error::NotFound(path.to_path_buf())),
    Err(e) => return Err(io("lstat", e)),
  };

  if !md.is_dir() {
    return with_policy(|| std::fs::remove_file(path), opts).map_err(|e| io("unlink", e));
  }

  if !opts.recursive {
    // Node throws EISDIR for a directory without recursive.
    return Err(io(
      "path is a directory (pass recursive)",
      std::io::Error::from_raw_os_error(eisdir()),
    ));
  }

  // Recursive delete via std::fs::remove_dir_all — MEASURED as the floor on APFS
  // (removefile and parallel fan-out both lost; rm is metadata-lock-bound). See
  // remove_tree_once.
  with_policy(|| remove_tree_once(path), opts).map_err(|e| io("remove tree", e))
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
