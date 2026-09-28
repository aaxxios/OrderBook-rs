use criterion::{BatchSize, BenchmarkId, Criterion};
use orderbook_rs::orderbook::sequencer::{
    InMemoryJournal, Journal, ReplayEngine, SequencerCommand, SequencerEvent, SequencerResult,
};
use pricelevel::{Hash32, Id, Price, Quantity, Side, TimeInForce, TimestampMs};
use std::hint::black_box;
use uuid::Uuid;

/// Build an add-order event for the journal.
fn make_add_event(seq: u64, id: Id, price: u128, qty: u64, side: Side) -> SequencerEvent<()> {
    let order = pricelevel::OrderType::Standard {
        id,
        price: Price::new(price),
        quantity: Quantity::new(qty),
        side,
        time_in_force: TimeInForce::Gtc,
        user_id: Hash32::zero(),
        timestamp: TimestampMs::new(0),
        extra_fields: (),
    };
    SequencerEvent {
        sequence_num: seq,
        timestamp_ns: seq.saturating_mul(1_000_000),
        command: SequencerCommand::AddOrder(order),
        result: SequencerResult::OrderAdded { order_id: id },
    }
}

/// Populate a journal with `n` non-crossing add-order events.
fn make_journal(n: usize) -> InMemoryJournal<()> {
    let journal = InMemoryJournal::new();
    for i in 0..n {
        let id = Id::from_uuid(Uuid::new_v4());
        let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
        let price = if side == Side::Buy {
            1000_u128.saturating_sub((i % 100) as u128)
        } else {
            1100_u128.saturating_add((i % 100) as u128)
        };
        let _ = journal.append(&make_add_event(i as u64, id, price, 10, side));
    }
    journal
}

/// Register journal and replay benchmarks.
///
/// # Methodology (issue #258)
///
/// `journal_append` used `iter_with_setup` (= `iter_batched` with
/// `BatchSize::PerIteration`) with a *consuming* `routine`, so the fresh
/// `InMemoryJournal` (holding up to 10 000 appended events) was dropped
/// as the closure returned — inside the timed window. `iter_batched_ref`
/// takes `(journal, events)` by `&mut` so that drop happens after the
/// window closes; only the `append` calls are timed.
/// `replay_from_journal` built the `journal` outside the loop already
/// (good) but used plain `b.iter`, whose timing model also includes
/// `mem::drop` of whatever the closure returns — here, a freshly
/// replayed `OrderBook` with up to 10 000 orders. `iter_with_large_drop`
/// defers that drop past the measurement window.
pub fn register_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("OrderBook - Replay");

    // ─── Journal append ─────────────────────────────────────────────
    for &event_count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::new("journal_append", event_count),
            &event_count,
            |b, &count| {
                b.iter_batched_ref(
                    || {
                        let ids: Vec<_> =
                            (0..count).map(|_| Id::from_uuid(Uuid::new_v4())).collect();
                        let events: Vec<_> = ids
                            .iter()
                            .enumerate()
                            .map(|(i, &id)| {
                                let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
                                let price = if side == Side::Buy {
                                    1000_u128.saturating_sub((i % 100) as u128)
                                } else {
                                    1100_u128.saturating_add((i % 100) as u128)
                                };
                                make_add_event(i as u64, id, price, 10, side)
                            })
                            .collect();
                        (InMemoryJournal::<()>::new(), events)
                    },
                    |(journal, events)| {
                        for event in events.iter() {
                            let _ = black_box(journal.append(event));
                        }
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // ─── Full replay from journal ───────────────────────────────────
    for &event_count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::new("replay_from_journal", event_count),
            &event_count,
            |b, &count| {
                let journal = make_journal(count);
                b.iter_with_large_drop(|| {
                    black_box(
                        ReplayEngine::<()>::replay_from(&journal, 0, "BENCH")
                            .expect("replay must succeed"),
                    )
                });
            },
        );
    }

    // ─── Replay verify ──────────────────────────────────────────────
    for &event_count in &[100, 1_000] {
        group.bench_with_input(
            BenchmarkId::new("replay_verify", event_count),
            &event_count,
            |b, &count| {
                let journal = make_journal(count);
                let (book, _) = ReplayEngine::<()>::replay_from(&journal, 0, "BENCH")
                    .expect("replay must succeed");
                let expected = book.create_snapshot(usize::MAX).expect("snapshot");
                b.iter(|| {
                    let result = ReplayEngine::<()>::verify(&journal, &expected);
                    let ok = result.expect("replay verification must succeed");
                    assert!(black_box(ok), "replay verification diverged");
                });
            },
        );
    }

    group.finish();
}
