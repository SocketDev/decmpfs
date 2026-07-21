//! macOS backend — APFS/HFS+ decmpfs transparent compression.
//!
//! decmpfs is an undocumented kernel ABI; afsctool and `ditto --hfsCompression` are
//! the references. We write the LZVN (type 8) and LZFSE (type 12) resource-fork
//! variants: the kernel decompresses on read(), so the file keeps its logical size
//! and stays a loadable native binary. Both codecs come from the system
//! libcompression library, so the macOS stub gains no Rust codec dependency.
//!
//! Common `.node` files take the speed-first LZVN path, with LZFSE as a no-gain
//! fallback. Large assets stream ratio-first LZFSE blocks directly into the named
//! resource fork, avoiding both a multi-gigabyte output allocation and `setxattr`'s
//! `E2BIG` ceiling. LZVN is the last fallback there for unusual codec-specific data.
//!
//! Layout written (verified by the kernel-roundtrip test):
//!   xattr com.apple.decmpfs      = [magic u32 LE][type=8/12 u32 LE][rawSize u64 LE]
//!   xattr com.apple.ResourceFork = [(numBlocks+1) u32 LE offsets][codec blocks]
//! A winning fork is built on an empty sibling temp with those xattrs and
//! UF_COMPRESSED; an expanding fork becomes an ordinary sibling data fork. Either
//! form is atomically renamed over the original — never an in-place truncate, so
//! a crash can't leave a 0-byte file.

use std::os::fd::AsRawFd;
use std::path::Path;

use crate::{cstring, io, Error, Support, UnsupportedReason};

const UF_COMPRESSED: u32 = 0x0000_0020;
const DECMPFS_MAGIC: u32 = 0x636d_7066; // 'cmpf' (XNU sys/decmpfs.h); LE on disk = "fpmc"
const BLOCK: usize = 0x1_0000; // 64 KiB
const XATTR_NOFOLLOW: libc::c_int = 0x0001;
const COMPRESSION_LZVN: i32 = 0x900;
const COMPRESSION_LZFSE: i32 = 0x801;

// 2026-07-16 — The data supports keeping a 64 MiB in-memory fast path for now.
// The sampled Darwin ARM64 Vite ecosystem topped out at SWC 36.563 MiB, followed
// by Rolldown 15.6–17.9 MiB, Oxlint 14.4 MiB, Lightning CSS 8.1 MiB, and Oxc
// bindings at 1.9–5.7 MiB. Keeping <=64 MiB on parallel LZVN + one `setxattr`
// leaves 27 MiB of headroom above that observed upper tail; larger assets stream
// to `..namedfork/rsrc` so peak output memory stays bounded. Re-benchmark this
// boundary as native-addon sizes or the streaming implementation changes.
pub(crate) const STREAMING_THRESHOLD: usize = 64 * 1024 * 1024;

fn should_stream_resource_fork(raw_len: usize, threshold: usize) -> bool {
  raw_len > threshold
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
  Lzvn,
  Lzfse,
}

impl Codec {
  const fn compression_type(self) -> u32 {
    match self {
      Self::Lzvn => 8,
      Self::Lzfse => 12,
    }
  }

  const fn algorithm(self) -> i32 {
    match self {
      Self::Lzvn => COMPRESSION_LZVN,
      Self::Lzfse => COMPRESSION_LZFSE,
    }
  }
}

#[link(name = "compression")]
extern "C" {
  fn compression_decode_buffer(
    dst_buffer: *mut u8,
    dst_size: usize,
    src_buffer: *const u8,
    src_size: usize,
    scratch_buffer: *mut u8,
    algorithm: i32,
  ) -> usize;
  fn compression_encode_buffer(
    dst_buffer: *mut u8,
    dst_size: usize,
    src_buffer: *const u8,
    src_size: usize,
    scratch_buffer: *mut u8,
    algorithm: i32,
  ) -> usize;
  fn compression_encode_scratch_buffer_size(algorithm: i32) -> usize;
}

fn resource_fork_too_large() -> Error {
  Error::Io {
    context: "decmpfs resource fork exceeds u32 offsets",
    source: std::io::Error::from_raw_os_error(libc::EFBIG),
  }
}

fn statfs(path: &Path) -> Result<libc::statfs, Error> {
  let cpath = cstring(path)?;
  let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
  if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
    return Err(io("statfs"));
  }
  Ok(buf)
}

/// Local APFS or HFS+ only — the two filesystems with the decmpfs path. A network
/// or non-local mount reports Unsupported (the signal isn't ours to trust).
pub(crate) fn detect(path: &Path) -> Result<Support, Error> {
  let buf = statfs(path)?;
  // f_fstypename is a NUL-padded C string ("apfs", "hfs").
  let name: Vec<u8> = buf
    .f_fstypename
    .iter()
    .take_while(|&&c| c != 0)
    .map(|&c| c as u8)
    .collect();
  Ok(classify_fs(
    buf.f_flags & (libc::MNT_LOCAL as u32) != 0,
    &name,
  ))
}

