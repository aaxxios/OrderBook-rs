//! Memory-mapped file journal implementation.
//!
//! [`FileJournal`] persists [`SequencerEvent`] instances to append-only,
//! memory-mapped segment files on disk. Each segment is pre-allocated to a
//! configurable size (default 256 MB) and rotated when full.
//!
//! # On-Disk Entry Format (little-endian)
//!
//! ```text
//! [4 bytes: entry_length][8 bytes: sequence_num][8 bytes: timestamp_ns]
//! [N bytes: JSON payload][4 bytes: CRC32]
//! ```
//!
//! - `entry_length` — total bytes after itself (sequence + timestamp +
//!   payload + CRC = 20 + N).
//! - CRC32 covers: sequence_num ‖ timestamp_ns ‖ payload (not
//!   `entry_length`).
//! - A zero `entry_length` marks the end of written data (segments are
//!   zero-filled when created). Every other value must describe a
//!   well-formed entry inside the segment: readers and
//!   [`Journal::verify_integrity`] report anything else as
//!   [`JournalError::InvalidEntryHeader`] instead of treating it as the end
//!   of the segment (#252).
//!
//! # Segment Files
//!
//! Segments are named `segment-{start_sequence:020}.journal` and stored in
//! the configured journal directory. Archived segments are renamed to
//! `.journal.archived`. Segments are created with `create_new` and never
//! truncated; rotation onto an existing file fails with
//! [`JournalError::SegmentExists`].
//!
//! # Crash Recovery
//!
//! Every append is flushed (`msync`) before it returns, so a crash can tear
//! at most the entry being written. On reopen the latest segment is walked
//! entry by entry with CRC validation; the first entry that does not
//! validate is a torn tail **only** if nothing valid follows it. The write
//! position is set to its start and every non-zero byte from there to the
//! end of the segment is zeroed and flushed, so neither the torn entry nor
//! stale bytes past a later, shorter append can ever decode as a header.
//! A damaged entry followed by a valid one is mid-segment corruption, not a
//! torn tail: [`FileJournal::open`] refuses it with a typed error rather
//! than overwrite the valid entries after it. The on-disk format is
//! unchanged; valid journals written by earlier releases open as before.

use super::error::JournalError;
use super::journal::{ENTRY_CRC_SIZE, Journal, JournalEntry, JournalReadIter};
use super::types::SequencerEvent;
use memmap2::MmapMut;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use tracing::{error, info, warn};

/// Default segment size in bytes (256 MB).
const DEFAULT_SEGMENT_SIZE: usize = 256 * 1024 * 1024;

/// Size of the `entry_length` field in bytes.
const ENTRY_LENGTH_SIZE: usize = 4;

/// Size of the `sequence_num` field in bytes.
const SEQUENCE_SIZE: usize = 8;

/// Size of the `timestamp_ns` field in bytes.
const TIMESTAMP_SIZE: usize = 8;

/// Bytes counted by `entry_length` besides the payload: sequence, timestamp
/// and CRC. The smallest valid `entry_length`.
const ENTRY_FIXED_BODY: usize = SEQUENCE_SIZE + TIMESTAMP_SIZE + ENTRY_CRC_SIZE;

/// Chunk size used when zeroing a recovered segment tail. Only chunks that
/// hold a non-zero byte are written, so the untouched (sparse) tail of a
/// pre-allocated segment is never materialised on disk.
const ZERO_SCAN_CHUNK: usize = 4096;

/// Bytes of the `entry_length` field after its first byte: a candidate
/// entry start lies at most this far before a non-zero byte.
const LENGTH_FIELD_TAIL: usize = ENTRY_LENGTH_SIZE - 1;

/// Offset of the JSON payload from the start of the entry body (after
/// `entry_length`): sequence plus timestamp.
const PAYLOAD_OFFSET: usize = SEQUENCE_SIZE + TIMESTAMP_SIZE;

// ─── Byte decoding ──────────────────────────────────────────────────────────

/// Reads a little-endian `u32` at `offset`, or `None` if out of bounds.
#[inline]
#[must_use]
fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..)?.first_chunk::<4>()?;
    Some(u32::from_le_bytes(*bytes))
}

/// Reads a little-endian `u64` at `offset`, or `None` if out of bounds.
#[inline]
#[must_use]
fn read_u64_le(data: &[u8], offset: usize) -> Option<u64> {
    let bytes = data.get(offset..)?.first_chunk::<8>()?;
    Some(u64::from_le_bytes(*bytes))
}

/// Builds an [`JournalError::InvalidEntryHeader`].
#[cold]
#[inline(never)]
fn invalid_header(offset: usize, message: impl Into<String>) -> JournalError {
    JournalError::InvalidEntryHeader {
        offset,
        message: message.into(),
    }
}

/// A framed entry located in a segment. Its bounds are validated; its CRC
/// may not match (see [`Self::crc_ok`]).
#[derive(Debug, Clone, Copy)]
struct RawEntry {
    /// Offset of the entry's `entry_length` field.
    offset: usize,
    /// Offset one past the entry's CRC (start of the next entry).
    end: usize,
    /// The `sequence_num` header field.
    sequence: u64,
    /// Start of the JSON payload.
    payload_start: usize,
    /// Start of the CRC trailer (end of the payload).
    crc_start: usize,
    /// The CRC stored in the trailer.
    stored_crc: u32,
    /// The CRC computed over `sequence ‖ timestamp ‖ payload`.
    computed_crc: u32,
}

impl RawEntry {
    /// Whether the stored CRC matches the entry bytes.
    #[inline]
    #[must_use]
    fn crc_ok(&self) -> bool {
        self.stored_crc == self.computed_crc
    }

    /// The [`JournalError::CorruptEntry`] describing a CRC mismatch.
    #[cold]
    #[inline(never)]
    fn corrupt(&self) -> JournalError {
        JournalError::CorruptEntry {
            sequence: self.sequence,
            expected_crc: self.stored_crc,
            actual_crc: self.computed_crc,
        }
    }
}

/// What [`decode_entry`] found at an offset.
#[derive(Debug, Clone, Copy)]
enum Decoded {
    /// End of written data: a zero `entry_length`, or fewer than four bytes
    /// left and all of them zero.
    End,
    /// A framed entry.
    Entry(RawEntry),
}

/// Decodes the entry framing at `offset` in `data`.
///
/// Every length and offset read from `data` is untrusted: a non-zero
/// `entry_length` below the minimum, one that runs past the end of `data`,
/// or a truncated `entry_length` field holding non-zero bytes is an
/// [`JournalError::InvalidEntryHeader`]. Only a zero `entry_length` (or a
/// zero-filled remainder shorter than the field) is the end of data. The
/// CRC is computed but not enforced here; see [`RawEntry::crc_ok`].
fn decode_entry(data: &[u8], offset: usize) -> Result<Decoded, JournalError> {
    let rest = data
        .get(offset..)
        .ok_or_else(|| invalid_header(offset, "offset beyond segment data"))?;
    let Some(length_bytes) = rest.first_chunk::<ENTRY_LENGTH_SIZE>() else {
        return if rest.iter().all(|b| *b == 0) {
            Ok(Decoded::End)
        } else {
            Err(invalid_header(offset, "truncated entry_length"))
        };
    };
    let entry_length = u32::from_le_bytes(*length_bytes);
    if entry_length == 0 {
        return Ok(Decoded::End);
    }
    let entry_length = usize::try_from(entry_length)
        .map_err(|_| invalid_header(offset, "entry_length does not fit usize"))?;
    if entry_length < ENTRY_FIXED_BODY {
        return Err(invalid_header(
            offset,
            format!("entry_length {entry_length} below the {ENTRY_FIXED_BODY}-byte minimum"),
        ));
    }
    let body_start = offset
        .checked_add(ENTRY_LENGTH_SIZE)
        .ok_or_else(|| invalid_header(offset, "offset overflow"))?;
    let end = body_start
        .checked_add(entry_length)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| {
            invalid_header(
                offset,
                format!(
                    "truncated entry: entry_length {entry_length} extends beyond segment data ({} bytes)",
                    data.len()
                ),
            )
        })?;
    // `entry_length >= ENTRY_FIXED_BODY` and `end <= data.len()`, so every
    // offset below is in bounds; the checked forms keep that explicit.
    let crc_start = end
        .checked_sub(ENTRY_CRC_SIZE)
        .ok_or_else(|| invalid_header(offset, "entry too small for CRC"))?;
    let payload_start = body_start
        .checked_add(PAYLOAD_OFFSET)
        .ok_or_else(|| invalid_header(offset, "offset overflow"))?;
    let sequence = read_u64_le(data, body_start)
        .ok_or_else(|| invalid_header(offset, "truncated sequence_num"))?;
    let stored_crc =
        read_u32_le(data, crc_start).ok_or_else(|| invalid_header(offset, "truncated CRC"))?;
    let checksummed = data
        .get(body_start..crc_start)
        .ok_or_else(|| invalid_header(offset, "truncated entry body"))?;
    Ok(Decoded::Entry(RawEntry {
        offset,
        end,
        sequence,
        payload_start,
        crc_start,
        stored_crc,
        computed_crc: crc32fast::hash(checksummed),
    }))
}

/// Computes the on-disk `entry_length` for a payload of `payload_len`
/// bytes.
///
/// # Errors
///
/// [`JournalError::EntryTooLarge`] when the entry does not fit the 32-bit
/// `entry_length` field (a payload of ~4 GiB).
fn entry_length_field(payload_len: usize, segment_size: usize) -> Result<u32, JournalError> {
    let body_len =
        payload_len
            .checked_add(ENTRY_FIXED_BODY)
            .ok_or(JournalError::EntryTooLarge {
                entry_bytes: payload_len,
                segment_size,
            })?;
    u32::try_from(body_len).map_err(|_| JournalError::EntryTooLarge {
        entry_bytes: body_len,
        segment_size,
    })
}

