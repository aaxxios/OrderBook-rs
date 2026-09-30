//! Order book snapshot for market data

use bitflags::bitflags;
use pricelevel::{OrderType, PriceLevelSnapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::trace;

use super::error::OrderBookError;
use super::fees::FeeSchedule;
use super::iterators::{checked_depth_add, checked_notional_add};
use super::risk::RiskConfig;
use super::stp::STPMode;

/// A snapshot of the order book state at a specific point in time
///
/// Each level is captured coherently on its own (quantities, order count and
/// the order vector in queue-consumption order). The level's embedded
/// `PriceLevelStatistics` execution aggregates are **advisory** when the
/// snapshot was taken while shared-gate takers were sweeping that level:
/// pricelevel 0.10 supports a single concurrent recorder per level, so such a
/// snapshot can hold a partially recorded execution. They are exact when no
/// sweep was in flight. See `OrderBook`'s "Level statistics are advisory
/// under concurrent takers" section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    /// The symbol or identifier for this order book
    pub symbol: String,

    /// Timestamp when the snapshot was created (milliseconds since epoch)
    pub timestamp: u64,

    /// Snapshot of bid price levels. Per-level execution statistics are
    /// advisory under concurrent takers (see the type docs).
    pub bids: Vec<PriceLevelSnapshot>,

    /// Snapshot of ask price levels. Per-level execution statistics are
    /// advisory under concurrent takers (see the type docs).
    pub asks: Vec<PriceLevelSnapshot>,

    /// Pending trailing stops (#286), in admission (trigger-priority) order.
    ///
    /// Held off book: they are not part of `bids` / `asks` and never count
    /// as depth. Each carries its current stop price (`price`) and
    /// watermark (`last_reference_price`). Not limited by the snapshot
    /// depth. Always empty without the `special_orders` feature.
    /// `#[serde(default)]`: absent from payloads written before format
    /// version 5.
    #[serde(default)]
    pub pending_stops: Vec<OrderType<()>>,

    /// The book's last trade price, in price ticks, or `None` when it has
    /// not traded (#286). Pending stops trail and trigger on it, and a
    /// restore installs it, so the restored book evaluates its stops (and
    /// last-trade price references) exactly like the original.
    /// `#[serde(default)]`: absent from payloads written before format
    /// version 5.
    #[serde(default)]
    pub last_trade_price: Option<u128>,
}

impl OrderBookSnapshot {
    /// Recomputes aggregate values for all included price levels.
    ///
    /// Stops at the first level whose aggregates cannot be recomputed;
    /// levels before it are refreshed, the failing level and the ones
    /// after it keep their previous aggregates. Every caller that
    /// checksums or restores the snapshot treats the error as fatal, so a
    /// partially refreshed snapshot is never checksummed or installed.
    ///
    /// # Errors
    ///
    /// [`OrderBookError::PriceLevelError`] when a level's aggregates do not
    /// fit their integer types (for example a visible or hidden quantity
    /// sum that overflows `u64`). Previously the error was ignored and the
    /// level kept stale aggregates that were then checksummed (#250).
    pub fn refresh_aggregates(&mut self) -> Result<(), OrderBookError> {
        for level in self.bids.iter_mut().chain(self.asks.iter_mut()) {
            level
                .refresh_aggregates()
                .map_err(OrderBookError::PriceLevelError)?;
        }
        Ok(())
    }

    /// Get the best bid price and quantity
    pub fn best_bid(&self) -> Option<(u128, u64)> {
        let bids = self
            .bids
            .iter()
            .map(|level| (level.price().as_u128(), level.visible_quantity().as_u64()))
            .max_by_key(|&(price, _)| price);
        trace!("best_bid: {:?}", bids);
        bids
    }

    /// Get the best ask price and quantity
    pub fn best_ask(&self) -> Option<(u128, u64)> {
        let ask = self
            .asks
            .iter()
            .map(|level| (level.price().as_u128(), level.visible_quantity().as_u64()))
            .min_by_key(|&(price, _)| price);
        trace!("best_ask: {:?}", ask);
        ask
    }

    /// Get the mid price (average of best bid and best ask)
    pub fn mid_price(&self) -> Option<f64> {
        let mid_price = match (self.best_bid(), self.best_ask()) {
            (Some((bid_price, _)), Some((ask_price, _))) => {
                Some((bid_price as f64 + ask_price as f64) / 2.0)
            }
            _ => None,
        };
        trace!("mid_price: {:?}", mid_price);
        mid_price
    }

    /// Get the spread (best ask - best bid), in price ticks.
    ///
    /// Returns `None` when either side is empty or when the snapshot is
    /// crossed (best ask below best bid): such a snapshot is malformed —
    /// restore rejects it with [`OrderBookError::SnapshotCrossed`] — and its
    /// negative difference is not a spread. It used to be clamped to `0`
    /// (#250). A locked snapshot (equal prices) returns `Some(0)`.
    pub fn spread(&self) -> Option<u128> {
        let spread = match (self.best_bid(), self.best_ask()) {
            (Some((bid_price, _)), Some((ask_price, _))) => ask_price.checked_sub(bid_price),
            _ => None,
        };
        trace!("spread: {:?}", spread);
        spread
    }