/// The pure detect policy — split from the statfs syscall so the network/non-APFS
/// branches are unit-testable without a network mount or an exotic filesystem.
fn classify_fs(is_local: bool, fstype: &[u8]) -> Support {
  if !is_local {
    return Support::Unsupported(UnsupportedReason::NetworkOrOverlay);
  }
  if fstype == b"apfs" || fstype == b"hfs" {
    Support::Supported
  } else {
    Support::Unsupported(UnsupportedReason::Filesystem)
  }
}

fn st_flags(path: &Path) -> Result<u32, Error> {
  let cpath = cstring(path)?;
  let mut st: libc::stat = unsafe { std::mem::zeroed() };
  if unsafe { libc::lstat(cpath.as_ptr(), &mut st) } != 0 {
    return Err(io("lstat"));
  }
  Ok(st.st_flags)
}

pub(crate) fn is_already_compressed(path: &Path) -> Result<bool, Error> {
  Ok(st_flags(path)? & UF_COMPRESSED != 0)
}

/// On macOS, UF_COMPRESSED is the authoritative win signal (st_blocks also drops,
/// but the flag is unambiguous and what we set).
pub(crate) fn compressed_on_disk(path: &Path) -> Result<Option<bool>, Error> {
  Ok(Some(is_already_compressed(path)?))
}

/// Encode `src` into one kernel-decodable block. libcompression emits a valid
/// frame even for incompressible input (slightly larger than `src`), so every
/// block decodes the same way. `None` means the codec declined outright.
fn compress_block_with_codec(src: &[u8], scratch: &mut [u8], codec: Codec) -> Option<Vec<u8>> {
  // Headroom for the worst case (incompressible data expands a little).
  let mut dst = vec![0u8; src.len() + src.len() / 16 + 1024];
  let n = unsafe {
    compression_encode_buffer(
      dst.as_mut_ptr(),
      dst.len(),
      src.as_ptr(),
      src.len(),
      scratch.as_mut_ptr(),
      codec.algorithm(),
    )
  };
  if n == 0 {
    return None;
  }
  dst.truncate(n);
  Some(dst)
}

// The compatibility wrapper keeps the focused LZVN unit tests terse.
#[cfg(test)]
fn compress_block(src: &[u8], scratch: &mut [u8]) -> Option<Vec<u8>> {
  compress_block_with_codec(src, scratch, Codec::Lzvn)
}

#[derive(Debug, PartialEq, Eq)]
enum ResourceForkPlan {
  /// The fork would not make the file smaller. Keep the ordinary data fork.
  Plain,
  /// The encoded fork is smaller and every offset fits the on-disk u32 table.
  Compressed { table_len: usize, total_len: usize },
}

fn resource_fork_table_len(num_blocks: usize) -> Result<usize, Error> {
  num_blocks
    .checked_add(1)
    .and_then(|entries| entries.checked_mul(std::mem::size_of::<u32>()))
    .ok_or_else(resource_fork_too_large)
}

/// Decide from lengths alone whether the fork is useful and representable. The
/// raw length in `com.apple.decmpfs` is u64; only offsets inside the resource
/// fork are u32. This allows a raw file beyond the old 3.9 GB cutoff whenever its
/// encoded fork is smaller than both the raw file and `u32::MAX`.
fn plan_resource_fork(
  raw_len: usize,
  num_blocks: usize,
  encoded_len: usize,
) -> Result<ResourceForkPlan, Error> {
  let table_len = resource_fork_table_len(num_blocks)?;
  let total_len = table_len
    .checked_add(encoded_len)
    .ok_or_else(resource_fork_too_large)?;

  if total_len >= raw_len {
    return Ok(ResourceForkPlan::Plain);
  }
  if total_len > u32::MAX as usize {
    return Err(resource_fork_too_large());
  }
  Ok(ResourceForkPlan::Compressed {
    table_len,
    total_len,
  })
}

fn compress_blocks(raw: &[u8], codec: Codec) -> Option<Vec<Vec<u8>>> {
  let num_blocks = raw.len().div_ceil(BLOCK).max(1);
  let scratch_len = unsafe { compression_encode_scratch_buffer_size(codec.algorithm()) };

  // The 64 KiB blocks are independent, so fan them across cores. Each worker
  // owns its libcompression scratch buffer. Contiguous regions keep the output
  // in block order without a sort.
  let workers = if std::env::var_os("DECMPFS_SERIAL").is_some() {
    1
  } else {
    std::thread::available_parallelism()
      .map(|n| n.get())
      .unwrap_or(1)
      .min(num_blocks)
  };
  if workers <= 1 || num_blocks < 8 {
    let mut scratch = vec![0u8; scratch_len];
    return raw
      .chunks(BLOCK)
      .map(|chunk| compress_block_with_codec(chunk, &mut scratch, codec))
      .collect();
  }

  let bytes_per_worker = num_blocks.div_ceil(workers) * BLOCK;
  let parts: Vec<Option<Vec<Vec<u8>>>> = std::thread::scope(|scope| {
    let handles: Vec<_> = raw
      .chunks(bytes_per_worker)
      .map(|region| {
        scope.spawn(move || {
          let mut scratch = vec![0u8; scratch_len];
          region
            .chunks(BLOCK)
            .map(|chunk| compress_block_with_codec(chunk, &mut scratch, codec))
            .collect::<Option<Vec<Vec<u8>>>>()
        })
      })
      .collect();
    handles
      .into_iter()
      .map(|handle| handle.join().ok().flatten())
      .collect()
  });
  let mut out = Vec::with_capacity(num_blocks);
  for part in parts {
    out.extend(part?);
  }
  Some(out)
}

