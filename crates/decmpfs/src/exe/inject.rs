//! Object-format surgery: splice the `SMOL/__DECMPFS` section into a Mach-O
//! stub (signable, then ad-hoc re-signed via the system `codesign`), or append
//! the `[payload][hash][len][MAGIC]` footer to an ELF/PE stub (their loaders
//! don't enforce a signature to `execve`, so no surgery is needed).
//!
//! Ported from napi-rs `crates/napi-compress/src/inject.rs` (the proven Mach-O
//! segment-insertion + slack/LINKEDIT-shift logic), with the crate-based
//! ad-hoc signer replaced by a `codesign -s - -f` shell-out so the crate stays
//! dep-lean.
//!
//! Every structural offset is walked at runtime from the load commands — the
//! stub is rebuilt with header slack (`-headerpad`) that guarantees room for
//! the new segment command, not a fixed offset. The byte layout matches a
//! binject(LIEF)-produced reference: segment `filesize` is the unpadded body
//! length, `vmsize` is page-rounded; section `size` is the body length,
//! `offset`/`fileoff` is the old `__LINKEDIT` fileoff; W^X `initprot` =
//! `maxprot` = `VM_PROT_READ`.

use std::path::Path;

const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x02;
const LC_DYSYMTAB: u32 = 0x0b;
const LC_DYLD_INFO: u32 = 0x22;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
const LC_FUNCTION_STARTS: u32 = 0x26;
const LC_DATA_IN_CODE: u32 = 0x29;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const LC_DYLD_CHAINED_FIXUPS: u32 = 0x8000_0034;
const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;

const CPU_TYPE_ARM64: u32 = 0x0100_000c;

const MACH_HEADER_64_SIZE: usize = 32;
/// `cmd,cmdsize,segname[16],vmaddr,vmsize,fileoff,filesize,maxprot,initprot,nsects,flags`.
const SEGMENT_COMMAND_64_SIZE: usize = 72;
/// `sectname[16],segname[16],addr,size,offset,align,reloff,nreloc,flags,reserved1..3`.
const SECTION_64_SIZE: usize = 80;
const NEW_LC_SIZE: usize = SEGMENT_COMMAND_64_SIZE + SECTION_64_SIZE; // 152

/// `VM_PROT_READ` only. An injected segment MUST be read-only: RWX (0x07) makes
/// dyld refuse to mmap the bundle on dlopen (EACCES) even with a valid signature.
const VM_PROT_READ: u32 = 0x01;

fn u32_le(bytes: &[u8], off: usize) -> Result<u32, String> {
  bytes
    .get(off..off + 4)
    .and_then(|s| s.try_into().ok())
    .map(u32::from_le_bytes)
    .ok_or_else(|| format!("truncated u32 at offset {off}"))
}

fn u64_le(bytes: &[u8], off: usize) -> Result<u64, String> {
  bytes
    .get(off..off + 8)
    .and_then(|s| s.try_into().ok())
    .map(u64::from_le_bytes)
    .ok_or_else(|| format!("truncated u64 at offset {off}"))
}

fn put_u32(bytes: &mut [u8], off: usize, value: u32) -> Result<(), String> {
  bytes
    .get_mut(off..off + 4)
    .ok_or_else(|| format!("truncated u32 write at offset {off}"))?
    .copy_from_slice(&value.to_le_bytes());
  Ok(())
}

fn put_u64(bytes: &mut [u8], off: usize, value: u64) -> Result<(), String> {
  bytes
    .get_mut(off..off + 8)
    .ok_or_else(|| format!("truncated u64 write at offset {off}"))?
    .copy_from_slice(&value.to_le_bytes());
  Ok(())
}

fn round_up(value: u64, align: u64) -> u64 {
  if align == 0 {
    return value;
  }
  value.div_ceil(align) * align
}

/// One linkedit-pointing field to bump: the absolute byte offset of a `u32`
/// file-offset field within the (post-splice) command stream.
struct OffsetField {
  at: usize,
}

