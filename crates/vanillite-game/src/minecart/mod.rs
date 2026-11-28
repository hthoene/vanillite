//! Minecarts (`AbstractMinecart` and its kinds): placing them on rails, their movement with
//! the default (non-experimental) `OldMinecartBehavior` of 26.3, breaking them, riding,
//! activator rails, the furnace minecart's fuel and push, the TNT minecart's fuse and
//! explosion, and the inventories of chest and hopper minecarts.
//!
//! Minecarts tick with the other non-player entities ([`tick`]). What needs the whole level
//! (detector rails, a hopper minecart taking in items, explosions) is queued as
//! [`WorldAction`]s and runs right after the minecart's tick, in Vanilla's order.
//! Command block minecarts run their command on a powered activator rail; spawner minecarts
//! tick their spawner like a spawner block.

mod movement;

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::{BlockState, ids as block_ids};
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_data::item::{ItemId, ids as item_ids};
use vanillite_nbt::Compound;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{EntityEvent, SoundSource};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{EntityFlags, EntityId, NetworkIds, NewEntity, Rotation};
use vanillite_worldgen::behaviour::{PosExt, StateExt};

use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::{MobContext, WorldAction};
use crate::survival::Effect;
use crate::survival::damage::{DamageSource, tag};
use crate::survival::player::Env;
use crate::survival::rules::Rule;

pub use movement::{exits, max_speed, push_by};

/// The kinds of minecart (`Minecart`, `MinecartChest`, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinecartKind {
    Plain,
    Chest,
    Hopper,
    Furnace,
    Tnt,
    CommandBlock,
    Spawner,
}

impl MinecartKind {
    /// The minecart kind of an entity type.
    pub fn of(kind: EntityTypeId) -> Option<MinecartKind> {
        Some(match kind {
            k if k == ids::MINECART => MinecartKind::Plain,
            k if k == ids::CHEST_MINECART => MinecartKind::Chest,
            k if k == ids::HOPPER_MINECART => MinecartKind::Hopper,
            k if k == ids::FURNACE_MINECART => MinecartKind::Furnace,
            k if k == ids::TNT_MINECART => MinecartKind::Tnt,
            k if k == ids::COMMAND_BLOCK_MINECART => MinecartKind::CommandBlock,
            k if k == ids::SPAWNER_MINECART => MinecartKind::Spawner,
            _ => return None,
        })
    }

    /// The entity type.
    pub fn type_id(self) -> EntityTypeId {
        match self {
            MinecartKind::Plain => ids::MINECART,
            MinecartKind::Chest => ids::CHEST_MINECART,
            MinecartKind::Hopper => ids::HOPPER_MINECART,
            MinecartKind::Furnace => ids::FURNACE_MINECART,
            MinecartKind::Tnt => ids::TNT_MINECART,
            MinecartKind::CommandBlock => ids::COMMAND_BLOCK_MINECART,
            MinecartKind::Spawner => ids::SPAWNER_MINECART,
        }
    }

    /// The minecart kind an item places (`MinecartItem`).
    pub fn of_item(item: ItemId) -> Option<MinecartKind> {
        Some(match item {
            i if i == item_ids::MINECART => MinecartKind::Plain,
            i if i == item_ids::CHEST_MINECART => MinecartKind::Chest,
            i if i == item_ids::HOPPER_MINECART => MinecartKind::Hopper,
            i if i == item_ids::FURNACE_MINECART => MinecartKind::Furnace,
            i if i == item_ids::TNT_MINECART => MinecartKind::Tnt,
            i if i == item_ids::COMMAND_BLOCK_MINECART => MinecartKind::CommandBlock,
            _ => return None,
        })
    }

    /// `getDropItem`: command block and spawner minecarts drop a plain minecart.
    pub fn drop_item(self) -> ItemId {
        match self {
            MinecartKind::Chest => item_ids::CHEST_MINECART,
            MinecartKind::Hopper => item_ids::HOPPER_MINECART,
            MinecartKind::Furnace => item_ids::FURNACE_MINECART,
            MinecartKind::Tnt => item_ids::TNT_MINECART,
            MinecartKind::Plain | MinecartKind::CommandBlock | MinecartKind::Spawner => {
                item_ids::MINECART
            }
        }
    }

    /// `getContainerSize` of the container kinds; 0 for the others.
    pub fn container_size(self) -> usize {
        match self {
            MinecartKind::Chest => 27,
            MinecartKind::Hopper => 5,
            _ => 0,
        }
    }

    /// `getDefaultDisplayOffset`.
    pub fn default_display_offset(self) -> i32 {
        match self {
            MinecartKind::Chest => 8,
            MinecartKind::Hopper => 1,
            _ => 6,
        }
    }