/// Build the com.apple.ResourceFork blob for `raw` in the LZVN/LZFSE decmpfs
/// layout (what `ditto` writes): `(numBlocks+1)` u32 LE offsets, then the blocks.
/// `offset[0]` = table size; `offset[i+1]` = end of block i; last = total size.
/// `Ok(None)` means this codec did not shrink the input.
fn build_resource_fork_with_codec(raw: &[u8], codec: Codec) -> Result<Option<Vec<u8>>, Error> {
  let num_blocks = raw.len().div_ceil(BLOCK).max(1);
  let Some(blocks) = compress_blocks(raw, codec) else {
    return Ok(None);
  };

  let encoded_len = blocks
    .iter()
    .try_fold(0usize, |sum, block| sum.checked_add(block.len()))
    .ok_or_else(resource_fork_too_large)?;
  let ResourceForkPlan::Compressed {
    table_len,
    total_len,
  } = plan_resource_fork(raw.len(), num_blocks, encoded_len)?
  else {
    return Ok(None);
  };

  let mut out = Vec::with_capacity(total_len);
  // Offset table: numBlocks+1 entries. offset[i] is where block i starts.
  let mut offset = u32::try_from(table_len).map_err(|_| resource_fork_too_large())?;
  out.extend_from_slice(&offset.to_le_bytes());
  for block in &blocks {
    offset = offset
      .checked_add(u32::try_from(block.len()).map_err(|_| resource_fork_too_large())?)
      .ok_or_else(resource_fork_too_large)?;
    out.extend_from_slice(&offset.to_le_bytes());
  }
  for block in &blocks {
    out.extend_from_slice(block);
  }
  debug_assert_eq!(out.len(), total_len);
  Ok(Some(out))
}

/// The existing speed-first LZVN builder, retained as a focused test seam.
#[cfg(test)]
fn build_resource_fork(raw: &[u8]) -> Result<Option<Vec<u8>>, Error> {
  build_resource_fork_with_codec(raw, Codec::Lzvn)
}

struct InMemoryResourceFork {
  codec: Codec,
  bytes: Vec<u8>,
}

/// Common addons prefer LZVN decode speed. Only a no-gain LZVN attempt pays for
/// the stronger LZFSE pass.
fn build_in_memory_resource_fork(raw: &[u8]) -> Result<Option<InMemoryResourceFork>, Error> {
  for codec in [Codec::Lzvn, Codec::Lzfse] {
    if let Some(bytes) = build_resource_fork_with_codec(raw, codec)? {
      return Ok(Some(InMemoryResourceFork { codec, bytes }));
    }
  }
  Ok(None)
}

