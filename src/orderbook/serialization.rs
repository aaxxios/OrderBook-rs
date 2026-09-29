//! Pluggable event serialization for NATS publishers and consumers.
//!
//! This module provides the [`EventSerializer`] trait and two built-in
//! implementations:
//!
//! - [`JsonEventSerializer`] — human-readable JSON (always available)
//! - `BincodeEventSerializer` — compact binary format (requires the
//!   `bincode` feature)
//!
//! Publishers such as `NatsTradePublisher` (requires the `nats` feature)
//! accept any `Arc<dyn EventSerializer>` so the serialization format can be
//! chosen at construction time without changing downstream code.
//!
//! # Feature Gate
//!
//! The `BincodeEventSerializer` requires the `bincode` feature:
//!
//! ```toml
//! [dependencies]
//! orderbook-rs = { version = "0.14", features = ["bincode"] }
//! ```

use crate::orderbook::book_change_event::PriceLevelChangedEvent;
use crate::orderbook::trade::TradeResult;

/// Errors that can occur during event serialization or deserialization.
///
/// A typed enum that preserves the underlying serde / bincode failure rather
/// than flattening it to a string. It bridges into
/// [`OrderBookError`](crate::orderbook::OrderBookError) via `From`, so an
/// [`EventSerializer`] failure can be `?`-propagated on paths that return
/// `OrderBookError`.
#[derive(Debug, thiserror::Error)]
pub enum SerializationError {
    /// JSON (`serde_json`) serialization or deserialization failed.
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// Binary (`bincode`) serialization or deserialization failed.
    #[error("bincode serialization error: {0}")]
    Bincode(String),

    /// The decoded payload had unexpected trailing bytes (corruption or a
    /// format mismatch).
    #[error("{0}")]
    TrailingBytes(String),

    /// The payload is longer than the serializer's configured limit, in
    /// bytes. Returned on decode before any parsing, and on encode when the
    /// produced payload would exceed the limit.
    #[error("payload of {len} bytes exceeds the {max}-byte limit")]
    PayloadTooLarge {
        /// Payload length in bytes.
        len: usize,
        /// Configured maximum payload length in bytes.
        max: usize,
    },

    /// The payload ended before the value was fully decoded, or a length
    /// prefix inside it declared more bytes than remain (malformed or hostile
    /// input). Rejected before allocating for the declared length.
    #[error("payload truncated: {additional} more bytes needed")]
    Truncated {
        /// Number of additional bytes the decoder needed.
        additional: usize,
    },
}

/// A pluggable serializer for order book events.
///
/// Implementations convert [`TradeResult`] and [`PriceLevelChangedEvent`]
/// to and from byte buffers. The format (JSON, Bincode, etc.) is an
/// implementation detail, allowing publishers and consumers to negotiate
/// the most efficient wire format.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` so they can be shared across
/// async task boundaries via `Arc<dyn EventSerializer>`.
pub trait EventSerializer: Send + Sync + std::fmt::Debug {
    /// Serialize a [`TradeResult`] into a byte buffer.
    ///
    /// # Errors
    ///
    /// Returns [`SerializationError`] if the event cannot be serialized.
    fn serialize_trade(&self, trade: &TradeResult) -> Result<Vec<u8>, SerializationError>;

    /// Serialize a [`PriceLevelChangedEvent`] into a byte buffer.
    ///
    /// # Errors
    ///
    /// Returns [`SerializationError`] if the event cannot be serialized.
    fn serialize_book_change(
        &self,
        event: &PriceLevelChangedEvent,
    ) -> Result<Vec<u8>, SerializationError>;

    /// Deserialize a [`TradeResult`] from a byte buffer.
    ///
    /// # Errors
    ///
    /// Returns [`SerializationError`] if the bytes are malformed or
    /// incompatible with the expected format.
    fn deserialize_trade(&self, data: &[u8]) -> Result<TradeResult, SerializationError>;

    /// Deserialize a [`PriceLevelChangedEvent`] from a byte buffer.
    ///
    /// # Errors
    ///
    /// Returns [`SerializationError`] if the bytes are malformed or
    /// incompatible with the expected format.
    fn deserialize_book_change(
        &self,
        data: &[u8],
    ) -> Result<PriceLevelChangedEvent, SerializationError>;

    /// Returns the MIME-like content type identifier for this format.
    ///
    /// Consumers can use this value to select the correct deserializer.
    /// Examples: `"application/json"`, `"application/x-bincode"`.
    #[must_use]
    fn content_type(&self) -> &'static str;
}

// ─── JSON ───────────────────────────────────────────────────────────────────

/// JSON event serializer using `serde_json`.
///
/// This is the default serializer, producing human-readable JSON payloads.
/// It is always available (no feature gate) since `serde_json` is a
/// required dependency.
///
/// # Content Type
///
/// `"application/json"`
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonEventSerializer;

impl JsonEventSerializer {
    /// Create a new JSON event serializer.
    #[must_use]
    #[inline]
    pub fn new() -> Self {
        Self
    }
}

impl EventSerializer for JsonEventSerializer {
    fn serialize_trade(&self, trade: &TradeResult) -> Result<Vec<u8>, SerializationError> {
        serde_json::to_vec(trade).map_err(SerializationError::Json)
    }

    fn serialize_book_change(
        &self,
        event: &PriceLevelChangedEvent,
    ) -> Result<Vec<u8>, SerializationError> {
        serde_json::to_vec(event).map_err(SerializationError::Json)
    }

    fn deserialize_trade(&self, data: &[u8]) -> Result<TradeResult, SerializationError> {
        serde_json::from_slice(data).map_err(SerializationError::Json)
    }