    /// `getDefaultDisplayBlockState` (the furnace's `lit` follows its fuel).
    pub fn default_display(self, has_fuel: bool) -> BlockState {
        match self {
            MinecartKind::Plain => BlockState::AIR,
            MinecartKind::Chest => block_ids::CHEST.default_state().set("facing", "north"),
            MinecartKind::Hopper => block_ids::HOPPER.default_state(),
            MinecartKind::Furnace => block_ids::FURNACE
                .default_state()
                .set("facing", "north")
                .set_flag("lit", has_fuel),
            MinecartKind::Tnt => block_ids::TNT.default_state(),
            MinecartKind::CommandBlock => block_ids::COMMAND_BLOCK.default_state(),
            MinecartKind::Spawner => block_ids::SPAWNER.default_state(),
        }
    }
}

/// The kind data of a minecart.
#[derive(Clone, Debug)]
pub struct MinecartData {
    pub kind: MinecartKind,
    /// `DATA_ID_HURT`, `DATA_ID_HURTDIR`, `DATA_ID_DAMAGE`.
    pub hurt_time: i32,
    pub hurt_dir: i32,
    pub damage: f32,
    /// `DATA_ID_CUSTOM_DISPLAY_BLOCK` (`DisplayState`).
    pub display_state: Option<BlockState>,
    /// `DATA_ID_DISPLAY_OFFSET` (`DisplayOffset`).
    pub display_offset: i32,
    /// `flipped` (`FlippedRotation`).
    pub flipped: bool,
    pub on_rails: bool,
    /// `horizontalCollision` of the last move.
    pub horizontal_collision: bool,
    /// `firstTick` (saved as `HasTicked`).
    pub first_tick: bool,
    /// `wasTouchingWater` / lava after the last fluid update.
    pub in_water: bool,
    pub in_lava: bool,
    pub fall_distance: f64,
    /// Custom name, encoded like the `custom_name` component.
    pub custom_name: Option<Box<[u8]>>,
    /// `tickCount`.
    pub tick_count: i32,
    /// The entity's own random.
    pub random: LegacyRandom,
    /// The slots of chest and hopper minecarts.
    pub items: Vec<ItemStack>,
    /// `LootTable` and `LootTableSeed` until the loot is rolled
    /// ([`crate::block::chest_loot::unpack_entity`]).
    pub loot_table: Option<(String, i64)>,
    /// Hopper minecart: `enabled` (off on a powered activator rail).
    pub enabled: bool,
    /// Furnace minecart: `fuel` ticks, `push` and `DATA_ID_FUEL`.
    pub fuel: i32,
    pub push: Vec3,
    pub has_fuel: bool,
    /// TNT minecart: `fuse` (-1 unprimed), `explosion_power`, `explosion_speed_factor` and the
    /// entity that ignited it.
    pub fuse: i32,
    pub explosion_power: f32,
    pub explosion_speed_factor: f32,
    pub ignited_by: Option<EntityId>,
    /// Command block minecart: its `BaseCommandBlock` and `lastActivated`.
    pub command: crate::block::technical::command_block::BaseCommandBlock,
    pub last_activated: i32,
    /// What the kind saves that is not simulated (command block output).
    pub extra: Compound,
    /// Spawner minecart: its `BaseSpawner`.
    pub spawner: Option<Box<crate::block::spawner::MobSpawner>>,
}

impl MinecartData {
    /// A new minecart of `kind` whose random starts at `seed`.
    pub fn new(kind: MinecartKind, seed: i64) -> MinecartData {
        MinecartData {
            kind,
            hurt_time: 0,
            hurt_dir: 1,
            damage: 0.0,
            display_state: None,
            display_offset: kind.default_display_offset(),
            flipped: false,
            on_rails: false,
            horizontal_collision: false,
            first_tick: true,
            in_water: false,
            in_lava: false,
            fall_distance: 0.0,
            custom_name: None,
            tick_count: 0,
            random: LegacyRandom::new(seed),
            items: vec![ItemStack::EMPTY; kind.container_size()],
            loot_table: None,
            enabled: true,
            fuel: 0,
            push: Vec3::ZERO,
            has_fuel: false,
            fuse: -1,
            explosion_power: 4.0,
            explosion_speed_factor: 1.0,
            ignited_by: None,
            command: Default::default(),
            last_activated: 0,
            extra: Compound::new(),
            spawner: (kind == MinecartKind::Spawner).then(Box::default),
        }
    }

    /// `getDisplayBlockState`.
    pub fn display(&self) -> BlockState {
        self.display_state
            .unwrap_or_else(|| self.kind.default_display(self.has_fuel))
    }

    /// `isVehicle` is checked on the entity; this is `isRideable`.
    pub fn rideable(&self) -> bool {
        self.kind == MinecartKind::Plain
    }

    /// `isFurnace`.
    pub fn furnace(&self) -> bool {
        self.kind == MinecartKind::Furnace
    }
}

/// Adds a minecart (`EntityType.create` + `setInitialPos`).
pub fn spawn(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    uuid: u128,
    pos: Vec3,
    rot: Rotation,
    data: MinecartData,
) -> EntityId {
    let mut new = NewEntity::new(data.kind.type_id(), network_ids.next(), uuid, pos);
    new.rot = rot;
    let network_id = new.network_id.0;
    let minecarts = &mut ents.minecarts;
    let id = ents.store.spawn(new, |id| minecarts.push(id, data));
    ents.by_network_id.insert(network_id, id);
    id
}

