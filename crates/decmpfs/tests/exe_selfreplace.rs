//! End-to-end proof of the self-replacing runtime (feature `exe`, unix):
//! pack a real payload into the `decmpfs-stub` binary, RUN the packed stub, and
//! assert it (a) produced the payload's own output, (b) left the on-disk file
//! FS-compressed when the filesystem supports it, and (c) on a second run takes
//! the plain-exec path (the stub is gone — the file IS the payload now).
//!
//! Cargo sets `CARGO_BIN_EXE_decmpfs-stub` for integration tests to the built
//! stub path; the `exe` feature is what builds that bin, so this file is empty
//! without it.
#![cfg(all(unix, feature = "exe"))]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn on_disk_bytes(path: &Path) -> u64 {
  use std::os::unix::fs::MetadataExt;
  std::fs::metadata(path)
    .expect("stat")
    .blocks()
    .saturating_mul(512)
}

#[test]
fn packed_stub_materializes_then_execs_payload_and_compresses_when_supported() {
  let stub = env!("CARGO_BIN_EXE_decmpfs-stub");
  let dir = std::env::temp_dir().join(format!("decmpfs-e2e-{}", std::process::id()));
  std::fs::create_dir_all(&dir).expect("scratch dir");

  // The payload is a shell script that echoes a marker + its args, padded with
  // a big, highly-compressible comment — large enough (~1.6 MB) that the
  // on-disk win is unambiguous against APFS's block rounding.
  let payload = dir.join("payload.sh");
  let filler = "# ".to_string() + &"decmpfs ".repeat(200_000);
  std::fs::write(
    &payload,
    format!("#!/bin/sh\n{filler}\necho \"MATERIALIZED $*\"\n"),
  )
  .expect("write payload");
  std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755))
    .expect("chmod payload");

  // Pack it into a copy of the stub.
  let packed = dir.join("packed");
  let outcome = decmpfs::exe::pack_executable_with_stub(
    Path::new(stub),
    &payload,
    &packed,
    &decmpfs::Gate::any(),
  )
  .expect("pack succeeds");
  assert!(
    matches!(outcome, decmpfs::exe::PackOutcome::Packed { .. }),
    "Gate::any() must pack, got {outcome:?}",
  );
  let compression_supported = matches!(
    decmpfs::probe(&packed),
    Ok(decmpfs::Support::Supported | decmpfs::Support::AlreadyCompressed)
  );

  // First run: the stub materializes the payload over itself and execs it.
  let first = Command::new(&packed)
    .arg("hello")
    .output()
    .expect("run packed stub");
  assert!(
    first.status.success(),
    "first run failed: {}",
    String::from_utf8_lossy(&first.stderr),
  );
  assert_eq!(
    String::from_utf8_lossy(&first.stdout).trim(),
    "MATERIALIZED hello",
    "first run must exec the materialized payload",
  );

  // The file on disk is now the payload. APFS and btrfs should compress it;
  // unsupported filesystems such as a stock Linux CI runner's ext4 still
  // exercise the materialize-and-exec fallback and intentionally land plain.
  let logical = std::fs::metadata(&packed).expect("stat").len();
  let physical = on_disk_bytes(&packed);
  if compression_supported {
    assert!(
      physical < logical,
      "materialized file should be FS-compressed: {physical} on disk vs {logical} logical",
    );
  }

  // Second run: the file IS the payload now (no packed section), so the stub
  // runtime is never re-entered — it just runs as the plain executable.
  let second = Command::new(&packed)
    .arg("again")
    .output()
    .expect("run materialized file");
  assert!(second.status.success());
  assert_eq!(
    String::from_utf8_lossy(&second.stdout).trim(),
    "MATERIALIZED again",
  );
  // The now-plain file carries no packed payload.
  assert!(
    !decmpfs::exe::contains_payload(&packed),
    "after materialize the file carries no packed payload",
  );

  std::fs::remove_dir_all(&dir).ok();
}
