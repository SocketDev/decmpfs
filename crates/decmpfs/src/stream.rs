//! Incremental, atomic writes into the platform's transparent compression.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{backend, verify, Error, Gate, Outcome, SkipReason, Support, UnsupportedReason};

enum PlainOutcome {
  Skipped(SkipReason),
  Unsupported(UnsupportedReason),
}

enum StreamInner {
  #[cfg(target_os = "macos")]
  Macos(backend::StreamingWriter),
  #[cfg(target_os = "macos")]
  MacosBuffered(Vec<u8>),
  File(std::fs::File),
  Closed,
}

/// An atomic incremental writer for OS-transparent filesystem compression.
///
/// The expected logical length is required before the first byte. macOS uses it
/// to reserve the decmpfs block-offset table; btrfs and NTFS use it to enforce
/// complete publication. Dropping or aborting the writer removes its sibling
/// temp, leaving any existing destination untouched.
pub struct DecmpfsWriter {
  target: PathBuf,
  temp: PathBuf,
  expected_len: u64,
  written: u64,
  inner: StreamInner,
  plain_outcome: Option<PlainOutcome>,
  finished: bool,
}

fn stream_error(context: &'static str, kind: std::io::ErrorKind) -> Error {
  Error::Io {
    context,
    source: std::io::Error::from(kind),
  }
}

fn unique_temp(path: &Path) -> Result<PathBuf, Error> {
  static STREAM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let dir = path.parent().ok_or_else(|| {
    stream_error(
      "stream target has no parent",
      std::io::ErrorKind::InvalidInput,
    )
  })?;
  let name = path.file_name().map_or_else(
    || std::borrow::Cow::Borrowed("stream"),
    |n| n.to_string_lossy(),
  );
  let seq = STREAM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|duration| duration.as_nanos())
    .unwrap_or(0);
  Ok(dir.join(format!(
    ".{name}.decmpfs-stream-{}-{nanos}-{seq}.tmp",
    std::process::id()
  )))
}

fn create_plain(temp: &Path) -> Result<std::fs::File, Error> {
  std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .create_new(true)
    .open(temp)
    .map_err(|source| Error::Io {
      context: "create streaming temp",
      source,
    })
}

fn publish(temp: &Path, target: &Path) -> Result<(), Error> {
  #[cfg(target_os = "windows")]
  {
    backend::publish_stream(temp, target)
  }
  #[cfg(not(target_os = "windows"))]
  {
    std::fs::rename(temp, target).map_err(|source| Error::Io {
      context: "publish streaming temp",
      source,
    })
  }
}

impl DecmpfsWriter {
  /// Open an incremental writer. The destination is published only by
  /// [`finish`](Self::finish) after exactly `expected_len` bytes arrive.
  pub fn create(path: &Path, expected_len: u64, gate: &Gate) -> Result<Self, Error> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let temp = unique_temp(path)?;
    if !gate.matches(&normalized, expected_len) {
      return Ok(Self {
        target: path.to_path_buf(),
        temp: temp.clone(),
        expected_len,
        written: 0,
        inner: StreamInner::File(create_plain(&temp)?),
        plain_outcome: Some(PlainOutcome::Skipped(SkipReason::GateExcluded)),
        finished: false,
      });
    }

    let probe = path.parent().unwrap_or(path);
    let support =
      backend::detect(probe).unwrap_or(Support::Unsupported(UnsupportedReason::Filesystem));
    match support {
      Support::Supported => {
        #[cfg(target_os = "macos")]
        let inner = match usize::try_from(expected_len) {
          Ok(len) if len <= backend::STREAMING_THRESHOLD => {
            StreamInner::MacosBuffered(Vec::with_capacity(len))
          }
          Ok(len) => StreamInner::Macos(backend::StreamingWriter::new(&temp, len)?),
          Err(_) => StreamInner::File(create_plain(&temp)?),
        };
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        let (inner, plain_outcome) = {
          let file = create_plain(&temp)?;
          match backend::prepare_stream(&file) {
            Ok(()) => (StreamInner::File(file), None),
            Err(_) => {
              drop(file);
              let _ = std::fs::remove_file(&temp);
              (
                StreamInner::File(create_plain(&temp)?),
                Some(PlainOutcome::Skipped(SkipReason::IntegrityRevert)),
              )
            }
          }
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let (inner, plain_outcome) = (
          StreamInner::File(create_plain(&temp)?),
          Some(PlainOutcome::Unsupported(UnsupportedReason::PlatformBuild)),
        );
        #[cfg(target_os = "macos")]
        let plain_outcome = if usize::try_from(expected_len).is_ok() {
          None
        } else {
          Some(PlainOutcome::Skipped(SkipReason::TooLarge))
        };
        Ok(Self {
          target: path.to_path_buf(),
          temp,
          expected_len,
          written: 0,
          inner,
          plain_outcome,
          finished: false,
        })
      }
      Support::AlreadyCompressed => Ok(Self {
        target: path.to_path_buf(),
        temp: temp.clone(),
        expected_len,
        written: 0,
        inner: StreamInner::File(create_plain(&temp)?),
        plain_outcome: Some(PlainOutcome::Unsupported(UnsupportedReason::Filesystem)),
        finished: false,
      }),
      Support::Unsupported(reason) => Ok(Self {
        target: path.to_path_buf(),
        temp: temp.clone(),
        expected_len,
        written: 0,
        inner: StreamInner::File(create_plain(&temp)?),
        plain_outcome: Some(PlainOutcome::Unsupported(reason)),
        finished: false,
      }),
    }
  }

