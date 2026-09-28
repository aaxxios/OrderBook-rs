//! Allocation bounds for bincode decoding of untrusted payloads (#251).
//!
//! Feature-gated on `alloc-counters` + `bincode`. Installs the counting
//! allocator and asserts that decoding hostile payloads, whose length
//! prefixes declare up to `u64::MAX` bytes / elements, returns a typed
//! [`SerializationError`] after allocating only a small, input-independent
//! amount. Before #251 the same payloads asked the allocator for the
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
use pricelevel::Id;
use std::alloc::System;
use uuid::Uuid;

#[global_allocator]
static GLOBAL: CountingAllocator<System> = CountingAllocator::new(System);

/// bincode `standard()` varint marker for "a u64 follows".
const VARINT_U64: u8 = 253;

/// Ceiling for a hostile `String` length prefix: rejected by the decode
/// limit before the string buffer is allocated.
const STRING_ATTACK_CEILING: u64 = 8 * 1024;

/// Ceiling for a hostile sequence length prefix: serde's `Vec` visitor caps
/// its up-front reservation at 1 MiB whatever the declared length.
const SEQ_ATTACK_CEILING: u64 = 1024 * 1024 + 64 * 1024;

fn huge_len() -> Vec<u8> {
    let mut out = vec![VARINT_U64];
    out.extend_from_slice(&u64::MAX.to_le_bytes());
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

#[test]
fn hostile_length_prefixes_allocate_a_bounded_amount() {
    let order_id = Id::from_uuid(Uuid::new_v4()).to_string();

    // 1. `symbol` declares u64::MAX bytes.
    let (bytes, result) = measure_trade(&huge_len());
    assert!(
        matches!(result, Err(SerializationError::DecodeLimitExceeded { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= STRING_ATTACK_CEILING,
        "symbol attack allocated {bytes} bytes"
    );

    // 2. `symbol` declares just under the 4 KiB tier: accepted by the limit,
    //    allocated, then truncated. Still bounded by the tier.
    let mut payload = vec![VARINT_U64];
    payload.extend_from_slice(&4000u64.to_le_bytes());
    let (bytes, result) = measure_trade(&payload);
    assert!(
        matches!(result, Err(SerializationError::Truncated { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= STRING_ATTACK_CEILING,
        "in-tier attack allocated {bytes} bytes"
    );

    // 3. `match_result.order_id` declares u64::MAX bytes.
    let mut payload = encode(&"BTC/USD".to_string());
    payload.extend_from_slice(&huge_len());
    let (bytes, result) = measure_trade(&payload);
    assert!(
        matches!(result, Err(SerializationError::DecodeLimitExceeded { .. })),
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
        matches!(result, Err(SerializationError::DecodeLimitExceeded { .. })),
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
        matches!(result, Err(SerializationError::DecodeLimitExceeded { .. })),
        "{result:?}"
    );
    assert!(
        bytes <= SEQ_ATTACK_CEILING,
        "nested filled-id attack allocated {bytes} bytes"
    );

    // 8. Book change: no containers; a hostile varint is still typed.
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