/// Stream one codec into the temp file's named resource fork. One single-slot
/// channel per worker keeps at most one encoded block per core waiting for the
/// ordered writer, so output memory is bounded while libcompression stays
/// parallel. `Ok(false)` means no gain, an unrepresentable u32 fork, or a codec
/// decline; the caller may retry the original bytes with another codec.
fn write_streaming_resource_fork(path: &Path, raw: &[u8], codec: Codec) -> Result<bool, Error> {
  use std::io::{Seek, Write};
  use std::sync::atomic::{AtomicBool, Ordering};

  let num_blocks = raw.len().div_ceil(BLOCK).max(1);
  let table_len = resource_fork_table_len(num_blocks)?;
  if table_len >= raw.len() || table_len > u32::MAX as usize {
    return Ok(false);
  }

  let fork_path = path.join("..namedfork").join("rsrc");
  let mut file = std::fs::OpenOptions::new()
    .write(true)
    .create(true)
    .truncate(true)
    .open(fork_path)
    .map_err(|source| Error::Io {
      context: "open resource fork",
      source,
    })?;
  file.set_len(table_len as u64).map_err(|source| Error::Io {
    context: "reserve resource-fork table",
    source,
  })?;
  file
    .seek(std::io::SeekFrom::Start(table_len as u64))
    .map_err(|source| Error::Io {
      context: "seek resource-fork payload",
      source,
    })?;
  // Coalesce codec blocks into larger writes. Besides syscall overhead, one
  // write per 64 KiB block makes APFS allocate several extra MiB of extents on
  // a multi-gigabyte fork even when its logical compressed bytes are identical.
  let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);

  let workers = if std::env::var_os("DECMPFS_SERIAL").is_some() {
    1
  } else {
    std::thread::available_parallelism()
      .map(|n| n.get())
      .unwrap_or(1)
      .min(num_blocks)
  };
  let scratch_len = unsafe { compression_encode_scratch_buffer_size(codec.algorithm()) };
  let cancelled = AtomicBool::new(false);
  let mut offsets = Vec::with_capacity(num_blocks + 1);
  offsets.push(u32::try_from(table_len).map_err(|_| resource_fork_too_large())?);
  let mut offset = table_len;

  let won = std::thread::scope(|scope| -> Result<bool, Error> {
    let mut receivers = Vec::with_capacity(workers);
    for worker in 0..workers {
      let (sender, receiver) = std::sync::mpsc::sync_channel(1);
      receivers.push(receiver);
      let cancelled = &cancelled;
      scope.spawn(move || {
        let mut scratch = vec![0u8; scratch_len];
        let mut block_index = worker;
        while block_index < num_blocks && !cancelled.load(Ordering::Relaxed) {
          let start = block_index * BLOCK;
          let end = start.saturating_add(BLOCK).min(raw.len());
          let encoded = compress_block_with_codec(&raw[start..end], &mut scratch, codec);
          if sender.send(encoded).is_err() {
            break;
          }
          block_index += workers;
        }
      });
    }

    let result = (|| -> Result<bool, Error> {
      for block_index in 0..num_blocks {
        let Some(block) = receivers[block_index % workers].recv().ok().flatten() else {
          return Ok(false);
        };
        let Some(next_offset) = offset.checked_add(block.len()) else {
          return Ok(false);
        };
        // The fork can only grow from here. Stop early rather than writing a
        // multi-gigabyte losing candidate before trying the fallback codec.
        if next_offset >= raw.len() || next_offset > u32::MAX as usize {
          return Ok(false);
        }
        writer.write_all(&block).map_err(|source| Error::Io {
          context: "write resource-fork block",
          source,
        })?;
        offset = next_offset;
        offsets.push(u32::try_from(offset).map_err(|_| resource_fork_too_large())?);
      }
      Ok(true)
    })();
    cancelled.store(true, Ordering::Relaxed);
    drop(receivers);
    result
  })?;

  if !won {
    return Ok(false);
  }
  debug_assert_eq!(offsets.len(), num_blocks + 1);
  let mut table = Vec::with_capacity(table_len);
  for offset in offsets {
    table.extend_from_slice(&offset.to_le_bytes());
  }
  debug_assert_eq!(table.len(), table_len);
  writer
    .seek(std::io::SeekFrom::Start(0))
    .and_then(|_| writer.write_all(&table))
    .and_then(|_| writer.flush())
    .map_err(|source| Error::Io {
      context: "finish resource fork",
      source,
    })?;
  writer.get_ref().sync_all().map_err(|source| Error::Io {
    context: "sync resource fork",
    source,
  })?;
  Ok(true)
}

/// Large assets favor ratio-first LZFSE so we do not write and discard a huge
/// LZVN candidate like Gemini's. LZVN remains a codec-specific fallback.
fn build_streaming_resource_fork(path: &Path, raw: &[u8]) -> Result<Option<Codec>, Error> {
  for codec in [Codec::Lzfse, Codec::Lzvn] {
    if write_streaming_resource_fork(path, raw, codec)? {
      return Ok(Some(codec));
    }
  }
  Ok(None)
}

enum StreamingState {
  Encoding(StreamingEncoding),
  Plain(std::fs::File),
  Closed,
}

struct StreamingEncoding {
  file: std::fs::File,
  fork: std::io::BufWriter<std::fs::File>,
  scratch: Vec<u8>,
  partial: Vec<u8>,
  offsets: Vec<u32>,
  encoded_offset: usize,
}

/// Incremental macOS writer used by the public streaming API. Raw input is held
/// only until the current 64 KiB block is complete; winning LZFSE blocks land
/// directly in the named resource fork. If the fork stops winning, its completed
/// blocks are decoded into a plain sibling and subsequent input streams there.
pub(crate) struct StreamingWriter {
  path: std::path::PathBuf,
  expected_len: usize,
  written: usize,
  state: StreamingState,
  complete: bool,
}

impl StreamingEncoding {
  fn write_block(&mut self, raw: &[u8], expected_len: usize) -> Result<bool, Error> {
    let Some(encoded) = compress_block_with_codec(raw, &mut self.scratch, Codec::Lzfse) else {
      return Ok(false);
    };
    // Verify every encoder result while the matching raw block is still in
    // memory. The final kernel oracle then only has to prove the decmpfs layout.
    let mut decoded = vec![0u8; raw.len()];
    let decoded_len = unsafe {
      compression_decode_buffer(
        decoded.as_mut_ptr(),
        decoded.len(),
        encoded.as_ptr(),
        encoded.len(),
        std::ptr::null_mut(),
        Codec::Lzfse.algorithm(),
      )
    };
    if decoded_len != raw.len() || decoded != raw {
      return Ok(false);
    }
    let Some(next_offset) = self.encoded_offset.checked_add(encoded.len()) else {
      return Ok(false);
    };
    if next_offset >= expected_len || next_offset > u32::MAX as usize {
      return Ok(false);
    }
    use std::io::Write;
    self.fork.write_all(&encoded).map_err(|source| Error::Io {
      context: "write streaming resource-fork block",
      source,
    })?;
    self.encoded_offset = next_offset;
    self
      .offsets
      .push(u32::try_from(next_offset).map_err(|_| resource_fork_too_large())?);
    Ok(true)
  }
}

