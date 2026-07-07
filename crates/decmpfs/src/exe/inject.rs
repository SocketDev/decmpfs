//! Object-format surgery: splice the `SMOL/__DECMPFS` section into a Mach-O
//! stub (signable, then ad-hoc re-signed via the system `codesign`), or append
//! the `[payload][hash][len][MAGIC]` footer to an ELF/PE stub (their loaders
//! don't enforce a signature to `execve`, so no surgery is needed).
//!
//! Ported from napi-rs `crates/napi-compress/src/inject.rs` (the proven Mach-O
//! segment-insertion + slack/LINKEDIT-shift logic), with the crate-based
//! ad-hoc signer replaced by a `codesign -s - -f` shell-out so the crate stays
//! dep-lean.

/// Inject `section_body` into `stub`, dispatching on the stub's object format.
/// Returns the modified bytes (Mach-O still UNSIGNED — caller runs [`resign`]).
#[allow(dead_code)] // wired in the inject stage
pub(crate) fn inject_payload(_stub: &[u8], _section_body: &[u8]) -> Result<Vec<u8>, String> {
  Err("inject_payload: not yet implemented".to_string())
}

/// Ad-hoc re-sign a materialized Mach-O at `path` via the system `codesign`
/// (`codesign -s - -f <path>`). macOS-only; a no-op `Ok(())` elsewhere.
#[allow(dead_code)] // wired in the inject stage
pub(crate) fn resign(_path: &std::path::Path) -> Result<(), String> {
  #[cfg(not(target_os = "macos"))]
  {
    return Ok(());
  }
  #[cfg(target_os = "macos")]
  {
    Err("resign: not yet implemented".to_string())
  }
}
