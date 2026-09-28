//! `NewOrder` inbound message.
//!
//! See `doc/wire-protocol.md` for the canonical layout.

use crate::wire::error::WireError;
use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Wire codes for the `side` field of [`NewOrderWire`].
pub const SIDE_BUY: u8 = 0;
/// Wire codes for the `side` field of [`NewOrderWire`].
pub const SIDE_SELL: u8 = 1;

/// Wire codes for the `time_in_force` field.
pub const TIF_GTC: u8 = 0;
/// Wire codes for the `time_in_force` field.
pub const TIF_IOC: u8 = 1;
/// Wire codes for the `time_in_force` field.
pub const TIF_FOK: u8 = 2;
/// Wire codes for the `time_in_force` field.
pub const TIF_DAY: u8 = 3;

/// Wire codes for the `order_type` field.
pub const ORDER_TYPE_STANDARD: u8 = 0;

/// Inbound `NewOrder` packed wire layout.
///
/// All fields are little-endian primitives. Total size: **48 bytes**.
///
/// | Offset | Size | Field           | Type | Notes                          |
/// |-------:|-----:|-----------------|------|--------------------------------|
/// |      0 |    8 | `client_ts`     | u64  | client-side timestamp (ms)     |
/// |      8 |    8 | `order_id`      | u64  | unique order id                |
/// |     16 |    8 | `account_id`    | u64  | numeric account id             |
/// |     24 |    8 | `price`         | i64  | tick-scaled limit price        |
/// |     32 |    8 | `qty`           | u64  | quantity                       |
/// |     40 |    1 | `side`          | u8   | `0` Buy, `1` Sell              |
/// |     41 |    1 | `time_in_force` | u8   | `0` GTC, `1` IOC, `2` FOK, `3` DAY |
/// |     42 |    1 | `order_type`    | u8   | `0` Standard (only one in MVP) |
/// |     43 |    5 | `_pad`          | u8×5 | reserved, must be zero         |
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Unaligned, Immutable, KnownLayout,
)]
#[repr(C, packed)]
pub struct NewOrderWire {
    /// Client-supplied timestamp (milliseconds since the Unix epoch).
    pub client_ts: u64,
    /// Unique order identifier supplied by the client.
    pub order_id: u64,
    /// Numeric account identifier supplied by the client.
    pub account_id: u64,
    /// Tick-scaled limit price.
    pub price: i64,
    /// Order quantity.
    pub qty: u64,
    /// Side: `0` = Buy, `1` = Sell.
    pub side: u8,
    /// Time-in-force: `0` GTC / `1` IOC / `2` FOK / `3` DAY.
    pub time_in_force: u8,
    /// Order type: `0` Standard (only Standard is supported in the MVP).
    pub order_type: u8,
    /// Reserved padding. Must be zero.
    pub _pad: [u8; 5],
}

// Compile-time layout guard: a size drift is a type mismatch between
// `[(); 48]` and `[(); size_of::<NewOrderWire>()]`, rejected by the compiler. It is
// evaluated at compile time only and has no runtime (panicking) form.
const _: [(); 48] = [(); core::mem::size_of::<NewOrderWire>()];

impl NewOrderWire {
    /// Returns the packed byte representation of `self`.
    ///
    /// Equivalent to `<Self as zerocopy::IntoBytes>::as_bytes(&self)` but
    /// callable without importing `zerocopy` at the call site.
    #[must_use]
    #[inline]
    pub fn as_payload_bytes(&self) -> &[u8] {
        <Self as zerocopy::IntoBytes>::as_bytes(self)
    }
}

/// Decodes a `NewOrder` payload (48 bytes).
///
/// # Errors
///
/// Returns [`WireError::InvalidPayload`] when the buffer length differs from
/// 48 bytes.
#[inline]
#[must_use = "the decoded value (or error) must be handled"]
pub fn decode_new_order(payload: &[u8]) -> Result<NewOrderWire, WireError> {
    let view = NewOrderWire::ref_from_bytes(payload)
        .map_err(|_| WireError::InvalidPayload("NewOrder: payload size mismatch"))?;
    Ok(*view)
}

/// Converts a decoded [`NewOrderWire`] into a domain [`OrderType<()>`],
/// validating the reserved padding and the enumerated fields.
impl TryFrom<&NewOrderWire> for OrderType<()> {
    type Error = WireError;