    /// Calculate the total volume (`visible + hidden`, in quantity units) on
    /// the bid side.
    ///
    /// # Errors
    /// - [`OrderBookError::PriceLevelError`] when a level's
    ///   `visible + hidden` total overflows `u64`.
    /// - [`OrderBookError::ArithmeticOverflow`] when the side total
    ///   overflows `u64`.
    pub fn total_bid_volume(&self) -> Result<u64, OrderBookError> {
        let volume = total_volume(&self.bids, "snapshot bid volume")?;
        trace!("total_bid_volume: {:?}", volume);
        Ok(volume)
    }

    /// Calculate the total volume (`visible + hidden`, in quantity units) on
    /// the ask side.
    ///
    /// # Errors
    /// - [`OrderBookError::PriceLevelError`] when a level's
    ///   `visible + hidden` total overflows `u64`.
    /// - [`OrderBookError::ArithmeticOverflow`] when the side total
    ///   overflows `u64`.
    pub fn total_ask_volume(&self) -> Result<u64, OrderBookError> {
        let volume = total_volume(&self.asks, "snapshot ask volume")?;
        trace!("total_ask_volume: {:?}", volume);
        Ok(volume)
    }

    /// Calculate the total value on the bid side (`price * quantity` summed
    /// over the levels, in price units times quantity units).
    ///
    /// # Errors
    /// - [`OrderBookError::PriceLevelError`] when a level's
    ///   `visible + hidden` total overflows `u64`.
    /// - [`OrderBookError::ArithmeticOverflow`] when a level's notional or
    ///   the side total overflows `u128`.
    pub fn total_bid_value(&self) -> Result<u128, OrderBookError> {
        let value = total_value(&self.bids, "snapshot bid value")?;
        trace!("total_bid_value: {:?}", value);
        Ok(value)
    }

    /// Calculate the total value on the ask side (`price * quantity` summed
    /// over the levels, in price units times quantity units).
    ///
    /// # Errors
    /// - [`OrderBookError::PriceLevelError`] when a level's
    ///   `visible + hidden` total overflows `u64`.
    /// - [`OrderBookError::ArithmeticOverflow`] when a level's notional or
    ///   the side total overflows `u128`.
    pub fn total_ask_value(&self) -> Result<u128, OrderBookError> {
        let value = total_value(&self.asks, "snapshot ask value")?;
        trace!("total_ask_value: {:?}", value);
        Ok(value)
    }
}

/// Total quantity (`visible + hidden`) of a level snapshot, with the
/// level's own overflow surfaced as a typed error (#245).
#[inline]
fn snapshot_level_total(level: &PriceLevelSnapshot) -> Result<u64, OrderBookError> {
    Ok(level.total_quantity()?.as_u64())
}

/// Checked `u64` sum of the level totals of `levels`.
fn total_volume(
    levels: &[PriceLevelSnapshot],
    operation: &'static str,
) -> Result<u64, OrderBookError> {
    levels.iter().try_fold(0u64, |acc, level| {
        checked_depth_add(acc, snapshot_level_total(level)?, operation)
    })
}

/// Checked `u128` sum of `price * level_total` over `levels`.
fn total_value(
    levels: &[PriceLevelSnapshot],
    operation: &'static str,
) -> Result<u128, OrderBookError> {
    levels.iter().try_fold(0u128, |acc, level| {
        checked_notional_add(
            acc,
            level.price().as_u128(),
            snapshot_level_total(level)?,
            operation,
        )
    })
}

/// Format version used for checksum-enabled order book snapshots.
///
/// Bumped to `5` for off-book trailing stops (#286): the snapshot carries
/// the pending stops (`OrderBookSnapshot::pending_stops`) and the last
/// trade price (`OrderBookSnapshot::last_trade_price`), and the checksum of
/// a version-5 package covers both. Packages of versions `2..=4` are
/// verified with their original checksum (over symbol, timestamp and
/// levels) and must not carry either field.
///
/// Bumped to `4` for pricelevel 0.10: the embedded level statistics'
/// `value_executed` is a `u128` (was `u64`), so a payload may carry a value
/// above `u64::MAX` that a pre-0.14 reader (pricelevel 0.9) cannot decode.
/// Newly written packages are stamped `4` so an old reader fails fast on
/// the version check whenever the payload itself still decodes; a payload
/// whose `value_executed` exceeds `u64::MAX` fails in deserialization
/// instead, because decoding runs before the version check.
///
/// Bumped to `3` for the pricelevel 0.9 statistics schema: embedded
/// level statistics may carry a `stats_degraded` field (serialized
/// only when `true`), which a pricelevel 0.8 reader rejects with
/// `unknown field` (#206).
///
/// Reads accept [`ORDERBOOK_SNAPSHOT_MIN_READ_VERSION`]`..=`this:
/// `version: 2` payloads (written by 0.11 / pricelevel 0.8) and
/// `version: 3` payloads (written by 0.12 / 0.13 / pricelevel 0.9) decode
/// under pricelevel 0.10 unchanged (a `u64` `value_executed` widens to
/// `u128` losslessly) and keep their original checksum.
/// `version: 1` payloads (no `engine_seq`) remain rejected by
/// [`OrderBookSnapshotPackage::validate`] with the existing
/// `Unsupported snapshot version` error — that format break is
/// intentional, with no special-case migration path.
pub const ORDERBOOK_SNAPSHOT_FORMAT_VERSION: u32 = 5;

