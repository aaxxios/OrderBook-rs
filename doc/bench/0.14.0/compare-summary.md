# Bench comparison summary

7 interleaved rounds. p50 / p99 in ns, median across rounds; "spread" is the round-to-round `(max - min) / median` of p50 as a percentage. A row with either side's spread > 10 pp is NOISY: inconclusive, never a pass; re-measure it. Otherwise a p50 delta above +3 % (uncontended) / +5 % (contended) is a REGRESSION, except for a single-op-timed row whose p50 moved by at most one clock tick (41.67 ns). See BENCH.md "Methodology".

| scenario | class / timer | baseline p50 (spread) | candidate p50 (spread) | p50 delta | baseline p99 | candidate p99 | p99 delta | verdict |
|---|---|---|---|---|---|---|---|---|
| add_only | uncontended / single | 1125 ns (7.4 pp) | 625 ns (6.7 pp) | -44.4% | 60671 | 30431 | -49.8% | OK |
| aggressive_walk | uncontended / batch | 2993 ns (64.1 pp) | 2529 ns (5.1 pp) | -15.5% | 4427 | 6323 | +42.8% | NOISY |
| cancel_only | uncontended / batch | 20 ns (15.0 pp) | 28 ns (3.6 pp) | +40.0% | 6835 | 4643 | -32.1% | NOISY |
| contended_add_4t | contended / batch | 2205 ns (5.4 pp) | 1391 ns (6.0 pp) | -36.9% | 3175 | 2335 | -26.5% | OK |
| contended_add_8t | contended / batch | 3739 ns (7.2 pp) | 3497 ns (6.5 pp) | -6.5% | 5911 | 5875 | -0.6% | OK |
| contended_add_listeners_4t | contended / batch | 2031 ns (20.4 pp) | 1567 ns (4.6 pp) | -22.8% | 2925 | 2685 | -8.2% | NOISY |
| contended_add_listeners_8t | contended / batch | 3855 ns (8.8 pp) | 4847 ns (11.4 pp) | +25.7% | 5779 | 7631 | +32.0% | NOISY |
| mass_cancel_burst | uncontended / single | 32047 ns (16.6 pp) | 29375 ns (12.5 pp) | -8.3% | 48191 | 41055 | -14.8% | NOISY |
| mixed_70_20_10 | uncontended / single | 959 ns (8.7 pp) | 500 ns (8.2 pp) | -47.9% | 28463 | 14879 | -47.7% | OK |
| replay_10k | uncontended / single | 9207807 ns (4.7 pp) | 4050943 ns (8.5 pp) | -56.0% | 10747903 | 4866047 | -54.7% | OK |
| snapshot_create_10k | uncontended / single | 383487 ns (4.2 pp) | 406015 ns (7.8 pp) | +5.9% | 508927 | 551935 | +8.5% | REGRESSION |
| snapshot_restore_10k | uncontended / single | 11165695 ns (4.0 pp) | 3997695 ns (5.9 pp) | -64.2% | 12886015 | 4694015 | -63.6% | OK |
| stp_cancel_maker | uncontended / single | 3333 ns (36.3 pp) | 2083 ns (8.0 pp) | -37.5% | 5919 | 4583 | -22.6% | NOISY |
| thin_book_sweep | uncontended / batch | 691 ns (64.0 pp) | 350 ns (4.6 pp) | -49.3% | 1483 | 1108 | -25.3% | NOISY |

Counts: NOISY 7, OK 6, REGRESSION 1
