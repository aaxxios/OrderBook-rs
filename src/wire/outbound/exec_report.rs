//! `ExecReport` outbound message.
//!
//! Outbound encoders use an explicit byte-cursor (`Vec<u8>::extend_from_slice`)
//! rather than `#[repr(C, packed)]`. This is I/O-dominated traffic — the cost
//! of a few dozen bytes of explicit copying is dwarfed by socket overhead, and
//! we get freedom to evolve the layout without exposing a packed type.
//!
//! See `doc/wire-protocol.md` for the canonical layout.

use crate::orderbook::order_state::OrderStatus;
use crate::wire::bytes::{read_i64_le, read_u8, read_u16_le, read_u64_le, reserve_payload};
use crate::wire::error::WireError;

/// Wire code for `OrderStatus::Open`.
pub const STATUS_OPEN: u8 = 0;
/// Wire code for `OrderStatus::PartiallyFilled`.
pub const STATUS_PARTIALLY_FILLED: u8 = 1;
/// Wire code for `OrderStatus::Filled`.
pub const STATUS_FILLED: u8 = 2;
/// Wire code for `OrderStatus::Cancelled`.
pub const STATUS_CANCELLED: u8 = 3;
/// Wire code for `OrderStatus::Rejected`.
pub const STATUS_REJECTED: u8 = 4;
/// Wire code for `OrderStatus::Triggered` (#286): a pending trailing stop
/// was elected and runs as a market order. Decoders before 0.14 reject it.
pub const STATUS_TRIGGERED: u8 = 5;

/// Highest valid `STATUS_*` code.
const STATUS_MAX: u8 = STATUS_TRIGGERED;

/// Fixed payload size in bytes for an `ExecReport`.
pub const EXEC_REPORT_SIZE: usize = 44;

/// Outbound `ExecReport` message body.
///
/// Total payload size: **44 bytes**.
///
/// | Offset | Size | Field            | Type | Notes                            |
/// |-------:|-----:|------------------|------|----------------------------------|
/// |      0 |    8 | `engine_seq`     | u64  | global engine sequence           |
/// |      8 |    8 | `order_id`       | u64  | order id                         |
/// |     16 |    1 | `status`         | u8   | see `STATUS_*` constants          |
/// |     17 |    8 | `filled_qty`     | u64  | cumulative filled quantity       |
/// |     25 |    8 | `remaining_qty`  | u64  | quantity still resting           |
/// |     33 |    8 | `price`          | i64  | tick-scaled price                |
/// |     41 |    2 | `reject_reason`  | u16  | reject code, `0` if not rejected |
/// |     43 |    1 | `_pad`           | u8   | reserved, must be zero           |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecReport {
    /// Global engine sequence (monotonic across outbound streams).
    pub engine_seq: u64,
    /// Order id.
    pub order_id: u64,
    /// Status discriminant — see `STATUS_*` constants.
    pub status: u8,
    /// Cumulative filled quantity for this order.
    pub filled_qty: u64,
    /// Quantity still resting on the book.
    pub remaining_qty: u64,
    /// Tick-scaled price.
    pub price: i64,
    /// Numeric reject code. `0` when the report is not a rejection.
    pub reject_reason: u16,
    /// Reserved. Must be zero.
    pub _pad: u8,
}

/// Maps an [`OrderStatus`] to its wire-side discriminant.
///
/// The mapping is stable across `0.7.x` patch releases.
#[must_use]
#[inline]
pub fn status_to_wire(status: &OrderStatus) -> u8 {
    match status {
        OrderStatus::Open => STATUS_OPEN,
        OrderStatus::PartiallyFilled { .. } => STATUS_PARTIALLY_FILLED,
        OrderStatus::Filled { .. } => STATUS_FILLED,
        OrderStatus::Cancelled { .. } => STATUS_CANCELLED,
        OrderStatus::Rejected { .. } => STATUS_REJECTED,
        OrderStatus::Triggered { .. } => STATUS_TRIGGERED,
    }
}

