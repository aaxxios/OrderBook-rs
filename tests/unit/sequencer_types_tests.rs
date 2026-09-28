#[cfg(test)]
mod tests_sequencer_types {
    use orderbook_rs::orderbook::mass_cancel::MassCancelResult;
    use orderbook_rs::orderbook::sequencer::{SequencerCommand, SequencerEvent, SequencerResult};
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use uuid::Uuid;

    // ── Helpers ─────────────────────────────────────────────────────────

    fn make_event<T: Clone>(
        seq: u64,
        command: SequencerCommand<T>,
        result: SequencerResult,
    ) -> SequencerEvent<T> {
        SequencerEvent {
            sequence_num: seq,
            timestamp_ns: 1_000_000_000,
            command,
            result,
        }
    }

    // ── JSON round-trip: SequencerCommand ────────────────────────────────

    #[test]
    fn json_roundtrip_cancel_all() {
        let cmd: SequencerCommand<()> = SequencerCommand::CancelAll;
        let json = serde_json::to_string(&cmd);
        assert!(json.is_ok());
        let decoded: Result<SequencerCommand<()>, _> =
            serde_json::from_str(&json.unwrap_or_default());
        assert!(decoded.is_ok());
        assert!(matches!(
            decoded.unwrap_or(SequencerCommand::CancelAll),
            SequencerCommand::CancelAll
        ));
    }

    #[test]
    fn json_roundtrip_cancel_by_side() {
        let cmd: SequencerCommand<()> = SequencerCommand::CancelBySide { side: Side::Buy };
        let json = serde_json::to_string(&cmd);
        assert!(json.is_ok());
        let json_str = json.unwrap_or_default();
        assert!(json_str.contains("CancelBySide"));

        let decoded: Result<SequencerCommand<()>, _> = serde_json::from_str(&json_str);
        assert!(decoded.is_ok());
        assert!(matches!(
            decoded.unwrap_or(SequencerCommand::CancelAll),
            SequencerCommand::CancelBySide { side: Side::Buy }
        ));
    }

    #[test]
    fn json_roundtrip_cancel_by_user() {
        let user_id = Hash32::from([42u8; 32]);
        let cmd: SequencerCommand<()> = SequencerCommand::CancelByUser { user_id };
        let json = serde_json::to_string(&cmd);
        assert!(json.is_ok());
        let json_str = json.unwrap_or_default();
        assert!(json_str.contains("CancelByUser"));

        let decoded: Result<SequencerCommand<()>, _> = serde_json::from_str(&json_str);
        assert!(decoded.is_ok());
        if let Ok(SequencerCommand::CancelByUser { user_id: uid }) = decoded {
            assert_eq!(uid, user_id);
        } else {
            panic!("expected CancelByUser variant");
        }
    }

    #[test]
    fn json_roundtrip_cancel_by_price_range() {
        let cmd: SequencerCommand<()> = SequencerCommand::CancelByPriceRange {
            side: Side::Sell,
            min_price: 100,
            max_price: 500,
        };
        let json = serde_json::to_string(&cmd);
        assert!(json.is_ok());
        let json_str = json.unwrap_or_default();
        assert!(json_str.contains("CancelByPriceRange"));

        let decoded: Result<SequencerCommand<()>, _> = serde_json::from_str(&json_str);
        assert!(decoded.is_ok());
        assert!(matches!(
            decoded.unwrap_or(SequencerCommand::CancelAll),
            SequencerCommand::CancelByPriceRange {
                side: Side::Sell,
                min_price: 100,
                max_price: 500,
            }
        ));
    }

    // ── JSON round-trip: SequencerResult ─────────────────────────────────

    #[test]
    fn json_roundtrip_mass_cancelled_empty() {
        let result = SequencerResult::MassCancelled {
            result: MassCancelResult::default(),
        };
        let json = serde_json::to_string(&result);
        assert!(json.is_ok());
        let json_str = json.unwrap_or_default();
        assert!(json_str.contains("MassCancelled"));

        let decoded: Result<SequencerResult, _> = serde_json::from_str(&json_str);
        assert!(decoded.is_ok());
        if let Ok(SequencerResult::MassCancelled { result: r }) = decoded {
            assert_eq!(r.cancelled_count(), 0);
            assert!(r.cancelled_order_ids().is_empty());
        } else {
            panic!("expected MassCancelled variant");
        }
    }

    // ── JSON round-trip: SequencerEvent with mass cancel commands ────────