/// The structural anchors found by walking the load commands once.
struct Layout {
  page_size: u64,
  /// Byte offset of the `__LINKEDIT` segment command (splice point).
  linkedit_lc_off: usize,
  linkedit_fileoff: u64,
  linkedit_vmaddr: u64,
  /// `LC_CODE_SIGNATURE`, if present: (command byte offset, sig dataoff).
  code_sig: Option<CodeSig>,
  /// First `__TEXT` section file offset — the slack boundary the new LC must fit
  /// under (everything before it is header + load commands).
  first_section_offset: u64,
  end_of_lc: usize,
  /// Linkedit-pointing `u32` file-offset fields, by their byte offset in the
  /// ORIGINAL command stream (caller re-bases by +NEW_LC after the splice).
  linkedit_pointers: Vec<OffsetField>,
}

struct CodeSig {
  lc_off: usize,
  dataoff: u64,
}

/// A NUL-padded fixed-width name slot equals `want`.
fn name_eq(slot: &[u8], want: &[u8]) -> bool {
  slot.len() >= want.len()
    && &slot[..want.len()] == want
    && slot[want.len()..].iter().all(|&b| b == 0)
}

/// Walk the mach_header_64 + load commands once, recording every anchor the
/// surgery touches. Refuses anything but a single-arch 64-bit LE Mach-O.
fn read_layout(bytes: &[u8]) -> Result<Layout, String> {
  if u32_le(bytes, 0)? != MH_MAGIC_64 {
    return Err("not a 64-bit little-endian Mach-O (bad magic)".to_string());
  }
  let cputype = u32_le(bytes, 4)?;
  let page_size: u64 = if cputype == CPU_TYPE_ARM64 {
    0x4000
  } else {
    0x1000
  };
  let ncmds = u32_le(bytes, 16)?;

  let mut linkedit_lc_off: Option<usize> = None;
  let mut linkedit_fileoff = 0u64;
  let mut linkedit_vmaddr = 0u64;
  let mut code_sig: Option<CodeSig> = None;
  let mut first_section_offset = u64::MAX;
  let mut linkedit_pointers: Vec<OffsetField> = Vec::new();

  let mut off = MACH_HEADER_64_SIZE;
  for _ in 0..ncmds {
    let cmd = u32_le(bytes, off)?;
    let cmdsize = u32_le(bytes, off + 4)? as usize;
    if cmdsize < 8 || off + cmdsize > bytes.len() {
      return Err(format!(
        "malformed load command at {off} (cmdsize {cmdsize})"
      ));
    }
    match cmd {
      LC_SEGMENT_64 => {
        let segname = bytes
          .get(off + 8..off + 24)
          .ok_or_else(|| format!("truncated segname at {off}"))?;
        let fileoff = u64_le(bytes, off + 40)?;
        let nsects = u32_le(bytes, off + 64)?;
        if name_eq(segname, b"__LINKEDIT") {
          linkedit_lc_off = Some(off);
          linkedit_fileoff = fileoff;
          linkedit_vmaddr = u64_le(bytes, off + 24)?;
        }
        // Track the smallest section file offset across every segment — the
        // header-slack ceiling (load commands must not overrun the first
        // mapped section's bytes).
        let mut soff = off + SEGMENT_COMMAND_64_SIZE;
        for _ in 0..nsects {
          let sect_off = u32_le(bytes, soff + 48)? as u64;
          // offset == 0 marks a zero-fill section (__bss/__thread_bss); skip it.
          if sect_off != 0 && sect_off < first_section_offset {
            first_section_offset = sect_off;
          }
          soff = soff
            .checked_add(SECTION_64_SIZE)
            .ok_or_else(|| "section table offset overflow".to_string())?;
        }
      }
      LC_DYLD_INFO | LC_DYLD_INFO_ONLY => {
        // rebase_off@8, bind_off@16, weak_bind_off@24, lazy_bind_off@32, export_off@40.
        for field in [8, 16, 24, 32, 40] {
          linkedit_pointers.push(OffsetField { at: off + field });
        }
      }
      LC_SYMTAB => {
        // symoff@8, stroff@16.
        linkedit_pointers.push(OffsetField { at: off + 8 });
        linkedit_pointers.push(OffsetField { at: off + 16 });
      }
      LC_DYSYMTAB => {
        // tocoff@32, modtaboff@40, extrefsymoff@48, indirectsymoff@56,
        // extreloff@64, locreloff@72 — all linkedit-relative file offsets.
        for field in [32, 40, 48, 56, 64, 72] {
          linkedit_pointers.push(OffsetField { at: off + field });
        }
      }
      LC_FUNCTION_STARTS | LC_DATA_IN_CODE | LC_DYLD_CHAINED_FIXUPS | LC_DYLD_EXPORTS_TRIE => {
        // linkedit_data_command: dataoff@8.
        linkedit_pointers.push(OffsetField { at: off + 8 });
      }
      LC_CODE_SIGNATURE => {
        // linkedit_data_command: dataoff is a u32 file offset at +8 (NOT a u64).
        code_sig = Some(CodeSig {
          lc_off: off,
          dataoff: u32_le(bytes, off + 8)? as u64,
        });
      }
      _ => {}
    }
    off = off
      .checked_add(cmdsize)
      .ok_or_else(|| "load command offset overflow".to_string())?;
  }

  let linkedit_lc_off =
    linkedit_lc_off.ok_or_else(|| "no __LINKEDIT segment to anchor the new section".to_string())?;
  if first_section_offset == u64::MAX {
    return Err("no mapped section to bound the header slack".to_string());
  }
  Ok(Layout {
    page_size,
    linkedit_lc_off,
    linkedit_fileoff,
    linkedit_vmaddr,
    code_sig,
    first_section_offset,
    end_of_lc: off,
    linkedit_pointers,
  })
}