impl Entities {
    /// The minecart data of an entity.
    pub fn minecart(&self, id: EntityId) -> Option<&MinecartData> {
        let e = self.store.get(id)?;
        MinecartKind::of(e.kind())?;
        self.minecarts.get(e.kind_row())
    }

    /// The minecart data of an entity, mutably.
    pub fn minecart_mut(&mut self, id: EntityId) -> Option<&mut MinecartData> {
        let e = self.store.get(id)?;
        MinecartKind::of(e.kind())?;
        let row = e.kind_row();
        self.minecarts.get_mut(row)
    }

    /// The slots of a container entity (`ContainerEntity`: chest and hopper minecarts, chest
    /// boats and rafts).
    pub fn container_items(&self, id: EntityId) -> Option<&[ItemStack]> {
        if let Some(m) = self.minecart(id) {
            return (m.kind.container_size() > 0).then_some(m.items.as_slice());
        }
        self.boat(id)
            .filter(|b| b.kind.chest)
            .map(|b| b.items.as_slice())
    }

    /// `LootTable` and `LootTableSeed` of a container entity that still has its loot to roll.
    pub fn container_loot_mut(&mut self, id: EntityId) -> Option<&mut Option<(String, i64)>> {
        if self.minecart(id).is_some() {
            return self.minecart_mut(id).map(|m| &mut m.loot_table);
        }
        self.boat_mut(id).map(|b| &mut b.loot_table)
    }

    /// The slots of a container entity, mutably.
    pub fn container_items_mut(&mut self, id: EntityId) -> Option<&mut [ItemStack]> {
        if self.minecart(id).is_some() {
            return self
                .minecart_mut(id)
                .filter(|m| m.kind.container_size() > 0)
                .map(|m| m.items.as_mut_slice());
        }
        self.boat_mut(id)
            .filter(|b| b.kind.chest)
            .map(|b| b.items.as_mut_slice())
    }
}

/// `AbstractMinecart.tick` (with the kind's additions) on the server.
pub fn tick(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) {
    let Some(m) = ents.minecart_mut(id) else {
        return;
    };
    m.tick_count += 1;
    let kind = m.kind;
    let mut dirty = false;
    if m.hurt_time > 0 {
        m.hurt_time -= 1;
        dirty = true;
    }
    if m.damage > 0.0 {
        m.damage -= 1.0;
        dirty = true;
    }
    if dirty && let Some(mut e) = ents.store.get_mut(id) {
        e.mark_data_dirty();
    }
    // `checkBelowWorld`.
    let y = ents.store.get(id).map_or(0.0, |e| e.pos().y);
    if y < f64::from(env.min_y - 64) {
        discard(cx, id);
        return;
    }
    movement::tick(level, ents, env, cx, id);
    if cx.discarded.contains(&id) {
        return;
    }
    update_fluids(ents, env, id);
    if ents.minecart(id).is_some_and(|m| m.in_lava) {
        // `lavaIgnite` (minecarts never burn: they do not tick fire), `lavaHurt`.
        let source = DamageSource::of(env.types.lava);
        hurt(ents, env, cx, id, source, 4.0);
        if let Some(m) = ents.minecart_mut(id) {
            m.fall_distance *= 0.5;
        }
    }
    if let Some(m) = ents.minecart_mut(id) {
        m.first_tick = false;
    }
    match kind {
        MinecartKind::Furnace => tick_furnace(ents, id),
        MinecartKind::Tnt => tick_tnt(ents, env, cx, id),
        MinecartKind::Hopper
            if ents.minecart(id).is_some_and(|m| m.enabled) && !cx.discarded.contains(&id) =>
        {
            cx.actions.push(WorldAction::HopperMinecart(id));
        }
        MinecartKind::Spawner if !cx.discarded.contains(&id) => {
            cx.actions.push(WorldAction::SpawnerMinecart(id));
        }
        _ => {}
    }
}

/// `updateFluidInteraction`: whether the minecart touches water or lava now; falling into
/// water splashes (`doWaterSplashEffect`, with draws from the minecart's random).
fn update_fluids(ents: &mut Entities, env: &mut Env<'_>, id: EntityId) {
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let (pos, velocity) = (e.pos(), e.velocity());
    let info = e.kind().info();
    let eye = pos + Vec3::new(0.0, f64::from(info.eye_height), 0.0);
    let contact = crate::survival::blocks::fluid_contact(env.blocks, env.facts, e.aabb(), eye);
    let Some(m) = ents.minecart_mut(id) else {
        return;
    };
    let splash = (contact.in_water() && !m.in_water && !m.first_tick)
        .then(|| crate::mob::living::water_splash_draws(&mut m.random, velocity, info.width, 0.2));
    m.in_water = contact.in_water();
    m.in_lava = contact.in_lava();
    if m.in_water {
        m.fall_distance = 0.0;
    }
    if let Some((volume, pitch)) = splash {
        crate::survival::play_sound(
            env.out,
            None,
            pos,
            "minecraft:entity.generic.splash",
            SoundSource::Neutral,
            volume,
            pitch,
            crate::survival::sound_seed(),
        );
        crate::survival::game_event(
            env.out,
            crate::level::game_event::events().splash,
            pos,
            Some(id),
        );
    }
}