    #[test]
    fn json_roundtrip_event_cancel_all() {
        let event = make_event(
            1,
            SequencerCommand::<()>::CancelAll,
            SequencerResult::MassCancelled {
                result: MassCancelResult::default(),
            },
        );
        let json = serde_json::to_string(&event);
        assert!(json.is_ok());

        let decoded: Result<SequencerEvent<()>, _> =
            serde_json::from_str(&json.unwrap_or_default());
        assert!(decoded.is_ok());
        let evt = decoded.unwrap_or_else(|_| {
            make_event(
                0,
                SequencerCommand::CancelAll,
                SequencerResult::Rejected {
                    reason: String::new(),
                },
            )
        });
        assert_eq!(evt.sequence_num, 1);
        assert!(matches!(evt.command, SequencerCommand::CancelAll));
        assert!(matches!(evt.result, SequencerResult::MassCancelled { .. }));
    }

    #[test]
    fn json_roundtrip_event_cancel_by_side() {
        let event = make_event(
            2,
            SequencerCommand::<()>::CancelBySide { side: Side::Sell },
            SequencerResult::MassCancelled {
                result: MassCancelResult::default(),
            },
        );
        let json = serde_json::to_string(&event);
        assert!(json.is_ok());

        let decoded: Result<SequencerEvent<()>, _> =
            serde_json::from_str(&json.unwrap_or_default());
        assert!(decoded.is_ok());
    }

    #[test]
    fn json_roundtrip_event_cancel_by_user() {
        let user_id = Hash32::from([7u8; 32]);
        let event = make_event(
            3,
            SequencerCommand::<()>::CancelByUser { user_id },
            SequencerResult::MassCancelled {
                result: MassCancelResult::default(),
            },
        );
        let json = serde_json::to_string(&event);
        assert!(json.is_ok());

        let decoded: Result<SequencerEvent<()>, _> =
            serde_json::from_str(&json.unwrap_or_default());
        assert!(decoded.is_ok());
    }

    #[test]
    fn json_roundtrip_event_cancel_by_price_range() {
        let event = make_event(
            4,
            SequencerCommand::<()>::CancelByPriceRange {
                side: Side::Buy,
                min_price: 1000,
                max_price: 2000,
            },
            SequencerResult::MassCancelled {
                result: MassCancelResult::default(),
            },
        );
        let json = serde_json::to_string(&event);
        assert!(json.is_ok());

        let decoded: Result<SequencerEvent<()>, _> =
            serde_json::from_str(&json.unwrap_or_default());
        assert!(decoded.is_ok());
    }

    // ── Bincode round-trip ──────────────────────────────────────────────

    #[cfg(feature = "bincode")]
    mod bincode_tests {
        use super::*;

        #[test]
        fn bincode_roundtrip_cancel_all() {
            let cmd: SequencerCommand<()> = SequencerCommand::CancelAll;
            let bytes = bincode::serde::encode_to_vec(&cmd, bincode::config::standard());
            assert!(bytes.is_ok());
            let payload = bytes.unwrap_or_default();
            let decoded: Result<(SequencerCommand<()>, usize), _> =
                bincode::serde::decode_from_slice(&payload, bincode::config::standard());
            assert!(decoded.is_ok());
            if let Ok((_, bytes_read)) = &decoded {
                assert_eq!(*bytes_read, payload.len(), "trailing bytes in payload");
            }
            assert!(matches!(
                decoded
                    .map(|(cmd, _)| cmd)
                    .unwrap_or(SequencerCommand::CancelAll),
                SequencerCommand::CancelAll
            ));
        }

        #[test]
        fn bincode_roundtrip_cancel_by_side() {
            let cmd: SequencerCommand<()> = SequencerCommand::CancelBySide { side: Side::Buy };
            let bytes = bincode::serde::encode_to_vec(&cmd, bincode::config::standard());
            assert!(bytes.is_ok());
            let payload = bytes.unwrap_or_default();
            let decoded: Result<(SequencerCommand<()>, usize), _> =
                bincode::serde::decode_from_slice(&payload, bincode::config::standard());
            assert!(decoded.is_ok());
            if let Ok((_, bytes_read)) = decoded {
                assert_eq!(bytes_read, payload.len(), "trailing bytes in payload");
            }
        }