/// Build the 152-byte `LC_SEGMENT_64` + one `section_64` for `SMOL/__DECMPFS`.
fn build_segment_lc(
  body_len: u64,
  delta: u64,
  fileoff: u64,
  vmaddr: u64,
) -> Result<Vec<u8>, String> {
  let mut lc = vec![0u8; NEW_LC_SIZE];
  // segment_command_64
  put_u32(&mut lc, 0, LC_SEGMENT_64)?;
  put_u32(&mut lc, 4, NEW_LC_SIZE as u32)?;
  lc.get_mut(8..12)
    .ok_or_else(|| "new LC too short for segname".to_string())?
    .copy_from_slice(b"SMOL"); // segname (NUL-padded)
  put_u64(&mut lc, 24, vmaddr)?; // vmaddr
  put_u64(&mut lc, 32, delta)?; // vmsize (page-rounded)
  put_u64(&mut lc, 40, fileoff)?; // fileoff
  put_u64(&mut lc, 48, body_len)?; // filesize (unpadded body)
  put_u32(&mut lc, 56, VM_PROT_READ)?; // maxprot
  put_u32(&mut lc, 60, VM_PROT_READ)?; // initprot (W^X)
  put_u32(&mut lc, 64, 1)?; // nsects
  put_u32(&mut lc, 68, 0)?; // flags
                            // section_64 at +72
  let s = SEGMENT_COMMAND_64_SIZE;
  lc.get_mut(s..s + 9)
    .ok_or_else(|| "new LC too short for sectname".to_string())?
    .copy_from_slice(b"__DECMPFS"); // sectname
  lc.get_mut(s + 16..s + 20)
    .ok_or_else(|| "new LC too short for section segname".to_string())?
    .copy_from_slice(b"SMOL"); // segname
  put_u64(&mut lc, s + 32, vmaddr)?; // addr
  put_u64(&mut lc, s + 40, body_len)?; // size (unpadded body)
  put_u32(&mut lc, s + 48, fileoff as u32)?; // offset
  put_u32(&mut lc, s + 52, 2)?; // align 2^2 = 4
                                // reloff/nreloc/flags/reserved1..3 stay 0
  Ok(lc)
}