/// Length of a hex-encoded SHA-256 digest (32 bytes, two hex digits each).
const SHA256_HEX_LEN: usize = 64;

/// Oldest package format version [`OrderBookSnapshotPackage::validate`]
/// still accepts on read. Version `2` packages predate the pricelevel
/// 0.9 `stats_degraded` statistics field and restore cleanly — the field
/// simply defaults to `false`.
pub const ORDERBOOK_SNAPSHOT_MIN_READ_VERSION: u32 = 2;

/// First package version whose snapshot carries pending stops and the last
/// trade price, and whose checksum covers them (#286).
const STOP_ORDERS_SNAPSHOT_VERSION: u32 = 5;

/// The snapshot fields packages of versions `2..=4` checksummed, in their
/// serialized order. Serializes byte-identically to the pre-#286
/// `OrderBookSnapshot`, so those packages keep verifying.
#[derive(Serialize)]
struct LegacySnapshotView<'a> {
    /// See [`OrderBookSnapshot::symbol`].
    symbol: &'a str,
    /// See [`OrderBookSnapshot::timestamp`].
    timestamp: u64,
    /// See [`OrderBookSnapshot::bids`].
    bids: &'a [PriceLevelSnapshot],
    /// See [`OrderBookSnapshot::asks`].
    asks: &'a [PriceLevelSnapshot],
}

impl<'a> LegacySnapshotView<'a> {
    /// The legacy view of `snapshot`.
    fn of(snapshot: &'a OrderBookSnapshot) -> Self {
        Self {
            symbol: &snapshot.symbol,
            timestamp: snapshot.timestamp,
            bids: &snapshot.bids,
            asks: &snapshot.asks,
        }
    }
}

/// Wrapper that provides checksum validation for `OrderBookSnapshot` instances.
///
/// In addition to the snapshot payload and checksum, this package carries
/// the order book's configuration fields (`fee_schedule`, `stp_mode`,
/// `tick_size`, `lot_size`, `min_order_size`, `max_order_size`) so that
/// [`OrderBook::restore_from_snapshot_package`](super::book::OrderBook::restore_from_snapshot_package)
/// can fully reconstruct the book's state, including validation rules and
/// fee settings.
///
/// All configuration fields use `#[serde(default)]` for backward
/// compatibility — snapshots created before this version will deserialize
/// with default values (`None` / `STPMode::None`).
///
/// The checksum certifies the payload's integrity, not the coherence of the
/// level statistics it carries: a package captured while shared-gate takers
/// were sweeping a level may hold advisory execution statistics for it (see
/// [`OrderBookSnapshot`]), and restore installs them verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshotPackage {
    /// Version of the snapshot schema for forward compatibility.
    pub version: u32,
    /// Snapshot payload.
    pub snapshot: OrderBookSnapshot,
    /// Hex-encoded checksum of the serialized snapshot.
    pub checksum: String,

    /// Fee schedule active at the time of the snapshot.
    #[serde(default)]
    pub fee_schedule: Option<FeeSchedule>,

    /// Self-trade prevention mode active at the time of the snapshot.
    #[serde(default)]
    pub stp_mode: STPMode,

    /// Tick size (minimum price increment) active at the time of the snapshot.
    #[serde(default)]
    pub tick_size: Option<u128>,

    /// Lot size (minimum quantity increment) active at the time of the snapshot.
    #[serde(default)]
    pub lot_size: Option<u64>,

    /// Minimum order size active at the time of the snapshot.
    #[serde(default)]
    pub min_order_size: Option<u64>,

    /// Maximum order size active at the time of the snapshot.
    #[serde(default)]
    pub max_order_size: Option<u64>,

    /// Engine sequence at the time of snapshot. Restored as the new
    /// counter value on
    /// [`OrderBook::restore_from_snapshot_package`](super::book::OrderBook::restore_from_snapshot_package)
    /// so monotonicity resumes from this point on the restored book.
    ///
    /// `#[serde(default)]` lets `version: 2` payloads that omit the field
    /// (e.g. older code paths constructing the package via the legacy
    /// [`OrderBookSnapshotPackage::new`]) deserialize cleanly with `0`.
    /// Payloads with `version: 1` are rejected by
    /// [`OrderBookSnapshotPackage::validate`].
    #[serde(default)]
    pub engine_seq: u64,

    /// Operational state of the kill switch at the time of snapshot.
    /// Restored as-is by
    /// [`OrderBook::restore_from_snapshot_package`](super::book::OrderBook::restore_from_snapshot_package)
    /// so disaster-recovered books resume in the same operational mode
    /// they were halted in.
    ///
    /// `#[serde(default)]` keeps the format version at `2`: payloads
    /// written before this field existed deserialize with `false`,
    /// matching the previous (implicit) behaviour where a restored book
    /// always came back disengaged.
    #[serde(default)]
    pub kill_switch_engaged: bool,

    /// Risk configuration active at the time of snapshot. `None` means
    /// no risk gating. Counters and per-order risk state are rebuilt
    /// post-restore by walking the snapshot's resting orders.
    ///
    /// `#[serde(default)]` keeps the format version at `2`: payloads
    /// written before this field existed deserialize with `None`, which
    /// matches the previous (implicit) behaviour where a restored book
    /// always came back without any risk gates engaged.
    #[serde(default)]
    pub risk_config: Option<RiskConfig>,

    /// Scheduled market-close timestamp (milliseconds since epoch) active at the
    /// time of snapshot — drives DAY / GTD expiry. `0` together with
    /// `has_market_close = false` means no close is configured. Restored by
    /// [`OrderBook::restore_from_snapshot_package`](super::book::OrderBook::restore_from_snapshot_package)
    /// so a recovered book keeps its session schedule (#100).
    ///
    /// `#[serde(default)]` keeps the format version at `2`: payloads written before
    /// these fields existed deserialize with `0` / `false`, matching the previous
    /// behaviour where a restored book always came back with no market close.
    #[serde(default)]
    pub market_close_timestamp: u64,

    /// Whether a market close was configured at the time of snapshot. See
    /// [`Self::market_close_timestamp`].
    #[serde(default)]
    pub has_market_close: bool,
}