/// Walks the valid entry chain of `data` from offset 0 and returns the
/// sequence of the last entry whose CRC validates. Stops silently at the end
/// of data or at the first entry that does not validate.
fn scan_last_sequence(data: &[u8]) -> Option<u64> {
    let mut offset = 0usize;
    let mut last = None;
    while let Ok(Decoded::Entry(raw)) = decode_entry(data, offset) {
        if !raw.crc_ok() {
            break;
        }
        last = Some(raw.sequence);
        offset = raw.end;
    }
    last
}

/// Result of the reopen scan of the latest segment.
#[derive(Debug, Clone, Copy)]
struct Recovery {
    /// End of the last valid entry: where the next append goes.
    write_pos: usize,
    /// Sequence of the last valid entry in the segment.
    last_seq: Option<u64>,
}

/// Scans a reopened segment for its write position, distinguishing a torn
/// tail from mid-segment corruption (see the module docs).
///
/// # Errors
///
/// [`JournalError::CorruptEntry`] when an entry fails its CRC and a valid
/// entry follows it: that is corruption inside committed data, and treating
/// it as a torn tail would overwrite the valid entries after it.
fn recover_segment(data: &[u8], path: &Path) -> Result<Recovery, JournalError> {
    let mut offset = 0usize;
    let mut last_seq = None;
    loop {
        let reason = match decode_entry(data, offset) {
            Ok(Decoded::End) => break,
            Ok(Decoded::Entry(raw)) if raw.crc_ok() => {
                last_seq = Some(raw.sequence);
                offset = raw.end;
                continue;
            }
            Ok(Decoded::Entry(_)) => "CRC mismatch",
            Err(_) => "invalid header",
        };
        // The entry at `offset` does not validate. A torn tail is the last
        // thing written, so no valid entry can follow it anywhere in the
        // segment. Any valid entry after it means the damage is inside
        // committed data (possibly several adjacent entries): refuse.
        if let Some(later) = find_valid_entry_after(data, offset) {
            error!(
                path = %path.display(),
                offset,
                later_offset = later.offset,
                later_sequence = later.sequence,
                "journal corruption inside committed data; refusing to open"
            );
            return Err(match decode_entry(data, offset) {
                Ok(Decoded::Entry(raw)) => raw.corrupt(),
                Err(err) => err,
                Ok(Decoded::End) => invalid_header(offset, "damaged entry"),
            });
        }
        warn!(
            path = %path.display(),
            offset,
            reason,
            "torn journal tail detected on reopen; truncating to the last good entry"
        );
        break;
    }
    Ok(Recovery {
        write_pos: offset,
        last_seq,
    })
}

/// Searches `data` after the damaged entry at `from` for any entry whose
/// framing and CRC validate, at every byte offset.
///
/// A valid entry has a non-zero `entry_length`, so every candidate start
/// lies at most `ENTRY_LENGTH_SIZE - 1` bytes before a non-zero byte; runs of
/// zero bytes (the pre-allocated tail) are skipped with a vectorisable scan
/// instead of being probed byte by byte. A random candidate passes the CRC
/// with probability 2^-32.
fn find_valid_entry_after(data: &[u8], from: usize) -> Option<RawEntry> {
    let mut probe = from.checked_add(1)?;
    loop {
        let rest = data.get(probe..)?;
        let nonzero = probe.checked_add(rest.iter().position(|b| *b != 0)?)?;
        // Candidates whose length field covers the non-zero byte.
        let mut candidate = match nonzero.checked_sub(LENGTH_FIELD_TAIL) {
            Some(start) => start.max(probe),
            None => probe,
        };
        while candidate <= nonzero {
            if let Ok(Decoded::Entry(raw)) = decode_entry(data, candidate)
                && raw.crc_ok()
            {
                return Some(raw);
            }
            candidate = candidate.checked_add(1)?;
        }
        probe = nonzero.checked_add(1)?;
    }
}

/// Zeroes every non-zero byte of `mmap` from `from` to the end and flushes
/// the zeroed range. Returns the number of bytes in the flushed range.
///
/// Chunks that are already zero are left untouched, so a clean, sparse
/// segment tail is read but never written.
fn zero_tail(mmap: &mut MmapMut, from: usize, path: &Path) -> Result<usize, JournalError> {
    let tail = mmap
        .get_mut(from..)
        .ok_or_else(|| invalid_header(from, "write position beyond segment data"))?;
    let mut pos = from;
    let mut dirty: Option<(usize, usize)> = None;
    for chunk in tail.chunks_mut(ZERO_SCAN_CHUNK) {
        let next = pos
            .checked_add(chunk.len())
            .ok_or(JournalError::CounterOverflow {
                counter: "segment zeroing offset",
            })?;
        if chunk.iter().any(|b| *b != 0) {
            chunk.fill(0);
            dirty = Some(match dirty {
                Some((start, _)) => (start, next),
                None => (pos, next),
            });
        }
        pos = next;
    }
    let Some((start, end)) = dirty else {
        return Ok(0);
    };
    let len = end
        .checked_sub(start)
        .ok_or(JournalError::CounterOverflow {
            counter: "segment zeroing range",
        })?;
    mmap.flush_range(start, len).map_err(|e| JournalError::Io {
        message: e.to_string(),
        path: Some(path.to_path_buf()),
    })?;
    Ok(len)
}

// ─── Segment writer ─────────────────────────────────────────────────────────

/// Manages writing to a single memory-mapped segment file.
struct SegmentWriter {
    /// The memory-mapped region for this segment. Its length is the
    /// segment's capacity.
    mmap: MmapMut,
    /// Current write position within the segment (bytes). Invariant:
    /// `write_pos <= mmap.len()`.
    write_pos: usize,
    /// Path to the segment file on disk.
    path: PathBuf,
}

impl SegmentWriter {
    /// Create a new segment file and memory-map it.
    ///
    /// The file is created with `create_new` (an existing file is never
    /// truncated: [`JournalError::SegmentExists`]) and pre-allocated to
    /// `capacity` zero bytes.
    fn create(path: &Path, capacity: usize) -> Result<Self, JournalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| {
                if e.kind() == ErrorKind::AlreadyExists {
                    JournalError::SegmentExists {
                        path: path.to_path_buf(),
                    }
                } else {
                    JournalError::Io {
                        message: e.to_string(),
                        path: Some(path.to_path_buf()),
                    }
                }
            })?;

        match Self::map_new(&file, path, capacity) {
            Ok(mmap) => Ok(Self {
                mmap,
                write_pos: 0,
                path: path.to_path_buf(),
            }),
            Err(err) => {
                // The file was created by this call and holds no entry, so
                // removing it cannot lose data; leaving it would make every
                // retry fail with `SegmentExists`.
                drop(file);
                if let Err(remove_err) = fs::remove_file(path) {
                    warn!(
                        path = %path.display(),
                        error = %remove_err,
                        "failed to remove a segment whose initialisation failed"
                    );
                }
                Err(err)
            }
        }
    }

    /// Sizes a freshly created segment file and maps it.
    fn map_new(file: &File, path: &Path, capacity: usize) -> Result<MmapMut, JournalError> {
        let len = u64::try_from(capacity).map_err(|_| JournalError::EntryTooLarge {
            entry_bytes: capacity,
            segment_size: capacity,
        })?;
        file.set_len(len).map_err(|e| JournalError::Io {
            message: e.to_string(),
            path: Some(path.to_path_buf()),
        })?;

        // SAFETY: The file was just created by this process with
        // `create_new` and is never truncated while the mapping is alive
        // (segments are pre-allocated and never shrunk). This is the single
        // documented `unsafe` exception (doc/panic-boundaries.md).
        #[allow(unsafe_code)]
        let mmap = unsafe {
            MmapMut::map_mut(file).map_err(|e| JournalError::Io {
                message: e.to_string(),
                path: Some(path.to_path_buf()),
            })?
        };
        Ok(mmap)
    }

    /// Open an existing segment file for appending.
    ///
    /// Recovers the write position with [`recover_segment`] and zeroes the
    /// segment past it ([`zero_tail`]). Returns the writer and the last
    /// valid sequence in the segment.
    fn open_existing(path: &Path) -> Result<(Self, Option<u64>), JournalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| JournalError::Io {
                message: e.to_string(),
                path: Some(path.to_path_buf()),
            })?;

        // SAFETY: The file is exclusively owned by this process and will not
        // be truncated or modified externally while the mmap is active.
        #[allow(unsafe_code)]
        let mut mmap = unsafe {
            MmapMut::map_mut(&file).map_err(|e| JournalError::Io {
                message: e.to_string(),
                path: Some(path.to_path_buf()),
            })?
        };

        let recovery = recover_segment(&mmap, path)?;
        let zeroed = zero_tail(&mut mmap, recovery.write_pos, path)?;
        if zeroed > 0 {
            warn!(
                path = %path.display(),
                write_pos = recovery.write_pos,
                zeroed_bytes = zeroed,
                "zeroed stale bytes past the recovered journal write position"
            );
        }

        Ok((
            Self {
                mmap,
                write_pos: recovery.write_pos,
                path: path.to_path_buf(),
            },
            recovery.last_seq,
        ))
    }

    /// Returns the remaining capacity in this segment, in bytes.
    #[inline]
    fn remaining(&self) -> Result<usize, JournalError> {
        self.mmap
            .len()
            .checked_sub(self.write_pos)
            .ok_or(JournalError::CounterOverflow {
                counter: "segment write position",
            })
    }

    /// Write a raw entry to the segment at the current position.
    ///
    /// Returns `Ok(())` after flushing the written range to disk. On error
    /// the write position is unchanged.
    fn write_entry(&mut self, entry_bytes: &[u8]) -> Result<(), JournalError> {
        let too_large = || JournalError::EntryTooLarge {
            entry_bytes: entry_bytes.len(),
            segment_size: self.mmap.len(),
        };
        let end = self
            .write_pos
            .checked_add(entry_bytes.len())
            .ok_or_else(too_large)?;
        let segment_size = self.mmap.len();
        let dst = self
            .mmap
            .get_mut(self.write_pos..end)
            .ok_or(JournalError::EntryTooLarge {
                entry_bytes: entry_bytes.len(),
                segment_size,
            })?;
        // `dst` spans `write_pos..write_pos + entry_bytes.len()`, so the
        // lengths are equal and `copy_from_slice` cannot panic.
        dst.copy_from_slice(entry_bytes);
        self.mmap
            .flush_range(self.write_pos, entry_bytes.len())
            .map_err(|e| JournalError::Io {
                message: e.to_string(),
                path: Some(self.path.clone()),
            })?;
        self.write_pos = end;
        Ok(())
    }
}

