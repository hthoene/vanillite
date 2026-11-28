//! Block breaking and placing, item use and dropping (Vanilla's `ServerPlayerGameMode` and the
//! matching handlers of `ServerGamePacketListenerImpl`).

use std::collections::HashMap;

use vanillite_base::math::{cos, floor, sin};
use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_config::GameMode;
use vanillite_data::block::{BlockKind, BlockState, flags as block_flags};
use vanillite_protocol::codec::Reader;
use vanillite_protocol::components::ids;
use vanillite_protocol::packets::play::clientbound::BlockUpdate;
use vanillite_protocol::packets::play::serverbound::{DigAction, Hand};
use vanillite_protocol::types::BlockPos as WirePos;

use super::sink::PacketSink;
use crate::advancements::trigger::Event;
use crate::block::place::{self, BlockReader, PlaceContext};
use crate::inventory::{Drop, PlayerInventory};
use crate::item::{self, ItemStack, ToolTable};
use crate::level::update;
use crate::loot::{BlockLootContext, LootTables};
use crate::player::world::{PlayLevel, SoundSource, events};
use crate::stats::{Stat, StatType, custom};

pub(crate) mod item_use;

pub use item_use::adventure_allows;
pub(crate) use item_use::flush_equips;

/// Static gameplay data the handlers read: tool rules, loot tables and enchantment ids.
#[derive(Debug, Default)]
pub struct GameData {
    /// Tool rules with resolved tags.
    pub tools: ToolTable,
    /// Block loot tables.
    pub loot: LootTables,
    /// `minecraft:enchantment` ids by name.
    pub enchantments: HashMap<String, u32>,
    /// Item tags: tag id → item ids.
    pub item_tags: HashMap<String, Vec<u16>>,
    /// Entity type tags: tag id → entity type ids.
    pub entity_tags: HashMap<String, Vec<u16>>,
    /// The combat effects of the enchantments, by registry id.
    pub enchants: crate::enchant::Enchantments,
    /// `#minecraft:enderman_holdable` block ids.
    pub enderman_holdable: Vec<u16>,
    /// Block tags: tag id → sorted block ids.
    pub block_tags: std::sync::Arc<HashMap<String, Vec<u16>>>,
    /// Recipes.
    pub recipes: crate::recipe::Recipes,
    /// Enchantment data.
    pub enchanting: crate::enchant::Enchanting,
    /// The `minecraft:trial_spawner` registry.
    pub trial_spawner_configs: crate::block::trial_spawner::TrialConfigs,
    /// The `minecraft:banner_pattern` registry.
    pub banner_patterns: crate::menu::BannerPatterns,
    /// The tags of the registries: registry → tag id (without `#`) → entry ids in tag order.
    pub registry_tags: HashMap<String, HashMap<String, Vec<u32>>>,
    /// The `minecraft:instrument` registry, by id.
    pub instruments: Vec<super::items::Instrument>,
    /// The `minecraft:painting_variant` registry: width and height by id.
    pub painting_variants: Vec<(i32, i32)>,
    /// The `minecraft:context_int_provider` registry (compostable layers).
    pub compostables: HashMap<String, crate::block::composter::IntProvider>,
    /// The `minecraft:villager_trade` and `minecraft:trade_set` registries.
    pub trades: crate::trading::data::TradeData,
    /// The `minecraft:jukebox_song` registry, by registry id.
    pub jukebox_songs: Vec<crate::block::jukebox::JukeboxSong>,
}

impl crate::predicate::EntityTags for GameData {
    fn entity_in_tag(&self, kind: vanillite_data::entity::EntityTypeId, tag: &str) -> bool {
        self.entity_tags
            .get(tag)
            .is_some_and(|v| v.contains(&kind.0))
    }
}

static GAME_DATA: std::sync::RwLock<Option<std::sync::Arc<GameData>>> =
    std::sync::RwLock::new(None);

/// Installs the gameplay data for code that has no player context (block drops of the level
/// simulation); `/reload` installs the reloaded data the same way.
pub fn install_game_data(data: std::sync::Arc<GameData>) {
    *GAME_DATA
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(data);
}

/// The installed gameplay data, if any. Callers keep the returned data for one operation, so
/// a reload never changes the data in the middle of it.
pub fn game_data() -> Option<std::sync::Arc<GameData>> {
    GAME_DATA
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

impl GameData {
    /// Level of the enchantment `name` on `stack`.
    pub fn enchantment_level(&self, stack: &ItemStack, name: &str) -> i32 {
        let Some(&id) = self.enchantments.get(name) else {
            return 0;
        };
        item::enchantments(stack)
            .into_iter()
            .find(|(e, _)| *e == id)
            .map_or(0, |(_, level)| level)
    }

    /// Whether a block (by block id) is in a block tag.
    pub fn block_in_tag(&self, block: u16, tag: &str) -> bool {
        self.block_tags
            .get(tag)
            .is_some_and(|v| v.binary_search(&block).is_ok())
    }

    /// Whether an item is in an item tag.
    pub fn item_in_tag(&self, item: u16, tag: &str) -> bool {
        self.item_tags.get(tag).is_some_and(|v| v.contains(&item))
    }

    /// One holder set entry of `registry` (`"ns:id"` or `"#ns:tag"`) as entry ids, in tag
    /// order; unknown entries give none.
    pub fn holder_set(&self, registry: &str, entry: &str) -> Vec<u32> {
        if let Some(tag) = entry.strip_prefix('#') {
            return self
                .registry_tags
                .get(registry)
                .and_then(|tags| tags.get(tag))
                .cloned()
                .unwrap_or_default();
        }
        if registry == "minecraft:enchantment" {
            return self.enchantments.get(entry).copied().into_iter().collect();
        }
        if let Some(r) = vanillite_data::registry::Registry::by_name(registry) {
            return r.id(entry).map(u32::from).into_iter().collect();
        }
        use vanillite_protocol::components_nbt::RegistryNames;
        vanillite_protocol::item::InstalledRegistries
            .entry_id(registry, entry)
            .and_then(|id| u32::try_from(id).ok())
            .into_iter()
            .collect()
    }
}

/// The acting player.
#[derive(Clone, Copy, Debug)]
pub struct Actor {
    /// Entity id in the world (`None` in tests without a world).
    pub entity: Option<vanillite_world::entity::EntityId>,
    /// Entity network id.
    pub network_id: i32,
    /// UUID.
    pub uuid: u128,
    /// Feet position.
    pub pos: Vec3,
    /// Eye height of the current pose.
    pub eye_height: f64,
    /// Yaw.
    pub y_rot: f32,
    /// Pitch.
    pub x_rot: f32,
    /// On ground.
    pub on_ground: bool,
    /// Sneaking (`isSecondaryUseActive`).
    pub sneaking: bool,
    /// Game mode.
    pub game_mode: GameMode,
    /// Operator permission level (0–4).
    pub permission_level: u8,
    /// Whether the player's eyes are in water.
    pub eyes_in_water: bool,
    /// `MobEffectUtil.getDigSpeedAmplification` when Haste or Conduit Power is active.
    pub dig_speed: Option<i32>,
    /// The amplifier of Mining Fatigue, if active.
    pub mining_fatigue: Option<i32>,
    /// The player's respawn position if it is in this world.
    pub respawn_here: Option<BlockPos>,
    /// The player holds the lead of some mob (`FenceBlock.useWithoutItem` binds them).
    pub leads_mobs: bool,
    /// `blockInteractionRange()` and `entityInteractionRange()` (see [`interaction_ranges`]).
    pub ranges: (f64, f64),
}

/// The `block_interaction_range` (base 4.5) and `entity_interaction_range` (base 3) of a
/// player: its equipment's modifiers and the creative mode ones (+0.5 / +2,
/// `Player.updatePlayerAttributes`).
pub fn interaction_ranges(inventory: &PlayerInventory, game_mode: GameMode) -> (f64, f64) {
    let creative = game_mode == GameMode::Creative;
    let range = |name: &str, base: f64, bonus: f64| {
        let mut modifiers = Vec::new();
        for slot in super::encode::EQUIPMENT_SLOTS {
            item::attribute_modifiers(inventory.equipment(slot), slot, name, &mut modifiers);
        }
        if creative {
            modifiers.push((0, bonus));
        }
        item::attribute_value(base, &modifiers, 0.0, 64.0)
    };
    (
        range("minecraft:block_interaction_range", 4.5, 0.5),
        range("minecraft:entity_interaction_range", 3.0, 2.0),
    )
}

impl Actor {
    fn creative(&self) -> bool {
        self.game_mode == GameMode::Creative
    }

    fn eye(&self) -> Vec3 {
        Vec3::new(self.pos.x, self.pos.y + self.eye_height, self.pos.z)
    }

    /// `blockInteractionRange`.
    fn block_range(&self) -> f64 {
        self.ranges.0
    }

    /// `isWithinBlockInteractionRange(pos, buffer)`.
    pub fn within_block_range(&self, pos: BlockPos, buffer: f64) -> bool {
        let max = self.block_range() + buffer;
        aabb_distance_sqr(&Aabb::of_block(pos), self.eye()) < max * max
    }

    /// `isWithinEntityInteractionRange(box, buffer)`.
    pub fn within_entity_range(&self, aabb: &Aabb, buffer: f64) -> bool {
        let max = self.ranges.1 + buffer;
        aabb_distance_sqr(aabb, self.eye()) < max * max
    }
}

/// `AABB.distanceToSqr(Vec3)`.
fn aabb_distance_sqr(b: &Aabb, p: Vec3) -> f64 {
    let dx = (b.min_x - p.x).max(p.x - b.max_x).max(0.0);
    let dy = (b.min_y - p.y).max(p.y - b.max_y).max(0.0);
    let dz = (b.min_z - p.z).max(p.z - b.max_z).max(0.0);
    dx * dx + dy * dy + dz * dz
}

/// What a play handler needs besides the connection.
pub struct PlayContext<'a> {
    /// The world.
    pub level: &'a mut dyn PlayLevel,
    /// The player's inventory.
    pub inventory: &'a mut PlayerInventory,
    /// The player's experience (`None` in tests).
    pub xp: Option<&'a mut crate::survival::player::Experience>,
    /// The player's own random (`Entity.random`; `None` in tests: the level random).
    pub random: Option<&'a mut vanillite_base::random::LegacyRandom>,
    /// The player's food (`None` in tests).
    pub food: Option<&'a mut crate::survival::food::FoodData>,
    /// The player's recipe book (`None` in tests).
    pub recipe_book: Option<&'a mut crate::recipe::book::PlayerRecipeBook>,
    /// The player.
    pub actor: Actor,
    /// Gameplay data.
    pub data: &'a GameData,
    /// Whether a block is protected around the spawn for this player.
    pub spawn_protected: &'a dyn Fn(BlockPos) -> bool,
    /// Stacks the player drops this tick.
    pub drops: &'a mut Vec<Drop>,
    /// Hand swings with whether they use the main hand's `attack_animation` (a punch; else
    /// `SwingAnimation.DEFAULT`).
    pub swings: &'a mut Vec<(Hand, bool)>,
    /// Plugin hooks (block breaking and placing).
    pub hooks: &'a dyn crate::hooks::GameHooks,
    /// The player's statistics (`None` in tests).
    pub stats: Option<&'a mut crate::stats::StatsCounter>,
    /// The player's advancements (`None` in tests).
    pub advancements: Option<&'a mut crate::advancements::PlayerAdvancements>,
}

impl PlayContext<'_> {
    /// `ServerPlayer.awardStat`.
    pub fn award_stat(&mut self, stat: Stat, count: i32) {
        if let Some(stats) = self.stats.as_deref_mut() {
            stats.award(stat, count);
        }
    }

    /// Fires a criterion trigger of the player.
    pub fn trigger(&mut self, event: Event) {
        if let Some(advancements) = self.advancements.as_deref_mut() {
            advancements.trigger(event);
        }
    }
}

impl std::fmt::Debug for PlayContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayContext")
            .field("actor", &self.actor)
            .finish_non_exhaustive()
    }
}