    fn deserialize_book_change(
        &self,
        data: &[u8],
    ) -> Result<PriceLevelChangedEvent, SerializationError> {
        serde_json::from_slice(data).map_err(SerializationError::Json)
    }

    #[inline]
    fn content_type(&self) -> &'static str {
        "application/json"
    }
}

// ─── Bincode ────────────────────────────────────────────────────────────────

/// Default ceiling, in bytes, on a single bincode payload accepted by
/// [`BincodeEventSerializer`] (8 MiB).
///
/// Chosen as the largest `max_payload` the NATS documentation recommends
/// configuring on a server (the server default is 1 MiB), since NATS is the
/// transport these payloads travel over. A bincode [`TradeResult`] costs
/// roughly 170 bytes per fill (three 36-byte `Id` strings plus price,
/// quantity, side and timestamp for the trade, and one `Id` string for a
/// filled maker), so 8 MiB leaves room for a single aggressive order that
/// sweeps about 48 000 resting orders. A [`PriceLevelChangedEvent`] is at
/// most 40 bytes. Deployments that raise the NATS `max_payload` beyond this
/// can opt in via [`BincodeEventSerializer::with_max_payload_bytes`].
#[cfg(feature = "bincode")]
pub const DEFAULT_MAX_BINCODE_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Hard ceiling, in bytes, on the configurable bincode payload limit
/// (64 MiB, the maximum `max_payload` a NATS server accepts).
///
/// [`BincodeEventSerializer::with_max_payload_bytes`] clamps to this value.
#[cfg(feature = "bincode")]
pub const MAX_BINCODE_PAYLOAD_BYTES_CEILING: usize = 64 * 1024 * 1024;

/// Bincode event serializer for compact binary payloads.
///
/// Produces significantly smaller payloads than JSON with much lower
/// serialization latency (typically < 500 ns per event). The trade-off
/// is that the output is not human-readable.
///
/// # Untrusted input
///
/// Deserialization treats the bytes as untrusted. Unconfigured, bincode
/// 2.0.1 decodes a `String` / `Vec<u8>` by allocating the length read from
/// the payload before checking the input holds it, so a ten-byte payload
/// could request an allocation of `u64::MAX` bytes. This serializer bounds
/// every allocation by the input instead:
///
/// 1. Payloads longer than [`max_payload_bytes`](Self::max_payload_bytes)
///    are rejected up front with [`SerializationError::PayloadTooLarge`].
/// 2. Decoding runs through bincode's borrowed serde path with string and
///    byte-buffer requests routed to the slicing `deserialize_str` /
///    `deserialize_bytes`: a length prefix is checked against the remaining
///    input (end-of-input is [`SerializationError::Truncated`]) before a
///    single byte is allocated, and the copy that follows is the real
///    length. Across one payload, string / byte allocations sum to at most
///    the payload length.
/// 3. Sequence / map size hints are clamped to the payload length, so a
///    hostile element count reserves at most
///    `min(payload_len * size_of::<T>(), 1 MiB)` before the first element
///    runs out of input (1 MiB is serde's own reservation cap).
///
/// Serialization enforces the same `max_payload_bytes`, so a producer never
/// emits a payload its matching consumer would reject.
///
/// # Feature Gate
///
/// Requires the `bincode` feature:
///
/// ```toml
/// [dependencies]
/// orderbook-rs = { version = "0.14", features = ["bincode"] }
/// ```
///
/// # Content Type
///
/// `"application/x-bincode"`
#[cfg(feature = "bincode")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BincodeEventSerializer {
    max_payload_bytes: usize,
}

#[cfg(feature = "bincode")]
impl Default for BincodeEventSerializer {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "bincode")]
impl BincodeEventSerializer {
    /// Create a new Bincode event serializer that accepts payloads up to
    /// [`DEFAULT_MAX_BINCODE_PAYLOAD_BYTES`].
    #[must_use]
    #[inline]
    pub const fn new() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_BINCODE_PAYLOAD_BYTES,
        }
    }

    /// Create a Bincode event serializer with a custom payload limit, in
    /// bytes.
    ///
    /// The value is clamped to [`MAX_BINCODE_PAYLOAD_BYTES_CEILING`]; read
    /// the effective limit back with
    /// [`max_payload_bytes`](Self::max_payload_bytes). Producer and consumer
    /// should agree on the value: a payload the producer accepts but the
    /// consumer does not is rejected on decode.
    #[must_use]
    #[inline]
    pub const fn with_max_payload_bytes(max_payload_bytes: usize) -> Self {
        let max_payload_bytes = if max_payload_bytes > MAX_BINCODE_PAYLOAD_BYTES_CEILING {
            MAX_BINCODE_PAYLOAD_BYTES_CEILING
        } else {
            max_payload_bytes
        };
        Self { max_payload_bytes }
    }

    /// The largest payload, in bytes, this serializer encodes or decodes.
    #[must_use]
    #[inline]
    pub const fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    /// Reject `len` if it exceeds the configured payload limit.
    #[inline]
    fn check_len(&self, len: usize) -> Result<(), SerializationError> {
        if len > self.max_payload_bytes {
            return Err(payload_too_large(len, self.max_payload_bytes));
        }
        Ok(())
    }

    /// Encode `value`, rejecting output larger than the payload limit.
    fn encode<V: serde::Serialize>(&self, value: &V) -> Result<Vec<u8>, SerializationError> {
        let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())
            .map_err(|e| SerializationError::Bincode(e.to_string()))?;
        self.check_len(bytes.len())?;
        Ok(bytes)
    }

    /// Decode a `V` from untrusted `data` with every allocation backed by
    /// the input (see the private `input_backed` module) and reject trailing
    /// bytes.
    fn decode<V: serde::de::DeserializeOwned>(
        &self,
        data: &[u8],
        what: &'static str,
    ) -> Result<V, SerializationError> {
        self.check_len(data.len())?;
        // No `with_limit`: its accounting charges the in-memory width of
        // every primitive (16 for a one-byte `u128` varint), so it cannot be
        // set to the input length without rejecting valid payloads, and the
        // input-backed adapter already bounds every allocation it guards.
        let seed = input_backed::Seed {
            seed: std::marker::PhantomData::<V>,
            max_elems: data.len(),
        };
        let (value, bytes_read) =
            bincode::serde::seed_decode_from_slice(seed, data, bincode::config::standard())
                .map_err(map_decode_error)?;
        if bytes_read != data.len() {
            return Err(SerializationError::TrailingBytes(format!(
                "trailing bytes after {what} payload: consumed {bytes_read} of {}",
                data.len()
            )));
        }
        Ok(value)
    }
}

