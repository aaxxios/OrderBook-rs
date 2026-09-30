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
plus or minus N ticks, and CME rests any unfilled remainder at that limit).

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
  child: IOC limit at `stop + collar`. A band that reaches or passes `0` /
  `u128::MAX` has that bound as its limit (unbounded on that side).
- Remainder: cancelled, **unlike CME**, which rests it at the limit. An
  elected stop never rests liquidity. A remainder the collar cut
  (liquidity left beyond the limit, including an empty band over a
  non-empty side) ends the stop `Cancelled { reason: StopProtectionBand }`
  (new `CancelReason`, appended); a remainder left because the side ran out
  within the band keeps `InsufficientLiquidity`, as does every market
  child. Review follow-up decision.
- `OrderStatus::Triggered` gains `limit_price: Option<u128>` (the child's
  limit, `None` for a market child; `#[serde(default)]`), so the election
  event states which kind of child ran. Review follow-up decision.
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

- With a collar, no stop child trades beyond its own `stop -/+ collar`
  (the taker whose print elects the first stop is not collared). The
  number of children a print can run is still bounded only by the number
  of pending stops: a ladder of stops spaced one collar apart walks the
  book `k × collar` in one call.
- The cost of protection: since the remainder is cancelled rather than
  rested, a stop whose band is exhausted is consumed and its position is
  left unprotected; a gap of more than one collar through the stop always
  consumes it with zero fill. Callers must watch for `StopProtectionBand`.
- A sell collar that reaches or exceeds the stop price (`collar >= stop`)
  gives a limit of `0`, and a buy `stop + collar >= u128::MAX` a limit of
  `u128::MAX`: that stop is not protected at all. This is silent (no per-election log) and
  documented on `StopProtection`.
- Replay must use the source book's collar, constant over the replayed
  range. A mismatch is not an `OutcomeMismatch`, and `snapshots_match`
  catches it only when it changed an outcome, since `OrderBookSnapshot`
  does not carry the collar (no election in the range, or coinciding
  fills, and the snapshots match while future elections differ). Callers
  verify the configuration explicitly by comparing `stop_protection()`
  with the source book's or the snapshot package's field.
- Paths without a pending stop never read the collar; the election path
  reads it once per elected stop.
- Breaking for 0.15: two new public struct fields, a new `CancelReason`
  variant, a new `OrderStatus::Triggered` field and snapshot format 6.
  Upgrade readers before writers.
- A later `set_tick_size` does not re-validate the collar, like the book's
  other shape rules; it keeps bounding execution exactly.
