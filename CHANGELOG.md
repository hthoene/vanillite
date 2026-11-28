# Changelog

The changes of each Vanillite release for players and server owners; the GitHub release of a
version shows its section.

## 0.1.0-beta.3

Minecraft Java Edition 26.3.

### Gameplay

- Mobs much closer to Vanilla 26.3: zombies and their reinforcements, skeletons, endermen,
  charged creepers, creakings, the wither, the warden, breezes, striders, happy ghasts,
  guardians, bats, raiders, ravagers, hoglins and piglins; leashes, tempting, panic, shoulder
  riding, babies with their own size, mob riders steering their mounts, zombie horsemen.
- Combat: melee and projectile hits on every entity, sweeping, spear stab and Lunge, critical
  hits like Vanilla, end crystals hit by projectiles and melee, death messages with entity names.
- Items: patched and normalised item components, spawn eggs with entity data, tridents,
  throwables, firework rockets, debug stick, locks matching custom data, enchantments on mob
  equipment drops, item entities and stuck arrows like Vanilla.
- Blocks: mining, block physics, dispensers, spawners, copper chests, sculk, berries, beacons,
  cauldrons and more; block contact of orbs, TNT, minecarts and boats; boats in bubble columns.
- Fluids, light and world generation checked against Vanilla over many seeds, with fixes.
- Advancements: effect sources, explosion causes, location, player and item predicates.
- Death: the cursor stack, crafting grid and open menu drop like Vanilla.

### Performance

- Faster world generation (SIMD noise, shared aquifer cells, faster ores and features), still
  bit-exact with Vanilla, and a faster chunk pipeline; the shared world generation caches stay
  within a memory bound.
- Crowds of players and mobs cost less: connections are written once per tick, effects and
  advancement checks run in parallel, cheaper collisions, pushes and entity queries, faster
  locator bar and entity tracking.
- Faster spawning and random ticks; tile region mode is on in the `performance` profile.
- Compute workers run at a lower priority by default, so the tick thread comes first; on weak
  hardware the network threads and read cache follow the memory budget.

### Server and plugins

- Paper and Spigot plugins: drop the jars into `plugins/` and start. The server installs the
  Bukkit layer and, if needed, a Java runtime by itself, also in Docker, on Windows and macOS.
- New `/vanillite` (`/vl`) admin command: status, TPS, MSPT, profiler, chunks, entities,
  memory, config reload and connection debug; `/plugins` and `/version` like Paper.
- Console with a line editor, prompt and colours on every terminal; a clear message and a
  console prompt when the EULA is not yet accepted (the Windows window stays open).
- Bans, whitelist and a full server are refused during login like Vanilla.
- `/give` accepts every item component.
- Plugin ABI 1.12: entity network ids, plugin commands registered after load, read-only world
  access in level hooks, explosion and piston block lists.
- Protocol: the remaining packets of Vanilla 26.3, including locator bar waypoints.
- Bedrock: anvil, enchanting, smithing, stonecutter, grindstone, beacon, loom, lectern and mount
  menus, enchantments and potions on items, teams; NetherNet no longer stalls on large batches.
- Older clients: respawn works, new mobs appear as stand-ins, related items are shown.

## 0.1.0-beta.2

Minecraft Java Edition 26.3.

### Gameplay

- Death and respawn like Vanilla: kill credit, death messages, sleeping, beds, respawn anchors
  and spawn points.
- Dispensers build golems and withers, use spawn eggs, buckets, fire charges and more;
  droppers and crafters reach every container; jukeboxes and shelves are containers.
- Many more items and blocks behave like Vanilla: tools, bone meal, wax, buckets, bottles,
  throwables, elytra, totems, tridents, maces, fishing, fireworks, shields, pots and sherds,
  hanging signs, suspicious blocks, waterlogging, sponges, dripleaf, turtle eggs, kelp, coral.
- Block contact for every entity (pressure plates, tripwires, cactus, cobwebs, lily pads) and
  falling block damage; step and fall sounds.
- Mobs: many more mob mechanics tick-exact with Vanilla (egg laying, sheep dyeing, lightning
  conversions, leashes on fences, piglins guarding containers, ominous trial spawners, animal
  sound variants).
- Vehicles carry their passengers through portals; the End dragon fight starts and ends
  correctly; `/kill` and `/summon` work on the dragon and all mobs like Vanilla.
- All advancement triggers and statistics of Minecraft 26.3, with rewards like Vanilla.
- Loot tables: better compatibility, `set_components` and `/loot spawn`.
- Container locks on block entities; custom dimensions from data packs; several independent
  worlds on one server.

### Performance

- World generation about 1.6x faster per core, still bit-exact with Vanilla.
- Faster light updates after block changes and faster chunk delivery while all workers are busy.
- Mob activation range for far-away mobs (configurable), cheaper
  despawn checks and path finding; dense crowds cost less network time.
- Lower memory use: memory is given back while the server is empty, world generation data
  and structure templates load on demand.
- Runs on very weak hardware: respects CPU quotas, one core plays smoothly.
- Linux archives and the Docker image include an x86-64-v3 (AVX2) build that is picked
  automatically on CPUs that support it.
- Optional tile region mode: natural spawning and random ticks on several cores.

### Server and plugins

- Plugin ABI 1.10: many new events (player, entity, block, chunk, world, inventory), entity,
  world, menu, item, scoreboard, boss bar and registry API, packet listeners, custom goals and
  world populators.
- New anti-cheat rule `anticheat.digging.fast-break-check`.
- Proxy: BungeeCord forwarding, `send`/`alert`/`reload` and a console; it tries each server
  once and shows the server's kick reason.
- Bedrock: clients can also join over NetherNet (WebRTC); containers, crafting, block
  entities, sidebar and item names; every connection attempt is logged.
- Older clients: recipe book, stonecutter, advancements and book and container items.
- Safer world storage: full disks and damaged files no longer lose data, world conversion
  can resume.
- Pterodactyl and Pelican: a simple and an advanced egg, one image per Minecraft version.

### Building from source

- The Vanilla data is no longer part of the source: the build downloads Mojang's official
  server jar and extracts the data from it (a JDK is downloaded too if none is installed).

## 0.1.0-beta.1

The first public release, for Minecraft Java Edition 26.3.