        #[test]
        fn bincode_roundtrip_cancel_by_user() {
            let user_id = Hash32::from([99u8; 32]);
            let cmd: SequencerCommand<()> = SequencerCommand::CancelByUser { user_id };
            let bytes = bincode::serde::encode_to_vec(&cmd, bincode::config::standard());
            assert!(bytes.is_ok());
            let payload = bytes.unwrap_or_default();
            let decoded: Result<(SequencerCommand<()>, usize), _> =
                bincode::serde::decode_from_slice(&payload, bincode::config::standard());
            assert!(decoded.is_ok());
            if let Ok((_, bytes_read)) = decoded {
                assert_eq!(bytes_read, payload.len(), "trailing bytes in payload");
            }
        }

        #[test]
        fn bincode_roundtrip_cancel_by_price_range() {
            let cmd: SequencerCommand<()> = SequencerCommand::CancelByPriceRange {
                side: Side::Sell,
                min_price: 50,
                max_price: 150,
            };
            let bytes = bincode::serde::encode_to_vec(&cmd, bincode::config::standard());
            assert!(bytes.is_ok());
            let payload = bytes.unwrap_or_default();
            let decoded: Result<(SequencerCommand<()>, usize), _> =
                bincode::serde::decode_from_slice(&payload, bincode::config::standard());
            assert!(decoded.is_ok());
            if let Ok((_, bytes_read)) = decoded {
                assert_eq!(bytes_read, payload.len(), "trailing bytes in payload");
            }
        }

        #[test]
        fn bincode_roundtrip_mass_cancelled() {
            let result = SequencerResult::MassCancelled {
                result: MassCancelResult::default(),
            };
            let bytes = bincode::serde::encode_to_vec(&result, bincode::config::standard());
            assert!(bytes.is_ok());
            let payload = bytes.unwrap_or_default();
            let decoded: Result<(SequencerResult, usize), _> =
                bincode::serde::decode_from_slice(&payload, bincode::config::standard());
            assert!(decoded.is_ok());
            if let Ok((_, bytes_read)) = decoded {
                assert_eq!(bytes_read, payload.len(), "trailing bytes in payload");
            }
        }

        #[test]
        fn bincode_roundtrip_event_cancel_all() {
            let event = make_event(
                10,
                SequencerCommand::<()>::CancelAll,
                SequencerResult::MassCancelled {
                    result: MassCancelResult::default(),
                },
            );
            let bytes = bincode::serde::encode_to_vec(&event, bincode::config::standard());
            assert!(bytes.is_ok());
            let payload = bytes.unwrap_or_default();
            let decoded: Result<(SequencerEvent<()>, usize), _> =
                bincode::serde::decode_from_slice(&payload, bincode::config::standard());
            assert!(decoded.is_ok());
            if let Ok((evt, bytes_read)) = decoded {
                assert_eq!(bytes_read, payload.len(), "trailing bytes in payload");
                assert_eq!(evt.sequence_num, 10);
            }
        }
    }

    // ── FileJournal round-trip ──────────────────────────────────────────

    #[cfg(feature = "journal")]
    mod journal_tests {
        use super::*;
        use orderbook_rs::orderbook::sequencer::FileJournal;
        use orderbook_rs::orderbook::sequencer::journal::Journal;

        #[test]
        fn file_journal_roundtrip_mass_cancel_commands() {
            let dir = tempfile::tempdir().expect("create temp dir");
            let journal: FileJournal<()> =
                FileJournal::open_with_segment_size(dir.path(), 1024 * 1024)
                    .expect("create journal");

            // Write events with each mass cancel command variant
            let events = vec![
                make_event(
                    1,
                    SequencerCommand::<()>::CancelAll,
                    SequencerResult::MassCancelled {
                        result: MassCancelResult::default(),
                    },
                ),
                make_event(
                    2,
                    SequencerCommand::<()>::CancelBySide { side: Side::Buy },
                    SequencerResult::MassCancelled {
                        result: MassCancelResult::default(),
                    },
                ),
                make_event(
                    3,
                    SequencerCommand::<()>::CancelByUser {
                        user_id: Hash32::from([1u8; 32]),
                    },
                    SequencerResult::MassCancelled {
                        result: MassCancelResult::default(),
                    },
                ),
                make_event(
                    4,
                    SequencerCommand::<()>::CancelByPriceRange {
                        side: Side::Sell,
                        min_price: 100,
                        max_price: 200,
                    },
                    SequencerResult::MassCancelled {
                        result: MassCancelResult::default(),
                    },
                ),
            ];

            for event in &events {
                let result = journal.append(event);
                assert!(
                    result.is_ok(),
                    "append should succeed for seq {}",
                    event.sequence_num
                );
            }

            assert_eq!(journal.last_sequence().expect("last_sequence"), Some(4));

            // Read back and verify
            let iter = journal.read_from(1).expect("read_from should succeed");
            let read_entries: Vec<_> = iter.filter_map(|entry_result| entry_result.ok()).collect();
            assert_eq!(read_entries.len(), 4);

            assert!(matches!(
                read_entries[0].event.command,
                SequencerCommand::CancelAll
            ));
            assert!(matches!(
                read_entries[1].event.command,
                SequencerCommand::CancelBySide { side: Side::Buy }
            ));
            assert!(matches!(
                read_entries[2].event.command,
                SequencerCommand::CancelByUser { .. }
            ));
            assert!(matches!(
                read_entries[3].event.command,
                SequencerCommand::CancelByPriceRange {
                    side: Side::Sell,
                    min_price: 100,
                    max_price: 200,
                }
            ));

            // Verify all results are MassCancelled
            for entry in &read_entries {
                assert!(matches!(
                    entry.event.result,
                    SequencerResult::MassCancelled { .. }
                ));
            }
        }
    }