/// `Direction.from3DDataValue(abs(v % 6))`.
pub fn direction_from_byte(v: u8) -> Direction {
    Direction::ALL
        .get(usize::from(v % 6))
        .copied()
        .unwrap_or(Direction::Down)
}

/// `Direction.BY_ID` with `WRAP`.
pub fn direction_wrapped(v: i32) -> Direction {
    Direction::ALL
        .get(usize::try_from(v.rem_euclid(6)).unwrap_or(0))
        .copied()
        .unwrap_or(Direction::Down)
}

fn relative(pos: BlockPos, d: Direction) -> BlockPos {
    let [x, y, z] = d.normal();
    BlockPos::new(
        pos.x.wrapping_add(x),
        pos.y.wrapping_add(y),
        pos.z.wrapping_add(z),
    )
}

fn wire(pos: BlockPos) -> WirePos {
    WirePos {
        x: pos.x,
        y: pos.y,
        z: pos.z,
    }
}

/// `ServerPlayer.sendSpawnProtectionMessage`.
/// A hand as plugins number it (0 main, 1 off).
fn hand_index(hand: Hand) -> u32 {
    match hand {
        Hand::MainHand => 0,
        Hand::OffHand => 1,
    }
}

/// The item id of the stack in a hand, e.g. `minecraft:stick`.
fn held_name(ctx: &PlayContext<'_>, hand: Hand) -> &'static str {
    let stack = match hand {
        Hand::MainHand => ctx.inventory.selected_item(),
        Hand::OffHand => &ctx.inventory.offhand,
    };
    item::item_id(stack).name()
}

fn spawn_protection_message(sink: &dyn PacketSink, pos: BlockPos) {
    use vanillite_protocol::text::{Arg, Color, Component, NamedColor};
    sink.send(
        &vanillite_protocol::packets::play::clientbound::SystemChat {
            content: Component::translatable_args(
                "build.spawn_protection",
                vec![Arg::String(format!("{}, {}, {}", pos.x, pos.y, pos.z))],
            )
            .color(Color::Named(NamedColor::Red)),
            overlay: true,
        },
    );
}

/// `ServerPlayer.sendBuildLimitMessage`.
fn build_limit_message(sink: &dyn PacketSink, too_high: bool, limit: i32) {
    use vanillite_protocol::text::{Arg, Color, Component, NamedColor};
    let key = if too_high {
        "build.tooHigh"
    } else {
        "build.tooLow"
    };
    sink.send(
        &vanillite_protocol::packets::play::clientbound::SystemChat {
            content: Component::translatable_args(key, vec![Arg::Int(limit)])
                .color(Color::Named(NamedColor::Red)),
            overlay: true,
        },
    );
}

fn block_update(sink: &dyn PacketSink, level: &dyn PlayLevel, pos: BlockPos) {
    sink.send(&BlockUpdate {
        pos: wire(pos),
        state: i32::from(level.block(pos).0),
    });
}

/// Blocks only operators in creative may break or use (`GameMasterBlock`).
fn is_game_master_block(state: BlockState) -> bool {
    crate::block::technical::is_game_master_block(state)
}

/// Blocks whose `useWithoutItem`/`useItemOn` consume a click (doors, levers, containers, …).
/// Their interactions are not implemented yet; the click is consumed so no block is placed
/// against them, and the client's prediction is corrected by the block updates.
fn has_use_action(state: BlockState) -> bool {
    matches!(
        state.block().info().kind,
        BlockKind::DoorBlock
            | BlockKind::TrapDoorBlock
            | BlockKind::FenceGateBlock
            | BlockKind::ButtonBlock
            | BlockKind::LeverBlock
            | BlockKind::ChestBlock
            | BlockKind::TrappedChestBlock
            | BlockKind::CopperChestBlock
            | BlockKind::WeatheringCopperChestBlock
            | BlockKind::EnderChestBlock
            | BlockKind::BarrelBlock
            | BlockKind::ShulkerBoxBlock
            | BlockKind::FurnaceBlock
            | BlockKind::BlastFurnaceBlock
            | BlockKind::SmokerBlock
            | BlockKind::CraftingTableBlock
            | BlockKind::CrafterBlock
            | BlockKind::AnvilBlock
            | BlockKind::NoteBlock
            | BlockKind::RepeaterBlock
            | BlockKind::ComparatorBlock
            | BlockKind::DispenserBlock
            | BlockKind::DropperBlock
            | BlockKind::HopperBlock
            | BlockKind::EnchantingTableBlock
            | BlockKind::BrewingStandBlock
            | BlockKind::LoomBlock
            | BlockKind::CartographyTableBlock
            | BlockKind::GrindstoneBlock
            | BlockKind::StonecutterBlock
            | BlockKind::SmithingTableBlock
            | BlockKind::BeaconBlock
            | BlockKind::LecternBlock
            | BlockKind::DaylightDetectorBlock
            | BlockKind::CakeBlock
            | BlockKind::BellBlock
    )
}

/// Block access for the placement rules.
struct LevelReader<'a>(&'a dyn PlayLevel);

impl BlockReader for LevelReader<'_> {
    fn state(&self, pos: BlockPos) -> BlockState {
        self.0.block(pos)
    }

    fn min_y(&self) -> i32 {
        self.0.min_y()
    }

    fn max_y(&self) -> i32 {
        self.0.max_y().saturating_add(1)
    }
}

/// `ServerPlayerGameMode`: the digging state of a player.
#[derive(Clone, Debug, Default)]
pub struct Digging {
    game_ticks: i32,
    is_destroying: bool,
    destroy_progress_start: i32,
    destroy_pos: Option<BlockPos>,
    destroy_direction: u8,
    has_delayed_destroy: bool,
    delayed_destroy_pos: BlockPos,
    delayed_tick_start: i32,
    /// Last game tick at which a delayed destroy may still complete (`fast_break_check`).
    delayed_deadline: i32,
    last_sent_state: i32,
    /// The world rule `anticheat.digging.fast-break-check`: stops are checked against the real
    /// time spent digging instead of Vanilla's start tick, too-fast breaks are rejected.
    pub fast_break_check: bool,
    /// Highest block change sequence to acknowledge this tick (`ackBlockChangesUpTo`).
    pub ack_up_to: i32,
    /// `dropSpamThrottler` for creative drops.
    drop_spam: i32,
    /// Whether the last `use_item_on` reached the item's own use on entities (spawn eggs,
    /// leads, minecarts): no block reacted and the item has no block use of its own.
    pub entity_item_use: bool,
    /// `FenceBlock.useWithoutItem` of the last `use_item_on`: the fence the player's leashed
    /// mobs get tied to (the world runs it next).
    pub bind_fence: Option<BlockPos>,
    /// `BlockItem.useOn` fell back to `Item.use` of a consumable block item (carrots, berries)
    /// that could not be placed: the world starts using it with this hand next.
    pub start_using: Option<Hand>,
}

/// `getDestroyProgress` of `state` for the actor.
fn destroy_progress(ctx: &PlayContext<'_>, state: BlockState) -> f32 {
    let speed = state.misc().destroy_speed;
    if speed == -1.0 {
        return 0.0;
    }
    let tool = ctx.inventory.selected_item();
    let correct = !state.has(block_flags::REQUIRES_CORRECT_TOOL)
        || ctx.data.tools.is_correct_for_drops(tool, state);
    let attribute = |name: &str, base: f64| {
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let value = equipment_attribute(ctx.inventory, name, base) as f32;
        value
    };
    let player_speed = crate::inventory::destroy_speed(
        &ctx.data.tools,
        tool,
        state,
        attribute("minecraft:mining_efficiency", 0.0),
        ctx.actor.dig_speed,
        ctx.actor.mining_fatigue,
        attribute("minecraft:block_break_speed", 1.0),
        ctx.actor
            .eyes_in_water
            .then(|| attribute("minecraft:submerged_mining_speed", 0.2)),
        ctx.actor.on_ground,
    );
    let modifier = if correct { 30.0 } else { 100.0 };
    player_speed / speed / modifier
}

/// `getAttributeValue(name)` of a player whose attribute `name` has the base value `base`:
/// the modifiers of its equipment and of their enchantments (`minecraft:attributes`).
pub fn equipment_attribute(inventory: &PlayerInventory, name: &str, base: f64) -> f64 {
    let mut modifiers = Vec::new();
    for slot in super::encode::EQUIPMENT_SLOTS {
        item::attribute_modifiers(inventory.equipment(slot), slot, name, &mut modifiers);
    }
    let attribute = item::attribute_id(name)
        .and_then(|id| u8::try_from(id).ok())
        .map(crate::effect::attribute::Attribute);
    let value = item::attribute_value(base, &modifiers, f64::MIN, f64::MAX);
    attribute.map_or(value, |a| a.sanitize(value))
}

/// `Player.getDestroySpeed`'s effect part: Haste / Conduit Power speed it up by 20 % per level,
/// Mining Fatigue slows it to `0.3^(level)`.
pub fn dig_effects(speed: f32, dig_speed: Option<i32>, fatigue: Option<i32>) -> f32 {
    let mut speed = speed;
    if let Some(amplification) = dig_speed {
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let factor = 1.0 + (amplification as f32 + 1.0) * 0.2;
        speed *= factor;
    }
    if let Some(amplifier) = fatigue {
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let scale = 0.3_f64.powi(amplifier.saturating_add(1)) as f32;
        speed *= scale;
    }
    speed
}

/// Ticks beyond the dig time seen so far within which a delayed destroy must complete under
/// the fast-break check (network jitter between the start and the stop packet).
const FAST_BREAK_GRACE_TICKS: i32 = 20;

/// `(int)(progress * 10.0F)`.
fn crack_state(progress: f32) -> i32 {
    let v = progress * 10.0;
    if v.is_nan() {
        0
    } else {
        // Saturating float-to-int like Java's `(int)`.
        #[allow(clippy::as_conversions)]
        let i = v as i32;
        i
    }
}

impl Digging {
    /// The state of a joining player (`ackBlockChangesUpTo = -1`).
    pub fn new() -> Digging {
        Digging {
            ack_up_to: -1,
            last_sent_state: -1,
            ..Digging::default()
        }
    }

    /// `ackBlockChangesUpTo`.
    fn ack(&mut self, sequence: i32) {
        self.ack_up_to = self.ack_up_to.max(sequence);
    }

    /// `ServerPlayerGameMode.tick`.
    pub fn tick(&mut self, ctx: &mut PlayContext<'_>, sink: &dyn PacketSink) {
        self.game_ticks = self.game_ticks.wrapping_add(1);
        // `dropSpamThrottler.tick()`: one less per tick.
        self.drop_spam = self.drop_spam.saturating_sub(1).max(0);
        if self.has_delayed_destroy {
            let pos = self.delayed_destroy_pos;
            let state = ctx.level.block(pos);
            if state.is_air() {
                self.has_delayed_destroy = false;
            } else {
                // Vanilla passes `delayedTickStart` itself as the ticks spent: the block breaks
                // on the first tick after the stop once the game mode is old enough. The
                // fast-break check counts the ticks since the dig started instead.
                let ticks = if self.fast_break_check {
                    self.game_ticks.wrapping_sub(self.delayed_tick_start)
                } else {
                    self.delayed_tick_start
                };
                let progress = self.increment_progress(ctx, state, pos, ticks);
                if progress >= 1.0 {
                    self.has_delayed_destroy = false;
                    self.destroy_block(ctx, pos, sink);
                } else if self.fast_break_check
                    && self.game_ticks.wrapping_sub(self.delayed_deadline) > 0
                {
                    // Too fast: the client took the block as broken, it gets it back.
                    self.has_delayed_destroy = false;
                    ctx.level.destroy_progress(ctx.actor.network_id, pos, -1);
                    self.last_sent_state = -1;
                    block_update(sink, ctx.level, pos);
                }
            }
        } else if self.is_destroying
            && let Some(pos) = self.destroy_pos
        {
            let state = ctx.level.block(pos);
            if state.is_air() {
                ctx.level.destroy_progress(ctx.actor.network_id, pos, -1);
                self.last_sent_state = -1;
                self.is_destroying = false;
            } else {
                let ticks = self.game_ticks.wrapping_sub(self.destroy_progress_start);
                let event = if ticks % 4 == 0 {
                    events::PARTICLES_AND_SOUND_BLOCK_DESTROY_PROGRESS
                } else {
                    events::PARTICLES_BLOCK_DESTROY_PROGRESS
                };
                self.increment_progress(ctx, state, pos, ticks);
                ctx.level
                    .level_event(None, event, pos, i32::from(self.destroy_direction));
            }
        }
    }

