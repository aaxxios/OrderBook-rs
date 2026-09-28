//! In-memory journal implementation for testing and benchmarking.
//!
//! [`InMemoryJournal`] stores all events in a `Vec` in insertion order.
//! Suitable for testing, benchmarking, and short-lived workloads where
//! persistence is not required.

use super::error::JournalError;
use super::journal::{Journal, JournalEntry, JournalReadIter};
use super::types::SequencerEvent;
use serde::{Deserialize, Serialize};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// In-memory implementation of [`Journal`].
///
/// Stores all events in a `Vec` in insertion order. Suitable for testing,
/// benchmarking, and short-lived workloads where persistence is not required.
///
/// Behaves like `FileJournal` on the shared [`Journal`] surface: appends
/// must carry strictly increasing sequence numbers
/// ([`JournalError::NonMonotonicSequence`] otherwise), and a poisoned
/// internal lock surfaces as [`JournalError::MutexPoisoned`] on every
/// method, never as an empty journal.
///
/// # Examples
///
/// ```
/// use orderbook_rs::orderbook::sequencer::{InMemoryJournal, Journal, JournalError, SequencerCommand, SequencerEvent, SequencerResult};
/// use pricelevel::Id;
/// use uuid::Uuid;
///
/// # fn main() -> Result<(), JournalError> {
/// let journal: InMemoryJournal<()> = InMemoryJournal::new();
/// assert_eq!(journal.last_sequence()?, None);
///
/// let event = SequencerEvent {
///     sequence_num: 1,
///     timestamp_ns: 0,
///     command: SequencerCommand::CancelOrder(Id::from_uuid(Uuid::new_v4())),
///     result: SequencerResult::OrderCancelled { order_id: Id::from_uuid(Uuid::new_v4()) },
/// };
/// journal.append(&event)?;
/// assert_eq!(journal.last_sequence()?, Some(1));
///
/// // A duplicate sequence is refused and the journal is unchanged.
/// assert!(matches!(
///     journal.append(&event),
///     Err(JournalError::NonMonotonicSequence { last: 1, attempted: 1 })
/// ));
/// assert_eq!(journal.len()?, 1);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct InMemoryJournal<T> {
    events: RwLock<Vec<SequencerEvent<T>>>,
}

impl<T> Default for InMemoryJournal<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> InMemoryJournal<T> {
    /// Creates a new empty in-memory journal.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: RwLock::new(Vec::new()),
        }
    }

    /// Creates a new in-memory journal with pre-allocated capacity for
    /// `capacity` events.
    ///
    /// Use this when the approximate number of events is known in advance
    /// to avoid repeated reallocations.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::AllocationFailed`] when the capacity cannot
    /// be reserved (capacity overflow or allocator refusal).
    pub fn with_capacity(capacity: usize) -> Result<Self, JournalError> {
        let mut events = Vec::new();
        events
            .try_reserve_exact(capacity)
            .map_err(|_| JournalError::AllocationFailed {
                what: "events",
                requested: capacity,
            })?;
        Ok(Self {
            events: RwLock::new(events),
        })
    }

    /// Returns the total number of events stored.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::MutexPoisoned`] if the internal lock is
    /// poisoned.
    pub fn len(&self) -> Result<usize, JournalError> {
        Ok(self.read_events()?.len())
    }

    /// Returns `true` if no events have been appended.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::MutexPoisoned`] if the internal lock is
    /// poisoned.
    pub fn is_empty(&self) -> Result<bool, JournalError> {
        Ok(self.read_events()?.is_empty())
    }

    /// Acquires the read lock, mapping poisoning to a typed error.
    #[inline]
    fn read_events(&self) -> Result<RwLockReadGuard<'_, Vec<SequencerEvent<T>>>, JournalError> {
        self.events.read().map_err(|_| JournalError::MutexPoisoned)
    }

    /// Acquires the write lock, mapping poisoning to a typed error.
    #[inline]
    fn write_events(&self) -> Result<RwLockWriteGuard<'_, Vec<SequencerEvent<T>>>, JournalError> {
        self.events.write().map_err(|_| JournalError::MutexPoisoned)
    }
}

