//! `BookUpdate` outbound message.
//!
//! See `doc/wire-protocol.md` for the canonical layout.

use crate::wire::bytes::{read_i64_le, read_u8, read_u64_le, reserve_payload};
use crate::wire::error::WireError;

/// Wire codes for the `side` field.
pub const SIDE_BUY: u8 = 0;
/// Wire codes for the `side` field.
pub const SIDE_SELL: u8 = 1;

/// Fixed payload size in bytes for a `BookUpdateWire` (with trailing pad).
pub const BOOK_UPDATE_SIZE: usize = 32;

/// Outbound `BookUpdate` message body.
///
/// Total payload size: **32 bytes** (25 bytes of fields + 7 bytes of trailing
/// pad to round to a 32-byte block — keeps the message a comfortable
/// cache-line slice and leaves room for forward-compatible additions).
///
/// | Offset | Size | Field        | Type | Notes                       |
/// |-------:|-----:|--------------|------|-----------------------------|
/// |      0 |    8 | `engine_seq` | u64  | global engine sequence      |
/// |      8 |    1 | `side`       | u8   | `0` Buy, `1` Sell           |
/// |      9 |    8 | `price`      | i64  | tick-scaled level price     |
/// |     17 |    8 | `qty`        | u64  | new total quantity at level |
/// |     25 |    7 | `_pad`       | u8×7 | reserved, must be zero      |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookUpdateWire {
    /// Global engine sequence (monotonic across outbound streams).
    pub engine_seq: u64,
    /// Side of the level: `0` = Buy, `1` = Sell.
    pub side: u8,
    /// Tick-scaled level price.
    pub price: i64,
    /// New total quantity resting at this level (`0` if the level was wiped).
    pub qty: u64,
}

/// Appends a `BookUpdate` payload (32 bytes) to `out`. The trailing 7-byte
/// pad is zero-filled.
///
/// Room for the whole payload is reserved up front with
/// [`Vec::try_reserve`], so the appends that follow never reallocate and the
/// encoder never hits `Vec`'s capacity-overflow panic.
///
/// # Errors
///
/// Returns [`WireError::CapacityOverflow`] when `out` cannot grow by
/// [`BOOK_UPDATE_SIZE`] bytes. `out` is left unchanged in that case.
#[inline]
pub fn encode_book_update(update: &BookUpdateWire, out: &mut Vec<u8>) -> Result<(), WireError> {
    reserve_payload(out, BOOK_UPDATE_SIZE)?;
    out.extend_from_slice(&update.engine_seq.to_le_bytes());
    out.push(update.side);
    out.extend_from_slice(&update.price.to_le_bytes());
    out.extend_from_slice(&update.qty.to_le_bytes());
    // 7 bytes of trailing pad to round to 32.
    out.extend_from_slice(&[0u8; 7]);
    Ok(())
}

/// Decodes a `BookUpdate` payload.
///
/// # Errors
///
/// Returns [`WireError::InvalidPayload`] when the buffer length differs from
/// [`BOOK_UPDATE_SIZE`].
#[inline]
#[must_use = "the decoded value (or error) must be handled"]
pub fn decode_book_update(payload: &[u8]) -> Result<BookUpdateWire, WireError> {
    if payload.len() != BOOK_UPDATE_SIZE {
        return Err(WireError::InvalidPayload(
            "BookUpdate: payload size mismatch",
        ));
    }
    let engine_seq = read_u64_le(payload, 0)?;
    let side = read_u8(payload, 8)?;
    if side != SIDE_BUY && side != SIDE_SELL {
        return Err(WireError::InvalidPayload("BookUpdate: unknown side"));
    }
    let price = read_i64_le(payload, 9)?;
    let qty = read_u64_le(payload, 17)?;
    let pad = payload.get(25..32).ok_or(WireError::Truncated)?;
    if pad.iter().any(|&byte| byte != 0) {
        return Err(WireError::InvalidPayload(
            "BookUpdate: non-zero reserved padding",
        ));
    }
    Ok(BookUpdateWire {
        engine_seq,
        side,
        price,
        qty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::framing::{decode_frame, encode_frame};
    use proptest::prelude::*;

    #[test]
    fn payload_size_constant() {
        let upd = BookUpdateWire {
            engine_seq: 0,
            side: SIDE_BUY,
            price: 0,
            qty: 0,
        };
        let mut buf = Vec::new();
        encode_book_update(&upd, &mut buf).expect("encode_book_update");
        assert_eq!(buf.len(), BOOK_UPDATE_SIZE);
    }

    proptest! {
        #[test]
        fn roundtrip_through_frame(
            engine_seq in any::<u64>(),
            side in 0u8..=1u8,
            price in any::<i64>(),
            qty in any::<u64>(),
        ) {
            let original = BookUpdateWire {
                engine_seq,
                side,
                price,
                qty,
            };
            let mut payload = Vec::new();
            encode_book_update(&original, &mut payload).expect("encode_book_update");
            let mut framed = Vec::new();
            encode_frame(0x83, &payload, &mut framed).expect("encode_frame");

            let (kind, decoded_payload, _) = decode_frame(&framed).expect("decode_frame");
            prop_assert_eq!(kind, 0x83u8);
            let decoded = decode_book_update(decoded_payload).expect("decode_book_update");
            prop_assert_eq!(decoded, original);
        }
    }

    #[test]
    fn rejects_wrong_size() {
        let buf = [0u8; BOOK_UPDATE_SIZE - 1];
        assert!(matches!(
            decode_book_update(&buf),
            Err(WireError::InvalidPayload(_))
        ));
    }

    #[test]
    fn encode_at_capacity_edge_appends_without_disturbing_prefix() {
        let msg = BookUpdateWire {
            engine_seq: 7,
            side: SIDE_SELL,
            price: -5,
            qty: 9,
        };
        // Buffer already full (len == capacity): the encoder must grow it
        // through `try_reserve` and append after the existing prefix.
        let mut full = Vec::with_capacity(3);
        full.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        assert_eq!(full.len(), full.capacity());
        encode_book_update(&msg, &mut full).expect("encode into full buffer");
        assert_eq!(full.get(..3), Some(&[0xAA, 0xBB, 0xCC][..]));
        let tail = full.get(3..).expect("appended payload");
        assert_eq!(tail.len(), BOOK_UPDATE_SIZE);
        assert_eq!(decode_book_update(tail), Ok(msg));

        // Exactly enough spare capacity: no reallocation is needed and the
        // capacity is unchanged afterwards.
        let mut exact = Vec::with_capacity(BOOK_UPDATE_SIZE);
        let cap = exact.capacity();
        encode_book_update(&msg, &mut exact).expect("encode into exact buffer");
        assert_eq!(exact.len(), BOOK_UPDATE_SIZE);
        assert_eq!(exact.capacity(), cap);
    }

    #[test]
    fn rejects_empty_and_oversized_payloads() {
        assert!(matches!(
            decode_book_update(&[]),
            Err(WireError::InvalidPayload(_))
        ));
        let long = [0u8; BOOK_UPDATE_SIZE + 1];
        assert!(matches!(
            decode_book_update(&long),
            Err(WireError::InvalidPayload(_))
        ));
    }
}
