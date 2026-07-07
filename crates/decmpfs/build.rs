//! Reserve Mach-O header slack in the `decmpfs-stub` binary so the `exe` packer
//! can splice a new `LC_SEGMENT_64` (`SMOL/__DECMPFS`) into it in place, without
//! relinking. Only the stub bin, only on macOS, only under the `exe` feature —
//! every other build is untouched.

fn main() {
  let exe_feature = std::env::var_os("CARGO_FEATURE_EXE").is_some();
  let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
  if exe_feature && macos {
    println!("cargo::rustc-link-arg-bin=decmpfs-stub=-Wl,-headerpad,0x1000");
  }
}
