//! Parity scenarios: a world, a region to observe, a setup and actions per tick.
//!
//! A scenario is a JSON file in `crates/vanillite-test/scenarios/`; its file name is its name.
//! The Java recorder (`ScenarioRecorder.java`) reads the same file.
//!
//! ```json
//! {
//!   "description": "What the scenario shows.",
//!   "tier": "fast",
//!   "profile": "vanilla",
//!   "world": {"seed": 0, "biome": "minecraft:plains",
//!             "layers": [["minecraft:bedrock", 1], ["minecraft:dirt", 2], ["minecraft:grass_block", 1]]},
//!   "rules": {"random_tick_speed": "0"},
//!   "region": [[0, 0, 0], [5, 3, 5]],
//!   "ticks": 4,
//!   "light": false,
//!   "setup": [{"op": "fill", "from": [0, 0, 0], "to": [5, 0, 5], "block": "minecraft:stone"}],
//!   "actions": [{"tick": 1, "op": "place", "pos": [3, 1, 3], "block": "minecraft:sand"}]
//! }
//! ```
//!
//! - `tier`: `fast` (golden in the repository, runs on every push) or `bulk` (golden in the
//!   bulk store, runs nightly). Default `fast`.
//! - `profile`: the configuration profile. Only `vanilla` can be recorded on Vanilla; any other
//!   profile needs a blessed golden (`cargo xtask golden bless`). Default `vanilla`.
//! - `world`: a flat world without structures or features; layers bottom up from the world's
//!   minimum y. Defaults: seed 0, plains, the classic layers above.
//! - `rules`: game rules on top of the defaults every scenario has (`random_tick_speed 0`,
//!   `spawn_mobs false`, `advance_time false`, `advance_weather false`).
//! - `region`: the observed box (inclusive). Its chunks and their neighbours are forced
//!   (`/forceload`) and ticking before the setup.
//! - `light`: frames also hold the settled sky and block light of the region, the levels a
//!   client sees. Default `false`.
//! - `weather`: `clear` (default), `rain` or `thunder`, at full strength from the start (no
//!   fade in); with `advance_weather` off it stays.
//! - `block_entities`: frames also hold the block entities of the region (their full saved
//!   data, see [`crate::trace`]). Default `false`.
//! - `feedback`: frames also hold the plain text of the messages the `command` actions of
//!   their tick sent to the server (its `System chat` lines), in order. Default `false`.
//! - `level_rng`: frames also hold the state of the level random (always with entities).
//!   Scenarios with random ticks set it: the random draws must match call by call.
//!
//! Random ticks: the level random is reseeded with the world seed after the setup, and the LCG
//! of random tick positions (`Level.randValue`) starts at the seed's low 32 bits.
//! - `setup`: applied without updates before frame 0 (`set`, `fill`; `place` with updates).
//! - `dimensions`: `true` runs the scenario with all three dimensions on the server, ticked
//!   one after another in Vanilla's order (`performance.tick.parallel-dimensions = false`): the
//!   Nether and the End are flat too (bedrock, then netherrack or end stone up to y 3), players
//!   load chunks like on a normal server (view and simulation distance 2), and every chunk
//!   ticket loads its chunks at once, like Vanilla's synchronous loads of a teleport. Then
//!   `dimension` is the dimension of the region (default the overworld), `views` are more
//!   regions whose entities the frames hold (`[{"dimension": ..., "region": [min, max]}]`,
//!   their chunks forced like the region's), entities outside the overworld have their
//!   `dim` in their detail, and a block op works in its `dimension` (default the overworld).
//!   Scenario entities start in the overworld; the level random of every dimension starts
//!   from the world seed.
//! - `client_attributes`: with `client_view`, the view also holds the players' own attribute
//!   values (`attr`, from `update_attributes`).
//! - `client_view`: with `dimensions`, frames also hold what the players' clients received
//!   (`"events"` instead of `true`: also their level and entity events)
//!   during the tick, normalized (`view`, see [`crate::client_view`]): entities added and
//!   removed, boss bars, login and respawn (dimension changes), block changes in view, game
//!   events, abilities and inventory slots. Default `false`.
//! - `end`: with `dimensions`, `"normal"` generates the End like a new Vanilla world (the
//!   main island with its spikes and crystals) instead of flat; the dragon fight starts once a
//!   player is there. Default `"flat"`.
//! - `datapack`: with `dimensions`, the data pack `datapacks/<name>/` of this crate is in the
//!   world's `datapacks/` folder from the start (enabled like a new pack), so its dimensions
//!   are loaded too; a flat one generates its layers in both servers.
//! - `actions`: before world tick `tick` (1-based): `place` (with neighbour and shape updates,
//!   Vanilla `setBlock` flags 3), `set` (without updates), `fill` (with updates), `force` and
//!   `unforce` (`"chunks": [[x, z], ...]`: adds or removes forced tickets like `/forceload`,
//!   then the chunk system runs without a game tick until every chunk is loaded, unloaded and
//!   has its entities like its new ticket level says), `restart` (the server saves and stops,
//!   then starts again on the saved world: the saved chunk tickets come back and their chunks
//!   load before the next tick like `MinecraftServer.prepareLevels`; the rules, time and
//!   difficulty are set again and the level random starts from the world seed; scenarios with
//!   entities cannot restart), `bonemeal` (`"pos"`: bone meal used on the block,
//!   `BoneMealItem.growCrop`).
//!
//! Positions in chunks that are not accessible (unloaded or above full level) are traced as
//! the state [`UNLOADED`].
//!
//! Block states are written `name[key=value,...]` like in commands.
//!
//! Entities (mob AI and path finding parity):
//!
//! ```json
//! {
//!   "time": 18000, "difficulty": "normal", "entity_seed": 7,
//!   "entities": [
//!     {"name": "steve", "type": "minecraft:player", "pos": [8.5, -60, 2.5]},
//!     {"name": "zombie", "type": "minecraft:zombie", "pos": [2.5, -60, 2.5], "rot": [0, 0],
//!      "seed": 1}
//!   ],
//!   "actions": [{"tick": 5, "op": "hurt", "entity": "zombie", "by": "steve", "amount": 1}]
//! }
//! ```
//!
//! - `time`: the day time set before the entities come (`/time set`); none = the world's start.
//! - `difficulty`: `peaceful`, `easy`, `normal` (default) or `hard`.
//! - `entities`: after the setup, players first (survival, on a connection nobody reads), then
//!   the level random is reseeded with the world seed, then the other entities in order, each
//!   like `/summon` makes it: created with its random seeded with `seed`, placed at `pos` with
//!   `rot` (yaw, pitch; head and body face the yaw), `finalizeSpawn` (unless `"finalize":
//!   false`), made a baby if `baby` says so. The `name` identifies the entity in the trace.
//!   A copper golem takes the oxidation `weather_state` (`CopperGolem.setWeatherState`).
//! - `entity_seed`: every entity created while the scenario runs (arrows, drops, orbs, babies,
//!   spawned mobs) seeds its random with the next `nextLong()` of a legacy random seeded with
//!   it, in creation order (default: the world seed). An entity loaded from storage (a
//!   reloaded chunk's entities) takes the low 64 bits of its UUID xor the game time instead,
//!   independent of the order the loads complete in. The Java recorder enforces this in
//!   Vanilla with its agent (`ParityAgent.java`).
//!
//! Commands: `{"tick": 2, "op": "command", "command": "fill 0 1 0 3 1 3 stone"}` runs the line
//! as the server (permission level 4) at the origin of the overworld before that tick.
//!
//! Known divergences from Vanilla live in `scenarios/divergences/<scenario>.toml`, not in the
//! scenario (so they change without a new recording), one file per scenario so parallel
//! changes never touch the same file:
//!
//! ```toml
//! # scenarios/divergences/mob_zombie_chase.toml
//! tick = 1
//! cause = "the zombie lacks the villager, iron golem and turtle target goals"
//! owner = "gameplay-developer #58"
//! ```
//!
//! The check passes while the first difference is at exactly that tick and fails when it
//! moves: earlier is a regression; later is progress (raise the tick, or remove the entry once
//! the scenario matches Vanilla).
//! - `hurt` action: `amount` damage to the entity `entity`, by `by` (player attack or mob
//!   attack) or generic damage without `by`.
//! - `lightning` action: the strike of `ServerLevel.tickThunder` in the column of `pos` (its y
//!   does not matter) without the chance roll: the target (`findLightningTargetAround`: the
//!   nearest lightning rod, a living entity under the open sky, or the column's top), then
//!   `isRainingAt`, the skeleton trap and the bolt. Needs `"weather": "thunder"` (or rain).
//! - `use` action: the player `entity` gets `item` (`count`, SNBT `components`;
//!   `minecraft:air` empties the hand; without `item` the hand stays as it is) in its main hand (or `"hand": "off"`) and clicks the block
//!   at `pos` on `face` (default `up`) at `hit` (relative to the block's corner, default the
//!   face's centre), sneaking if `sneak`, with its food level set to `food` first
//!   (`ServerPlayerGameMode.useItemOn`). `attack`: the
//!   player starts to break the block (`START_DESTROY_BLOCK`). `"player_state": true` adds
//!   the players' food and hands to the frames, `"sleep_state": true` their sleeping position,
//!   sleep timer, pose id and respawn point (`[x, y, z, forced]`). `"progress_state": true`
//!   (with `dimensions`: only the server evaluates advancements) adds the players' completed
//!   advancement criteria (`adv`: sorted `advancement#criterion`) and their non-zero statistics
//!   (`stats`: `minecraft.used:minecraft.crossbow` and so on) without the time statistics
//!   (`play_time`, `total_world_time`, `time_since_death`, `time_since_rest`, `crouch_time`) and
//!   the distances (`*_one_cm`).
//! - `effect` action: `addEffect` on the entity `entity` with the mob effect `effect`,
//!   `duration` ticks and `amplifier` (default 0).
//! - `use_item` action: the player `entity` gets `count` (default 1) of the item `item` (SNBT
//!   `components`; without `item` the hand stays as it is) in its main hand and uses it
//!   (`ServerPlayerGameMode.useItem`, as a `use_item` packet would). `release_item`: the
//!   player stops using its item (`releaseUsingItem`, the `RELEASE_USE_ITEM` action).
//!   `attack_entity`: the player, with the same optional stack, attacks the entity `target`
//!   (`Player.attack`), with its `fall_distance` set first if given (as its client's moves
//!   would; the server does not add up a player's fall itself). `stab`: the player, with the
//!   same optional stack, jabs with its piercing weapon (the `STAB` player action). With
//!   `food` the player's food level is set first; `inventory` (`[{"slot": 40, "item": ...,
//!   "count": ..., "components": ...}]`) puts stacks into inventory slots first (0-35 the
//!   items, 36-39 the armour, 40 the off hand).
//!   `interact`: like `use_item`, but on the entity `target` (`Player.interactOn`).
//!
//! Packet-level players (`"client": true` on a player entity) are driven by the serverbound
//! play packets a real client sends, handled by the real packet handlers on both sides
//! (Vanilla: `ServerGamePacketListenerImpl`; Vanillite: `Connection::handle_packets`, then the
//! moves and entity actions like the server world). The harness plays the client's part of the
//! protocol that does not depend on the scenario: the join (the server teleports the player to
//! `pos`; the client accepts and sends `player_loaded`), accepting every later server teleport
//! at the start of a tick, and `client_tick_end` after the packets of each tick. Frames of
//! client players hold their inventory (`inv`: `[slot, item, count]` of the non-empty slots in
//! Vanilla's `Inventory` order) and food.
//!
//! ```json
//! {"tick": 3, "op": "packet", "entity": "steve", "packet": "attack", "data": "00",
//!  "target": "zombie", "what": "Attack { entity_id: 0 }"}
//! ```
//!
//! - Client players may start with `"inventory": [[slot, item, count], ...]` (Vanilla
//!   `Inventory` indices: 0–8 hotbar, 9–35 main, 36–39 armor, 40 off hand) and a `"food"`
//!   level.
//! - `packet`: the Vanilla packet name (play state, serverbound), `data`: its body in hex
//!   without the packet id. With `target`, the leading VarInt of the body (the entity id of
//!   `attack`, `interact`, `player_command`, `pick_item_from_entity`) is replaced by the
//!   network id of the scenario entity `target` when the packet is sent. `what` is a readable
//!   form for people; nobody reads it.
//!
//! Session replays (`cargo xtask parity replay`, #152) run on a copy of a recorded world:
//!
//! - `world.snapshot`: a folder with the `region/` and `poi/` files (Anvil) of an overworld
//!   instead of flat layers. Its chunks load from there; missing chunks are generated from the
//!   seed by the normal overworld generator without structures on both sides. The snapshot's
//!   entities are not loaded, and entities that chunk generation spawns are removed before
//!   frame 0: their randoms and network ids cannot match between the two servers. With
//!   `world.entities: true` (the parity world corpus, #244) the snapshot's `entities/` load
//!   with its chunks (their randoms from their UUIDs, see `entity_seed`) and nothing is
//!   removed.
//! - Players: `"game_mode"` (`survival` default, `creative`, `adventure`, `spectator`), and
//!   `"join": tick` to join before the actions of that tick instead of at the start.
//! - `"client_tick_end": false`: the harness sends no `client_tick_end` after each tick; the
//!   recorded ones are among the `packet` actions, where the client sent them.
//! - `target` of a `packet` action may also be `uuid:<32 hex digits>` (an entity of the
//!   snapshot) or `none`; a target that does not exist is the id -1 (an unknown entity). For
//!   `spectator_action` the target's UUID replaces the body.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use vanillite_base::{BlockPos, ChunkPos};