    fn increment_progress(
        &mut self,
        ctx: &mut PlayContext<'_>,
        state: BlockState,
        pos: BlockPos,
        ticks: i32,
    ) -> f32 {
        #[allow(clippy::as_conversions)]
        let factor = ticks.saturating_add(1) as f32;
        let progress = destroy_progress(ctx, state) * factor;
        let crack = crack_state(progress);
        if crack != self.last_sent_state {
            ctx.level.destroy_progress(ctx.actor.network_id, pos, crack);
            self.last_sent_state = crack;
        }
        progress
    }

    /// `handlePlayerAction` for the digging actions, then `ackBlockChangesUpTo`.
    pub fn handle_action(
        &mut self,
        ctx: &mut PlayContext<'_>,
        action: DigAction,
        pos: BlockPos,
        direction: u8,
        sequence: i32,
        sink: &dyn PacketSink,
    ) {
        self.handle_block_break_action(ctx, action, pos, direction, sink);
        self.ack(sequence.max(0));
    }

    fn handle_block_break_action(
        &mut self,
        ctx: &mut PlayContext<'_>,
        action: DigAction,
        pos: BlockPos,
        direction: u8,
        sink: &dyn PacketSink,
    ) {
        if !ctx.actor.within_block_range(pos, 1.0) {
            if self.is_destroying && action == DigAction::AbortDestroyBlock {
                self.abort(ctx, pos);
            }
            return;
        }
        if pos.y > ctx.level.max_y() {
            block_update(sink, ctx.level, pos);
            return;
        }
        match action {
            DigAction::StartDestroyBlock => {
                if (ctx.spawn_protected)(pos) {
                    spawn_protection_message(sink, pos);
                    return;
                }
                if !ctx.level.within_border(pos) {
                    block_update(sink, ctx.level, pos);
                    return;
                }
                if !ctx.level.is_loaded(pos) {
                    block_update(sink, ctx.level, pos);
                    return;
                }
                // Bukkit's `PlayerInteractEvent` `LEFT_CLICK_BLOCK`: a cancelled hit leaves
                // the block as it is and corrects the client.
                if ctx.hooks.wants(vanillite_abi::event::PLAYER_INTERACT)
                    && !ctx.hooks.player_interact(
                        ctx.level.hook_world(),
                        ctx.actor.uuid,
                        vanillite_abi::interact::LEFT_CLICK_BLOCK,
                        hand_index(Hand::MainHand),
                        held_name(ctx, Hand::MainHand),
                        Some((pos, u32::from(direction % 6), ctx.level.block(pos))),
                    )
                {
                    block_update(sink, ctx.level, pos);
                    return;
                }
                if ctx.actor.creative() {
                    self.destroy_and_ack(ctx, pos, sink);
                    return;
                }
                if block_action_restricted(ctx, pos) {
                    block_update(sink, ctx.level, pos);
                    return;
                }
                self.destroy_progress_start = self.game_ticks;
                let state = ctx.level.block(pos);
                let mut progress = 1.0;
                if !state.is_air() {
                    if let Some(player) = ctx.actor.entity
                        && let Some(level) = ctx.level.level_mut()
                    {
                        level.attack_block(pos, player);
                    }
                    progress = destroy_progress(ctx, state);
                }
                if !state.is_air() && progress >= 1.0 {
                    self.destroy_and_ack(ctx, pos, sink);
                } else {
                    if self.is_destroying
                        && let Some(old) = self.destroy_pos
                    {
                        block_update(sink, ctx.level, old);
                    }
                    self.is_destroying = true;
                    self.destroy_pos = Some(pos);
                    self.destroy_direction = direction % 6;
                    let crack = crack_state(progress);
                    ctx.level.destroy_progress(ctx.actor.network_id, pos, crack);
                    self.last_sent_state = crack;
                }
            }
            DigAction::ChangeDestroyDirection => self.destroy_direction = direction % 6,
            DigAction::StopDestroyBlock => {
                // Fast-break check: only a running dig can be finished.
                if self.fast_break_check && !(self.is_destroying && Some(pos) == self.destroy_pos) {
                    block_update(sink, ctx.level, pos);
                    return;
                }
                if Some(pos) == self.destroy_pos {
                    let ticks = self.game_ticks.wrapping_sub(self.destroy_progress_start);
                    let state = ctx.level.block(pos);
                    if !state.is_air() {
                        #[allow(clippy::as_conversions)]
                        let factor = ticks.saturating_add(1) as f32;
                        let progress = destroy_progress(ctx, state) * factor;
                        // Vanilla accepts 70 % to allow for latency (fast-break check).
                        if progress >= 0.7 {
                            self.is_destroying = false;
                            ctx.level.destroy_progress(ctx.actor.network_id, pos, -1);
                            self.destroy_and_ack(ctx, pos, sink);
                            return;
                        }
                        if !self.has_delayed_destroy {
                            self.is_destroying = false;
                            self.has_delayed_destroy = true;
                            self.delayed_destroy_pos = pos;
                            self.delayed_tick_start = self.destroy_progress_start;
                            // Latency allowance: as long again as the dig took, plus a second.
                            self.delayed_deadline = self
                                .game_ticks
                                .wrapping_add(ticks.max(0))
                                .wrapping_add(FAST_BREAK_GRACE_TICKS);
                        } else if self.fast_break_check {
                            self.is_destroying = false;
                            ctx.level.destroy_progress(ctx.actor.network_id, pos, -1);
                            block_update(sink, ctx.level, pos);
                        }
                    }
                }
            }
            DigAction::AbortDestroyBlock => self.abort(ctx, pos),
            _ => {}
        }
    }

    fn abort(&mut self, ctx: &mut PlayContext<'_>, pos: BlockPos) {
        self.is_destroying = false;
        if let Some(old) = self.destroy_pos
            && old != pos
        {
            ctx.level.destroy_progress(ctx.actor.network_id, old, -1);
        }
        ctx.level.destroy_progress(ctx.actor.network_id, pos, -1);
    }

    fn destroy_and_ack(&mut self, ctx: &mut PlayContext<'_>, pos: BlockPos, sink: &dyn PacketSink) {
        if !self.destroy_block(ctx, pos, sink) {
            block_update(sink, ctx.level, pos);
        }
    }

    /// `ServerPlayerGameMode.destroyBlock`.
    fn destroy_block(
        &mut self,
        ctx: &mut PlayContext<'_>,
        pos: BlockPos,
        sink: &dyn PacketSink,
    ) -> bool {
        let state = ctx.level.block(pos);
        let tool = ctx.inventory.selected_item().clone();
        if item::item_id(&tool) == vanillite_data::item::ids::DEBUG_STICK {
            // `DebugStickItem.canDestroyBlock`: selects the next property instead.
            item_use::debug_stick(ctx, pos, false, Hand::MainHand, sink);
            return false;
        }
        if ctx.actor.creative() && !ctx.data.tools.can_destroy_in_creative(&tool) {
            return false;
        }
        if is_game_master_block(state) && !(ctx.actor.creative() && ctx.actor.permission_level >= 2)
        {
            block_update(sink, ctx.level, pos);
            return false;
        }
        if block_action_restricted(ctx, pos) {
            return false;
        }
        if !ctx
            .hooks
            .block_break(ctx.level.hook_world(), ctx.actor.uuid, pos, state)
        {
            block_update(sink, ctx.level, pos);
            return false;
        }
        // `CreakingHeartBlock.playerWillDestroy`: the creaking dies by the player's attack.
        if state.block() == vanillite_data::block::ids::CREAKING_HEART
            && let Some(player) = ctx.actor.entity
            && let Some(level) = ctx.level.level_mut()
        {
            let prevents = ctx.actor.creative();
            crate::block::creaking_heart::player_will_destroy(level, pos, state, player, prevents);
        }
        // `TripWireBlock.playerWillDestroy`: shears in the main hand disarm the string first.
        if state.block() == vanillite_data::block::ids::TRIPWIRE
            && let Some(level) = ctx.level.level_mut()
        {
            let shears = !tool.is_empty() && tool.item == vanillite_data::item::ids::SHEARS.0;
            crate::block::tripwire::wire_player_will_destroy(
                level,
                pos,
                state,
                shears,
                ctx.actor.entity,
            );
        }
        // `TntBlock` / `PistonHeadBlock.playerWillDestroy`: unstable TNT primes, a creative
        // player's piston head takes the base along.
        if let Some(level) = ctx.level.level_mut() {
            crate::block::break_hooks::player_will_destroy(level, state, pos, ctx.actor.creative());
        }
        // `DecoratedPotBlock.playerWillDestroy`: a tool that breaks pots cracks it (unless
        // enchanted against it); the cracked pot then breaks into its sherds.
        let state = if state.block() == vanillite_data::block::ids::DECORATED_POT
            && ctx
                .data
                .item_in_tag(tool.item, "minecraft:breaks_decorated_pots")
            && !crate::block::break_hooks::has_enchantment_tag(
                &tool,
                "minecraft:prevents_decorated_pot_shattering",
            ) {
            let cracked = vanillite_worldgen::behaviour::StateExt::set_flag(state, "cracked", true);
            ctx.level.set_block(
                pos,
                cracked,
                update::INVISIBLE | update::SKIP_BLOCK_ENTITY_SIDEEFFECTS,
            );
            cracked
        } else {
            state
        };
        // `Block.playerWillDestroy`: breaking a block guarded by piglins angers them.
        if crate::block::place::tags::GUARDED_BY_PIGLINS.has(state)
            && let Some(player) = ctx.actor.entity
            && let Some(level) = ctx.level.level_mut()
        {
            level.output().piglin_anger.push((player, false));
        }
        // `DoublePlantBlock.playerWillDestroy`: the broken half drops while the other half is
        // still there (the `location_check` of tall grass and large ferns); `playerDestroy`
        // then drops nothing.
        let double_plant = is_double_plant(state);
        if double_plant && !ctx.actor.creative() {
            drop_resources(ctx, state, pos, &tool, None, None);
        }
        // `playerWillDestroy`: break particles and sound for everyone else, and the other half
        // of double blocks. `BaseFireBlock` sends the extinguish sound to everyone instead.
        if crate::block::is_fire(state) {
            ctx.level.level_event(None, 1009, pos, 0);
        } else {
            ctx.level.level_event(
                Some(ctx.actor.network_id),
                events::PARTICLES_DESTROY_BLOCK,
                pos,
                i32::from(state.0),
            );
        }
        // `DoorBlock.playerWillDestroy`: also without the correct tool for drops the lower
        // half goes without dropping (`preventDropFromBottomPart`).
        let door = matches!(
            state.block().info().kind,
            BlockKind::DoorBlock | BlockKind::WeatheringCopperDoorBlock
        );
        let prevents = ctx.actor.creative()
            || door
                && state.has(block_flags::REQUIRES_CORRECT_TOOL)
                && !ctx.data.tools.is_correct_for_drops(&tool, state);
        let mut extra = Vec::new();
        place::destroy_extra(state, pos, &LevelReader(&*ctx.level), prevents, &mut extra);
        for (p, s) in extra {
            let old = ctx.level.block(p);
            if ctx
                .level
                .set_block(p, s, update::ALL | update::SUPPRESS_DROPS)
            {
                ctx.level.level_event(
                    Some(ctx.actor.network_id),
                    events::PARTICLES_DESTROY_BLOCK,
                    p,
                    i32::from(old.0),
                );
            }
        }
        // `ShulkerBoxBlock.playerWillDestroy`: the loot table is rolled for the player (in
        // creative by `isEmpty`, without the player).
        if crate::block::container::kind_of(state.block())
            == Some(crate::block::container::Kind::ShulkerBox)
            && let Some(level) = ctx.level.level_mut()
        {
            let player = if ctx.actor.creative() {
                None
            } else {
                ctx.actor.entity
            };
            crate::block::chest_loot::unpack(level, pos, player);
        }
        // The block entity as `playerDestroy` sees it (captured before the removal).
        let entity = ctx
            .level
            .level_mut()
            .and_then(|l| l.block_entity(pos).cloned());
        let kept = ctx
            .level
            .level_mut()
            .and_then(|l| l.block_entities.kept_components(pos).cloned());
        if ctx.actor.creative()
            && let Some(entity) = &entity
            && let Some(level) = ctx.level.level_mut()
        {
            // `ShulkerBoxBlock.playerWillDestroy`: a creative player still gets a filled box.
            crate::block::container::creative_shulker_drop(level, pos, state, entity);
            // `BeehiveBlock.playerWillDestroy`: a hive with bees or honey drops as an item.
            if crate::block::beehive::is_beehive(state.block()) {
                crate::block::beehive::creative_drop(level, pos, state);
            }
        }
        ctx.level.game_event(
            events_ids().block_destroy,
            pos,
            ctx.actor.entity,
            Some(state),
        );
        // `removeBlock`: the fluid of a waterlogged block stays.
        let replacement = state.fluid_state().block_state();
        let changed = ctx.level.set_block(pos, replacement, update::ALL);
        // `Block.destroy`: a moving block takes an extended piston base behind it along.
        if changed
            && state.block() == vanillite_data::block::ids::MOVING_PISTON
            && let Some(level) = ctx.level.level_mut()
        {
            crate::block::piston::moving_destroyed(level, pos, state);
        }
        if ctx.actor.creative() {
            return true;
        }
        let correct = !state.has(block_flags::REQUIRES_CORRECT_TOOL)
            || ctx.data.tools.is_correct_for_drops(&tool, state);
        self.mine_block(ctx, state);
        if changed && correct {
            // `Block.playerDestroy`: the statistic, exhaustion, the drops.
            ctx.award_stat(Stat::of(StatType::Mined, state.block().0), 1);
            if !ctx.actor.creative()
                && let Some(food) = ctx.food.as_deref_mut()
            {
                food.add_exhaustion(0.005);
            }
            if !double_plant {
                drop_resources(ctx, state, pos, &tool, entity.as_ref(), kept.as_ref());
            }
            // `TurtleEggBlock.playerDestroy`: one egg less (`decreaseEggs`).
            if state.block() == vanillite_data::block::ids::TURTLE_EGG
                && let Some(level) = ctx.level.level_mut()
            {
                crate::block::eggs::decrease_eggs(level, pos, state);
            }
            if let Some(level) = ctx.level.level_mut() {
                crate::block::break_hooks::ice_player_destroy(level, state, pos, &tool);
            }
            if crate::block::beehive::is_beehive(state.block())
                && let Some(player) = ctx.actor.entity
                && let Some(level) = ctx.level.level_mut()
            {
                crate::block::beehive::player_destroy(
                    level,
                    pos,
                    state,
                    entity.as_ref(),
                    player,
                    &tool,
                );
            }
        }
        true
    }