impl<T> Journal<T> for InMemoryJournal<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn append(&self, event: &SequencerEvent<T>) -> Result<(), JournalError> {
        // Clone outside the lock: `T::clone` is caller code and must not run
        // while the write guard is held.
        let owned = event.clone();
        let mut events = self.write_events()?;
        if let Some(last) = events.last().map(|e| e.sequence_num)
            && owned.sequence_num <= last
        {
            return Err(JournalError::NonMonotonicSequence {
                last,
                attempted: owned.sequence_num,
            });
        }
        events
            .try_reserve(1)
            .map_err(|_| JournalError::AllocationFailed {
                what: "events",
                requested: 1,
            })?;
        events.push(owned);
        Ok(())
    }

    fn read_from(&self, sequence: u64) -> Result<JournalReadIter<T>, JournalError> {
        let events = self.read_events()?;

        let filtered: Vec<_> = events
            .iter()
            .filter(|e| e.sequence_num >= sequence)
            .map(|event| {
                Ok(JournalEntry {
                    event: event.clone(),
                    stored_crc: 0, // No CRC for in-memory journal
                })
            })
            .collect();

        Ok(Box::new(filtered.into_iter()))
    }

    fn last_sequence(&self) -> Result<Option<u64>, JournalError> {
        Ok(self.read_events()?.last().map(|e| e.sequence_num))
    }

    fn verify_integrity(&self) -> Result<(), JournalError> {
        // No on-disk representation, and `append` enforces strictly
        // increasing sequences, so the stored events are always valid. The
        // lock is still taken so a poisoned journal is reported.
        self.read_events().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orderbook::sequencer::types::{SequencerCommand, SequencerResult};
    use pricelevel::Id;

    fn event(seq: u64) -> SequencerEvent<()> {
        SequencerEvent {
            sequence_num: seq,
            timestamp_ns: 0,
            command: SequencerCommand::CancelOrder(Id::from_u64(seq)),
            result: SequencerResult::OrderCancelled {
                order_id: Id::from_u64(seq),
            },
        }
    }

    fn poison(journal: &InMemoryJournal<()>) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = journal.events.write().expect("lock to poison");
            panic!("intentional poison");
        }));
    }

    #[test]
    fn test_in_memory_append_rejects_duplicate_and_lower_sequences() {
        let journal = InMemoryJournal::<()>::new();
        journal.append(&event(5)).expect("first");
        for bad in [5, 4, 0] {
            match journal.append(&event(bad)) {
                Err(JournalError::NonMonotonicSequence { last, attempted }) => {
                    assert_eq!((last, attempted), (5, bad));
                }
                other => panic!("expected NonMonotonicSequence, got {other:?}"),
            }
        }
        journal.append(&event(9)).expect("gaps are allowed");
        assert_eq!(journal.len().expect("len"), 2);
        assert_eq!(journal.last_sequence().expect("last"), Some(9));
    }

    #[test]
    fn test_in_memory_with_capacity_overflow_is_typed() {
        match InMemoryJournal::<()>::with_capacity(usize::MAX) {
            Err(JournalError::AllocationFailed { requested, .. }) => {
                assert_eq!(requested, usize::MAX);
            }
            other => panic!("expected AllocationFailed, got {other:?}"),
        }
        let journal = InMemoryJournal::<()>::with_capacity(16).expect("small capacity");
        assert!(journal.is_empty().expect("is_empty"));
    }

    #[test]
    fn test_in_memory_poisoned_lock_is_typed_everywhere() {
        let journal = InMemoryJournal::<()>::new();
        journal.append(&event(1)).expect("append");
        poison(&journal);
        assert!(matches!(
            journal.append(&event(2)),
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
        assert!(matches!(journal.len(), Err(JournalError::MutexPoisoned)));
        assert!(matches!(
            journal.is_empty(),
            Err(JournalError::MutexPoisoned)
        ));
        assert!(matches!(
            journal.verify_integrity(),
            Err(JournalError::MutexPoisoned)
        ));
    }
}
