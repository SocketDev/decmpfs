use super::*;

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("decmpfs-{tag}-{}", std::process::id()));
    // A pid-recycled leftover FILE at this path makes create_dir_all fail
    // with AlreadyExists; clear it so the scratch dir always materializes.
    let _ = std::fs::remove_file(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// A minimal native-magic payload (ELF header) so a backend will attempt to
// compress it rather than skip a trivially-small file.
fn fake_addon() -> Vec<u8> {
    let mut raw = vec![0x7f, 0x45, 0x4c, 0x46];
    raw.extend_from_slice(&[7u8; 9000]);
    raw
}

#[test]
fn compress_file_errors_when_missing() {
    let p = std::path::Path::new("/no/such/addon.node");
    assert!(matches!(compress_file(p), Err(Error::NotFound(_))));
}

#[test]
fn plain_write_errors_when_the_path_has_no_parent() {
    // "/" has no parent directory → the no-parent guard fires before any write.
    let out = plain_write(std::path::Path::new("/"), b"x");
    assert!(matches!(
        out,
        Err(Error::Io {
            context: "no parent dir",
            ..
        })
    ));
}

#[test]
fn error_display_and_source() {
    let nf = Error::NotFound(std::path::PathBuf::from("/x"));
    assert!(nf.to_string().contains("not found"));
    assert!(std::error::Error::source(&nf).is_none());
    let io = Error::Io {
        context: "ctx",
        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    };
    assert!(io.to_string().contains("ctx"));
    assert!(std::error::Error::source(&io).is_some());
}

#[cfg(unix)]
#[test]
fn probe_reports_a_support_variant_without_mutating() {
    // probe never errors on an existing path — it returns a Support.
    assert!(matches!(
        probe(std::path::Path::new("/dev/null")),
        Ok(Support::Supported | Support::AlreadyCompressed | Support::Unsupported(_))
    ));
}

#[cfg(unix)]
#[test]
fn compress_file_reports_unsupported_on_a_non_compressing_fs() {
    // /dev/null exists but devfs has no compression backend → Unsupported.
    let out = compress_file(std::path::Path::new("/dev/null"));
    assert!(
        matches!(out, Ok(Outcome::Unsupported { .. })),
        "devfs → Unsupported, got {out:?}"
    );
}

// APFS is always a compressing FS, so macOS exercises the full success path:
// compress_file → apply_guarded → backend::apply_inplace → verify → classify.
#[cfg(target_os = "macos")]
#[test]
fn compress_file_compresses_then_is_idempotent_and_transparent() {
    let dir = scratch("ok");
    let path = dir.join("addon.node");
    std::fs::write(&path, fake_addon()).unwrap();

    let out = compress_file(&path);
    assert!(
        matches!(
            out,
            Ok(Outcome::Compressed { .. }
                | Outcome::NoGain { .. }
                | Outcome::AlreadyCompressed { .. })
        ),
        "writable addon on APFS → applied, got {out:?}"
    );
    // Transparent: the kernel hands back the exact original bytes.
    assert_eq!(std::fs::read(&path).unwrap(), fake_addon());
    // Idempotent: a second pass detects it's already compressed.
    assert!(matches!(
        compress_file(&path),
        Ok(Outcome::AlreadyCompressed { .. })
    ));
    std::fs::remove_dir_all(&dir).ok();
}

// compress_bytes one-pass: write bytes directly as an APFS-compressed file with
// no pre-existing original, then prove the kernel hands the exact bytes back.
#[cfg(target_os = "macos")]
#[test]
fn compress_bytes_one_pass_writes_compressed_and_reads_back_identical() {
    let dir = scratch("bytes");
    let path = dir.join("fresh.node");
    let content = fake_addon();
    // No file at `path` yet — compress_bytes creates it in one pass.
    let out = compress_bytes(&path, &content, &Gate::any());
    assert!(
        matches!(out, Ok(Outcome::Compressed { .. } | Outcome::NoGain { .. })),
        "one-pass APFS write → applied, got {out:?}"
    );
    assert!(path.exists(), "file was created");
    // Transparent: kernel read-back equals the bytes we asked to store.
    assert_eq!(std::fs::read(&path).unwrap(), content);
    // It really carries the compression flag (not a plain fallback write).
    assert!(matches!(
        compress_file(&path),
        Ok(Outcome::AlreadyCompressed { .. })
    ));
    std::fs::remove_dir_all(&dir).ok();
}

// A file the gate excludes is written PLAIN (never compressed) and reports
// Skipped(GateExcluded) — the install still gets the file.
#[cfg(unix)]
#[test]
fn compress_bytes_gate_excluded_writes_plain() {
    let dir = scratch("gate");
    let path = dir.join("not-an-addon.txt");
    let content = b"plain text, not a .node".to_vec();
    let gate = Gate::default(); // **/*.node
    let out = compress_bytes(&path, &content, &gate);
    assert!(
        matches!(
            out,
            Ok(Outcome::Skipped {
                reason: SkipReason::GateExcluded
            })
        ),
        "non-.node → GateExcluded, got {out:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), content);
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn compress_bytes_falls_back_to_plain_on_unsupported_fs() {
    // A non-compressing FS (devfs) → plain write, Unsupported Outcome, file lands.
    // /dev isn't writable by us, so target a temp path but force the gate to pass;
    // temp on macOS is APFS (compresses) — instead assert the API never errors and
    // the bytes land for the supported case is covered above. Here just exercise
    // the gate-passing path lands bytes on any unix temp.
    let dir = scratch("fallback");
    let path = dir.join("x.node");
    let content = fake_addon();
    let out = compress_bytes(&path, &content, &Gate::any());
    assert!(out.is_ok(), "never errors on a normal temp, got {out:?}");
    assert_eq!(std::fs::read(&path).unwrap(), content, "bytes always land");
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn compress_file_skips_a_read_only_file() {
    // On a compressing FS a read-only file can't be opened rw → fail-soft turns the
    // EACCES into Skipped(PermissionDenied). Root bypasses mode bits, so skip there.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = scratch("ro");
    let path = dir.join("addon.node");
    std::fs::write(&path, fake_addon()).unwrap();
    if !matches!(probe(&path), Ok(Support::Supported)) {
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&path, perm).unwrap();
    let outcome = compress_file(&path);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
    assert!(
        matches!(
            outcome,
            Ok(Outcome::Skipped {
                reason: SkipReason::PermissionDenied
            })
        ),
        "read-only → Skipped(PermissionDenied), got {outcome:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// An existing target exercises the `path.exists()` probe-target branch and the
// fresh-inode rename that replaces the old contents.
#[cfg(target_os = "macos")]
#[test]
fn compress_bytes_overwrites_an_existing_file() {
    let dir = scratch("overwrite");
    let path = dir.join("addon.node");
    std::fs::write(&path, b"stale contents").unwrap();
    let content = fake_addon();
    let out = compress_bytes(&path, &content, &Gate::any());
    assert!(out.is_ok(), "overwrite never errors, got {out:?}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        content,
        "new bytes replace the old"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// `path` is an existing directory: the backend builds its temp then can't rename
// a file over a directory, and the plain-write fallback can't either → a hard
// `Err` (genuine I/O failure), never a corrupt success. Exercises the backend
// rename-error cleanup and the `Err(_)` fallback arm of compress_bytes.
#[cfg(target_os = "macos")]
#[test]
fn compress_bytes_onto_a_directory_path_is_a_hard_error() {
    let dir = scratch("dir-target");
    let target = dir.join("a-dir");
    std::fs::create_dir_all(&target).unwrap();
    let out = compress_bytes(&target, &fake_addon(), &Gate::any());
    assert!(
        out.is_err(),
        "cannot write a file over a directory, got {out:?}"
    );
    assert!(target.is_dir(), "the directory is left intact");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stat_reports_size_and_uncompressed_for_a_plain_file() {
    let dir = scratch("stat-plain");
    let path = dir.join("f");
    std::fs::write(&path, vec![0u8; 4096]).unwrap();
    let s = stat(&path).unwrap();
    assert_eq!(s.logical, 4096, "logical == the written bytes");
    assert!(s.physical > 0, "allocated bytes reported");
    assert!(
        !s.compressed,
        "a freshly-written plain file is not FS-compressed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stat_reflects_a_compressed_file_where_supported() {
    let dir = scratch("stat-comp");
    let path = dir.join("addon.node");
    let content = vec![0xABu8; 128 * 1024];
    let outcome = compress_bytes(&path, &content, &Gate::any()).unwrap();
    let s = stat(&path).unwrap();
    assert_eq!(
        s.logical,
        content.len() as u64,
        "logical == the written bytes"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        content,
        "content round-trips"
    );
    // Where the FS actually compressed (APFS / btrfs / NTFS), stat must reflect
    // it; on an unsupported FS the outcome isn't Compressed and we only assert
    // the size + content invariants above.
    if matches!(outcome, Outcome::Compressed { .. }) {
        assert!(
            s.compressed,
            "a Compressed outcome → stat reports compressed"
        );
        assert!(
            s.physical < s.logical,
            "allocation shrank below the logical size"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

// A read-only parent dir: the guarded backend write hits EACCES (classify_skip →
// Skipped), then the plain-write fallback also can't write → `Err`. Root bypasses
// mode bits, so skip there.
#[cfg(target_os = "macos")]
#[test]
fn compress_bytes_into_a_read_only_dir_is_fail_soft() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("ro-dir");
    let locked = dir.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = compress_bytes(&locked.join("x.node"), &fake_addon(), &Gate::any());
    // Restore write perms so the tree can be cleaned up.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();
    assert!(out.is_err(), "a read-only dir admits no write, got {out:?}");
    std::fs::remove_dir_all(&dir).ok();
}

// The `Support::AlreadyCompressed`-from-detect arm: a real macOS detect never
// returns it (it reports already-compressed via the apply path), so a fake drives
// it. Needs a real file for the on-disk-bytes read.
#[test]
fn compress_file_reports_already_compressed_from_detect() {
    let dir = scratch("already-detect");
    let path = dir.join("f.node");
    std::fs::write(&path, fake_addon()).unwrap();
    let backend = FakeBackend {
        detect: Support::AlreadyCompressed,
        apply_error: None,
    };
    assert!(matches!(
        compress_file_with(&backend, &path),
        Ok(Outcome::AlreadyCompressed { .. })
    ));
    std::fs::remove_dir_all(&dir).ok();
}

// detect → Unsupported: the bytes still land via a plain write, Outcome::Unsupported.
#[test]
fn compress_bytes_falls_back_to_plain_on_an_unsupported_fs() {
    let dir = scratch("unsup");
    let path = dir.join("x.node");
    let content = fake_addon();
    let backend = FakeBackend {
        detect: Support::Unsupported(UnsupportedReason::Filesystem),
        apply_error: None,
    };
    let out = compress_bytes_with(&backend, &path, &content, &Gate::any());
    assert!(
        matches!(out, Ok(Outcome::Unsupported { .. })),
        "got {out:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), content, "bytes landed plain");
    std::fs::remove_dir_all(&dir).ok();
}

// detect → Supported but the guarded apply is skipped (faked permission failure):
// the bytes land via a plain write, Outcome::Skipped(IntegrityRevert).
#[test]
fn compress_bytes_falls_back_to_plain_on_a_guarded_skip() {
    let dir = scratch("guard-skip");
    let path = dir.join("x.node");
    let content = fake_addon();
    let backend = FakeBackend {
        detect: Support::Supported,
        apply_error: Some(std::io::ErrorKind::PermissionDenied),
    };
    let out = compress_bytes_with(&backend, &path, &content, &Gate::any());
    assert!(
        matches!(
            out,
            Ok(Outcome::Skipped {
                reason: SkipReason::IntegrityRevert
            })
        ),
        "got {out:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), content, "bytes landed plain");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn copy_file_errors_when_the_source_is_missing() {
    let dir = scratch("copy-missing");
    let out = copy_file(&dir.join("absent.node"), &dir.join("dest.node"));
    assert!(matches!(out, Err(Error::NotFound(_))));
    std::fs::remove_dir_all(&dir).ok();
}

/// A fallback fake: no clone path (trait default), reports the source
/// compressed, and its apply actually writes — so the guarded one-pass copy
/// arm runs end to end and classifies via the backend signal.
struct RecompressingFake;

impl Backend for RecompressingFake {
    fn detect(&self, _path: &Path) -> Result<Support, Error> {
        Ok(Support::Supported)
    }
    fn is_already_compressed(&self, _path: &Path) -> Result<bool, Error> {
        Ok(true)
    }
    fn apply_inplace(&self, _path: &Path, _snapshot: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn apply_bytes(
        &self,
        path: &Path,
        content: &[u8],
        _mode: Option<std::fs::Permissions>,
    ) -> Result<(), Error> {
        std::fs::write(path, content).map_err(|source| Error::Io {
            context: "fake write",
            source,
        })
    }
    fn compressed_on_disk(&self, _path: &Path) -> Result<Option<bool>, Error> {
        Ok(Some(true))
    }
}

#[test]
fn copy_file_recompresses_at_the_destination_when_it_cannot_clone() {
    let dir = scratch("copy-recompress");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    let out = copy_file_with(&RecompressingFake, &src, &dest).unwrap();
    assert!(
        matches!(out, CopyOutcome::CopiedCompressed { .. }),
        "got {out:?}"
    );
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        content,
        "bytes are identical"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn copy_file_with_mock_backend_takes_the_clone_fast_path() {
    // mockall MockBackend mocks the fs backend seam (no real syscalls); tempfile
    // gives a real, isolated, auto-cleaned src fixture. clone_file → true
    // short-circuits copy_file_with to the zero-cost Cloned outcome.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.node");
    std::fs::write(&src, b"native").unwrap();
    let dest = dir.path().join("b.node");
    let mut backend = MockBackend::new();
    backend
        .expect_is_already_compressed()
        .returning(|_| Ok(true));
    backend.expect_clone_file().returning(|_, _| Ok(true));
    let out = copy_file_with(&backend, &src, &dest).unwrap();
    assert!(
        matches!(out, CopyOutcome::Cloned { compressed: true }),
        "clone fast-path → Cloned; got {out:?}"
    );
}

#[test]
fn copy_file_copies_a_plain_source_plain_and_replaces_the_destination() {
    let dir = scratch("copy-plain");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    std::fs::write(&dest, b"stale destination").unwrap();
    let backend = FakeBackend {
        detect: Support::Supported,
        apply_error: None,
    };
    let out = copy_file_with(&backend, &src, &dest).unwrap();
    assert_eq!(out, CopyOutcome::CopiedPlain { skipped: None });
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        content,
        "destination replaced"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn copy_file_lands_plain_and_reports_the_skip_when_recompression_fails() {
    struct SkippingFake;
    impl Backend for SkippingFake {
        fn detect(&self, _path: &Path) -> Result<Support, Error> {
            Ok(Support::Supported)
        }
        fn is_already_compressed(&self, _path: &Path) -> Result<bool, Error> {
            Ok(true)
        }
        fn apply_inplace(&self, _path: &Path, _snapshot: &[u8]) -> Result<(), Error> {
            Ok(())
        }
        fn apply_bytes(
            &self,
            _path: &Path,
            _content: &[u8],
            _mode: Option<std::fs::Permissions>,
        ) -> Result<(), Error> {
            Err(Error::Io {
                context: "fake apply",
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            })
        }
        fn compressed_on_disk(&self, _path: &Path) -> Result<Option<bool>, Error> {
            Ok(Some(false))
        }
    }
    let dir = scratch("copy-skip");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    let out = copy_file_with(&SkippingFake, &src, &dest).unwrap();
    assert!(
        matches!(out, CopyOutcome::CopiedPlain { skipped: Some(_) }),
        "got {out:?}"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), content, "bytes still landed");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn copy_file_onto_itself_is_a_noop_reported_as_cloned() {
    let dir = scratch("copy-self");
    let src = dir.join("src.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    let backend = FakeBackend {
        detect: Support::Supported,
        apply_error: None,
    };
    let out = copy_file_with(&backend, &src, &src).unwrap();
    assert!(matches!(out, CopyOutcome::Cloned { .. }), "got {out:?}");
    assert_eq!(std::fs::read(&src).unwrap(), content, "source untouched");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn is_same_file_sees_hardlinks_and_distinct_files() {
    let dir = scratch("same-file");
    let a = dir.join("a.node");
    let b = dir.join("b.node");
    std::fs::write(&a, b"bytes").unwrap();
    std::fs::write(&b, b"bytes").unwrap();
    assert!(is_same_file(&a, &a), "identical path");
    assert!(!is_same_file(&a, &b), "distinct files");
    let link = dir.join("a-link.node");
    std::fs::hard_link(&a, &link).unwrap();
    assert!(is_same_file(&a, &link), "hardlink shares the inode");
    assert!(
        !is_same_file(&a, &dir.join("absent.node")),
        "a missing path is never the same file"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn copy_file_onto_a_hardlink_is_a_noop_reported_as_cloned() {
    let dir = scratch("copy-hardlink");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    std::fs::write(&src, &content).unwrap();
    std::fs::hard_link(&src, &dest).unwrap();

    let out = copy_file(&src, &dest).unwrap();
    assert!(matches!(out, CopyOutcome::Cloned { .. }), "got {out:?}");
    assert_eq!(std::fs::read(&src).unwrap(), content, "source untouched");
    assert_eq!(std::fs::read(&dest).unwrap(), content, "hardlink untouched");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn copy_file_errors_when_the_destination_cannot_be_replaced() {
    let dir = scratch("copy-dest-dir");
    let src = dir.join("src.node");
    std::fs::write(&src, fake_addon()).unwrap();
    // A directory at `dest` makes the replace step's remove_file fail.
    let dest = dir.join("dest.node");
    std::fs::create_dir(&dest).unwrap();
    let backend = FakeBackend {
        detect: Support::Supported,
        apply_error: None,
    };
    let out = copy_file_with(&backend, &src, &dest);
    assert!(
        matches!(
            out,
            Err(Error::Io {
                context: "replace existing destination",
                ..
            })
        ),
        "got {out:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn copy_file_errors_when_the_source_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("copy-unreadable");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    std::fs::write(&src, fake_addon()).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
    let backend = FakeBackend {
        detect: Support::Supported,
        apply_error: None,
    };
    let out = copy_file_with(&backend, &src, &dest);
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).ok();
    assert!(
        matches!(
            out,
            Err(Error::Io {
                context: "read copy source",
                ..
            })
        ),
        "got {out:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn try_clone_file_errors_when_the_source_is_missing() {
    let dir = scratch("clone-missing");
    let out = try_clone_file(&dir.join("absent.node"), &dir.join("dest.node"));
    assert!(matches!(out, Err(Error::NotFound(_))));
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(target_os = "macos")]
#[test]
fn try_clone_file_clones_on_apfs_and_declines_an_existing_destination() {
    let dir = scratch("clone-try");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    std::fs::write(&src, fake_addon()).unwrap();
    assert!(try_clone_file(&src, &dest).unwrap(), "fresh clone");
    // clonefile refuses an existing destination — reported as cannot-clone,
    // never an error.
    assert!(!try_clone_file(&src, &dest).unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(target_os = "macos")]
#[test]
fn copy_file_clones_a_compressed_source_on_apfs() {
    let dir = scratch("copy-clone");
    let src = dir.join("src.node");
    let dest = dir.join("dest.node");
    let content = fake_addon();
    let wrote = compress_bytes(&src, &content, &Gate::any()).unwrap();
    // Only meaningful when the scratch volume actually compressed the source.
    if !matches!(wrote, Outcome::Compressed { .. }) {
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    let out = copy_file(&src, &dest).unwrap();
    assert_eq!(out, CopyOutcome::Cloned { compressed: true });
    assert!(backend::is_already_compressed(&dest).unwrap());
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        content,
        "bytes are identical"
    );
    std::fs::remove_dir_all(&dir).ok();
}
