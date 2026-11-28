//! Falling blocks (`FallingBlock`, `ConcretePowderBlock`, `AnvilBlock`, `BrushableBlock`) and
//! the `falling_block` entity (`FallingBlockEntity`).
//!
//! A falling block schedules a tick when it is placed or a neighbour changes; the tick turns it
//! into a `falling_block` entity if the block below is free. The entity falls with gravity
//! 0.04 and drag 0.98 and becomes a block again where it lands, or drops as an item where it
//! cannot.

use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::{BlockId, BlockState, ids};
use vanillite_data::entity::ids as entity_ids;
use vanillite_world::entity::{EntityFlags, EntityId, NewEntity};
use vanillite_worldgen::behaviour::{self as shared, Class, class_of};
use vanillite_worldgen::behaviour::{PosExt, StateExt};

use super::{can_be_replaced, drops, fluid, is_fire};
use crate::entities::FallingBlockData;
use crate::level::Level;
use crate::level::view::LevelView;

/// `LevelEvent.SOUND_ANVIL_LAND`.
const ANVIL_LAND: i32 = 1031;
/// `LevelEvent.SOUND_ANVIL_BROKEN`.
const ANVIL_BROKEN: i32 = 1029;

/// `getDelayAfterPlace()`.
fn delay_after_place(state: BlockState) -> i32 {
    if state.is(ids::DRAGON_EGG) { 5 } else { 2 }
}

/// `FallingBlock.onPlace`.
pub fn on_place(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    _old: BlockState,
    _moved: bool,
) {
    level.schedule_block_tick(pos, state.block(), delay_after_place(state));
}

/// `FallingBlock.tick`: falls if the block below is free.
pub fn tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    if is_free(level.block(pos.below())) && pos.y >= level.min_y() {
        fall(level, pos, state, true);
    }
}

/// `BrushableBlock.onPlace`.
pub fn brushable_on_place(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    _old: BlockState,
    _moved: bool,
) {
    level.schedule_block_tick(pos, state.block(), 2);
}

/// `BrushableBlock.tick`: falls and breaks where it lands (`disableDrop`: the entity is
/// removed instead of placing the block; it still drops its item when it times out).
pub fn brushable_tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    super::brushable::check_reset(level, pos);
    if is_free(level.block(pos.below())) && pos.y >= level.min_y() {
        let id = fall(level, pos, state, true);
        if let Some(d) = level.entities.falling_block_mut(id) {
            d.cancel_drop = true;
        }
    }
}

/// `ScaffoldingBlock.onPlace`: the distance is checked on the next tick.
pub fn scaffolding_on_place(
    level: &mut Level,
    _state: BlockState,
    pos: BlockPos,
    _old: BlockState,
    _moved: bool,
) {
    level.schedule_block_tick(pos, ids::SCAFFOLDING, 1);
}

/// `ScaffoldingBlock.tick`: takes its distance to support; without support it falls (if it
/// had none before) or breaks.
pub fn scaffolding_tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    let distance = vanillite_worldgen::behaviour::scaffolding_distance(level, pos);
    let bottom = distance > 0 && !level.block(pos.below()).is(ids::SCAFFOLDING);
    let new = state
        .set_int("distance", distance)
        .set_flag("bottom", bottom);
    if distance == 7 {
        if state.int("distance") == Some(7) {
            fall(level, pos, new, true);
        } else {
            level.destroy_block(pos, true, None);
        }
    } else if state != new {
        level.set_block_and_update(pos, new);
    }
}

/// `FallingBlock.isFree(state)`.
pub fn is_free(state: BlockState) -> bool {
    state.is_air()
        || is_fire(state)
        || state.has(vanillite_data::block::flags::LIQUID)
        || can_be_replaced(state)
}

