//! Item entities (`ItemEntity`): movement with block collisions, merging, despawning and
//! pickup.

use vanillite_base::math::floor;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::{BlockState, flags as block_flags};

use vanillite_world::entity::EntityId;

use crate::item::{self, ItemStack};
use crate::survival::damage::{DamageKind, DamageTypes};

/// `INFINITE_PICKUP_DELAY`.
pub const NEVER_PICK_UP: i16 = 32767;
/// `INFINITE_LIFETIME`.
pub const UNLIMITED_LIFETIME: i16 = -32768;

/// The kind data of an item entity.
#[derive(Clone, Debug)]
pub struct ItemData {
    /// The stack (`DATA_ITEM`).
    pub stack: ItemStack,
    /// Ticks lived.
    pub age: i16,
    /// Ticks until it can be picked up.
    pub pickup_delay: i16,
    /// Who threw it.
    pub thrower: Option<u128>,
    /// Only this player may pick it up (`target`).
    pub target: Option<u128>,
    /// `tickCount`.
    pub tick_count: u32,
    /// `health`: explosions and fire wear it down from 5.
    pub health: i16,
    /// `remainingFireTicks`.
    pub fire_ticks: i32,
    /// The 48-bit state of the item's own random (`Entity.random`, not saved: a loaded item
    /// gets a fresh one).
    pub random: i64,
    /// `wasTouchingWater`: the item touched water at its last fluid update (not saved).
    pub in_water: bool,
    /// `stuckSpeedMultiplier`: a cobweb or powder snow slows the next move (not saved).
    pub stuck: Vec3,
    /// `finalMovementsThisTick`: the way of the last move, walked again while the item rests
    /// (`applyEffectsFromBlocksForLastMovements`; not saved).
    pub last_movement: Option<crate::survival::inside::Movement>,
    /// `fallDistance` (not saved).
    pub fall_distance: f64,
}

impl ItemData {
    /// A new item entity's data.
    /// `random` is the state of its own random ([`crate::mob::new_item_entity`],
    /// [`loaded_random`]).
    pub fn new(
        stack: ItemStack,
        pickup_delay: i16,
        thrower: Option<u128>,
        random: i64,
    ) -> ItemData {
        ItemData {
            stack,
            age: 0,
            pickup_delay,
            thrower,
            target: None,
            tick_count: 0,
            health: 5,
            fire_ticks: 0,
            random,
            in_water: false,
            stuck: Vec3::ZERO,
            last_movement: None,
            fall_distance: 0.0,
        }
    }

    /// Draws from the item's own random.
    fn with_random<R>(&mut self, draw: impl FnOnce(&mut LegacyRandom) -> R) -> R {
        let mut random = LegacyRandom::from_state(self.random);
        let value = draw(&mut random);
        self.random = random.state();
        value
    }

    /// `random.nextFloat()` of the item's own random.
    pub fn next_float(&mut self) -> f32 {
        self.with_random(|r| r.next_float())
    }

    /// `isMergable`.
    pub fn is_mergable(&self, lifetime: i16) -> bool {
        !self.stack.is_empty()
            && self.pickup_delay != NEVER_PICK_UP
            && self.age != UNLIMITED_LIFETIME
            && self.age < lifetime
            && self.stack.count < item::max_stack_size(&self.stack)
    }
}

/// The random state of an item entity that `EntityType.create` makes (loading, `/summon`):
/// its random takes `seed`, then the constructor draws `bobOffs` and the yaw (which the saved
/// rotation replaces).
pub fn loaded_random(seed: i64) -> i64 {
    let mut random = LegacyRandom::new(seed);
    let _bob = random.next_float();
    let _y_rot = random.next_float();
    random.state()
}

/// What the fire handling of an item entity reads of the world.
#[derive(Clone, Copy)]
pub struct FireWorld<'a> {
    pub blocks: &'a dyn crate::survival::blocks::BlockAccess,
    pub facts: &'a crate::survival::blocks::BlockFacts,
    pub types: &'a DamageTypes,
    /// `Level.isRainingAt`.
    pub raining_at: &'a dyn Fn(BlockPos) -> bool,
    /// Whether a block has an `entityInside` behaviour of its own (pressure plates, tripwire,
    /// big dripleaves, ...): [`burn_effects`] runs it.
    pub reacts: &'a dyn Fn(BlockState) -> bool,
}

impl std::fmt::Debug for FireWorld<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FireWorld").finish_non_exhaustive()
    }
}

/// The sound of `Entity.doWaterSplashEffect`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Splash {
    /// Where the item was.
    pub pos: Vec3,
    pub volume: f32,
    pub pitch: f32,
}

/// What fire, lava and water did to an item entity in one tick ([`burn_effects`] tells the
/// world).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Burn {
    /// The item has the `Invulnerable` tag (set before the tick): nothing here hurts it.
    pub invulnerable: bool,
    /// The item fell into water in `baseTick`, before it moved.
    pub splash_before: Option<Splash>,
    /// How often the item was hurt (`ItemEntity.hurtServer`: one `ENTITY_DAMAGE` each).
    pub hurts: u8,
    /// The pitch of the burn sound of `lavaHurt`.
    pub lava_sound: Option<f32>,
    /// The pitch of `playEntityOnFireExtinguishedSound`: water or rain put the item out.
    pub extinguished: Option<f32>,
    /// The item fell into water with its move.
    pub splash_after: Option<Splash>,
    /// Its health ran out: `Item.onDestroyed` and `discard`.
    pub destroyed: bool,
    /// The blocks with an `entityInside` of their own the item is in, in Vanilla's order.
    pub inside: [Option<BlockPos>; 4],
    /// `checkFallDamage`: the item landed after a fall, at this position on this block
    /// (`fallOn` and the `HIT_GROUND` game event).
    pub landed: Option<(Vec3, BlockPos)>,
    /// `applyEffectsFromBlocks` on the ground: the block with a `stepOn` of its own below it.
    pub step_on: Option<BlockPos>,
    /// The tops of the bubble columns the item is above (`onAboveBubbleColumn`): their
    /// particles draw from the level random.
    pub bubble_tops: [Option<BlockPos>; 2],
    /// The item bounced on a block (`bounce` game event).
    pub bounced: bool,
    /// The moving item is in powder snow at this block: the snowflake draws the level random.
    pub snowflake: Option<BlockPos>,
    /// The burning item melts this powder snow (with `mob_griefing`) or lowers this cauldron.
    pub melt_snow: Option<BlockPos>,
    pub cauldron_fire: Option<BlockPos>,
}

