//! Allocation bounds for bincode decoding of untrusted payloads (#251).
//!
//! Feature-gated on `alloc-counters` + `bincode`. Installs the counting
//! allocator and asserts that decoding hostile payloads, whose length
//! prefixes declare up to `u64::MAX` bytes / elements, returns a typed
//! [`SerializationError`] without allocating for the declared length:
//! string / byte-buffer prefixes are checked against the remaining input
//! before any allocation, and sequence reservations are clamped to the
//! payload length. Before #251 the same payloads asked the allocator for the
//! declared length (capacity-overflow panic or OOM abort).
//!
//! A single `#[test]` runs every scenario sequentially: the counters are
//! process-global, so parallel tests in this binary would pollute them.

// Integration-test crate root, not production (see tests/alloc_budget.rs
// for the same rationale under issue #242's panic-policy gate).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]
#![cfg(all(feature = "alloc-counters", feature = "bincode"))]

use orderbook_rs::utils::CountingAllocator;
use orderbook_rs::{BincodeEventSerializer, EventSerializer, SerializationError};
use pricelevel::{Id, MatchResult, Price, Quantity, Side, TimestampMs, Trade};
use std::alloc::System;
use uuid::Uuid;

#[global_allocator]
static GLOBAL: CountingAllocator<System> = CountingAllocator::new(System);

/// bincode `standard()` varint marker for "a u64 follows".
const VARINT_U64: u8 = 253;

/// Ceiling for a hostile `String` length prefix in a small payload: the
/// prefix is checked against the remaining input before the string buffer
/// is allocated, so only the valid strings before it are ever copied.
const STRING_ATTACK_CEILING: u64 = 1024;

/// Ceiling for a hostile sequence length prefix in a small payload: the
/// reservation is clamped to `payload_len * size_of::<T>()`.
const SEQ_ATTACK_CEILING: u64 = 64 * 1024;

/// 200 MiB: the declared length of the hostile prefixes in the
/// 8 MiB-class payload scenarios.
const DECLARED_200_MIB: u64 = 200 * 1024 * 1024;

/// ~8 MiB: an input just under `DEFAULT_MAX_BINCODE_PAYLOAD_BYTES`.
const LARGE_INPUT: usize = 8 * 1024 * 1024 - 1024;

fn huge_len() -> Vec<u8> {
    len_prefix(u64::MAX)
}

fn len_prefix(n: u64) -> Vec<u8> {
    let mut out = vec![VARINT_U64];
    out.extend_from_slice(&n.to_le_bytes());
    out
}

fn encode<V: serde::Serialize>(value: &V) -> Vec<u8> {
    bincode::serde::encode_to_vec(value, bincode::config::standard()).expect("encode fixture")
}

/// Bytes allocated while decoding `payload` as a trade, plus the result.
fn measure_trade(payload: &[u8]) -> (u64, Result<(), SerializationError>) {
    let serializer = BincodeEventSerializer::new();
    let before = GLOBAL.snapshot();
    let result = serializer.deserialize_trade(payload).map(|_| ());
    let delta = GLOBAL.snapshot().since(before);
    (delta.bytes_allocated, result)
}

/// Encoding of a [`TradeResult`] prefix up to (excluding) the
/// `filled_order_ids` length, holding as many real trades as fit in about
/// `target - 64 KiB` bytes. Returns the prefix and the full valid encoding
/// of the same result (no filled ids), which starts with that prefix.
fn large_valid_prefix(target: usize) -> (Vec<u8>, Vec<u8>) {
    // One bincode trade is ~120 bytes; stop well below `target`.
    let fills = (target - 64 * 1024) / 128;
    let taker = Id::from_uuid(Uuid::new_v4());
    let mut mr = MatchResult::new(taker, Quantity::new(fills as u64 + 1));
    for _ in 0..fills {
        let trade = Trade::with_timestamp(
            Id::from_uuid(Uuid::new_v4()),
            taker,
            Id::from_uuid(Uuid::new_v4()),
            Price::new(50_000),
            Quantity::new(1),
            Side::Buy,
            TimestampMs::new(1_700_000_000_000),
        );
        mr.add_trade(trade).expect("valid fill");
    }
    let bytes = encode(&(
        "BTC/USD".to_string(),
        taker.to_string(),
        mr.trades(),
        mr.remaining_quantity().as_u64(),
        mr.is_complete(),
    ));
    assert!(bytes.len() < target, "prefix is {} bytes", bytes.len());
    // Guard the fixture: a real encoding of the same result (no filled ids)
    // starts with these bytes, followed by a zero `filled_order_ids` length.
    let full = BincodeEventSerializer::new()
        .serialize_trade(&orderbook_rs::TradeResult::new("BTC/USD".to_string(), mr))
        .expect("encode full trade");
    assert!(
        full.starts_with(&bytes),
        "prefix fixture diverged from the real layout"
    );
    assert_eq!(full.get(bytes.len()), Some(&0u8));
    (bytes, full)
}