/// `FallingBlockEntity.fall(level, pos, state)`: the block becomes its fluid and an entity
/// takes its place.
pub fn fall(level: &mut Level, pos: BlockPos, state: BlockState, drop_item: bool) -> EntityId {
    let falling = state.with("waterlogged", "false").unwrap_or(state);
    let below = state.fluid_state().block_state();
    let spawn_pos = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    );
    level.set_block_and_update(pos, below);
    // The entity takes its own random (not drawn from at creation) with its UUID.
    let (uuid, seed) = level.new_uuid_and_seed();
    let network_id = level.network_ids().next();
    let new = NewEntity::new(entity_ids::FALLING_BLOCK, network_id, uuid, spawn_pos);
    let mut data = FallingBlockData::new(falling, drop_item, pos, seed);
    if is_anvil(falling.block()) {
        // `AnvilBlock.falling`: `setHurtsEntities(2.0F, 40)`.
        data.hurt_entities = true;
        data.fall_hurt_amount = 2.0;
    }
    let id = level.entities.spawn_falling_block(new, data);
    level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0);
    id
}

/// `state.is(BlockTags.ANVIL)`.
pub fn is_anvil(block: BlockId) -> bool {
    block == ids::ANVIL || block == ids::CHIPPED_ANVIL || block == ids::DAMAGED_ANVIL
}

/// `FallingBlockEntity.setHurtsEntities(damagePerDistance, max)`.
pub fn set_hurts_entities(level: &mut Level, id: EntityId, per_distance: f32, max: i32) {
    if let Some(d) = level.entities.falling_block_mut(id) {
        d.hurt_entities = true;
        d.fall_hurt_amount = per_distance;
        d.fall_hurt_max = max;
    }
}

/// What a falling block's damage is (`Fallable.getFallDamageSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallingDamage {
    /// `falling_block`.
    Block,
    /// `falling_anvil`.
    Anvil,
    /// `falling_stalactite`.
    Stalactite,
}

/// A living entity a landing falling block hurts, for the mob system to apply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FallingHurt {
    pub target: EntityId,
    /// The falling block (the direct and causing entity) and where it was.
    pub source: EntityId,
    pub pos: Vec3,
    pub kind: FallingDamage,
    pub amount: f32,
}

fn aabb_center_bottom(aabb: &Aabb) -> Vec3 {
    Vec3::new(
        f64::midpoint(aabb.min_x, aabb.max_x),
        aabb.min_y,
        f64::midpoint(aabb.min_z, aabb.max_z),
    )
}

/// Applies the damage of landing falling blocks (`entity.hurt(source, damage)` with
/// `damageSources().anvil(this)` and the like).
pub fn apply_falling_hurts(
    level: &Level,
    ents: &mut crate::entities::Entities,
    env: &mut crate::survival::player::Env<'_>,
    cx: &mut crate::mob::MobContext,
    hurts: Vec<FallingHurt>,
) {
    for hurt in hurts {
        let kind = match hurt.kind {
            FallingDamage::Block => env.types.falling_block,
            FallingDamage::Anvil => env.types.falling_anvil,
            FallingDamage::Stalactite => env.types.falling_stalactite,
        };
        let mut source = crate::survival::damage::DamageSource::of(kind);
        source.cause = Some(hurt.source);
        source.direct = Some(hurt.source);
        source.position = Some(hurt.pos);
        crate::mob::combat::hurt(level, ents, env, cx, hurt.target, source, hurt.amount);
    }
}

/// `Block.fallOn` of the block a falling block lands on: the fall distance its
/// `causeFallDamage` sees, `None` for blocks that never call it (powder snow).
fn falling_block_fall_on(level: &mut Level, on: BlockState, distance: f64) -> Option<f64> {
    use crate::survival::blocks::{Landing, bounce_restitution};
    let landing = match on.block() {
        b if b == ids::HAY_BLOCK || b == ids::SLIME_BLOCK || b == ids::HONEY_BLOCK => {
            Landing::Fall(1.0)
        }
        b if b == ids::FARMLAND => Landing::Farmland,
        b if b == ids::POWDER_SNOW => Landing::PowderSnow,
        b if b == ids::POINTED_DRIPSTONE
            && on.value("vertical_direction") == Some("up")
            && on.value("thickness") == Some("tip") =>
        {
            Landing::Stalagmite
        }
        b if bounce_restitution(b) > 0.0 => Landing::Bed,
        _ => Landing::Fall(1.0),
    };
    match landing {
        Landing::PowderSnow => None,
        Landing::Stalagmite => Some(distance + 2.5),
        Landing::Bed => Some(distance * f64::from(1.0_f32 - 0.5)),
        Landing::Farmland => {
            // `level.getRandom().nextFloat() < fallDistance - 0.5` runs for every entity.
            let _ = vanillite_base::random::RandomSource::next_float(level.random());
            Some(distance)
        }
        _ => Some(distance),
    }
}