    /// `ItemStack.mineBlock`: tools count as used and lose durability.
    fn mine_block(&mut self, ctx: &mut PlayContext<'_>, state: BlockState) {
        let tool = ctx.inventory.selected_item();
        if !ctx.data.tools.is_tool(tool) {
            return;
        }
        let used = tool.item;
        ctx.award_stat(Stat::of(StatType::Used, used), 1);
        let damage = {
            let tool = ctx.inventory.selected_item();
            // `ShearsItem.mineBlock` skips only `#fire`, `Item.mineBlock` instant blocks.
            let shears = item::item_id(tool) == vanillite_data::item::ids::SHEARS;
            if (shears && crate::block::is_fire(state))
                || (!shears && state.misc().destroy_speed == 0.0)
            {
                return;
            }
            ctx.data.tools.damage_per_block(tool)
        };
        let infinite = ctx.actor.creative();
        let amount = crate::durability::change(
            ctx.inventory.selected_item(),
            i32::try_from(damage).unwrap_or(i32::MAX),
            infinite,
            &mut LevelRandom(&mut *ctx.level),
        );
        let Some(tool) = ctx.inventory.selected_item_mut() else {
            return;
        };
        if amount == 0 {
            return;
        }
        let new_damage = item::damage(tool).saturating_add(amount);
        let broke = crate::advancements::view::apply_damage(
            ctx.advancements.as_deref_mut(),
            ctx.stats.as_deref_mut(),
            tool,
            new_damage,
        );
        if broke {
            equipment_broken(ctx, Hand::MainHand);
        }
    }

    /// `handleUseItemOn`.
    pub fn use_item_on(
        &mut self,
        ctx: &mut PlayContext<'_>,
        p: &vanillite_protocol::packets::play::serverbound::UseItemOn,
        sink: &dyn PacketSink,
    ) {
        self.ack(p.sequence.max(0));
        self.entity_item_use = false;
        self.bind_fence = None;
        self.start_using = None;
        let pos = BlockPos::new(p.pos.x, p.pos.y, p.pos.z);
        let face = direction_wrapped(p.face);
        if !ctx.actor.within_block_range(pos, 1.0) {
            return;
        }
        let hit = Vec3::new(
            f64::from(pos.x) + f64::from(p.cursor_x),
            f64::from(pos.y) + f64::from(p.cursor_y),
            f64::from(pos.z) + f64::from(p.cursor_z),
        );
        let limit = 1.000_000_1;
        let center = Vec3::new(
            f64::from(pos.x) + 0.5,
            f64::from(pos.y) + 0.5,
            f64::from(pos.z) + 0.5,
        );
        if !((hit.x - center.x).abs() < limit
            && (hit.y - center.y).abs() < limit
            && (hit.z - center.z).abs() < limit)
        {
            log::warn!("Rejecting UseItemOnPacket: location too far away from hit block {pos:?}");
            return;
        }
        let (min_y, max_y) = (ctx.level.min_y(), ctx.level.max_y());
        if pos.y > max_y {
            build_limit_message(sink, true, max_y);
        } else if pos.y < min_y {
            build_limit_message(sink, false, min_y);
        } else {
            if (ctx.spawn_protected)(pos) {
                spawn_protection_message(sink, pos);
            } else if !ctx.level.is_loaded(pos) {
                // Not loaded yet: nothing happens.
            } else if !ctx.level.within_border(pos) {
                // `mayInteract` refuses: Vanilla answers with the build limit message.
                build_limit_message(sink, true, max_y);
            } else if !ctx.hooks.player_interact(
                ctx.level.hook_world(),
                ctx.actor.uuid,
                vanillite_abi::interact::USE_ON_BLOCK,
                hand_index(p.hand),
                held_name(ctx, p.hand),
                Some((
                    pos,
                    Direction::ALL
                        .iter()
                        .position(|&d| d == face)
                        .and_then(|i| u32::try_from(i).ok())
                        .unwrap_or(0),
                    ctx.level.block(pos),
                )),
            ) {
                // A plugin cancelled the use: the block updates below correct the client.
            } else if self.use_item_on_block(ctx, p.hand, pos, face, hit, p.inside, sink) {
                // Vanilla 26.3 swings twice for a successful use (two branches of
                // `handleUseItemOn` both call `swingAndResetAttackStrength`).
                ctx.swings.push((p.hand, false));
                ctx.swings.push((p.hand, false));
            }
            block_update(sink, ctx.level, pos);
            block_update(sink, ctx.level, relative(pos, face));
        }
    }