/// Map a bincode decode failure to a typed [`SerializationError`].
#[cfg(feature = "bincode")]
#[cold]
fn map_decode_error(err: bincode::error::DecodeError) -> SerializationError {
    match err {
        bincode::error::DecodeError::UnexpectedEnd { additional } => {
            SerializationError::Truncated { additional }
        }
        other => SerializationError::Bincode(other.to_string()),
    }
}

/// Input-backed serde adapter for bincode's borrowed deserializer (#251).
///
/// bincode 2.0.1's borrowed serde deserializer
/// (`bincode::serde::seed_decode_from_slice`) implements `deserialize_str` /
/// `deserialize_bytes` by slicing the input: `<&[u8]>::borrow_decode` calls
/// `SliceReader::take_bytes(len)`, which returns `UnexpectedEnd` when `len`
/// exceeds the remaining bytes, before anything is allocated. Its
/// `deserialize_string` / `deserialize_byte_buf`, however, go through the
/// owned `Vec<u8>::decode`, which runs `vec![0u8; len]` on the declared
/// length first, and serde's `String` / `Vec<u8>` impls ask for exactly
/// those. [`InputBacked`] wraps the deserializer and, recursively through
/// every nested seq / map / enum / option / newtype access:
///
/// - routes `deserialize_string` to `deserialize_str` and
///   `deserialize_byte_buf` to `deserialize_bytes`, so a string or byte
///   buffer is only allocated after it was sliced out of the input: its
///   allocation equals its real length and the sum over one payload never
///   exceeds the payload length;
/// - clamps a sequence / map `size_hint` to the payload length, so a
///   declared element count the input cannot back does not drive serde's
///   up-front reservation. Every element of a non-zero-sized type consumes at
///   least one input byte, so a legitimate count never exceeds the payload
///   length and the clamp never shrinks a correct hint. The residual
///   reservation for a hostile count is
///   `min(payload_len * size_of::<T>(), 1 MiB)` (serde's own cap), released
///   when the first element hits end-of-input.
///
/// No other deserializer method of bincode's borrowed path allocates
/// (primitives decode in place; `deserialize_any` / `deserialize_identifier`
/// / `deserialize_ignored_any` are unsupported by bincode and error out).
#[cfg(feature = "bincode")]
mod input_backed {
    use serde::de::{
        DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor,
    };
    use std::fmt;

    /// Clamp a container `size_hint` to the payload length.
    #[inline]
    fn clamp_hint(hint: Option<usize>, max: usize) -> Option<usize> {
        hint.map(|n| n.min(max))
    }

    /// Seed that deserializes `T` through [`InputBacked`].
    pub(super) struct Seed<S> {
        pub(super) seed: S,
        pub(super) max_elems: usize,
    }

    impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Seed<S> {
        type Value = S::Value;

        #[inline]
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<S::Value, D::Error> {
            self.seed.deserialize(InputBacked {
                inner: deserializer,
                max_elems: self.max_elems,
            })
        }
    }

    /// Deserializer wrapper; see the module docs.
    struct InputBacked<D> {
        inner: D,
        max_elems: usize,
    }

    /// Visitor wrapper that re-wraps every nested deserializer / access.
    struct Wrap<V> {
        visitor: V,
        max_elems: usize,
    }

    impl<V> Wrap<V> {
        #[inline]
        fn new(visitor: V, max_elems: usize) -> Self {
            Self { visitor, max_elems }
        }
    }