/// `FallingBlockEntity.causeFallDamage`: anvils and stalactite tips hurt the living entities
/// in their box; an anvil may chip (or break: `cancelDrop`).
fn falling_block_fall_damage(level: &mut Level, id: EntityId, distance: f64) {
    let Some(data) = level.entities.falling_block(id).copied() else {
        return;
    };
    if !data.hurt_entities {
        return;
    }
    #[allow(clippy::cast_possible_truncation)]
    let n = (distance - 1.0).ceil() as i32;
    if n < 0 {
        return;
    }
    let block = data.state.block();
    let kind = if is_anvil(block) {
        FallingDamage::Anvil
    } else if block == ids::POINTED_DRIPSTONE {
        FallingDamage::Stalactite
    } else {
        FallingDamage::Block
    };
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let damage = ((n as f32 * data.fall_hurt_amount).floor() as i32).min(data.fall_hurt_max) as f32;
    let Some(aabb) = level.entities.store.get(id).map(|e| e.aabb()) else {
        return;
    };
    let mut targets = Vec::new();
    let ents = &level.entities;
    ents.store.for_each_in(&aabb, false, |e| {
        if e == id {
            return;
        }
        // `NO_CREATIVE_OR_SPECTATOR` and `LIVING_ENTITY_STILL_ALIVE`.
        let alive = if let Some(p) = ents.player(e) {
            !matches!(
                p.game_mode,
                vanillite_config::GameMode::Creative | vanillite_config::GameMode::Spectator
            ) && p.survival.health > 0.0
        } else {
            ents.mob(e).is_some_and(crate::mob::MobData::is_alive)
        };
        if alive {
            targets.push(e);
        }
    });
    for target in targets {
        level.output().falling_hurts.push(FallingHurt {
            target,
            source: id,
            pos: aabb_center_bottom(&aabb),
            kind,
            amount: damage,
        });
    }
    if is_anvil(block) && damage > 0.0 {
        // The entity's own random: its first draw.
        let mut random = vanillite_base::random::LegacyRandom::new(data.seed);
        #[allow(clippy::cast_precision_loss)]
        let chance = 0.05 + n as f32 * 0.05;
        if vanillite_base::random::RandomSource::next_float(&mut random) < chance
            && let Some(d) = level.entities.falling_block_mut(id)
        {
            // `AnvilBlock.damage`.
            let next = if block == ids::ANVIL {
                Some(ids::CHIPPED_ANVIL)
            } else if block == ids::CHIPPED_ANVIL {
                Some(ids::DAMAGED_ANVIL)
            } else {
                None
            };
            match next {
                Some(next) => {
                    let facing = d.state.value("facing").unwrap_or("north");
                    d.state = next
                        .default_state()
                        .with("facing", facing)
                        .unwrap_or_else(|| next.default_state());
                }
                None => d.cancel_drop = true,
            }
        }
    }
}

/// `FallingBlockEntity.tick`.
pub fn tick_entity(level: &mut Level, id: EntityId) {
    if let Some(resume) = tick_until_hurt(level, id) {
        resume_tick(level, id, resume);
    }
}

/// Where [`tick_until_hurt`] stopped: the rest of the tick after the entities it hurt took
/// their damage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resume {
    velocity: Vec3,
    /// The movement `move` was asked for (the velocity, slowed by a cobweb or powder snow).
    delta: Vec3,
    moved: Vec3,
    new_pos: Vec3,
    on_pos: BlockPos,
    time: i32,
}