/// Inject `section_body` into `stub`, dispatching on the stub's object format.
/// Mach-O gets the signable `SMOL/__DECMPFS` segment splice (`section_body` is
/// the caller's [`super::section::build_section_payload`] output); ELF/PE need
/// no surgery — `section_body` is the caller's [`super::section::build_footer`]
/// output and is simply appended at EOF. Returns the modified bytes (Mach-O
/// still UNSIGNED — the caller runs [`resign`]).
#[allow(dead_code)] // wired in the replace stage
pub(crate) fn inject_payload(stub: &[u8], section_body: &[u8]) -> Result<Vec<u8>, String> {
  match stub.first().copied() {
    Some(0xcf) if stub.get(0..4) == Some(&MH_MAGIC_64.to_le_bytes()) => {
      inject_macho(stub, section_body)
    }
    Some(0x7f) if stub.get(0..4) == Some(b"\x7fELF") => Ok(append_footer(stub, section_body)),
    Some(0x4d) if stub.get(0..2) == Some(b"MZ") => Ok(append_footer(stub, section_body)),
    _ => Err("unrecognized stub format: not a 64-bit LE Mach-O, ELF, or PE".to_string()),
  }
}

/// ELF/PE: their loaders map from program headers / section headers already on
/// disk and never look past EOF for anything the loader cares about, so the
/// footer built by [`super::section::build_footer`] rides as a plain append —
/// no header field moves.
fn append_footer(stub: &[u8], footer: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(stub.len() + footer.len());
  out.extend_from_slice(stub);
  out.extend_from_slice(footer);
  out
}