    /// `ServerPlayerGameMode.useItemOn`; returns whether the hand swings for others.
    #[allow(clippy::too_many_arguments)]
    fn use_item_on_block(
        &mut self,
        ctx: &mut PlayContext<'_>,
        hand: Hand,
        pos: BlockPos,
        face: Direction,
        hit: Vec3,
        inside: bool,
        sink: &dyn PacketSink,
    ) -> bool {
        let state = ctx.level.block(pos);
        if ctx.actor.game_mode == GameMode::Spectator {
            // Spectators open the menus of blocks that have one, nothing else.
            if let Some(level) = ctx.level.level_mut()
                && let Some(request) = crate::block::container::menu_provider(level, state, pos)
            {
                open_menu(ctx, request, sink);
            }
            return false;
        }
        let has_items =
            !ctx.inventory.selected_item().is_empty() || !ctx.inventory.offhand.is_empty();
        let suppress_use = ctx.actor.sneaking && has_items;
        if !suppress_use {
            // The game master blocks: `useItemOn` tries with an empty hand, `useWithoutItem`
            // opens the edit screen for operators in creative and passes for everyone else.
            if is_game_master_block(state) {
                let editor = crate::block::technical::Editor {
                    player: ctx.actor.entity,
                    uuid: ctx.actor.uuid,
                    allowed: crate::block::technical::can_use_game_master_blocks(
                        ctx.actor.creative(),
                        ctx.actor.permission_level,
                    ),
                    permission_level: ctx.actor.permission_level,
                    sink,
                };
                if hand == Hand::MainHand
                    && let Some(level) = ctx.level.level_mut()
                    && crate::block::technical::use_without_item(level, pos, &editor)
                {
                    ctx.trigger(Event::DefaultBlockUse(pos));
                    any_block_use(ctx, hand, pos);
                    return true;
                }
            } else if hand == Hand::MainHand && crate::block::shelf::is_shelf(state.block()) {
                // `ShelfBlock.useItemOn`: swaps with the hotbar, so it gets the inventory.
                let before = ctx.inventory.selected_item().clone();
                let selected = ctx.inventory.selected;
                let infinite = ctx.actor.creative();
                let result = match ctx.level.level_mut() {
                    Some(level) => crate::block::shelf::use_item_on(
                        level,
                        state,
                        pos,
                        face,
                        hit,
                        infinite,
                        &mut ctx.inventory.items[..9],
                        selected,
                    ),
                    None => None,
                };
                if let Some(result) = result {
                    ctx.trigger(Event::ItemUsedOnBlock(pos, before));
                    any_block_use(ctx, hand, pos);
                    return result == crate::block::Interaction::Success;
                }
            } else if hand == Hand::MainHand
                && ctx.actor.leads_mobs
                && crate::mob::tame::is_knot_block(state)
            {
                // `FenceBlock.useWithoutItem` -> `LeadItem.bindPlayerMobs`: the world ties the
                // player's leashed mobs to the fence next.
                self.bind_fence = Some(pos);
                ctx.trigger(Event::DefaultBlockUse(pos));
                any_block_use(ctx, hand, pos);
                return true;
            } else {
                // `state.useItemOn(stack, …)`, then `useWithoutItem` for the main hand when it
                // tries with an empty hand.
                let (has_use, has_item_use) = ctx.level.level_mut().map_or((false, false), |l| {
                    let b = l.behaviours().get(state.block());
                    (b.use_without_item.is_some(), b.use_item_on.is_some())
                });
                if (has_use || has_item_use)
                    && let Some(player) = ctx.actor.entity
                {
                    let use_ctx = crate::block::UseContext {
                        player,
                        may_build: ctx.actor.game_mode != GameMode::Adventure,
                        yaw: ctx.actor.y_rot,
                        face,
                        hit,
                        infinite: ctx.actor.creative(),
                        // `canEat(false)`: creative players are invulnerable.
                        can_eat: ctx.actor.creative()
                            || ctx.food.as_deref().is_some_and(|f| f.needs_food()),
                        game_master: ctx.actor.creative() && ctx.actor.permission_level >= 2,
                        main_hand: hand == Hand::MainHand,
                        off_hand_item: ctx.inventory.offhand.item,
                        respawn_here: ctx.actor.respawn_here,
                    };
                    let mut held = match hand {
                        Hand::MainHand => ctx.inventory.selected_item().clone(),
                        Hand::OffHand => ctx.inventory.offhand.clone(),
                    };
                    let before = held.clone();
                    let item_use = match ctx.level.level_mut() {
                        Some(level) if has_item_use => level.use_item_on(pos, &use_ctx, &mut held),
                        _ => crate::block::ItemInteraction::TryWithEmptyHand,
                    };
                    let (result, item_used) = match item_use {
                        crate::block::ItemInteraction::Done(result) => {
                            if held != before && !ctx.actor.creative() {
                                let slot = match hand {
                                    Hand::MainHand => ctx.inventory.selected_item_mut(),
                                    Hand::OffHand => Some(&mut ctx.inventory.offhand),
                                };
                                if let Some(slot) = slot {
                                    *slot = held;
                                }
                            }
                            (Some(result), true)
                        }
                        crate::block::ItemInteraction::TryWithEmptyHand
                            if hand == Hand::MainHand && has_use =>
                        {
                            let used = ctx
                                .level
                                .level_mut()
                                .map(|level| level.use_without_item(pos, &use_ctx));
                            (used, false)
                        }
                        _ => (None, false),
                    };
                    apply_player_effects(ctx, hand);
                    self.open_requested(ctx, sink);
                    if let Some(
                        result @ (crate::block::Interaction::Success
                        | crate::block::Interaction::Consume),
                    ) = result
                    {
                        ctx.trigger(if item_used {
                            Event::ItemUsedOnBlock(pos, before)
                        } else {
                            Event::DefaultBlockUse(pos)
                        });
                        any_block_use(ctx, hand, pos);
                        return result == crate::block::Interaction::Success;
                    }
                } else if has_use_action(state) {
                    // Blocks whose use is not implemented yet consume the click.
                    return false;
                }
            }
        }
        let stack = match hand {
            Hand::MainHand => ctx.inventory.selected_item().clone(),
            Hand::OffHand => ctx.inventory.offhand.clone(),
        };
        // `VaultBlock.useItemOn`: an active vault takes the key, the use is consumed.
        if !suppress_use
            && state.block() == vanillite_data::block::ids::VAULT
            && let Some(level) = ctx.level.level_mut()
            && let Some(taken) =
                crate::block::vault::use_item_on(level, pos, ctx.actor.uuid, &stack)
        {
            if taken {
                // `VaultBlockEntity.tryInsertKey`: the key counts as used.
                ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
            }
            let Some(level) = ctx.level.level_mut() else {
                return true;
            };
            if taken && !ctx.actor.creative() {
                let count = crate::block::vault::key_count(level, pos);
                let slot = match hand {
                    Hand::MainHand => ctx.inventory.selected_item_mut(),
                    Hand::OffHand => Some(&mut ctx.inventory.offhand),
                };
                if let Some(slot) = slot {
                    slot.shrink(count);
                }
            }
            ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
            any_block_use(ctx, hand, pos);
            return true;
        }
        if stack.is_empty() {
            return false;
        }
        let used = item::item_id(&stack);
        // `ItemStack.useOn`: adventure players use items only on blocks `can_place_on`
        // allows (map on a banner, compass on a lodestone, decorations).
        let may_build =
            ctx.actor.game_mode != GameMode::Adventure || item_use::can_place_on(ctx, &stack, pos);
        if crate::map::is_map_item(used.0)
            && may_build
            && let Some(level) = ctx.level.level_mut()
            && let Some(ok) = crate::map::use_on_banner(level, &stack, pos)
        {
            // `MapItem.useOn` on a banner.
            if ok {
                ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
                ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
                any_block_use(ctx, hand, pos);
            }
            return ok;
        }
        if used == vanillite_data::item::ids::COMPASS
            && may_build
            && ctx.level.block(pos).block() == vanillite_data::block::ids::LODESTONE
        {
            // `CompassItem.useOn`.
            ctx.level.play_sound(
                None,
                Vec3::new(
                    f64::from(pos.x) + 0.5,
                    f64::from(pos.y) + 0.5,
                    f64::from(pos.z) + 0.5,
                ),
                u16::try_from(
                    crate::survival::sound_event("minecraft:item.lodestone_compass.lock").0,
                )
                .unwrap_or(0),
                crate::player::world::SoundSource::Players,
                1.0,
                1.0,
            );
            let dimension = ctx.level.dimension().to_owned();
            let creative = ctx.actor.creative();
            let slot = match hand {
                Hand::MainHand => ctx.inventory.selected_item_mut(),
                Hand::OffHand => Some(&mut ctx.inventory.offhand),
            };
            let extra =
                slot.and_then(|s| crate::carried::bind_lodestone(s, &dimension, pos, creative));
            if let Some(mut compass) = extra
                && !ctx.inventory.add(None, &mut compass, creative)
            {
                ctx.drops.push(Drop {
                    stack: compass,
                    thrown: false,
                });
            }
            ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
            ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
            any_block_use(ctx, hand, pos);
            return true;
        }
        if used == vanillite_data::item::ids::FLINT_AND_STEEL
            || used == vanillite_data::item::ids::ENDER_EYE
        {
            // `ItemStack.useOn`: adventure players need `can_place_on`.
            if ctx.actor.game_mode == GameMode::Adventure
                && !item_use::can_place_on(ctx, &stack, pos)
            {
                return false;
            }
            let done = if used == vanillite_data::item::ids::FLINT_AND_STEEL {
                self.use_flint_and_steel(ctx, hand, pos, face)
            } else {
                use_ender_eye(ctx, hand, pos)
            };
            if done {
                // `ItemStack.useOn` of a successful item interaction, then
                // `ITEM_USED_ON_BLOCK` and `ANY_BLOCK_USE`.
                ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
                ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
                any_block_use(ctx, hand, pos);
            }
            return done;
        }
        if used == vanillite_data::item::ids::END_CRYSTAL {
            if !ctx.level.use_end_crystal(pos, ctx.actor.entity) {
                return false;
            }
            if !ctx.actor.creative() {
                let slot = match hand {
                    Hand::MainHand => ctx.inventory.selected_item_mut(),
                    Hand::OffHand => Some(&mut ctx.inventory.offhand),
                };
                if let Some(slot) = slot {
                    slot.shrink(1);
                }
            }
            ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
            ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
            any_block_use(ctx, hand, pos);
            return true;
        }
        // `BrushItem.useOn`: the click's ray hit this block, brushing starts (`CONSUME`: no
        // swing, but the statistic and the triggers of a used item).
        if used == vanillite_data::item::ids::BRUSH {
            if ctx.actor.game_mode == GameMode::Adventure
                && !item_use::can_place_on(ctx, &stack, pos)
            {
                return false;
            }
            self.start_using = Some(hand);
            ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
            ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
            any_block_use(ctx, hand, pos);
            return false;
        }
        let click = item_use::Click { hand, pos, face };
        if let Some(result) = item_use::use_on(ctx, click, sink) {
            if result == item_use::Used::Success {
                ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
                any_block_use(ctx, hand, pos);
                return true;
            }
            return false;
        }
        if let Some(done) = self.spawn_egg_on_spawner(ctx, hand, pos, &stack, sink) {
            return done;
        }
        if let Some(placed) = self.place_decoration(ctx, hand, &stack, pos, face, hit) {
            return placed;
        }
        let used = item::item_id(&stack);
        let Some(_block) = used.block() else {
            // Spawn eggs, leads and minecarts act on entities: the world runs them next.
            self.entity_item_use = ctx.actor.game_mode != GameMode::Adventure
                || item_use::can_place_on(ctx, &stack, pos);
            return false;
        };
        // `PlaceOnWaterBlockItem.useOn` passes: lily pads and frogspawn go on water by `use`.
        if used == vanillite_data::item::ids::LILY_PAD
            || used == vanillite_data::item::ids::FROGSPAWN
        {
            return false;
        }
        if ctx.actor.game_mode == GameMode::Adventure && !item_use::can_place_on(ctx, &stack, pos) {
            return false;
        }
        let place_ctx = PlaceContext::new(
            pos,
            face,
            hit,
            inside,
            ctx.actor.y_rot,
            ctx.actor.x_rot,
            ctx.actor.sneaking,
            item::item_id(&stack),
            &LevelReader(&*ctx.level),
        );
        let Some(place_ctx) =
            place::update_placement_context(&place_ctx, &LevelReader(&*ctx.level))
        else {
            self.consumable_fallback(ctx, hand, &stack, pos);
            return false;
        };
        let placed = self.place_block(ctx, hand, &stack, &place_ctx);
        self.open_requested(ctx, sink);
        if placed {
            // `ItemStack.useOn` of a successful item interaction.
            ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
            ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
            any_block_use(ctx, hand, pos);
        } else {
            self.consumable_fallback(ctx, hand, &stack, pos);
        }
        placed
    }

    /// The end of `BlockItem.useOn`: a block item with `consumable` that was not placed is used
    /// like in the air (`Item.use` -> `Consumable.startConsuming`). It consumes the click
    /// without a swing when the player can eat it (`CONSUME`: the stat and the triggers of a
    /// successful item use), and fails otherwise.
    fn consumable_fallback(
        &mut self,
        ctx: &mut PlayContext<'_>,
        hand: Hand,
        stack: &ItemStack,
        pos: BlockPos,
    ) {
        if !crate::consumable::is_consumable(stack) {
            return;
        }
        let can = crate::consumable::food(stack).is_none_or(|food| {
            food.can_always_eat
                || matches!(
                    ctx.actor.game_mode,
                    GameMode::Creative | GameMode::Spectator
                )
                || ctx.food.as_deref().is_some_and(|f| f.needs_food())
        });
        if !can {
            return;
        }
        self.start_using = Some(hand);
        ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
        ctx.trigger(Event::ItemUsedOnBlock(pos, stack.clone()));
        any_block_use(ctx, hand, pos);
    }

    /// `useOn` of the items that place decorations: armor stands, item frames, paintings and
    /// cushions. `None` for other items; else whether the hand swings.
    fn place_decoration(
        &mut self,
        ctx: &mut PlayContext<'_>,
        hand: Hand,
        stack: &ItemStack,
        pos: BlockPos,
        face: Direction,
        hit: Vec3,
    ) -> Option<bool> {
        use crate::decoration::hanging::Placed;
        let used = item::item_id(stack);
        let kind = if used == vanillite_data::item::ids::ARMOR_STAND {
            vanillite_data::entity::ids::ARMOR_STAND
        } else if used == vanillite_data::item::ids::ITEM_FRAME {
            vanillite_data::entity::ids::ITEM_FRAME
        } else if used == vanillite_data::item::ids::GLOW_ITEM_FRAME {
            vanillite_data::entity::ids::GLOW_ITEM_FRAME
        } else if used == vanillite_data::item::ids::PAINTING {
            vanillite_data::entity::ids::PAINTING
        } else if crate::decoration::cushion::color_of_item(stack).is_some() {
            vanillite_data::entity::ids::CUSHION
        } else {
            return None;
        };
        // `ItemStack.useOn`: adventure players only where `can_place_on` allows.
        if ctx.actor.game_mode == GameMode::Adventure && !item_use::can_place_on(ctx, stack, pos) {
            return Some(false);
        }
        let creative = ctx.actor.creative();
        let player = ctx.actor.entity;
        let y_rot = ctx.actor.y_rot;
        let spectators = ctx.level.spectators();
        let level = ctx.level.level_mut()?;
        let mut held = stack.clone();
        let result = if kind == vanillite_data::entity::ids::ARMOR_STAND {
            if crate::decoration::stand::use_on(
                level,
                pos,
                face,
                y_rot,
                &mut held,
                player,
                creative,
                &spectators,
            ) {
                Placed::Success
            } else {
                Placed::Fail
            }
        } else if kind == vanillite_data::entity::ids::CUSHION {
            crate::decoration::cushion::use_on(
                level, pos, face, hit.y, y_rot, &mut held, player, creative,
            )
        } else {
            crate::decoration::hanging::use_on(
                level, kind, pos, face, &mut held, player, true, creative,
            )
        };
        if result == Placed::Success {
            let slot = match hand {
                Hand::MainHand => ctx.inventory.selected_item_mut(),
                Hand::OffHand => Some(&mut ctx.inventory.offhand),
            };
            if let Some(slot) = slot {
                *slot = held;
            }
            ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
            ctx.trigger(Event::ItemUsedOnBlock(pos, stack.clone()));
        }
        Some(result == Placed::Success)
    }