impl Burn {
    /// Nothing happened (nearly every tick of nearly every item).
    pub fn is_none(&self) -> bool {
        self.hurts == 0
            && self.inside[0].is_none()
            && self.landed.is_none()
            && self.step_on.is_none()
            && self.bubble_tops[0].is_none()
            && !self.bounced
            && self.snowflake.is_none()
            && self.melt_snow.is_none()
            && self.cauldron_fire.is_none()
            && self.splash_before.is_none()
            && self.splash_after.is_none()
            && self.extinguished.is_none()
    }
}

/// What an item entity touches of water and lava (`EntityFluidInteraction.update`).
pub type Fluids = (
    crate::survival::blocks::FluidContact,
    crate::mob::living::Currents,
);

/// `Entity.updateFluidInteraction` of an item at `pos` moving with `velocity`: falling into
/// water splashes, flowing water and lava push it. Whether it touches a fluid.
fn update_fluid_interaction(
    data: &mut ItemData,
    (contact, currents): Fluids,
    first_tick: bool,
    fast_lava: bool,
    pos: Vec3,
    velocity: &mut Vec3,
) -> (bool, Option<Splash>) {
    let in_water = contact.in_water();
    if in_water {
        data.fall_distance = 0.0;
    }
    let splash =
        (in_water && !data.in_water && !first_tick).then(|| water_splash(data, pos, *velocity));
    data.in_water = in_water;
    if in_water && let Some(pushed) = currents.water.push(*velocity, 0.014) {
        *velocity = pushed;
    }
    if contact.in_lava() {
        let scale = if fast_lava {
            0.007
        } else {
            0.002_333_333_333_333_333_5
        };
        if let Some(pushed) = currents.lava.push(*velocity, scale) {
            *velocity = pushed;
        }
    }
    (in_water || contact.in_lava(), splash)
}

/// `Entity.doWaterSplashEffect` of an item: the splash sound, and the draws of the bubble
/// and splash particles (the server adds none).
fn water_splash(data: &mut ItemData, pos: Vec3, v: Vec3) -> Splash {
    let (volume, pitch) =
        data.with_random(|r| crate::mob::living::water_splash_draws(r, v, 0.25, 0.2));
    Splash { pos, volume, pitch }
}

/// `ItemEntity.hurtServer`: damage the stack does not resist (`canBeHurtBy`) wears the health
/// down.
fn hurt(data: &mut ItemData, types: &DamageTypes, kind: DamageKind, damage: f32, burn: &mut Burn) {
    if burn.invulnerable || burn.destroyed || !item::can_be_hurt_by(&data.stack, types, kind) {
        return;
    }
    #[allow(clippy::cast_possible_truncation)]
    let health = (f32::from(data.health) - damage) as i16;
    data.health = health;
    burn.hurts = burn.hurts.saturating_add(1);
    burn.destroyed = health <= 0;
}

/// `ItemEntity.fireImmune`: the stack resists fire.
fn fire_immune(data: &ItemData, types: &DamageTypes) -> bool {
    !item::can_be_hurt_by(&data.stack, types, types.in_fire)
}

/// The fire part of `Entity.baseTick` for an item, before it moves: a burning item takes one
/// fire damage every second outside lava (`isInLava`).
fn base_tick_fire(data: &mut ItemData, in_lava: bool, world: &FireWorld<'_>, burn: &mut Burn) {
    if data.fire_ticks <= 0 {
        return;
    }
    if fire_immune(data, world.types) {
        data.fire_ticks = 0;
        return;
    }
    if data.fire_ticks % 20 == 0 && !in_lava {
        hurt(data, world.types, world.types.on_fire, 1.0, burn);
    }
    data.fire_ticks -= 1;
}

