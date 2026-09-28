//! Journal compatibility pin: a `FileJournal` segment written by
//! orderbook-rs 0.13.1 (pricelevel 0.9.2) must still read, verify and replay
//! under 0.14 / pricelevel 0.10 (#239).
//!
//! The fixture `fixtures/journal_0_13_1/segment-00000000000000000000.journal`
//! is the verbatim segment (16 KiB, zero-padded) produced by a throwaway
//! crate depending on the untouched v0.13.1 tag with features
//! `journal,bincode`. It ran a minimal sequencer (execute on a live book with
//! a `StubClock` at 1_700_000_000_000 ms, journal the command with the outcome
//! the command API returned) over symbol `JRNL`:
//!
//! | seq | command                                   | journaled result          |
//! |-----|-------------------------------------------|---------------------------|
//! | 1   | add standard sell 10 @ 100 (id 1)         | `OrderAdded`              |
//! | 2   | add iceberg sell 5 + 20 @ 100 (id 2)      | `OrderAdded`              |
//! | 3   | add standard sell 7 @ 101 (id 3)          | `OrderAdded`              |
//! | 4   | add standard buy 9 @ 99 (id 4)            | `OrderAdded`              |
//! | 5   | market buy 12 (id 10)                     | `TradeExecuted` (2 trades)|
//! | 6   | add standard sell 1 @ 105 (id 3, dup)     | `RejectedWithCode`        |
//! | 7   | add standard buy 5 @ 98 (id 5)            | `OrderAdded`              |
//! | 8   | add standard buy 3 @ 97 (id 6)            | `OrderAdded`              |
//! | 9   | cancel by side (buy)                      | `MassCancelled` (3 ids)   |
//! | 10  | cancel order id 3                         | `OrderCancelled`          |
//!
//! The 0.13.1 book ended with a single ask level at 100 holding the iceberg
//! (3 visible, 20 hidden).

#[cfg(all(test, feature = "journal"))]
mod tests {
    use crate::orderbook::OrderBook;
    use crate::orderbook::reject_reason::RejectReason;
    use crate::orderbook::sequencer::{
        FileJournal, Journal, ReplayEngine, SequencerCommand, SequencerEvent, SequencerResult,
        snapshots_match,
    };
    use pricelevel::Id;

    const SYMBOL: &str = "JRNL";
    const SEGMENT: &[u8] =
        include_bytes!("fixtures/journal_0_13_1/segment-00000000000000000000.journal");

