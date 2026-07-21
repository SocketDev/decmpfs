#![no_main]
//! FUZZ target `decode_pressed_data` — the bin-infra pressed-data blob parser.
//!
//! This is the HIGHEST-PRIORITY untrusted-input entry: `decode_pressed_data`
//! parses an on-disk blob (`decmpfs::addon`) that a package-manager helper reads
//! out of a napi `--compress` hybrid section. The bytes are attacker-controllable
//! (they arrive from a downloaded addon), and the parser reads two `u64` length
//! fields, a 64-byte SHA-512, a config-present flag, then slices out a zstd
//! payload and inflates it:
//!
//!   [magic 32B]["comp" u64 LE]["uncomp" u64 LE][key 16B][plat 3B][sha512 64B]
//!   [has_config 1B]([config 1192B]?)[zstd payload]
//!
//! Feed RAW bytes: the offset arithmetic (`at + compressed_size`, the `+1192`
//! config skip), the DoS size guards (`MAX_DECOMPRESSED`), the SHA-512 gate, and
//! the zstd frame decode are exactly what we want to exercise. Finding = panic /
//! abort / integer overflow / OOM / hang. A graceful `None` (bad magic, short
//! buffer, size-guard trip, integrity mismatch, malformed zstd frame) is a
//! NON-finding.

use decmpfs::addon::decode_pressed_data;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
  let _ = decode_pressed_data(data);
});
