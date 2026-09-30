# Bench comparison summary

5 interleaved rounds. p50 / p99 in ns, median across rounds; "spread" is the round-to-round `(max - min) / median` of p50 as a percentage. A row with either side's spread > 10 pp is NOISY: inconclusive, never a pass; re-measure it. Exception (separation rule): with >= 5 rounds per side and disjoint per-round p50 ranges, such a row is REGRESSION (separated) when the candidate is slower beyond the threshold, or FASTER (separated), counted apart from OK. Otherwise a p50 delta above +3 % (uncontended) / +5 % (contended) is a REGRESSION, except for a single-op-timed row whose p50 moved by at most one clock tick (41.67 ns). See BENCH.md "Methodology".

| scenario | class / timer | baseline p50 (spread) | candidate p50 (spread) | p50 delta | baseline p99 | candidate p99 | p99 delta | verdict |
|---|---|---|---|---|---|---|---|---|
| add_only | uncontended / single | 1167 ns (7.2 pp) | 667 ns (12.4 pp) | -42.8% | 66943 | 32591 | -51.3% | FASTER (separated) |
| aggressive_walk | uncontended / batch | 2885 ns (57.5 pp) | 2695 ns (22.1 pp) | -6.6% | 5259 | 7223 | +37.3% | NOISY |
| cancel_only | uncontended / batch | 907 ns (19.5 pp) | 848 ns (25.4 pp) | -6.5% | 2125 | 1401 | -34.1% | NOISY |
| contended_add_4t | contended / batch | 2293 ns (5.1 pp) | 1377 ns (7.3 pp) | -39.9% | 5115 | 3109 | -39.2% | OK |
| contended_add_8t | contended / batch | 3611 ns (6.0 pp) | 3447 ns (3.4 pp) | -4.5% | 6043 | 7051 | +16.7% | OK |
| contended_add_listeners_4t | contended / batch | 2119 ns (57.5 pp) | 1575 ns (5.0 pp) | -25.7% | 3795 | 3277 | -13.6% | FASTER (separated) |
| contended_add_listeners_8t | contended / batch | 3873 ns (7.7 pp) | 4511 ns (3.9 pp) | +16.5% | 6571 | 6935 | +5.5% | REGRESSION |
| mass_cancel_burst | uncontended / single | 885247 ns (23.4 pp) | 529407 ns (7.1 pp) | -40.2% | 1620991 | 1127423 | -30.4% | FASTER (separated) |
| mixed_70_20_10 | uncontended / single | 1042 ns (4.0 pp) | 542 ns (15.3 pp) | -48.0% | 31759 | 15295 | -51.8% | FASTER (separated) |
| replay_10k | uncontended / single | 10354687 ns (20.6 pp) | 4202495 ns (5.1 pp) | -59.4% | 13311999 | 5844991 | -56.1% | FASTER (separated) |
| snapshot_create_10k | uncontended / single | 402431 ns (8.8 pp) | 198655 ns (4.6 pp) | -50.6% | 616447 | 262143 | -57.5% | OK |
| snapshot_restore_10k | uncontended / single | 11829247 ns (4.2 pp) | 3942399 ns (4.8 pp) | -66.7% | 16252927 | 5091327 | -68.7% | OK |
| stp_cancel_maker | uncontended / single | 3543 ns (52.9 pp) | 1125 ns (7.4 pp) | -68.2% | 6375 | 2751 | -56.8% | FASTER (separated) |
| thin_book_sweep | uncontended / batch | 508 ns (101.8 pp) | 300 ns (5.7 pp) | -40.9% | 1333 | 775 | -41.9% | NOISY |

Counts: FASTER 6, NOISY 3, OK 4, REGRESSION 1