    macro_rules! forward_leaf {
        ($($method:ident),* $(,)?) => {
            $(
                #[inline]
                fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
                    self.inner.$method(visitor)
                }
            )*
        };
    }

    impl<'de, D: Deserializer<'de>> Deserializer<'de> for InputBacked<D> {
        type Error = D::Error;

        forward_leaf!(
            deserialize_any,
            deserialize_bool,
            deserialize_i8,
            deserialize_i16,
            deserialize_i32,
            deserialize_i64,
            deserialize_i128,
            deserialize_u8,
            deserialize_u16,
            deserialize_u32,
            deserialize_u64,
            deserialize_u128,
            deserialize_f32,
            deserialize_f64,
            deserialize_char,
            deserialize_str,
            deserialize_bytes,
            deserialize_unit,
            deserialize_identifier,
            deserialize_ignored_any,
        );

        /// Slice first, copy after: see the module docs.
        #[inline]
        fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
            self.inner.deserialize_str(visitor)
        }

        /// Slice first, copy after: see the module docs.
        #[inline]
        fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
            self.inner.deserialize_bytes(visitor)
        }

        fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
            self.inner
                .deserialize_option(Wrap::new(visitor, self.max_elems))
        }

        fn deserialize_unit_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            visitor: V,
        ) -> Result<V::Value, D::Error> {
            self.inner.deserialize_unit_struct(name, visitor)
        }

        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            visitor: V,
        ) -> Result<V::Value, D::Error> {
            self.inner
                .deserialize_newtype_struct(name, Wrap::new(visitor, self.max_elems))
        }

        fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
            self.inner
                .deserialize_seq(Wrap::new(visitor, self.max_elems))
        }

        fn deserialize_tuple<V: Visitor<'de>>(
            self,
            len: usize,
            visitor: V,
        ) -> Result<V::Value, D::Error> {
            self.inner
                .deserialize_tuple(len, Wrap::new(visitor, self.max_elems))
        }

        fn deserialize_tuple_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            len: usize,
            visitor: V,
        ) -> Result<V::Value, D::Error> {
            self.inner
                .deserialize_tuple_struct(name, len, Wrap::new(visitor, self.max_elems))
        }

        fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
            self.inner
                .deserialize_map(Wrap::new(visitor, self.max_elems))
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            fields: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, D::Error> {
            self.inner
                .deserialize_struct(name, fields, Wrap::new(visitor, self.max_elems))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            name: &'static str,
            variants: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, D::Error> {
            self.inner
                .deserialize_enum(name, variants, Wrap::new(visitor, self.max_elems))
        }

        #[inline]
        fn is_human_readable(&self) -> bool {
            self.inner.is_human_readable()
        }
    }

    macro_rules! forward_visit {
        ($($method:ident($ty:ty)),* $(,)?) => {
            $(
                #[inline]
                fn $method<E: serde::de::Error>(self, v: $ty) -> Result<V::Value, E> {
                    self.visitor.$method(v)
                }
            )*
        };
    }

    impl<'de, V: Visitor<'de>> Visitor<'de> for Wrap<V> {
        type Value = V::Value;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.visitor.expecting(f)
        }

        forward_visit!(
            visit_bool(bool),
            visit_i8(i8),
            visit_i16(i16),
            visit_i32(i32),
            visit_i64(i64),
            visit_i128(i128),
            visit_u8(u8),
            visit_u16(u16),
            visit_u32(u32),
            visit_u64(u64),
            visit_u128(u128),
            visit_f32(f32),
            visit_f64(f64),
            visit_char(char),
            visit_str(&str),
            visit_borrowed_str(&'de str),
            visit_string(String),
            visit_bytes(&[u8]),
            visit_borrowed_bytes(&'de [u8]),
            visit_byte_buf(Vec<u8>),
        );

        #[inline]
        fn visit_none<E: serde::de::Error>(self) -> Result<V::Value, E> {
            self.visitor.visit_none()
        }

        #[inline]
        fn visit_unit<E: serde::de::Error>(self) -> Result<V::Value, E> {
            self.visitor.visit_unit()
        }

        fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<V::Value, D::Error> {
            self.visitor.visit_some(InputBacked {
                inner: deserializer,
                max_elems: self.max_elems,
            })
        }

        fn visit_newtype_struct<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<V::Value, D::Error> {
            self.visitor.visit_newtype_struct(InputBacked {
                inner: deserializer,
                max_elems: self.max_elems,
            })
        }

        fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<V::Value, A::Error> {
            self.visitor.visit_seq(Wrap::new(seq, self.max_elems))
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<V::Value, A::Error> {
            self.visitor.visit_map(Wrap::new(map, self.max_elems))
        }

        fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<V::Value, A::Error> {
            self.visitor.visit_enum(Wrap::new(data, self.max_elems))
        }
    }

    impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Wrap<A> {
        type Error = A::Error;

        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>, A::Error> {
            self.visitor.next_element_seed(Seed {
                seed,
                max_elems: self.max_elems,
            })
        }

        #[inline]
        fn size_hint(&self) -> Option<usize> {
            clamp_hint(self.visitor.size_hint(), self.max_elems)
        }
    }

    impl<'de, A: MapAccess<'de>> MapAccess<'de> for Wrap<A> {
        type Error = A::Error;

        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, A::Error> {
            self.visitor.next_key_seed(Seed {
                seed,
                max_elems: self.max_elems,
            })
        }

        fn next_value_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<T::Value, A::Error> {
            self.visitor.next_value_seed(Seed {
                seed,
                max_elems: self.max_elems,
            })
        }

        #[inline]
        fn size_hint(&self) -> Option<usize> {
            clamp_hint(self.visitor.size_hint(), self.max_elems)
        }
    }

    impl<'de, A: EnumAccess<'de>> EnumAccess<'de> for Wrap<A> {
        type Error = A::Error;
        type Variant = Wrap<A::Variant>;

        fn variant_seed<T: DeserializeSeed<'de>>(
            self,
            seed: T,
        ) -> Result<(T::Value, Self::Variant), A::Error> {
            let max_elems = self.max_elems;
            let (value, variant) = self.visitor.variant_seed(Seed { seed, max_elems })?;
            Ok((value, Wrap::new(variant, max_elems)))
        }
    }

    impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Wrap<A> {
        type Error = A::Error;

        #[inline]
        fn unit_variant(self) -> Result<(), A::Error> {
            self.visitor.unit_variant()
        }

        fn newtype_variant_seed<T: DeserializeSeed<'de>>(
            self,
            seed: T,
        ) -> Result<T::Value, A::Error> {
            self.visitor.newtype_variant_seed(Seed {
                seed,
                max_elems: self.max_elems,
            })
        }

        fn tuple_variant<V: Visitor<'de>>(
            self,
            len: usize,
            visitor: V,
        ) -> Result<V::Value, A::Error> {
            self.visitor
                .tuple_variant(len, Wrap::new(visitor, self.max_elems))
        }

        fn struct_variant<V: Visitor<'de>>(
            self,
            fields: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, A::Error> {
            self.visitor
                .struct_variant(fields, Wrap::new(visitor, self.max_elems))
        }
    }
}