impl OrderBookSnapshotPackage {
    /// Creates a new snapshot package computing the checksum of the snapshot contents.
    ///
    /// The level aggregates are refreshed first, and the checksum covers the
    /// refreshed snapshot.
    ///
    /// # Errors
    ///
    /// [`OrderBookError::PriceLevelError`] when a level's aggregates cannot
    /// be recomputed (see [`OrderBookSnapshot::refresh_aggregates`]; since
    /// #250 this is propagated instead of checksumming stale aggregates),
    /// and [`OrderBookError::SerializationError`] when the checksum payload
    /// cannot be encoded.
    pub fn new(mut snapshot: OrderBookSnapshot) -> Result<Self, OrderBookError> {
        snapshot.refresh_aggregates()?;

        let checksum = Self::compute_checksum(ORDERBOOK_SNAPSHOT_FORMAT_VERSION, &snapshot)?;

        Ok(Self {
            version: ORDERBOOK_SNAPSHOT_FORMAT_VERSION,
            snapshot,
            checksum,
            fee_schedule: None,
            stp_mode: STPMode::None,
            tick_size: None,
            lot_size: None,
            min_order_size: None,
            max_order_size: None,
            engine_seq: 0,
            kill_switch_engaged: false,
            risk_config: None,
            market_close_timestamp: 0,
            has_market_close: false,
        })
    }

    /// Serializes the package to JSON.
    pub fn to_json(&self) -> Result<String, OrderBookError> {
        serde_json::to_string(self).map_err(|error| OrderBookError::SerializationError {
            message: error.to_string(),
        })
    }

    /// Deserializes the package from JSON.
    pub fn from_json(data: &str) -> Result<Self, OrderBookError> {
        serde_json::from_str(data).map_err(|error| OrderBookError::DeserializationError {
            message: error.to_string(),
        })
    }

    /// Validates the checksum and version.
    ///
    /// Accepts package versions
    /// [`ORDERBOOK_SNAPSHOT_MIN_READ_VERSION`]`..=`[`ORDERBOOK_SNAPSHOT_FORMAT_VERSION`]
    /// and rejects anything older or newer with a typed error. The
    /// checksum covers the snapshot payload only (not the version field
    /// or the configuration fields): for version `5` the whole snapshot,
    /// pending stops and last trade price included; for versions `2..=4`
    /// the payload those versions wrote (symbol, timestamp and levels), so
    /// their packages keep their original checksum. A package below
    /// version `5` that carries pending stops or a last trade price is
    /// rejected: those fields did not exist then and its checksum would
    /// not cover them.
    #[must_use = "an unchecked snapshot package must not be restored"]
    pub fn validate(&self) -> Result<(), OrderBookError> {
        if self.version < ORDERBOOK_SNAPSHOT_MIN_READ_VERSION
            || self.version > ORDERBOOK_SNAPSHOT_FORMAT_VERSION
        {
            return Err(OrderBookError::InvalidOperation {
                message: format!(
                    "Unsupported snapshot version: {} (supported {}..={})",
                    self.version,
                    ORDERBOOK_SNAPSHOT_MIN_READ_VERSION,
                    ORDERBOOK_SNAPSHOT_FORMAT_VERSION
                ),
            });
        }

        if self.version < STOP_ORDERS_SNAPSHOT_VERSION
            && (!self.snapshot.pending_stops.is_empty() || self.snapshot.last_trade_price.is_some())
        {
            return Err(OrderBookError::InvalidOperation {
                message: format!(
                    "snapshot version {} cannot carry pending stops or a last trade price (introduced in version {})",
                    self.version, STOP_ORDERS_SNAPSHOT_VERSION
                ),
            });
        }

        let computed = Self::compute_checksum(self.version, &self.snapshot)?;
        if computed != self.checksum {
            return Err(OrderBookError::ChecksumMismatch {
                expected: self.checksum.clone(),
                actual: computed,
            });
        }

        Ok(())
    }