/// `FallingBlockEntity.tick` up to the landing damage: returns `Some` when the falling block
/// hurt entities ([`crate::level::LevelOutput::falling_hurts`]); the caller applies the damage
/// (deaths drop their loot before the block lands) and finishes with [`resume_tick`].
pub fn tick_until_hurt(level: &mut Level, id: EntityId) -> Option<Resume> {
    let resume = tick_move(level, id)?;
    if level.output().falling_hurts.is_empty() {
        resume_tick(level, id, resume);
        return None;
    }
    Some(resume)
}

fn tick_move(level: &mut Level, id: EntityId) -> Option<Resume> {
    let data = level.entities.falling_block(id).copied()?;
    if data.state.is_air() {
        level.entities.despawn(id);
        return None;
    }
    let time = data.time + 1;
    if let Some(d) = level.entities.falling_block_mut(id) {
        d.time = time;
    }
    let entity = level.entities.store.get(id)?;
    let (pos, aabb) = (entity.pos(), entity.aabb());
    // `applyGravity` and `move(SELF, deltaMovement)`.
    // Added as a vector: a -0.0 of x or z becomes 0.0 like in Java.
    let mut velocity = entity.velocity() + Vec3::new(0.0, -0.04, 0.0);
    let mut delta = velocity;
    if data.stuck.length_sqr() > 1.0e-7 {
        // `stuckSpeedMultiplier`: the slowed move, then the block stands still.
        delta = velocity.multiply(data.stuck.x, data.stuck.y, data.stuck.z);
        velocity = Vec3::ZERO;
        if let Some(d) = level.entities.falling_block_mut(id) {
            d.stuck = Vec3::ZERO;
        }
    }
    // `PowderSnowBlock.getCollisionShape`: a falling block lands on powder snow, on the lower
    // falling shape after a fall of more than 2.5 blocks.
    let snow_top = if data.fall_distance > 2.5 {
        0.899_999_976_158_142_1
    } else {
        1.0
    };
    let moved = collide_with_snow(&level.view(), aabb, delta, Some(snow_top));
    let vertical = moved.y != delta.y;
    let on_ground = vertical && delta.y < 0.0;
    let new_pos = Vec3::new(pos.x + moved.x, pos.y + moved.y, pos.z + moved.z);
    if let Some(mut entity) = level.entities.store.get_mut(id) {
        entity.set_pos(new_pos);
        entity.set_flags(EntityFlags::ON_GROUND, on_ground);
    }
    // `Entity.checkFallDamage`: falling blocks never update their water state.
    let on_pos = BlockPos::containing(new_pos.x, new_pos.y - f64::from(0.2_f32), new_pos.z);
    if moved.y < 0.0 {
        #[allow(clippy::cast_possible_truncation)]
        let dy = f64::from(moved.y as f32);
        if let Some(d) = level.entities.falling_block_mut(id) {
            d.fall_distance -= dy;
        }
    }
    if on_ground {
        let distance = level
            .entities
            .falling_block(id)
            .map_or(0.0, |d| d.fall_distance);
        if distance > 0.0 {
            let on_state = level.block(on_pos);
            if let Some(distance) = falling_block_fall_on(level, on_state, distance) {
                falling_block_fall_damage(level, id, distance);
            }
            level.game_event(
                crate::level::game_event::events().hit_ground,
                new_pos,
                crate::level::game_event::GameEventContext {
                    source: Some(id),
                    affected: Some(on_state),
                    owner: None,
                },
            );
        }
        if let Some(d) = level.entities.falling_block_mut(id) {
            d.fall_distance = 0.0;
        }
    }
    Some(Resume {
        velocity,
        delta,
        moved,
        new_pos,
        on_pos,
        time,
    })
}