/// Appends an `ExecReport` payload (44 bytes) to `out`.
///
/// Room for the whole payload is reserved up front with
/// [`Vec::try_reserve`], so the appends that follow never reallocate and the
/// encoder never hits `Vec`'s capacity-overflow panic.
///
/// # Errors
///
/// Returns [`WireError::InvalidPayload`] when `status` is not one of the
/// `STATUS_*` codes or `_pad` is non-zero: [`decode_exec_report`] rejects
/// both, so the encoder never emits a frame its own decoder refuses (#295).
/// Returns [`WireError::CapacityOverflow`] when `out` cannot grow by
/// [`EXEC_REPORT_SIZE`] bytes. `out` is left unchanged in either case.
#[inline]
pub fn encode_exec_report(report: &ExecReport, out: &mut Vec<u8>) -> Result<(), WireError> {
    if report.status > STATUS_MAX {
        return Err(WireError::InvalidPayload("ExecReport: unknown status"));
    }
    if report._pad != 0 {
        return Err(WireError::InvalidPayload(
            "ExecReport: non-zero reserved padding",
        ));
    }
    reserve_payload(out, EXEC_REPORT_SIZE)?;
    out.extend_from_slice(&report.engine_seq.to_le_bytes());
    out.extend_from_slice(&report.order_id.to_le_bytes());
    out.push(report.status);
    out.extend_from_slice(&report.filled_qty.to_le_bytes());
    out.extend_from_slice(&report.remaining_qty.to_le_bytes());
    out.extend_from_slice(&report.price.to_le_bytes());
    out.extend_from_slice(&report.reject_reason.to_le_bytes());
    out.push(report._pad);
    Ok(())
}