/// Mach-O: splice a READ-only `SMOL/__DECMPFS` `LC_SEGMENT_64`. Returns the modified
/// bytes (still unsigned — caller re-signs via [`resign`]).
fn inject_macho(stub: &[u8], section_body: &[u8]) -> Result<Vec<u8>, String> {
  let layout = read_layout(stub)?;

  // 1. Slack guard: the new 152-byte LC must fit between END_OF_LC and the first
  //    mapped section. The stub is linked with -headerpad,0x1000 to guarantee it.
  let slack = (layout.first_section_offset as usize)
    .checked_sub(layout.end_of_lc)
    .ok_or_else(|| {
      "first mapped section precedes the end of load commands (corrupt layout)".to_string()
    })?;
  if slack < NEW_LC_SIZE {
    return Err(format!(
      "header slack {slack} < {NEW_LC_SIZE} bytes for the new segment command; \
       rebuild the stub with -headerpad,0x1000"
    ));
  }

  let body_len = section_body.len() as u64;
  let delta = round_up(body_len, layout.page_size);
  let new_fileoff = layout.linkedit_fileoff;
  let new_vmaddr = layout.linkedit_vmaddr;
  let linkedit_start = layout.linkedit_fileoff as usize;
  // Exclude the old signature bytes — they trail __LINKEDIT and the signer
  // regenerates them. Without a signature, __LINKEDIT runs to EOF.
  let linkedit_end = match &layout.code_sig {
    Some(sig) => sig.dataoff as usize,
    None => stub.len(),
  };
  if linkedit_end < linkedit_start {
    return Err("code signature precedes __LINKEDIT (corrupt layout)".to_string());
  }
  // Mach-O section fileoff + every LINKEDIT-relative offset are 32-bit. `linkedit_end
  // + delta` bounds the widest post-splice offset (the new section's fileoff and each
  // `current + delta` bump all fall at or below it), so guard that sum against u32::MAX
  // before the `as u32` casts below — release builds have overflow-checks off, so a
  // >4 GiB payload would otherwise truncate silently and corrupt the LINKEDIT tables.
  if (linkedit_end as u64)
    .checked_add(delta)
    .map_or(true, |end| end > u64::from(u32::MAX))
  {
    return Err(format!(
      "injected payload too large: file offset {linkedit_end} + {delta} exceeds the u32 Mach-O limit"
    ));
  }
  let end_after_new_lc = layout
    .end_of_lc
    .checked_add(NEW_LC_SIZE)
    .ok_or_else(|| "end-of-load-commands offset overflow".to_string())?;
  let stub_before_linkedit = stub
    .get(0..layout.linkedit_lc_off)
    .ok_or_else(|| "__LINKEDIT LC offset out of range".to_string())?;
  let stub_lc_tail = stub
    .get(layout.linkedit_lc_off..layout.end_of_lc)
    .ok_or_else(|| "load-command tail out of range".to_string())?;
  let stub_headerpad_tail = stub
    .get(end_after_new_lc..linkedit_start)
    .ok_or_else(|| "headerpad tail out of range".to_string())?;
  let stub_linkedit_body = stub
    .get(linkedit_start..linkedit_end)
    .ok_or_else(|| "__LINKEDIT body out of range".to_string())?;

  // 2. Assemble the new file so NOTHING before __LINKEDIT moves its file offset.
  //    The new 152-byte LC is written into the header slack: the load-command
  //    bytes [linkedit_lc_off, END_OF_LC) shift forward by 152, consuming 152 of
  //    the headerpad gap; the first mapped section and every byte up to
  //    __LINKEDIT keep their original file offset. __LINKEDIT's body then slides
  //    down by DELTA only (the page-rounded section content occupies the old
  //    __LINKEDIT file region).
  //
  //    [0, linkedit_lc_off)              header + LCs before __LINKEDIT's LC
  //    new_lc (152)                      the SMOL/__DECMPFS segment command
  //    [linkedit_lc_off, END_OF_LC)      __LINKEDIT's LC + the LCs after it
  //    [END_OF_LC+152, linkedit_start)   remaining headerpad + all mapped bytes
  //    section_body + zero-pad to DELTA  the injected section content
  //    [linkedit_start, linkedit_end)    __LINKEDIT body (sans old signature)
  let new_lc = build_segment_lc(body_len, delta, new_fileoff, new_vmaddr)?;
  let pad_len = (delta - body_len) as usize;
  let mut out: Vec<u8> = Vec::with_capacity(stub.len() + delta as usize);
  out.extend_from_slice(stub_before_linkedit);
  out.extend_from_slice(&new_lc);
  out.extend_from_slice(stub_lc_tail);
  // Headerpad after the (now-larger) command stream, shrunk by the 152 bytes the
  // new LC consumed, so the first mapped section stays at its original offset.
  out.extend_from_slice(stub_headerpad_tail);
  out.extend_from_slice(section_body);
  out.resize(
    out
      .len()
      .checked_add(pad_len)
      .ok_or_else(|| "padded section length overflow".to_string())?,
    0,
  );
  out.extend_from_slice(stub_linkedit_body);

  // 3. Header: ncmds += 1, sizeofcmds += NEW_LC. (The strip below nets these back
  //    down by the code-signature command.)
  let ncmds = u32_le(&out, 16)?;
  put_u32(&mut out, 16, ncmds + 1)?;
  let sizeofcmds = u32_le(&out, 20)?;
  put_u32(&mut out, 20, sizeofcmds + NEW_LC_SIZE as u32)?;

  // 4. Shift __LINKEDIT's own fileoff + vmaddr by DELTA (its LC now sits at
  //    linkedit_lc_off + NEW_LC, after the new segment command). When the old
  //    signature was excluded, shrink filesize/vmsize to the bytes that remain on
  //    disk (up to where the signature began) — else the segment claims more than
  //    the file holds and the signer's parser rejects it; the signer re-extends.
  let le_lc = layout
    .linkedit_lc_off
    .checked_add(NEW_LC_SIZE)
    .ok_or_else(|| "__LINKEDIT LC offset overflow after splice".to_string())?;
  let le_fileoff = u64_le(&out, le_lc + 40)?;
  put_u64(&mut out, le_lc + 40, le_fileoff + delta)?;
  let le_vmaddr = u64_le(&out, le_lc + 24)?;
  put_u64(&mut out, le_lc + 24, le_vmaddr + delta)?;
  if let Some(sig) = &layout.code_sig {
    let remaining = sig
      .dataoff
      .checked_sub(layout.linkedit_fileoff)
      .ok_or_else(|| "code signature precedes __LINKEDIT fileoff".to_string())?;
    put_u64(&mut out, le_lc + 48, remaining)?; // filesize
    put_u64(&mut out, le_lc + 32, round_up(remaining, layout.page_size))?; // vmsize
  }

  // 5. Bump every linkedit-pointing file offset by DELTA. A field whose command
  //    sits at/after __LINKEDIT's LC had its byte position shifted +NEW_LC when the
  //    new LC was written before __LINKEDIT; a field before it keeps its position.
  //    Skip zeros (an absent table).
  for field in &layout.linkedit_pointers {
    let at = if field.at >= layout.linkedit_lc_off {
      field
        .at
        .checked_add(NEW_LC_SIZE)
        .ok_or_else(|| "linkedit-pointer offset overflow after splice".to_string())?
    } else {
      field.at
    };
    let current = u32_le(&out, at)?;
    if current != 0 {
      put_u32(&mut out, at, current + delta as u32)?;
    }
  }

  // 6. Strip LC_CODE_SIGNATURE in place (it is the LAST command, so zeroing its
  //    bytes + decrementing the header counts removes it WITHOUT shifting any file
  //    offset — a splice here would move __LINKEDIT and re-break step 5). Its
  //    trailing __LINKEDIT bytes were already excluded in step 2; the signer
  //    re-adds a correct command into the freed command-stream slack.
  if let Some(sig) = &layout.code_sig {
    let sig_lc = sig
      .lc_off
      .checked_add(NEW_LC_SIZE)
      .ok_or_else(|| "code-signature LC offset overflow after splice".to_string())?;
    let sig_cmdsize = u32_le(&out, sig_lc + 4)? as usize;
    let ncmds = u32_le(&out, 16)?;
    put_u32(&mut out, 16, ncmds - 1)?;
    let sizeofcmds = u32_le(&out, 20)?;
    put_u32(&mut out, 20, sizeofcmds - sig_cmdsize as u32)?;
    let zero_range = out
      .get_mut(sig_lc..sig_lc + sig_cmdsize)
      .ok_or_else(|| "code-signature LC out of range after splice".to_string())?;
    zero_range.fill(0);
  }

  Ok(out)
}