/// The rest of `FallingBlockEntity.tick` after [`tick_until_hurt`].
pub fn resume_tick(level: &mut Level, id: EntityId, r: Resume) {
    let Resume {
        mut velocity,
        delta,
        moved,
        new_pos,
        on_pos,
        time,
    } = r;
    let vertical = moved.y != delta.y;
    // `Mth.equal`: within the float 1.0E-5F.
    let equal = |a: f64, b: f64| (b - a).abs() < f64::from(1.0e-5_f32);
    let (x_collision, z_collision) = (!equal(delta.x, moved.x), !equal(delta.z, moved.z));
    let horizontal = x_collision || z_collision;
    let on_ground = vertical && delta.y < 0.0;
    // `restituteMovementAfterCollisions` without bounciness: `-v * 0.0` keeps the sign bit.
    if x_collision {
        velocity.x = -velocity.x * 0.0;
    }
    if z_collision {
        velocity.z = -velocity.z * 0.0;
    }
    if vertical {
        // `Block.updateEntityMovementAfterFallOn`.
        velocity.y = 0.0;
    }
    // `applyEffectsFromBlocks`: `stepOn` of the block below (redstone ore lights up), then
    // `entityInside` of the plates, tripwire and kin on the way.
    if on_ground {
        let block = level.block(on_pos).block();
        if block == ids::SLIME_BLOCK {
            // `SlimeBlock.stepOn`.
            let dy = velocity.y.abs();
            if dy < 0.1 {
                let factor = 0.4 + dy * 0.2;
                velocity = Vec3::new(velocity.x * factor, velocity.y, velocity.z * factor);
            }
        } else if super::contact::has_step_on(block) {
            super::contact::step_on(level, on_pos, id);
        }
    }
    if let Some(dims) = level.entities.store.get(id).map(|e| e.dims()) {
        let way = [crate::survival::inside::Movement::plain(
            new_pos - moved,
            new_pos,
        )];
        velocity = inside_blocks(level, id, dims, &way, velocity);
        if !level.entities.store.contains(id) {
            return;
        }
    }
    let Some(data) = level.entities.falling_block(id).copied() else {
        return;
    };
    let mut block_pos = BlockPos::containing(new_pos.x, new_pos.y, new_pos.z);
    let block = data.state.block();
    let concrete = class_of(data.state) == Class::ConcretePowder;
    // `isStuckInWater`.
    let mut stuck_in_water = concrete && fluid::is_water(level.fluid(block_pos));
    if concrete && velocity.length_sqr() > 1.0 {
        // Fast concrete powder solidifies at the first water source it passed.
        let from = new_pos - moved;
        if let Some((hit, _)) =
            crate::mob::physics::clip_source_fluids(&level.view(), from, new_pos)
            && fluid::is_water(level.fluid(hit))
        {
            block_pos = hit;
            stuck_in_water = true;
        }
    }
    let mut removed = false;
    if !on_ground && !stuck_in_water {
        if time > 100 && (block_pos.y <= level.min_y() || block_pos.y > level.max_y()) || time > 600
        {
            if data.drop_item && level.settings.entity_drops {
                drops::spawn_block_item(level, new_pos, block);
            }
            level.entities.despawn(id);
            removed = true;
        }
    } else {
        let current = level.block(block_pos);
        velocity = Vec3::new(velocity.x * 0.7, velocity.y * -0.5, velocity.z * 0.7);
        if current.is(ids::MOVING_PISTON) {
            // A block moved by a piston: the entity stays for another tick.
        } else if data.cancel_drop {
            level.entities.despawn(id);
            removed = true;
            on_broken_after_fall(level, data.state, block_pos, new_pos, id);
        } else {
            let may_replace = can_be_replaced_by_falling(current);
            let would_continue =
                is_free(level.block(block_pos.below())) && (!concrete || !stuck_in_water);
            let would_survive = level.can_survive(data.state, block_pos) && !would_continue;
            if may_replace && would_survive {
                let mut state = data.state;
                if level.fluid(block_pos).info().fluid == fluid::WATER
                    && let Some(waterlogged) = state.with("waterlogged", "true")
                {
                    state = waterlogged;
                }
                if level.set_block_and_update(block_pos, state) {
                    landed_packets(level, id, block_pos);
                    level.entities.despawn(id);
                    removed = true;
                    on_land(level, block_pos, state, current);
                } else if data.drop_item && level.settings.entity_drops {
                    // A block that could not be set stays an entity unless it drops.
                    level.entities.despawn(id);
                    removed = true;
                    on_broken_after_fall(level, data.state, block_pos, new_pos, id);
                    drops::spawn_block_item(level, new_pos, block);
                }
            } else {
                level.entities.despawn(id);
                removed = true;
                if data.drop_item && level.settings.entity_drops {
                    on_broken_after_fall(level, data.state, block_pos, new_pos, id);
                    drops::spawn_block_item(level, new_pos, block);
                }
            }
        }
    }
    if !removed && let Some(mut entity) = level.entities.store.get_mut(id) {
        // `scale(getAirDrag())`: the float 0.98F widened to double.
        let drag = f64::from(0.98_f32);
        entity.set_velocity(Vec3::new(
            velocity.x * drag,
            velocity.y * drag,
            velocity.z * drag,
        ));
        entity.set_flags(EntityFlags::PRECISE_POSITION, horizontal || vertical);
    }
}