/// `MinecartFurnace.tick` after the movement.
fn tick_furnace(ents: &mut Entities, id: EntityId) {
    let Some(m) = ents.minecart_mut(id) else {
        return;
    };
    if m.fuel > 0 {
        m.fuel -= 1;
    }
    if m.fuel <= 0 {
        m.push = Vec3::ZERO;
    }
    let has_fuel = m.fuel > 0;
    let changed = m.has_fuel != has_fuel;
    m.has_fuel = has_fuel;
    // The smoke particle draws from the entity's random on the server too.
    if m.has_fuel {
        m.random.next_int_bounded(4);
    }
    if changed && let Some(mut e) = ents.store.get_mut(id) {
        e.mark_data_dirty();
    }
}

/// `MinecartTNT.tick` after the movement.
fn tick_tnt(ents: &mut Entities, env: &mut Env<'_>, cx: &mut MobContext, id: EntityId) {
    let Some(m) = ents.minecart_mut(id) else {
        return;
    };
    if m.fuse > 0 {
        m.fuse -= 1;
    } else if m.fuse == 0 {
        let speed = ents
            .store
            .get(id)
            .map_or(0.0, |e| horizontal_sqr(e.velocity()));
        explode(ents, env, cx, id, speed);
    }
    if ents.minecart(id).is_some_and(|m| m.horizontal_collision) {
        let speed = ents
            .store
            .get(id)
            .map_or(0.0, |e| horizontal_sqr(e.velocity()));
        if speed >= f64::from(0.01_f32) {
            explode(ents, env, cx, id, speed);
        }
    }
}

/// `discard`: the entity is removed after this tick.
fn discard(cx: &mut MobContext, id: EntityId) {
    if !cx.discarded.contains(&id) {
        cx.discarded.push(id);
    }
}

fn horizontal_sqr(v: Vec3) -> f64 {
    v.x * v.x + v.z * v.z
}

/// `MinecartTNT.explode`: an explosion stronger with speed; the minecart is gone.
pub(crate) fn explode(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    speed_sqr: f64,
) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = ents.minecart_mut(id) else {
        return;
    };
    if env.rules.bool(Rule::TNT_EXPLODES) {
        let speed = speed_sqr.sqrt().min(5.0);
        let r = m.random.next_double();
        #[allow(clippy::cast_possible_truncation)]
        let radius = (f64::from(m.explosion_power)
            + f64::from(m.explosion_speed_factor) * r * 1.5 * speed) as f32;
        // A primed TNT minecart spares rails (`MinecartTNT.getBlockExplosionResistance`).
        let resistance = if m.fuse > -1 {
            crate::level::explosion::BlockResistance::TntMinecart
        } else {
            crate::level::explosion::BlockResistance::Default
        };
        cx.actions
            .push(WorldAction::Explode(crate::mob::ExplosionRequest {
                source: Some(id),
                indirect: m.ignited_by,
                pos,
                radius,
                mob: false,
                resistance,
                fire: false,
            }));
        discard(cx, id);
    } else if m.fuse > -1 {
        discard(cx, id);
    }
}

/// `MinecartTNT.primeFuse`: 80 ticks, entity event 70 and the primed sound.
fn prime_fuse(ents: &mut Entities, env: &mut Env<'_>, id: EntityId, cause: Option<EntityId>) {
    if !env.rules.bool(Rule::TNT_EXPLODES) {
        return;
    }
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let (pos, network_id) = (e.pos(), e.network_id().0);
    let Some(m) = ents.minecart_mut(id) else {
        return;
    };
    m.fuse = 80;
    if m.ignited_by.is_none() {
        m.ignited_by = cause;
    }
    env.out.push(Effect::Tracking {
        entity: id,
        with_self: false,
        packet: SharedPacket::of(&EntityEvent {
            entity_id: network_id,
            event: 70,
        }),
    });
    crate::survival::play_sound(
        env.out,
        None,
        pos,
        "minecraft:entity.tnt.primed",
        SoundSource::Blocks,
        1.0,
        1.0,
        crate::survival::sound_seed(),
    );
}

