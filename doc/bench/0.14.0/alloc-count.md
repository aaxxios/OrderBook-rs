# alloc_count raw output (#259)

before = main 5447250, after = 0.14.0 candidate; three runs each; host as in system-info.md.

## before_r1

```
alloc_count_mixed_70_20_10                         allocs/op 16.4626   bytes/op 9375.52
alloc_count_add_only_one_level_with_user           allocs/op 6.2980    bytes/op 18257.86
alloc_count_add_only_one_level_no_user             allocs/op 6.2760    bytes/op 18216.70
alloc_count_add_only_distinct_levels_with_user     allocs/op 8.1460    bytes/op 18345.17
alloc_count_cross_one_level_full_fill              allocs/op 128.0159  bytes/op 4072.84
alloc_count_cross_one_level_partial_fill           allocs/op 4.0000    bytes/op 1200.00
alloc_count_cross_deep_level_large_taker           allocs/op 4.0000    bytes/op 88320.00
alloc_count_market_sweep_one_level                 allocs/op 119.0318  bytes/op 3049.89
alloc_count_market_sweep_three_levels              allocs/op 233.0952  bytes/op 6829.25
```

## before_r2

```
alloc_count_mixed_70_20_10                         allocs/op 32.4206   bytes/op 9758.57
alloc_count_add_only_one_level_with_user           allocs/op 6.2820    bytes/op 18236.63
alloc_count_add_only_one_level_no_user             allocs/op 6.2830    bytes/op 18242.93
alloc_count_add_only_distinct_levels_with_user     allocs/op 8.1450    bytes/op 18342.04
alloc_count_cross_one_level_full_fill              allocs/op 104.0159  bytes/op 3496.84
alloc_count_cross_one_level_partial_fill           allocs/op 4.0000    bytes/op 1200.00
alloc_count_cross_deep_level_large_taker           allocs/op 4.0000    bytes/op 88320.00
alloc_count_market_sweep_one_level                 allocs/op 101.0318  bytes/op 2617.89
alloc_count_market_sweep_three_levels              allocs/op 209.0952  bytes/op 6253.25
```

## before_r3

```
alloc_count_mixed_70_20_10                         allocs/op 23.9564   bytes/op 9555.36
alloc_count_add_only_one_level_with_user           allocs/op 6.2800    bytes/op 18235.20
alloc_count_add_only_one_level_no_user             allocs/op 6.2920    bytes/op 18247.86
alloc_count_add_only_distinct_levels_with_user     allocs/op 8.1360    bytes/op 18338.33
alloc_count_cross_one_level_full_fill              allocs/op 18.0159   bytes/op 1432.84
alloc_count_cross_one_level_partial_fill           allocs/op 4.0000    bytes/op 1200.00
alloc_count_cross_deep_level_large_taker           allocs/op 4.0000    bytes/op 88320.00
alloc_count_market_sweep_one_level                 allocs/op 44.0318   bytes/op 1249.89
alloc_count_market_sweep_three_levels              allocs/op 248.0952  bytes/op 7189.25
```

## after_r1

```
alloc_count_mixed_70_20_10                         allocs/op 3.3545    bytes/op 3323.18
alloc_count_add_only_one_level_with_user           allocs/op 3.2790    bytes/op 945.86
alloc_count_add_only_one_level_no_user             allocs/op 3.2900    bytes/op 963.16
alloc_count_add_only_distinct_levels_with_user     allocs/op 8.1440    bytes/op 18455.72
alloc_count_cross_one_level_full_fill              allocs/op 3.0159    bytes/op 1072.84
alloc_count_cross_one_level_partial_fill           allocs/op 4.0000    bytes/op 1200.00
alloc_count_cross_deep_level_large_taker           allocs/op 4.0000    bytes/op 88320.00
alloc_count_market_sweep_one_level                 allocs/op 2.0318    bytes/op 241.89
alloc_count_market_sweep_three_levels              allocs/op 8.0952    bytes/op 1429.25
```

## after_r2

```
alloc_count_mixed_70_20_10                         allocs/op 3.3545    bytes/op 3323.08
alloc_count_add_only_one_level_with_user           allocs/op 3.2950    bytes/op 983.82
alloc_count_add_only_one_level_no_user             allocs/op 3.2830    bytes/op 952.95
alloc_count_add_only_distinct_levels_with_user     allocs/op 8.1400    bytes/op 18443.27
alloc_count_cross_one_level_full_fill              allocs/op 3.0159    bytes/op 1072.84
alloc_count_cross_one_level_partial_fill           allocs/op 4.0000    bytes/op 1200.00
alloc_count_cross_deep_level_large_taker           allocs/op 4.0000    bytes/op 88320.00
alloc_count_market_sweep_one_level                 allocs/op 2.0318    bytes/op 241.89
alloc_count_market_sweep_three_levels              allocs/op 8.0952    bytes/op 1429.25
```

## after_r3

```
alloc_count_mixed_70_20_10                         allocs/op 3.3545    bytes/op 3323.11
alloc_count_add_only_one_level_with_user           allocs/op 3.2790    bytes/op 957.96
alloc_count_add_only_one_level_no_user             allocs/op 3.2690    bytes/op 917.45
alloc_count_add_only_distinct_levels_with_user     allocs/op 8.1470    bytes/op 18457.30
alloc_count_cross_one_level_full_fill              allocs/op 3.0159    bytes/op 1072.84
alloc_count_cross_one_level_partial_fill           allocs/op 4.0000    bytes/op 1200.00
alloc_count_cross_deep_level_large_taker           allocs/op 4.0000    bytes/op 88320.00
alloc_count_market_sweep_one_level                 allocs/op 2.0318    bytes/op 241.89
alloc_count_market_sweep_three_levels              allocs/op 8.0952    bytes/op 1429.25
```