/// `applyEffectsFromBlocks` of a falling block on `way`: cobwebs and powder snow slow its
/// next move (`makeStuckInBlock`, resetting the fall distance), bubble columns drag it, and
/// the blocks with an `entityInside` of their own (plates, tripwire, frogspawn) react, in the
/// order of the walk. Returns the velocity.
fn inside_blocks(
    level: &mut Level,
    id: EntityId,
    dims: vanillite_world::entity::Dimensions,
    way: &[crate::survival::inside::Movement],
    mut velocity: Vec3,
) -> Vec3 {
    enum Hit {
        Stuck(Vec3),
        Bubble(BlockPos),
        Hook(BlockPos),
    }
    let mut hits = Vec::new();
    {
        let view = level.view();
        let walker = crate::survival::inside::Walker {
            blocks: &view,
            facts: None,
            dims,
            fast_fall: false,
        };
        walker.walk(way, |cell| {
            let block = cell.state.block();
            if block == ids::BUBBLE_COLUMN {
                if cell.precise {
                    hits.push(Hit::Bubble(cell.pos));
                }
            } else if !cell.in_block {
            } else if block == ids::COBWEB {
                hits.push(Hit::Stuck(Vec3::new(0.25, f64::from(0.05_f32), 0.25)));
            } else if block == ids::POWDER_SNOW {
                hits.push(Hit::Stuck(Vec3::new(
                    f64::from(0.9_f32),
                    1.5,
                    f64::from(0.9_f32),
                )));
            } else if level.has_entity_inside(cell.state) {
                hits.push(Hit::Hook(cell.pos));
            }
            true
        });
    }
    for hit in hits {
        if !level.entities.store.contains(id) {
            break;
        }
        match hit {
            Hit::Stuck(multiplier) => {
                if let Some(d) = level.entities.falling_block_mut(id) {
                    d.fall_distance = 0.0;
                    d.stuck = multiplier;
                }
            }
            Hit::Bubble(pos) => {
                let (v, reset) = crate::level::particles::bubble_column_drag(level, pos, velocity);
                velocity = v;
                if reset && let Some(d) = level.entities.falling_block_mut(id) {
                    d.fall_distance = 0.0;
                }
            }
            Hit::Hook(pos) => level.entity_inside(pos, id),
        }
    }
    velocity
}

/// `FrogspawnBlock.entityInside`: a falling block destroys the frogspawn.
pub fn frogspawn_entity_inside(level: &mut Level, _: BlockState, pos: BlockPos, entity: EntityId) {
    if level.entities.falling_block(entity).is_some() {
        level.destroy_block(pos, false, None);
    }
}

/// `currentState.canBeReplaced(new DirectionalPlaceContext(..., ItemStack.EMPTY, ...))`.
fn can_be_replaced_by_falling(state: BlockState) -> bool {
    if state.is(ids::SNOW) {
        // `SnowLayerBlock.canBeReplaced`: without the snow item only a single layer.
        return state.value("layers") == Some("1");
    }
    can_be_replaced(state)
}