/// Ad-hoc re-sign a materialized Mach-O at `path` via the system `codesign`
/// (`codesign -s - -f <path>`). macOS-only; a no-op `Ok(())` elsewhere — ELF/PE
/// loaders enforce no signature, so the appended footer needs none.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)] // wired in the replace stage
pub(crate) fn resign(_path: &Path) -> Result<(), String> {
  Ok(())
}

#[cfg(target_os = "macos")]
#[allow(dead_code)] // wired in the replace stage
pub(crate) fn resign(path: &Path) -> Result<(), String> {
  let status = std::process::Command::new("codesign")
    .arg("-s")
    .arg("-")
    .arg("-f")
    .arg(path)
    .status()
    .map_err(|source| format!("spawn codesign for {}: {source}", path.display()))?;
  if !status.success() {
    return Err(format!("codesign failed for {} ({status})", path.display()));
  }
  Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
  use super::*;
  use crate::exe::section::{build_footer, build_section_payload, find_footer};
  #[cfg(target_os = "macos")]
  use crate::exe::section::{find_section, parse_section_payload};

  /// A minimal two-segment 64-bit Mach-O: `__TEXT` (one section, anchoring the
  /// header-slack boundary) then `__LINKEDIT` (the splice point). Big enough for
  /// `read_layout`/`inject_macho` to walk for real, small enough to stay a unit
  /// test — unlike [`crate::exe::section::synthetic_object_with_section`] (built
  /// for `find_section`'s read path, which never inspects `__LINKEDIT`), this
  /// fixture carries the segment `inject_macho` requires to splice at all.
  #[cfg(target_os = "macos")]
  fn minimal_macho64_with_linkedit(
    first_section_offset: u32,
    linkedit_fileoff: u64,
    linkedit_body: &[u8],
  ) -> Vec<u8> {
    let linkedit_lc_off = MACH_HEADER_64_SIZE + NEW_LC_SIZE; // 32 + 152 = 184
    let linkedit_len = linkedit_body.len() as u64;
    let mut m = vec![0u8; linkedit_fileoff as usize + linkedit_body.len()];

    // mach_header_64.
    put_u32(&mut m, 0, MH_MAGIC_64).expect("header magic");
    put_u32(&mut m, 16, 2).expect("ncmds"); // __TEXT, __LINKEDIT
    put_u32(&mut m, 20, (NEW_LC_SIZE + SEGMENT_COMMAND_64_SIZE) as u32).expect("sizeofcmds");

    // __TEXT segment_command_64 + one section_64, at MACH_HEADER_64_SIZE.
    let text = MACH_HEADER_64_SIZE;
    put_u32(&mut m, text, LC_SEGMENT_64).expect("text cmd");
    put_u32(&mut m, text + 4, NEW_LC_SIZE as u32).expect("text cmdsize");
    m[text + 8..text + 14].copy_from_slice(b"__TEXT");
    put_u32(&mut m, text + 64, 1).expect("text nsects");
    let text_sect = text + SEGMENT_COMMAND_64_SIZE;
    m[text_sect..text_sect + 6].copy_from_slice(b"__text");
    m[text_sect + 16..text_sect + 22].copy_from_slice(b"__TEXT");
    put_u32(&mut m, text_sect + 48, first_section_offset).expect("text section offset");

    // __LINKEDIT segment_command_64 (no sections).
    put_u32(&mut m, linkedit_lc_off, LC_SEGMENT_64).expect("linkedit cmd");
    put_u32(&mut m, linkedit_lc_off + 4, SEGMENT_COMMAND_64_SIZE as u32).expect("linkedit cmdsize");
    m[linkedit_lc_off + 8..linkedit_lc_off + 18].copy_from_slice(b"__LINKEDIT");
    put_u64(
      &mut m,
      linkedit_lc_off + 24,
      0x1_0000_0000 + linkedit_fileoff,
    )
    .expect("linkedit vmaddr");
    put_u64(&mut m, linkedit_lc_off + 40, linkedit_fileoff).expect("linkedit fileoff");
    put_u64(&mut m, linkedit_lc_off + 48, linkedit_len).expect("linkedit filesize");

    m[linkedit_fileoff as usize..linkedit_fileoff as usize + linkedit_body.len()]
      .copy_from_slice(linkedit_body);
    m
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn macho_injection_round_trips_through_find_section() {
    let body = build_section_payload(0x0123_4567_89ab_cdef, b"the-zstd-payload-bytes");
    let stub = minimal_macho64_with_linkedit(512, 600, b"LINKEDIT-CONTENT");
    let out = inject_payload(&stub, &body).expect("inject");

    let raw = find_section(&out).expect("section found");
    let got = parse_section_payload(raw).expect("parses");
    assert_eq!(got.content_hash, 0x0123_4567_89ab_cdef);
    assert_eq!(got.payload, b"the-zstd-payload-bytes");
  }

  #[test]
  fn elf_and_pe_injection_append_the_footer_verbatim() {
    // The ELF/PE loader maps from program/section headers already on disk and
    // never looks past EOF, so injection is a pure append: the footer bytes
    // `find_footer` locates at the tail of the grown image must equal the
    // footer `build_footer` produced, byte for byte.
    let footer = build_footer(0xfeed_face_dead_beef, b"the-zstd-bytes");

    let mut elf_stub = vec![0u8; 64];
    elf_stub[0..4].copy_from_slice(b"\x7fELF");
    let elf_out = inject_payload(&elf_stub, &footer).expect("inject elf");
    assert_eq!(
      find_footer(&elf_out).expect("footer found"),
      footer.as_slice()
    );

    let mut pe_stub = vec![0u8; 64];
    pe_stub[0..2].copy_from_slice(b"MZ");
    let pe_out = inject_payload(&pe_stub, &footer).expect("inject pe");
    assert_eq!(
      find_footer(&pe_out).expect("footer found"),
      footer.as_slice()
    );
  }

  #[test]
  fn dispatch_rejects_unknown_format() {
    assert!(inject_payload(b"not an object file", b"x").is_err());
  }

  #[cfg(not(target_os = "macos"))]
  #[test]
  fn resign_is_a_no_op_off_macos() {
    assert!(resign(Path::new("/does/not/exist")).is_ok());
  }
}