/// The traced state of a position in a chunk that is not accessible.
pub const UNLOADED: &str = "<unloaded>";

use crate::golden;

/// Where a scenario's golden lives and when it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Golden in the repository; every push.
    Fast,
    /// Golden in the bulk store; nightly.
    Bulk,
}

/// The flat world a scenario runs in.
#[derive(Clone, Debug)]
pub struct WorldSpec {
    pub seed: i64,
    /// A recorded world (`world.snapshot`) instead of the flat layers.
    pub snapshot: Option<std::path::PathBuf>,
    /// Whether the snapshot's entities load with its chunks (`world.entities`, #244).
    pub snapshot_entities: bool,
    pub biome: String,
    /// Layers bottom up: block name and thickness.
    pub layers: Vec<(String, u32)>,
    /// Game rules on top of the scenario defaults.
    pub rules: BTreeMap<String, String>,
    /// Configuration profile (`vanilla` unless the scenario says otherwise).
    pub profile: String,
    /// Day time set before the entities come.
    pub time: Option<i64>,
    /// The observed region (inclusive): its chunks and their neighbours are forced.
    pub region: (BlockPos, BlockPos),
    /// Chunks forced before the setup: the region's chunks and one chunk around them.
    pub forced: Vec<ChunkPos>,
    /// Whether the world computes light: with entities (mobs see it), random ticks (crops,
    /// grass and saplings check it) or `"light": true`. Other block-only scenarios run
    /// without it, which is much faster.
    pub light: bool,
    /// Whether frames observe light (`"light": true`).
    pub observe_light: bool,
    /// `peaceful`, `easy`, `normal` or `hard`.
    pub difficulty: String,
    /// `clear`, `rain` or `thunder`.
    pub weather: String,
    /// Whether frames observe the level random.
    pub level_rng: bool,
    /// Whether frames hold the players' food and hands.
    pub player_state: bool,
    /// Whether the harness sends `client_tick_end` for every client after each tick's packets
    /// (`"client_tick_end"`, default `true`).
    pub client_tick_end: bool,
    /// Runs with all three dimensions in Vanilla's order (`"dimensions": true`).
    pub dimensions: bool,
    /// The dimension of the region.
    pub dimension: String,
    /// More regions whose entities the frames hold (with `dimensions`).
    pub views: Vec<View>,
    /// Whether frames hold the players' sleep state, pose and respawn point.
    pub sleep_state: bool,
    /// The data pack in the world from the start (`datapacks/<name>/` of this crate).
    pub datapack: Option<String>,
    /// Whether frames hold the players' completed advancement criteria and statistics.
    pub progress_state: bool,
    /// Whether frames hold what the players' clients received (`"client_view": true`).
    pub client_view: bool,
    /// Whether the client view holds the players' own attributes (`"client_attributes":
    /// true`; the recorder then records `update_attributes`).
    pub client_attributes: bool,
    /// Whether the client view also holds level and entity events (`"client_view": "events"`).
    pub view_events: bool,
    /// Whether the End generates like a new Vanilla world (`"end": "normal"`).
    pub normal_end: bool,
    /// Whether frames hold the region's block entities (`"block_entities": true`, #244).
    pub block_entities: bool,
    /// Whether frames hold the command feedback of their tick (`"feedback": true`, #304).
    pub feedback: bool,
}