/// `applyEffectsFromBlocks` of an item at `pos` on the way `movement`: cobwebs and powder
/// snow slow its next move (`makeStuckInBlock`), fire ignites and hurts it
/// (`BaseFireBlock.entityInside`), lava ignites and hurts it (`lavaIgnite`, `lavaHurt`), water
/// and rain put it out (`playEntityOnFireExtinguishedSound`). The pitches of the sounds come
/// from the item's own random.
fn inside_blocks_fire(
    data: &mut ItemData,
    movement: crate::survival::inside::Movement,
    pos: Vec3,
    world: &FireWorld<'_>,
    burn: &mut Burn,
    v: &mut Vec3,
) {
    let dims = vanillite_world::entity::Dimensions {
        width: 0.25,
        height: 0.25,
    };
    let mut cacti = 0_u8;
    let mut reacting = 0;
    let inside = crate::survival::blocks::inside_blocks(
        world.blocks,
        world.facts,
        dims,
        false,
        &[movement],
        |cell| {
            if cell.state.block() == vanillite_data::block::ids::CACTUS {
                cacti = cacti.saturating_add(1);
            } else if cell.state.block() == vanillite_data::block::ids::BUBBLE_COLUMN {
                if cell.in_block && cell.precise {
                    bubble_column(data, cell, world, burn, v);
                }
            } else if (world.reacts)(cell.state)
                && let Some(slot) = burn.inside.get_mut(reacting)
            {
                *slot = Some(cell.pos);
                reacting += 1;
            }
        },
    );
    // `CactusBlock.entityInside`: one point of damage per cactus touched.
    for _ in 0..cacti {
        hurt(data, world.types, world.types.cactus, 1.0, burn);
    }
    if inside.cobweb {
        // `WebBlock.entityInside`.
        data.stuck = Vec3::new(0.25, f64::from(0.05_f32), 0.25);
        data.fall_distance = 0.0;
    }
    if let Some(snow) = inside.powder_snow {
        // `PowderSnowBlock.entityInside`, with the snowflake of a moving item.
        data.stuck = Vec3::new(f64::from(0.9_f32), 1.5, f64::from(0.9_f32));
        data.fall_distance = 0.0;
        if v.x != 0.0 || v.z != 0.0 {
            burn.snowflake = Some(snow);
        }
    }
    if !inside.burns() && data.fire_ticks == 0 {
        return;
    }
    let immune = fire_immune(data, world.types);
    let previous = data.fire_ticks;
    let was_on_fire = !immune && previous > 0;
    for step in inside.steps() {
        if let Some(damage) = step.fire {
            // `BaseFireBlock.fireIgnite`, then the block's fire damage.
            if !immune {
                if data.fire_ticks < 0 {
                    data.fire_ticks += 1;
                }
                if data.fire_ticks >= 0 {
                    data.fire_ticks = data.fire_ticks.max(160);
                }
            }
            hurt(data, world.types, world.types.in_fire, damage, burn);
        }
        if step.lava_blocks > 0 && !immune {
            data.fire_ticks = data.fire_ticks.max(300);
            let before = burn.hurts;
            hurt(data, world.types, world.types.lava, 4.0, burn);
            // `shouldPlayLavaHurtSound`: every tenth tick, and when the item is destroyed.
            if burn.hurts > before && (data.health <= 0 || data.tick_count.is_multiple_of(10)) {
                burn.lava_sound = Some(2.0 + data.next_float() * 0.4);
            }
        }
        if data.fire_ticks > 0 {
            // `runBefore(EXTINGUISH)` of powder snow and cauldrons: a burning item melts the
            // snow and lowers the cauldron.
            if let Some(snow) = step.melt_snow {
                burn.melt_snow.get_or_insert(snow);
            }
            if let Some(cauldron) = step.cauldron {
                burn.cauldron_fire.get_or_insert(cauldron);
            }
        }
        if step.extinguish && data.fire_ticks > 0 {
            // `EXTINGUISH`: `clearFire`.
            data.fire_ticks = 0;
        }
    }
    if data.fire_ticks > 0 {
        // `isInRain`: `clearFire`.
        let feet = BlockPos::new(floor(pos.x), floor(pos.y), floor(pos.z));
        let top = BlockPos::new(feet.x, floor(pos.y + 0.25), feet.z);
        if (world.raining_at)(feet) || (world.raining_at)(top) {
            data.fire_ticks = 0;
        }
    }
    if was_on_fire && data.fire_ticks <= 0 {
        let pitch = data.with_random(|r| 1.6 + (r.next_float() - r.next_float()) * 0.4);
        burn.extinguished = Some(pitch);
    }
    // Not burning and not ignited this tick: `-getFireImmuneTicks()`.
    if data.fire_ticks <= 0 && data.fire_ticks <= previous {
        data.fire_ticks = 0;
    }
}

/// `BubbleColumnBlock.entityInside`: the column pulls the item down or pushes it up, harder at
/// its top (`onAboveBubbleColumn`, whose particles the caller sends) than inside
/// (`onInsideBubbleColumn`, which resets the fall distance).
fn bubble_column(
    data: &mut ItemData,
    cell: &crate::survival::inside::Cell,
    world: &FireWorld<'_>,
    burn: &mut Burn,
    v: &mut Vec3,
) {
    let above = BlockPos::new(cell.pos.x, cell.pos.y + 1, cell.pos.z);
    let above = world
        .blocks
        .block_state(above)
        .unwrap_or(vanillite_data::block::BlockState::AIR);
    let (top, y) = crate::block::bubble::drag(above, cell.state, v.y);
    v.y = y;
    if top {
        if let Some(slot) = burn.bubble_tops.iter_mut().find(|t| t.is_none()) {
            *slot = Some(cell.pos);
        }
    } else {
        data.fall_distance = 0.0;
    }
}

/// `Entity.doWaterSplashEffect`: the sound and the `SPLASH` game event.
fn splash_effects(level: &mut crate::level::Level, id: EntityId, splash: Splash) {
    let pos = splash.pos;
    level.play_sound(
        None,
        pos.x,
        pos.y,
        pos.z,
        crate::block::sound_id("minecraft:entity.generic.splash"),
        crate::level::SOUND_NEUTRAL,
        splash.volume,
        splash.pitch,
    );
    level.game_event(
        crate::level::game_event::events().splash,
        pos,
        crate::level::game_event::GameEventContext::of(Some(id)),
    );
}