/// `activateMinecart(level, x, y, z, powered)` of an activator rail under the minecart.
pub(crate) fn activate(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    powered: bool,
) {
    let Some(m) = ents.minecart_mut(id) else {
        return;
    };
    match m.kind {
        MinecartKind::Plain => {
            if !powered {
                return;
            }
            let hurt_now = m.hurt_time == 0;
            if hurt_now {
                m.hurt_dir = -m.hurt_dir;
                m.hurt_time = 10;
                m.damage = 50.0;
            }
            if !ents.riding.passengers_of(id).is_empty() {
                crate::riding::eject_passengers(ents, env, id);
            }
            if hurt_now && let Some(mut e) = ents.store.get_mut(id) {
                e.mark_data_dirty();
                e.set_flags(EntityFlags::HURT_MARKED, true);
            }
        }
        MinecartKind::Hopper => m.enabled = !powered,
        MinecartKind::Tnt => {
            if powered && m.fuse < 0 {
                prime_fuse(ents, env, id, None);
            }
        }
        // `performCommand` every 4 ticks; the command runs on the level right after the
        // minecart's tick.
        MinecartKind::CommandBlock if powered && m.tick_count - m.last_activated >= 4 => {
            m.last_activated = m.tick_count;
            cx.actions.push(WorldAction::MinecartCommand(id));
        }
        _ => {}
    }
}

/// `damageSourceIgnitesTnt`.
fn ignites_tnt(ents: &Entities, env: &Env<'_>, source: &DamageSource) -> bool {
    if let Some(direct) = source.direct
        && let Some(arrow) = ents.arrow(direct)
    {
        return arrow.fire > 0;
    }
    let tags = env.types.get(source.kind).tags;
    tags & (tag::IS_FIRE | tag::IS_EXPLOSION) != 0
}

/// `VehicleEntity.hurtServer` of a minecart: hits shake it; enough damage, or any hit of a
/// creative player, breaks it. Returns whether the hit counted.
pub fn hurt(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    source: DamageSource,
    amount: f32,
) -> bool {
    if cx.discarded.contains(&id) {
        return true;
    }
    let Some(kind) = ents.minecart(id).map(|m| m.kind) else {
        return false;
    };
    if crate::mob::entity_nbt::invulnerable_to(ents, env.types, id, &source) {
        return false;
    }
    // `MinecartTNT.hurtServer`: a burning arrow sets it off.
    if kind == MinecartKind::Tnt
        && let Some(direct) = source.direct
        && let Some(arrow) = ents.arrow(direct)
        && arrow.fire > 0
    {
        let speed = ents
            .store
            .get(direct)
            .map_or(0.0, |e| e.velocity().length_sqr());
        if let Some(m) = ents.minecart_mut(id) {
            m.ignited_by = source.cause;
        }
        explode(ents, env, cx, id, speed);
        if cx.discarded.contains(&id) {
            return true;
        }
    }
    let creative = source
        .cause
        .and_then(|c| ents.player(c))
        .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Creative);
    let source_destroys = kind == MinecartKind::Tnt && ignites_tnt(ents, env, &source);
    let Some(m) = ents.minecart_mut(id) else {
        return false;
    };
    m.hurt_dir = -m.hurt_dir;
    m.hurt_time = 10;
    m.damage += amount * 10.0;
    let destroy = (!creative && m.damage > 40.0) || source_destroys;
    if let Some(mut e) = ents.store.get_mut(id) {
        e.mark_data_dirty();
        e.set_flags(EntityFlags::HURT_MARKED, true);
    }
    damage_game_event(ents, cx, id, source.cause);
    if destroy {
        destroy_minecart(ents, env, cx, id, &source);
    } else if creative {
        crate::riding::eject_passengers(ents, env, id);
        discard(cx, id);
    }
    true
}

/// `gameEvent(ENTITY_DAMAGE, source.getEntity())` of a vehicle that was hurt.
pub(crate) fn damage_game_event(
    ents: &Entities,
    cx: &mut MobContext,
    id: EntityId,
    cause: Option<EntityId>,
) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    cx.actions.push(crate::mob::WorldAction::Level(
        crate::mob::boss::warden::LevelAction::GameEvent {
            event: crate::level::game_event::events().entity_damage,
            pos,
            context: crate::level::game_event::GameEventContext::of(cause),
        },
    ));
}

/// `destroy(level, source)` of the minecart kinds.
fn destroy_minecart(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    source: &DamageSource,
) {
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let (pos, velocity) = (e.pos(), e.velocity());
    let ignites = ignites_tnt(ents, env, source);
    let by_player = source.direct.filter(|&d| ents.player(d).is_some());
    let Some(m) = ents.minecart_mut(id) else {
        return;
    };
    if m.kind == MinecartKind::Tnt {
        // `MinecartTNT.destroy`: fire, explosions and speed prime it instead.
        let speed = horizontal_sqr(velocity);
        if ignites || speed >= f64::from(0.01_f32) {
            if m.fuse < 0 {
                let cause = source.cause;
                prime_fuse(ents, env, id, cause);
                if let Some(m) = ents.minecart_mut(id) {
                    m.fuse = m.random.next_int_bounded(20) + m.random.next_int_bounded(20);
                }
            }
            return;
        }
    }
    let items = std::mem::take(&mut m.items);
    let loot = m.loot_table.take();
    // `chestVehicleDestroyed`: a player breaking a container minecart angers piglins.
    let anger = matches!(m.kind, MinecartKind::Chest | MinecartKind::Hopper)
        && env.rules.bool(Rule::ENTITY_DROPS)
        && by_player.is_some();
    let drop = m.kind.drop_item();
    let name = m.custom_name.clone();
    crate::riding::eject_passengers(ents, env, id);
    discard(cx, id);
    let mut stack = ItemStack::new(drop.0, 1);
    if let Some(n) = name {
        stack.set_component(vanillite_protocol::components::ids::CUSTOM_NAME, n);
    }
    let entity_drops = env.rules.bool(Rule::ENTITY_DROPS);
    if let Some((table, seed)) = loot.filter(|_| !items.is_empty()) {
        // The loot is rolled first, which needs the world.
        cx.actions
            .push(WorldAction::DestroyLootVehicle(Box::new(LootVehicleDrop {
                pos,
                table,
                seed,
                slots: items.len(),
                drop: Some(stack),
                entity_drops,
            })));
        if anger && let Some(p) = by_player {
            cx.actions.push(WorldAction::PiglinAnger(p));
        }
        return;
    }
    // `kill` → `remove(KILLED)`: a container drops its contents (whatever the rules say).
    drop_contents(env, pos, &items);
    if entity_drops {
        spawn_at_location(env, pos, stack);
        if !items.is_empty() {
            // `chestVehicleDestroyed` drops the (now empty) contents once more.
            drop_contents(env, pos, &vec![ItemStack::EMPTY; items.len()]);
        }
    }
    if anger && let Some(p) = by_player {
        cx.actions.push(WorldAction::PiglinAnger(p));
    }
}