/// The overworld's dimension id.
pub const OVERWORLD: &str = "minecraft:overworld";

/// A region of another dimension whose entities the frames hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub dimension: String,
    pub min: BlockPos,
    pub max: BlockPos,
    /// Chunks forced before the setup: the view's chunks and one chunk around them.
    pub forced: Vec<ChunkPos>,
}

/// What an operation does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpKind {
    /// One block without updates.
    #[default]
    Set,
    /// One block with neighbour and shape updates.
    Place,
    /// A box: without updates in the setup, with updates as an action.
    Fill,
    /// Forces chunks (an action).
    Force,
    /// Removes the forced tickets of chunks (an action).
    Unforce,
    /// Damage to a scenario entity ([`Op::hurt`]).
    Hurt,
    /// A lightning strike in the column of `from` (an action).
    Lightning,
    /// Saves, stops and starts the server again (an action).
    Restart,
    /// A status effect on a scenario entity ([`Op::effect`]).
    Effect,
    /// A player uses an item ([`Op::use_item`]).
    UseItem,
    /// A player uses an item on an entity ([`Op::use_item`] with a `target`).
    Interact,
    /// Bone meal used on the block at `pos` (an action).
    Bonemeal,
    /// A command run as the server at the origin ([`Op::command`]).
    Command,
    /// A thrown splash or lingering potion ([`Op::throw_potion`]).
    ThrowPotion,
    /// A player uses a block with a stack ([`Op::player_use`]).
    Use,
    /// A player starts to break a block ([`Op::player_use`]).
    Attack,
    /// A serverbound play packet of a client player ([`Op::packet`]).
    Packet,
}