  /// Consume one raw chunk without retaining it after the current filesystem
  /// compression block has been completed.
  pub fn write_chunk(&mut self, buf: &[u8]) -> Result<(), Error> {
    let next = self
      .written
      .checked_add(buf.len() as u64)
      .filter(|&len| len <= self.expected_len)
      .ok_or_else(|| {
        stream_error(
          "stream exceeds expected length",
          std::io::ErrorKind::InvalidData,
        )
      })?;
    match &mut self.inner {
      #[cfg(target_os = "macos")]
      StreamInner::Macos(writer) => writer.write_all(buf)?,
      #[cfg(target_os = "macos")]
      StreamInner::MacosBuffered(buffer) => buffer.extend_from_slice(buf),
      StreamInner::File(file) => file.write_all(buf).map_err(|source| Error::Io {
        context: "write streaming temp",
        source,
      })?,
      StreamInner::Closed => {
        return Err(stream_error(
          "write closed stream",
          std::io::ErrorKind::BrokenPipe,
        ));
      }
    }
    self.written = next;
    Ok(())
  }

  /// Remove the incomplete sibling temp and leave the destination untouched.
  pub fn abort(mut self) -> Result<(), Error> {
    self.inner = StreamInner::Closed;
    self.finished = true;
    match std::fs::remove_file(&self.temp) {
      Ok(()) => Ok(()),
      Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
      Err(source) => Err(Error::Io {
        context: "abort streaming temp",
        source,
      }),
    }
  }

  /// Flush, verify the received length, and atomically publish the destination.
  pub fn finish(mut self) -> Result<Outcome, Error> {
    if self.written != self.expected_len {
      return Err(stream_error(
        "finish incomplete stream",
        std::io::ErrorKind::UnexpectedEof,
      ));
    }

    #[cfg(target_os = "macos")]
    let macos_compressed = match &mut self.inner {
      StreamInner::Macos(writer) => Some(writer.finish()?),
      StreamInner::MacosBuffered(buffer) => {
        backend::apply_bytes(&self.temp, buffer, None)?;
        Some(backend::is_already_compressed(&self.temp).unwrap_or(false))
      }
      _ => None,
    };
    #[cfg(not(target_os = "macos"))]
    let macos_compressed: Option<bool> = None;

    if let StreamInner::File(file) = &mut self.inner {
      file
        .flush()
        .and_then(|()| file.sync_all())
        .map_err(|source| Error::Io {
          context: "finish streaming temp",
          source,
        })?;
    }
    self.inner = StreamInner::Closed;
    publish(&self.temp, &self.target)?;
    self.finished = true;

    if let Some(plain) = self.plain_outcome.take() {
      return Ok(match plain {
        PlainOutcome::Skipped(reason) => Outcome::Skipped { reason },
        PlainOutcome::Unsupported(reason) => Outcome::Unsupported { reason },
      });
    }
    if macos_compressed == Some(false) {
      return Ok(Outcome::NoGain {
        before: self.expected_len,
        after: verify::on_disk_bytes(&self.target)?,
      });
    }

    let after = verify::on_disk_bytes(&self.target)?;
    let signal = backend::compressed_on_disk(&self.target).ok().flatten();
    if signal.unwrap_or(after < self.expected_len) {
      Ok(Outcome::Compressed {
        before: self.expected_len,
        after,
      })
    } else {
      Ok(Outcome::NoGain {
        before: self.expected_len,
        after,
      })
    }
  }
}

impl Write for DecmpfsWriter {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    self.write_chunk(buf).map_err(std::io::Error::other)?;
    Ok(buf.len())
  }

  fn flush(&mut self) -> std::io::Result<()> {
    match &mut self.inner {
      #[cfg(target_os = "macos")]
      StreamInner::Macos(_) => Ok(()),
      #[cfg(target_os = "macos")]
      StreamInner::MacosBuffered(_) => Ok(()),
      StreamInner::File(file) => file.flush(),
      StreamInner::Closed => Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
    }
  }
}

impl Drop for DecmpfsWriter {
  fn drop(&mut self) {
    if !self.finished {
      self.inner = StreamInner::Closed;
      let _ = std::fs::remove_file(&self.temp);
    }
  }
}