/// A destroyed chest minecart or chest boat whose loot table was never rolled: the world
/// rolls it, then drops what `destroy` drops ([`destroy_with_loot`]).
#[derive(Clone, Debug, PartialEq)]
pub struct LootVehicleDrop {
    pub pos: Vec3,
    pub table: String,
    pub seed: i64,
    /// The container's size.
    pub slots: usize,
    /// The vehicle's own item.
    pub drop: Option<ItemStack>,
    /// The `entity_drops` game rule.
    pub entity_drops: bool,
}

/// `destroy` of a container vehicle with a loot table: `remove(KILLED)` rolls the table
/// (`getItem` → `unpackChestVehicleLootTable(null)`) and drops the contents; with entity
/// drops the vehicle's item follows and `chestVehicleDestroyed` drops the (now empty)
/// contents once more.
pub fn destroy_with_loot(level: &mut Level, d: LootVehicleDrop) {
    let mut items = vec![ItemStack::EMPTY; d.slots];
    crate::block::chest_loot::fill(level, &d.table, d.seed, d.pos, 0.0, None, &mut items);
    let base = BlockPos::containing(d.pos.x, d.pos.y, d.pos.z);
    for stack in items {
        crate::block::container::drop_item_stack(level, base, stack);
    }
    if d.entity_drops {
        if let Some(stack) = d.drop {
            crate::block::drops::spawn_item_moving(level, d.pos, stack, None, 10, None);
        }
        for _ in 0..d.slots {
            crate::block::container::drop_item_stack(level, base, ItemStack::EMPTY);
        }
    }
}

/// `spawnAtLocation(stack)`: an item at the entity's position with the default pickup delay
/// (the item draws its yaw and motion from its own random).
pub(crate) fn spawn_at_location(env: &mut Env<'_>, pos: Vec3, stack: ItemStack) {
    let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
    env.out.push(Effect::DropItem {
        random: item_random,
        uuid,
        pos,
        velocity,
        y_rot,
        pickup_delay: 10,
        stack,
    });
}

/// `Containers.dropContents(level, entity, container)`: every slot drops in parts of 10 to 30
/// items around the block the entity is in.
pub(crate) fn drop_contents(env: &mut Env<'_>, pos: Vec3, items: &[ItemStack]) {
    let base = BlockPos::containing(pos.x, pos.y, pos.z);
    for stack in items {
        let mut stack = stack.clone();
        let r = &mut *env.random;
        let size = 0.25;
        let center_range = 1.0 - size;
        let half = size / 2.0;
        let xo = f64::from(base.x) + r.next_double() * center_range + half;
        let yo = f64::from(base.y) + r.next_double() * center_range;
        let zo = f64::from(base.z) + r.next_double() * center_range + half;
        while !stack.is_empty() {
            let r = &mut *env.random;
            let n = r.next_int_bounded(21) + 10;
            let part = stack.split(n);
            let mut triangle = |mode: f64| {
                let a = r.next_double();
                let b = r.next_double();
                mode + 0.114_850_001_711_398_36 * (a - b)
            };
            let velocity = Vec3::new(triangle(0.0), triangle(0.2), triangle(0.0));
            // `new ItemEntity(level, x, y, z, stack)` draws its yaw and first motion from the
            // item's own random.
            let (uuid, y_rot, _, item_random) = crate::mob::new_item_entity();
            env.out.push(Effect::DropItem {
                random: item_random,
                uuid,
                pos: Vec3::new(xo, yo, zo),
                velocity,
                y_rot,
                pickup_delay: 0,
                stack: part,
            });
        }
    }
}