#[cfg(feature = "bincode")]
#[cold]
fn payload_too_large(len: usize, max: usize) -> SerializationError {
    SerializationError::PayloadTooLarge { len, max }
}

#[cfg(feature = "bincode")]
impl EventSerializer for BincodeEventSerializer {
    fn serialize_trade(&self, trade: &TradeResult) -> Result<Vec<u8>, SerializationError> {
        self.encode(trade)
    }

    fn serialize_book_change(
        &self,
        event: &PriceLevelChangedEvent,
    ) -> Result<Vec<u8>, SerializationError> {
        self.encode(event)
    }

    fn deserialize_trade(&self, data: &[u8]) -> Result<TradeResult, SerializationError> {
        self.decode(data, "trade")
    }

    fn deserialize_book_change(
        &self,
        data: &[u8],
    ) -> Result<PriceLevelChangedEvent, SerializationError> {
        self.decode(data, "book-change")
    }

    #[inline]
    fn content_type(&self) -> &'static str {
        "application/x-bincode"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pricelevel::{Id, MatchResult, Quantity, Side};
    use uuid::Uuid;

    fn make_trade_result() -> TradeResult {
        let order_id = Id::from_uuid(Uuid::new_v4());
        let match_result = MatchResult::new(order_id, Quantity::new(100));
        TradeResult::new("BTC/USD".to_string(), match_result).expect("valid trade result")
    }

    fn make_book_change() -> PriceLevelChangedEvent {
        PriceLevelChangedEvent {
            side: Side::Buy,
            price: 50_000_000,
            quantity: 1_000,
            engine_seq: 0,
        }
    }

    // ─── JSON tests ─────────────────────────────────────────────────────

    #[test]
    fn test_json_serialize_trade() {
        let serializer = JsonEventSerializer::new();
        let trade = make_trade_result();
        let result = serializer.serialize_trade(&trade);
        assert!(result.is_ok());
        let bytes = result.unwrap_or_default();
        assert!(!bytes.is_empty());

        let json_str = String::from_utf8(bytes).unwrap_or_default();
        assert!(json_str.contains("BTC/USD"));
    }

    #[test]
    fn test_json_roundtrip_trade() {
        let serializer = JsonEventSerializer::new();
        let trade = make_trade_result();
        let bytes = serializer.serialize_trade(&trade);
        assert!(bytes.is_ok());
        let bytes = bytes.unwrap_or_default();

        let decoded = serializer.deserialize_trade(&bytes);
        assert!(decoded.is_ok());
        let decoded = decoded.unwrap_or_else(|_| make_trade_result());
        assert_eq!(decoded.symbol, trade.symbol);
        assert_eq!(decoded.total_maker_fees, trade.total_maker_fees);
        assert_eq!(decoded.total_taker_fees, trade.total_taker_fees);
    }

