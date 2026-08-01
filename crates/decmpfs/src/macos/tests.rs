use super::*;

// The kernel-roundtrip oracle. decmpfs is undocumented — the only proof the
// format is right is that a normal read() returns identical bytes after apply.
#[test]
fn kernel_roundtrips_decmpfs() {
    let dir = std::env::temp_dir().join(format!("decmpfs-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.bin");
    // > 1 block (64 KiB) of compressible data, so the offset table + LZVN blocks
    // are both exercised.
    let mut raw = Vec::new();
    let pat = b"the quick brown fox decmpfs lzvn resource-fork oracle line ";
    while raw.len() < 2_000_000 {
        raw.extend_from_slice(pat);
    }
    std::fs::write(&path, &raw).unwrap();

    assert!(
        matches!(detect(&path).unwrap(), Support::Supported),
        "temp dir is local APFS/HFS+"
    );
    apply_inplace(&path, &raw).unwrap();
    assert!(is_already_compressed(&path).unwrap(), "UF_COMPRESSED set");
    assert_eq!(
        compressed_on_disk(&path).unwrap(),
        Some(true),
        "reports compressed"
    );
    // THE ORACLE: the kernel decompresses our resource fork on read().
    assert_eq!(
        std::fs::read(&path).unwrap(),
        raw,
        "kernel read-back must equal the original bytes"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn incremental_writer_streams_lzfse_blocks_into_a_kernel_readable_file() {
    let dir =
        std::env::temp_dir().join(format!("decmpfs-incremental-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("model.bin");
    let raw = b"incremental lzfse resource fork ".repeat((2 << 20) / 34 + 1);
    let mut writer = StreamingWriter::new(&path, raw.len()).unwrap();
    for chunk in raw.chunks(17_003) {
        writer.write_all(chunk).unwrap();
    }
    assert!(writer.finish().unwrap(), "compressible stream must win");
    assert!(is_already_compressed(&path).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), raw);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn incremental_writer_reconstructs_plain_bytes_when_compression_loses() {
    let dir = std::env::temp_dir().join(format!(
        "decmpfs-incremental-fallback-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("random.bin");
    let mut raw = Vec::with_capacity(2 << 20);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    while raw.len() < (2 << 20) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        raw.extend_from_slice(&x.to_le_bytes());
    }
    let mut writer = StreamingWriter::new(&path, raw.len()).unwrap();
    for chunk in raw.chunks(17_003) {
        writer.write_all(chunk).unwrap();
    }
    assert!(
        !writer.finish().unwrap(),
        "incompressible stream stays plain"
    );
    assert!(!is_already_compressed(&path).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), raw);
    std::fs::remove_dir_all(&dir).ok();
}

// Opt-in perf probe (ignored in CI — timing is machine-specific). Reports the
// decmpfs write time for a ~40 MiB addon; run serial vs parallel with
//   cargo test -p decmpfs write_time -- --ignored --nocapture
//   DECMPFS_SERIAL=1 cargo test -p decmpfs write_time -- --ignored --nocapture
#[test]
#[ignore]
fn write_time_probe() {
    let dir = std::env::temp_dir().join(format!("decmpfs-time-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("addon.node");
    let mut raw: Vec<u8> = Vec::with_capacity(40 << 20);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    while raw.len() < (40 << 20) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        raw.extend_from_slice(&x.to_le_bytes());
        raw.extend_from_slice(b"native addon .node text segment padding ");
    }
    if !matches!(detect(&dir), Ok(Support::Supported)) {
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let serial = std::env::var_os("DECMPFS_SERIAL").is_some();
    let start = std::time::Instant::now();
    apply_bytes(&path, &raw, None).unwrap();
    let ms = start.elapsed().as_secs_f64() * 1e3;
    eprintln!(
        "decmpfs write {}MiB — {} ({} cores): {:.1} ms",
        raw.len() >> 20,
        if serial { "serial" } else { "parallel" },
        cores,
        ms,
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn detect_and_flags_error_on_a_missing_path() {
    let p = std::path::Path::new("/no/such/decmpfs/path/x.bin");
    assert!(detect(p).is_err(), "statfs of a missing path errors");
    assert!(
        is_already_compressed(p).is_err(),
        "lstat of a missing path errors"
    );
}

#[test]
fn apply_inplace_errors_when_the_file_cannot_be_read() {
    // A 0-perm file: apply_inplace's initial read fails before any apply. Root
    // bypasses mode bits, so skip there.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("decmpfs-noread-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.bin");
    let content = b"\x7fELF unreadable";
    std::fs::write(&path, content).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    // apply_inplace no longer reads the file (the caller passes the snapshot it
    // already holds); the fail-soft guard is now the W_OK access check, which
    // rejects a file we cannot write before the temp+rename would replace it.
    let out = apply_inplace(&path, content);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
    assert!(matches!(
        out,
        Err(Error::Io {
            context: "access",
            ..
        })
    ));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn setxattr_errors_on_a_missing_path() {
    let out = setxattr(c"/no/such/decmpfs/path", c"com.apple.decmpfs", b"x");
    assert!(matches!(
        out,
        Err(Error::Io {
            context: "setxattr",
            ..
        })
    ));
}

#[test]
fn compress_block_returns_none_for_empty_input() {
    // libcompression encodes zero bytes to nothing → the n == 0 guard returns None.
    let scratch_len = unsafe { compression_encode_scratch_buffer_size(COMPRESSION_LZVN) };
    let mut scratch = vec![0u8; scratch_len];
    assert!(compress_block(b"", &mut scratch).is_none());
}

#[test]
fn build_resource_fork_zero_length_is_no_gain() {
    assert!(
        build_resource_fork(&[]).unwrap().is_none(),
        "a resource fork cannot make an empty file smaller"
    );
}

#[test]
fn streaming_threshold_keeps_vite_native_addons_on_the_fast_path() {
    // The largest Darwin ARM64 addon in the 2026-07-16 Vite-family sample was
    // SWC at 36.563 MiB. The complete observed set must stay comfortably below
    // the in-memory cutoff, while the first byte beyond it streams.
    assert!(!should_stream_resource_fork(37 << 20, STREAMING_THRESHOLD));
    assert!(!should_stream_resource_fork(
        STREAMING_THRESHOLD,
        STREAMING_THRESHOLD
    ));
    assert!(should_stream_resource_fork(
        STREAMING_THRESHOLD + 1,
        STREAMING_THRESHOLD
    ));
}

#[test]
fn kernel_roundtrips_forced_streaming_lzfse() {
    let dir = std::env::temp_dir().join(format!("decmpfs-streaming-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.bin");
    let raw = b"streamed lzfse decmpfs resource fork oracle ".repeat((2 << 20) / 46 + 1);
    std::fs::write(&path, &raw).unwrap();

    if matches!(detect(&path).unwrap(), Support::Supported) {
        apply_bytes_with_streaming_threshold(&path, &raw, None, 0).unwrap();
        assert!(is_already_compressed(&path).unwrap(), "UF_COMPRESSED set");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            raw,
            "kernel read-back must decode the streamed type-12 resource fork"
        );

        let cpath = cstring(&path).unwrap();
        let mut header = [0u8; 16];
        let len = unsafe {
            libc::getxattr(
                cpath.as_ptr(),
                c"com.apple.decmpfs".as_ptr(),
                header.as_mut_ptr().cast(),
                header.len(),
                0,
                XATTR_NOFOLLOW | 0x0020, // XATTR_SHOWCOMPRESSION
            )
        };
        assert_eq!(len, header.len() as isize);
        assert_eq!(u32::from_le_bytes(header[4..8].try_into().unwrap()), 12);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn in_memory_path_falls_back_to_lzfse_when_lzvn_has_no_gain() {
    // Skewed symbol frequencies give LZFSE's entropy coder something to exploit
    // without manufacturing the repeated strings that LZVN specializes in.
    let mut raw = Vec::with_capacity(1 << 20);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    while raw.len() < raw.capacity() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        raw.push(if x.is_multiple_of(4) {
            0
        } else {
            (x >> 32) as u8
        });
    }
    assert!(
        build_resource_fork_with_codec(&raw, Codec::Lzvn)
            .unwrap()
            .is_none(),
        "fixture must reach the fallback"
    );
    let candidate = build_in_memory_resource_fork(&raw)
        .unwrap()
        .expect("LZFSE should exploit the skewed symbols");
    assert_eq!(candidate.codec, Codec::Lzfse);
    assert!(candidate.bytes.len() < raw.len());
}

#[test]
fn build_resource_fork_last_offset_equals_length() {
    // Invariant across sizes that actually encode: the final table offset equals
    // the total blob length. (Tiny/incompressible inputs return None — the codec
    // declines — which is a separate, correct path.)
    for size in [512usize, BLOCK, BLOCK + 1, BLOCK * 3 + 7] {
        let raw = vec![0x41u8; size];
        let Some(rf) = build_resource_fork(&raw).unwrap() else {
            continue;
        };
        let num_blocks = size.div_ceil(BLOCK);
        let last_idx = num_blocks * 4; // offset[num_blocks] is the last entry
        let last = u32::from_le_bytes(rf[last_idx..last_idx + 4].try_into().unwrap()) as usize;
        assert_eq!(last, rf.len(), "size {size}: last offset != buffer length");
    }
}

#[test]
fn cstring_rejects_an_interior_nul() {
    use std::os::unix::ffi::OsStrExt;
    let p = std::path::Path::new(std::ffi::OsStr::from_bytes(b"a\0b"));
    assert!(cstring(p).is_err());
}

#[test]
fn detect_rejects_a_non_apfs_filesystem() {
    // /dev is devfs (local, but not apfs/hfs) → Unsupported(Filesystem).
    assert!(matches!(
        detect(std::path::Path::new("/dev")),
        Ok(Support::Unsupported(UnsupportedReason::Filesystem))
    ));
}

#[test]
fn classify_fs_covers_every_branch() {
    // Non-local (e.g. a network mount) — no real mount needed.
    assert!(matches!(
        classify_fs(false, b"nfs"),
        Support::Unsupported(UnsupportedReason::NetworkOrOverlay)
    ));
    assert!(matches!(classify_fs(true, b"apfs"), Support::Supported));
    assert!(matches!(classify_fs(true, b"hfs"), Support::Supported));
    assert!(matches!(
        classify_fs(true, b"ext4"),
        Support::Unsupported(UnsupportedReason::Filesystem)
    ));
}

#[test]
fn resource_fork_plan_accepts_raw_files_beyond_the_old_limit() {
    // The raw byte count is stored as u64. Only resource-fork offsets are u32,
    // so a >3.9 GB input is valid whenever its encoded fork fits in u32.
    let raw_len = 4_100_000_000usize;
    let num_blocks = raw_len.div_ceil(BLOCK);
    assert!(matches!(
        plan_resource_fork(raw_len, num_blocks, 3_000_000_000).unwrap(),
        ResourceForkPlan::Compressed { .. }
    ));
}

#[test]
fn resource_fork_plan_accepts_raw_files_beyond_four_gib_when_the_fork_fits() {
    let raw_len = 5_000_000_000usize;
    let num_blocks = raw_len.div_ceil(BLOCK);
    assert!(matches!(
        plan_resource_fork(raw_len, num_blocks, 3_000_000_000).unwrap(),
        ResourceForkPlan::Compressed { .. }
    ));
}

#[test]
fn resource_fork_plan_rejects_a_compressed_fork_past_u32() {
    let raw_len = 5_000_000_000usize;
    let num_blocks = raw_len.div_ceil(BLOCK);
    match plan_resource_fork(raw_len, num_blocks, 4_400_000_000).unwrap_err() {
        Error::Io { source, .. } => assert_eq!(source.raw_os_error(), Some(libc::EFBIG)),
        other => panic!("expected EFBIG Io, got {other:?}"),
    }
}

#[test]
fn gemini_nano_lzvn_resource_fork_is_no_gain() {
    // Chrome 150's v3Nano weights.bin measured with this exact 64 KiB LZVN
    // encoder: the encoded blocks expand enough to cross the u32 fork ceiling.
    assert_eq!(
        plan_resource_fork(4_269_932_544, 65_154, 4_364_775_458).unwrap(),
        ResourceForkPlan::Plain
    );
}

#[test]
fn gemini_nano_lzfse_resource_fork_fits_and_wins() {
    // The streamed type-12 run encoded the same 65,154 blocks to this payload;
    // with its 260,620-byte offset table the fork is safely below u32::MAX.
    assert_eq!(
        plan_resource_fork(4_269_932_544, 65_154, 3_598_249_560).unwrap(),
        ResourceForkPlan::Compressed {
            table_len: 260_620,
            total_len: 3_598_510_180,
        }
    );
}

// Incompressible data → LZVN would expand the resource fork, so keep an
// ordinary data fork. The bytes and compression-state signal must agree.
#[test]
fn kernel_roundtrips_incompressible_blocks() {
    let dir = std::env::temp_dir().join(format!("decmpfs-raw-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.bin");
    let mut raw = Vec::new();
    let mut x: u32 = 0x9e37_79b9;
    while raw.len() < 200_000 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        raw.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(&path, &raw).unwrap();
    if matches!(detect(&path).unwrap(), Support::Supported) {
        assert!(matches!(
            crate::compress_file(&path).unwrap(),
            crate::Outcome::NoGain { .. }
        ));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            raw,
            "plain fallback reads back identically"
        );
        assert!(
            !is_already_compressed(&path).unwrap(),
            "no-gain input must not carry UF_COMPRESSED"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_bytes_preserves_ownership_of_an_overwritten_file() {
    // Non-root exercises the chown path over an existing file — owner is our own
    // uid, so preservation is a no-op we assert stays stable + non-corrupting.
    // The root path (a file owned by a different uid) is verified in CI.
    let dir = std::env::temp_dir().join(format!("decmpfs-own-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f");
    std::fs::write(&path, vec![0u8; 4096]).unwrap();
    if !matches!(detect(&path), Ok(Support::Supported)) {
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let before_uid = std::fs::metadata(&path).unwrap().uid();
    let content = vec![0xABu8; 8192];
    apply_bytes(&path, &content, None).unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.uid(), before_uid, "owner preserved across the rewrite");
    assert_eq!(std::fs::read(&path).unwrap(), content, "content intact");
    std::fs::remove_dir_all(&dir).ok();
}
