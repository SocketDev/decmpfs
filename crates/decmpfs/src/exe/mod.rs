//! Self-replacing executable packing (feature `exe`).
//!
//! [`pack_executable`] turns a real executable `E` into a stub `E'` carrying a
//! zstd-compressed copy of `E` in a signable `SMOL/__DECMPFS` section (macOS) or
//! an EOF footer (ELF/PE). On first run `E'` calls [`self_replace_and_exec`]:
//! it decompresses the payload, writes `E` back to disk **FS-compressed** via
//! [`crate::compress_bytes`], atomically renames over `argv[0]`, re-signs on
//! macOS, and `execve`s the materialized binary. Every later run is native — the
//! stub is gone, replaced by the real (smaller-on-disk) executable.
//!
//! The section/footer wire format is owned by [`section`]; the object surgery by
//! [`inject`]; the runtime swap by [`replace`]. All three are private; the crate
//! surface is the two functions below plus [`PackOutcome`].

use std::path::Path;

use crate::{Error, Gate};

mod inject;
mod replace;
mod section;

/// The result of packing an executable. Only `Err` is a hard failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackOutcome {
  /// Packed: `before` = original size, `after` = the stub's size on disk.
  Packed { before: u64, after: u64 },
  /// The gate excluded the input (by glob/size) — nothing written.
  SkippedGate,
}

/// Host-side packer: read `src`, compress it, inject the payload into a stub, and
/// write the self-replacing executable to `dest`. `gate` filters by glob/size the
/// same way [`crate::compress_bytes`] does; a gate miss returns
/// [`PackOutcome::SkippedGate`] without writing.
///
/// The stub bytes are the CURRENT executable's own image by default (a decmpfs
/// binary that links the `exe` feature IS the stub); pass an explicit stub via
/// [`pack_executable_with_stub`] to cross-pack.
pub fn pack_executable(src: &Path, dest: &Path, gate: &Gate) -> Result<PackOutcome, Error> {
  let stub = std::env::current_exe().map_err(|source| Error::Io {
    context: "resolve current_exe for the pack stub",
    source,
  })?;
  pack_executable_with_stub(&stub, src, dest, gate)
}

/// [`pack_executable`] with an explicit stub image — the self-replacing runtime
/// binary whose `SMOL/__DECMPFS` section/footer receives the payload.
pub fn pack_executable_with_stub(
  _stub: &Path,
  _src: &Path,
  _dest: &Path,
  _gate: &Gate,
) -> Result<PackOutcome, Error> {
  // Skeleton — implemented in the section/inject stages.
  Err(Error::Io {
    context: "pack_executable: not yet implemented",
    source: std::io::Error::from(std::io::ErrorKind::Unsupported),
  })
}

/// Runtime entry the packed stub calls from its `main`: resolve self → read the
/// payload → decompress → FS-compress the bytes to disk → atomically replace
/// `argv[0]` → re-sign (macOS) → `execve`. On success it does NOT return (the
/// process image is replaced); `Ok(false)` means "this binary is not a packed
/// stub, run your normal main". `Err` is a genuine I/O / integrity failure.
pub fn self_replace_and_exec(_argv: &[String]) -> Result<bool, Error> {
  // Skeleton — implemented in the replace stage.
  Ok(false)
}