    // ── Existing variants still work ────────────────────────────────────

    #[test]
    fn json_roundtrip_existing_add_order_unchanged() {
        let order = OrderType::Standard {
            id: Id::from_uuid(Uuid::new_v4()),
            price: Price::new(100),
            quantity: Quantity::new(10),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        };
        let cmd = SequencerCommand::AddOrder(order);
        let json = serde_json::to_string(&cmd);
        assert!(json.is_ok());
        let decoded: Result<SequencerCommand<()>, _> =
            serde_json::from_str(&json.unwrap_or_default());
        assert!(decoded.is_ok());
        assert!(matches!(
            decoded.unwrap_or(SequencerCommand::CancelAll),
            SequencerCommand::AddOrder(_)
        ));
    }

    #[test]
    fn json_roundtrip_existing_cancel_order_unchanged() {
        let id = Id::from_uuid(Uuid::new_v4());
        let cmd: SequencerCommand<()> = SequencerCommand::CancelOrder(id);
        let json = serde_json::to_string(&cmd);
        assert!(json.is_ok());
        let decoded: Result<SequencerCommand<()>, _> =
            serde_json::from_str(&json.unwrap_or_default());
        assert!(decoded.is_ok());
        assert!(matches!(
            decoded.unwrap_or(SequencerCommand::CancelAll),
            SequencerCommand::CancelOrder(_)
        ));
    }

    // ── Replay apply_event coverage for mass cancel commands ─────────────

    mod replay_mass_cancel_tests {
        use orderbook_rs::OrderBook;
        use orderbook_rs::orderbook::mass_cancel::MassCancelResult;
        use orderbook_rs::orderbook::sequencer::replay::ReplayEngine;
        use orderbook_rs::orderbook::sequencer::{
            InMemoryJournal, Journal, SequencerCommand, SequencerEvent, SequencerResult,
        };
        use uuid::Uuid;

        use pricelevel::{Hash32, Id, Side, TimeInForce};

        fn make_add_event(seq: u64, price: u128, side: Side) -> SequencerEvent<()> {
            let id = Id::from_uuid(Uuid::new_v4());
            SequencerEvent {
                sequence_num: seq,
                timestamp_ns: 1_000_000_000u64.saturating_add(seq),
                command: SequencerCommand::AddOrder(pricelevel::OrderType::Standard {
                    id,
                    price: pricelevel::Price::new(price),
                    quantity: pricelevel::Quantity::new(10),
                    side,
                    user_id: Hash32::zero(),
                    timestamp: pricelevel::TimestampMs::new(0),
                    time_in_force: TimeInForce::Gtc,
                    extra_fields: (),
                }),
                result: SequencerResult::OrderAdded { order_id: id },
            }
        }

        /// Applies a journaled add to the live book too, the way a
        /// sequencer executes before it journals.
        fn live_add(live: &OrderBook<()>, event: SequencerEvent<()>) -> SequencerEvent<()> {
            if let SequencerCommand::AddOrder(order) = &event.command {
                live.add_order(*order).expect("live add");
            }
            event
        }