    /// Opens the menus and sign editors blocks asked for during a use.
    fn open_requested(&mut self, ctx: &mut PlayContext<'_>, sink: &dyn PacketSink) {
        let Some(level) = ctx.level.level_mut() else {
            return;
        };
        let requests = level.take_menu_requests();
        let editors = level.take_sign_editors();
        let gives = level.take_player_gives();
        for request in requests {
            open_menu(ctx, request, sink);
        }
        for (player, mut stack) in gives {
            if Some(player) == ctx.actor.entity {
                let infinite = ctx.actor.creative();
                if !ctx.inventory.add(None, &mut stack, infinite) {
                    ctx.drops.push(Drop {
                        stack,
                        thrown: false,
                    });
                }
            }
        }
        for (player, pos, front) in editors {
            if Some(player) == ctx.actor.entity {
                sink.send(
                    &vanillite_protocol::packets::play::clientbound::OpenSignEditor {
                        pos: wire(pos),
                        front,
                    },
                );
            }
        }
    }

    /// `FlintAndSteelItem.useOn`: lights a campfire, candle or candle cake, or places fire
    /// against the clicked face (lighting a nether portal frame).
    fn use_flint_and_steel(
        &mut self,
        ctx: &mut PlayContext<'_>,
        hand: Hand,
        pos: BlockPos,
        face: Direction,
    ) -> bool {
        let state = ctx.level.block(pos);
        let lightable = can_light(state);
        let target = if lightable { pos } else { relative(pos, face) };
        if !lightable {
            // `BaseFireBlock.canBePlacedAt`.
            let reader = LevelReader(&*ctx.level);
            let fire = place::fire_state(&reader, target);
            let forward = place::horizontal_direction_of(ctx.actor.y_rot);
            let placeable = ctx.level.block(target).is_air()
                && (place::can_survive(fire, target, &reader)
                    || ctx.level.lights_portal(target, forward));
            if !placeable {
                return false;
            }
        }
        // Bukkit's `BlockIgniteEvent` (`FLINT_AND_STEEL`): a cancelled use does nothing.
        if ctx.hooks.wants(vanillite_abi::event::BLOCK_IGNITE) {
            let mut ev = crate::hooks::Event {
                pos: target,
                cause: vanillite_abi::ignite_cause::FLINT_AND_STEEL,
                entity: ctx.actor.uuid,
                player: ctx.actor.uuid,
                state: Some(ctx.level.block(target)),
                ..crate::hooks::Event::new(vanillite_abi::event::BLOCK_IGNITE)
            };
            ctx.hooks.event(ctx.level.hook_world(), &mut ev);
            if ev.cancelled {
                return false;
            }
        }
        let pitch = ctx.level.random_float() * 0.4 + 0.8;
        let sound = vanillite_data::registry::SOUND_EVENT
            .id("minecraft:item.flintandsteel.use")
            .unwrap_or(0);
        ctx.level.play_sound(
            Some(ctx.actor.network_id),
            Vec3::new(
                f64::from(target.x) + 0.5,
                f64::from(target.y) + 0.5,
                f64::from(target.z) + 0.5,
            ),
            sound,
            SoundSource::Blocks,
            1.0,
            pitch,
        );
        let new_state = if lightable {
            state.with("lit", "true").unwrap_or(state)
        } else {
            place::fire_state(&LevelReader(&*ctx.level), target)
        };
        ctx.level
            .set_block(target, new_state, update::ALL_IMMEDIATE);
        // `BLOCK_PLACE` at the clicked block for fire, `BLOCK_CHANGE` for a lit block.
        let event = if lightable {
            events_ids().block_change
        } else {
            events_ids().block_place
        };
        ctx.level.game_event(event, pos, ctx.actor.entity, None);
        if !lightable {
            // `PLACED_BLOCK` of the placed fire, before the damage.
            let stack = match hand {
                Hand::MainHand => ctx.inventory.selected_item().clone(),
                Hand::OffHand => ctx.inventory.offhand.clone(),
            };
            ctx.trigger(Event::PlacedBlock(target, stack));
        }
        hurt_and_break(ctx, hand, 1);
        true
    }

    /// `BlockItem.place`.
    fn place_block(
        &mut self,
        ctx: &mut PlayContext<'_>,
        hand: Hand,
        stack: &ItemStack,
        place_ctx: &PlaceContext,
    ) -> bool {
        let target = place_ctx.pos;
        if target.y < ctx.level.min_y() || target.y > ctx.level.max_y() {
            return false;
        }
        if !ctx.level.is_loaded(target) {
            return false;
        }
        let existing = ctx.level.block(target);
        if !place_ctx.replace_clicked && !place::can_be_replaced(existing, place_ctx) {
            return false;
        }
        // `GameMasterBlockItem.getPlacementState`: no state without game master rights.
        if vanillite_data::item::ItemId(stack.item)
            .block()
            .is_some_and(|b| is_game_master_block(b.default_state()))
            && !crate::block::technical::can_use_game_master_blocks(
                ctx.actor.creative(),
                ctx.actor.permission_level,
            )
        {
            return false;
        }
        let Some(mut state) = place::state_for_placement(place_ctx, &LevelReader(&*ctx.level))
        else {
            return false;
        };
        // `GrowingPlantHeadBlock.getStateForPlacement`: a new head gets a random age.
        {
            use vanillite_data::block::ids;
            use vanillite_worldgen::behaviour::StateExt;
            if [
                ids::KELP,
                ids::WEEPING_VINES,
                ids::TWISTING_VINES,
                ids::CAVE_VINES,
            ]
            .contains(&state.block())
            {
                let age = ctx.level.random_int(25);
                state = state.set_int("age", age);
            }
        }
        // `CoralBlock.getStateForPlacement`: without water next to it the die tick is scheduled
        // (`scanForWater`).
        if vanillite_worldgen::behaviour::class_of_block(state.block())
            == vanillite_worldgen::behaviour::Class::Coral
            && !Direction::ALL
                .iter()
                .any(|&d| crate::mob::is_water(ctx.level.block(target.relative(d))))
        {
            let delay = ctx.level.random_int(40).saturating_add(60);
            if let Some(level) = ctx.level.level_mut() {
                level.schedule_block_tick(target, state.block(), delay);
            }
        }
        // `GameMasterBlockItem.getPlacementState`: only creative operators.
        if is_game_master_block(state) && !(ctx.actor.creative() && ctx.actor.permission_level >= 2)
        {
            return false;
        }
        if let Some(level) = ctx.level.level_mut() {
            state = crate::block::creaking_heart::placement_state(level, state);
            // `canSurvive` with the light the placement rules take as met: crops and pitcher
            // pods need light 8, mushrooms less than 13 (`getRawBrightness(pos, 0)`).
            use vanillite_worldgen::behaviour::{Class, class_of_block};
            if matches!(
                class_of_block(state.block()),
                Class::Crop | Class::Mushroom | Class::PitcherCrop
            ) && !level.can_survive(state, target)
            {
                return false;
            }
        }
        if !ctx.level.is_unobstructed(state, target) {
            return false;
        }
        if !ctx
            .hooks
            .block_place(ctx.level.hook_world(), ctx.actor.uuid, target, state)
        {
            return false;
        }
        if !ctx.level.set_block(target, state, update::ALL_IMMEDIATE) {
            return false;
        }
        // `PLACED_BLOCK` only while the placed block is still there (`placedState.is(block)`:
        // not for concrete powder that hardened or TNT that primed at once).
        if ctx.level.block(target).block() == state.block() {
            ctx.trigger(Event::PlacedBlock(target, stack.clone()));
        }
        if ctx.level.block(target).block() == state.block() {
            if let Some(updated) = apply_block_state_component(stack, state) {
                ctx.level.set_block(target, updated, update::CLIENTS);
                state = updated;
            }
            let mut extra = Vec::new();
            place::placed_extra(
                state,
                target,
                place_ctx,
                &LevelReader(&*ctx.level),
                &mut extra,
            );
            for (p, s) in extra {
                ctx.level.set_block(p, s, update::ALL);
            }
            if state.block().info().kind == BlockKind::MossyCarpetBlock {
                // The topper draws one boolean per side that can hang down; which sides can is
                // fixed by the level, so count them first, then draw from the level random.
                let mut sides = 0u8;
                place::mossy_carpet_topper(state, target, &LevelReader(&*ctx.level), &mut || {
                    sides = sides.saturating_add(1);
                    false
                });
                let draws: Vec<bool> = (0..sides).map(|_| ctx.level.random_boolean()).collect();
                let mut draws = draws.into_iter();
                if let Some(topper) = place::mossy_carpet_topper(
                    state,
                    target,
                    &LevelReader(&*ctx.level),
                    &mut || draws.next().unwrap_or(false),
                ) {
                    ctx.level
                        .set_block(target.relative(Direction::Up), topper, update::ALL);
                }
            }
        }
        let placed = ctx.level.block(target);
        if let Some(level) = ctx.level.level_mut()
            && placed.block() == state.block()
        {
            // `BlockItem.updateCustomBlockEntityTag`, `updateBlockEntityComponents` and
            // `Block.setPlacedBy`.
            crate::level::block_entity::apply_block_entity_data(
                level,
                target,
                stack,
                crate::block::technical::can_use_game_master_blocks(
                    ctx.actor.creative(),
                    ctx.actor.permission_level,
                ),
            );
            crate::level::block_entity::apply_components(level, target, stack);
            crate::block::redstone::set_placed_by(level, placed, target);
            crate::block::crafter::set_placed_by(level, placed, target);
            crate::block::technical::command_block::set_placed_by(
                level,
                placed,
                target,
                stack.component(ids::BLOCK_ENTITY_DATA).is_some(),
            );
            if placed.block() == vanillite_data::block::ids::STRUCTURE_BLOCK {
                crate::block::technical::structure_block::set_placed_by(
                    level,
                    target,
                    ctx.actor.entity,
                );
            }
            if placed.block() == vanillite_data::block::ids::TRIPWIRE_HOOK {
                crate::block::tripwire::hook_set_placed_by(level, target, placed);
            }
            // `JukeboxBlock.setPlacedBy`: a record in the item's block entity data.
            if placed.block() == vanillite_data::block::ids::JUKEBOX
                && crate::level::block_entity::block_entity_data_contains(stack, "RecordItem")
                && let Some(with) = placed.with("has_record", "true")
            {
                level.set_block(target, with, crate::level::update::CLIENTS);
            }
            // `DriedGhastBlock.setPlacedBy`: its own sound to everyone.
            if placed.block() == vanillite_data::block::ids::DRIED_GHAST {
                let sound = if placed.value("waterlogged") == Some("true") {
                    "block.dried_ghast.place_in_water"
                } else {
                    "block.dried_ghast.place"
                };
                let sound = crate::block::sound_id(sound);
                level.play_block_sound(None, target, sound, crate::level::SOUND_BLOCKS, 1.0, 1.0);
            }
            // `SignBlock.setPlacedBy`: the editor of the front opens unless the sign is waxed
            // or its front has text that is not plain.
            if crate::block::sign::is_sign(placed.block())
                && let Some(player) = ctx.actor.entity
                && let Some(crate::level::BlockEntity::Sign(sign)) = level.block_entity_mut(target)
                && !sign.waxed
                && sign.front.has_editable_text()
            {
                sign.editor = Some(ctx.actor.uuid);
                level.request_sign_editor(player, target, true);
            }
        }
        if placed.block() == state.block() {
            ctx.level.placed_by(target, placed);
        }
        let sound = placed.block().sound();
        // `SolidBucketItem`: its own place sound, the empty bucket stays in the hand.
        let solid_bucket = stack.item == vanillite_data::item::ids::POWDER_SNOW_BUCKET.0;
        ctx.level.play_sound(
            Some(ctx.actor.network_id),
            Vec3::new(
                f64::from(target.x) + 0.5,
                f64::from(target.y) + 0.5,
                f64::from(target.z) + 0.5,
            ),
            if solid_bucket {
                crate::block::sound_id("item.bucket.empty_powder_snow")
            } else {
                sound.place_sound
            },
            SoundSource::Blocks,
            (sound.volume + 1.0) / 2.0,
            sound.pitch * 0.8,
        );
        ctx.level.game_event(
            events_ids().block_place,
            target,
            ctx.actor.entity,
            Some(placed),
        );
        if !ctx.actor.creative() {
            let slot = match hand {
                Hand::MainHand => ctx.inventory.selected_item_mut(),
                Hand::OffHand => Some(&mut ctx.inventory.offhand),
            };
            if let Some(slot) = slot {
                slot.shrink(1);
                if solid_bucket {
                    *slot = ItemStack::new(vanillite_data::item::ids::BUCKET.0, 1);
                }
            }
        }
        true
    }