    /// Test-only: stamps the package with `version` and recomputes its
    /// checksum the way that version computes it, so a test can build a
    /// valid legacy-labelled package from a current book.
    ///
    /// # Errors
    ///
    /// [`OrderBookError::SerializationError`] when the payload cannot be
    /// encoded.
    #[cfg(test)]
    pub(crate) fn relabelled_for_test(mut self, version: u32) -> Result<Self, OrderBookError> {
        self.version = version;
        self.checksum = Self::compute_checksum(version, &self.snapshot)?;
        Ok(self)
    }

    /// Consumes the package and returns the validated snapshot.
    #[must_use = "the validated snapshot (or the validation error) must be handled"]
    pub fn into_snapshot(self) -> Result<OrderBookSnapshot, OrderBookError> {
        self.validate()?;
        Ok(self.snapshot)
    }

    /// SHA-256 (hex) of the checksummed payload of a `version` package:
    /// the whole snapshot from version 5 on, the pre-#286 fields
    /// ([`LegacySnapshotView`]) before it.
    fn compute_checksum(
        version: u32,
        snapshot: &OrderBookSnapshot,
    ) -> Result<String, OrderBookError> {
        let payload = if version >= STOP_ORDERS_SNAPSHOT_VERSION {
            serde_json::to_vec(snapshot)
        } else {
            serde_json::to_vec(&LegacySnapshotView::of(snapshot))
        }
        .map_err(|error| OrderBookError::SerializationError {
            message: error.to_string(),
        })?;

        let mut hasher = Sha256::new();
        hasher.update(payload);

        let checksum_bytes = hasher.finalize();
        let mut out = String::with_capacity(SHA256_HEX_LEN);
        for byte in checksum_bytes.iter() {
            use std::fmt::Write;
            write!(&mut out, "{byte:02x}").map_err(|error| OrderBookError::SerializationError {
                message: error.to_string(),
            })?;
        }
        Ok(out)
    }
}

bitflags! {
    /// Flags for selecting which metrics to calculate in enriched snapshots
    ///
    /// Use these flags to optimize performance by calculating only the metrics
    /// you need. Multiple flags can be combined using bitwise OR.
    ///
    /// # Examples
    /// ```
    /// use orderbook_rs::MetricFlags;
    ///
    /// // Calculate only mid price and spread
    /// let flags = MetricFlags::MID_PRICE | MetricFlags::SPREAD;
    ///
    /// // Calculate all metrics
    /// let flags = MetricFlags::ALL;
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct MetricFlags: u32 {
        /// Calculate mid price (average of best bid and ask)
        const MID_PRICE = 1 << 0;

        /// Calculate spread in basis points
        const SPREAD = 1 << 1;

        /// Calculate total depth on each side
        const DEPTH = 1 << 2;

        /// Calculate VWAP for top N levels
        const VWAP = 1 << 3;

        /// Calculate order book imbalance
        const IMBALANCE = 1 << 4;

        /// Calculate all metrics
        const ALL = Self::MID_PRICE.bits() | Self::SPREAD.bits()
                  | Self::DEPTH.bits() | Self::VWAP.bits() | Self::IMBALANCE.bits();
    }
}

