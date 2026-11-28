# Tile region mode: one world on several cores

Crates: `vanillite-game` (`level::tiles`, `mob::spawn`), `vanillite-server` (`world`, `compute`),
`vanillite-config` (`performance.region-threading`).

A world ticks on one thread. With players spread out, most of that tick is work per simulated
chunk: natural mob spawning and random ticks. The tile region mode spreads this work over the
compute pool. It is an opt-in rule because it cannot be tick-exact with Vanilla: Vanilla draws
one random stream in one world-wide order, and any parallel split changes who draws which
number.

```toml
[performance.region-threading]
enabled = true   # off in every profile
tile-size = 16   # edge of a tile in chunks, 8 to 32
```

Both keys can be set per world and are applied on `vanillite reload config`. Off, the world
ticks in Vanilla's serial order with the code path it always had.

## How it works

- **Tiles.** The world is cut into fixed squares of `tile-size × tile-size` chunks, aligned to
  the chunk grid. Tiles never merge or split, so many players cannot collapse them into one
  region. A tile is busy when it has chunks the phase works on; other tiles cost nothing.
- **Propose, in parallel.** The busy tiles are worked out on the compute pool. The tiles share
  the level read-only (`&Level`; nothing writes meanwhile) and return what they want to
  change. Each has its own random stream, seeded from the world seed, the game time, the tile
  and the phase (`tiles::tile_seed`).
