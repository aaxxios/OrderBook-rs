//! `TradePrint` outbound message.
//!
//! See `doc/wire-protocol.md` for the canonical layout.

use crate::wire::bytes::{read_i64_le, read_u64_le, reserve_payload};
use crate::wire::error::WireError;

/// Fixed payload size in bytes for a `TradePrintWire`.
pub const TRADE_PRINT_SIZE: usize = 48;

/// Outbound `TradePrint` message body.
///
/// Total payload size: **48 bytes**.
///
/// | Offset | Size | Field         | Type | Notes                        |
/// |-------:|-----:|---------------|------|------------------------------|
/// |      0 |    8 | `engine_seq`  | u64  | global engine sequence       |
/// |      8 |    8 | `maker_id`    | u64  | maker order id               |
/// |     16 |    8 | `taker_id`    | u64  | taker order id               |
/// |     24 |    8 | `price`       | i64  | tick-scaled fill price       |
/// |     32 |    8 | `qty`         | u64  | matched quantity             |
/// |     40 |    8 | `ts`          | u64  | engine timestamp (ms)        |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradePrintWire {
    /// Global engine sequence (monotonic across outbound streams).
    pub engine_seq: u64,
    /// Maker order id (the resting side of the match).
    pub maker_id: u64,
    /// Taker order id (the incoming side of the match).
    pub taker_id: u64,
    /// Tick-scaled fill price.
    pub price: i64,
    /// Matched quantity.
    pub qty: u64,
    /// Engine timestamp in milliseconds.
    pub ts: u64,
}

/// Appends a `TradePrint` payload (48 bytes) to `out`.
///
/// Room for the whole payload is reserved up front with
/// [`Vec::try_reserve`], so the appends that follow never reallocate and the
/// encoder never hits `Vec`'s capacity-overflow panic.
///
/// # Errors
///
/// Returns [`WireError::CapacityOverflow`] when `out` cannot grow by
/// [`TRADE_PRINT_SIZE`] bytes. `out` is left unchanged in that case.
#[inline]
pub fn encode_trade_print(trade: &TradePrintWire, out: &mut Vec<u8>) -> Result<(), WireError> {
    reserve_payload(out, TRADE_PRINT_SIZE)?;
    out.extend_from_slice(&trade.engine_seq.to_le_bytes());
    out.extend_from_slice(&trade.maker_id.to_le_bytes());
    out.extend_from_slice(&trade.taker_id.to_le_bytes());
    out.extend_from_slice(&trade.price.to_le_bytes());
    out.extend_from_slice(&trade.qty.to_le_bytes());
    out.extend_from_slice(&trade.ts.to_le_bytes());
    Ok(())
}

/// Decodes a `TradePrint` payload.
///
/// # Errors
///
/// Returns [`WireError::InvalidPayload`] when the buffer length differs from
/// [`TRADE_PRINT_SIZE`].
#[inline]
#[must_use = "the decoded value (or error) must be handled"]
pub fn decode_trade_print(payload: &[u8]) -> Result<TradePrintWire, WireError> {
    if payload.len() != TRADE_PRINT_SIZE {
        return Err(WireError::InvalidPayload(
            "TradePrint: payload size mismatch",
        ));
    }
    Ok(TradePrintWire {
        engine_seq: read_u64_le(payload, 0)?,
        maker_id: read_u64_le(payload, 8)?,
        taker_id: read_u64_le(payload, 16)?,
        price: read_i64_le(payload, 24)?,
        qty: read_u64_le(payload, 32)?,
        ts: read_u64_le(payload, 40)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::framing::{decode_frame, encode_frame};
    use proptest::prelude::*;

    #[test]
    fn payload_size_constant() {
        let trade = TradePrintWire {
            engine_seq: 0,
            maker_id: 0,
            taker_id: 0,
            price: 0,
            qty: 0,
            ts: 0,
        };
        let mut buf = Vec::new();
        encode_trade_print(&trade, &mut buf).expect("encode_trade_print");
        assert_eq!(buf.len(), TRADE_PRINT_SIZE);
    }

    proptest! {
        #[test]
        fn roundtrip_through_frame(
            engine_seq in any::<u64>(),
            maker_id in any::<u64>(),
            taker_id in any::<u64>(),
            price in any::<i64>(),
            qty in any::<u64>(),
            ts in any::<u64>(),
        ) {
            let original = TradePrintWire {
                engine_seq,
                maker_id,
                taker_id,
                price,
                qty,
                ts,
            };
            let mut payload = Vec::new();
            encode_trade_print(&original, &mut payload).expect("encode_trade_print");
            let mut framed = Vec::new();
            encode_frame(0x82, &payload, &mut framed).expect("encode_frame");

            let (kind, decoded_payload, _) = decode_frame(&framed).expect("decode_frame");
            prop_assert_eq!(kind, 0x82u8);
            let decoded = decode_trade_print(decoded_payload).expect("decode_trade_print");
            prop_assert_eq!(decoded, original);
        }
    }

    #[test]
    fn rejects_wrong_size() {
        let buf = [0u8; TRADE_PRINT_SIZE - 1];
        assert!(matches!(
            decode_trade_print(&buf),
            Err(WireError::InvalidPayload(_))
        ));
    }

    #[test]
    fn encode_at_capacity_edge_appends_without_disturbing_prefix() {
        let msg = TradePrintWire {
            engine_seq: 7,
            maker_id: 1,
            taker_id: 2,
            price: -5,
            qty: 9,
            ts: 1_700_000_000_000,
        };
        // Buffer already full (len == capacity): the encoder must grow it
        // through `try_reserve` and append after the existing prefix.
        let mut full = Vec::with_capacity(3);
        full.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        assert_eq!(full.len(), full.capacity());
        encode_trade_print(&msg, &mut full).expect("encode into full buffer");
        assert_eq!(full.get(..3), Some(&[0xAA, 0xBB, 0xCC][..]));
        let tail = full.get(3..).expect("appended payload");
        assert_eq!(tail.len(), TRADE_PRINT_SIZE);
        assert_eq!(decode_trade_print(tail), Ok(msg));

        // Exactly enough spare capacity: no reallocation is needed and the
        // capacity is unchanged afterwards.
        let mut exact = Vec::with_capacity(TRADE_PRINT_SIZE);
        let cap = exact.capacity();
        encode_trade_print(&msg, &mut exact).expect("encode into exact buffer");
        assert_eq!(exact.len(), TRADE_PRINT_SIZE);
        assert_eq!(exact.capacity(), cap);
    }

    #[test]
    fn rejects_empty_and_oversized_payloads() {
        assert!(matches!(
            decode_trade_print(&[]),
            Err(WireError::InvalidPayload(_))
        ));
        let long = [0u8; TRADE_PRINT_SIZE + 1];
        assert!(matches!(
            decode_trade_print(&long),
            Err(WireError::InvalidPayload(_))
        ));
    }
}