    /// Copies the verbatim fixture into a fresh directory (opening a
    /// `FileJournal` may append / remap, so the committed bytes are never
    /// opened in place).
    fn open_fixture() -> (tempfile::TempDir, FileJournal<()>) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("segment-00000000000000000000.journal"),
            SEGMENT,
        )
        .expect("write fixture segment");
        let journal = FileJournal::<()>::open(dir.path()).expect("open 0.13.1 journal");
        (dir, journal)
    }

    fn read_all(journal: &FileJournal<()>) -> Vec<SequencerEvent<()>> {
        journal
            .read_from(0)
            .expect("read_from")
            .map(|entry| entry.expect("every 0.13.1 entry decodes").event)
            .collect()
    }

    #[test]
    fn test_journal_0_13_1_reads_and_verifies() {
        let (_dir, journal) = open_fixture();
        journal
            .verify_integrity()
            .expect("0.13.1 CRCs verify under 0.14");
        assert_eq!(journal.last_sequence(), Some(10));

        let events = read_all(&journal);
        assert_eq!(events.len(), 10, "all ten events decode");
        let sequences: Vec<u64> = events.iter().map(|e| e.sequence_num).collect();
        assert_eq!(sequences, (1..=10).collect::<Vec<u64>>());

        // TradeExecuted embeds a pricelevel MatchResult written without the
        // 0.10 `error` field: it decodes as "no error".
        match &events[4].result {
            SequencerResult::TradeExecuted { trade_result } => {
                let trades = trade_result.match_result.trades().as_vec();
                assert_eq!(trades.len(), 2);
                assert_eq!(trades[0].maker_order_id(), Id::from_u64(1));
                assert_eq!(trades[0].quantity().as_u64(), 10);
                assert_eq!(trades[1].maker_order_id(), Id::from_u64(2));
                assert_eq!(trades[1].quantity().as_u64(), 2);
                assert!(trade_result.match_result.error().is_none());
                assert_eq!(trade_result.total_maker_fees, 0);
                assert_eq!(trade_result.total_taker_fees, 0);
            }
            other => panic!("seq 5 must be TradeExecuted, got {other:?}"),
        }

        match &events[5].result {
            SequencerResult::RejectedWithCode { code, .. } => {
                assert_eq!(*code, RejectReason::DuplicateOrderId);
            }
            other => panic!("seq 6 must be RejectedWithCode, got {other:?}"),
        }

        // MassCancelled written without the 0.14 `failures` field decodes
        // with an empty failure list.
        match &events[8].result {
            SequencerResult::MassCancelled { result } => {
                assert_eq!(
                    result.cancelled_order_ids(),
                    &[Id::from_u64(6), Id::from_u64(5), Id::from_u64(4)]
                );
                assert!(result.failures().is_empty());
            }
            other => panic!("seq 9 must be MassCancelled, got {other:?}"),
        }
    }

    /// Replaying the 0.13.1 journal under 0.14 reaches the same book as
    /// executing the same commands directly on a fresh 0.14 book, and the
    /// re-executed trade / mass-cancel outcomes equal what 0.13.1 journaled.
    #[test]
    fn test_journal_0_13_1_replays_to_reference_book() {
        let (_dir, journal) = open_fixture();
        let events = read_all(&journal);

        // The fixture sequencer numbered events from 1, so replay starts there.
        let (replayed, last) =
            ReplayEngine::<()>::replay_from(&journal, 1, SYMBOL).expect("replay 0.13.1 journal");
        assert_eq!(last, 10);

        let reference: OrderBook<()> = OrderBook::new(SYMBOL);
        for event in &events {
            match (&event.command, &event.result) {
                (SequencerCommand::AddOrder(order), SequencerResult::OrderAdded { .. }) => {
                    reference.add_order(*order).expect("add");
                }
                (SequencerCommand::AddOrder(order), SequencerResult::RejectedWithCode { .. }) => {
                    assert!(reference.add_order(*order).is_err(), "still rejected");
                }
                (
                    SequencerCommand::MarketOrder { id, quantity, side },
                    SequencerResult::TradeExecuted { trade_result },
                ) => {
                    let result = reference
                        .submit_market_order(*id, *quantity, *side)
                        .expect("market order");
                    let live: Vec<(Id, u64, u128)> = result
                        .trades()
                        .as_vec()
                        .iter()
                        .map(|t| {
                            (
                                t.maker_order_id(),
                                t.quantity().as_u64(),
                                t.price().as_u128(),
                            )
                        })
                        .collect();
                    let recorded: Vec<(Id, u64, u128)> = trade_result
                        .match_result
                        .trades()
                        .as_vec()
                        .iter()
                        .map(|t| {
                            (
                                t.maker_order_id(),
                                t.quantity().as_u64(),
                                t.price().as_u128(),
                            )
                        })
                        .collect();
                    assert_eq!(live, recorded, "same fills as 0.13.1");
                }
                (
                    SequencerCommand::CancelBySide { side },
                    SequencerResult::MassCancelled { result },
                ) => {
                    let live = reference.cancel_orders_by_side(*side);
                    assert_eq!(
                        live.cancelled_order_ids(),
                        result.cancelled_order_ids(),
                        "same mass-cancel ids as 0.13.1"
                    );
                    assert!(!live.has_failures());
                }
                (SequencerCommand::CancelOrder(id), SequencerResult::OrderCancelled { .. }) => {
                    assert!(reference.cancel_order(*id).expect("cancel").is_some());
                }
                (command, result) => panic!("unexpected fixture event {command:?} / {result:?}"),
            }
        }

        let replayed_snapshot = replayed.create_snapshot(usize::MAX).expect("snapshot");
        let reference_snapshot = reference.create_snapshot(usize::MAX).expect("snapshot");
        assert!(
            snapshots_match(&replayed_snapshot, &reference_snapshot),
            "replayed book must match the reference book"
        );

        // And both equal the 0.13.1 end state.
        assert!(replayed_snapshot.bids.is_empty());
        assert_eq!(replayed_snapshot.asks.len(), 1);
        let level = &replayed_snapshot.asks[0];
        assert_eq!(level.price().as_u128(), 100);
        assert_eq!(level.visible_quantity().as_u64(), 3);
        assert_eq!(level.hidden_quantity().as_u64(), 20);
        assert_eq!(replayed.best_ask(), Some(100));
        assert_eq!(replayed.best_bid(), None);
    }
}