    /// `SpawnEggItem.useOn` on a spawner: the spawner spawns the egg's mob (`None` when the
    /// block is no spawner or the item no spawn egg).
    fn spawn_egg_on_spawner(
        &mut self,
        ctx: &mut PlayContext<'_>,
        hand: Hand,
        pos: BlockPos,
        stack: &ItemStack,
        sink: &dyn PacketSink,
    ) -> Option<bool> {
        let name = item::item_id(stack).name();
        let entity = name.strip_suffix("_spawn_egg")?;
        if ctx.actor.game_mode == GameMode::Adventure && !item_use::can_place_on(ctx, stack, pos) {
            return Some(false);
        }
        let player = ctx.actor.entity;
        let level = ctx.level.level_mut()?;
        // `instanceof Spawner`: mob spawners and trial spawners.
        if !matches!(
            level.block_entity(pos),
            Some(
                crate::level::BlockEntity::Spawner(_) | crate::level::BlockEntity::TrialSpawner(_)
            )
        ) {
            return None;
        }
        // `EntityType.canSpawn`: monsters not in Peaceful.
        if level.spawner_env.difficulty == crate::survival::rules::Difficulty::Peaceful
            && !crate::mob::spawn::allowed_in_peaceful(entity.trim_start_matches("minecraft:"))
        {
            return Some(false);
        }
        if !level.spawner_env.spawner_blocks_work {
            sink.send(
                &vanillite_protocol::packets::play::clientbound::SystemChat {
                    content: vanillite_protocol::text::Component::translatable_args(
                        "advMode.notEnabled.spawner",
                        Vec::new(),
                    ),
                    overlay: false,
                },
            );
            return Some(false);
        }
        let mut random = level.random().clone();
        if let Some(crate::level::BlockEntity::Spawner(spawner)) = level.block_entity_mut(pos) {
            spawner.set_entity_id(entity, &mut random);
        }
        *level.random() = random;
        if matches!(
            level.block_entity(pos),
            Some(crate::level::BlockEntity::TrialSpawner(_))
        ) {
            crate::block::trial_spawner::set_entity_id(level, pos, entity);
        }
        level.block_entity_changed(pos);
        level.game_event_at(player, crate::level::game_event::events().block_change, pos);
        if !ctx.actor.creative() {
            let mut held = item_use::held(ctx, hand);
            held.shrink(1);
            item_use::set_held(ctx, hand, held);
        }
        ctx.award_stat(Stat::of(StatType::Used, stack.item), 1);
        ctx.trigger(Event::ItemUsedOnBlock(pos, stack.clone()));
        any_block_use(ctx, hand, pos);
        Some(true)
    }

    /// `handleUseItem` (`ServerPlayerGameMode.useItem`) of the items whose use needs only the
    /// world: buckets, glass bottles, lily pads and frogspawn, swappable equipment. Returns
    /// whether the item was handled here (else the world's entity handlers use it).
    pub fn use_item(
        &mut self,
        ctx: &mut PlayContext<'_>,
        hand: Hand,
        sequence: i32,
        y_rot: f32,
        x_rot: f32,
    ) -> bool {
        self.ack(sequence.max(0));
        if ctx.actor.game_mode == GameMode::Spectator {
            return false;
        }
        if !ctx.hooks.player_interact(
            ctx.level.hook_world(),
            ctx.actor.uuid,
            vanillite_abi::interact::USE_ITEM,
            hand_index(hand),
            held_name(ctx, hand),
            None,
        ) {
            // A plugin cancelled the use: nothing uses the item.
            return true;
        }
        let y_rot = vanillite_base::math::wrap_degrees_f(y_rot);
        let x_rot = vanillite_base::math::wrap_degrees_f(x_rot);
        match item_use::use_item(ctx, hand, y_rot, x_rot) {
            None => false,
            Some(item_use::UseAir::Done(result)) => {
                if result == item_use::Used::Success {
                    ctx.swings.push((hand, false));
                }
                true
            }
            Some(item_use::UseAir::PlaceOnWater(pos, face, hit)) => {
                // `BlockItem.useOn` with the hit moved onto the block above the water.
                let stack = item_use::held(ctx, hand);
                let place_ctx = PlaceContext::new(
                    pos,
                    face,
                    hit,
                    false,
                    y_rot,
                    x_rot,
                    ctx.actor.sneaking,
                    item::item_id(&stack),
                    &LevelReader(&*ctx.level),
                );
                if let Some(place_ctx) =
                    place::update_placement_context(&place_ctx, &LevelReader(&*ctx.level))
                    && self.place_block(ctx, hand, &stack, &place_ctx)
                {
                    ctx.swings.push((hand, false));
                }
                true
            }
        }
    }

    /// `drop(boolean all)` (Q / Ctrl+Q).
    pub fn drop_selected(&mut self, ctx: &mut PlayContext<'_>, all: bool) {
        if ctx.actor.game_mode == GameMode::Spectator {
            return;
        }
        let stack = ctx.inventory.drop_selected(all);
        if !stack.is_empty() {
            ctx.drops.push(Drop {
                stack,
                thrown: true,
            });
        }
    }

    /// `handleSetCreativeModeSlot`.
    pub fn set_creative_slot(&mut self, ctx: &mut PlayContext<'_>, slot: i16, stack: ItemStack) {
        if !ctx.actor.creative() {
            return;
        }
        let valid_data = stack.is_empty() || stack.count <= item::max_stack_size(&stack);
        if (1..=45).contains(&slot) && valid_data {
            ctx.inventory
                .set_creative_slot(usize::try_from(slot).unwrap_or(0), stack);
            flush_equips(ctx);
        } else if slot < 0 && valid_data && !stack.is_empty() {
            // `dropSpamThrottler`: 20 per drop, threshold 1480.
            if self.drop_spam < 1480 {
                self.drop_spam = self.drop_spam.saturating_add(20);
                ctx.drops.push(Drop {
                    stack,
                    thrown: true,
                });
            } else {
                log::warn!("A player was dropping items too fast in creative mode, ignoring.");
            }
        }
    }

    /// The block change sequence to acknowledge this tick, reset afterwards.
    pub fn take_ack(&mut self) -> Option<i32> {
        let ack = self.ack_up_to;
        self.ack_up_to = -1;
        (ack > -1).then_some(ack)
    }
}

/// `ANY_BLOCK_USE` after a use that consumed the click, with the stack now in the hand.
fn any_block_use(ctx: &mut PlayContext<'_>, hand: Hand, pos: BlockPos) {
    let held = match hand {
        Hand::MainHand => ctx.inventory.selected_item().clone(),
        Hand::OffHand => ctx.inventory.offhand.clone(),
    };
    ctx.trigger(Event::AnyBlockUse(pos, held));
}

/// Applies what blocks did to the player during a use ([`crate::block::PlayerEffect`]).
fn apply_player_effects(ctx: &mut PlayContext<'_>, hand: Hand) {
    use crate::block::PlayerEffect;
    let Some(level) = ctx.level.level_mut() else {
        return;
    };
    let effects = level.take_player_effects();
    for (player, effect) in effects {
        if Some(player) != ctx.actor.entity {
            continue;
        }
        match effect {
            PlayerEffect::Eat(nutrition, saturation) => {
                if let Some(food) = ctx.food.as_deref_mut() {
                    food.eat(&crate::survival::food::FoodProperties {
                        nutrition,
                        saturation,
                        can_always_eat: false,
                    });
                }
            }
            PlayerEffect::GiveIfMissing(mut stack) => {
                if !ctx.inventory.contains(&stack) {
                    ctx.inventory.add(None, &mut stack, true);
                }
            }
            PlayerEffect::SetHand(stack) => {
                let slot = match hand {
                    Hand::MainHand => ctx.inventory.selected_item_mut(),
                    Hand::OffHand => Some(&mut ctx.inventory.offhand),
                };
                if let Some(slot) = slot {
                    *slot = stack;
                }
            }
        }
    }
}

/// The statistic a block awards when it opens its menu (`getOpenChestStat`,
/// `INTERACT_WITH_*`, `INSPECT_*`).
fn open_stat(block: vanillite_data::block::BlockId) -> Option<Stat> {
    let name = block.name().strip_prefix("minecraft:").unwrap_or("");
    Some(match name {
        "trapped_chest" => custom::TRIGGER_TRAPPED_CHEST,
        "ender_chest" => custom::OPEN_ENDERCHEST,
        "barrel" => custom::OPEN_BARREL,
        "dispenser" => custom::INSPECT_DISPENSER,
        "dropper" => custom::INSPECT_DROPPER,
        "hopper" => custom::INSPECT_HOPPER,
        "crafting_table" => custom::INTERACT_WITH_CRAFTING_TABLE,
        "furnace" => custom::INTERACT_WITH_FURNACE,
        "blast_furnace" => custom::INTERACT_WITH_BLAST_FURNACE,
        "smoker" => custom::INTERACT_WITH_SMOKER,
        "stonecutter" => custom::INTERACT_WITH_STONECUTTER,
        "anvil" | "chipped_anvil" | "damaged_anvil" => custom::INTERACT_WITH_ANVIL,
        "grindstone" => custom::INTERACT_WITH_GRINDSTONE,
        "smithing_table" => custom::INTERACT_WITH_SMITHING_TABLE,
        "brewing_stand" => custom::INTERACT_WITH_BREWINGSTAND,
        "cartography_table" => custom::INTERACT_WITH_CARTOGRAPHY_TABLE,
        "loom" => custom::INTERACT_WITH_LOOM,
        "beacon" => custom::INTERACT_WITH_BEACON,
        "lectern" => custom::INTERACT_WITH_LECTERN,
        _ if name.ends_with("shulker_box") => custom::OPEN_SHULKER_BOX,
        _ if name == "chest" || name.ends_with("copper_chest") => custom::OPEN_CHEST,
        _ => return None,
    })
}

/// `ServerPlayer.openMenu` for a menu a block asked for.
fn open_menu(
    ctx: &mut PlayContext<'_>,
    request: crate::menu::container::MenuRequest,
    sink: &dyn PacketSink,
) {
    let Some(level) = ctx.level.level_mut() else {
        return;
    };
    let who = crate::menu::container::Opener {
        spectator: ctx.actor.game_mode == GameMode::Spectator,
        infinite: ctx.actor.creative(),
        disconnected: false,
    };
    // Spectators open menus without using the block, so without its statistic.
    let stat = open_stat(request.block).filter(|_| !who.spectator);
    crate::menu::container::open(ctx.inventory, level, request, who, ctx.drops, sink);
    if let Some(stat) = stat {
        ctx.award_stat(stat, 1);
    }
}

