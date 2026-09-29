# Bench comparison summary

7 interleaved rounds. p50 / p99 in ns, median across rounds; "spread" is the round-to-round `(max - min) / median` of p50 as a percentage. A row with either side's spread > 10 pp is NOISY: inconclusive, never a pass; re-measure it. Exception (separation rule): with >= 5 rounds per side and disjoint per-round p50 ranges, such a row is REGRESSION (separated) when the candidate is slower beyond the threshold, or FASTER (separated), counted apart from OK. Otherwise a p50 delta above +3 % (uncontended) / +5 % (contended) is a REGRESSION, except for a single-op-timed row whose p50 moved by at most one clock tick (41.67 ns). See BENCH.md "Methodology".

| scenario | class / timer | baseline p50 (spread) | candidate p50 (spread) | p50 delta | baseline p99 | candidate p99 | p99 delta | verdict |
|---|---|---|---|---|---|---|---|---|
| add_only | uncontended / single | 1083 ns (0.1 pp) | 584 ns (7.0 pp) | -46.1% | 59103 | 29807 | -49.6% | OK |
| aggressive_walk | uncontended / batch | 3365 ns (35.2 pp) | 2437 ns (4.2 pp) | -27.6% | 4811 | 6115 | +27.1% | NOISY |
| cancel_only | uncontended / batch | 713 ns (7.3 pp) | 687 ns (3.3 pp) | -3.6% | 1066 | 1024 | -3.9% | OK |
| contended_add_4t | contended / batch | 2171 ns (3.3 pp) | 1343 ns (7.1 pp) | -38.1% | 3145 | 2151 | -31.6% | OK |
| contended_add_8t | contended / batch | 3649 ns (4.2 pp) | 3321 ns (7.0 pp) | -9.0% | 5423 | 5207 | -4.0% | OK |
| contended_add_listeners_4t | contended / batch | 2059 ns (18.3 pp) | 1529 ns (1.2 pp) | -25.7% | 2783 | 2301 | -17.3% | FASTER (separated) |
| contended_add_listeners_8t | contended / batch | 3795 ns (8.3 pp) | 4363 ns (1.9 pp) | +15.0% | 5435 | 6675 | +22.8% | REGRESSION |
| mass_cancel_burst | uncontended / single | 760831 ns (2.6 pp) | 754175 ns (2.6 pp) | -0.9% | 1242111 | 1033215 | -16.8% | OK |
| mixed_70_20_10 | uncontended / single | 917 ns (0.0 pp) | 459 ns (9.2 pp) | -49.9% | 28543 | 14583 | -48.9% | OK |
| replay_10k | uncontended / single | 9019391 ns (0.6 pp) | 3942399 ns (1.2 pp) | -56.3% | 10264575 | 4415487 | -57.0% | OK |
| snapshot_create_10k | uncontended / single | 372991 ns (2.6 pp) | 396287 ns (7.4 pp) | +6.2% | 427519 | 470783 | +10.1% | REGRESSION |
| snapshot_restore_10k | uncontended / single | 10887167 ns (3.0 pp) | 3962879 ns (0.9 pp) | -63.6% | 12738559 | 4538367 | -64.4% | OK |
| stp_cancel_maker | uncontended / single | 2709 ns (53.9 pp) | 2041 ns (10.2 pp) | -24.7% | 5295 | 4335 | -18.1% | FASTER (separated) |
| thin_book_sweep | uncontended / batch | 541 ns (81.7 pp) | 341 ns (2.6 pp) | -37.0% | 1291 | 1066 | -17.4% | NOISY |

Counts: FASTER 2, NOISY 2, OK 8, REGRESSION 2