/// An enriched snapshot with pre-calculated metrics
///
/// This provides better performance than creating a snapshot and calculating
/// metrics separately, as it computes everything in a single pass through the data.
/// This is particularly beneficial for high-frequency trading applications.
///
/// # Performance
/// - Single pass through data vs multiple passes
/// - Better cache locality
/// - Optional metric selection for optimization
///
/// # Examples
/// ```
/// use orderbook_rs::OrderBook;
/// use pricelevel::{Id, Side, TimeInForce};
/// use uuid::Uuid;
///
/// let book = OrderBook::<()>::new("BTC/USD");
/// let _ = book.add_limit_order(Id::from_uuid(Uuid::new_v4()), 100, 10, Side::Buy, TimeInForce::Gtc, None);
/// let _ = book.add_limit_order(Id::from_uuid(Uuid::new_v4()), 101, 10, Side::Sell, TimeInForce::Gtc, None);
///
/// let snapshot = book.enriched_snapshot(10)?;
///
/// if let Some(mid) = snapshot.mid_price {
///     println!("Mid price: {}", mid);
/// }
/// if let Some(spread) = snapshot.spread_bps {
///     println!("Spread: {} bps", spread);
/// }
/// # Ok::<(), orderbook_rs::OrderBookError>(())
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichedSnapshot {
    /// The symbol or identifier for this order book
    pub symbol: String,

    /// Timestamp when the snapshot was created (milliseconds since epoch)
    pub timestamp: u64,

    /// Snapshot of bid price levels. Per-level execution statistics are
    /// advisory under concurrent takers, as in [`OrderBookSnapshot`]; the
    /// metrics below use prices and quantities only.
    pub bids: Vec<PriceLevelSnapshot>,

    /// Snapshot of ask price levels. Per-level execution statistics are
    /// advisory under concurrent takers, as in [`OrderBookSnapshot`].
    pub asks: Vec<PriceLevelSnapshot>,

    /// Mid price (average of best bid and best ask)
    pub mid_price: Option<f64>,

    /// Spread in basis points
    pub spread_bps: Option<f64>,

    /// Total depth on bid side (in units)
    pub bid_depth_total: u64,

    /// Total depth on ask side (in units)
    pub ask_depth_total: u64,

    /// Order book imbalance (-1.0 to 1.0)
    pub order_book_imbalance: f64,

    /// VWAP for top N bid levels
    pub vwap_bid: Option<f64>,

    /// VWAP for top N ask levels
    pub vwap_ask: Option<f64>,
}

impl EnrichedSnapshot {
    /// Creates a new enriched snapshot with all metrics calculated
    ///
    /// # Arguments
    /// - `symbol`: Symbol identifier
    /// - `timestamp`: Timestamp in milliseconds
    /// - `bids`: Bid price levels
    /// - `asks`: Ask price levels
    /// - `vwap_levels`: Number of levels to use for VWAP calculation
    /// - `imbalance_levels`: Number of levels to use for imbalance calculation
    ///
    /// # Errors
    /// Same as [`Self::with_metrics`] with [`MetricFlags::ALL`].
    pub fn new(
        symbol: String,
        timestamp: u64,
        bids: Vec<PriceLevelSnapshot>,
        asks: Vec<PriceLevelSnapshot>,
        vwap_levels: usize,
        imbalance_levels: usize,
    ) -> Result<Self, OrderBookError> {
        Self::with_metrics(
            symbol,
            timestamp,
            bids,
            asks,
            vwap_levels,
            imbalance_levels,
            MetricFlags::ALL,
        )
    }

    /// Creates a new enriched snapshot with custom metric selection
    ///
    /// # Arguments
    /// - `symbol`: Symbol identifier
    /// - `timestamp`: Timestamp in milliseconds
    /// - `bids`: Bid price levels
    /// - `asks`: Ask price levels
    /// - `vwap_levels`: Number of levels to use for VWAP calculation
    /// - `imbalance_levels`: Number of levels to use for imbalance calculation
    /// - `flags`: Metrics to calculate
    ///
    /// # Errors
    /// Only the selected metrics are computed, so only they can fail:
    /// - [`OrderBookError::PriceLevelError`] when a level's
    ///   `visible + hidden` total overflows `u64` (`DEPTH`, `VWAP`,
    ///   `IMBALANCE`).
    /// - [`OrderBookError::ArithmeticOverflow`] when a side depth (`u64`),
    ///   a VWAP notional (`u128`) or the imbalance total (`u64`) overflows.
    pub fn with_metrics(
        symbol: String,
        timestamp: u64,
        bids: Vec<PriceLevelSnapshot>,
        asks: Vec<PriceLevelSnapshot>,
        vwap_levels: usize,
        imbalance_levels: usize,
        flags: MetricFlags,
    ) -> Result<Self, OrderBookError> {
        // Calculate mid price if needed
        let mid_price = if flags.contains(MetricFlags::MID_PRICE) {
            Self::calculate_mid_price(&bids, &asks)
        } else {
            None
        };

        // Calculate spread if needed
        let spread_bps = if flags.contains(MetricFlags::SPREAD) {
            Self::calculate_spread_bps(&bids, &asks)
        } else {
            None
        };

        // Calculate depths if needed
        let (bid_depth_total, ask_depth_total) = if flags.contains(MetricFlags::DEPTH) {
            (
                Self::calculate_total_depth(&bids)?,
                Self::calculate_total_depth(&asks)?,
            )
        } else {
            (0, 0)
        };

        // Calculate VWAP if needed
        let (vwap_bid, vwap_ask) = if flags.contains(MetricFlags::VWAP) {
            (
                Self::calculate_vwap(&bids, vwap_levels)?,
                Self::calculate_vwap(&asks, vwap_levels)?,
            )
        } else {
            (None, None)
        };

        // Calculate imbalance if needed
        let order_book_imbalance = if flags.contains(MetricFlags::IMBALANCE) {
            Self::calculate_imbalance(&bids, &asks, imbalance_levels)?
        } else {
            0.0
        };

        Ok(Self {
            symbol,
            timestamp,
            bids,
            asks,
            mid_price,
            spread_bps,
            bid_depth_total,
            ask_depth_total,
            order_book_imbalance,
            vwap_bid,
            vwap_ask,
        })
    }

