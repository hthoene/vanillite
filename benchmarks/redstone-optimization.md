# Redstone: measurements and options

Local measurements of 2026-11-27/28, written by hand (unlike [`README.md`](README.md), which
`cargo xtask bench` generates). They compare the branch with `dev` `24be3b54` on one Windows
machine; they are **not** comparable with the numbers of the nightly benchmark server.

## Result

The order-preserving changes of this branch gain **little**: about 4 % of the mean tick time in
the server scenario and about 2 % in the dust micro-benchmark. The large redstone gain that is
available today is the existing `alternate-current` rule, which is not tick-exact.

## Changes on the branch

| Change | File |
|---|---|
| Signal reads skip air and, while a wire computes its power, other wires without the behaviour table | `crates/vanillite-game/src/level/signal.rs` |
| The `HashSet` iteration order hashes each position once (counting sort instead of `sort_by_key`) | `crates/vanillite-game/src/block/redstone/wire.rs` |
| Test: the counting sort gives the order of a stable sort by bucket | `wire.rs` |
| Ignored micro-benchmark `wire_lines_bench` | `crates/vanillite-game/src/block/redstone/tests.rs` |

## Setup

- Intel Core i9-11900KF, 32 GB RAM, Windows 10, rustc 1.99.0. No CPU pinning or quota; normal
  desktop programs were running.
- **Server scenario:** the commands of `cargo xtask bench commands redstone` (93 observer
  clocks, 1 395 dust, 93 sticky pistons), the benchmark's seed, peaceful, view and simulation
  distance 10, one still `tools/loadbot` bot, `release-fast` build, `vanilla` profile. 20 s
  after the teleport the area is built, 10 s later the 60 s measurement starts. The numbers
  are the last 1 200 ticks of `VANILLITE_TICK_LOG`. A local script drove the runs (the
  benchmark suite needs its Linux server); it is not part of the repository.
- **Micro-benchmark:** 93 lines of 15 dust, switched on and off by a redstone block at one
  end. It covers dust only, no observers or pistons.
  `cargo test -p vanillite-game --release --lib -- --ignored wire_lines_bench --nocapture`

## Server scenario

Tick times in ms, lower is better. `dev` and the branch ran alternately.

| Run | mean | p50 | p95 | p99 | max | CPU % |
|---|---:|---:|---:|---:|---:|---:|
| dev 1 | 6.17 | 1.55 | 23.92 | 28.24 | 37.52 | 16 |
| dev 2 | 6.31 | 1.59 | 24.49 | 28.70 | 43.76 | 16 |
| dev 3 | 6.17 | 1.61 | 23.91 | 26.92 | 34.70 | 18 |
| branch 1 | 6.09 | 1.63 | 23.70 | 27.92 | 34.49 | 15 |
| branch 2 | 5.94 | 1.53 | 23.01 | 26.81 | 32.22 | 17 |
| branch 3 | 5.97 | 1.65 | 22.82 | 25.30 | 52.55 | 14 |

| Average of three runs | mean | p95 | p99 |
|---|---:|---:|---:|
| dev | 6.22 | 24.11 | 27.95 |
| branch | 6.00 | 23.18 | 26.68 |
| Difference | −3.5 % | −3.9 % | −4.6 % |

Mean and p95 of all branch runs are below those of all `dev` runs. Three runs each are a small
sample; the p50 and max values show no difference.

## Dust micro-benchmark

Time per switch of all lines in ms. Both test binaries were built once and then run
alternately. Every run took the same 177 445 neighbour and shape update steps per switch.

| Round | 1 | 2 | 3 | 4 | 5 | 6 | Average |
|---|---:|---:|---:|---:|---:|---:|---:|
| dev | 20.02 | 19.84 | 19.44 | 19.64 | 19.79 | 19.66 | 19.73 |
| branch | 19.23 | 19.61 | 19.47 | 19.14 | 19.36 | 19.52 | 19.39 |

The difference is 1.7 %; the branch is faster in five of six rounds.

**Discarded numbers:** a first series on the older base `615c5825`, run one change after the
other, showed 26.1 → 24.8 ms (sort) and → 20.9 ms (signal reads), about 20 % in total. It did
not hold up: after the merge, `dev` without the changes ran at about 19.4–20.0 ms, although
`dev` had not changed the code on this path. The likely cause is varying machine load, which
was not verified. Do not quote the 20 %.

## Where the time goes

Measured on the older base `615c5825` before any change.

- `vanillite profile 60`: scheduled block ticks took 72.8 % of 8.51 ms per tick, mob spawning
  14.4 %, random ticks 4.6 %, piston block events 1.9 %.
- A sampling profile of the tick thread (build with debug symbols): the chain observer tick →
  neighbour updates → dust was in 58 % of the running samples, `best_neighbor_signal` in 20 %.
  About 22 % were the wait for the next tick.

The ticks are uneven: half take under 2 ms, the slowest 5 % over 22 ms. Vanilla's wire
algorithm updates the neighbours again at every power step. A tick-exact implementation has to
run these updates in the same order and can only make each one cheaper.

## `alternate-current`

`performance.redstone.implementation = "alternate-current"` already existed; the `paper` profile
set it when these runs were made. Since this branch only the `performance` profile does. Final
wire powers match Vanilla, **the order of neighbour and shape updates does not**; some piston
and observer builds can behave differently.

| Server scenario, branch | mean | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|---:|
| `vanilla` wire (average of three runs) | 6.00 | 1.60 | 23.18 | 26.68 | – |
| `alternate-current` (one run) | 2.47 | 1.64 | 4.61 | 5.32 | 12.82 |

In that run block ticks took 0.92 ms per tick (37.4 %). The older ignored benchmark
`alternate_current_bench` (a 48 × 48 dust plane) gave 20.8 ms and 165 072 update steps per
switch with the `vanilla` wire, 2.8 ms and 16 600 steps with `alternate-current` (one run).

## Correctness

On the branch merged with `dev` `24be3b54`:

- `cargo test -p vanillite-game --release --lib`: 455 passed, 0 failed, 2 ignored.
- All fast parity scenarios (`cargo test -p vanillite-test --test parity`) pass. The bulk tier
  was not run.

On the older base, `mob_zombie_reinforcement_night` failed with and without the changes; the
merged `dev` fixed it.

## Compared with MCHPRS

[MCHPRS](https://github.com/MCHPR/MCHPRS) (MIT) is a creative server built for redstone. No
code was taken from it. This table summarises a research pass with a small model that read
parts of the source through summaries of the pages; it was not checked line by line.
"Tick-exact" and the expected value are assessments, not measurements.

| Technique | In Vanillite | Tick-exact | Note |
|---|---|---|---|
| Redpiler: circuits compiled into a graph with its own tick queue | no | no | Very high effort. Its docs name no pistons or observers, so little is expected for this scenario. Its `--optimize` can break builds. |
| Redpiler optimisation passes (coalescing, constant folding, pruning) | no | no | Only useful together with Redpiler. Pass bodies not read. |
| "Turbo" wire (`crates/redstone/src/wire/turbo.rs`) | no | no (inferred: skips intermediate updates) | Would be another non-exact mode next to `alternate-current`. |
| One pass over the whole wire network | yes, `alternate-current` | no | See above. Not an MCHPRS technique. |
| Block changes batched per chunk section for clients | yes, `section_blocks_update` | – | Already present. |
| Cached chunk section for block reads | yes, `chunks/holders.rs` | yes | Already present; MCHPRS was not compared here. |

Not examined: MCHPRS's lighting, most of its wire code, Eigencraft, and the source of
Lithium's block update optimisations.
