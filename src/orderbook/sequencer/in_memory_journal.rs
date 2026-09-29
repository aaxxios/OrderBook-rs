//! In-memory journal implementation for testing and benchmarking.
//!
//! [`InMemoryJournal`] stores all events in a `Vec` in insertion order.
//! Suitable for testing, benchmarking, and short-lived workloads where
//! persistence is not required.

use super::error::JournalError;
use super::journal::{Journal, JournalEntry, JournalReadIter};
use super::types::SequencerEvent;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Stored events. Each is behind an `Arc` so a reader can snapshot the
/// range it needs under the read guard with refcount bumps only, and run
/// `T::clone` (caller code) after releasing it.
type Events<T> = Vec<Arc<SequencerEvent<T>>>;

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
/// # Locking
///
/// `T::clone` is caller code and never runs under the internal lock:
/// `append` clones before taking the write guard, and `read_from`
/// snapshots `Arc` handles to the requested range under the read guard,
/// then clones each event lazily as the iterator yields it.
///
/// # Parity limits with `FileJournal`
///
/// `InMemoryJournal` stores clones, not bytes. It does not round-trip
/// events through JSON and has no CRC (`JournalEntry::stored_crc` is `0`),
/// so it cannot surface what only a byte format can:
///
/// - a `T` whose serde round-trip is lossy or fails reads back unchanged
///   here, while `FileJournal` returns the decoded value or a
///   `SerializationError` / `DeserializationError`;
/// - corruption, torn tails and truncation (`CorruptEntry`,
///   `InvalidEntryHeader`) cannot happen, and `verify_integrity` only
///   checks the lock;
/// - no entry-size limit applies (`EntryTooLarge`).
///
/// Use `FileJournal` (feature `journal`) where those paths matter.
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
    events: RwLock<Events<T>>,
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
    fn read_events(&self) -> Result<RwLockReadGuard<'_, Events<T>>, JournalError> {
        self.events.read().map_err(|_| JournalError::MutexPoisoned)
    }

    /// Acquires the write lock, mapping poisoning to a typed error.
    #[inline]
    fn write_events(&self) -> Result<RwLockWriteGuard<'_, Events<T>>, JournalError> {
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
        let owned = Arc::new(event.clone());
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
        // Under the read guard: only `Arc` refcount bumps, no caller code.
        let snapshot: Events<T> = {
            let events = self.read_events()?;
            // Sequences are strictly increasing (enforced by `append`).
            let start = events.partition_point(|e| e.sequence_num < sequence);
            let range = events.get(start..).unwrap_or_default();
            let mut snapshot = Vec::new();
            snapshot.try_reserve_exact(range.len()).map_err(|_| {
                JournalError::AllocationFailed {
                    what: "events",
                    requested: range.len(),
                }
            })?;
            snapshot.extend(range.iter().cloned());
            snapshot
        };

        // `T::clone` runs here, per yielded entry, with no lock held.
        Ok(Box::new(snapshot.into_iter().map(|event| {
            Ok(JournalEntry {
                event: SequencerEvent::clone(&event),
                stored_crc: 0, // No CRC for in-memory journal
            })
        })))
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
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects)]
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

    /// Extra-fields probe whose `clone` records whether the journal's lock
    /// was free (a write guard could be taken) while it ran.
    #[derive(Debug, Serialize, Deserialize)]
    struct LockProbe;

    thread_local! {
        static PROBED: std::cell::RefCell<Option<Arc<InMemoryJournal<LockProbe>>>> =
            const { std::cell::RefCell::new(None) };
        static CLONES_UNDER_LOCK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static CLONES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    impl Clone for LockProbe {
        fn clone(&self) -> Self {
            PROBED.with(|slot| {
                if let Some(journal) = slot.borrow().as_ref() {
                    CLONES.with(|c| c.set(c.get() + 1));
                    if journal.events.try_write().is_err() {
                        CLONES_UNDER_LOCK.with(|c| c.set(c.get() + 1));
                    }
                }
            });
            LockProbe
        }
    }

    fn probe_event(seq: u64) -> SequencerEvent<LockProbe> {
        let id = Id::from_u64(seq);
        SequencerEvent {
            sequence_num: seq,
            timestamp_ns: 0,
            command: SequencerCommand::AddOrder(pricelevel::OrderType::Standard {
                id,
                price: pricelevel::Price::new(100),
                quantity: pricelevel::Quantity::new(1),
                side: pricelevel::Side::Buy,
                time_in_force: pricelevel::TimeInForce::Gtc,
                user_id: pricelevel::Hash32::zero(),
                timestamp: pricelevel::TimestampMs::new(0),
                extra_fields: LockProbe,
            }),
            result: SequencerResult::OrderAdded { order_id: id },
        }
    }

    /// #295: `T::clone` (caller code) never runs under the journal lock,
    /// on append or on read.
    #[test]
    fn test_in_memory_clones_t_outside_the_lock() {
        let journal = Arc::new(InMemoryJournal::<LockProbe>::new());
        let events: Vec<_> = (1..=3).map(probe_event).collect();
        PROBED.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&journal)));
        for event in &events {
            journal.append(event).expect("append");
        }
        let read: Vec<_> = journal
            .read_from(2)
            .expect("read_from")
            .map(|entry| entry.expect("entry").event.sequence_num)
            .collect();
        PROBED.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(read, vec![2, 3]);
        assert!(
            CLONES.with(std::cell::Cell::get) >= 5,
            "3 appends + 2 reads"
        );
        assert_eq!(
            CLONES_UNDER_LOCK.with(std::cell::Cell::get),
            0,
            "no T::clone may run while the journal lock is held"
        );
    }

    #[test]
    fn test_in_memory_read_from_filters_by_sequence() {
        let journal = InMemoryJournal::<()>::new();
        for seq in [1, 4, 7] {
            journal.append(&event(seq)).expect("append");
        }
        let seqs = |from| -> Vec<u64> {
            journal
                .read_from(from)
                .expect("read_from")
                .map(|e| e.expect("entry").event.sequence_num)
                .collect()
        };
        assert_eq!(seqs(0), vec![1, 4, 7]);
        assert_eq!(seqs(4), vec![4, 7]);
        assert_eq!(seqs(5), vec![7]);
        assert!(seqs(8).is_empty());
    }
}