    fn calculate_mid_price(
        bids: &[PriceLevelSnapshot],
        asks: &[PriceLevelSnapshot],
    ) -> Option<f64> {
        let best_bid = bids.first().map(|l| l.price())?;
        let best_ask = asks.first().map(|l| l.price())?;
        Some((best_bid.to_f64_lossy() + best_ask.to_f64_lossy()) / 2.0)
    }

    fn calculate_spread_bps(
        bids: &[PriceLevelSnapshot],
        asks: &[PriceLevelSnapshot],
    ) -> Option<f64> {
        let best_bid = bids.first().map(|l| l.price())?.to_f64_lossy();
        let best_ask = asks.first().map(|l| l.price())?.to_f64_lossy();
        let mid_price = (best_bid + best_ask) / 2.0;

        if mid_price == 0.0 {
            return None;
        }

        let spread = best_ask - best_bid;
        Some((spread / mid_price) * 10000.0)
    }

    fn calculate_total_depth(levels: &[PriceLevelSnapshot]) -> Result<u64, OrderBookError> {
        total_volume(levels, "enriched snapshot depth")
    }

    fn calculate_vwap(
        levels: &[PriceLevelSnapshot],
        max_levels: usize,
    ) -> Result<Option<f64>, OrderBookError> {
        let levels_to_use = levels.iter().take(max_levels);

        let mut total_value = 0u128;
        let mut total_quantity = 0u64;

        for level in levels_to_use {
            let quantity = snapshot_level_total(level)?;
            if quantity > 0 {
                total_value = checked_notional_add(
                    total_value,
                    level.price().as_u128(),
                    quantity,
                    "enriched snapshot vwap notional",
                )?;
                total_quantity =
                    checked_depth_add(total_quantity, quantity, "enriched snapshot vwap quantity")?;
            }
        }

        if total_quantity == 0 {
            Ok(None)
        } else {
            Ok(Some(total_value as f64 / total_quantity as f64))
        }
    }