/// Mutable writer state, guarded by one mutex so the write, the rotation and
/// the `last_seq` / `segment_start_seq` updates are atomic with respect to
/// each other.
struct WriterState {
    /// The active segment being written to.
    segment: SegmentWriter,
    /// The sequence number in the active segment's file name.
    segment_start_seq: u64,
    /// The last sequence number written to the journal.
    last_seq: Option<u64>,
}

// ─── FileJournal ────────────────────────────────────────────────────────────

/// A memory-mapped, append-only event journal with segment rotation.
///
/// `FileJournal` stores [`SequencerEvent`] instances in pre-allocated
/// segment files using memory-mapped I/O. Each entry is checksummed with
/// CRC32 for corruption detection.
///
/// # Segment Rotation
///
/// When the current segment cannot fit the next entry, a new segment file
/// is created (never over an existing file) and the write position resets.
/// Old segments remain on disk for reading until explicitly archived via
/// [`archive_segments_before`](FileJournal::archive_segments_before).
///
/// # Sequence Discipline
///
/// Appends must carry strictly increasing sequence numbers; a duplicate or
/// restarted sequence is refused with
/// [`JournalError::NonMonotonicSequence`] before anything is written, so it
/// can neither rotate onto nor overwrite an existing segment.
///
/// # Thread Safety
///
/// The writer state (active segment, `last_seq`, active segment start) is
/// protected by a single [`Mutex`]. Nothing inside the guarded sections
/// panics, so the lock cannot be poisoned by this crate's own code; if it
/// is poisoned anyway, every method reports [`JournalError::MutexPoisoned`]
/// instead of guessing. The intended usage is single-writer (Sequencer
/// thread) with concurrent readers (replay); readers never read past the
/// committed write position of the active segment.
///
/// # Example
///
/// ```rust,no_run
/// use orderbook_rs::orderbook::sequencer::{FileJournal, Journal, SequencerEvent};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let journal: FileJournal<()> = FileJournal::open("/tmp/journal")?;
/// // Use journal.append(&event) in the Sequencer run loop
/// # Ok(())
/// # }
/// ```
pub struct FileJournal<T> {
    /// Directory containing segment files.
    dir: PathBuf,
    /// The writer state.
    state: Mutex<WriterState>,
    /// Maximum size of each segment file in bytes.
    segment_size: usize,
    /// Marker for the generic event payload type.
    _phantom: PhantomData<T>,
}

impl<T> FileJournal<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    /// Open or create a journal in the given directory.
    ///
    /// If the directory contains existing segment files, the journal
    /// resumes from the latest segment after crash recovery (see the module
    /// docs). Otherwise, a new segment is created starting at sequence 0.
    ///
    /// # Arguments
    ///
    /// * `dir` — path to the journal directory (created if it does not
    ///   exist)
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if the directory cannot be created, existing
    /// segments cannot be opened, or the latest segment holds corruption
    /// inside committed data ([`JournalError::CorruptEntry`]).
    pub fn open<P: AsRef<Path>>(dir: P) -> Result<Self, JournalError> {
        Self::open_with_segment_size(dir, DEFAULT_SEGMENT_SIZE)
    }

    /// Open or create a journal with a custom segment size.
    ///
    /// # Arguments
    ///
    /// * `dir` — path to the journal directory
    /// * `segment_size` — maximum size of each segment file in bytes
    ///
    /// # Errors
    ///
    /// Same as [`Self::open`].
    pub fn open_with_segment_size<P: AsRef<Path>>(
        dir: P,
        segment_size: usize,
    ) -> Result<Self, JournalError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| JournalError::Io {
            message: e.to_string(),
            path: Some(dir.clone()),
        })?;

        // Find existing segments sorted by start sequence
        let mut segments = list_segments(&dir)?;
        segments.sort_unstable();

        let state = match segments.split_last() {
            Some((&latest, earlier)) => {
                let path = segment_path(&dir, latest);
                let (segment, last_in_latest) = SegmentWriter::open_existing(&path)?;
                // An empty latest segment (rotation created it, then the
                // process stopped before the entry landed) says nothing about
                // the last sequence; take it from the newest earlier segment
                // so the monotonic check still holds after a restart.
                let last_seq = match last_in_latest {
                    Some(seq) => Some(seq),
                    None => last_sequence_in(&dir, earlier)?,
                };
                WriterState {
                    segment,
                    segment_start_seq: latest,
                    last_seq,
                }
            }
            None => {
                // No existing segments — create the first one
                let path = segment_path(&dir, 0);
                WriterState {
                    segment: SegmentWriter::create(&path, segment_size)?,
                    segment_start_seq: 0,
                    last_seq: None,
                }
            }
        };

        info!(
            dir = %dir.display(),
            segment_start_seq = state.segment_start_seq,
            last_seq = ?state.last_seq,
            "journal opened"
        );

        Ok(Self {
            dir,
            state: Mutex::new(state),
            segment_size,
            _phantom: PhantomData,
        })
    }

    /// Locks the writer state, mapping poisoning to a typed error.
    #[inline]
    fn lock_state(&self) -> Result<MutexGuard<'_, WriterState>, JournalError> {
        self.state.lock().map_err(|_| JournalError::MutexPoisoned)
    }

    /// Archive all segment files whose start sequence is strictly less
    /// than `before_sequence`.
    ///
    /// Archived segments are renamed from `.journal` to
    /// `.journal.archived` and are excluded from future reads. The active
    /// segment is never archived. The writer lock is held for the whole
    /// call, so a concurrent rotation cannot race the renames.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if any segment file cannot be renamed, or
    /// [`JournalError::MutexPoisoned`] if the writer lock is poisoned.
    pub fn archive_segments_before(&self, before_sequence: u64) -> Result<usize, JournalError> {
        let state = self.lock_state()?;
        let segments = list_segments(&self.dir)?;
        let mut archived = 0usize;

        for start_seq in segments {
            if start_seq < before_sequence && start_seq != state.segment_start_seq {
                let src = segment_path(&self.dir, start_seq);
                let mut dst = src.clone();
                dst.set_extension("journal.archived");
                fs::rename(&src, &dst).map_err(|e| JournalError::Io {
                    message: e.to_string(),
                    path: Some(src),
                })?;
                // checked_add (not saturating) so an overflow surfaces as a
                // typed error rather than silently capping the tally.
                archived = archived
                    .checked_add(1)
                    .ok_or(JournalError::CounterOverflow {
                        counter: "archived segment count",
                    })?;
            }
        }

        Ok(archived)
    }

    /// Rotate to a new segment file starting at the given sequence.
    ///
    /// On error the active segment is unchanged.
    fn rotate_segment(&self, state: &mut WriterState, start_seq: u64) -> Result<(), JournalError> {
        // Flush the old segment's mmap before rotating away from it.
        state.segment.mmap.flush().map_err(|e| JournalError::Io {
            message: e.to_string(),
            path: Some(state.segment.path.clone()),
        })?;

        // Create the new segment (never over an existing file) and swap it
        // in.
        let new_path = segment_path(&self.dir, start_seq);
        state.segment = SegmentWriter::create(&new_path, self.segment_size)?;
        state.segment_start_seq = start_seq;

        // NOTE: we deliberately do NOT `set_len` the old segment down to its
        // used size. A concurrent reader (`read_from` / `verify_integrity`) may
        // already have it mmap'd at full capacity, and shrinking a mapped file
        // makes touching pages past the new EOF undefined behaviour (SIGBUS on
        // Unix) — exactly what the `SegmentWriter` SAFETY comments rely on never
        // happening. The unused tail is a sparse hole (the segment was grown
        // with `set_len`, never written), so it costs no physical disk; there is
        // nothing to reclaim.
        Ok(())
    }

    /// Serialize and encode a single event into the on-disk binary format.
    fn encode_entry(
        event: &SequencerEvent<T>,
        segment_size: usize,
    ) -> Result<Vec<u8>, JournalError> {
        let payload = serde_json::to_vec(event).map_err(|e| JournalError::SerializationError {
            message: e.to_string(),
        })?;

        let entry_length = entry_length_field(payload.len(), segment_size)?;
        let total_bytes = usize::try_from(entry_length)
            .ok()
            .and_then(|len| len.checked_add(ENTRY_LENGTH_SIZE))
            .ok_or(JournalError::EntryTooLarge {
                entry_bytes: payload.len(),
                segment_size,
            })?;

        let mut buf = Vec::new();
        buf.try_reserve_exact(total_bytes)
            .map_err(|_| JournalError::AllocationFailed {
                what: "entry bytes",
                requested: total_bytes,
            })?;

        let sequence = event.sequence_num.to_le_bytes();
        let timestamp = event.timestamp_ns.to_le_bytes();

        // CRC32 over (sequence_num ‖ timestamp_ns ‖ payload) — the same range
        // `decode_entry` re-checks on read. It does not cover `entry_length`
        // or the CRC field itself.
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&sequence);
        hasher.update(&timestamp);
        hasher.update(&payload);
        let crc = hasher.finalize();

        // Capacity is reserved above, so none of these reallocates.
        buf.extend_from_slice(&entry_length.to_le_bytes());
        buf.extend_from_slice(&sequence);
        buf.extend_from_slice(&timestamp);
        buf.extend_from_slice(&payload);
        buf.extend_from_slice(&crc.to_le_bytes());

        Ok(buf)
    }
}

