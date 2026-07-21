#![no_main]
//! FUZZ target `unwrap_if_hybrid` — the executable-container section walk that
//! precedes the pressed-data decode (`decmpfs::addon`).
//!
//! `unwrap_if_hybrid` dispatches on the leading magic and hand-walks the binary's
//! section / load-command table to locate the pressed-data section, then hands it
//! to `decode_pressed_data`:
//!
//!   * Mach-O 64 (either endianness): `ncmds` load-command walk, `LC_SEGMENT_64`
//!     section scan for segment `SMOL` / section `__PRESSED_DATA`.
//!   * ELF 64: section-header table walk resolving `.PRESSED_DATA` via the
//!     section-header string table.
//!   * PE/COFF: section table walk for `.PRESSED`.
//!
//! Every one of those paths reads offsets/counts out of untrusted bytes and slices
//! with them — the raw-parsing surface with the most opportunity for an
//! out-of-bounds/overflow before the safe pressed-data decoder even runs. Feed RAW
//! bytes so the mutator explores all three container dispatches. Finding = panic /
//! abort / overflow / OOM / hang; a graceful `None` is a NON-finding.

use decmpfs::addon::unwrap_if_hybrid;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
  let _ = unwrap_if_hybrid(data);
});