/// `Fallable.onLand`.
fn on_land(level: &mut Level, pos: BlockPos, state: BlockState, replaced: BlockState) {
    match class_of(state) {
        Class::Anvil => level.level_event(None, ANVIL_LAND, pos, 0),
        Class::ConcretePowder => {
            // `shouldSolidify`.
            if (fluid::is_water(replaced.fluid_state()) || shared::touches_liquid(level, pos))
                && let Some(concrete) = shared::concrete_of(state)
            {
                super::hook_events::set_block_and_update(
                    level,
                    vanillite_abi::event::BLOCK_FORM,
                    pos,
                    concrete.default_state(),
                );
            }
        }
        _ => {}
    }
}

/// `Fallable.onBrokenAfterFall` of the block `state` that fell as `entity` (at `entity_pos`):
/// anvils and stalactites sound, suspicious sand and gravel break.
fn on_broken_after_fall(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    entity_pos: Vec3,
    entity: EntityId,
) {
    match class_of(state) {
        Class::Anvil => level.level_event(None, ANVIL_BROKEN, pos, 0),
        Class::Speleothem => {
            // `getStalactiteLandingSound`.
            let event = if state.is(ids::SULFUR_SPIKE) {
                1052
            } else {
                1045
            };
            level.level_event(None, event, pos, 0);
        }
        Class::Brushable => {
            // The centre of the entity's box (0.98 high).
            let centre = Vec3::new(
                entity_pos.x,
                entity_pos.y + f64::from(0.98_f32) / 2.0,
                entity_pos.z,
            );
            let at = BlockPos::containing(centre.x, centre.y, centre.z);
            level.level_event(None, 2001, at, i32::from(state.0));
            level.game_event(
                crate::level::game_event::events().block_destroy,
                centre,
                crate::level::game_event::GameEventContext::of(Some(entity)),
            );
        }
        _ => {}
    }
}

/// `Entity.collide` against block collision shapes: the movement left after the boxes in the
/// way, Y first, then X and Z (larger first).
pub(crate) fn collide(level: &LevelView<'_>, aabb: Aabb, movement: Vec3) -> Vec3 {
    collide_with_snow(level, aabb, movement, None)
}

/// [`collide`] where powder snow is a box of the height `snow_top` (falling blocks).
fn collide_with_snow(
    level: &LevelView<'_>,
    aabb: Aabb,
    movement: Vec3,
    snow_top: Option<f64>,
) -> Vec3 {
    let mut border = Vec::new();
    if movement.length_sqr() != 0.0 {
        let area = aabb.expand_towards(movement.x, movement.y, movement.z);
        crate::mob::physics::border_boxes(level.border(), &aabb, &area, &mut border);
    }
    let bottom = aabb.min_y;
    let collide_axis = |aabb: &Aabb, axis: usize, delta: f64| {
        let delta = collide_axis(level, aabb, axis, delta, bottom, snow_top);
        crate::mob::physics::collide_axis(&border, aabb, axis, delta)
    };
    let mut aabb = aabb;
    let dy = collide_axis(&aabb, 1, movement.y);
    aabb = aabb.offset(0.0, dy, 0.0);
    let (first, second) = if movement.x.abs() < movement.z.abs() {
        (2, 0)
    } else {
        (0, 2)
    };
    let mut out = [0.0, dy, 0.0];
    for axis in [first, second] {
        let delta = collide_axis(&aabb, axis, [movement.x, 0.0, movement.z][axis]);
        out[axis] = delta;
        aabb = match axis {
            0 => aabb.offset(delta, 0.0, 0.0),
            _ => aabb.offset(0.0, 0.0, delta),
        };
    }
    Vec3::new(out[0], out[1], out[2])
}