fn streaming_fallback_path(path: &Path) -> std::path::PathBuf {
  static FALLBACK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let seq = FALLBACK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  let name = path.file_name().map_or_else(
    || std::borrow::Cow::Borrowed("stream"),
    |n| n.to_string_lossy(),
  );
  path.with_file_name(format!(".{name}.plain-{}-{seq}.tmp", std::process::id()))
}

fn decode_streaming_prefix(
  path: &Path,
  encoding: &mut StreamingEncoding,
  current: &[u8],
  expected_len: usize,
) -> Result<(std::path::PathBuf, std::fs::File), Error> {
  use std::io::{Read, Seek, Write};

  encoding.fork.flush().map_err(|source| Error::Io {
    context: "flush streaming resource fork",
    source,
  })?;
  encoding
    .fork
    .get_ref()
    .sync_all()
    .map_err(|source| Error::Io {
      context: "sync streaming resource fork",
      source,
    })?;
  let fork_path = path.join("..namedfork").join("rsrc");
  let mut fork = std::fs::File::open(fork_path).map_err(|source| Error::Io {
    context: "open streaming resource fork for fallback",
    source,
  })?;
  let fallback = streaming_fallback_path(path);
  let mut plain = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .create_new(true)
    .open(&fallback)
    .map_err(|source| Error::Io {
      context: "create streaming plain fallback",
      source,
    })?;

  let decoded = (|| -> Result<(), Error> {
    for (block_index, pair) in encoding.offsets.windows(2).enumerate() {
      let start = pair[0] as u64;
      let encoded_len = (pair[1] - pair[0]) as usize;
      let mut encoded = vec![0u8; encoded_len];
      fork
        .seek(std::io::SeekFrom::Start(start))
        .and_then(|_| fork.read_exact(&mut encoded))
        .map_err(|source| Error::Io {
          context: "read streaming resource fork for fallback",
          source,
        })?;
      let raw_len = expected_len
        .saturating_sub(block_index.saturating_mul(BLOCK))
        .min(BLOCK);
      let mut raw = vec![0u8; raw_len];
      let raw_len = unsafe {
        compression_decode_buffer(
          raw.as_mut_ptr(),
          raw.len(),
          encoded.as_ptr(),
          encoded.len(),
          std::ptr::null_mut(),
          Codec::Lzfse.algorithm(),
        )
      };
      if raw_len != raw.len() {
        return Err(Error::Io {
          context: "decode streaming resource fork for fallback",
          source: std::io::Error::from(std::io::ErrorKind::InvalidData),
        });
      }
      plain.write_all(&raw).map_err(|source| Error::Io {
        context: "write streaming plain fallback",
        source,
      })?;
    }
    plain.write_all(current).map_err(|source| Error::Io {
      context: "write current streaming fallback block",
      source,
    })
  })();
  if let Err(err) = decoded {
    drop(plain);
    let _ = std::fs::remove_file(&fallback);
    return Err(err);
  }
  Ok((fallback, plain))
}

fn streaming_kernel_matches(
  path: &Path,
  encoding: &StreamingEncoding,
  expected_len: usize,
) -> Result<bool, Error> {
  use std::io::{Read, Seek};

  let mut logical = match std::fs::File::open(path) {
    Ok(file) => file,
    Err(_) => return Ok(false),
  };
  let fork_path = path.join("..namedfork").join("rsrc");
  let mut fork = std::fs::File::open(fork_path).map_err(|source| Error::Io {
    context: "open finished streaming resource fork",
    source,
  })?;
  for (block_index, pair) in encoding.offsets.windows(2).enumerate() {
    let encoded_len = (pair[1] - pair[0]) as usize;
    let mut encoded = vec![0u8; encoded_len];
    fork
      .seek(std::io::SeekFrom::Start(pair[0] as u64))
      .and_then(|_| fork.read_exact(&mut encoded))
      .map_err(|source| Error::Io {
        context: "read finished streaming resource fork",
        source,
      })?;
    let raw_len = expected_len
      .saturating_sub(block_index.saturating_mul(BLOCK))
      .min(BLOCK);
    let mut decoded = vec![0u8; raw_len];
    let decoded_len = unsafe {
      compression_decode_buffer(
        decoded.as_mut_ptr(),
        decoded.len(),
        encoded.as_ptr(),
        encoded.len(),
        std::ptr::null_mut(),
        Codec::Lzfse.algorithm(),
      )
    };
    if decoded_len != raw_len {
      return Ok(false);
    }
    let mut kernel = vec![0u8; raw_len];
    if logical.read_exact(&mut kernel).is_err() || kernel != decoded {
      return Ok(false);
    }
  }
  let mut extra = [0u8; 1];
  Ok(logical.read(&mut extra).is_ok_and(|len| len == 0))
}

