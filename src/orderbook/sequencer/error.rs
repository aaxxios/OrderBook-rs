//! Error types for the journal subsystem.
//!
//! [`JournalError`] covers all failure modes of the append-only event
//! journal, including I/O errors, corruption, misuse (non-monotonic
//! sequences, segment collisions) and capacity issues.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur within the journal subsystem.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JournalError {
    /// An I/O error occurred while reading or writing journal files.
    #[error("journal I/O error{}: {message}", describe_path(.path.as_deref()))]
    Io {
        /// The underlying I/O error message.
        message: String,
        /// The file path involved, if known.
        path: Option<PathBuf>,
    },

    /// A journal entry failed CRC32 integrity verification.
    #[error(
        "corrupt journal entry at sequence {sequence}: expected CRC {expected_crc:#010x}, got {actual_crc:#010x}"
    )]
    CorruptEntry {
        /// The sequence number of the corrupt entry.
        sequence: u64,
        /// The expected CRC32 checksum.
        expected_crc: u32,
        /// The actual CRC32 checksum computed from the entry bytes.
        actual_crc: u32,
    },

    /// The journal entry payload could not be deserialized.
    #[error("journal deserialization error at sequence {sequence}: {message}")]
    DeserializationError {
        /// The sequence number of the entry that failed to deserialize.
        sequence: u64,
        /// The underlying deserialization error message.
        message: String,
    },

    /// The journal entry payload could not be serialized.
    #[error("journal serialization error: {message}")]
    SerializationError {
        /// The underlying serialization error message.
        message: String,
    },

    /// An entry does not fit in a segment, or its length does not fit the
    /// on-disk 32-bit `entry_length` field.
    #[error(
        "journal entry too large: {entry_bytes} bytes exceeds segment size {segment_size} bytes"
    )]
    EntryTooLarge {
        /// The size of the serialized entry (or payload) in bytes.
        entry_bytes: usize,
        /// The maximum segment size in bytes.
        segment_size: usize,
    },

    /// The journal directory does not exist or is not accessible.
    #[error("invalid journal directory: {}", .path.display())]
    InvalidDirectory {
        /// The path that was expected to be a valid directory.
        path: PathBuf,
    },

    /// An internal lock was poisoned (another thread panicked while
    /// holding it).
    #[error("journal internal mutex poisoned")]
    MutexPoisoned,

    /// The requested sequence number was not found in the journal.
    #[error("sequence {sequence} not found in journal")]
    SequenceNotFound {
        /// The sequence number that was requested.
        sequence: u64,
    },

    /// The journal entry has an invalid header (truncated or malformed).
    ///
    /// Returned by reads and by `verify_integrity` for any non-zero
    /// `entry_length` that does not describe a well-formed entry inside the
    /// segment. Only a zero `entry_length` marks the end of written data.
    #[error("invalid journal entry header at offset {offset}: {message}")]
    InvalidEntryHeader {
        /// Byte offset within the segment where the error occurred.
        offset: usize,
        /// Description of the header problem.
        message: String,
    },

    /// A protocol counter (archived-segment tally, segment index, write
    /// position) overflowed while advancing. Surfaced as a typed error rather
    /// than silently capping, per the no-saturating-on-protocol-counters
    /// rule. Unreachable at any realistic journal size.
    #[error("journal counter overflowed: {counter}")]
    CounterOverflow {
        /// Name of the counter that overflowed.
        counter: &'static str,
    },

    /// An append carried a sequence number that is not strictly greater
    /// than the last one in the journal (#252).
    ///
    /// Journals are append-only and strictly increasing; a duplicate or
    /// restarted sequence is refused before anything is written, so the
    /// journal is unchanged.
    #[error("non-monotonic journal sequence: attempted {attempted} after last {last}")]
    NonMonotonicSequence {
        /// The last sequence number already in the journal.
        last: u64,
        /// The sequence number the rejected append carried.
        attempted: u64,
    },

    /// Segment rotation found a segment file already present at the new
    /// segment's path (#252).
    ///
    /// Segments are created with `create_new`, never truncated: an existing
    /// file (a stray segment, or one left by another writer) is preserved
    /// and the append fails without writing.
    #[error("journal segment already exists: {}", .path.display())]
    SegmentExists {
        /// Path of the existing segment file.
        path: PathBuf,
    },

    /// A fallible allocation (`try_reserve`) failed (#252).
    #[error("journal allocation failed: could not reserve {requested} {what}")]
    AllocationFailed {
        /// What was being reserved (for example `"entry bytes"`).
        what: &'static str,
        /// The number of units requested.
        requested: usize,
    },
}

/// Renders the optional path of an [`JournalError::Io`] as ` at <path>`.
fn describe_path(path: Option<&Path>) -> String {
    match path {
        Some(p) => format!(" at {}", p.display()),
        None => String::new(),
    }
}

impl From<std::io::Error> for JournalError {
    #[cold]
    fn from(err: std::io::Error) -> Self {
        JournalError::Io {
            message: err.to_string(),
            path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counter_overflow_display() {
        let err = JournalError::CounterOverflow {
            counter: "archived segment count",
        };
        let msg = err.to_string();
        assert!(msg.contains("counter overflowed"));
        assert!(msg.contains("archived segment count"));
    }

    #[test]
    fn test_io_display_with_and_without_path() {
        let with = JournalError::Io {
            message: "boom".to_string(),
            path: Some(PathBuf::from("/tmp/j")),
        };
        assert_eq!(with.to_string(), "journal I/O error at /tmp/j: boom");
        let without = JournalError::Io {
            message: "boom".to_string(),
            path: None,
        };
        assert_eq!(without.to_string(), "journal I/O error: boom");
    }

    #[test]
    fn test_new_variants_display() {
        let err = JournalError::NonMonotonicSequence {
            last: 7,
            attempted: 7,
        };
        assert_eq!(
            err.to_string(),
            "non-monotonic journal sequence: attempted 7 after last 7"
        );
        let err = JournalError::SegmentExists {
            path: PathBuf::from("/tmp/j/segment-1.journal"),
        };
        assert!(err.to_string().contains("segment-1.journal"));
        let err = JournalError::AllocationFailed {
            what: "events",
            requested: 3,
        };
        assert!(err.to_string().contains("3 events"));
    }
}