impl<T> Journal<T> for FileJournal<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn append(&self, event: &SequencerEvent<T>) -> Result<(), JournalError> {
        let entry_bytes = Self::encode_entry(event, self.segment_size)?;

        let mut state = self.lock_state()?;

        // Refuse a duplicate or restarted sequence before touching disk: it
        // could otherwise rotate onto (and, before #252, truncate) an
        // existing segment.
        if let Some(last) = state.last_seq
            && event.sequence_num <= last
        {
            return Err(JournalError::NonMonotonicSequence {
                last,
                attempted: event.sequence_num,
            });
        }

        if state.segment.remaining()? < entry_bytes.len() {
            // A single entry larger than a whole segment can never fit;
            // refuse it before creating a segment for it.
            if entry_bytes.len() > self.segment_size {
                return Err(JournalError::EntryTooLarge {
                    entry_bytes: entry_bytes.len(),
                    segment_size: self.segment_size,
                });
            }
            self.rotate_segment(&mut state, event.sequence_num)?;
        }

        state.segment.write_entry(&entry_bytes)?;
        // Same guard as the durable write: this update cannot fail, so
        // `last_sequence()` never lags a successful append.
        state.last_seq = Some(event.sequence_num);

        Ok(())
    }

    fn read_from(&self, sequence: u64) -> Result<JournalReadIter<T>, JournalError> {
        // Snapshot the segment list and the committed end of the active
        // segment under the writer lock, so the iterator never reads an entry
        // that is still being written (a segment created after this point is
        // not in the list).
        let (active, mut segments) = {
            let state = self.lock_state()?;
            (
                (state.segment_start_seq, state.segment.write_pos),
                list_segments(&self.dir)?,
            )
        };
        segments.sort_unstable();

        // Find the segment that could contain the requested sequence.
        // The right segment has the largest start_seq <= sequence.
        let start_idx = match segments.binary_search(&sequence) {
            Ok(idx) | Err(idx @ 0) => idx,
            Err(idx) => idx.checked_sub(1).ok_or(JournalError::CounterOverflow {
                counter: "segment index",
            })?,
        };

        let segments_from: Vec<u64> = segments.into_iter().skip(start_idx).collect();

        let iter = SegmentIterator::<T> {
            dir: self.dir.clone(),
            segments: segments_from,
            segment_idx: 0,
            offset: 0,
            mmap: None,
            limit: 0,
            active,
            start_sequence: sequence,
            started: false,
            finished: false,
            _phantom: PhantomData,
        };

        Ok(Box::new(iter))
    }

    fn last_sequence(&self) -> Result<Option<u64>, JournalError> {
        Ok(self.lock_state()?.last_seq)
    }

    fn verify_integrity(&self) -> Result<(), JournalError> {
        // Same snapshot discipline as `read_from`.
        let (active_start, active_write_pos, mut segments) = {
            let state = self.lock_state()?;
            (
                state.segment_start_seq,
                state.segment.write_pos,
                list_segments(&self.dir)?,
            )
        };
        segments.sort_unstable();

        let mut previous: Option<u64> = None;
        for start_seq in segments {
            let path = segment_path(&self.dir, start_seq);
            let mmap = map_read_only(&path)?;
            let limit = if start_seq == active_start {
                active_write_pos
            } else {
                mmap.len()
            };
            let data = mmap
                .get(..limit)
                .ok_or_else(|| invalid_header(limit, "segment shorter than its write position"))?;

            let mut offset = 0usize;
            while let Decoded::Entry(raw) = decode_entry(data, offset)? {
                if !raw.crc_ok() {
                    error!(
                        path = %path.display(),
                        offset = raw.offset,
                        sequence = raw.sequence,
                        "journal entry failed CRC verification"
                    );
                    return Err(raw.corrupt());
                }
                if let Some(last) = previous
                    && raw.sequence <= last
                {
                    return Err(JournalError::NonMonotonicSequence {
                        last,
                        attempted: raw.sequence,
                    });
                }
                previous = Some(raw.sequence);
                offset = raw.end;
            }
        }

        Ok(())
    }
}

impl<T> std::fmt::Debug for FileJournal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("FileJournal");
        s.field("dir", &self.dir)
            .field("segment_size", &self.segment_size);
        match self.state.lock() {
            Ok(state) => s.field("last_seq", &state.last_seq),
            Err(_) => s.field("last_seq", &"<poisoned>"),
        };
        s.finish()
    }
}

// ─── Iteration ──────────────────────────────────────────────────────────────

/// An iterator over journal entries across multiple segment files.
///
/// A framing error ([`JournalError::InvalidEntryHeader`]) or a segment that
/// cannot be opened ends the iteration after the error is yielded: the next
/// entry boundary is unknown, so continuing could only yield garbage. A CRC
/// or deserialization failure on a well-framed entry is yielded and the
/// iterator moves on to the next entry.
struct SegmentIterator<T> {
    dir: PathBuf,
    segments: Vec<u64>,
    segment_idx: usize,
    offset: usize,
    mmap: Option<memmap2::Mmap>,
    /// Readable length of the current segment: its committed write position
    /// when it is the active segment, its full length otherwise.
    limit: usize,
    /// `(start sequence, committed write position)` of the active segment
    /// when the iterator was created.
    active: (u64, usize),
    start_sequence: u64,
    started: bool,
    finished: bool,
    _phantom: PhantomData<T>,
}

impl<T> SegmentIterator<T>
where
    T: for<'de> Deserialize<'de> + Clone + 'static,
{
    /// Load the next segment's mmap. Returns false if no more segments.
    fn load_next_segment(&mut self) -> Result<bool, JournalError> {
        let Some(&start_seq) = self.segments.get(self.segment_idx) else {
            return Ok(false);
        };
        let path = segment_path(&self.dir, start_seq);
        // checked_add (not saturating) so an overflow surfaces as a typed error
        // rather than silently stalling the segment cursor.
        self.segment_idx =
            self.segment_idx
                .checked_add(1)
                .ok_or(JournalError::CounterOverflow {
                    counter: "segment index",
                })?;
        self.offset = 0;

        let mmap = map_read_only(&path)?;
        let (active_start, active_write_pos) = self.active;
        self.limit = if start_seq == active_start {
            // The writer committed `active_write_pos` bytes; a shorter file
            // was truncated externally and lost committed entries, even when
            // the cut lands on an entry boundary.
            if mmap.len() < active_write_pos {
                return Err(invalid_header(
                    mmap.len(),
                    format!(
                        "active segment truncated to {} bytes below its committed write position {active_write_pos}",
                        mmap.len()
                    ),
                ));
            }
            active_write_pos
        } else {
            mmap.len()
        };
        self.mmap = Some(mmap);
        Ok(true)
    }

    /// Try to decode the next entry from the current mmap at `self.offset`.
    ///
    /// `None` means the current segment is exhausted.
    fn decode_next(&mut self) -> Option<Result<JournalEntry<T>, JournalError>> {
        let mmap = self.mmap.as_ref()?;
        let data = mmap.get(..self.limit)?;

        let raw = match decode_entry(data, self.offset) {
            Ok(Decoded::End) => return None,
            Ok(Decoded::Entry(raw)) => raw,
            Err(err) => {
                self.finished = true;
                return Some(Err(err));
            }
        };
        self.offset = raw.end;

        if !raw.crc_ok() {
            return Some(Err(raw.corrupt()));
        }

        let Some(json_data) = data.get(raw.payload_start..raw.crc_start) else {
            self.finished = true;
            return Some(Err(invalid_header(raw.offset, "truncated payload")));
        };

        let event: SequencerEvent<T> = match serde_json::from_slice(json_data) {
            Ok(ev) => ev,
            Err(e) => {
                return Some(Err(JournalError::DeserializationError {
                    sequence: raw.sequence,
                    message: e.to_string(),
                }));
            }
        };

        if event.sequence_num != raw.sequence {
            return Some(Err(JournalError::DeserializationError {
                sequence: raw.sequence,
                message: format!(
                    "payload sequence {} does not match header sequence {}",
                    event.sequence_num, raw.sequence
                ),
            }));
        }

        Some(Ok(JournalEntry {
            event,
            stored_crc: raw.stored_crc,
        }))
    }
}