/// Tells the world what fire, lava and water did to the item entity `id` ([`Burn`]): the
/// splashes, the `ENTITY_DAMAGE` game events, the burn and extinguish sounds, and for a
/// destroyed item the contents it leaves (`Item.onDestroyed`) and its removal.
pub fn burn_effects(level: &mut crate::level::Level, id: EntityId, burn: Burn) {
    let Some(e) = level.entities.store.get(id) else {
        return;
    };
    let pos = e.pos();
    if let Some(snow) = burn.snowflake {
        use vanillite_base::random::RandomSource;
        let r = level.random();
        if r.next_boolean() {
            let vx = f64::from((r.next_float() * 2.0 - 1.0) * 0.083_333_336);
            let vz = f64::from((r.next_float() * 2.0 - 1.0) * 0.083_333_336);
            level.send_particles(
                crate::level::particles::simple("minecraft:snowflake"),
                Vec3::new(pos.x, f64::from(snow.y + 1), pos.z),
                0,
                Vec3::new(vx, f64::from(0.05_f32), vz),
                1.0,
            );
        }
    }
    if let Some(snow) = burn.melt_snow
        && level.settings.mob_griefing
    {
        level.destroy_block(snow, false, None);
    }
    if let Some(cauldron) = burn.cauldron_fire {
        crate::block::cauldron::burning_inside(level, cauldron);
    }
    // `entityInside` of pressure plates, tripwire and kin, in the order the item met them.
    for block in burn.inside.into_iter().flatten() {
        if !level.entities.store.contains(id) {
            return;
        }
        level.entity_inside(block, id);
    }
    for top in burn.bubble_tops.into_iter().flatten() {
        crate::block::bubble::send_particles(level, top);
    }
    if let Some(splash) = burn.splash_before {
        splash_effects(level, id, splash);
    }
    for _ in 0..burn.hurts {
        level.game_event(
            crate::level::game_event::events().entity_damage,
            pos,
            crate::level::game_event::GameEventContext::of(None),
        );
    }
    if let Some(pitch) = burn.lava_sound {
        level.play_sound(
            None,
            pos.x,
            pos.y,
            pos.z,
            crate::block::sound_id("minecraft:entity.generic.burn"),
            crate::level::SOUND_NEUTRAL,
            0.4,
            pitch,
        );
    }
    if let Some(pitch) = burn.extinguished {
        level.play_sound(
            None,
            pos.x,
            pos.y,
            pos.z,
            crate::block::sound_id("minecraft:entity.generic.extinguish_fire"),
            crate::level::SOUND_NEUTRAL,
            0.7,
            pitch,
        );
    }
    if let Some((at, on)) = burn.landed {
        let on_state = level.block(on);
        // `Block.fallOn`: only farmland draws for an item (`nextFloat() < fallDistance - 0.5`
        // comes before its `LivingEntity` check).
        if on_state.block() == vanillite_data::block::ids::FARMLAND {
            let _ = vanillite_base::random::RandomSource::next_float(level.random());
        }
        level.game_event(
            crate::level::game_event::events().hit_ground,
            at,
            crate::level::game_event::GameEventContext::with_state(Some(id), on_state),
        );
    }
    if burn.bounced {
        level.game_event(
            crate::level::game_event::events().bounce,
            pos,
            crate::level::game_event::GameEventContext::of(Some(id)),
        );
    }
    if let Some(on) = burn.step_on
        && level.entities.store.contains(id)
        && crate::block::contact::has_step_on(level.block(on).block())
    {
        crate::block::contact::step_on(level, on, id);
    }
    if let Some(splash) = burn.splash_after {
        splash_effects(level, id, splash);
    }
    if burn.destroyed {
        let contents = level
            .entities
            .item_mut(id)
            .map(|d| take_destroyed_contents(&mut d.stack))
            .unwrap_or_default();
        for stack in contents {
            crate::block::drops::spawn_item_moving(level, pos, stack, None, 0, None);
        }
        level.entities.despawn(id);
    }
}