    /// # Errors
    ///
    /// Returns [`WireError::InvalidPayload`] when the wire message is malformed:
    /// - non-zero reserved padding (`_pad`),
    /// - a negative `price`,
    /// - `price == 0` (the cache "no best price" sentinel; only `price > 0`
    ///   is admissible),
    /// - `qty == 0` (a zero-quantity order is structurally meaningless),
    /// - `account_id == 0` (reserved — it would collide with the
    ///   `Hash32::zero()` "no STP" sentinel),
    /// - an unknown `side` byte (not `SIDE_BUY` / `SIDE_SELL`),
    /// - an unknown `time_in_force` byte (not GTC / IOC / FOK / DAY), or
    /// - an unsupported `order_type` (only `ORDER_TYPE_STANDARD` is accepted).
    fn try_from(value: &NewOrderWire) -> Result<Self, Self::Error> {
        // Copy each packed field into a local first — taking a reference to a
        // packed field is undefined behavior. The `{ value.field }` syntax
        // forces a copy.
        let order_id = { value.order_id };
        let account_id = { value.account_id };
        let client_ts = { value.client_ts };
        let price_raw = { value.price };
        let qty = { value.qty };
        let side_byte = { value.side };
        let tif_byte = { value.time_in_force };
        let kind_byte = { value.order_type };
        let pad = { value._pad };

        if pad.iter().any(|&byte| byte != 0) {
            return Err(WireError::InvalidPayload(
                "NewOrder: non-zero reserved padding",
            ));
        }
        // A checked conversion (not an `as` cast) rejects every negative
        // price with a typed error.
        let price_ticks = u64::try_from(price_raw)
            .map_err(|_| WireError::InvalidPayload("NewOrder: negative price"))?;
        // Reject `price == 0` at the trust boundary: price 0 is the cache's
        // "no best price" sentinel and a zero-priced limit order is
        // structurally meaningless. Only `price > 0` is admissible.
        if price_ticks == 0 {
            return Err(WireError::InvalidPayload("NewOrder: zero price"));
        }
        // Reject `qty == 0`: a zero-quantity order is structurally meaningless
        // and otherwise slips through (the lot check passes for 0 and
        // `min_order_size` defaults to `None`), reaching the insert/match path
        // as a degenerate order. Reject it close to the source.
        if qty == 0 {
            return Err(WireError::InvalidPayload("NewOrder: zero quantity"));
        }
        // `account_id == 0` is reserved: it encodes to an all-zero `Hash32`,
        // which equals `Hash32::zero()` — the "no STP" sentinel — so an order
        // from numeric account 0 would silently lose self-trade protection
        // (it could match its own resting orders and would never be grouped
        // with other account-0 orders for STP). Reject it at the trust boundary.
        if account_id == 0 {
            return Err(WireError::InvalidPayload("NewOrder: account_id 0 reserved"));
        }

        let side = match side_byte {
            SIDE_BUY => Side::Buy,
            SIDE_SELL => Side::Sell,
            _ => return Err(WireError::InvalidPayload("NewOrder: unknown side")),
        };
        let time_in_force = match tif_byte {
            TIF_GTC => TimeInForce::Gtc,
            TIF_IOC => TimeInForce::Ioc,
            TIF_FOK => TimeInForce::Fok,
            TIF_DAY => TimeInForce::Day,
            _ => {
                return Err(WireError::InvalidPayload("NewOrder: unknown time_in_force"));
            }
        };
        if kind_byte != ORDER_TYPE_STANDARD {
            return Err(WireError::InvalidPayload(
                "NewOrder: unsupported order_type",
            ));
        }

        // Encode the numeric account_id (guaranteed non-zero above) into the
        // low 8 bytes of a Hash32. With `account_id != 0` the result is never
        // all-zero, so it never collides with `Hash32::zero()` — the "no STP"
        // sentinel — and STP applies correctly to wire-sourced orders.
        let mut user_bytes = [0u8; 32];
        // `zip` stops at the shorter side (8 bytes): no length-matching
        // `copy_from_slice` precondition to violate.
        for (dst, src) in user_bytes.iter_mut().zip(account_id.to_le_bytes()) {
            *dst = src;
        }
        let user_id = Hash32::new(user_bytes);

        Ok(OrderType::Standard {
            id: Id::from_u64(order_id),
            price: Price::new(u128::from(price_ticks)),
            quantity: Quantity::new(qty),
            side,
            user_id,
            timestamp: TimestampMs::new(client_ts),
            time_in_force,
            extra_fields: (),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::framing::{decode_frame, encode_frame};
    use proptest::prelude::*;
    use zerocopy::IntoBytes;

    proptest! {
        #[test]
        fn roundtrip_through_frame(
            client_ts in any::<u64>(),
            order_id in any::<u64>(),
            account_id in any::<u64>(),
            price in 0i64..i64::MAX,
            qty in any::<u64>(),
            side in 0u8..=1u8,
            tif in 0u8..=3u8,
        ) {
            let original = NewOrderWire {
                client_ts,
                order_id,
                account_id,
                price,
                qty,
                side,
                time_in_force: tif,
                order_type: ORDER_TYPE_STANDARD,
                _pad: [0u8; 5],
            };

            let mut framed = Vec::new();
            encode_frame(0x01, original.as_bytes(), &mut framed).expect("encode_frame");

            let (kind, payload, used) = decode_frame(&framed).expect("decode_frame");
            prop_assert_eq!(kind, 0x01u8);
            prop_assert_eq!(used, framed.len());

            let decoded = decode_new_order(payload).expect("decode_new_order");
            // Read packed fields via copy.
            prop_assert_eq!({ decoded.client_ts }, client_ts);
            prop_assert_eq!({ decoded.order_id }, order_id);
            prop_assert_eq!({ decoded.account_id }, account_id);
            prop_assert_eq!({ decoded.price }, price);
            prop_assert_eq!({ decoded.qty }, qty);
            prop_assert_eq!({ decoded.side }, side);
            prop_assert_eq!({ decoded.time_in_force }, tif);
            prop_assert_eq!({ decoded.order_type }, ORDER_TYPE_STANDARD);
            prop_assert_eq!({ decoded._pad }, [0u8; 5]);
        }
    }

    #[test]
    fn rejects_short_payload() {
        let buf = [0u8; 47];
        assert!(matches!(
            decode_new_order(&buf),
            Err(WireError::InvalidPayload(_))
        ));
    }

    #[test]
    fn rejects_long_payload() {
        let buf = [0u8; 49];
        assert!(matches!(
            decode_new_order(&buf),
            Err(WireError::InvalidPayload(_))
        ));
    }

    #[test]
    fn try_from_rejects_unknown_side() {
        let wire = NewOrderWire {
            client_ts: 0,
            order_id: 1,
            account_id: 2,
            price: 100,
            qty: 5,
            side: 9,
            time_in_force: TIF_GTC,
            order_type: ORDER_TYPE_STANDARD,
            _pad: [0u8; 5],
        };
        let res: Result<OrderType<()>, _> = (&wire).try_into();
        assert!(matches!(res, Err(WireError::InvalidPayload(_))));
    }

    #[test]
    fn try_from_rejects_negative_price() {
        let wire = NewOrderWire {
            client_ts: 0,
            order_id: 1,
            account_id: 2,
            price: -1,
            qty: 5,
            side: SIDE_BUY,
            time_in_force: TIF_GTC,
            order_type: ORDER_TYPE_STANDARD,
            _pad: [0u8; 5],
        };
        let res: Result<OrderType<()>, _> = (&wire).try_into();
        assert!(matches!(res, Err(WireError::InvalidPayload(_))));
    }

    #[test]
    fn try_from_rejects_account_id_zero_issue_103() {
        // account_id 0 would encode to Hash32::zero() (the no-STP sentinel),
        // silently disabling self-trade prevention. Reject it at the boundary.
        let wire = NewOrderWire {
            client_ts: 0,
            order_id: 1,
            account_id: 0,
            price: 100,
            qty: 5,
            side: SIDE_BUY,
            time_in_force: TIF_GTC,
            order_type: ORDER_TYPE_STANDARD,
            _pad: [0u8; 5],
        };
        let res: Result<OrderType<()>, _> = (&wire).try_into();
        assert!(matches!(res, Err(WireError::InvalidPayload(_))));

        // A non-zero account converts and yields a non-zero (non-sentinel) user_id.
        let ok = NewOrderWire {
            account_id: 1,
            ..wire
        };
        let order: OrderType<()> = (&ok).try_into().expect("non-zero account converts");
        assert_ne!(
            order.user_id(),
            pricelevel::Hash32::zero(),
            "a valid account must never produce the no-STP sentinel"
        );
    }

    #[test]
    fn try_from_rejects_zero_quantity_issue_125() {
        // A zero-quantity order is structurally meaningless and otherwise slips
        // past the default-config lot / min-size checks. Reject it precisely.
        let wire = NewOrderWire {
            client_ts: 0,
            order_id: 1,
            account_id: 2,
            price: 100,
            qty: 0,
            side: SIDE_BUY,
            time_in_force: TIF_GTC,
            order_type: ORDER_TYPE_STANDARD,
            _pad: [0u8; 5],
        };
        let res: Result<OrderType<()>, _> = (&wire).try_into();
        assert!(matches!(
            res,
            Err(WireError::InvalidPayload("NewOrder: zero quantity"))
        ));
    }

    #[test]
    fn try_from_rejects_zero_price_issue_125() {
        // price 0 is the cache "no best price" sentinel; only price > 0 is
        // admissible at the wire boundary.
        let wire = NewOrderWire {
            client_ts: 0,
            order_id: 1,
            account_id: 2,
            price: 0,
            qty: 5,
            side: SIDE_BUY,
            time_in_force: TIF_GTC,
            order_type: ORDER_TYPE_STANDARD,
            _pad: [0u8; 5],
        };
        let res: Result<OrderType<()>, _> = (&wire).try_into();
        assert!(matches!(
            res,
            Err(WireError::InvalidPayload("NewOrder: zero price"))
        ));
    }

    #[test]
    fn try_from_builds_standard_order() {
        let wire = NewOrderWire {
            client_ts: 1_700_000_000_000,
            order_id: 42,
            account_id: 7,
            price: 9_999,
            qty: 10,
            side: SIDE_SELL,
            time_in_force: TIF_IOC,
            order_type: ORDER_TYPE_STANDARD,
            _pad: [0u8; 5],
        };
        let order: OrderType<()> = (&wire).try_into().expect("convert to OrderType");
        match order {
            OrderType::Standard {
                price,
                quantity,
                side,
                time_in_force,
                ..
            } => {
                assert_eq!(price.as_u128(), 9_999);
                assert_eq!(quantity.as_u64(), 10);
                assert_eq!(side, Side::Sell);
                assert_eq!(time_in_force, TimeInForce::Ioc);
            }
            _ => panic!("expected Standard variant"),
        }
    }

    #[test]
    fn try_from_rejects_min_price_and_accepts_max_price() {
        let wire = NewOrderWire {
            client_ts: 0,
            order_id: 1,
            account_id: 2,
            price: i64::MIN,
            qty: 5,
            side: SIDE_BUY,
            time_in_force: TIF_GTC,
            order_type: ORDER_TYPE_STANDARD,
            _pad: [0u8; 5],
        };
        let res: Result<OrderType<()>, _> = (&wire).try_into();
        assert_eq!(
            res.err(),
            Some(WireError::InvalidPayload("NewOrder: negative price"))
        );

        let max = NewOrderWire {
            price: i64::MAX,
            ..wire
        };
        let order: OrderType<()> = (&max).try_into().expect("i64::MAX price converts");
        assert_eq!(order.price().as_u128(), u128::from(i64::MAX.unsigned_abs()));
    }

    #[test]
    fn try_from_encodes_account_id_into_low_user_bytes() {
        let account_id = 0x0102_0304_0506_0708_u64;
        let wire = NewOrderWire {
            client_ts: 0,
            order_id: 1,
            account_id,
            price: 100,
            qty: 5,
            side: SIDE_BUY,
            time_in_force: TIF_GTC,
            order_type: ORDER_TYPE_STANDARD,
            _pad: [0u8; 5],
        };
        let order: OrderType<()> = (&wire).try_into().expect("convert");
        let mut expected = [0u8; 32];
        for (dst, src) in expected.iter_mut().zip(account_id.to_le_bytes()) {
            *dst = src;
        }
        assert_eq!(order.user_id(), Hash32::new(expected));
    }
}