    fn calculate_imbalance(
        bids: &[PriceLevelSnapshot],
        asks: &[PriceLevelSnapshot],
        max_levels: usize,
    ) -> Result<f64, OrderBookError> {
        let bid_volume = bids
            .get(..max_levels.min(bids.len()))
            .map_or(Ok(0), |top| {
                total_volume(top, "enriched snapshot imbalance")
            })?;
        let ask_volume = asks
            .get(..max_levels.min(asks.len()))
            .map_or(Ok(0), |top| {
                total_volume(top, "enriched snapshot imbalance")
            })?;

        let total = checked_depth_add(bid_volume, ask_volume, "enriched snapshot imbalance")?;

        if total == 0 {
            Ok(0.0)
        } else {
            Ok((bid_volume as f64 - ask_volume as f64) / total as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pricelevel::PriceLevelSnapshot;

    fn level(visible: u64) -> PriceLevelSnapshot {
        serde_json::from_value(serde_json::json!({
            "price": 100,
            "visible_quantity": visible,
            "hidden_quantity": 0,
            "order_count": 1,
            "orders": []
        }))
        .expect("valid PriceLevelSnapshot JSON")
    }

    #[test]
    fn test_calculate_imbalance_extreme_volume_returns_overflow_error() {
        // Two levels whose volumes overflow u64 are reported (#245), not
        // saturated, panicked on in debug or wrapped in release.
        let big = u64::MAX / 2 + 1;
        let bids = vec![level(big), level(big)];
        let asks = vec![level(1)];
        let err = EnrichedSnapshot::calculate_imbalance(&bids, &asks, 10)
            .expect_err("u64 overflow must be reported");
        assert!(matches!(err, OrderBookError::ArithmeticOverflow { .. }));
    }

    #[test]
    fn test_calculate_imbalance_bid_plus_ask_overflow_returns_error() {
        let bids = vec![level(u64::MAX)];
        let asks = vec![level(1)];
        let err = EnrichedSnapshot::calculate_imbalance(&bids, &asks, 10)
            .expect_err("bid + ask overflow must be reported");
        assert!(matches!(err, OrderBookError::ArithmeticOverflow { .. }));
    }

    #[test]
    fn test_calculate_imbalance_realistic() {
        let bids = vec![level(60)];
        let asks = vec![level(40)];
        let imbalance = EnrichedSnapshot::calculate_imbalance(&bids, &asks, 10).expect("imbalance");
        assert!(
            (imbalance - 0.2).abs() < 1e-9,
            "(60 - 40) / 100 = 0.2, got {imbalance}"
        );
    }

    #[test]
    fn test_calculate_imbalance_respects_level_cap() {
        let bids = vec![level(60), level(u64::MAX)];
        let asks = vec![level(40)];
        // Only the top level per side is read, so the huge second bid level
        // never enters the sum.
        let imbalance = EnrichedSnapshot::calculate_imbalance(&bids, &asks, 1).expect("imbalance");
        assert!((imbalance - 0.2).abs() < 1e-9);
    }

    fn level_at(price: u128, visible: u64, hidden: u64) -> PriceLevelSnapshot {
        // `from_str` (not `json!`): a `serde_json::Value` cannot carry a
        // `u128` above `u64::MAX`, the text parser can.
        serde_json::from_str(&format!(
            r#"{{"price":{price},"visible_quantity":{visible},"hidden_quantity":{hidden},"order_count":1,"orders":[]}}"#
        ))
        .expect("valid PriceLevelSnapshot JSON")
    }

    fn snapshot(bids: Vec<PriceLevelSnapshot>, asks: Vec<PriceLevelSnapshot>) -> OrderBookSnapshot {
        OrderBookSnapshot {
            symbol: "TEST".to_string(),
            timestamp: 0,
            bids,
            asks,
            pending_stops: Vec::new(),
            last_trade_price: None,
        }
    }

    #[test]
    fn test_snapshot_totals_realistic() {
        let snap = snapshot(
            vec![level_at(100, 10, 5), level_at(99, 20, 0)],
            vec![level_at(101, 7, 0)],
        );
        assert_eq!(snap.total_bid_volume().expect("volume"), 35);
        assert_eq!(snap.total_ask_volume().expect("volume"), 7);
        assert_eq!(snap.total_bid_value().expect("value"), 100 * 15 + 99 * 20);
        assert_eq!(snap.total_ask_value().expect("value"), 101 * 7);
    }

    #[test]
    fn test_snapshot_total_volume_u64_overflow_returns_error() {
        let snap = snapshot(
            vec![level_at(100, u64::MAX, 0), level_at(99, 1, 0)],
            vec![level_at(101, u64::MAX, 0), level_at(102, 1, 0)],
        );
        assert!(matches!(
            snap.total_bid_volume(),
            Err(OrderBookError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(
            snap.total_ask_volume(),
            Err(OrderBookError::ArithmeticOverflow { .. })
        ));
    }

    #[test]
    fn test_snapshot_level_total_overflow_propagates_price_level_error() {
        // visible + hidden of a single level exceeds u64.
        let snap = snapshot(
            vec![level_at(100, u64::MAX, 1)],
            vec![level_at(101, u64::MAX, 1)],
        );
        assert!(matches!(
            snap.total_bid_volume(),
            Err(OrderBookError::PriceLevelError(_))
        ));
        assert!(matches!(
            snap.total_ask_value(),
            Err(OrderBookError::PriceLevelError(_))
        ));
    }

    #[test]
    fn test_snapshot_total_value_u128_overflow_returns_error() {
        // u128::MAX * 2 overflows the product.
        let snap = snapshot(
            vec![level_at(u128::MAX, 2, 0)],
            vec![level_at(u128::MAX, 2, 0)],
        );
        assert!(matches!(
            snap.total_bid_value(),
            Err(OrderBookError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(
            snap.total_ask_value(),
            Err(OrderBookError::ArithmeticOverflow { .. })
        ));
        // The sum overflows even when each product fits.
        let snap = snapshot(
            vec![level_at(u128::MAX, 1, 0), level_at(1, 1, 0)],
            Vec::new(),
        );
        assert!(matches!(
            snap.total_bid_value(),
            Err(OrderBookError::ArithmeticOverflow { .. })
        ));
    }

    #[test]
    fn test_enriched_metrics_overflow_returns_error_only_when_selected() {
        let bids = vec![level_at(u128::MAX, 2, 0)];
        let asks = vec![level_at(u128::MAX, 1, 0)];
        // VWAP notional u128::MAX * 2 overflows.
        let err = EnrichedSnapshot::with_metrics(
            "T".to_string(),
            0,
            bids.clone(),
            asks.clone(),
            10,
            10,
            MetricFlags::VWAP,
        )
        .expect_err("vwap overflow must be reported");
        assert!(matches!(err, OrderBookError::ArithmeticOverflow { .. }));
        // Not selected: no error.
        let ok = EnrichedSnapshot::with_metrics(
            "T".to_string(),
            0,
            bids.clone(),
            asks.clone(),
            10,
            10,
            MetricFlags::DEPTH | MetricFlags::IMBALANCE,
        )
        .expect("depth and imbalance fit");
        assert_eq!(ok.bid_depth_total, 2);
        assert_eq!(ok.ask_depth_total, 1);
        assert!(ok.vwap_bid.is_none());

        // Depth overflow through `new` (ALL metrics).
        let err = EnrichedSnapshot::new(
            "T".to_string(),
            0,
            vec![level_at(100, u64::MAX, 0), level_at(99, 1, 0)],
            Vec::new(),
            10,
            10,
        )
        .expect_err("depth overflow must be reported");
        assert!(matches!(err, OrderBookError::ArithmeticOverflow { .. }));
    }
}
