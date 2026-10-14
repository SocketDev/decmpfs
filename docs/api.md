# decmpfs API reference

`decmpfs` compresses one file with the host filesystem's transparent per-file compression. The kernel returns the original logical bytes when a process reads the file. The crate supports APFS decmpfs, btrfs, and NTFS. Other filesystems return `Outcome::Unsupported`.

The complete versioned Rust item reference is hosted on [docs.rs](https://docs.rs/decmpfs/0.1.5/decmpfs/). This page summarizes the main operations and their effects.

## Probe and compression

- `probe(path)` checks filesystem support and the file's current compression state. It does not modify the file. It returns `Support::Supported`, `AlreadyCompressed`, or `Unsupported(reason)`.
- `compress_file(path)` compresses an existing file in place. It checks the result by reading the logical bytes back. It rolls back if verification fails.
- `compress_bytes(path, content, gate)` writes bytes to the destination in one pass. The gate can exclude the destination by filename pattern or size; an excluded file is written without compression and reports `Outcome::Skipped { reason: GateExcluded }`.
- `stat(path)` reports whether the file is compressed and its logical and physical byte counts.

Capability gaps and safe skips are successful `Outcome` values. `Error` reports a missing file or an I/O failure that can leave integrity unknown.

## Outcomes

| Variant                                 | Meaning                                                                                           |
| --------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `Outcome::Compressed { before, after }` | Compression reduced allocated bytes.                                                              |
| `Outcome::NoGain { before, after }`     | Compression was applied, but allocation did not decrease.                                         |
| `Outcome::AlreadyCompressed { before }` | The file was compressed before this call.                                                         |
| `Outcome::Unsupported { reason }`       | The platform, filesystem, mount, or backend has no supported compression path.                    |
| `Outcome::Skipped { reason }`           | Compression was not applied because of permission, locking, immutable state, size, or gate rules. |

The size fields are allocated byte counts unless the method documentation for a type states otherwise. A returned outcome does not replace an I/O error check: callers must still handle `Result::Err`.

## Copy, remove, and streaming writes

- `try_clone_file(src, dest)` attempts a copy-on-write clone. `Ok(false)` means the platform or filesystem cannot clone that pair; it is not an error.
- `copy_file(src, dest)` preserves the source's compression state when possible. If the destination cannot be compressed or compression is skipped, it completes a plain copy and returns `CopyOutcome::CopiedPlain`; inspect the returned outcome to distinguish that case. An existing destination is replaced.
- `rm(path, options)` removes a file with the options in `RmOptions`.
- `DecmpfsWriter::create(path, expected_len, gate)` accepts data in chunks. Call `finish()` to verify the received byte count and publish the completed file, or `abort()` to stop the write.

## Optional features

The default build has no codec dependency. The `addon` feature enables hybrid `.node` decoding before compression. The `exe` feature enables executable packing support and adds the zstd and SHA-2 dependencies.