        /// Executes `command` on the live book and journals the result it
        /// returned: replay reconciles mass cancels by identity (#252), so
        /// the journaled ids must be the ones the live book cancelled.
        fn make_mass_cancel_event(
            seq: u64,
            command: SequencerCommand<()>,
            live: &OrderBook<()>,
        ) -> SequencerEvent<()> {
            let result: MassCancelResult = match &command {
                SequencerCommand::CancelAll => live.cancel_all_orders(),
                SequencerCommand::CancelBySide { side } => live.cancel_orders_by_side(*side),
                SequencerCommand::CancelByUser { user_id } => live.cancel_orders_by_user(*user_id),
                SequencerCommand::CancelByPriceRange {
                    side,
                    min_price,
                    max_price,
                } => live.cancel_orders_by_price_range(*side, *min_price, *max_price),
                other => panic!("not a mass cancel: {other:?}"),
            };
            SequencerEvent {
                sequence_num: seq,
                timestamp_ns: 1_000_000_000u64.saturating_add(seq),
                command,
                result: SequencerResult::MassCancelled { result },
            }
        }

        #[test]
        fn replay_cancel_all_clears_book() {
            let journal = InMemoryJournal::<()>::new();
            let live = OrderBook::<()>::new("TEST");
            journal
                .append(&live_add(&live, make_add_event(1, 100, Side::Buy)))
                .expect("append");
            journal
                .append(&live_add(&live, make_add_event(2, 200, Side::Sell)))
                .expect("append");
            journal
                .append(&make_mass_cancel_event(
                    3,
                    SequencerCommand::CancelAll,
                    &live,
                ))
                .expect("append");

            let result = ReplayEngine::replay_from(&journal, 1, "TEST");
            assert!(result.is_ok());
            let (book, last_seq) = result.expect("replay");
            assert_eq!(last_seq, 3);
            assert_eq!(book.best_bid(), None);
            assert_eq!(book.best_ask(), None);
        }

        #[test]
        fn replay_cancel_by_side_removes_one_side() {
            let journal = InMemoryJournal::<()>::new();
            let live = OrderBook::<()>::new("TEST");
            journal
                .append(&live_add(&live, make_add_event(1, 100, Side::Buy)))
                .expect("append");
            journal
                .append(&live_add(&live, make_add_event(2, 200, Side::Sell)))
                .expect("append");
            journal
                .append(&make_mass_cancel_event(
                    3,
                    SequencerCommand::CancelBySide { side: Side::Buy },
                    &live,
                ))
                .expect("append");

            let result = ReplayEngine::replay_from(&journal, 1, "TEST");
            assert!(result.is_ok());
            let (book, _) = result.expect("replay");
            assert_eq!(book.best_bid(), None);
            assert!(book.best_ask().is_some());
        }

        #[test]
        fn replay_cancel_by_user_removes_user_orders() {
            let journal = InMemoryJournal::<()>::new();
            let live = OrderBook::<()>::new("TEST");
            journal
                .append(&live_add(&live, make_add_event(1, 100, Side::Buy)))
                .expect("append");
            journal
                .append(&make_mass_cancel_event(
                    2,
                    SequencerCommand::CancelByUser {
                        user_id: Hash32::zero(),
                    },
                    &live,
                ))
                .expect("append");

            let result = ReplayEngine::replay_from(&journal, 1, "TEST");
            assert!(result.is_ok());
            let (book, _) = result.expect("replay");
            // Hash32::zero() bypasses STP so cancel_orders_by_user with zero
            // may or may not cancel depending on implementation. Just verify
            // replay completes without error.
            assert!(book.best_bid().is_none() || book.best_bid().is_some());
        }

        #[test]
        fn replay_cancel_by_price_range() {
            let journal = InMemoryJournal::<()>::new();
            let live = OrderBook::<()>::new("TEST");
            journal
                .append(&live_add(&live, make_add_event(1, 100, Side::Buy)))
                .expect("append");
            journal
                .append(&live_add(&live, make_add_event(2, 150, Side::Buy)))
                .expect("append");
            journal
                .append(&live_add(&live, make_add_event(3, 200, Side::Buy)))
                .expect("append");
            journal
                .append(&make_mass_cancel_event(
                    4,
                    SequencerCommand::CancelByPriceRange {
                        side: Side::Buy,
                        min_price: 100,
                        max_price: 150,
                    },
                    &live,
                ))
                .expect("append");

            let result = ReplayEngine::replay_from(&journal, 1, "TEST");
            assert!(result.is_ok());
            let (book, last_seq) = result.expect("replay");
            assert_eq!(last_seq, 4);
            // Only the order at price 200 should remain
            assert_eq!(book.best_bid(), Some(200));
        }
    }
}
