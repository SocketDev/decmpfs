use super::*;

pub(super) enum StreamingState {
    Encoding(StreamingEncoding),
    Plain(std::fs::File),
    Closed,
}

pub(super) struct StreamingEncoding {
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
        self.offsets
            .push(u32::try_from(next_offset).map_err(|_| resource_fork_too_large())?);
        Ok(true)
    }
}

pub(super) fn streaming_fallback_path(path: &Path) -> std::path::PathBuf {
    static FALLBACK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = FALLBACK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map_or_else(
        || std::borrow::Cow::Borrowed("stream"),
        |n| n.to_string_lossy(),
    );
    path.with_file_name(format!(".{name}.plain-{}-{seq}.tmp", std::process::id()))
}

pub(super) fn decode_streaming_prefix(
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
            fork.seek(std::io::SeekFrom::Start(start))
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

pub(super) fn streaming_kernel_matches(
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
        fork.seek(std::io::SeekFrom::Start(pair[0] as u64))
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
        let scratch_len =
            unsafe { compression_encode_scratch_buffer_size(Codec::Lzfse.algorithm()) };
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
                        let block =
                            std::mem::replace(&mut encoding.partial, Vec::with_capacity(BLOCK));
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
                StreamingState::Encoding(encoding) => {
                    encoding.write_block(&block, self.expected_len)?
                }
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
                let mut table =
                    Vec::with_capacity(encoding.offsets.len() * std::mem::size_of::<u32>());
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
