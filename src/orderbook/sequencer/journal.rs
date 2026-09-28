//! Append-only event journal trait for deterministic replay.
//!
//! The [`Journal`] trait defines the contract for persisting
//! [`SequencerEvent`] instances to durable
//! storage. Implementations must guarantee write-ahead semantics: an event
//! is considered committed only after [`append`](Journal::append) returns
//! `Ok(())`.
//!
//! See `FileJournal` (in the `file_journal` module) for the default
//! memory-mapped file implementation.

use super::error::JournalError;
use super::types::SequencerEvent;
use serde::{Deserialize, Serialize};

/// Size of the fixed-size entry header in bytes.
///
/// Layout: `[4 bytes entry_length][8 bytes sequence_num][8 bytes timestamp_ns]`
pub const ENTRY_HEADER_SIZE: usize = 4 + 8 + 8;

/// Size of the CRC32 trailer appended to each entry in bytes.
pub const ENTRY_CRC_SIZE: usize = 4;

/// Total overhead per journal entry (header + CRC trailer) in bytes.
pub const ENTRY_OVERHEAD: usize = ENTRY_HEADER_SIZE + ENTRY_CRC_SIZE;

/// A single journal entry as read back from storage.
///
/// Contains the deserialized event together with its on-disk metadata.
#[derive(Debug, Clone)]
pub struct JournalEntry<T> {
    /// The deserialized sequencer event.
    pub event: SequencerEvent<T>,

    /// The CRC32 checksum that was stored alongside the entry.
    pub stored_crc: u32,
}

/// Type alias for the iterator returned by [`Journal::read_from`].
///
/// Each item is either a successfully decoded [`JournalEntry`] or a
/// [`JournalError`] (e.g. corrupt CRC, deserialization failure).
pub type JournalReadIter<T> = Box<dyn Iterator<Item = Result<JournalEntry<T>, JournalError>>>;

/// An append-only event journal for deterministic replay.
///
/// Implementations must provide durable, ordered storage of
/// [`SequencerEvent`] instances. The journal is the foundation of the
/// write-ahead log pattern: every event must be persisted before its
/// result is returned to the caller.
///
/// # Type Parameters
///
/// * `T` — the extra-fields type carried by `OrderType<T>`. Must be
///   serializable and deserializable for journal persistence.
///
/// # Thread Safety
///
/// The trait requires `Send + Sync` so the journal can be shared across
/// async task boundaries. However, the intended usage pattern is
/// single-writer (the Sequencer thread) with concurrent readers (replay,
/// monitoring).
pub trait Journal<T>: Send + Sync
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    /// Append an event to the journal.
    ///
    /// The event must be durably persisted before this method returns.
    /// Implementations should flush the underlying storage to guarantee
    /// write-ahead semantics.
    ///
    /// Sequence numbers are strictly increasing: an event whose
    /// `sequence_num` is not greater than [`Self::last_sequence`] must be
    /// refused with [`JournalError::NonMonotonicSequence`] before anything
    /// is written, leaving the journal unchanged. Gaps are allowed here and
    /// detected by replay.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::NonMonotonicSequence`] for a duplicate or
    /// restarted sequence, or another [`JournalError`] if serialization,
    /// allocation, I/O, or flushing fails.
    fn append(&self, event: &SequencerEvent<T>) -> Result<(), JournalError>;

    /// Read events starting from the given sequence number.
    ///
    /// Returns an iterator that yields events in sequence order, starting
    /// from `sequence` (inclusive). If `sequence` is beyond the last
    /// written entry, the iterator is empty.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if the segment files cannot be opened or
    /// the starting position cannot be located.
    fn read_from(&self, sequence: u64) -> Result<JournalReadIter<T>, JournalError>;

    /// Returns the sequence number of the last entry in the journal.
    ///
    /// Returns `Ok(None)` if the journal is empty.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] when the last sequence cannot be read, for
    /// example [`JournalError::MutexPoisoned`]. An unreadable journal is
    /// never reported as an empty one (#252).
    fn last_sequence(&self) -> Result<Option<u64>, JournalError>;

    /// Verify the integrity of the entire journal by checking every entry's
    /// CRC32 checksum.
    ///
    /// # Errors
    ///
    /// Returns the first [`JournalError::CorruptEntry`] or
    /// [`JournalError::InvalidEntryHeader`] encountered,
    /// [`JournalError::NonMonotonicSequence`] if stored sequences do not
    /// strictly increase, or an I/O error if segment files cannot be read.
    fn verify_integrity(&self) -> Result<(), JournalError>;
}
