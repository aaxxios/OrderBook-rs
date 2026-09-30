# Bench comparison summary

5 interleaved rounds. p50 / p99 in ns, median across rounds; "spread" is the round-to-round `(max - min) / median` of p50 as a percentage. A row with either side's spread > 10 pp is NOISY: inconclusive, never a pass; re-measure it. Exception (separation rule): with >= 5 rounds per side and disjoint per-round p50 ranges, such a row is REGRESSION (separated) when the candidate is slower beyond the threshold, or FASTER (separated), counted apart from OK. Otherwise a p50 delta above +3 % (uncontended) / +5 % (contended) is a REGRESSION, except for a single-op-timed row whose p50 moved by at most one clock tick (41.67 ns). See BENCH.md "Methodology".

| scenario | class / timer | baseline p50 (spread) | candidate p50 (spread) | p50 delta | baseline p99 | candidate p99 | p99 delta | verdict |
|---|---|---|---|---|---|---|---|---|
| add_only | uncontended / single | 1167 ns (3.6 pp) | 625 ns (6.6 pp) | -46.4% | 62815 | 30751 | -51.0% | OK |
| aggressive_walk | uncontended / batch | 3843 ns (49.3 pp) | 2529 ns (6.2 pp) | -34.2% | 5451 | 6355 | +16.6% | NOISY |
| contended_add_4t | contended / batch | 2219 ns (2.9 pp) | 1368 ns (2.5 pp) | -38.4% | 3161 | 2779 | -12.1% | OK |
| contended_add_8t | contended / batch | 3765 ns (1.6 pp) | 3463 ns (4.8 pp) | -8.0% | 5967 | 6247 | +4.7% | OK |
| contended_add_listeners_4t | contended / batch | 2075 ns (17.8 pp) | 1583 ns (1.4 pp) | -23.7% | 3071 | 3019 | -1.7% | FASTER (separated) |
| contended_add_listeners_8t | contended / batch | 3755 ns (9.9 pp) | 4567 ns (2.8 pp) | +21.6% | 5815 | 7231 | +24.4% | REGRESSION |
| mixed_70_20_10 | uncontended / single | 1000 ns (0.0 pp) | 500 ns (16.6 pp) | -50.0% | 30175 | 15087 | -50.0% | FASTER (separated) |
| stp_cancel_maker | uncontended / single | 2251 ns (50.0 pp) | 1166 ns (3.6 pp) | -48.2% | 4919 | 2751 | -44.1% | FASTER (separated) |
| thin_book_sweep | uncontended / batch | 425 ns (96.0 pp) | 300 ns (3.0 pp) | -29.4% | 1208 | 758 | -37.3% | FASTER (separated) |

Counts: FASTER 4, NOISY 1, OK 3, REGRESSION 1
