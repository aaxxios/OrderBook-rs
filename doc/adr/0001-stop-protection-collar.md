# ADR 0001: Protection collar for elected stop orders

- Status: accepted (0.15.0, #302)
- Code: `src/orderbook/stop_protection.rs`, `src/orderbook/stop_orders.rs`
  (`execute_stop_child`), `src/orderbook/snapshot.rs`,
  `src/orderbook/sequencer/replay.rs`

## Context

Since 0.14.0 (#286) an elected trailing stop executes as an unpriced
immediate-or-cancel market order. The book applies no price band to market
orders and a cascade is bounded only by the number of pending stops, so in
a thin book one print can elect a chain of stops that sweeps one side down
to its last level. Venues bound this with a protection collar (for example
CME protection points: the triggered order becomes a limit at trigger price
plus or minus N ticks).

## Decision

- A per-book `Option<StopProtection>`, set with
  `OrderBook::set_stop_protection`. `None` (the default) keeps the 0.14
  market child.
- The collar is an absolute offset in price units (`NonZeroU128`, the same
  units as order prices). With a tick size it must be a multiple of it
  (`InvalidTickSize`); zero is refused (`InvalidStopProtection`), since
  "unset" is `None`.
- Reference price: the stop's trailed stop price at election, not the
  electing print. Sell stop child: IOC limit at `stop - collar`; buy stop
  child: IOC limit at `stop + collar`. A band past `0` / `u128::MAX` is
  clamped (unbounded on that side).
- Remainder: cancelled. An elected stop never rests liquidity. An empty
  band ends the stop `Cancelled { filled_quantity: 0, reason:
  InsufficientLiquidity }`.
- The type is compiled in every build so snapshots and replay configs have
  one shape; it only acts under `special_orders`.
- Persistence: `OrderBookSnapshotPackage::stop_protection`
  (`#[serde(default)]`) with a format bump to 6 so a 0.14 reader refuses
  the package instead of dropping the collar. The checksummed payload is
  unchanged. `ReplayBookConfig::stop_protection` carries it into replay;
  restore and replay install it without the tick-size re-check.
- Out of scope: maximum cascade depth and velocity pauses (a separate
  issue if needed).

## Consequences

- With a collar, no trade of an elected stop's child is beyond its own
  `stop -/+ collar`; a cascade cannot trade beyond the band of the stop
  that reaches furthest. The number of children a print can run is still
  bounded only by the number of pending stops.
- The cost of protection: a stop whose band is empty is cancelled
  unexecuted and the position stays open. Callers must watch the stop's
  terminal state.
- Paths without a pending stop never read the collar; the election path
  reads it once per elected stop.
- Breaking for 0.15: two new public struct fields and snapshot format 6.
  Upgrade readers before writers.
- A later `set_tick_size` does not re-validate the collar, like the book's
  other shape rules; it keeps bounding execution exactly.
