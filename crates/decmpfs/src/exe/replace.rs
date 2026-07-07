//! The runtime swap: read the packed stub's own payload, decompress, write the
//! real executable back to disk FS-compressed, atomically replace `argv[0]`,
//! re-sign (macOS), and `execve`.
//!
//! A running image can't be overwritten on Windows, so there the swap is
//! deferred: write the materialized binary alongside and schedule a
//! rename-on-next-start (MoveFileEx with MOVEFILE_DELAY_UNTIL_REBOOT is too
//! coarse; the runtime writes a `.pending` sibling and a tiny relauncher).
//!
//! Unix path: `std::os::unix::process::CommandExt::exec` replaces the process
//! image so control never returns on success.

use crate::Error;

/// Read the current executable's own packed payload, if any. `Ok(None)` means
/// this binary is a plain executable (not a packed stub) — the caller runs its
/// normal `main`.
#[allow(dead_code)] // wired in the replace stage
pub(crate) fn read_self_payload() -> Result<Option<super::section::SectionData>, Error> {
  Ok(None)
}

/// Materialize + swap + exec. Does not return on success (process image
/// replaced). Skeleton returns `Ok(false)` = "not a packed stub".
#[allow(dead_code)] // wired in the replace stage
pub(crate) fn materialize_and_exec(_argv: &[String]) -> Result<bool, Error> {
  Ok(false)
}