/// `Item.onDestroyed` of a destroyed item entity's stack: a bundle (`BundleItem`) or a block
/// item with a `container` (`BlockItem`) is emptied; the stacks to drop.
pub fn take_destroyed_contents(stack: &mut ItemStack) -> Vec<ItemStack> {
    if let Some(items) = crate::bundle::items(stack) {
        crate::bundle::set_items(stack, &[]);
        return items;
    }
    if item::item_id(stack).block().is_none() {
        return Vec::new();
    }
    let Some(value) = stack
        .component(vanillite_protocol::components::ids::CONTAINER)
        .map(<[u8]>::to_vec)
    else {
        return Vec::new();
    };
    // `set(CONTAINER, EMPTY)`: the empty contents are the default.
    stack
        .patch_mut()
        .unset(vanillite_protocol::components::ids::CONTAINER);
    vanillite_protocol::item::decode_container_contents(&value)
        .map(|items| items.into_iter().filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// `ItemEntity.areMergable`.
pub fn are_mergable(a: &ItemStack, b: &ItemStack) -> bool {
    b.count.saturating_add(a.count) <= item::max_stack_size(b) && a.same_item_same_components(b)
}

/// `ItemEntity.merge(toStack, fromStack, 64)`: the new `to` stack; `from` shrinks.
pub fn merge_stacks(to: &ItemStack, from: &mut ItemStack) -> ItemStack {
    merge_stacks_max(to, from, 64)
}

/// `ItemEntity.merge(toStack, fromStack, maxCount)`.
pub fn merge_stacks_max(to: &ItemStack, from: &mut ItemStack, max: i32) -> ItemStack {
    let delta = (item::max_stack_size(to).min(max) - to.count).min(from.count);
    let new_to = to.with_count(to.count + delta);
    from.shrink(delta);
    new_to
}

/// The collision boxes of the blocks around `area` (`Level.getBlockCollisions`, without the
/// world border and entity shapes).
/// `bottom` is the entity's feet (its collision context, see
/// [`crate::mob::physics::context_shape`]).
fn block_boxes(
    area: &Aabb,
    bottom: f64,
    block: &dyn crate::mob::physics::Blocks,
    out: &mut Vec<Aabb>,
) {
    out.clear();
    let x0 = floor(area.min_x - 1.0e-7) - 1;
    let x1 = floor(area.max_x + 1.0e-7) + 1;
    let y0 = floor(area.min_y - 1.0e-7) - 1;
    let y1 = floor(area.max_y + 1.0e-7) + 1;
    let z0 = floor(area.min_z - 1.0e-7) - 1;
    let z1 = floor(area.max_z + 1.0e-7) + 1;
    for x in x0..=x1 {
        for z in z0..=z1 {
            for y in y0..=y1 {
                let pos = BlockPos::new(x, y, z);
                let state = block.state(pos);
                if state.is_air() {
                    continue;
                }
                let (dx, dy, dz) = (f64::from(x), f64::from(y), f64::from(z));
                if state.block() == vanillite_data::block::ids::MOVING_PISTON {
                    let mut moving = Vec::new();
                    block.moving_piston_boxes(pos, &mut moving);
                    for b in moving {
                        let shape = Aabb::new(
                            b[0] + dx,
                            b[1] + dy,
                            b[2] + dz,
                            b[3] + dx,
                            b[4] + dy,
                            b[5] + dz,
                        );
                        if shape.intersects(area) {
                            out.push(shape);
                        }
                    }
                    continue;
                }
                // `BlockCollisions`: the whole shape when it overlaps the area (touching is not
                // enough), so even its boxes beside the area make `Shapes.collide` drop a
                // movement below 1.0E-7.
                let start = out.len();
                let mut overlaps = false;
                let (ox, oz) = crate::mob::physics::collision_offset(state, x, z);
                for b in crate::mob::physics::context_shape(state, dy, Some(bottom)) {
                    let shape = Aabb::new(
                        (b[0] + ox) + dx,
                        b[1] + dy,
                        (b[2] + oz) + dz,
                        (b[3] + ox) + dx,
                        b[4] + dy,
                        (b[5] + oz) + dz,
                    );
                    overlaps |= shape.intersects(area);
                    out.push(shape);
                }
                if !overlaps {
                    out.truncate(start);
                }
            }
        }
    }
}

const EPS: f64 = 1.0e-7;

/// `Shapes.collide` along one axis (0 = x, 1 = y, 2 = z).
fn collide_axis(axis: usize, b: &Aabb, shapes: &[Aabb], mut delta: f64) -> f64 {
    let lo = |a: &Aabb, i: usize| [a.min_x, a.min_y, a.min_z][i];
    let hi = |a: &Aabb, i: usize| [a.max_x, a.max_y, a.max_z][i];
    for s in shapes {
        if delta.abs() < EPS {
            return 0.0;
        }
        let overlaps = (0..3)
            .filter(|&i| i != axis)
            .all(|i| hi(s, i) - EPS > lo(b, i) && lo(s, i) + EPS < hi(b, i));
        if !overlaps {
            continue;
        }
        if delta > 0.0 && lo(s, axis) >= hi(b, axis) - EPS {
            delta = delta.min(lo(s, axis) - hi(b, axis));
        } else if delta < 0.0 && hi(s, axis) <= lo(b, axis) + EPS {
            delta = delta.max(hi(s, axis) - lo(b, axis));
        }
    }
    delta
}

/// The result of `Entity.move`.
#[derive(Clone, Copy, Debug)]
pub struct Moved {
    /// The new position.
    pub pos: Vec3,
    /// The movement after collisions.
    pub movement: Vec3,
    /// Collided horizontally on x / z, vertically.
    pub hit_x: bool,
    pub hit_y: bool,
    pub hit_z: bool,
    /// The entity moved (`Entity.move` drops a tiny rest of a blocked movement).
    pub applied: bool,
}

/// `Entity.collide` + `move` for an entity of `half_width` and `height` without step-up.
pub fn move_with_collisions(
    pos: Vec3,
    half_width: f64,
    height: f64,
    movement: Vec3,
    block: &dyn crate::mob::physics::Blocks,
    scratch: &mut Vec<Aabb>,
) -> Moved {
    let mut b = Aabb::new(
        pos.x - half_width,
        pos.y,
        pos.z - half_width,
        pos.x + half_width,
        pos.y + height,
        pos.z + half_width,
    );
    let (mut x, mut y, mut z) = (movement.x, movement.y, movement.z);
    if x * x + y * y + z * z > 0.0 {
        block_boxes(&b.expand_towards(x, y, z), b.min_y, block, scratch);
        if y != 0.0 {
            y = collide_axis(1, &b, scratch, y);
            if y != 0.0 {
                b = b.offset(0.0, y, 0.0);
            }
        }
        let z_first = x.abs() < z.abs();
        if z_first && z != 0.0 {
            z = collide_axis(2, &b, scratch, z);
            if z != 0.0 {
                b = b.offset(0.0, 0.0, z);
            }
        }
        if x != 0.0 {
            x = collide_axis(0, &b, scratch, x);
            if !z_first && x != 0.0 {
                b = b.offset(x, 0.0, 0.0);
            }
        }
        if !z_first && z != 0.0 {
            z = collide_axis(2, &b, scratch, z);
        }
    }
    // `Entity.move`: a tiny remaining movement that collisions mostly ate is not applied.
    let length = x * x + y * y + z * z;
    let wanted = movement.x * movement.x + movement.y * movement.y + movement.z * movement.z;
    let applies = length > 1.0e-7 || wanted - length < 1.0e-7;
    // `Mth.equal`: within the float 1.0E-5F.
    let equal = |a: f64, b: f64| (b - a).abs() < f64::from(1.0e-5_f32);
    Moved {
        pos: if applies {
            Vec3::new(pos.x + x, pos.y + y, pos.z + z)
        } else {
            pos
        },
        movement: Vec3::new(x, y, z),
        hit_x: !equal(movement.x, x),
        hit_y: y != movement.y,
        hit_z: !equal(movement.z, z),
        applied: applies,
    }
}

/// Whether the box collides with any block (`noCollision` negated).
pub fn collides(
    b: &Aabb,
    block: &dyn crate::mob::physics::Blocks,
    scratch: &mut Vec<Aabb>,
) -> bool {
    block_boxes(b, b.min_y, block, scratch);
    scratch.iter().any(|s| s.intersects(b))
}

/// What one tick of an item entity did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ticked {
    /// New position.
    pub pos: Vec3,
    /// New velocity.
    pub velocity: Vec3,
    /// On ground after the move.
    pub on_ground: bool,
    /// The velocity changed noticeably (`needsSync`).
    pub needs_sync: bool,
    /// The entity crossed a block boundary (merges every 2 ticks instead of 40).
    pub moved_block: bool,
    /// The entity is gone (empty stack, lifetime over or below the world).
    pub discard: bool,
}

/// Inputs of one item entity tick.
#[derive(Clone, Copy, Debug)]
pub struct TickInput {
    pub pos: Vec3,
    pub velocity: Vec3,
    pub on_ground: bool,
    pub network_id: i32,
    /// Lowest block y of the world.
    pub min_y: i32,
    /// Ticks until an item despawns (`LIFETIME`, 6000 in Vanilla).
    pub lifetime: i16,
    /// The dimension's `fast_lava` attribute: lava pushes three times as hard.
    pub fast_lava: bool,
    /// `NoGravity`: `getGravity()` is 0.
    pub no_gravity: bool,
}

/// `Entity.getBlockSpeedFactor`: the block at the item, else (when that is neither water nor
/// a bubble column and has the factor 1) the block below that affects its movement.
fn block_speed_factor(block: &dyn crate::mob::physics::Blocks, pos: Vec3) -> f32 {
    let state = block.state(BlockPos::containing(pos.x, pos.y, pos.z));
    let here = state.block().info().speed_factor;
    let name = state.block().name();
    if here != 1.0 || name == "minecraft:water" || name == "minecraft:bubble_column" {
        return here;
    }
    let below = BlockPos::new(
        floor(pos.x),
        floor(pos.y - f64::from(0.500_001_f32)),
        floor(pos.z),
    );
    block.state(below).block().info().speed_factor
}

/// `getOnPosLegacy` of an item (the block 0.2 below its feet).
fn on_pos_legacy(pos: Vec3) -> BlockPos {
    BlockPos::new(
        floor(pos.x),
        floor(pos.y - f64::from(0.2_f32)),
        floor(pos.z),
    )
}

/// The bounding box of an item entity at `pos`.
fn body_at(pos: Vec3) -> Aabb {
    Aabb::new(
        pos.x - 0.125,
        pos.y,
        pos.z - 0.125,
        pos.x + 0.125,
        pos.y + 0.25,
        pos.z + 0.125,
    )
}

/// `ItemEntity.tick` without merging (the caller merges with the returned rate): the fluids
/// (`fluids` of a bounding box), fire and lava (into `burn`), the movement and the lifetime.
pub fn tick(
    data: &mut ItemData,
    input: TickInput,
    block: &dyn crate::mob::physics::Blocks,
    fluids: &dyn Fn(Aabb) -> Fluids,
    fire: &FireWorld<'_>,
    scratch: &mut Vec<Aabb>,
    burn: &mut Burn,
) -> Ticked {
    let mut out = Ticked {
        pos: input.pos,
        velocity: input.velocity,
        on_ground: input.on_ground,
        ..Ticked::default()
    };
    if data.stack.is_empty() || input.pos.y < f64::from(input.min_y) - 64.0 {
        out.discard = true;
        return out;
    }
    let first_tick = data.tick_count == 0;
    data.tick_count = data.tick_count.wrapping_add(1);
    let old = input.pos;
    let mut v = input.velocity;
    let half = 0.125;
    let height = 0.25;
    let body = body_at(old);
    // `Entity.baseTick`: the fluids, then the fire.
    let touched = fluids(body);
    let contact = touched.0;
    let (_, splash) =
        update_fluid_interaction(data, touched, first_tick, input.fast_lava, old, &mut v);
    burn.splash_before = splash;
    base_tick_fire(data, !first_tick && contact.in_lava(), fire, burn);
    if burn.destroyed {
        return out;
    }
    if data.pickup_delay > 0 && data.pickup_delay != NEVER_PICK_UP {
        data.pickup_delay -= 1;
    }
    let old_velocity = v;
    // `getFluidHeight(tag) > 0.1F`.
    let deep = f64::from(0.1_f32);
    if contact.water_height > deep {
        // `setUnderwaterMovement`: float constants widened to double.
        let m = f64::from(0.99_f32);
        let rise = if v.y < f64::from(0.06_f32) {
            f64::from(5.0e-4_f32)
        } else {
            0.0
        };
        v = Vec3::new(v.x * m, v.y + rise, v.z * m);
    } else if contact.lava_height > deep {
        // `setUnderLavaMovement`.
        let m = f64::from(0.95_f32);
        let rise = if v.y < f64::from(0.06_f32) {
            f64::from(5.0e-4_f32)
        } else {
            0.0
        };
        v = Vec3::new(v.x * m, v.y + rise, v.z * m);
    } else if !input.no_gravity {
        // `applyGravity`: `add(0, -0.04, 0)`, which turns Java's -0.0 into 0.0.
        v = Vec3::new(v.x + 0.0, v.y - 0.04, v.z + 0.0);
    }
    let no_physics = collides(&body.inflate(-1.0e-7, -1.0e-7, -1.0e-7), block, scratch);
    if no_physics {
        v = move_towards_closest_space(old, (body.min_y + body.max_y) / 2.0, v, block, &mut || {
            data.next_float()
        });
    }
    let horizontal = v.x * v.x + v.z * v.z;
    let skip_move = input.on_ground
        && horizontal <= f64::from(1.0e-5_f32)
        && !data
            .tick_count
            .wrapping_add(u32::from_ne_bytes(input.network_id.to_ne_bytes()))
            .is_multiple_of(4);
    if !skip_move {
        let mut delta = v;
        let moved = if no_physics {
            Moved {
                pos: Vec3::new(old.x + v.x, old.y + v.y, old.z + v.z),
                movement: v,
                hit_x: false,
                hit_y: false,
                hit_z: false,
                applied: true,
            }
        } else {
            if data.stuck.length_sqr() > 1.0e-7 {
                // `stuckSpeedMultiplier`: the slowed move, then the item stands still.
                delta = v.multiply(data.stuck.x, data.stuck.y, data.stuck.z);
                data.stuck = Vec3::ZERO;
                v = Vec3::ZERO;
            }
            let moved = move_with_collisions(old, half, height, delta, block, scratch);
            // `Entity.move` records its way; without one the item is walked where it is.
            data.last_movement = Some(if moved.applied {
                crate::survival::inside::Movement {
                    from: old,
                    to: moved.pos,
                    original: Some(delta),
                }
            } else {
                crate::survival::inside::Movement::still(old)
            });
            moved
        };
        out.pos = moved.pos;
        // `Entity.move` with `noPhysics` leaves the ground flag as it was.
        if !no_physics {
            out.on_ground = moved.hit_y && delta.y < 0.0;
        }
        // `checkFallDamage` (not in `noPhysics` moves).
        if !no_physics {
            if !data.in_water && moved.movement.y < 0.0 {
                data.fall_distance -= moved.movement.y;
            }
            if out.on_ground {
                if data.fall_distance > 0.0 {
                    burn.landed = Some((out.pos, on_pos_legacy(out.pos)));
                }
                data.fall_distance = 0.0;
            }
        }
        // `restituteMovementAfterCollisions` without bounciness of its own: `-v * 0.0` keeps
        // Java's signed zeros; landing fast on a bouncy block (slime, beds, shelf mushrooms)
        // bounces it with 0.8 of the block's restitution.
        if moved.hit_x {
            v.x = -v.x * 0.0;
        }
        if moved.hit_z {
            v.z = -v.z * 0.0;
        }
        if moved.hit_y {
            let gravity = 0.04;
            let effect = block.state(BlockPos::containing(
                moved.pos.x,
                moved.pos.y - f64::from(0.2_f32),
                moved.pos.z,
            ));
            let restitution = if delta.y < 0.0
                && -v.y > gravity
                && !crate::block::block_tags()
                    .suppresses_bounce
                    .contains(effect)
            {
                crate::block::tnt::block_bounciness(effect.block())
            } else {
                0.0
            };
            v.y = if restitution > 0.0 {
                let portion = moved.movement.y / v.y;
                let drag = 1.0 + portion * (f64::from(0.98_f32) - 1.0);
                burn.bounced = true;
                (portion * gravity - v.y) * drag * restitution
            } else {
                // `(0 - y) * 1 * 0`: a resting 0.0 stays positive.
                (0.0 - v.y) * 0.0
            };
        }
        // `applyEffectsFromBlocks`: `SlimeBlock.stepOn` slows a resting item.
        if !no_physics
            && out.on_ground
            && block.state(on_pos_legacy(out.pos)).block()
                == vanillite_data::block::ids::SLIME_BLOCK
        {
            v = crate::block::contact::slime_step_on(v);
        }
        if !no_physics && let Some(movement) = data.last_movement {
            inside_blocks_fire(data, movement, out.pos, fire, burn, &mut v);
        }
        // `Entity.move`: `getBlockSpeedFactor` (soul sand, honey) slows the horizontal motion.
        if !no_physics {
            let factor = f64::from(block_speed_factor(block, out.pos));
            v = Vec3::new(v.x * factor, v.y, v.z * factor);
        }
        // `getAirDrag()` and the ground friction are floats, widened to double.
        let air_drag = 0.98_f32;
        let mut ground = air_drag;
        if out.on_ground {
            let below = block.state(BlockPos::new(
                floor(out.pos.x),
                floor(out.pos.y - 0.999_999),
                floor(out.pos.z),
            ));
            ground = air_drag * below.block().info().friction;
        }
        let (ground, air_drag) = (f64::from(ground), f64::from(air_drag));
        v = Vec3::new(v.x * ground, v.y * air_drag, v.z * ground);
        if out.on_ground && v.y < 0.0 {
            v.y *= -0.5;
        }
    } else if !no_physics {
        // `applyEffectsFromBlocksForLastMovements`: `SlimeBlock.stepOn` of a resting item, then
        // the blocks on the way of its last move.
        if out.on_ground
            && block.state(on_pos_legacy(out.pos)).block()
                == vanillite_data::block::ids::SLIME_BLOCK
        {
            v = crate::block::contact::slime_step_on(v);
        }
        if let Some(movement) = data.last_movement {
            inside_blocks_fire(data, movement, out.pos, fire, burn, &mut v);
        }
    }
    // `applyEffectsFromBlocks` on the way of the move, for a resting item on the way of its
    // last one (`isAffectedByBlocks`): `stepOn` of the block below, then `entityInside`.
    if !no_physics && out.on_ground {
        let on = on_pos_legacy(out.pos);
        if crate::block::contact::has_step_on(block.state(on).block()) {
            burn.step_on = Some(on);
        }
    }
    out.moved_block = floor(old.x) != floor(out.pos.x)
        || floor(old.y) != floor(out.pos.y)
        || floor(old.z) != floor(out.pos.z);
    // The second `updateFluidInteraction`, where the item is now.
    let touched = if out.pos == old {
        touched
    } else {
        fluids(body_at(out.pos))
    };
    let (in_fluid, splash) =
        update_fluid_interaction(data, touched, false, input.fast_lava, out.pos, &mut v);
    burn.splash_after = splash;
    out.velocity = v;
    let dx = v.x - old_velocity.x;
    let dy = v.y - old_velocity.y;
    let dz = v.z - old_velocity.z;
    out.needs_sync = in_fluid || dx * dx + dy * dy + dz * dz > 0.01;
    out
}

/// The end of `ItemEntity.tick`, after `mergeWithNeighbours`: `age++` (not for an unlimited
/// lifetime), then whether it reached `lifetime` and is discarded.
pub fn age_tick(data: &mut ItemData, lifetime: i16) -> bool {
    if data.age != UNLIMITED_LIFETIME {
        data.age = data.age.saturating_add(1);
    }
    data.age >= lifetime && data.age != UNLIMITED_LIFETIME
}

/// `state.isCollisionShapeFullBlock(level, pos)`: a moving piston's shape comes from its block
/// entity (it has a dynamic shape), so it is full only where the moved block stands whole.
fn collision_full_block(block: &dyn crate::mob::physics::Blocks, pos: BlockPos) -> bool {
    let state = block.state(pos);
    if state.block() != vanillite_data::block::ids::MOVING_PISTON {
        return state.has(block_flags::COLLISION_FULL_BLOCK);
    }
    let mut boxes = Vec::new();
    block.moving_piston_boxes(pos, &mut boxes);
    // `Block.isShapeFullBlock`: the union is exactly the unit cube.
    if boxes
        .iter()
        .any(|b| b[0] < 0.0 || b[1] < 0.0 || b[2] < 0.0 || b[3] > 1.0 || b[4] > 1.0 || b[5] > 1.0)
    {
        return false;
    }
    let cuts = |axis: usize| {
        let mut c = vec![0.0, 1.0];
        for b in &boxes {
            c.push(b[axis]);
            c.push(b[axis + 3]);
        }
        c.sort_by(f64::total_cmp);
        c.dedup();
        c
    };
    let (xs, ys, zs) = (cuts(0), cuts(1), cuts(2));
    xs.windows(2).all(|x| {
        ys.windows(2).all(|y| {
            zs.windows(2).all(|z| {
                let p = [
                    (x[0] + x[1]) / 2.0,
                    (y[0] + y[1]) / 2.0,
                    (z[0] + z[1]) / 2.0,
                ];
                boxes
                    .iter()
                    .any(|b| (0..3).all(|a| b[a] < p[a] && p[a] < b[a + 3]))
            })
        })
    })
}

/// `Entity.moveTowardsClosestSpace`: pushes an entity stuck in a block out of it.
pub(crate) fn move_towards_closest_space(
    pos: Vec3,
    y: f64,
    velocity: Vec3,
    block: &dyn crate::mob::physics::Blocks,
    random_float: &mut dyn FnMut() -> f32,
) -> Vec3 {
    let base = BlockPos::new(floor(pos.x), floor(y), floor(pos.z));
    let delta = [
        pos.x - f64::from(base.x),
        y - f64::from(base.y),
        pos.z - f64::from(base.z),
    ];
    // North, south, west, east, up: (axis, positive).
    let dirs: [(usize, bool, [i32; 3]); 5] = [
        (2, false, [0, 0, -1]),
        (2, true, [0, 0, 1]),
        (0, false, [-1, 0, 0]),
        (0, true, [1, 0, 0]),
        (1, true, [0, 1, 0]),
    ];
    let mut best = (1, true);
    let mut closest = f64::MAX;
    for (axis, positive, [dx, dy, dz]) in dirs {
        let n = BlockPos::new(
            base.x.wrapping_add(dx),
            base.y.wrapping_add(dy),
            base.z.wrapping_add(dz),
        );
        if !collision_full_block(block, n) {
            let axis_delta = delta.get(axis).copied().unwrap_or(0.0);
            let distance = if positive {
                1.0 - axis_delta
            } else {
                axis_delta
            };
            if distance < closest {
                closest = distance;
                best = (axis, positive);
            }
        }
    }
    let speed = f64::from(random_float() * 0.2 + 0.1);
    let step = if best.1 { 1.0 } else { -1.0 };
    let v = Vec3::new(velocity.x * 0.75, velocity.y * 0.75, velocity.z * 0.75);
    match best.0 {
        0 => Vec3::new(step * speed, v.y, v.z),
        1 => Vec3::new(v.x, step * speed, v.z),
        _ => Vec3::new(v.x, v.y, step * speed),
    }
}