impl StreamingWriter {
  pub(crate) fn new(path: &Path, expected_len: usize) -> Result<Self, Error> {
    let num_blocks = expected_len.div_ceil(BLOCK).max(1);
    let table_len = resource_fork_table_len(num_blocks)?;
    if expected_len == 0 || table_len >= expected_len || table_len > u32::MAX as usize {
      let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| Error::Io {
          context: "create streaming plain temp",
          source,
        })?;
      return Ok(Self {
        path: path.to_path_buf(),
        expected_len,
        written: 0,
        state: StreamingState::Plain(file),
        complete: false,
      });
    }

    let file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create_new(true)
      .open(path)
      .map_err(|source| Error::Io {
        context: "create streaming decmpfs temp",
        source,
      })?;
    let fork_file = (|| -> Result<std::fs::File, Error> {
      let fork_path = path.join("..namedfork").join("rsrc");
      let mut fork_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(fork_path)
        .map_err(|source| Error::Io {
          context: "open streaming resource fork",
          source,
        })?;
      use std::io::Seek;
      fork_file
        .set_len(table_len as u64)
        .map_err(|source| Error::Io {
          context: "reserve streaming resource-fork table",
          source,
        })?;
      fork_file
        .seek(std::io::SeekFrom::Start(table_len as u64))
        .map_err(|source| Error::Io {
          context: "seek streaming resource-fork payload",
          source,
        })?;
      Ok(fork_file)
    })();
    let fork_file = match fork_file {
      Ok(fork_file) => fork_file,
      Err(error) => {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
      }
    };
    let scratch_len = unsafe { compression_encode_scratch_buffer_size(Codec::Lzfse.algorithm()) };
    Ok(Self {
      path: path.to_path_buf(),
      expected_len,
      written: 0,
      state: StreamingState::Encoding(StreamingEncoding {
        file,
        fork: std::io::BufWriter::with_capacity(1 << 20, fork_file),
        scratch: vec![0u8; scratch_len],
        partial: Vec::with_capacity(BLOCK),
        offsets: vec![u32::try_from(table_len).map_err(|_| resource_fork_too_large())?],
        encoded_offset: table_len,
      }),
      complete: false,
    })
  }

  fn switch_to_plain(&mut self, current: &[u8]) -> Result<(), Error> {
    let StreamingState::Encoding(mut encoding) =
      std::mem::replace(&mut self.state, StreamingState::Closed)
    else {
      return Err(Error::Io {
        context: "switch streaming writer to plain",
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
      });
    };
    let (fallback, mut plain) =
      decode_streaming_prefix(&self.path, &mut encoding, current, self.expected_len)?;
    drop(encoding);
    if let Err(source) = std::fs::remove_file(&self.path) {
      let _ = std::fs::remove_file(&fallback);
      return Err(Error::Io {
        context: "remove streaming decmpfs temp",
        source,
      });
    }
    if let Err(source) = std::fs::rename(&fallback, &self.path) {
      let _ = std::fs::remove_file(&fallback);
      return Err(Error::Io {
        context: "adopt streaming plain fallback",
        source,
      });
    }
    use std::io::Seek;
    plain
      .seek(std::io::SeekFrom::End(0))
      .map_err(|source| Error::Io {
        context: "seek streaming plain fallback",
        source,
      })?;
    self.state = StreamingState::Plain(plain);
    Ok(())
  }

  pub(crate) fn write_all(&mut self, mut input: &[u8]) -> Result<(), Error> {
    let next_written = self
      .written
      .checked_add(input.len())
      .filter(|&len| len <= self.expected_len)
      .ok_or_else(|| Error::Io {
        context: "stream exceeds expected length",
        source: std::io::Error::from(std::io::ErrorKind::InvalidData),
      })?;

    while !input.is_empty() {
      match &mut self.state {
        StreamingState::Plain(file) => {
          use std::io::Write;
          file.write_all(input).map_err(|source| Error::Io {
            context: "write streaming plain temp",
            source,
          })?;
          input = &[];
        }
        StreamingState::Encoding(encoding) => {
          let take = (BLOCK - encoding.partial.len()).min(input.len());
          encoding.partial.extend_from_slice(&input[..take]);
          input = &input[take..];
          if encoding.partial.len() == BLOCK {
            let block = std::mem::replace(&mut encoding.partial, Vec::with_capacity(BLOCK));
            if !encoding.write_block(&block, self.expected_len)? {
              self.switch_to_plain(&block)?;
            }
          }
        }
        StreamingState::Closed => {
          return Err(Error::Io {
            context: "write closed streaming writer",
            source: std::io::Error::from(std::io::ErrorKind::BrokenPipe),
          });
        }
      }
    }
    self.written = next_written;
    Ok(())
  }

  pub(crate) fn finish(&mut self) -> Result<bool, Error> {
    if self.written != self.expected_len {
      return Err(Error::Io {
        context: "finish incomplete streaming writer",
        source: std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
      });
    }
    let partial = match &mut self.state {
      StreamingState::Encoding(encoding) if !encoding.partial.is_empty() => Some(
        std::mem::replace(&mut encoding.partial, Vec::with_capacity(BLOCK)),
      ),
      _ => None,
    };
    if let Some(block) = partial {
      let won = match &mut self.state {
        StreamingState::Encoding(encoding) => encoding.write_block(&block, self.expected_len)?,
        _ => false,
      };
      if !won {
        self.switch_to_plain(&block)?;
      }
    }

    let compressed = match std::mem::replace(&mut self.state, StreamingState::Closed) {
      StreamingState::Plain(file) => {
        file.sync_all().map_err(|source| Error::Io {
          context: "sync streaming plain temp",
          source,
        })?;
        false
      }
      StreamingState::Encoding(mut encoding) => {
        use std::io::{Seek, Write};
        let mut table = Vec::with_capacity(encoding.offsets.len() * std::mem::size_of::<u32>());
        for offset in &encoding.offsets {
          table.extend_from_slice(&offset.to_le_bytes());
        }
        encoding
          .fork
          .seek(std::io::SeekFrom::Start(0))
          .and_then(|_| encoding.fork.write_all(&table))
          .and_then(|_| encoding.fork.flush())
          .map_err(|source| Error::Io {
            context: "finish streaming resource fork",
            source,
          })?;
        encoding
          .fork
          .get_ref()
          .sync_all()
          .map_err(|source| Error::Io {
            context: "sync finished streaming resource fork",
            source,
          })?;
        let cpath = cstring(&self.path)?;
        setxattr(
          &cpath,
          c"com.apple.decmpfs",
          &decmpfs_header(Codec::Lzfse, self.expected_len),
        )?;
        if unsafe { libc::fchflags(encoding.file.as_raw_fd(), UF_COMPRESSED) } != 0 {
          return Err(io("fchflags streaming temp"));
        }
        encoding.file.sync_all().map_err(|source| Error::Io {
          context: "sync streaming decmpfs temp",
          source,
        })?;
        if streaming_kernel_matches(&self.path, &encoding, self.expected_len)? {
          true
        } else {
          let (fallback, plain) =
            decode_streaming_prefix(&self.path, &mut encoding, &[], self.expected_len)?;
          drop(encoding);
          std::fs::remove_file(&self.path).map_err(|source| Error::Io {
            context: "remove failed streaming decmpfs oracle",
            source,
          })?;
          if let Err(source) = std::fs::rename(&fallback, &self.path) {
            let _ = std::fs::remove_file(&fallback);
            return Err(Error::Io {
              context: "publish streaming oracle fallback",
              source,
            });
          }
          plain.sync_all().map_err(|source| Error::Io {
            context: "sync streaming oracle fallback",
            source,
          })?;
          false
        }
      }
      StreamingState::Closed => {
        return Err(Error::Io {
          context: "finish closed streaming writer",
          source: std::io::Error::from(std::io::ErrorKind::BrokenPipe),
        });
      }
    };
    self.complete = true;
    Ok(compressed)
  }
}