    #[test]
    fn test_json_serialize_book_change() {
        let serializer = JsonEventSerializer::new();
        let event = make_book_change();
        let result = serializer.serialize_book_change(&event);
        assert!(result.is_ok());
        let bytes = result.unwrap_or_default();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn test_json_roundtrip_book_change() {
        let serializer = JsonEventSerializer::new();
        let event = make_book_change();
        let bytes = serializer.serialize_book_change(&event);
        assert!(bytes.is_ok());
        let bytes = bytes.unwrap_or_default();

        let decoded = serializer.deserialize_book_change(&bytes);
        assert!(decoded.is_ok());
        let decoded = decoded.unwrap_or_else(|_| make_book_change());
        assert_eq!(decoded, event);
    }

    #[test]
    fn test_json_content_type() {
        let serializer = JsonEventSerializer::new();
        assert_eq!(serializer.content_type(), "application/json");
    }

    #[test]
    fn test_json_deserialize_trade_error() {
        let serializer = JsonEventSerializer::new();
        let result = serializer.deserialize_trade(b"not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn test_json_deserialize_book_change_error() {
        let serializer = JsonEventSerializer::new();
        let result = serializer.deserialize_book_change(b"not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn test_serialization_error_display_preserves_underlying() {
        // The Json variant preserves the typed serde error.
        let serde_err = serde_json::from_str::<i32>("not a number").unwrap_err();
        let err = SerializationError::Json(serde_err);
        let display = format!("{err}");
        assert!(display.contains("JSON serialization error"));

        let trailing = SerializationError::TrailingBytes("consumed 3 of 5".to_string());
        assert!(format!("{trailing}").contains("consumed 3 of 5"));
    }

    #[test]
    fn test_serialization_error_propagates_into_orderbook_error() {
        use crate::orderbook::error::OrderBookError;

        // A function returning OrderBookError can `?` a SerializationError
        // thanks to the `From` bridge.
        fn run() -> Result<i32, OrderBookError> {
            let parsed: i32 = serde_json::from_str("nope").map_err(SerializationError::Json)?;
            Ok(parsed)
        }

        match run() {
            Err(OrderBookError::SerializationError { .. }) => {}
            other => panic!("expected OrderBookError::SerializationError, got {other:?}"),
        }
    }

    // ─── Bincode tests ──────────────────────────────────────────────────

    #[cfg(feature = "bincode")]
    mod bincode_tests {
        use super::*;

        #[test]
        fn test_bincode_serialize_trade() {
            let serializer = BincodeEventSerializer::new();
            let trade = make_trade_result();
            let result = serializer.serialize_trade(&trade);
            assert!(result.is_ok());
            let bytes = result.unwrap_or_default();
            assert!(!bytes.is_empty());

            // Bincode should be more compact than JSON
            let json_serializer = JsonEventSerializer::new();
            let json_bytes = json_serializer.serialize_trade(&trade).unwrap_or_default();
            assert!(
                bytes.len() < json_bytes.len(),
                "bincode ({}) should be smaller than json ({})",
                bytes.len(),
                json_bytes.len()
            );
        }

        #[test]
        fn test_bincode_roundtrip_trade() {
            let serializer = BincodeEventSerializer::new();
            let trade = make_trade_result();
            let bytes = serializer.serialize_trade(&trade);
            assert!(bytes.is_ok());
            let bytes = bytes.unwrap_or_default();

            let decoded = serializer.deserialize_trade(&bytes);
            assert!(decoded.is_ok());
            let decoded = decoded.unwrap_or_else(|_| make_trade_result());
            assert_eq!(decoded.symbol, trade.symbol);
            assert_eq!(decoded.total_maker_fees, trade.total_maker_fees);
            assert_eq!(decoded.total_taker_fees, trade.total_taker_fees);
        }

        #[test]
        fn test_bincode_serialize_book_change() {
            let serializer = BincodeEventSerializer::new();
            let event = make_book_change();
            let result = serializer.serialize_book_change(&event);
            assert!(result.is_ok());
            let bytes = result.unwrap_or_default();
            assert!(!bytes.is_empty());
        }

        #[test]
        fn test_bincode_roundtrip_book_change() {
            let serializer = BincodeEventSerializer::new();
            let event = make_book_change();
            let bytes = serializer.serialize_book_change(&event);
            assert!(bytes.is_ok());
            let bytes = bytes.unwrap_or_default();

            let decoded = serializer.deserialize_book_change(&bytes);
            assert!(decoded.is_ok());
            let decoded = decoded.unwrap_or_else(|_| make_book_change());
            assert_eq!(decoded, event);
        }

        #[test]
        fn test_bincode_content_type() {
            let serializer = BincodeEventSerializer::new();
            assert_eq!(serializer.content_type(), "application/x-bincode");
        }

        #[test]
        fn test_bincode_deserialize_trade_error() {
            let serializer = BincodeEventSerializer::new();
            let result = serializer.deserialize_trade(b"\x00\x01");
            assert!(result.is_err());
        }

        #[test]
        fn test_bincode_deserialize_book_change_error() {
            let serializer = BincodeEventSerializer::new();
            let result = serializer.deserialize_book_change(b"\x00\x01");
            assert!(result.is_err());
        }

        // ─── Untrusted-input bounds (#251) ─────────────────────────────

        /// bincode `standard()` varint marker for "a u64 follows".
        const VARINT_U64: u8 = 253;

        /// A varint length prefix declaring `u64::MAX` elements / bytes.
        fn huge_len_prefix() -> Vec<u8> {
            let mut out = vec![VARINT_U64];
            out.extend_from_slice(&u64::MAX.to_le_bytes());
            out
        }

        /// A varint length prefix declaring `n` elements / bytes.
        fn len_prefix(n: u64) -> Vec<u8> {
            let mut out = vec![VARINT_U64];
            out.extend_from_slice(&n.to_le_bytes());
            out
        }

        fn encode_raw<V: serde::Serialize>(value: &V) -> Vec<u8> {
            bincode::serde::encode_to_vec(value, bincode::config::standard())
                .expect("encode fixture")
        }

        /// Wire prefix of a [`TradeResult`] up to (excluding) the
        /// `trades` length: `symbol`, then `match_result.order_id`.
        fn trade_prefix_before_trades(symbol: &str, order_id: Id) -> Vec<u8> {
            encode_raw(&(symbol.to_string(), order_id.to_string()))
        }

        /// Wire prefix of a [`TradeResult`] with no trades, up to
        /// (excluding) the `filled_order_ids` length.
        fn trade_prefix_before_filled_ids(symbol: &str, order_id: Id, remaining: u64) -> Vec<u8> {
            // `trades` is a `TradeList { trades: Vec<Trade> }` → a single
            // length varint; an empty list encodes as one zero byte.
            encode_raw(&(
                symbol.to_string(),
                order_id.to_string(),
                0u64,
                remaining,
                false,
            ))
        }

        fn make_large_trade_result(fills: usize) -> TradeResult {
            let taker = Id::from_uuid(Uuid::new_v4());
            let total = u64::try_from(fills).expect("fits");
            let mut mr = MatchResult::new(taker, Quantity::new(total));
            for i in 0..fills {
                let maker = Id::from_uuid(Uuid::new_v4());
                let trade = pricelevel::Trade::with_timestamp(
                    Id::from_uuid(Uuid::new_v4()),
                    taker,
                    maker,
                    pricelevel::Price::new(
                        50_000u128
                            .checked_add(u128::try_from(i).expect("fits"))
                            .expect("fits"),
                    ),
                    Quantity::new(1),
                    Side::Buy,
                    pricelevel::TimestampMs::new(1_700_000_000_000),
                );
                mr.add_trade(trade).expect("valid fill");
                mr.add_filled_order_id(maker).expect("filled id");
            }
            TradeResult::new("BTC/USD".to_string(), mr).expect("valid trade result")
        }

        #[test]
        fn test_bincode_prefix_fixtures_match_real_encoding() {
            // Guards the hand-built malicious payloads below: they must
            // share the exact layout of a real encoding.
            let order_id = Id::from_uuid(Uuid::new_v4());
            let trade = TradeResult::new(
                "BTC/USD".to_string(),
                MatchResult::new(order_id, Quantity::new(100)),
            )
            .expect("valid trade result");
            let bytes = BincodeEventSerializer::new()
                .serialize_trade(&trade)
                .expect("encode");
            let before_trades = trade_prefix_before_trades("BTC/USD", order_id);
            assert!(bytes.starts_with(&before_trades));
            let before_ids = trade_prefix_before_filled_ids("BTC/USD", order_id, 100);
            assert!(bytes.starts_with(&before_ids));
            // …and `filled_order_ids` is empty: next byte is a zero length.
            assert_eq!(bytes.get(before_ids.len()), Some(&0u8));
        }

        #[test]
        fn test_bincode_deserialize_trade_huge_symbol_len_rejected_before_alloc() {
            let serializer = BincodeEventSerializer::new();
            let payload = huge_len_prefix();
            match serializer.deserialize_trade(&payload) {
                Err(SerializationError::Truncated { additional }) => {
                    // The declared length was checked against the remaining
                    // input (none) instead of being allocated.
                    assert_eq!(additional, usize::MAX);
                }
                other => panic!("expected Truncated, got {other:?}"),
            }
        }

        #[test]
        fn test_bincode_deserialize_trade_huge_order_id_len_rejected() {
            let serializer = BincodeEventSerializer::new();
            let mut payload = encode_raw(&"BTC/USD".to_string());
            payload.extend_from_slice(&huge_len_prefix());
            assert!(matches!(
                serializer.deserialize_trade(&payload),
                Err(SerializationError::Truncated { .. })
            ));
        }

        #[test]
        fn test_bincode_deserialize_trade_len_one_past_input_rejected() {
            // A string length one byte longer than the remaining input is
            // rejected, with the exact shortfall reported.
            let serializer = BincodeEventSerializer::new();
            let mut payload = len_prefix(8);
            payload.extend_from_slice(b"BTC/USD");
            assert!(matches!(
                serializer.deserialize_trade(&payload),
                Err(SerializationError::Truncated { additional: 1 })
            ));
        }

        #[test]
        fn test_bincode_deserialize_trade_huge_trade_list_len_rejected() {
            let serializer = BincodeEventSerializer::new();
            let order_id = Id::from_uuid(Uuid::new_v4());
            let mut payload = trade_prefix_before_trades("BTC/USD", order_id);
            payload.extend_from_slice(&huge_len_prefix());
            // The size hint is clamped to the payload length; the first
            // element then runs out of input.
            assert!(matches!(
                serializer.deserialize_trade(&payload),
                Err(SerializationError::Truncated { .. })
            ));
        }

        #[test]
        fn test_bincode_deserialize_trade_huge_nested_trade_id_len_rejected() {
            let serializer = BincodeEventSerializer::new();
            let order_id = Id::from_uuid(Uuid::new_v4());
            let mut payload = trade_prefix_before_trades("BTC/USD", order_id);
            payload.push(1); // one trade…
            payload.extend_from_slice(&huge_len_prefix()); // …whose trade_id is huge
            assert!(matches!(
                serializer.deserialize_trade(&payload),
                Err(SerializationError::Truncated { .. })
            ));
        }

        #[test]
        fn test_bincode_deserialize_trade_huge_filled_ids_len_rejected() {
            let serializer = BincodeEventSerializer::new();
            let order_id = Id::from_uuid(Uuid::new_v4());
            let mut payload = trade_prefix_before_filled_ids("BTC/USD", order_id, 100);
            payload.extend_from_slice(&huge_len_prefix());
            assert!(matches!(
                serializer.deserialize_trade(&payload),
                Err(SerializationError::Truncated { .. })
            ));
        }

        #[test]
        fn test_bincode_deserialize_trade_huge_nested_filled_id_len_rejected() {
            let serializer = BincodeEventSerializer::new();
            let order_id = Id::from_uuid(Uuid::new_v4());
            let mut payload = trade_prefix_before_filled_ids("BTC/USD", order_id, 100);
            payload.push(1); // one filled id…
            payload.extend_from_slice(&huge_len_prefix()); // …with a huge length
            assert!(matches!(
                serializer.deserialize_trade(&payload),
                Err(SerializationError::Truncated { .. })
            ));
        }

        #[test]
        fn test_bincode_deserialize_book_change_malformed_rejected() {
            let serializer = BincodeEventSerializer::new();
            // Side variant index as a huge varint.
            assert!(
                serializer
                    .deserialize_book_change(&huge_len_prefix())
                    .is_err()
            );
            // u128 varint marker with no body.
            assert!(matches!(
                serializer.deserialize_book_change(&[0, 254]),
                Err(SerializationError::Truncated { .. })
            ));
            // Empty payload.
            assert!(matches!(
                serializer.deserialize_book_change(&[]),
                Err(SerializationError::Truncated { .. })
            ));
        }

        #[test]
        fn test_bincode_deserialize_truncated_payloads_return_errors() {
            let serializer = BincodeEventSerializer::new();
            let trade = serializer
                .serialize_trade(&make_large_trade_result(3))
                .expect("encode trade");
            for cut in 0..trade.len() {
                let prefix = trade.get(..cut).expect("in range");
                assert!(
                    serializer.deserialize_trade(prefix).is_err(),
                    "trade truncated at {cut} of {} decoded",
                    trade.len()
                );
            }
            let change = serializer
                .serialize_book_change(&make_book_change())
                .expect("encode book change");
            for cut in 0..change.len() {
                let prefix = change.get(..cut).expect("in range");
                assert!(serializer.deserialize_book_change(prefix).is_err());
            }
        }

        #[test]
        fn test_bincode_deserialize_mutated_payloads_never_panic() {
            // Fuzz-style: every single-byte overwrite of a real payload with
            // length-prefix markers and extremes must decode or fail typed.
            let serializer = BincodeEventSerializer::new();
            let trade = serializer
                .serialize_trade(&make_large_trade_result(2))
                .expect("encode trade");
            let change = serializer
                .serialize_book_change(&make_book_change())
                .expect("encode book change");
            for replacement in [0u8, 1, 250, 251, 252, 253, 254, 255] {
                for i in 0..trade.len() {
                    let mut mutated = trade.clone();
                    mutated[i] = replacement;
                    let _ = serializer.deserialize_trade(&mutated);
                    // Same position with a huge u64 spliced in.
                    let mut spliced = trade.get(..i).expect("in range").to_vec();
                    spliced.extend_from_slice(&huge_len_prefix());
                    spliced.extend_from_slice(trade.get(i..).expect("in range"));
                    let _ = serializer.deserialize_trade(&spliced);
                }
                for i in 0..change.len() {
                    let mut mutated = change.clone();
                    mutated[i] = replacement;
                    let _ = serializer.deserialize_book_change(&mutated);
                }
            }
        }

        #[test]
        fn test_bincode_payload_limit_enforced_on_both_directions() {
            let trade = make_large_trade_result(4);
            let bytes = BincodeEventSerializer::new()
                .serialize_trade(&trade)
                .expect("encode");
            let len = bytes.len();

            // Exactly at the limit: accepted both ways.
            let at_limit = BincodeEventSerializer::with_max_payload_bytes(len);
            assert_eq!(at_limit.max_payload_bytes(), len);
            let decoded = at_limit.deserialize_trade(&bytes).expect("decode at limit");
            assert_eq!(decoded.match_result.trades().len(), 4);
            assert!(at_limit.serialize_trade(&trade).is_ok());

            // One byte under: rejected up front, typed.
            let below = BincodeEventSerializer::with_max_payload_bytes(len - 1);
            match below.deserialize_trade(&bytes) {
                Err(SerializationError::PayloadTooLarge { len: l, max }) => {
                    assert_eq!(l, len);
                    assert_eq!(max, len - 1);
                }
                other => panic!("expected PayloadTooLarge, got {other:?}"),
            }
            assert!(matches!(
                below.serialize_trade(&trade),
                Err(SerializationError::PayloadTooLarge { .. })
            ));
        }

        #[test]
        fn test_bincode_with_max_payload_bytes_clamps_to_ceiling() {
            let s = BincodeEventSerializer::with_max_payload_bytes(usize::MAX);
            assert_eq!(s.max_payload_bytes(), MAX_BINCODE_PAYLOAD_BYTES_CEILING);
            assert_eq!(
                BincodeEventSerializer::default().max_payload_bytes(),
                DEFAULT_MAX_BINCODE_PAYLOAD_BYTES
            );
        }

        #[test]
        fn test_bincode_roundtrip_multi_mib_trade_result() {
            // ~30 000 fills ≈ 5 MiB: a multi-MiB payload with many strings must
            // round-trip under the default limit.
            let serializer = BincodeEventSerializer::new();
            let trade = make_large_trade_result(30_000);
            let bytes = serializer.serialize_trade(&trade).expect("encode");
            assert!(
                bytes.len() > 4 * 1024 * 1024,
                "payload is {} bytes",
                bytes.len()
            );
            assert!(bytes.len() <= DEFAULT_MAX_BINCODE_PAYLOAD_BYTES);
            let decoded = serializer.deserialize_trade(&bytes).expect("decode");
            assert_eq!(decoded.symbol, trade.symbol);
            assert_eq!(
                decoded.match_result.trades().as_vec(),
                trade.match_result.trades().as_vec()
            );
            assert_eq!(
                decoded.match_result.filled_order_ids(),
                trade.match_result.filled_order_ids()
            );
            assert_eq!(decoded.total_maker_fees, trade.total_maker_fees);
            assert_eq!(decoded.total_taker_fees, trade.total_taker_fees);
        }

        #[test]
        fn test_bincode_serialization_error_display_new_variants() {
            let e = SerializationError::PayloadTooLarge { len: 10, max: 5 };
            assert!(e.to_string().contains("10"));
            let e = SerializationError::Truncated { additional: 3 };
            assert!(e.to_string().contains("3 more bytes"));
        }

        #[test]
        fn test_bincode_smaller_than_json_book_change() {
            let event = make_book_change();
            let bincode_ser = BincodeEventSerializer::new();
            let json_ser = JsonEventSerializer::new();

            let bin_bytes = bincode_ser
                .serialize_book_change(&event)
                .unwrap_or_default();
            let json_bytes = json_ser.serialize_book_change(&event).unwrap_or_default();

            assert!(
                bin_bytes.len() < json_bytes.len(),
                "bincode ({}) should be smaller than json ({})",
                bin_bytes.len(),
                json_bytes.len()
            );
        }
    }
}