/// The menu a container entity opens (`createMenu`): three rows for chests, the hopper menu
/// for hopper minecarts.
pub fn menu_request(
    ents: &Entities,
    id: EntityId,
    player: EntityId,
) -> Option<crate::menu::container::MenuRequest> {
    use crate::menu::container::{MenuKind, MenuRequest, Storage};
    let e = ents.store.get(id)?;
    let (menu, name) = if let Some(m) = ents.minecart(id) {
        let menu = match m.kind {
            MinecartKind::Chest => MenuKind::Generic { rows: 3 },
            MinecartKind::Hopper => MenuKind::Hopper,
            _ => return None,
        };
        (menu, m.custom_name.clone())
    } else {
        let b = ents.boat(id).filter(|b| b.kind.chest)?;
        (MenuKind::Generic { rows: 3 }, b.custom_name.clone())
    };
    let title = name
        .and_then(|n| crate::mob::nbt::component_tag(&n))
        .map(|t| vanillite_protocol::text::Component::from_nbt(&t))
        .unwrap_or_else(|| {
            let type_name = e.kind().info().name;
            let path = type_name.strip_prefix("minecraft:").unwrap_or(type_name);
            vanillite_protocol::text::Component::translatable(
                format!("entity.minecraft.{path}"),
                Vec::new(),
            )
        });
    let p = e.pos();
    Some(MenuRequest {
        player: Some(player),
        menu,
        storage: Storage::Entity(id),
        title,
        pos: BlockPos::containing(p.x, p.y, p.z),
        block: vanillite_data::block::BlockId(0),
    })
}

/// `interact` of a minecart.
pub fn interact(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    sneaking: bool,
) -> Interaction {
    let Some(kind) = ents.minecart(target).map(|m| m.kind) else {
        return Interaction::Pass;
    };
    match kind {
        MinecartKind::Plain => {
            // `Minecart.interact`: get in unless sneaking or taken.
            if sneaking || !ents.riding.passengers_of(target).is_empty() {
                return Interaction::Pass;
            }
            if crate::riding::start_riding(ents, env, player, target, false) {
                Interaction::Success
            } else {
                Interaction::Consume
            }
        }
        MinecartKind::Chest | MinecartKind::Hopper => {
            open_container(ents, env, cx, player, target);
            Interaction::Success
        }
        MinecartKind::Furnace => {
            // `MinecartFurnace.interact`: coal or charcoal adds 3600 ticks of fuel and pushes
            // the minecart away from the player.
            let stack = crate::mob::animal::held(ents, player, hand);
            let fuel = crate::player::play::game_data()
                .is_some_and(|d| d.item_in_tag(stack.item, "minecraft:furnace_minecart_fuel"));
            let ppos = ents.store.get(player).map(|e| e.pos());
            let mpos = ents.store.get(target).map(|e| e.pos());
            if let (Some(ppos), Some(mpos)) = (ppos, mpos)
                && let Some(m) = ents.minecart_mut(target)
                && fuel
                && m.fuel + 3600 <= 32000
            {
                m.fuel += 3600;
                let d = mpos - ppos;
                m.push = Vec3::new(d.x, 0.0, d.z);
                crate::mob::animal::use_item(ents, player, hand);
            }
            Interaction::Success
        }
        // `MinecartCommandBlock.interact`: the client opens its editor for operators.
        MinecartKind::CommandBlock => {
            let creative = ents
                .player(player)
                .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Creative);
            if creative {
                Interaction::Success
            } else {
                Interaction::Pass
            }
        }
        MinecartKind::Tnt | MinecartKind::Spawner => Interaction::Pass,
    }
}

/// `interactWithContainerVehicle`: the player opens the container (spectators not while it
/// still has a loot table).
pub fn open_container(
    ents: &mut Entities,
    _env: &mut Env<'_>,
    cx: &mut MobContext,
    player: EntityId,
    target: EntityId,
) {
    if let Some(request) = menu_request(ents, target, player) {
        cx.menus.push(request);
    }
}

/// `MinecartItem.useOn`: a minecart on the rail that was clicked. Returns whether the item
/// was used (the stack shrinks unless `infinite`).
pub fn use_item_on(
    level: &Level,
    ents: &mut Entities,
    network_ids: &NetworkIds,
    pos: BlockPos,
    stack: &mut ItemStack,
    infinite: bool,
    op: bool,
) -> bool {
    let Some(kind) = MinecartKind::of_item(crate::item::item_id(stack)) else {
        return false;
    };
    let state = level.block(pos);
    if !crate::block::is_rail(state.block()) {
        return false;
    }
    let shape = crate::block::rail::RailShape::of(state);
    let offset = if shape.is_slope() { 0.5 } else { 0.0 };
    let at = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.0625 + offset,
        f64::from(pos.z) + 0.5,
    );
    let mut data = MinecartData::new(kind, crate::survival::sound_seed());
    data.custom_name = stack
        .component(vanillite_protocol::components::ids::CUSTOM_NAME)
        .map(Into::into);
    let id = spawn(
        ents,
        network_ids,
        crate::block::tnt::insecure_uuid(),
        at,
        Rotation::default(),
        data,
    );
    crate::vehicle::apply_stack_entity_data(ents, id, stack, op);
    if !ents.spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0) {
        return false;
    }
    if !infinite {
        stack.shrink(1);
    }
    true
}