impl Drop for StreamingWriter {
  fn drop(&mut self) {
    if !self.complete {
      self.state = StreamingState::Closed;
      let _ = std::fs::remove_file(&self.path);
    }
  }
}

fn decmpfs_header(codec: Codec, raw_len: usize) -> [u8; 16] {
  let mut header = [0u8; 16];
  header[..4].copy_from_slice(&DECMPFS_MAGIC.to_le_bytes());
  header[4..8].copy_from_slice(&codec.compression_type().to_le_bytes());
  header[8..].copy_from_slice(&(raw_len as u64).to_le_bytes());
  header
}

fn setxattr(path: &std::ffi::CStr, name: &std::ffi::CStr, value: &[u8]) -> Result<(), Error> {
  let rc = unsafe {
    libc::setxattr(
      path.as_ptr(),
      name.as_ptr(),
      value.as_ptr().cast(),
      value.len(),
      0,
      XATTR_NOFOLLOW,
    )
  };
  if rc != 0 {
    return Err(io("setxattr"));
  }
  Ok(())
}

pub(crate) fn apply_inplace(path: &Path, snapshot: &[u8]) -> Result<(), Error> {
  // Fail-soft: skip if we can't write the original (by mode or ownership) — the
  // temp+rename below would otherwise replace even a file we can't open for write.
  let cpath = cstring(path)?;
  if unsafe { libc::access(cpath.as_ptr(), libc::W_OK) } != 0 {
    return Err(io("access"));
  }

  // `snapshot` is the file's bytes the caller already read for rollback — reuse
  // it instead of a second full read.
  let mode = std::fs::metadata(path).map(|m| m.permissions()).ok();
  apply_bytes(path, snapshot, mode)
}