/// A serverbound play packet of a packet-level player (`packet` action).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketOp {
    /// Name of the client player.
    pub entity: String,
    /// Vanilla packet name without namespace, e.g. `move_player_pos`.
    pub packet: String,
    /// The packet body without its id.
    pub data: Vec<u8>,
    /// The scenario entity whose network id replaces the leading VarInt of the body.
    pub target: Option<String>,
}

impl PacketOp {
    /// The body with the network id of the target (if any) in place of its leading VarInt
    /// (`spectator_action`: the target's UUID as the body).
    pub fn body(&self, target: Option<(i32, u128)>) -> Result<Vec<u8>, String> {
        let Some((id, uuid)) = target else {
            return Ok(self.data.clone());
        };
        if self.packet == "spectator_action" {
            return Ok(uuid.to_be_bytes().to_vec());
        }
        let skip = self
            .data
            .iter()
            .position(|b| b & 0x80 == 0)
            .filter(|&i| i < 5)
            .ok_or("a packet with a target starts with a VarInt")?;
        let mut out = Vec::with_capacity(self.data.len() + 4);
        vanillite_protocol::codec::write_varint(&mut out, id);
        out.extend_from_slice(&self.data[skip + 1..]);
        Ok(out)
    }
}

/// Hex text of bytes (`data` of packet actions).
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("packet data needs an even number of hex digits".to_owned());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(text.get(i..i + 2).unwrap_or("x"), 16)
                .map_err(|_| format!("bad packet data `{text}`"))
        })
        .collect()
}

/// The item use of a `use_item`, `release_item` or `attack_entity` action.
#[derive(Clone, Debug, PartialEq)]
pub struct UseItemOp {
    /// Name of the player.
    pub entity: String,
    /// Item id, e.g. `minecraft:ender_pearl`; `None` keeps the held stack.
    pub item: Option<String>,
    pub count: i32,
    /// SNBT components of the stack.
    pub components: Option<String>,
    pub mode: UseMode,
    /// The player's `fallDistance` before the action.
    pub fall_distance: Option<f64>,
    /// The entity an `interact` action clicks (`Player.interactOn`).
    pub target: Option<String>,
    /// The player's food level set before the action.
    pub food: Option<i32>,
    /// Stacks put into inventory slots before the action.
    pub inventory: Vec<SlotStack>,
}

/// A stack for an inventory slot of a [`UseItemOp`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotStack {
    pub slot: usize,
    pub item: String,
    pub count: i32,
    /// SNBT components of the stack.
    pub components: Option<String>,
}

/// What a [`UseItemOp`] does with the held stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UseMode {
    /// `useItem`.
    Use,
    /// `releaseUsingItem`.
    Release,
    /// `Player.attack` of the named entity.
    Attack(String),
    /// The `STAB` player action (`PiercingWeapon.attack`).
    Stab,
}

/// The potion of a `throw_potion` action.
#[derive(Clone, Debug, PartialEq)]
pub struct ThrowPotionOp {
    /// `minecraft:splash_potion` or `minecraft:lingering_potion`.
    pub item: String,
    /// Potion id, e.g. `minecraft:strong_poison`.
    pub potion: String,
    pub pos: [f64; 3],
    pub motion: [f64; 3],
    /// Name of the thrower, if any.
    pub by: Option<String>,
}

/// The status effect of an `effect` action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectOp {
    /// Name of the entity.
    pub entity: String,
    /// Mob effect id, e.g. `minecraft:poison`.
    pub effect: String,
    pub duration: i32,
    pub amplifier: i32,
}

/// The damage of a `hurt` action.
#[derive(Clone, Debug, PartialEq)]
pub struct Hurt {
    /// Name of the entity hurt.
    pub entity: String,
    /// Name of the attacker, if any.
    pub by: Option<String>,
    pub amount: f32,
}

/// A known divergence from Vanilla (`scenarios/divergences/<scenario>.toml`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// The tick of the first difference.
    pub tick: u32,
    /// Why it differs.
    pub cause: String,
    /// Who fixes it.
    pub owner: String,
}

/// A scenario entity.
#[derive(Clone, Debug, PartialEq)]
pub struct EntitySpec {
    /// Name in the trace.
    pub name: String,
    /// Entity type, e.g. `minecraft:zombie`.
    pub kind: String,
    pub pos: [f64; 3],
    /// Yaw and pitch.
    pub rot: [f32; 2],
    /// Seed of the entity's random.
    pub seed: i64,
    /// Made a baby (or an adult) after spawning, if set.
    pub baby: Option<bool>,
    /// `finalizeSpawn` like `/summon`.
    pub finalize: bool,
    /// Velocity set after spawning.
    pub motion: Option<[f64; 3]>,
    /// The oxidation of a copper golem (`weather_state`), set after spawning.
    pub weather_state: Option<String>,
    /// A player driven by client packets (`"client": true`).
    pub client: bool,
    /// The player's stacks at the start: inventory slot (Vanilla `Inventory` index), item,
    /// count (`"inventory": [[0, "minecraft:stone", 64]]`).
    pub inventory: Vec<(usize, String, i32)>,
    /// The player's food level at the start (`"food"`).
    pub food: Option<i32>,
    /// The player's game mode (`"game_mode"`, default survival).
    pub game_mode: Option<String>,
    /// The tick before whose actions the player joins (`"join"`); `None` = at the start.
    pub join: Option<u32>,
}