/// The movement along one axis (0 = X, 1 = Y, 2 = Z) until `aabb` touches a collision box
/// (`Shapes.collide` with its 1.0E-7 tolerance).
fn collide_axis(
    level: &LevelView<'_>,
    aabb: &Aabb,
    axis: usize,
    delta: f64,
    bottom: f64,
    snow_top: Option<f64>,
) -> f64 {
    const EPSILON: f64 = 1.0e-7;
    if delta.abs() < EPSILON {
        return 0.0;
    }
    let min = [aabb.min_x, aabb.min_y, aabb.min_z];
    let max = [aabb.max_x, aabb.max_y, aabb.max_z];
    let mut lo = min;
    let mut hi = max;
    if delta > 0.0 {
        hi[axis] += delta;
    } else {
        lo[axis] += delta;
    }
    // One block of margin below for shapes taller than a block (fences, walls).
    let start = BlockPos::containing(lo[0] - EPSILON, lo[1] - 1.0, lo[2] - EPSILON);
    let end = BlockPos::containing(hi[0] + EPSILON, hi[1] + EPSILON, hi[2] + EPSILON);
    let mut delta = delta;
    let mut moving = Vec::new();
    // `BlockCollisions` also reads the cells around the box: a moving piston's shape (its
    // block entity's) reaches up to a block out of its cell; corner cells never count.
    for y in start.y..=end.y + 1 {
        for z in start.z - 1..=end.z + 1 {
            for x in start.x - 1..=end.x + 1 {
                let edges = usize::from(x < start.x || x > end.x)
                    + usize::from(y > end.y)
                    + usize::from(z < start.z || z > end.z);
                if edges == 3 {
                    continue;
                }
                let pos = BlockPos::new(x, y, z);
                let state = level.block(pos);
                let piston = state.block() == ids::MOVING_PISTON;
                if edges > 0 && !piston {
                    continue;
                }
                let origin = [f64::from(x), f64::from(y), f64::from(z)];
                let snow;
                let shape = match snow_top {
                    Some(top) if state.block() == ids::POWDER_SNOW => {
                        snow = [[0.0, 0.0, 0.0, 1.0, top, 1.0]];
                        &snow[..]
                    }
                    _ if piston => {
                        moving.clear();
                        crate::mob::physics::Blocks::moving_piston_boxes(level, pos, &mut moving);
                        &moving[..]
                    }
                    _ => crate::mob::physics::context_shape(state, origin[1], Some(bottom)),
                };
                let (ox, oz) = crate::mob::physics::collision_offset(state, x, z);
                for b in shape {
                    let box_min = [
                        (b[0] + ox) + origin[0],
                        b[1] + origin[1],
                        (b[2] + oz) + origin[2],
                    ];
                    let box_max = [
                        (b[3] + ox) + origin[0],
                        b[4] + origin[1],
                        (b[5] + oz) + origin[2],
                    ];
                    let overlaps = (0..3)
                        .filter(|&a| a != axis)
                        .all(|a| box_max[a] - EPSILON > min[a] && box_min[a] + EPSILON < max[a]);
                    if !overlaps {
                        continue;
                    }
                    if delta > 0.0 && box_min[axis] >= max[axis] - EPSILON {
                        delta = delta.min(box_min[axis] - max[axis]);
                    } else if delta < 0.0 && box_max[axis] <= min[axis] + EPSILON {
                        delta = delta.max(box_max[axis] - min[axis]);
                    }
                }
            }
        }
    }
    delta
}

/// `FallingBlockEntity.tick` after placing the block: `block_update` and
/// `add_transient_block` of the new state to the players tracking the entity, so the client
/// shows the block before the entity disappears.
fn landed_packets(level: &mut Level, id: EntityId, pos: BlockPos) {
    use vanillite_protocol::packets::play::clientbound::{AddTransientBlock, BlockUpdate};
    let Some(at) = level.entities.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let state = i32::from(level.block(pos).0);
    let wire = vanillite_protocol::types::BlockPos {
        x: pos.x,
        y: pos.y,
        z: pos.z,
    };
    let range = f64::from(tracking_range());
    let out = level.output();
    out.nearby_packets.push((
        at,
        range,
        vanillite_protocol::SharedPacket::of(&BlockUpdate { pos: wire, state }),
    ));
    out.nearby_packets.push((
        at,
        range,
        vanillite_protocol::SharedPacket::of(&AddTransientBlock { pos: wire, state }),
    ));
}

/// The tracking range of falling blocks in blocks (`clientTrackingRange(10)`).
fn tracking_range() -> u32 {
    entity_ids::FALLING_BLOCK
        .info()
        .tracking_range
        .saturating_mul(16)
}