/// Write `content` to `path` as a fresh decmpfs-compressed file in ONE pass — no
/// write-then-read-back. The decmpfs is built directly from `content`, dropped on a
/// sibling temp (empty data fork + the two xattrs + UF_COMPRESSED when compression
/// wins; ordinary data fork on no gain), then atomically renamed over `path`. A
/// crash can only leave the original or the finished file; the rename also gives a
/// fresh inode, the copy-break from any pnpm CAS hardlink siblings. This is the
/// one-pass core both `compress_bytes` (no original) and `apply_inplace` (read
/// first) share.
pub(crate) fn apply_bytes(
  path: &Path,
  content: &[u8],
  mode: Option<std::fs::Permissions>,
) -> Result<(), Error> {
  apply_bytes_with_streaming_threshold(path, content, mode, STREAMING_THRESHOLD)
}

fn apply_bytes_with_streaming_threshold(
  path: &Path,
  content: &[u8],
  mode: Option<std::fs::Permissions>,
  streaming_threshold: usize,
) -> Result<(), Error> {
  let stream = should_stream_resource_fork(content.len(), streaming_threshold);
  let in_memory_resource_fork = if stream {
    None
  } else {
    build_in_memory_resource_fork(content)?
  };

  let dir = path.parent().ok_or_else(|| io("parent"))?;
  let name = path
    .file_name()
    .ok_or_else(|| io("file_name"))?
    .to_string_lossy();
  // Uniqueness beyond the PID: a crash can leave `.name.decmpfs-<pid>.tmp`, and a
  // later run that reuses that PID (common after reboot) would fail `create_new`
  // forever. PID + wall-clock nanos + a process-local counter makes a stale
  // sibling collision astronomically unlikely while keeping `create_new`'s
  // concurrent-writer safety.
  static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_nanos())
    .unwrap_or(0);
  let tmp = dir.join(format!(
    ".{name}.decmpfs-{}-{nanos}-{seq}.tmp",
    std::process::id()
  ));

  let build = (|| -> Result<(), Error> {
    let create_temp = || {
      std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|source| Error::Io {
          context: "create temp",
          source,
        })
    };
    let mut file = create_temp()?;
    let ctmp = cstring(&tmp)?;
    let codec = if stream {
      build_streaming_resource_fork(&tmp, content)?
    } else if let Some(resource_fork) = &in_memory_resource_fork {
      // Install the payload before publishing metadata that tells the kernel to
      // decode it. The temp inode is not visible at the destination yet either
      // way, but this ordering also keeps direct temp-path observers safe.
      setxattr(&ctmp, c"com.apple.ResourceFork", &resource_fork.bytes)?;
      Some(resource_fork.codec)
    } else {
      None
    };

    if let Some(codec) = codec {
      setxattr(
        &ctmp,
        c"com.apple.decmpfs",
        &decmpfs_header(codec, content.len()),
      )?;
      if unsafe { libc::fchflags(file.as_raw_fd(), UF_COMPRESSED) } != 0 {
        return Err(io("fchflags"));
      }
    } else {
      // A losing streamed attempt left a partial resource fork on the temp.
      // Recreate the inode rather than publish a plain file with stale fork data.
      if stream {
        drop(file);
        std::fs::remove_file(&tmp).map_err(|source| Error::Io {
          context: "remove losing streamed temp",
          source,
        })?;
        file = create_temp()?;
      }
      use std::io::Write;
      file.write_all(content).map_err(|source| Error::Io {
        context: "plain temp write",
        source,
      })?;
      file.sync_all().map_err(|source| Error::Io {
        context: "plain temp sync",
        source,
      })?;
    }
    Ok(())
  })();

  if let Err(e) = build {
    let _ = std::fs::remove_file(&tmp);
    return Err(e);
  }
  if let Some(perm) = mode {
    let _ = std::fs::set_permissions(&tmp, perm);
  }
  // Preserve ownership across the rewrite. Running as root (a global npm install,
  // a Docker build) the temp is created owned by the current euid, so the rename
  // would otherwise change the file's owner. Match the original's uid/gid.
  // Best-effort: a no-op for a new path (nothing to preserve) and for a non-root
  // process (chown to another owner is EPERM — but then the file was already
  // ours, so nothing changes).
  if let Ok(meta) = std::fs::metadata(path) {
    use std::os::unix::fs::MetadataExt;
    let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
  }
  std::fs::rename(&tmp, path).map_err(|source| {
    let _ = std::fs::remove_file(&tmp);
    Error::Io {
      context: "rename",
      source,
    }
  })
}

/// Copy-on-write clone via `clonefile(2)` — shares the extents AND the decmpfs
/// state, so a compressed source stays compressed at zero cost. `Ok(false)`
/// means "cannot clone here" (cross-volume, unsupported FS, …) and the caller
/// falls back to a byte copy; a failed clonefile never leaves a partial
/// destination.
pub(crate) fn clone_file(src: &Path, dest: &Path) -> Result<bool, Error> {
  let csrc = cstring(src)?;
  let cdest = cstring(dest)?;
  Ok(unsafe { libc::clonefile(csrc.as_ptr(), cdest.as_ptr(), 0) } == 0)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
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
}
