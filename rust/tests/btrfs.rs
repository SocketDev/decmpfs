//! Real-filesystem test for the btrfs backend. Skips unless DECMPFS_BTRFS_DIR
//! points at a mounted btrfs (the `ci/` Docker harness + CI btrfs job set it);
//! never runs on a dev macOS box. Exercises the PUBLIC surface: probe /
//! compress_file. The on-disk shrink is decided by FIEMAP's ENCODED flag, not
//! st_blocks (btrfs reports the logical size via st_blocks).
#![allow(clippy::print_stderr)] // CI diagnostics under --nocapture; not product code

use std::fs;
use std::path::PathBuf;

use decmpfs::{compress_file, probe, Outcome, Support};

mod common;
use common::fake_addon;

fn btrfs_dir() -> Option<PathBuf> {
  std::env::var_os("DECMPFS_BTRFS_DIR").map(PathBuf::from)
}

#[test]
fn compress_file_applies_flag_round_trips_and_stays_loadable() {
  let Some(dir) = btrfs_dir() else {
    eprintln!("skip: DECMPFS_BTRFS_DIR unset");
    return;
  };
  let path = dir.join("raw.node");
  let raw = fake_addon();
  fs::write(&path, &raw).unwrap();

  assert_eq!(probe(&path).unwrap(), Support::Supported, "btrfs detected");

  match compress_file(&path).unwrap() {
    Outcome::Compressed { before, after } => {
      eprintln!("FIEMAP-confirmed compressed (st_blocks {before} -> {after})");
    }
    other => panic!("expected Compressed (FIEMAP ENCODED), got {other:?}"),
  }

  // Transparent: reading back yields the identical bytes (still loadable).
  assert_eq!(fs::read(&path).unwrap(), raw);

  // Flag round-trip: the first call set FS_COMPR_FL, so a second short-circuits.
  assert!(
    matches!(
      compress_file(&path).unwrap(),
      Outcome::AlreadyCompressed { .. }
    ),
    "second call must detect the compress flag set by the first"
  );
}

#[test]
fn read_only_filesystem_is_skipped_not_errored() {
  let Some(dir) = std::env::var_os("DECMPFS_BTRFS_RO_DIR").map(PathBuf::from) else {
    eprintln!("skip: DECMPFS_BTRFS_RO_DIR unset");
    return;
  };
  let path = dir.join("ro.node");
  // Detection still reports Supported (it IS btrfs); the write can't happen on a
  // read-only mount → fail-soft turns that into Skipped, never an Err.
  match compress_file(&path).unwrap() {
    Outcome::Skipped { reason } => eprintln!("read-only -> Skipped({reason:?})"),
    other => panic!("expected Skipped on a read-only fs, got {other:?}"),
  }
}