/// `MinecartDispenseItemBehavior.execute`: a dispenser puts a minecart onto the rail in front
/// of it (or one below the block in front). Returns whether it did.
pub fn dispense(
    level: &mut Level,
    kind: MinecartKind,
    dispenser: BlockPos,
    facing: vanillite_base::Direction,
    stack: &ItemStack,
) -> bool {
    use vanillite_base::Direction;
    let [fx, fy, fz] = facing.normal();
    let x = f64::from(dispenser.x) + 0.5 + f64::from(fx) * 1.125;
    let y = (f64::from(dispenser.y) + 0.5).floor() + f64::from(fy);
    let z = f64::from(dispenser.z) + 0.5 + f64::from(fz) * 1.125;
    let front = dispenser.relative(facing);
    let front_state = level.block(front);
    let offset = if crate::block::is_rail(front_state.block()) {
        if crate::block::rail::RailShape::of(front_state).is_slope() {
            0.6
        } else {
            0.1
        }
    } else {
        let below_state = level.block(front.below());
        if !front_state.is_air() || !crate::block::is_rail(below_state.block()) {
            return false;
        }
        if facing != Direction::Down && crate::block::rail::RailShape::of(below_state).is_slope() {
            -0.4
        } else {
            -0.9
        }
    };
    let at = Vec3::new(x, y + offset, z);
    let mut data = MinecartData::new(kind, crate::survival::sound_seed());
    data.custom_name = stack
        .component(vanillite_protocol::components::ids::CUSTOM_NAME)
        .map(Into::into);
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    let id = spawn(
        &mut level.entities,
        &network_ids,
        crate::block::tnt::insecure_uuid(),
        at,
        Rotation::default(),
        data,
    );
    crate::vehicle::apply_stack_entity_data(&mut level.entities, id, stack, false);
    level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::DISPENSER, 0);
    true
}

/// `getPassengerAttachmentPoint`: villagers and wandering traders sit lower.
pub fn passenger_height(ents: &Entities, passenger: EntityId) -> f64 {
    let lowered = ents
        .store
        .get(passenger)
        .is_some_and(|e| e.kind() == ids::VILLAGER || e.kind() == ids::WANDERING_TRADER);
    if lowered { 0.0 } else { 0.1875 }
}

/// `getRedstoneSignalFromContainer` of a container minecart (detector rail comparators).
pub fn container_signal(m: &MinecartData) -> i32 {
    crate::menu::redstone_signal(&m.items, crate::menu::CONTAINER_MAX)
}

/// A hopper minecart takes in one item (`MinecartHopper.tryConsumeItems`, run after its
/// tick with the whole level): from the container above, else from item entities above and
/// around it.
pub fn hopper_suck(level: &mut Level, id: EntityId) {
    let Some(e) = level.entities.store.get(id) else {
        return;
    };
    let (pos, aabb) = (e.pos(), e.aabb());
    if !level.entities.minecart(id).is_some_and(|m| m.enabled) {
        return;
    }
    let level_y = pos.y + 0.5;
    if crate::block::hopper::suck_into_entity(level, id, Vec3::new(pos.x, level_y, pos.z)) {
        return;
    }
    // `getEntitiesOfClass(ItemEntity, box.inflate(0.25, 0, 0.25))`.
    let area = aabb.inflate(0.25, 0.0, 0.25);
    let mut found = Vec::new();
    level
        .entities
        .store
        .for_each_in(&area, false, |o| found.push(o));
    let into = crate::block::container::ContainerRef::Entity(id);
    for o in found {
        if crate::block::hopper::add_item_entity(level, into, o) {
            return;
        }
    }
}

/// `causeFallDamage` of a TNT minecart: a fall of 3 or more blocks sets it off.
pub(crate) fn fall_on(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    fall_distance: f64,
) {
    if ents
        .minecart(id)
        .is_some_and(|m| m.kind == MinecartKind::Tnt)
        && fall_distance >= 3.0
    {
        let power = fall_distance / 10.0;
        explode(ents, env, cx, id, power * power);
    }
}

/// `thunderHit` and other plain damage of a minecart by its entity id, for callers that do not
/// know the kind.
pub fn is_minecart(ents: &Entities, id: EntityId) -> bool {
    ents.minecart(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_drops() {
        assert_eq!(
            MinecartKind::of(ids::HOPPER_MINECART),
            Some(MinecartKind::Hopper)
        );
        assert!(MinecartKind::of(ids::OAK_BOAT).is_none());
        assert_eq!(MinecartKind::CommandBlock.drop_item(), item_ids::MINECART);
        assert_eq!(MinecartKind::Chest.container_size(), 27);
        assert_eq!(MinecartKind::Hopper.default_display_offset(), 1);
        assert_eq!(
            MinecartKind::of_item(item_ids::TNT_MINECART),
            Some(MinecartKind::Tnt)
        );
    }
}