impl EntitySpec {
    /// Whether the entity is a player.
    pub fn is_player(&self) -> bool {
        self.kind == "minecraft:player"
    }

    /// The fixed UUID of the scenario entity with index `index` (`ScenarioEntities.uuid`).
    pub fn uuid(&self, index: usize) -> u128 {
        let most = 0x5641_4E49_0000_0000_u64 | (index as u64 + 1);
        (u128::from(most) << 64) | u128::from(self.seed as u64)
    }
}

/// A setup step or an action.
///
/// Build it with `Op { tick, kind, <fields of the kind>, ..Op::default() }`, so a new field
/// (which must implement `Default`) needs no change at the other construction sites.
#[derive(Clone, Debug, Default)]
pub struct Op {
    /// Tick before which an action runs (0 for setup steps).
    pub tick: u32,
    pub kind: OpKind,
    pub from: BlockPos,
    pub to: BlockPos,
    /// Block state, `name[key=value,...]`; empty for `hurt` and chunk ops.
    pub block: String,
    /// The damage of a `hurt` action.
    pub hurt: Option<Hurt>,
    /// The effect of an `effect` action.
    pub effect: Option<EffectOp>,
    /// The item use of a `use_item` action.
    pub use_item: Option<UseItemOp>,
    /// The potion of a `throw_potion` action.
    pub throw_potion: Option<ThrowPotionOp>,
    /// The chunks of `force` and `unforce`.
    pub chunks: Vec<ChunkPos>,
    /// The command line of a `command` action.
    pub command: Option<String>,
    /// The player's click of a `use` or `attack` action.
    pub player_use: Option<PlayerUse>,
    /// The dimension of a block op (`None`: the overworld).
    pub dimension: Option<String>,
    /// The packet of a `packet` action.
    pub packet: Option<PacketOp>,
}

/// A `use` action (`ServerPlayerGameMode.useItemOn` with the stack put into the hand) or an
/// `attack` action (`handleBlockBreakAction` with `START_DESTROY_BLOCK`).
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerUse {
    /// Name of the player.
    pub entity: String,
    /// `attack` instead of `use`.
    pub attack: bool,
    /// The item's use in the air (`use_item` actions; `pos` and `face` are unused).
    pub air: bool,
    pub pos: BlockPos,
    /// The clicked face: `down`, `up`, `north`, `south`, `west` or `east`.
    pub face: String,
    /// The hit location relative to the block's corner (default: the face's centre).
    pub hit: Option<[f64; 3]>,
    /// The stack put into the hand first (`None` keeps the hand as it is).
    pub item: Option<String>,
    pub count: i32,
    /// SNBT of the stack's components, e.g. `{"minecraft:dyed_color": 255}`.
    pub components: Option<String>,
    /// The off hand instead of the main hand.
    pub offhand: bool,
    /// Sneaking (`isSecondaryUseActive`).
    pub sneak: bool,
    /// The player's food level set before the click.
    pub food: Option<i32>,
}

/// A parsed scenario.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    /// SHA-1 of the scenario file: the scenario part of its golden key.
    pub sha1: String,
    pub description: String,
    pub tier: Tier,
    pub world: WorldSpec,
    pub min: BlockPos,
    pub max: BlockPos,
    pub ticks: u32,
    pub setup: Vec<Op>,
    pub actions: Vec<Op>,
    pub entities: Vec<EntitySpec>,
    /// Seed of the stream new entities take their seeds from.
    pub entity_seed: i64,
    /// A known divergence from Vanilla.
    pub diverges: Option<Divergence>,
}

impl Scenario {
    /// Directory of the scenario files.
    pub fn dir() -> std::path::PathBuf {
        golden::crate_dir().join("scenarios")
    }

    /// Every scenario, sorted by name.
    pub fn all() -> Result<Vec<Self>, String> {
        Self::all_in(&Self::dir())
    }

    /// Directory of the soak scenarios (`cargo xtask parity soak`, [`crate::soak`]).
    pub fn soak_dir() -> std::path::PathBuf {
        Self::dir().join("soak")
    }

    /// Every scenario of a directory, sorted by name.
    pub fn all_in(dir: &Path) -> Result<Vec<Self>, String> {
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        paths.iter().map(|p| Self::load(p)).collect()
    }