impl<T> Iterator for SegmentIterator<T>
where
    T: for<'de> Deserialize<'de> + Clone + 'static,
{
    type Item = Result<JournalEntry<T>, JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        // Load the first segment if not yet started
        if !self.started {
            self.started = true;
            match self.load_next_segment() {
                Ok(true) => {}
                Ok(false) => return None,
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        }

        loop {
            // Try to read from the current segment
            if let Some(result) = self.decode_next() {
                if let Ok(entry) = &result {
                    // Skip entries before the requested start sequence
                    if entry.event.sequence_num < self.start_sequence {
                        continue;
                    }
                }
                return Some(result);
            }
            if self.finished {
                return None;
            }

            // Current segment exhausted — try the next one
            match self.load_next_segment() {
                Ok(true) => continue,
                Ok(false) => return None,
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Maps a segment file read-only.
fn map_read_only(path: &Path) -> Result<memmap2::Mmap, JournalError> {
    let file = File::open(path).map_err(|e| JournalError::Io {
        message: e.to_string(),
        path: Some(path.to_path_buf()),
    })?;

    // SAFETY: Read-only mapping of a segment file; segments are never
    // truncated while in use (single-writer pattern, pre-allocated segments).
    #[allow(unsafe_code)]
    let mmap = unsafe {
        memmap2::Mmap::map(&file).map_err(|e| JournalError::Io {
            message: e.to_string(),
            path: Some(path.to_path_buf()),
        })?
    };
    Ok(mmap)
}

/// Returns the last valid sequence in the newest of `segments` (sorted
/// ascending) that holds one.
fn last_sequence_in(dir: &Path, segments: &[u64]) -> Result<Option<u64>, JournalError> {
    for &start_seq in segments.iter().rev() {
        let mmap = map_read_only(&segment_path(dir, start_seq))?;
        if let Some(seq) = scan_last_sequence(&mmap) {
            return Ok(Some(seq));
        }
    }
    Ok(None)
}

/// Build the path for a segment file given its start sequence.
fn segment_path(dir: &Path, start_sequence: u64) -> PathBuf {
    dir.join(format!("segment-{start_sequence:020}.journal"))
}

/// List all active (non-archived) segment start sequences in the directory.
fn list_segments(dir: &Path) -> Result<Vec<u64>, JournalError> {
    let mut seqs = Vec::new();

    let entries = fs::read_dir(dir).map_err(|e| JournalError::Io {
        message: e.to_string(),
        path: Some(dir.to_path_buf()),
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| JournalError::Io {
            message: e.to_string(),
            path: Some(dir.to_path_buf()),
        })?;

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Match pattern: segment-{seq}.journal (NOT .journal.archived)
        if let Some(rest) = name_str.strip_prefix("segment-")
            && let Some(seq_str) = rest.strip_suffix(".journal")
            && let Ok(seq) = seq_str.parse::<u64>()
        {
            seqs.push(seq);
        }
    }

    Ok(seqs)
}

#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::orderbook::sequencer::types::{SequencerCommand, SequencerResult};
    use pricelevel::Id;
    use std::io::Write;

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    fn make_event(seq: u64) -> SequencerEvent<()> {
        SequencerEvent {
            sequence_num: seq,
            timestamp_ns: 1_700_000_000_000_000_000u64.checked_add(seq).unwrap_or(0),
            command: SequencerCommand::CancelOrder(new_id()),
            result: SequencerResult::OrderCancelled { order_id: new_id() },
        }
    }

    #[test]
    fn test_encode_entry_and_decode() {
        let event = make_event(42);
        let entry_bytes = FileJournal::<()>::encode_entry(&event, DEFAULT_SEGMENT_SIZE);
        assert!(entry_bytes.is_ok());
        let buf = entry_bytes.unwrap_or_default();
        assert!(!buf.is_empty());

        // Verify entry_length field
        let entry_length = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(entry_length + 4, buf.len());

        // Verify sequence_num
        let seq = u64::from_le_bytes([
            buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
        ]);
        assert_eq!(seq, 42);
    }

    #[test]
    fn test_write_and_read_single_entry() {
        let dir = tempfile::tempdir();
        assert!(dir.is_ok());
        let dir = dir.unwrap_or_else(|_| panic!("tempdir"));

        let journal = FileJournal::<()>::open(dir.path());
        assert!(journal.is_ok());
        let journal = journal.unwrap_or_else(|_| panic!("open"));

        let event = make_event(0);
        let result = journal.append(&event);
        assert!(result.is_ok());

        assert_eq!(journal.last_sequence().expect("last_sequence"), Some(0));

        let entries: Vec<_> = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_ok());
        let entry = entries[0].as_ref().unwrap_or_else(|_| panic!("entry"));
        assert_eq!(entry.event.sequence_num, 0);
    }

    #[test]
    fn test_write_and_read_multiple_entries() {
        let dir = tempfile::tempdir();
        assert!(dir.is_ok());
        let dir = dir.unwrap_or_else(|_| panic!("tempdir"));

        let journal = FileJournal::<()>::open(dir.path());
        assert!(journal.is_ok());
        let journal = journal.unwrap_or_else(|_| panic!("open"));

        for i in 0..10 {
            let event = make_event(i);
            let result = journal.append(&event);
            assert!(result.is_ok());
        }

        assert_eq!(journal.last_sequence().expect("last_sequence"), Some(9));

        // Read from sequence 5
        let entries: Vec<_> = journal
            .read_from(5)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert_eq!(entries.len(), 5);
        for (i, entry) in entries.iter().enumerate() {
            assert!(entry.is_ok());
            let e = entry.as_ref().unwrap_or_else(|_| panic!("entry"));
            assert_eq!(e.event.sequence_num, 5 + i as u64);
        }
    }

    #[test]
    fn test_read_from_empty_journal() {
        let dir = tempfile::tempdir();
        assert!(dir.is_ok());
        let dir = dir.unwrap_or_else(|_| panic!("tempdir"));

        let journal = FileJournal::<()>::open(dir.path());
        assert!(journal.is_ok());
        let journal = journal.unwrap_or_else(|_| panic!("open"));

        assert_eq!(journal.last_sequence().expect("last_sequence"), None);

        let entries: Vec<_> = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_segment_rotation() {
        let dir = tempfile::tempdir();
        assert!(dir.is_ok());
        let dir = dir.unwrap_or_else(|_| panic!("tempdir"));

        // Use a very small segment size to force rotation
        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), 512);
        assert!(journal.is_ok());
        let journal = journal.unwrap_or_else(|_| panic!("open"));

        // Write enough entries to force at least one rotation
        for i in 0..20 {
            let event = make_event(i);
            let result = journal.append(&event);
            assert!(result.is_ok());
        }

        assert_eq!(journal.last_sequence().expect("last_sequence"), Some(19));

        // Verify all entries can be read back
        let entries: Vec<_> = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert_eq!(entries.len(), 20);
        for (i, entry) in entries.iter().enumerate() {
            assert!(entry.is_ok());
            let e = entry.as_ref().unwrap_or_else(|_| panic!("entry"));
            assert_eq!(e.event.sequence_num, i as u64);
        }

        // Verify multiple segment files exist
        let segments = list_segments(dir.path());
        assert!(segments.is_ok());
        let segs = segments.unwrap_or_default();
        assert!(
            segs.len() > 1,
            "expected multiple segments, got {}",
            segs.len()
        );
    }

    #[test]
    fn test_verify_integrity_on_valid_journal() {
        let dir = tempfile::tempdir();
        assert!(dir.is_ok());
        let dir = dir.unwrap_or_else(|_| panic!("tempdir"));

        let journal = FileJournal::<()>::open(dir.path());
        assert!(journal.is_ok());
        let journal = journal.unwrap_or_else(|_| panic!("open"));

        for i in 0..5 {
            let event = make_event(i);
            let result = journal.append(&event);
            assert!(result.is_ok());
        }

        let integrity = journal.verify_integrity();
        assert!(integrity.is_ok());
    }

    /// Overwrites `bytes` at `offset` of `path` in place (no truncation), so
    /// it is safe while the segment is mapped.
    fn patch_file(path: &Path, offset: usize, bytes: &[u8]) {
        use std::io::{Seek, SeekFrom};
        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap_or_else(|_| panic!("open for patch"));
        file.seek(SeekFrom::Start(offset as u64))
            .unwrap_or_else(|_| panic!("seek"));
        file.write_all(bytes).unwrap_or_else(|_| panic!("patch"));
        file.sync_all().unwrap_or_else(|_| panic!("sync"));
    }

    /// Byte offsets of each entry's start in a segment file.
    fn entry_offsets(data: &[u8]) -> Vec<usize> {
        let mut offsets = Vec::new();
        let mut off = 0usize;
        while off + 4 <= data.len() {
            let el = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                as usize;
            if el == 0 || off + 4 + el > data.len() {
                break;
            }
            offsets.push(off);
            off += 4 + el;
        }
        offsets
    }

    #[test]
    fn test_verify_integrity_detects_corruption() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
        for i in 0..3 {
            assert!(journal.append(&make_event(i)).is_ok());
        }
        assert!(journal.verify_integrity().is_ok());

        // Flip a payload byte of the committed middle entry in place.
        let seg_path = segment_path(dir.path(), 0);
        let data = fs::read(&seg_path).unwrap_or_default();
        let offsets = entry_offsets(&data);
        assert_eq!(offsets.len(), 3);
        let at = offsets[1] + 30;
        patch_file(&seg_path, at, &[data[at] ^ 0xFF]);

        match journal.verify_integrity() {
            Err(JournalError::CorruptEntry { sequence, .. }) => assert_eq!(sequence, 1),
            other => panic!("expected CorruptEntry, got {other:?}"),
        }
        // Replay surfaces it too.
        let results: Vec<_> = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert!(matches!(
            results[1],
            Err(JournalError::CorruptEntry { sequence: 1, .. })
        ));
    }

    /// #252: a damaged entry followed by a valid one is corruption inside
    /// committed data, not a torn tail. Reopen refuses it instead of
    /// truncating (and later overwriting) the valid entries after it.
    #[test]
    fn test_reopen_refuses_mid_segment_corruption_without_destroying_data() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
        for i in 0..3 {
            assert!(journal.append(&make_event(i)).is_ok());
        }
        drop(journal);

        let seg_path = segment_path(dir.path(), 0);
        let data = fs::read(&seg_path).unwrap_or_default();
        let offsets = entry_offsets(&data);
        let at = offsets[0] + 30;
        patch_file(&seg_path, at, &[data[at] ^ 0xFF]);
        let before = fs::read(&seg_path).unwrap_or_default();

        match FileJournal::<()>::open(dir.path()) {
            Err(JournalError::CorruptEntry { sequence, .. }) => assert_eq!(sequence, 0),
            other => panic!("expected CorruptEntry, got {other:?}"),
        }
        assert_eq!(
            fs::read(&seg_path).unwrap_or_default(),
            before,
            "a refused reopen must not modify the segment"
        );
    }

    /// Walks the entry-length headers (the pre-CRC scan) to find the byte
    /// offset of the end of written data. Used by the torn-tail test to corrupt
    /// the final entry without depending on the CRC-aware scanner under test.
    fn written_len(data: &[u8]) -> usize {
        let mut off = 0usize;
        while off + 4 <= data.len() {
            let el = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                as usize;
            if el == 0 {
                break;
            }
            match off.checked_add(4).and_then(|v| v.checked_add(el)) {
                Some(end) if end <= data.len() => off = end,
                _ => break,
            }
        }
        off
    }

    #[test]
    fn test_reopen_truncates_torn_tail_then_appends_and_replays() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));

        // Write three valid entries (seq 0, 1, 2).
        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
        for i in 0..3 {
            assert!(journal.append(&make_event(i)).is_ok());
        }
        assert_eq!(journal.last_sequence().expect("last_sequence"), Some(2));
        drop(journal); // release the mmap

        // Corrupt the final entry's trailing CRC byte: a crash mid-flush leaves
        // the entry_length/sequence_num header intact but the payload/CRC torn.
        let segs = list_segments(dir.path()).unwrap_or_default();
        assert_eq!(segs.len(), 1);
        let seg_path = segment_path(dir.path(), segs[0]);
        let mut data = fs::read(&seg_path).unwrap_or_default();
        let write_pos = written_len(&data);
        assert!(write_pos > 0);
        data[write_pos - 1] ^= 0xFF; // flip the last CRC byte of the last entry
        fs::write(&seg_path, &data).unwrap_or_default();

        // Reopen: the scan must stop at the torn entry, so last_sequence reports
        // the last *good* entry (seq 1), not the torn seq 2.
        let journal2 = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("reopen"));
        assert_eq!(
            journal2.last_sequence().expect("last_sequence"),
            Some(1),
            "torn tail must truncate to the last good entry"
        );

        // Appending after a torn-tail reopen overwrites the corrupt bytes, so a
        // subsequent integrity check and replay succeed.
        assert!(journal2.append(&make_event(2)).is_ok());
        assert_eq!(journal2.last_sequence().expect("last_sequence"), Some(2));
        assert!(
            journal2.verify_integrity().is_ok(),
            "the torn entry must be overwritten by the new append"
        );

        let entries: Vec<_> = journal2
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert_eq!(entries.len(), 3, "0, 1, and the re-appended 2");
        for (i, entry) in entries.iter().enumerate() {
            let e = entry.as_ref().unwrap_or_else(|_| panic!("entry decodes"));
            assert_eq!(e.event.sequence_num, i as u64);
        }
    }

    /// Poison a mutex by panicking while holding its guard, so a later `lock()`
    /// returns `Err(PoisonError)`.
    fn poison<G>(mutex: &Mutex<G>) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap_or_else(|_| panic!("lock to poison"));
            panic!("intentional poison");
        }));
    }

    /// #252: every method reports a poisoned writer lock as a typed error;
    /// `last_sequence` never turns it into an empty journal.
    #[test]
    fn test_poisoned_writer_state_is_typed_everywhere() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
        assert!(journal.append(&make_event(0)).is_ok());

        poison(&journal.state);

        assert!(matches!(
            journal.append(&make_event(1)),
            Err(JournalError::MutexPoisoned)
        ));
        assert!(matches!(
            journal.last_sequence(),
            Err(JournalError::MutexPoisoned)
        ));
        assert!(matches!(
            journal.read_from(0),
            Err(JournalError::MutexPoisoned)
        ));
        assert!(matches!(
            journal.verify_integrity(),
            Err(JournalError::MutexPoisoned)
        ));
        assert!(matches!(
            journal.archive_segments_before(10),
            Err(JournalError::MutexPoisoned)
        ));
        assert!(format!("{journal:?}").contains("<poisoned>"));
    }

    #[test]
    fn test_archive_segments_before() {
        let dir = tempfile::tempdir();
        assert!(dir.is_ok());
        let dir = dir.unwrap_or_else(|_| panic!("tempdir"));

        // Small segment size to force rotation
        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), 512);
        assert!(journal.is_ok());
        let journal = journal.unwrap_or_else(|_| panic!("open"));

        for i in 0..20 {
            let event = make_event(i);
            let result = journal.append(&event);
            assert!(result.is_ok());
        }

        let segments_before = list_segments(dir.path()).unwrap_or_default();
        assert!(segments_before.len() > 1);

        // Archive all segments before the last one
        let last_start = *segments_before.iter().max().unwrap_or(&0);
        let archived = journal.archive_segments_before(last_start);
        assert!(archived.is_ok());
        let archived_count = archived.unwrap_or(0);
        assert!(archived_count > 0);

        // Verify active segments decreased
        let segments_after = list_segments(dir.path()).unwrap_or_default();
        assert!(segments_after.len() < segments_before.len());
    }

    #[test]
    fn test_reopen_journal_resumes() {
        let dir = tempfile::tempdir();
        assert!(dir.is_ok());
        let dir = dir.unwrap_or_else(|_| panic!("tempdir"));

        // Write some entries
        {
            let journal = FileJournal::<()>::open(dir.path());
            assert!(journal.is_ok());
            let journal = journal.unwrap_or_else(|_| panic!("open"));

            for i in 0..5 {
                let event = make_event(i);
                let result = journal.append(&event);
                assert!(result.is_ok());
            }
        }

        // Re-open and continue writing
        {
            let journal = FileJournal::<()>::open(dir.path());
            assert!(journal.is_ok());
            let journal = journal.unwrap_or_else(|_| panic!("reopen"));

            assert_eq!(journal.last_sequence().expect("last_sequence"), Some(4));

            for i in 5..10 {
                let event = make_event(i);
                let result = journal.append(&event);
                assert!(result.is_ok());
            }

            assert_eq!(journal.last_sequence().expect("last_sequence"), Some(9));

            // Read all entries
            let entries: Vec<_> = journal
                .read_from(0)
                .unwrap_or_else(|_| panic!("read_from"))
                .collect();
            assert_eq!(entries.len(), 10);
        }
    }

    #[test]
    fn test_segment_path_format() {
        let dir = PathBuf::from("/tmp/journal");
        let path = segment_path(&dir, 42);
        assert_eq!(
            path.to_string_lossy(),
            "/tmp/journal/segment-00000000000000000042.journal"
        );
    }

    #[test]
    fn test_entry_overhead_constant() {
        assert_eq!(super::super::journal::ENTRY_OVERHEAD, 24);
        assert_eq!(super::super::journal::ENTRY_HEADER_SIZE, 20);
        assert_eq!(ENTRY_CRC_SIZE, 4);
    }

    #[test]
    fn test_journal_error_display() {
        let err = JournalError::CorruptEntry {
            sequence: 42,
            expected_crc: 0xDEAD_BEEF,
            actual_crc: 0xCAFE_BABE,
        };
        let display = format!("{err}");
        assert!(display.contains("corrupt journal entry"));
        assert!(display.contains("42"));

        let err2 = JournalError::MutexPoisoned;
        let display2 = format!("{err2}");
        assert!(display2.contains("mutex poisoned"));
    }

    #[test]
    fn test_sequencer_event_serialize_roundtrip() {
        let event = make_event(7);
        let json = serde_json::to_vec(&event);
        assert!(json.is_ok());
        let bytes = json.unwrap_or_default();

        let decoded: Result<SequencerEvent<()>, _> = serde_json::from_slice(&bytes);
        assert!(decoded.is_ok());
        let decoded = decoded.unwrap_or_else(|_| panic!("decode"));
        assert_eq!(decoded.sequence_num, 7);
    }

    /// #189: an `EvictExpiredOrders` command survives a full `FileJournal`
    /// append/read cycle (CRC-verified, memory-mapped), keeping `FileJournal`
    /// in parity with `InMemoryJournal` for the appended variant. The
    /// journaled `now_ms` decodes byte-identically.
    #[test]
    fn test_evict_expired_orders_command_file_journal_roundtrip() {
        use pricelevel::TimestampMs;

        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));

        let event = SequencerEvent::<()> {
            sequence_num: 0,
            timestamp_ns: 0,
            command: SequencerCommand::EvictExpiredOrders {
                now_ms: TimestampMs::new(1_700_000_000_000),
            },
            result: SequencerResult::OrderCancelled { order_id: new_id() },
        };
        assert!(journal.append(&event).is_ok());
        assert!(journal.verify_integrity().is_ok());

        let decoded = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .next()
            .and_then(Result::ok)
            .unwrap_or_else(|| panic!("decode"));
        match decoded.event.command {
            SequencerCommand::EvictExpiredOrders { now_ms } => {
                assert_eq!(now_ms, TimestampMs::new(1_700_000_000_000));
            }
            other => panic!("expected EvictExpiredOrders, got {other:?}"),
        }
    }

    /// #240: a `MatchAborted` result with its committed prefix survives a
    /// full `FileJournal` append/read cycle, in parity with `InMemoryJournal`.
    #[test]
    fn test_match_aborted_result_file_journal_roundtrip() {
        use crate::orderbook::reject_reason::RejectReason;
        use crate::orderbook::sequencer::types::{CommittedPrefix, CommittedTrade};
        use pricelevel::{Price, Quantity, Side};

        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));

        let committed = CommittedPrefix {
            executed_quantity: 7,
            trades: vec![
                CommittedTrade {
                    trade_id: new_id(),
                    maker_order_id: new_id(),
                    price: Price::new(100),
                    quantity: Quantity::new(4),
                },
                CommittedTrade {
                    trade_id: new_id(),
                    maker_order_id: new_id(),
                    price: Price::new(101),
                    quantity: Quantity::new(3),
                },
            ],
        };
        let event = SequencerEvent::<()> {
            sequence_num: 0,
            timestamp_ns: 0,
            command: SequencerCommand::MarketOrder {
                id: new_id(),
                quantity: 10,
                side: Side::Buy,
            },
            result: SequencerResult::MatchAborted {
                reason: "match aborted".to_string(),
                code: RejectReason::MatchAborted,
                committed: committed.clone(),
            },
        };
        assert!(journal.append(&event).is_ok());
        assert!(journal.verify_integrity().is_ok());

        let decoded = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .next()
            .and_then(Result::ok)
            .unwrap_or_else(|| panic!("decode"));
        match decoded.event.result {
            SequencerResult::MatchAborted {
                code,
                committed: decoded,
                ..
            } => {
                assert_eq!(code, RejectReason::MatchAborted);
                assert_eq!(decoded, committed);
            }
            other => panic!("expected MatchAborted, got {other:?}"),
        }
    }

    // ─── #252 hardening ─────────────────────────────────────────────────────

    /// An event whose JSON payload is roughly `pad` bytes larger than
    /// `make_event`'s.
    fn make_big_event(seq: u64, pad: usize) -> SequencerEvent<()> {
        SequencerEvent {
            sequence_num: seq,
            timestamp_ns: 7,
            command: SequencerCommand::CancelOrder(new_id()),
            result: SequencerResult::Rejected {
                reason: "x".repeat(pad),
            },
        }
    }

    fn committed_entries(journal: &FileJournal<()>) -> Vec<u64> {
        journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .map(|e| {
                e.unwrap_or_else(|err| panic!("entry decodes: {err}"))
                    .event
                    .sequence_num
            })
            .collect()
    }

    #[test]
    fn test_decode_entry_zero_length_is_end() {
        let data = [0u8; 64];
        assert!(matches!(decode_entry(&data, 0), Ok(Decoded::End)));
        // A zero-filled remainder shorter than the length field is the end.
        assert!(matches!(decode_entry(&data[..3], 0), Ok(Decoded::End)));
        assert!(matches!(decode_entry(&data, 62), Ok(Decoded::End)));
    }

    #[test]
    fn test_decode_entry_bad_headers_are_typed_errors() {
        // entry_length below the 20-byte minimum.
        let mut data = [0u8; 64];
        data[..4].copy_from_slice(&5u32.to_le_bytes());
        assert!(matches!(
            decode_entry(&data, 0),
            Err(JournalError::InvalidEntryHeader { offset: 0, .. })
        ));
        // entry_length running past the segment.
        data[..4].copy_from_slice(&1_000u32.to_le_bytes());
        assert!(matches!(
            decode_entry(&data, 0),
            Err(JournalError::InvalidEntryHeader { offset: 0, .. })
        ));
        // A truncated, non-zero length field.
        assert!(matches!(
            decode_entry(&[1u8, 0], 0),
            Err(JournalError::InvalidEntryHeader { .. })
        ));
        // An offset past the data.
        assert!(matches!(
            decode_entry(&data, 65),
            Err(JournalError::InvalidEntryHeader { offset: 65, .. })
        ));
    }

    #[test]
    fn test_decode_entry_roundtrips_encoded_entry() {
        let bytes = FileJournal::<()>::encode_entry(&make_event(9), DEFAULT_SEGMENT_SIZE)
            .unwrap_or_else(|_| panic!("encode"));
        match decode_entry(&bytes, 0) {
            Ok(Decoded::Entry(raw)) => {
                assert!(raw.crc_ok());
                assert_eq!(raw.sequence, 9);
                assert_eq!(raw.end, bytes.len());
            }
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    /// #252: the `entry_length` field is 32 bits; a payload that does not
    /// fit is `EntryTooLarge`, never a truncating cast.
    #[test]
    fn test_entry_length_field_rejects_payload_over_u32() {
        assert_eq!(entry_length_field(100, 10).unwrap_or(0), 120);
        let max_ok = u32::MAX as usize - ENTRY_FIXED_BODY;
        assert_eq!(entry_length_field(max_ok, 10).unwrap_or(0), u32::MAX);
        for huge in [max_ok + 1, u32::MAX as usize, usize::MAX] {
            assert!(matches!(
                entry_length_field(huge, 10),
                Err(JournalError::EntryTooLarge { .. })
            ));
        }
    }

    /// #252: a duplicate sequence at a rotation boundary is refused before
    /// anything is written; the segment it would have rotated onto is not
    /// truncated.
    #[test]
    fn test_duplicate_sequence_at_rotation_boundary_is_refused_without_truncation() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let entry_total = FileJournal::<()>::encode_entry(&make_event(0), DEFAULT_SEGMENT_SIZE)
            .unwrap_or_else(|_| panic!("encode"))
            .len();
        let segment_size = entry_total + 8; // one entry per segment
        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), segment_size)
            .unwrap_or_else(|_| panic!("open"));
        assert!(journal.append(&make_event(0)).is_ok());
        assert!(
            journal.append(&make_event(1)).is_ok(),
            "rotates to segment 1"
        );
        let seg1 = segment_path(dir.path(), 1);
        let seg1_before = fs::read(&seg1).unwrap_or_default();

        for dup in [1u64, 0] {
            match journal.append(&make_event(dup)) {
                Err(JournalError::NonMonotonicSequence { last, attempted }) => {
                    assert_eq!((last, attempted), (1, dup));
                }
                other => panic!("expected NonMonotonicSequence, got {other:?}"),
            }
        }
        assert_eq!(fs::read(&seg1).unwrap_or_default(), seg1_before);
        assert_eq!(list_segments(dir.path()).unwrap_or_default().len(), 2);
        assert_eq!(committed_entries(&journal), vec![0, 1]);
        drop(journal);

        // The check survives a restart: the last sequence is recovered.
        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), segment_size)
            .unwrap_or_else(|_| panic!("reopen"));
        assert!(matches!(
            journal.append(&make_event(1)),
            Err(JournalError::NonMonotonicSequence {
                last: 1,
                attempted: 1
            })
        ));
        assert!(journal.append(&make_event(2)).is_ok());
        assert_eq!(fs::read(&seg1).unwrap_or_default(), seg1_before);
        assert!(journal.verify_integrity().is_ok());
    }

    /// #252: rotation uses `create_new`; an existing file at the new
    /// segment's path is preserved and the append fails with a typed error.
    #[test]
    fn test_rotation_never_truncates_an_existing_segment_file() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let entry_total = FileJournal::<()>::encode_entry(&make_event(0), DEFAULT_SEGMENT_SIZE)
            .unwrap_or_else(|_| panic!("encode"))
            .len();
        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), entry_total + 8)
            .unwrap_or_else(|_| panic!("open"));
        assert!(journal.append(&make_event(0)).is_ok());

        let stray = segment_path(dir.path(), 1);
        fs::write(&stray, b"do not truncate").unwrap_or_else(|_| panic!("stray"));

        match journal.append(&make_event(1)) {
            Err(JournalError::SegmentExists { path }) => assert_eq!(path, stray),
            other => panic!("expected SegmentExists, got {other:?}"),
        }
        assert_eq!(fs::read(&stray).unwrap_or_default(), b"do not truncate");
        assert_eq!(journal.last_sequence().expect("last_sequence"), Some(0));
    }

    /// #252: a bad header inside committed data is an error, not a silent
    /// end of the segment (which would let replay "succeed" on a prefix).
    #[test]
    fn test_bad_header_mid_segment_is_an_error_not_the_end() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
        for i in 0..4 {
            assert!(journal.append(&make_event(i)).is_ok());
        }
        let seg_path = segment_path(dir.path(), 0);
        let offsets = entry_offsets(&fs::read(&seg_path).unwrap_or_default());
        // Entry 2's length now claims to run far past the segment.
        patch_file(&seg_path, offsets[2], &0xFFFF_FF00u32.to_le_bytes());

        let results: Vec<_> = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert_eq!(results.len(), 3, "two entries, the error, then nothing");
        assert!(results[0].is_ok() && results[1].is_ok());
        assert!(matches!(
            results[2],
            Err(JournalError::InvalidEntryHeader { .. })
        ));
        assert!(matches!(
            journal.verify_integrity(),
            Err(JournalError::InvalidEntryHeader { .. })
        ));

        // An entry_length below the minimum is rejected the same way.
        patch_file(&seg_path, offsets[2], &3u32.to_le_bytes());
        assert!(matches!(
            journal.verify_integrity(),
            Err(JournalError::InvalidEntryHeader { .. })
        ));
    }

    /// #252: a truncated segment that is not the latest one (so reopen does
    /// not recover it) is reported by reads and `verify_integrity`.
    #[test]
    fn test_truncated_earlier_segment_is_reported() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), 600)
            .unwrap_or_else(|_| panic!("open"));
        for i in 0..8 {
            assert!(journal.append(&make_event(i)).is_ok());
        }
        drop(journal);
        let segs = list_segments(dir.path()).unwrap_or_default();
        assert!(segs.len() > 1);
        let first = segment_path(dir.path(), 0);
        let data = fs::read(&first).unwrap_or_default();
        let offsets = entry_offsets(&data);
        assert!(offsets.len() >= 2);
        // Cut the file in the middle of its second entry.
        fs::write(&first, &data[..offsets[1] + 10]).unwrap_or_else(|_| panic!("truncate"));

        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), 600)
            .unwrap_or_else(|_| panic!("reopen"));
        assert!(matches!(
            journal.verify_integrity(),
            Err(JournalError::InvalidEntryHeader { .. })
        ));
        let results: Vec<_> = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert_eq!(results.len(), 2, "entry 0, then the error, then nothing");
        assert!(results[0].is_ok());
        assert!(matches!(
            results[1],
            Err(JournalError::InvalidEntryHeader { .. })
        ));
    }

    /// #252: after a torn tail, reopen zeroes the torn range. A later,
    /// shorter append then leaves no stale bytes that decode as a header.
    #[test]
    fn test_stale_tail_after_crash_then_shorter_append_is_clean() {
        for header_lost in [false, true] {
            let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
            let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
            assert!(journal.append(&make_event(0)).is_ok());
            assert!(journal.append(&make_event(1)).is_ok());
            assert!(journal.append(&make_big_event(2, 5_000)).is_ok());
            drop(journal);

            let seg_path = segment_path(dir.path(), 0);
            let data = fs::read(&seg_path).unwrap_or_default();
            let offsets = entry_offsets(&data);
            let torn = offsets[2];
            if header_lost {
                // The header page never reached disk; the payload did.
                patch_file(&seg_path, torn, &[0, 0, 0, 0]);
            } else {
                // The CRC trailer never reached disk.
                let end = torn
                    + 4
                    + u32::from_le_bytes([
                        data[torn],
                        data[torn + 1],
                        data[torn + 2],
                        data[torn + 3],
                    ]) as usize;
                patch_file(&seg_path, end - 1, &[data[end - 1] ^ 0xFF]);
            }

            let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("reopen"));
            assert_eq!(journal.last_sequence().expect("last_sequence"), Some(1));
            let after = fs::read(&seg_path).unwrap_or_default();
            assert!(
                after[torn..].iter().all(|b| *b == 0),
                "reopen must zero the torn range (header_lost = {header_lost})"
            );

            // Shorter re-append of 2, then 3: the chain stays well-formed.
            assert!(journal.append(&make_event(2)).is_ok());
            assert!(journal.append(&make_event(3)).is_ok());
            assert!(journal.verify_integrity().is_ok());
            assert_eq!(committed_entries(&journal), vec![0, 1, 2, 3]);
            drop(journal);

            let journal =
                FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("reopen 2"));
            assert_eq!(journal.last_sequence().expect("last_sequence"), Some(3));
            assert_eq!(committed_entries(&journal), vec![0, 1, 2, 3]);
        }
    }

    /// #252: an empty latest segment (rotation created it, the entry never
    /// landed) does not reset the monotonic check after a restart.
    #[test]
    fn test_empty_latest_segment_recovers_last_sequence_from_earlier_segment() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let entry_total = FileJournal::<()>::encode_entry(&make_event(0), DEFAULT_SEGMENT_SIZE)
            .unwrap_or_else(|_| panic!("encode"))
            .len();
        let segment_size = entry_total + 8;
        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), segment_size)
            .unwrap_or_else(|_| panic!("open"));
        for i in 0..3 {
            assert!(journal.append(&make_event(i)).is_ok());
        }
        drop(journal);
        let latest = segment_path(dir.path(), 2);
        let len = fs::metadata(&latest).map(|m| m.len()).unwrap_or(0) as usize;
        fs::write(&latest, vec![0u8; len]).unwrap_or_else(|_| panic!("blank latest"));

        let journal = FileJournal::<()>::open_with_segment_size(dir.path(), segment_size)
            .unwrap_or_else(|_| panic!("reopen"));
        assert_eq!(journal.last_sequence().expect("last_sequence"), Some(1));
        assert!(matches!(
            journal.append(&make_event(0)),
            Err(JournalError::NonMonotonicSequence {
                last: 1,
                attempted: 0
            })
        ));
        assert!(journal.append(&make_event(2)).is_ok());
        assert_eq!(committed_entries(&journal), vec![0, 1, 2]);
    }

    /// #252: `verify_integrity` reports stored sequences that do not
    /// strictly increase (a journal written before the append check).
    #[test]
    fn test_verify_integrity_reports_non_monotonic_stored_sequences() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let mut segment = vec![0u8; 4096];
        let mut off = 0;
        for seq in [0u64, 1, 1] {
            let bytes = FileJournal::<()>::encode_entry(&make_event(seq), DEFAULT_SEGMENT_SIZE)
                .unwrap_or_else(|_| panic!("encode"));
            segment[off..off + bytes.len()].copy_from_slice(&bytes);
            off += bytes.len();
        }
        fs::write(segment_path(dir.path(), 0), &segment).unwrap_or_else(|_| panic!("write"));

        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
        assert!(matches!(
            journal.verify_integrity(),
            Err(JournalError::NonMonotonicSequence {
                last: 1,
                attempted: 1
            })
        ));
    }

    /// #252: a header sequence that disagrees with the payload's is reported
    /// even when the CRC matches.
    #[test]
    fn test_header_payload_sequence_mismatch_is_reported() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let mut bytes = FileJournal::<()>::encode_entry(&make_event(5), DEFAULT_SEGMENT_SIZE)
            .unwrap_or_else(|_| panic!("encode"));
        bytes[4..12].copy_from_slice(&6u64.to_le_bytes());
        let crc_start = bytes.len() - 4;
        let crc = crc32fast::hash(&bytes[4..crc_start]);
        bytes[crc_start..].copy_from_slice(&crc.to_le_bytes());
        let mut segment = vec![0u8; 4096];
        segment[..bytes.len()].copy_from_slice(&bytes);
        fs::write(segment_path(dir.path(), 0), &segment).unwrap_or_else(|_| panic!("write"));

        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
        let first = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .next();
        assert!(matches!(
            first,
            Some(Err(JournalError::DeserializationError { sequence: 6, .. }))
        ));
    }

    /// #252 review: several adjacent damaged entries followed by a valid,
    /// durable one are corruption inside committed data. Reopen scans past
    /// all of them, finds the valid entry and refuses without touching the
    /// file (a one-entry lookahead would have zeroed the valid entry).
    #[test]
    fn test_reopen_refuses_adjacent_corrupted_entries_before_a_valid_one() {
        for header_damaged in [false, true] {
            let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
            let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
            for i in 0..4 {
                assert!(journal.append(&make_event(i)).is_ok());
            }
            drop(journal);

            let seg_path = segment_path(dir.path(), 0);
            let data = fs::read(&seg_path).unwrap_or_default();
            let offsets = entry_offsets(&data);
            assert_eq!(offsets.len(), 4);
            // Damage entries 1 and 2; entry 3 stays valid.
            for &entry in &offsets[1..3] {
                let at = entry + 30;
                patch_file(&seg_path, at, &[data[at] ^ 0xFF]);
            }
            if header_damaged {
                // Entry 1's length now runs past the segment: no framing to
                // follow, so only a byte scan can find entry 3.
                patch_file(&seg_path, offsets[1], &0xFFFF_FF00u32.to_le_bytes());
            }
            let before = fs::read(&seg_path).unwrap_or_default();

            match FileJournal::<()>::open(dir.path()) {
                Err(JournalError::CorruptEntry { sequence, .. }) => {
                    assert!(!header_damaged);
                    assert_eq!(sequence, 1);
                }
                Err(JournalError::InvalidEntryHeader { offset, .. }) => {
                    assert!(header_damaged);
                    assert_eq!(offset, offsets[1]);
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
            assert_eq!(
                fs::read(&seg_path).unwrap_or_default(),
                before,
                "a refused reopen must not modify the segment"
            );
        }
    }

    /// #252 review: an active segment truncated externally below its
    /// committed write position, even on an entry boundary, is an error on
    /// read instead of a silently shorter replay.
    #[test]
    fn test_read_reports_active_segment_truncated_below_write_position() {
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
        let journal = FileJournal::<()>::open(dir.path()).unwrap_or_else(|_| panic!("open"));
        for i in 0..3 {
            assert!(journal.append(&make_event(i)).is_ok());
        }
        let seg_path = segment_path(dir.path(), 0);
        let offsets = entry_offsets(&fs::read(&seg_path).unwrap_or_default());
        // Cut exactly at the start of entry 2 (an entry boundary). Nothing
        // below touches the writer's mapping past the new end of file.
        OpenOptions::new()
            .write(true)
            .open(&seg_path)
            .and_then(|f| f.set_len(offsets[2] as u64))
            .unwrap_or_else(|_| panic!("truncate"));

        let results: Vec<_> = journal
            .read_from(0)
            .unwrap_or_else(|_| panic!("read_from"))
            .collect();
        assert_eq!(results.len(), 1, "the error, then nothing");
        assert!(matches!(
            results[0],
            Err(JournalError::InvalidEntryHeader { .. })
        ));
        assert!(matches!(
            journal.verify_integrity(),
            Err(JournalError::InvalidEntryHeader { .. })
        ));
    }

    /// The forward scan finds a valid entry at an arbitrary offset and
    /// ignores zero runs and garbage.
    #[test]
    fn test_find_valid_entry_after_scans_every_offset() {
        let entry = FileJournal::<()>::encode_entry(&make_event(4), DEFAULT_SEGMENT_SIZE)
            .unwrap_or_else(|_| panic!("encode"));
        let mut data = vec![0u8; 8192];
        data[10..20].copy_from_slice(&[7u8; 10]); // garbage
        data[5003..5003 + entry.len()].copy_from_slice(&entry);
        let found = find_valid_entry_after(&data, 0).unwrap_or_else(|| panic!("found"));
        assert_eq!((found.offset, found.sequence), (5003, 4));
        assert!(find_valid_entry_after(&data, 5003).is_none());
        assert!(find_valid_entry_after(&[0u8; 64], 0).is_none());
    }
}
