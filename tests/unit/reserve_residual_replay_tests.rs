//! #230: replay reproduces the new reserve-residual outcome.
//!
//! The residual policy changed what an aggressive
//! `{10 visible, 20 hidden, no replenish_amount, auto_replenish off}` reserve
//! leaves behind: it used to rest a refreshed residual, and now ends with its
//! hidden remainder discarded. A journal recorded before the change therefore
//! replays to a different book, which is a real compatibility note rather than
//! a replay bug.
//!
//! This file pins the post-change contract: a session recorded through the
//! public sequencer API replays into a book that ends the order exactly as
//! the live one did, and `snapshots_match` certifies the two as replay-equal.
//! It uses `SequencerCommand` / `SequencerEvent` with an `InMemoryJournal`
//! and `ReplayEngine` only; nothing under `src/orderbook/sequencer/` is
//! touched.

#[cfg(test)]
mod tests_reserve_residual_replay {
    use orderbook_rs::orderbook::sequencer::{
        InMemoryJournal, Journal, ReplayEngine, SequencerCommand, SequencerEvent, SequencerResult,
        snapshots_match,
    };
    use orderbook_rs::{Clock, OrderBook, StubClock};
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use std::sync::Arc;

    const SYMBOL: &str = "RSV-REPLAY";
    const PRICE: u128 = 100;
    /// The reserve taker's visible tranche, and the contra depth it meets.
    const VISIBLE: u64 = 10;
    /// The reserve taker's hidden tranche: discarded when the visible one is
    /// exhausted without automatic replenishment.
    const HIDDEN: u64 = 20;

    /// Fixed ids so the journal and the live book rest byte-identical orders:
    /// since #208 `snapshots_match` compares full order identity.
    const CONTRA_ID: u64 = 7_001;
    const TAKER_ID: u64 = 7_002;

    /// Deterministic clock, so live and replayed books agree on timestamps.
    fn stub_clock() -> Arc<dyn Clock> {
        Arc::new(StubClock::starting_at(0))
    }

    /// The resting SELL that the reserve taker will sweep: exactly the
    /// taker's visible tranche, so the sweep empties it and stops.
    fn contra_sell() -> OrderType<()> {
        OrderType::Standard {
            id: Id::from_u64(CONTRA_ID),
            price: Price::new(PRICE),
            quantity: Quantity::new(VISIBLE),
            side: Side::Sell,
            time_in_force: TimeInForce::Gtc,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        }
    }

    /// The aggressive reserve taker: 10 visible / 20 hidden, no explicit
    /// replenish amount, automatic replenishment off.
    fn reserve_taker() -> OrderType<()> {
        OrderType::ReserveOrder {
            id: Id::from_u64(TAKER_ID),
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(VISIBLE),
            hidden_quantity: Quantity::new(HIDDEN),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(0),
            replenish_amount: None,
            auto_replenish: false,
            extra_fields: (),
        }
    }

    /// One `AddOrder` journal entry.
    fn add_event(sequence_num: u64, order: OrderType<()>) -> SequencerEvent<()> {
        let order_id = order.id();
        SequencerEvent {
            sequence_num,
            timestamp_ns: 0,
            command: SequencerCommand::AddOrder(order),
            result: SequencerResult::OrderAdded { order_id },
        }
    }

    /// Journal of the recorded session: seed the contra depth, then submit
    /// the aggressive reserve.
    fn recorded_session() -> (InMemoryJournal<()>, u64) {
        let journal: InMemoryJournal<()> = InMemoryJournal::new();
        assert!(
            journal.append(&add_event(0, contra_sell())).is_ok(),
            "journalling the contra maker must succeed"
        );
        assert!(
            journal.append(&add_event(1, reserve_taker())).is_ok(),
            "journalling the reserve taker must succeed"
        );
        (journal, 1)
    }

    /// Replaying a journal whose reserve taker exhausts its visible tranche
    /// without automatic replenishment reconstructs the ended order, not the
    /// pre-#230 residual: nothing rests on either side, the executed
    /// quantity is the visible tranche, and the replayed book is
    /// `snapshots_match`-equal to the live one.
    #[test]
    fn test_replay_reserve_residual_without_auto_reconstructs_ended_order() {
        let (journal, last_sequence) = recorded_session();

        // Ground truth: the same two commands against a live book.
        let live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
        let seeded = live.add_order(contra_sell());
        assert!(seeded.is_ok(), "seeding the contra maker: {seeded:?}");
        let submitted = live.add_order_with_result(reserve_taker());
        let live_executed: u64 = match submitted {
            Ok((_, Some(trade))) => trade
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|print| print.quantity().as_u64())
                .sum(),
            other => panic!("expected a trade from the aggressive reserve, got {other:?}"),
        };
        assert_eq!(
            live_executed, VISIBLE,
            "the live session executes exactly the visible tranche"
        );
        let live_snapshot = live.create_snapshot(usize::MAX).expect("snapshot");

        // Replay into a fresh book under the same clock.
        let (replayed, sequence) =
            ReplayEngine::<()>::replay_from_with_clock(&journal, 0, SYMBOL, stub_clock())
                .expect("replay must succeed");
        assert_eq!(sequence, last_sequence, "every journalled event applied");

        // The reserve taker ended: it rests nowhere, and neither does the
        // contra maker it consumed.
        assert!(
            replayed.get_order(Id::from_u64(TAKER_ID)).is_none(),
            "the discarded residual must not rest on the replayed book"
        );
        assert!(
            replayed.best_bid().is_none(),
            "no bid level may survive the ended residual"
        );
        assert!(
            replayed.get_order(Id::from_u64(CONTRA_ID)).is_none(),
            "the contra maker was fully consumed"
        );
        assert!(
            replayed.best_ask().is_none(),
            "no ask level may survive the sweep"
        );

        // The replay executed the same quantity at the same price.
        assert_eq!(
            replayed.last_trade_price(),
            Some(PRICE),
            "the replayed sweep must have traded at the recorded price"
        );

        // The oracle: live and replayed books are replay-equal.
        let replayed_snapshot = replayed.create_snapshot(usize::MAX).expect("snapshot");
        assert!(
            snapshots_match(&live_snapshot, &replayed_snapshot),
            "replayed book must match the live one that recorded the journal"
        );

        // Both sides are genuinely empty, so the match above is not a
        // vacuous comparison of two partially-populated books.
        assert!(
            live_snapshot.bids.is_empty() && live_snapshot.asks.is_empty(),
            "the live session ends with an empty book"
        );
        assert!(
            replayed_snapshot.bids.is_empty() && replayed_snapshot.asks.is_empty(),
            "the replayed session ends with an empty book"
        );
    }
}