    /// Reads and parses a scenario file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("{}: bad file name", path.display()))?;
        Self::parse(name, &text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parses a scenario from its JSON text.
    pub fn parse(name: &str, text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let tier = match v.get("tier").and_then(Value::as_str).unwrap_or("fast") {
            "fast" => Tier::Fast,
            "bulk" => Tier::Bulk,
            t => return Err(format!("unknown tier `{t}`")),
        };
        let world = v.get("world");
        let layers = match world.and_then(|w| w.get("layers")) {
            Some(l) => l
                .as_array()
                .ok_or("world.layers must be an array")?
                .iter()
                .map(|l| {
                    let block = l.get(0).and_then(Value::as_str);
                    let height = l.get(1).and_then(Value::as_u64);
                    match (block, height) {
                        (Some(b), Some(h)) => Ok((b.to_owned(), u32::try_from(h).unwrap_or(0))),
                        _ => Err("a layer is [block, height]".to_owned()),
                    }
                })
                .collect::<Result<_, _>>()?,
            None => vec![
                ("minecraft:bedrock".to_owned(), 1),
                ("minecraft:dirt".to_owned(), 2),
                ("minecraft:grass_block".to_owned(), 1),
            ],
        };
        let rules: BTreeMap<String, String> = v
            .get("rules")
            .and_then(Value::as_object)
            .map(|r| {
                r.iter()
                    .map(|(k, v)| {
                        let v = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                        (k.clone(), v)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let random_ticks = rules
            .get("random_tick_speed")
            .is_some_and(|speed| speed != "0");
        let region = v.get("region").ok_or("region is missing")?;
        let (min, max) = (pos(region.get(0))?, pos(region.get(1))?);
        let forced = forced_around(min, max);
        let dimensions = v
            .get("dimensions")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let dimension = v
            .get("dimension")
            .and_then(Value::as_str)
            .unwrap_or(OVERWORLD)
            .to_owned();
        let views = v
            .get("views")
            .and_then(Value::as_array)
            .map(|views| {
                views
                    .iter()
                    .map(|view| {
                        let region = view.get("region").ok_or("a view needs a region")?;
                        let (min, max) = (pos(region.get(0))?, pos(region.get(1))?);
                        Ok(View {
                            dimension: view
                                .get("dimension")
                                .and_then(Value::as_str)
                                .ok_or("a view needs a dimension")?
                                .to_owned(),
                            min,
                            max,
                            forced: forced_around(min, max),
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .transpose()?
            .unwrap_or_default();
        let datapack = v.get("datapack").and_then(Value::as_str).map(str::to_owned);
        if !dimensions && (dimension != OVERWORLD || !views.is_empty() || datapack.is_some()) {
            return Err(
                "a region in another dimension, views and a data pack need \"dimensions\""
                    .to_owned(),
            );
        }
        let progress_state = v
            .get("progress_state")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if progress_state && !dimensions {
            return Err("\"progress_state\" needs \"dimensions\"".to_owned());
        }
        let view_events = v.get("client_view").and_then(Value::as_str) == Some("events");
        let client_view = view_events
            || v.get("client_view")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let client_attributes = v
            .get("client_attributes")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let normal_end = match v.get("end").and_then(Value::as_str) {
            None | Some("flat") => false,
            Some("normal") => true,
            Some(other) => return Err(format!("unknown \"end\" {other}: flat or normal")),
        };
        if (client_view || normal_end) && !dimensions {
            return Err("\"client_view\" and \"end\" need \"dimensions\"".to_owned());
        }
        let ops = |key: &str, actions: bool| -> Result<Vec<Op>, String> {
            v.get(key)
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|o| op(o, actions)).collect())
                .unwrap_or(Ok(Vec::new()))
        };
        Ok(Self {
            name: name.to_owned(),
            sha1: golden::text_sha1(text),
            description: v
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            tier,
            world: WorldSpec {
                seed: world
                    .and_then(|w| w.get("seed"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                snapshot: world
                    .and_then(|w| w.get("snapshot"))
                    .and_then(Value::as_str)
                    .map(std::path::PathBuf::from),
                snapshot_entities: world
                    .and_then(|w| w.get("entities"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                biome: world
                    .and_then(|w| w.get("biome"))
                    .and_then(Value::as_str)
                    .unwrap_or("minecraft:plains")
                    .to_owned(),
                layers,
                rules,
                profile: v
                    .get("profile")
                    .and_then(Value::as_str)
                    .unwrap_or("vanilla")
                    .to_owned(),
                time: v.get("time").and_then(Value::as_i64),
                region: (pos(region.get(0))?, pos(region.get(1))?),
                forced,
                light: v.get("entities").is_some()
                    || random_ticks
                    || v.get("light").and_then(Value::as_bool).unwrap_or(false),
                observe_light: v.get("light").and_then(Value::as_bool).unwrap_or(false),
                difficulty: v
                    .get("difficulty")
                    .and_then(Value::as_str)
                    .unwrap_or("normal")
                    .to_owned(),
                weather: v
                    .get("weather")
                    .and_then(Value::as_str)
                    .unwrap_or("clear")
                    .to_owned(),
                level_rng: v.get("level_rng").and_then(Value::as_bool).unwrap_or(false),
                block_entities: v
                    .get("block_entities")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                feedback: v.get("feedback").and_then(Value::as_bool).unwrap_or(false),
                client_tick_end: v
                    .get("client_tick_end")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                player_state: v
                    .get("player_state")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                dimensions,
                dimension,
                views,
                sleep_state: v
                    .get("sleep_state")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                datapack,
                progress_state,
                client_view,
                client_attributes,
                view_events,
                normal_end,
            },
            min,
            max,
            ticks: v
                .get("ticks")
                .and_then(Value::as_u64)
                .and_then(|t| u32::try_from(t).ok())
                .ok_or("ticks is missing")?,
            setup: ops("setup", false)?,
            actions: ops("actions", true)?,
            entities: v
                .get("entities")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(entity).collect())
                .unwrap_or(Ok(Vec::new()))?,
            diverges: divergence(name)?,
            entity_seed: v
                .get("entity_seed")
                .and_then(Value::as_i64)
                .or_else(|| world.and_then(|w| w.get("seed")).and_then(Value::as_i64))
                .unwrap_or(0),
        })
    }
}

/// The chunks of a region and one chunk around them.
fn forced_around(min: BlockPos, max: BlockPos) -> Vec<ChunkPos> {
    let mut forced = Vec::new();
    for x in (min.x >> 4) - 1..=(max.x >> 4) + 1 {
        for z in (min.z >> 4) - 1..=(max.z >> 4) + 1 {
            forced.push(ChunkPos::new(x, z));
        }
    }
    forced
}

fn pos(v: Option<&Value>) -> Result<BlockPos, String> {
    let c = |i: usize| {
        v.and_then(|v| v.get(i))
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok())
            .ok_or_else(|| format!("bad position {v:?}"))
    };
    Ok(BlockPos::new(c(0)?, c(1)?, c(2)?))
}

/// The known divergence of a scenario from `scenarios/divergences/<name>.toml`.
fn divergence(name: &str) -> Result<Option<Divergence>, String> {
    let path = Scenario::dir()
        .join("divergences")
        .join(format!("{name}.toml"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let entry: toml::Table = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let field = |k: &str| {
        entry
            .get(k)
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
    };
    Ok(Some(Divergence {
        tick: entry
            .get("tick")
            .and_then(toml::Value::as_integer)
            .and_then(|t| u32::try_from(t).ok())
            .ok_or_else(|| format!("{}: needs a tick", path.display()))?,
        cause: field("cause").to_owned(),
        owner: field("owner").to_owned(),
    }))
}

fn vec3(v: Option<&Value>) -> Result<[f64; 3], String> {
    let c = |i: usize| {
        v.and_then(|v| v.get(i))
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("bad position {v:?}"))
    };
    Ok([c(0)?, c(1)?, c(2)?])
}

fn entity(v: &Value) -> Result<EntitySpec, String> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let kind = s("type").ok_or("an entity needs a type")?;
    let seed = v.get("seed").and_then(Value::as_i64);
    let rot = |i: usize| {
        v.get("rot")
            .and_then(|r| r.get(i))
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as f32
    };
    Ok(EntitySpec {
        name: s("name").ok_or("an entity needs a name")?,
        pos: vec3(v.get("pos"))?,
        rot: [rot(0), rot(1)],
        seed: match seed {
            Some(s) => s,
            None if kind == "minecraft:player" => 0,
            None => return Err(format!("entity {kind} needs a seed")),
        },
        baby: v.get("baby").and_then(Value::as_bool),
        finalize: v.get("finalize").and_then(Value::as_bool).unwrap_or(true),
        motion: v.get("motion").map(|m| vec3(Some(m))).transpose()?,
        weather_state: v
            .get("weather_state")
            .and_then(Value::as_str)
            .map(str::to_owned),
        client: v.get("client").and_then(Value::as_bool).unwrap_or(false),
        inventory: match v.get("inventory").and_then(Value::as_array) {
            Some(list) => list
                .iter()
                .map(|e| {
                    let slot = e.get(0).and_then(Value::as_u64);
                    let item = e.get(1).and_then(Value::as_str);
                    let count = e.get(2).and_then(Value::as_i64);
                    match (slot, item, count) {
                        (Some(s), Some(i), Some(c)) if s <= 40 => Ok((
                            usize::try_from(s).unwrap_or(0),
                            i.to_owned(),
                            i32::try_from(c).unwrap_or(1),
                        )),
                        _ => Err(format!("bad inventory entry {e}")),
                    }
                })
                .collect::<Result<_, _>>()?,
            None => Vec::new(),
        },
        food: v
            .get("food")
            .and_then(Value::as_i64)
            .and_then(|f| i32::try_from(f).ok()),
        game_mode: s("game_mode"),
        join: match v.get("join").and_then(Value::as_u64) {
            Some(t) if t >= 1 && kind == "minecraft:player" => u32::try_from(t).ok(),
            Some(_) => return Err("join needs a player and a tick >= 1".to_owned()),
            None => None,
        },
        kind,
    })
}

fn op(v: &Value, action: bool) -> Result<Op, String> {
    let kind = match v.get("op").and_then(Value::as_str) {
        Some("set") => OpKind::Set,
        Some("place") => OpKind::Place,
        Some("fill") => OpKind::Fill,
        Some("hurt") if action => OpKind::Hurt,
        Some("lightning") if action => OpKind::Lightning,
        Some("effect") if action => OpKind::Effect,
        Some("use_item" | "release_item" | "attack_entity" | "stab") if action => OpKind::UseItem,
        Some("interact") if action => OpKind::Interact,
        Some("throw_potion") if action => OpKind::ThrowPotion,
        Some("force") if action => OpKind::Force,
        Some("unforce") if action => OpKind::Unforce,
        Some("command") if action => OpKind::Command,
        Some("restart") if action => OpKind::Restart,
        Some("use") if action => OpKind::Use,
        Some("attack") if action => OpKind::Attack,
        Some("bonemeal") if action => OpKind::Bonemeal,
        Some("packet") if action => OpKind::Packet,
        o => return Err(format!("unknown op {o:?}")),
    };
    if kind == OpKind::Packet {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let tick = v
            .get("tick")
            .and_then(Value::as_u64)
            .and_then(|t| u32::try_from(t).ok())
            .filter(|&t| t >= 1)
            .ok_or("an action needs a tick >= 1")?;
        return Ok(Op {
            tick,
            kind,
            packet: Some(PacketOp {
                entity: s("entity").ok_or("packet needs an entity")?,
                packet: s("packet").ok_or("packet needs a packet name")?,
                data: from_hex(&s("data").unwrap_or_default())?,
                target: s("target"),
            }),
            ..Op::default()
        });
    }
    if matches!(kind, OpKind::Use | OpKind::Attack) {
        let tick = v
            .get("tick")
            .and_then(Value::as_u64)
            .and_then(|t| u32::try_from(t).ok())
            .filter(|&t| t >= 1)
            .ok_or("an action needs a tick >= 1")?;
        let p = pos(v.get("pos"))?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let hit = match v.get("hit").and_then(Value::as_array) {
            Some(a) if a.len() == 3 => Some([
                a[0].as_f64().ok_or("bad hit")?,
                a[1].as_f64().ok_or("bad hit")?,
                a[2].as_f64().ok_or("bad hit")?,
            ]),
            Some(_) => return Err("hit needs three numbers".to_owned()),
            None => None,
        };
        return Ok(Op {
            tick,
            kind,
            from: p,
            to: p,
            player_use: Some(PlayerUse {
                entity: s("entity").ok_or("use needs an entity")?,
                attack: kind == OpKind::Attack,
                air: false,
                pos: p,
                face: s("face").unwrap_or_else(|| "up".to_owned()),
                hit,
                item: s("item"),
                count: v
                    .get("count")
                    .and_then(Value::as_i64)
                    .and_then(|c| i32::try_from(c).ok())
                    .unwrap_or(1),
                components: s("components"),
                offhand: v.get("hand").and_then(Value::as_str) == Some("off"),
                sneak: v.get("sneak").and_then(Value::as_bool).unwrap_or(false),
                food: v
                    .get("food")
                    .and_then(Value::as_i64)
                    .and_then(|f| i32::try_from(f).ok()),
            }),
            ..Op::default()
        });
    }
    if kind == OpKind::Command {
        let tick = v
            .get("tick")
            .and_then(Value::as_u64)
            .and_then(|t| u32::try_from(t).ok())
            .filter(|&t| t >= 1)
            .ok_or("an action needs a tick >= 1")?;
        return Ok(Op {
            tick,
            kind,
            command: Some(
                v.get("command")
                    .and_then(Value::as_str)
                    .ok_or("command needs a command")?
                    .to_owned(),
            ),
            ..Op::default()
        });
    }
    if matches!(kind, OpKind::Force | OpKind::Unforce | OpKind::Restart) {
        let chunks = match v.get("chunks").and_then(Value::as_array) {
            Some(_) if kind == OpKind::Restart => return Err("restart takes no chunks".to_owned()),
            Some(list) => list
                .iter()
                .map(|c| {
                    let c = |i: usize| {
                        c.get(i)
                            .and_then(Value::as_i64)
                            .and_then(|v| i32::try_from(v).ok())
                            .ok_or_else(|| format!("bad chunk {c:?}"))
                    };
                    Ok(ChunkPos::new(c(0)?, c(1)?))
                })
                .collect::<Result<_, String>>()?,
            None if kind == OpKind::Restart => Vec::new(),
            None => return Err("a chunk op needs chunks".to_owned()),
        };
        let tick = v
            .get("tick")
            .and_then(Value::as_u64)
            .and_then(|t| u32::try_from(t).ok())
            .filter(|&t| t >= 1)
            .ok_or("an action needs a tick >= 1")?;
        return Ok(Op {
            tick,
            kind,
            chunks,
            ..Op::default()
        });
    }
    let tick = if action {
        v.get("tick")
            .and_then(Value::as_u64)
            .and_then(|t| u32::try_from(t).ok())
            .filter(|&t| t >= 1)
            .ok_or("an action needs a tick >= 1")?
    } else {
        0
    };
    if kind == OpKind::ThrowPotion {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        return Ok(Op {
            tick,
            kind,
            throw_potion: Some(ThrowPotionOp {
                item: s("item").ok_or("throw_potion needs an item")?,
                potion: s("potion").ok_or("throw_potion needs a potion")?,
                pos: vec3(v.get("pos"))?,
                motion: vec3(v.get("motion"))?,
                by: s("by"),
            }),
            ..Op::default()
        });
    }
    if kind == OpKind::Effect {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let int = |k: &str| {
            v.get(k)
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
        };
        return Ok(Op {
            tick,
            kind,
            effect: Some(EffectOp {
                entity: s("entity").ok_or("effect needs an entity")?,
                effect: s("effect").ok_or("effect needs an effect")?,
                duration: int("duration").ok_or("effect needs a duration")?,
                amplifier: int("amplifier").unwrap_or(0),
            }),
            ..Op::default()
        });
    }
    if matches!(kind, OpKind::UseItem | OpKind::Interact) {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        return Ok(Op {
            tick,
            kind,
            use_item: Some(UseItemOp {
                entity: s("entity").ok_or("use_item needs an entity")?,
                item: s("item"),
                count: v
                    .get("count")
                    .and_then(Value::as_i64)
                    .and_then(|n| i32::try_from(n).ok())
                    .unwrap_or(1),
                components: s("components"),
                mode: match v.get("op").and_then(Value::as_str) {
                    Some("release_item") => UseMode::Release,
                    Some("stab") => UseMode::Stab,
                    Some("attack_entity") => {
                        UseMode::Attack(s("target").ok_or("attack_entity needs a target")?)
                    }
                    _ => UseMode::Use,
                },
                fall_distance: v.get("fall_distance").and_then(Value::as_f64),
                food: v
                    .get("food")
                    .and_then(Value::as_i64)
                    .and_then(|f| i32::try_from(f).ok()),
                inventory: v
                    .get("inventory")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|e| {
                        Ok(SlotStack {
                            slot: e
                                .get("slot")
                                .and_then(Value::as_u64)
                                .and_then(|s| usize::try_from(s).ok())
                                .ok_or("an inventory entry needs a slot")?,
                            item: e
                                .get("item")
                                .and_then(Value::as_str)
                                .ok_or("an inventory entry needs an item")?
                                .to_owned(),
                            count: e
                                .get("count")
                                .and_then(Value::as_i64)
                                .and_then(|n| i32::try_from(n).ok())
                                .unwrap_or(1),
                            components: e
                                .get("components")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                    })
                    .collect::<Result<_, String>>()?,
                target: if kind == OpKind::Interact {
                    Some(s("target").ok_or("interact needs a target")?)
                } else {
                    None
                },
            }),
            ..Op::default()
        });
    }
    if kind == OpKind::Hurt {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        return Ok(Op {
            tick,
            kind,
            hurt: Some(Hurt {
                entity: s("entity").ok_or("hurt needs an entity")?,
                by: s("by"),
                amount: v
                    .get("amount")
                    .and_then(Value::as_f64)
                    .ok_or("hurt needs an amount")? as f32,
            }),
            ..Op::default()
        });
    }
    if matches!(kind, OpKind::Bonemeal | OpKind::Lightning) {
        let p = pos(v.get("pos"))?;
        return Ok(Op {
            tick,
            kind,
            from: p,
            to: p,
            ..Op::default()
        });
    }
    let (from, to) = if kind == OpKind::Fill {
        (pos(v.get("from"))?, pos(v.get("to"))?)
    } else {
        let p = pos(v.get("pos"))?;
        (p, p)
    };
    Ok(Op {
        tick,
        kind,
        from,
        to,
        block: v
            .get("block")
            .and_then(Value::as_str)
            .ok_or("an op needs a block")?
            .to_owned(),
        dimension: v
            .get("dimension")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ..Op::default()
    })
}