#[test]
fn hostile_length_prefixes_allocate_a_bounded_amount() {
    let order_id = Id::from_uuid(Uuid::new_v4()).to_string();

    // 1. `symbol` declares u64::MAX bytes.
    let (bytes, result) = measure_trade(&huge_len());
    assert!(
        matches!(result, Err(SerializationError::Truncated { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= STRING_ATTACK_CEILING,
        "symbol attack allocated {bytes} bytes"
    );

    // 2. `symbol` declares 4000 bytes the input does not hold: rejected
    //    before allocating anything for it.
    let mut payload = vec![VARINT_U64];
    payload.extend_from_slice(&4000u64.to_le_bytes());
    let (bytes, result) = measure_trade(&payload);
    assert!(
        matches!(result, Err(SerializationError::Truncated { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= STRING_ATTACK_CEILING,
        "short-prefix attack allocated {bytes} bytes"
    );

    // 3. `match_result.order_id` declares u64::MAX bytes.
    let mut payload = encode(&"BTC/USD".to_string());
    payload.extend_from_slice(&huge_len());
    let (bytes, result) = measure_trade(&payload);
    assert!(
        matches!(result, Err(SerializationError::Truncated { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= STRING_ATTACK_CEILING,
        "order_id attack allocated {bytes} bytes"
    );

    // 4. `trades` declares u64::MAX elements.
    let mut payload = encode(&("BTC/USD".to_string(), order_id.clone()));
    payload.extend_from_slice(&huge_len());
    let (bytes, result) = measure_trade(&payload);
    assert!(result.is_err(), "{result:?}");
    assert!(
        bytes <= SEQ_ATTACK_CEILING,
        "trade-list attack allocated {bytes} bytes"
    );

    // 5. One trade whose `trade_id` declares u64::MAX bytes.
    let mut payload = encode(&("BTC/USD".to_string(), order_id.clone()));
    payload.push(1);
    payload.extend_from_slice(&huge_len());
    let (bytes, result) = measure_trade(&payload);
    assert!(
        matches!(result, Err(SerializationError::Truncated { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= SEQ_ATTACK_CEILING,
        "nested trade_id attack allocated {bytes} bytes"
    );

    // 6. `filled_order_ids` declares u64::MAX elements.
    let prefix = encode(&("BTC/USD".to_string(), order_id.clone(), 0u64, 100u64, false));
    let mut payload = prefix.clone();
    payload.extend_from_slice(&huge_len());
    let (bytes, result) = measure_trade(&payload);
    assert!(result.is_err(), "{result:?}");
    assert!(
        bytes <= SEQ_ATTACK_CEILING,
        "filled-ids attack allocated {bytes} bytes"
    );

    // 7. One filled id declaring u64::MAX bytes.
    let mut payload = prefix;
    payload.push(1);
    payload.extend_from_slice(&huge_len());
    let (bytes, result) = measure_trade(&payload);
    assert!(
        matches!(result, Err(SerializationError::Truncated { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= SEQ_ATTACK_CEILING,
        "nested filled-id attack allocated {bytes} bytes"
    );

    // 8. 200 MiB `symbol` prefix at the head of an ~8 MiB payload (under
    //    the 8 MiB default limit): nothing is allocated for the string.
    let mut payload = len_prefix(DECLARED_200_MIB);
    payload.resize(LARGE_INPUT, 0xAB);
    let input = payload.len() as u64;
    let (bytes, result) = measure_trade(&payload);
    assert!(
        matches!(result, Err(SerializationError::Truncated { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= STRING_ATTACK_CEILING,
        "200 MiB symbol prefix in a {input}-byte payload allocated {bytes} bytes"
    );

    // 9. 200 MiB prefix on a filled id AFTER ~8 MiB of valid trades. The
    //    valid part decodes (allocating for what it really holds); the
    //    hostile string allocates nothing on top. Baseline: decoding the
    //    same result with a zero filled-id count, which succeeds.
    let (valid_prefix, full) = large_valid_prefix(LARGE_INPUT);
    let (baseline, result) = measure_trade(&full);
    assert!(result.is_ok(), "{result:?}");
    let mut payload = valid_prefix;
    payload.push(1); // one filled id…
    payload.extend_from_slice(&len_prefix(DECLARED_200_MIB)); // …declaring 200 MiB
    payload.resize(LARGE_INPUT, 0xAB);
    let input = payload.len() as u64;
    let (bytes, result) = measure_trade(&payload);
    assert!(
        matches!(result, Err(SerializationError::Truncated { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= baseline + STRING_ATTACK_CEILING,
        "200 MiB filled-id prefix in a {input}-byte payload allocated {bytes} bytes \
         vs {baseline} for the valid decode"
    );

    // 10. Book change: no containers; a hostile varint is still typed.
    let serializer = BincodeEventSerializer::new();
    let before = GLOBAL.snapshot();
    let result = serializer.deserialize_book_change(&huge_len());
    let bytes = GLOBAL.snapshot().since(before).bytes_allocated;
    assert!(result.is_err());
    assert!(
        bytes <= STRING_ATTACK_CEILING,
        "book-change attack allocated {bytes} bytes"
    );
}