- **Adaptive jobs.** Each round plans its jobs from the tiles' cost, a moving average of how
  long each busy tile took in earlier rounds of the phase (`tiles::TileCosts`, kept only for busy
  tiles). A hot tile (at least a quarter of one thread's share of the round) runs alone, and
  the jobs start longest first; adjacent cold tiles of the same colour (tile x and z parity)
  share one job. This is only a scheduling decision: no data moves between tiles, a tile
  computes the same whichever job runs it, and the replies are applied in tile order.
- **Commit, serially.** The world's owner applies the proposals tile by tile in tile order
  (x, then z) with the whole world writable.
- No job writes shared state, so there are no locks, and the result depends only on the world
  state: one thread or sixteen give the same world (tests
  `tiled_random_ticks_do_not_depend_on_threads`, `tiled_spawns_do_not_depend_on_threads`).
- **Serial fallback.** A phase runs in Vanilla's exact serial order, with the level's own
  random, in a tick where its chunks lie in one tile, are fewer than 512 (one player's
  simulation distance 10 is 441 chunks) or a synchronous chunk load's `unknown` ticket exists
  (see below). A single player or one crowd therefore plays exactly as without the rule.

## What runs per tile

| Phase | Per tile, in parallel | Serial commit |
|---|---|---|
| Natural spawning (`NaturalSpawner`, `spawn::tick_tiled`) | the tile's spawning chunks, their shuffle, every spawn attempt with all position and spawn rule checks, `finalizeSpawn` | adding the mobs; inhabited time and `tickThunder` of the chunks |
| Random ticks (`ServerLevel.tickChunk`, `Level::tick_chunks_tiled`) | picking the positions of every chunk and keeping those whose block or fluid ticks randomly, and the precipitation rolls | the random tick of each kept position (the block is read again), `tickPrecipitation` |

Everything else ticks serially as before: the players' packets, scheduled block and fluid
ticks, block events, entities, block entities, chunk loading and what is sent to clients.
Their in-place writes need tiles that own their chunks and entities (the entity store is one
store per world today); that is the next step of this mode, not part of it yet.

## Deviations from Vanilla

They apply only in ticks where a phase runs tiled.

| # | Deviation | Observable |
|---|---|---|
| T1 | Random tick positions (`randValue`), the precipitation roll, the shuffle of the spawning chunks and the spawn attempts (positions, mob choice, pack size, `finalizeSpawn`) draw from a random stream per tile, tick and phase. The world's `level.random` is not advanced by them, so everything that draws from it later sees other numbers than a seeded Vanilla run. | Same rates and distributions; a seeded trace differs. |
| T2 | Tiles work in tile order, and inside a tile the chunks in x/z order (random ticks) or shuffled per tile (spawning), instead of the world-wide chunk order and one world-wide shuffle. | Only the order of effects within one tick. |
| T3 | Random tick positions are chosen on the world as it is when the phase starts. A section that starts or stops ticking randomly through another random tick of the same tick counts with its state at the start; a chosen position whose block only became randomly ticking in this tick is not ticked. The block is read again when its tick runs, so no tick runs on a stale block. | None in practice. |
| T4 | Spawn attempts read the world as it is when the round starts. A tile counts its own spawns of the tick against the players' mob caps and the spawn cost budget, but does not see what other tiles spawn in the same tick. | A player's mob cap can be exceeded by one tick's spawns where the player's range spans tiles; the next tick sees them and stops. |
| T5 | Mobs spawned in the same tick are not in the world for later attempts of that tick: a jockey looks for its mount among the mobs of the tick's start, and a mount two jockeys chose is taken by the first (the second spawns on foot). | A chicken jockey slightly less often. |
| T6 | Inhabited time and `tickThunder` of a tile's chunks run after the spawn attempts of all tiles instead of before each chunk's attempts. Lightning still draws from `level.random`. | None in practice. |
| T7 | A naturally spawned mob's own random is seeded from the tile's stream (Vanilla: unseeded). | None. |

Not deviations: effects across a tile border are not delayed. The commit writes the whole
world, so a random tick at the edge of a tile changes the block in the next tile in the same
tick (test `random_ticks_spread_across_a_tile_border`), a pack that wanders over the border
spawns there (test `tiled_spawns_cross_tile_borders_and_stay_in_ticking_chunks`), and entities,
block updates, redstone, fluids and scheduled ticks at tile borders run in the serial phases
exactly as without the rule. Positions in chunks that are not loaded or do not tick are
rejected like in the serial round.

## Owners: the seam for tiles on other servers

A tile is worked out by its owner (`tiles::OwnerId`), not by "a thread". Today every tile's
owner is this process (`OwnerId::LOCAL`, `TileRun::owner`) and its job runs on the compute pool.
The contract is already the one a remote owner needs:

- What goes to the owner is the tile, its chunks and the inputs of the phase; what comes back
  is a `TileReply`: the tile, its owner and its changes as plain data (block positions, new
  mobs), with no references into the level.
- Tiles share nothing with each other and never call each other. Everything that crosses a
  tile border goes through the world's owner, which applies the replies in tile order.
- The random stream of a tile depends only on world seed, game time, tile and phase, so the
  result is the same wherever the tile is worked out.

Not there yet for tiles on other servers: the jobs read the shared `&Level`, so a remote owner
needs its own copy of the tile's chunks and entities plus a halo, kept current; replies and
requests need a wire format (mobs have their NBT form, positions are trivial); the phases that
write in place (entities, scheduled ticks, block events, block entities) must become tile
owned first; players, packets and the server-wide state (player list, scoreboard, the level
random, game rules) need an owner and routing of their own.

## Measurements

Test server (4 cores), vanilla profile, one server process per run, bots spread over a new
world within 1 500 blocks (`spread:1500`, the world is still being generated while measured).
The rule was switched by `vanillite reload config` every 40 s in the order off, on, on, off,
off, on; the table has the median of the three windows per setting (MSPT from the tick log).

| Players | Rule | MSPT p50 | p95 | Server CPU per tick | RSS |
|---:|---|---:|---:|---:|---:|
| 250 | off | 187 ms | 323 ms | 666 ms | 1 257 MiB |
| 250 | on | 106 ms | 165 ms | 386 ms | 1 136 MiB |
| 500 | off | 621 ms | 828 ms | 1 620 ms | 2 157 MiB |
| 500 | on | 278 ms | 543 ms | 1 110 ms | 2 114 MiB |
| 1 000 | off | 1 013 ms | 1 465 ms | 2 227 ms | 3 244 MiB |
| 1 000 | on | 427 ms | 585 ms | 1 364 ms | 3 197 MiB |

A crowd in one place gains nothing, as designed: 2 000 bots walking around the spawn (same
server and method, 45 s windows, median of three per setting, host under load) tick at p50
151 ms / p95 280 ms with the rule off and p50 150 ms / p95 347 ms with it on, with 378 ms of
server CPU per tick both times.

Where the tick goes with 200 spread players (phase timers, rule off): natural spawning 52 %,
random ticks 21 %, entities 16 %. With the rule on, spawning takes about a quarter of its
serial time on 4 cores and the entity tick becomes the largest part.

## Cost with the rule off

None: off, the world ticks through the serial code it had before the mode, and the shared
spawn logic compiles to the same work for the world round. Checked by instruction counts
(callgrind, `release` profile) of the same seeded runs on the commit before the mode and with
it: eight parity scenarios through the server tick 20 177.6 M to 20 173.7 M instructions
(-0.02 %), 60 serial spawn rounds with one player 23.31 M to 23.07 M (-1.0 %). Two things
keep it that way: the tracked chunk read (`Chunks::note_read`, only while a synchronous
load's `unknown` ticket exists) stays out of line so block reads do not pay for it, and the
world round draws from the level's random in place (`Round::with_level_random` swaps it out
only around `finalizeSpawn`).

## Sharing the level between threads

`Level` is `Sync` (checked at compile time). Reads through `&Level` are pure except for
Vanilla's chunk cache of the tick view, which only exists while a synchronous chunk load's
`unknown` ticket does (`Chunks::reads_are_pure`, see `world-access.md`). A tiled phase runs
only while reads are pure, so the tick view (`Level::block`) can be used by code that runs per
tile; a tick with such a ticket uses the serial order.

## Adding a tiled phase

1. Split the phase into what it decides (reads, random draws) and what it writes. The first
   part gets `&Level`, a tile's chunks and a `LegacyRandom` from `tile_seed`; it returns its
   writes as data.
2. Keep one code path for the game logic: the serial phase and the tile job call the same
   function (`spawn_category_for_chunk` over `Round`, `Level::random_tick_at`).
3. Use `TileRun::applies`, `group` and `map`, apply the results in the order `map` returns
   them, and add a test that compares `inline_fork` with a fork on threads.
4. Add the deviations to the table above and to the rule's doc comment in
   `vanillite-config/src/catalogue.rs`.