/// `CampfireBlock.canLight`, `CandleBlock.canLight` and `CandleCakeBlock.canLight`: an unlit
/// (and, for campfires and candles, dry) one.
fn can_light(state: BlockState) -> bool {
    let blocks = &vanillite_worldgen::behaviour::tags().blocks;
    let unlit = state.value("lit") == Some("false");
    let dry = state.value("waterlogged") == Some("false");
    if blocks.campfires.contains(state) || blocks.candles.contains(state) {
        unlit && dry
    } else {
        blocks.candle_cakes.contains(state) && unlit
    }
}

/// `EnderEyeItem.useOn`: an eye into an empty End portal frame.
fn use_ender_eye(ctx: &mut PlayContext<'_>, hand: Hand, pos: BlockPos) -> bool {
    let state = ctx.level.block(pos);
    if state.block() != vanillite_data::block::ids::END_PORTAL_FRAME
        || state.value("eye") != Some("false")
    {
        return false;
    }
    let filled = state.with("eye", "true").unwrap_or(state);
    // `Block.pushEntitiesUp`: entities on the frame move onto the eye.
    if let Some(level) = ctx.level.level_mut() {
        let (x, y, z) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
        crate::block::contact::push_entities_up_from(
            level,
            vanillite_base::Aabb::new(x + 0.25, y + 0.8125, z + 0.25, x + 0.75, y + 1.0, z + 0.75),
        );
    }
    ctx.level.set_block(pos, filled, update::CLIENTS);
    if !ctx.actor.creative() {
        let slot = match hand {
            Hand::MainHand => ctx.inventory.selected_item_mut(),
            Hand::OffHand => Some(&mut ctx.inventory.offhand),
        };
        if let Some(slot) = slot {
            slot.shrink(1);
        }
    }
    // The comparators, the eye's particles and sound (1503), then the portal.
    ctx.level.end_portal_frame_filled(pos);
    true
}

/// `ItemStack.hurtAndBreak(amount, player, hand)`: the item in `hand` loses durability (the
/// unbreaking enchantment may save each point); creative players' items do not.
fn hurt_and_break(ctx: &mut PlayContext<'_>, hand: Hand, amount: i32) {
    let stack = match hand {
        Hand::MainHand => ctx.inventory.selected_item().clone(),
        Hand::OffHand => ctx.inventory.offhand.clone(),
    };
    let damage = crate::durability::change(
        &stack,
        amount,
        ctx.actor.creative(),
        &mut LevelRandom(&mut *ctx.level),
    );
    let slot = match hand {
        Hand::MainHand => ctx.inventory.selected_item_mut(),
        Hand::OffHand => Some(&mut ctx.inventory.offhand),
    };
    let Some(tool) = slot else {
        return;
    };
    if damage == 0 {
        return;
    }
    let new_damage = item::damage(tool).saturating_add(damage);
    let broke = crate::advancements::view::apply_damage(
        ctx.advancements.as_deref_mut(),
        ctx.stats.as_deref_mut(),
        tool,
        new_damage,
    );
    if broke {
        equipment_broken(ctx, hand);
    }
}

/// The world's random source as the enchantment effects draw from it.
impl crate::enchant::FloatRandom for LevelRandom<'_> {
    fn next_float(&mut self) -> f32 {
        self.0.random_float()
    }
    fn next_gaussian(&mut self) -> f64 {
        self.0.random_gaussian()
    }
}

/// `LivingEntity.onEquippedItemBroken`: entity event 47 (main hand) or 48 (off hand), the
/// break sound and particles for the player and everyone tracking it.
fn equipment_broken(ctx: &mut PlayContext<'_>, hand: Hand) {
    let slot = match hand {
        Hand::MainHand => crate::item::EquipmentSlot::MainHand,
        Hand::OffHand => crate::item::EquipmentSlot::OffHand,
    };
    let (network_id, pos) = (ctx.actor.network_id, ctx.actor.pos);
    if let Some(level) = ctx.level.level_mut() {
        let packet = crate::durability::break_packet(network_id, slot);
        level.output().nearby_packets.push((pos, 128.0, packet));
    }
}

/// `blockActionRestricted`: spectators never, adventure players only with a main hand item
/// whose `can_break` matches the block.
fn block_action_restricted(ctx: &PlayContext<'_>, pos: BlockPos) -> bool {
    match ctx.actor.game_mode {
        GameMode::Spectator => true,
        GameMode::Adventure => {
            let tool = ctx.inventory.selected_item();
            tool.is_empty() || !item_use::can_break(ctx, tool, pos)
        }
        _ => false,
    }
}

/// `BlockItem.updateBlockStateFromTag`: the `block_state` component sets properties.
pub(crate) fn apply_block_state_component(
    stack: &ItemStack,
    state: BlockState,
) -> Option<BlockState> {
    let data = stack.component(ids::BLOCK_STATE)?;
    let mut r = Reader::new(data);
    let n = r.varint().ok()?;
    let mut out = state;
    for _ in 0..n.clamp(0, 64) {
        let key = r.string(32767).ok()?;
        let value = r.string(32767).ok()?;
        if let Some(s) = out.with(key, value) {
            out = s;
        }
    }
    (out != state).then_some(out)
}

/// `DoublePlantBlock` and its subclasses (tall flowers, tall grass, large fern, tall seagrass,
/// small dripleaf, pitcher crop).
fn is_double_plant(state: BlockState) -> bool {
    use vanillite_data::block::BlockKind;
    matches!(
        state.block().info().kind,
        BlockKind::DoublePlantBlock
            | BlockKind::TallFlowerBlock
            | BlockKind::TallSeagrassBlock
            | BlockKind::SmallDripleafBlock
            | BlockKind::PitcherCropBlock
    )
}

/// `Block.dropResources` + `popResource`: loot table drops as item entities.
fn drop_resources(
    ctx: &mut PlayContext<'_>,
    state: BlockState,
    pos: BlockPos,
    tool: &ItemStack,
    entity: Option<&crate::level::BlockEntity>,
    kept: Option<&vanillite_nbt::Compound>,
) {
    let mut stacks = Vec::new();
    {
        let data = ctx.data;
        let enchantment_level = |name: &str| data.enchantment_level(tool, name);
        let item_in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
        let components = |id: u16| {
            entity.and_then(|e| crate::level::block_entity::collect_component_with(e, kept, id))
        };
        let loot = BlockLootContext {
            state,
            tool,
            enchantment_level: &enchantment_level,
            item_in_tag: &item_in_tag,
            explosion_radius: None,
            this_present: true,
            entity: None,
            block_entity: Some(&components),
        };
        // `LootContext.Builder.create`: the table's random sequence, else the level random;
        // with the level, `location_check` sees the blocks around.
        if let Some(level) = ctx.level.level_mut() {
            let mut random = level.lend_random();
            data.loot
                .roll_block(level, pos, &loot, &mut random, &mut stacks);
            level.restore_random(random);
        } else {
            match (
                data.loot.block_sequence(state),
                ctx.level.random_sequences(),
            ) {
                (Some(key), Some(sequences)) => {
                    let mut sequences = sequences
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    data.loot
                        .block_drops(&loot, sequences.get(key), &mut stacks);
                }
                _ => {
                    let mut rng = LevelRandom(&mut *ctx.level);
                    data.loot.block_drops(&loot, &mut rng, &mut stacks);
                }
            }
        }
    }
    if ctx.hooks.wants(vanillite_abi::event::BLOCK_DROP) {
        let xp = ctx.level.level_mut().map_or(0, |level| {
            crate::block::break_hooks::break_experience(level, state, tool)
        });
        let Some(xp) = crate::block::drops::block_drop_event(
            ctx.hooks,
            ctx.actor.uuid,
            pos,
            state,
            &mut stacks,
            xp,
        ) else {
            return;
        };
        for stack in stacks {
            pop_resource(ctx.level, pos, stack);
        }
        if let Some(level) = ctx.level.level_mut() {
            crate::block::break_hooks::spawn_after_break_with(level, state, pos, tool, xp);
        }
        return;
    }
    for stack in stacks {
        pop_resource(ctx.level, pos, stack);
    }
    if let Some(level) = ctx.level.level_mut() {
        crate::block::break_hooks::spawn_after_break(level, state, pos, tool, true);
    }
}

/// The world's random source for loot rolls.
struct LevelRandom<'a>(&'a mut dyn PlayLevel);

impl crate::loot::LootRandom for LevelRandom<'_> {
    fn next_int(&mut self, bound: i32) -> i32 {
        self.0.random_int(bound.max(1))
    }

    fn next_float(&mut self) -> f32 {
        self.0.random_float()
    }
}

/// `Block.popResource`.
pub fn pop_resource(level: &mut dyn PlayLevel, pos: BlockPos, stack: ItemStack) {
    if let Some(level) = level.level_mut() {
        crate::block::drops::pop_resource(level, pos, stack);
        return;
    }
    if stack.is_empty() {
        return;
    }
    let half_height = 0.125;
    let offset = |level: &mut dyn PlayLevel| level.random_double() * 0.5 - 0.25;
    let x = f64::from(pos.x) + 0.5 + offset(level);
    let y = f64::from(pos.y) + 0.5 + offset(level) - half_height;
    let z = f64::from(pos.z) + 0.5 + offset(level);
    let velocity = Vec3::new(
        level.random_double() * 0.2 - 0.1,
        0.2,
        level.random_double() * 0.2 - 0.1,
    );
    level.spawn_item(Vec3::new(x, y, z), stack, velocity, 10, None);
}

/// `LivingEntity.createItemStackToDrop(stack, false, thrown)`: a stack thrown from the eye
/// position in look direction.
///
/// `own_random` is the player's own random (`Survival::own_random`).
pub fn throw_item(level: &mut dyn PlayLevel, actor: &Actor, drop: Drop, own_random: &mut i64) {
    if drop.stack.is_empty() {
        return;
    }
    let (from, velocity) = throw_motion(
        actor.pos,
        actor.eye_height,
        actor.y_rot,
        actor.x_rot,
        own_random,
    );
    let thrower = drop.thrown.then_some(actor.uuid);
    level.spawn_item(from, drop.stack, velocity, 40, thrower);
}

/// `createItemStackToDrop(stack, false, _)`'s start position and motion for a player at `pos`
/// with eye height `eye_height` looking at `y_rot`/`x_rot`; draws from `own_random`.
pub fn throw_motion(
    pos: Vec3,
    eye_height: f64,
    y_rot: f32,
    x_rot: f32,
    own_random: &mut i64,
) -> (Vec3, Vec3) {
    let y = pos.y + eye_height - f64::from(0.3_f32);
    let deg = std::f64::consts::PI / 180.0;
    let sin_x = sin(f64::from(x_rot) * deg);
    let cos_x = cos(f64::from(x_rot) * deg);
    let sin_y = sin(f64::from(y_rot) * deg);
    let cos_y = cos(f64::from(y_rot) * deg);
    // The player's own random, not the level's.
    let mut r = vanillite_base::random::LegacyRandom::from_state(*own_random);
    let (dir, pow2, spread) = {
        use vanillite_base::RandomSource;
        let dir = r.next_float() * std::f32::consts::TAU;
        let pow2 = 0.02 * r.next_float();
        let spread = r.next_float() - r.next_float();
        (dir, pow2, spread)
    };
    *own_random = r.state();
    let velocity = Vec3::new(
        f64::from(-sin_y * cos_x * 0.3) + f64::from(dir).cos() * f64::from(pow2),
        f64::from(-sin_x * 0.3 + 0.1 + spread * 0.1),
        f64::from(cos_y * cos_x * 0.3) + f64::from(dir).sin() * f64::from(pow2),
    );
    (Vec3::new(pos.x, y, pos.z), velocity)
}

/// `floor` of a position.
pub fn block_pos_of(v: Vec3) -> BlockPos {
    BlockPos::new(floor(v.x), floor(v.y), floor(v.z))
}

#[cfg(test)]
#[path = "play_tests.rs"]
mod tests;

/// The Vanilla game event ids.
fn events_ids() -> &'static crate::level::game_event::GameEvents {
    crate::level::game_event::events()
}