/// Decodes an `ExecReport` payload.
///
/// # Errors
///
/// Returns [`WireError::InvalidPayload`] when the buffer length differs from
/// [`EXEC_REPORT_SIZE`].
#[inline]
#[must_use = "the decoded value (or error) must be handled"]
pub fn decode_exec_report(payload: &[u8]) -> Result<ExecReport, WireError> {
    if payload.len() != EXEC_REPORT_SIZE {
        return Err(WireError::InvalidPayload(
            "ExecReport: payload size mismatch",
        ));
    }
    let engine_seq = read_u64_le(payload, 0)?;
    let order_id = read_u64_le(payload, 8)?;
    let status = read_u8(payload, 16)?;
    if status > STATUS_MAX {
        return Err(WireError::InvalidPayload("ExecReport: unknown status"));
    }
    let filled_qty = read_u64_le(payload, 17)?;
    let remaining_qty = read_u64_le(payload, 25)?;
    let price = read_i64_le(payload, 33)?;
    let reject_reason = read_u16_le(payload, 41)?;
    let pad = read_u8(payload, 43)?;
    if pad != 0 {
        return Err(WireError::InvalidPayload(
            "ExecReport: non-zero reserved padding",
        ));
    }
    Ok(ExecReport {
        engine_seq,
        order_id,
        status,
        filled_qty,
        remaining_qty,
        price,
        reject_reason,
        _pad: pad,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orderbook::reject_reason::RejectReason;
    use crate::wire::framing::{decode_frame, encode_frame};
    use proptest::prelude::*;

    #[test]
    fn payload_size_constant() {
        let report = ExecReport {
            engine_seq: 0,
            order_id: 0,
            status: STATUS_OPEN,
            filled_qty: 0,
            remaining_qty: 0,
            price: 0,
            reject_reason: 0,
            _pad: 0,
        };
        let mut buf = Vec::new();
        encode_exec_report(&report, &mut buf).expect("encode_exec_report");
        assert_eq!(buf.len(), EXEC_REPORT_SIZE);
    }

    #[test]
    fn status_to_wire_covers_all_variants() {
        assert_eq!(status_to_wire(&OrderStatus::Open), STATUS_OPEN);
        assert_eq!(
            status_to_wire(&OrderStatus::PartiallyFilled {
                original_quantity: 10,
                filled_quantity: 4
            }),
            STATUS_PARTIALLY_FILLED
        );
        assert_eq!(
            status_to_wire(&OrderStatus::Filled {
                filled_quantity: 10
            }),
            STATUS_FILLED
        );
        assert_eq!(
            status_to_wire(&OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: crate::orderbook::order_state::CancelReason::UserRequested,
            }),
            STATUS_CANCELLED
        );
        assert_eq!(
            status_to_wire(&OrderStatus::Rejected {
                reason: RejectReason::KillSwitchActive
            }),
            STATUS_REJECTED
        );
        assert_eq!(
            status_to_wire(&OrderStatus::Triggered {
                child_id: pricelevel::Id::from_u64(1),
                trigger_price: 100,
            }),
            STATUS_TRIGGERED
        );
    }

    proptest! {
        #[test]
        fn roundtrip_through_frame(
            engine_seq in any::<u64>(),
            order_id in any::<u64>(),
            status in 0u8..=5u8,
            filled_qty in any::<u64>(),
            remaining_qty in any::<u64>(),
            price in any::<i64>(),
            reject_reason in any::<u16>(),
        ) {
            let original = ExecReport {
                engine_seq,
                order_id,
                status,
                filled_qty,
                remaining_qty,
                price,
                reject_reason,
                _pad: 0,
            };
            let mut payload = Vec::new();
            encode_exec_report(&original, &mut payload).expect("encode_exec_report");
            let mut framed = Vec::new();
            encode_frame(0x81, &payload, &mut framed).expect("encode_frame");

            let (kind, decoded_payload, _) = decode_frame(&framed).expect("decode_frame");
            prop_assert_eq!(kind, 0x81u8);
            let decoded = decode_exec_report(decoded_payload).expect("decode_exec_report");
            prop_assert_eq!(decoded, original);
        }
    }

    #[test]
    fn rejects_short_payload() {
        let buf = [0u8; EXEC_REPORT_SIZE - 1];
        assert!(matches!(
            decode_exec_report(&buf),
            Err(WireError::InvalidPayload(_))
        ));
    }

    #[test]
    fn encode_at_capacity_edge_appends_without_disturbing_prefix() {
        let msg = ExecReport {
            engine_seq: 7,
            order_id: 42,
            status: STATUS_PARTIALLY_FILLED,
            filled_qty: 3,
            remaining_qty: 9,
            price: -5,
            reject_reason: 0,
            _pad: 0,
        };
        // Buffer already full (len == capacity): the encoder must grow it
        // through `try_reserve` and append after the existing prefix.
        let mut full = Vec::with_capacity(3);
        full.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        assert_eq!(full.len(), full.capacity());
        encode_exec_report(&msg, &mut full).expect("encode into full buffer");
        assert_eq!(full.get(..3), Some(&[0xAA, 0xBB, 0xCC][..]));
        let tail = full.get(3..).expect("appended payload");
        assert_eq!(tail.len(), EXEC_REPORT_SIZE);
        assert_eq!(decode_exec_report(tail), Ok(msg));

        // Exactly enough spare capacity: no reallocation is needed and the
        // capacity is unchanged afterwards.
        let mut exact = Vec::with_capacity(EXEC_REPORT_SIZE);
        let cap = exact.capacity();
        encode_exec_report(&msg, &mut exact).expect("encode into exact buffer");
        assert_eq!(exact.len(), EXEC_REPORT_SIZE);
        assert_eq!(exact.capacity(), cap);
    }

    /// #295: the encoder refuses what the decoder would reject, leaving
    /// `out` unchanged.
    #[test]
    fn encoder_rejects_invalid_status_and_padding() {
        let valid = ExecReport {
            engine_seq: 1,
            order_id: 2,
            status: STATUS_REJECTED,
            filled_qty: 0,
            remaining_qty: 0,
            price: 0,
            reject_reason: 0,
            _pad: 0,
        };
        let mut out = vec![0xEE];
        for bad in [
            ExecReport {
                status: STATUS_MAX + 1,
                ..valid
            },
            ExecReport {
                status: u8::MAX,
                ..valid
            },
            ExecReport { _pad: 1, ..valid },
        ] {
            assert!(matches!(
                encode_exec_report(&bad, &mut out),
                Err(WireError::InvalidPayload(_))
            ));
            assert_eq!(out, vec![0xEE], "out unchanged");
        }
        encode_exec_report(&valid, &mut out).expect("valid report encodes");
    }

    #[test]
    fn rejects_empty_and_oversized_payloads() {
        assert!(matches!(
            decode_exec_report(&[]),
            Err(WireError::InvalidPayload(_))
        ));
        let long = [0u8; EXEC_REPORT_SIZE + 1];
        assert!(matches!(
            decode_exec_report(&long),
            Err(WireError::InvalidPayload(_))
        ));
    }
}
