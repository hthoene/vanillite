//! Experience orbs (`ExperienceOrb`): awarding experience as orbs, their movement towards the
//! nearest player, merging, despawning, pickup and Mending.

use std::cell::RefCell;

use vanillite_base::math::floor;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::entity::ids as entity_ids;
use vanillite_world::entity::{EntityFlags, EntityId, NetworkIds, NewEntity, Rotation};

use crate::enchant::Enchanting;
use crate::entities::Entities;
use crate::inventory::PlayerInventory;
use crate::item::{self, EquipmentSlot, ItemStack};
use crate::level::Level;
use vanillite_config::GameMode;

/// `LIFETIME`: ticks until an orb despawns.
pub const LIFETIME: i32 = 6000;
/// `ORB_GROUPS_PER_AREA`.
const ORB_GROUPS_PER_AREA: i32 = 40;
/// `getDefaultGravity()`.
const GRAVITY: f64 = 0.03;
/// `MAX_FOLLOW_DIST`.
const MAX_FOLLOW_DIST: f64 = 8.0;
/// `DEFAULT_HEALTH`.
const DEFAULT_HEALTH: i32 = 5;
/// `takeXpDelay` after a pickup.
pub const TAKE_XP_DELAY: i32 = 2;

/// The kind data of an experience orb.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExperienceOrbData {
    /// `DATA_VALUE`: experience per orb.
    pub value: i32,
    /// `count`: orbs merged into this one.
    pub count: i32,
    /// `age`.
    pub age: i32,
    /// `health`.
    pub health: i32,
    /// `followingPlayer`.
    pub following: Option<EntityId>,
    /// `tickCount`.
    pub tick_count: u32,
    /// `remainingFireTicks`: lava and fire set orbs burning.
    pub fire_ticks: i32,
    /// The state of the orb's own random (`Entity.random`).
    pub random_state: i64,
    /// `fallDistance`.
    pub fall_distance: f64,
}

impl ExperienceOrbData {
    /// A fresh orb of `value`.
    pub fn new(value: i32) -> ExperienceOrbData {
        ExperienceOrbData {
            value,
            count: 1,
            age: 0,
            health: DEFAULT_HEALTH,
            following: None,
            tick_count: 0,
            fire_ticks: 0,
            random_state: 0,
            fall_distance: 0.0,
        }
    }
}

/// `ExperienceOrb.getExperienceValue(maxValue)`: the largest orb size up to `max`.
pub fn experience_value(max: i32) -> i32 {
    const SIZES: [i32; 10] = [2477, 1237, 617, 307, 149, 73, 37, 17, 7, 3];
    SIZES.into_iter().find(|&s| max >= s).unwrap_or(1)
}

/// `getIcon()`: the client's orb sprite for a value.
pub fn icon(value: i32) -> i32 {
    const SIZES: [i32; 10] = [2477, 1237, 617, 307, 149, 73, 37, 17, 7, 3];
    SIZES
        .iter()
        .position(|&s| value >= s)
        .and_then(|i| i32::try_from(10 - i).ok())
        .unwrap_or(0)
}

thread_local! {
    /// The orbs' own random (`Entity.random`): the constructor's yaw and motion do not shift
    /// the level random.
    static ORB_RANDOM: RefCell<LegacyRandom> = RefCell::new(LegacyRandom::new(seed()));
}

/// Runs `f` with the random of entities that is not the level random (`Entity.random` of
/// orbs and players: pickup choice and Mending).
pub fn with_entity_random<R>(f: impl FnOnce(&mut LegacyRandom) -> R) -> R {
    ORB_RANDOM.with_borrow_mut(f)
}

/// Reseeds this thread's [`with_entity_random`] (tick exact comparisons with Vanilla).
pub fn seed_entity_random(seed: i64) {
    ORB_RANDOM.with_borrow_mut(|r| *r = LegacyRandom::new(seed));
}

fn seed() -> i64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    i64::from(nanos) ^ 0x2545_F491
}

/// `ExperienceOrb.award(level, pos, amount)`.
pub fn award(level: &mut Level, pos: Vec3, amount: i32) {
    award_with_direction(level, pos, Vec3::default(), amount);
}

/// `ExperienceOrb.awardWithDirection`: the amount in Vanilla's orb sizes, each merged into an
/// orb of the same size nearby or spawned.
pub fn award_with_direction(level: &mut Level, pos: Vec3, direction: Vec3, amount: i32) {
    let (entities, network_ids, random) = level.entity_parts();
    award_in(entities, network_ids, random, pos, direction, amount);
}

/// [`award_with_direction`] on the parts of a level: its entities, network ids and random.
pub fn award_in<R: RandomSource>(
    entities: &mut Entities,
    network_ids: &NetworkIds,
    random: &mut R,
    pos: Vec3,
    direction: Vec3,
    mut amount: i32,
) {
    while amount > 0 {
        let value = experience_value(amount);
        amount -= value;
        if !try_merge_to_existing(entities, random, pos, value) {
            spawn(entities, network_ids, pos, direction, value);
        }
    }
}

/// `tryMergeToExisting`: an orb of the same value and merge group in the block around `pos`
/// takes one more orb.
fn try_merge_to_existing<R: RandomSource>(
    entities: &mut Entities,
    random: &mut R,
    pos: Vec3,
    value: i32,
) -> bool {
    let area = Aabb::new(
        pos.x - 0.5,
        pos.y - 0.5,
        pos.z - 0.5,
        pos.x + 0.5,
        pos.y + 0.5,
        pos.z + 0.5,
    );
    let group = random.next_int_bounded(ORB_GROUPS_PER_AREA);
    let mut found = None;
    entities.store.for_each_in(&area, false, |other| {
        if found.is_none() && can_merge(entities, other, group, value) {
            found = Some(other);
        }
    });
    let Some(orb) = found.and_then(|id| entities.experience_orb_mut(id)) else {
        return false;
    };
    orb.count = orb.count.saturating_add(1);
    orb.age = 0;
    true
}

/// `canMerge(orb, id, value)`: a live orb of the value whose network id is in the group.
fn can_merge(entities: &Entities, orb: EntityId, group: i32, value: i32) -> bool {
    let Some(entity) = entities.store.get(orb) else {
        return false;
    };
    let network_id = entity.network_id().0;
    entities
        .experience_orb(orb)
        .is_some_and(|d| d.count > 0 && d.value == value)
        && network_id.wrapping_sub(group) % ORB_GROUPS_PER_AREA == 0
}

/// `new ExperienceOrb(level, pos, roughly, value)` and `addFreshEntity`.
pub(crate) fn spawn(
    entities: &mut Entities,
    network_ids: &NetworkIds,
    pos: Vec3,
    direction: Vec3,
    value: i32,
) -> EntityId {
    // The orb's own random (seeded from the entity seed stream): the yaw, then the motion.
    let (y_rot, mut motion, random_state) = {
        let mut r = LegacyRandom::new(crate::mob::new_entity_seed());
        let y_rot = r.next_float() * 360.0;
        let x = (r.next_double() * 0.2 - 0.1) * 2.0;
        let y = r.next_double() * 0.2 * 2.0;
        let z = (r.next_double() * 0.2 - 0.1) * 2.0;
        (y_rot, Vec3::new(x, y, z), r.state())
    };
    let length_sqr = direction.length_sqr();
    let mut at = pos;
    if length_sqr > 0.0 {
        if direction.x * motion.x + direction.y * motion.y + direction.z * motion.z < 0.0 {
            motion = Vec3::new(-motion.x, -motion.y, -motion.z);
        }
        // `pos.add(roughly.normalize().scale(size * 0.5))` with the box size 0.5.
        let length = length_sqr.sqrt();
        let scale = 0.25 / length;
        at = Vec3::new(
            pos.x + direction.x * scale,
            pos.y + direction.y * scale,
            pos.z + direction.z * scale,
        );
    }
    let mut new = NewEntity::new(
        entity_ids::EXPERIENCE_ORB,
        network_ids.next(),
        crate::block::tnt::insecure_uuid(),
        at,
    );
    new.velocity = motion;
    new.rot = Rotation {
        y_rot,
        x_rot: 0.0,
        y_head_rot: y_rot,
    };
    let mut data = ExperienceOrbData::new(value);
    data.random_state = random_state;
    let id = entities.spawn_experience_orb(new, data);
    entities.spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0);
    id
}

thread_local! {
    static SCRATCH: RefCell<Vec<Aabb>> = const { RefCell::new(Vec::new()) };
}

/// Whether the box collides with a block (`!noCollision(box)`).
fn collides(level: &Level, b: &Aabb) -> bool {
    SCRATCH.with_borrow_mut(|scratch| crate::item_entity::collides(b, level, scratch))
}

/// `ExperienceOrb.tick`.
pub fn tick_entity(level: &mut Level, id: EntityId) {
    let Some(mut data) = level.entities.experience_orb(id).copied() else {
        return;
    };
    let Some(entity) = level.entities.store.get(id) else {
        return;
    };
    let (pos, aabb) = (entity.pos(), entity.aabb());
    let mut v = entity.velocity();
    data.tick_count = data.tick_count.wrapping_add(1);
    // `checkBelowWorld`.
    if pos.y < f64::from(level.min_y()) - 64.0 {
        level.entities.despawn(id);
        return;
    }
    // `baseTick`: a burning orb takes one fire damage every second outside lava.
    if data.fire_ticks > 0 {
        let in_lava =
            crate::survival::blocks::fluid_contact(&level.view(), facts(), aabb, pos).in_lava();
        if data.fire_ticks % 20 == 0 && !in_lava {
            data.health -= 1;
            if data.health <= 0 {
                level.entities.despawn(id);
                return;
            }
        }
        data.fire_ticks -= 1;
    }
    if let Some(mut e) = level.entities.store.get_mut(id) {
        e.set_flags(EntityFlags::ON_FIRE, data.fire_ticks > 0);
    }
    // `baseTick`: `updateFluidInteraction` (water resets the fall distance, flowing water and
    // lava push the orb), then `ExperienceOrb.tick` swims up with its eyes in water.
    let eye = Vec3::new(pos.x, pos.y + f64::from(0.425_f32), pos.z);
    let (contact, currents) = crate::mob::living::fluid_currents(
        level,
        crate::survival::blocks::BlockFacts::fluids_only(),
        aabb,
        eye,
    );
    let in_water = contact.in_water();
    if in_water {
        data.fall_distance = 0.0;
        if let Some(pushed) = currents.water.push(v, 0.014) {
            v = pushed;
        }
    }
    if contact.in_lava()
        && let Some(pushed) = currents.lava.push(v, lava_current_scale(level))
    {
        v = pushed;
    }
    let colliding = collides(level, &aabb);
    if contact.eye_in_water {
        // `setUnderwaterMovement` (float constants).
        let drag = f64::from(0.99_f32);
        let rise = (v.y + f64::from(5.0e-4_f32)).min(f64::from(0.06_f32));
        v = Vec3::new(v.x * drag, rise, v.z * drag);
    } else if !colliding {
        // `applyGravity` adds a vector: a -0.0 of x or z becomes 0.0.
        v = v + Vec3::new(0.0, -GRAVITY, 0.0);
    }
    let feet = BlockPos::new(floor(pos.x), floor(pos.y), floor(pos.z));
    if crate::block::fluid::is_lava(level.fluid(feet)) {
        // `this.random`: the orb's own random.
        let mut r = LegacyRandom::from_state(data.random_state);
        let x = (r.next_float() - r.next_float()) * 0.2;
        let z = (r.next_float() - r.next_float()) * 0.2;
        data.random_state = r.state();
        let y = 0.2_f32;
        v = Vec3::new(f64::from(x), f64::from(y), f64::from(z));
    }
    if let Some(d) = level.entities.experience_orb_mut(id) {
        *d = data;
    }
    if data.tick_count % 20 == 1 {
        let radius = level.settings.xp_merge_radius;
        scan_for_merges(&mut level.entities, id, aabb, radius);
    }
    let following = follow_nearby_player(level, id, pos, &mut v);
    let mut needs_sync = false;
    if following.is_none() && colliding {
        let next = aabb.offset(v.x, v.y, v.z);
        if collides(level, &next) {
            let mut r = LegacyRandom::from_state(data.random_state);
            v = crate::item_entity::move_towards_closest_space(
                pos,
                (aabb.min_y + aabb.max_y) / 2.0,
                v,
                level,
                &mut || r.next_float(),
            );
            if let Some(d) = level.entities.experience_orb_mut(id) {
                d.random_state = r.state();
            }
            needs_sync = true;
        }
    }
    let fall_speed = v.y;
    // `move(MoverType.SELF, delta)`.
    let movement = crate::block::falling::collide(&level.view(), aabb, v);
    // `Entity.move` keeps the position when the collision left almost nothing of the motion.
    let moved = movement.length_sqr();
    let new_pos = if moved > 1.0e-7 || v.length_sqr() - moved < 1.0e-7 {
        pos + movement
    } else {
        pos
    };
    let hit_x = (v.x - movement.x).abs() >= 1.0e-5;
    let hit_z = (v.z - movement.z).abs() >= 1.0e-5;
    let hit_y = v.y != movement.y;
    let below = hit_y && v.y < 0.0;
    // `restituteMovementAfterCollisions` without bounciness: `-v * 0.0` keeps the sign bit.
    if hit_x {
        v.x = -v.x * 0.0;
    }
    if hit_z {
        v.z = -v.z * 0.0;
    }
    if hit_y {
        // `Block.updateEntityMovementAfterFallOn`.
        v.y = 0.0;
    }
    // `checkFallDamage`: the fall distance of the move, the landing.
    let mut fall_distance = data.fall_distance;
    if !in_water && movement.y < 0.0 {
        #[allow(clippy::cast_possible_truncation)]
        let ya = f64::from(movement.y as f32);
        fall_distance -= ya;
    }
    let on_ground = if fall_speed != 0.0 {
        below
    } else {
        level
            .entities
            .store
            .get(id)
            .is_some_and(|e| e.flags().contains(EntityFlags::ON_GROUND))
    };
    let landed = on_ground && fall_distance > 0.0;
    if on_ground {
        fall_distance = 0.0;
    }
    // `applyEffectsFromBlocks`: `stepOn` of the block below (`getOnPosLegacy`).
    let on = BlockPos::new(
        floor(new_pos.x),
        floor(new_pos.y - f64::from(0.2_f32)),
        floor(new_pos.z),
    );
    let on_block = level.block(on).block();
    if on_ground && on_block == vanillite_data::block::ids::SLIME_BLOCK {
        v = crate::block::contact::slime_step_on(v);
    }
    // `applyEffectsFromBlocks`: the blocks on the way (one walk; bubble columns act at once,
    // cacti, plates and tripwire after the move is stored).
    let dims = level.entities.store.get(id).map(|e| e.dims());
    let mut cacti = 0;
    // The `InsideBlockEffectType`s of the walk: fire damage, lava, extinguishing.
    let mut fire: Option<f32> = None;
    let mut lava = false;
    let mut extinguish = false;
    let mut reacting = Vec::new();
    let mut bubbles = [None; 4];
    if let Some(dims) = dims {
        let way = [crate::survival::inside::Movement::plain(pos, new_pos)];
        let view = level.view();
        let walker = crate::survival::inside::Walker {
            blocks: &view,
            facts: Some(facts()),
            dims,
            fast_fall: false,
        };
        let mut count = 0;
        walker.walk(&way, |cell| {
            let block = cell.state.block();
            if cell.in_fluid {
                let fluid = cell.state.fluid_state();
                if facts().is_lava(fluid) {
                    lava = true;
                } else if facts().is_water(fluid) {
                    extinguish = true;
                }
            }
            if cell.in_block {
                use vanillite_data::block::ids;
                if block == ids::LAVA_CAULDRON {
                    lava = true;
                } else if block == ids::WATER_CAULDRON
                    || block == ids::POWDER_SNOW_CAULDRON
                    || block == ids::POWDER_SNOW
                {
                    extinguish = true;
                } else if let Some(damage) = facts().fire_damage(cell.state) {
                    fire = Some(fire.map_or(damage, |d: f32| d.max(damage)));
                }
            }
            if cell.in_block {
                if cell.state.block() == vanillite_data::block::ids::CACTUS {
                    cacti += 1;
                } else if cell.state.block() == vanillite_data::block::ids::BUBBLE_COLUMN {
                    if cell.precise
                        && let Some(slot) = bubbles.get_mut(count)
                    {
                        *slot = Some((cell.pos, cell.state));
                        count += 1;
                    }
                } else if level.has_entity_inside(cell.state) {
                    reacting.push(cell.pos);
                }
            }
            true
        });
    }
    for (at, state) in bubbles.into_iter().flatten() {
        let above = BlockPos::new(at.x, at.y + 1, at.z);
        let (top, y) = crate::block::bubble::drag(level.block(above), state, v.y);
        v.y = y;
        if top {
            crate::block::bubble::send_particles(level, at);
        } else {
            fall_distance = 0.0;
        }
    }
    if !burn_effects(level, id, new_pos, fire, lava, extinguish) {
        return;
    }
    // `getAirDrag()` (0.98F) times the block friction, in float.
    let mut friction: f32 = 0.98;
    if below {
        let under = BlockPos::new(
            floor(new_pos.x),
            floor(new_pos.y - 0.999_999),
            floor(new_pos.z),
        );
        friction *= level.block(under).block().info().friction;
    }
    let friction = f64::from(friction);
    v = Vec3::new(v.x * friction, v.y * friction, v.z * friction);
    if below && fall_speed < -GRAVITY {
        v.y = -fall_speed * 0.4;
    }
    let Some(d) = level.entities.experience_orb_mut(id) else {
        return;
    };
    d.age = d.age.saturating_add(1);
    d.fall_distance = fall_distance;
    let expired = d.age >= LIFETIME;
    if let Some(mut e) = level.entities.store.get_mut(id) {
        e.set_pos(new_pos);
        e.set_velocity(v);
        if fall_speed != 0.0 {
            e.set_flags(EntityFlags::ON_GROUND, below);
        }
        if needs_sync {
            e.set_flags(EntityFlags::HAS_IMPULSE, true);
        }
    }
    if landed {
        crate::block::contact::non_living_land(level, id, new_pos, on);
    }
    if on_ground
        && crate::block::contact::has_step_on(on_block)
        && level.entities.store.contains(id)
    {
        crate::block::contact::step_on(level, on, id);
    }
    // `applyEffectsFromBlocks`: cacti hurt the orb, plates and tripwire react.
    if dims.is_some() {
        for block in reacting {
            if !level.entities.store.contains(id) {
                return;
            }
            level.entity_inside(block, id);
        }
        // `CactusBlock.entityInside` -> `ExperienceOrb.hurtServer`.
        let invulnerable = crate::mob::entity_nbt::is_invulnerable(&level.entities, id);
        for _ in 0..cacti {
            if invulnerable {
                break;
            }
            let Some(d) = level.entities.experience_orb_mut(id) else {
                return;
            };
            d.health -= 1;
            if d.health <= 0 {
                level.entities.despawn(id);
                return;
            }
        }
    }
    if expired {
        level.entities.despawn(id);
    }
}

/// `ExperienceOrb.getSoundSource`: `SoundSource.AMBIENT`.
const SOUND_AMBIENT: u8 = 8;

use crate::survival::blocks::fluid_facts as facts;

/// The fire part of `applyEffectsFromBlocks` (`insideEffectCollector.applyAndClear`, then
/// rain): fire blocks ignite the orb and hurt it (`BaseFireBlock.fireIgnite`), lava ignites it
/// for 15 s and hurts it by 4 (`lavaHurt`, with its burn sound from the orb's random), water
/// and powder snow put it out (`clearFire`). Whether the orb is still there.
fn burn_effects(
    level: &mut Level,
    id: EntityId,
    pos: Vec3,
    fire: Option<f32>,
    lava: bool,
    extinguish: bool,
) -> bool {
    let Some(d) = level.entities.experience_orb(id).copied() else {
        return false;
    };
    if fire.is_none() && !lava && d.fire_ticks == 0 {
        return true;
    }
    let mut d = d;
    let previous = d.fire_ticks;
    let was_on_fire = previous > 0;
    let mut removed = false;
    // `ExperienceOrb.hurtServer`: nothing once the orb is gone (`isRemoved`).
    let mut hurt = |d: &mut ExperienceOrbData, damage: f32| {
        if removed {
            return false;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let health = (d.health as f32 - damage) as i32;
        d.health = health;
        removed = health <= 0;
        true
    };
    if let Some(damage) = fire {
        if d.fire_ticks < 0 {
            d.fire_ticks += 1;
        }
        if d.fire_ticks >= 0 {
            d.fire_ticks = d.fire_ticks.max(160);
        }
        hurt(&mut d, damage);
    }
    let mut burn_pitch = None;
    if lava {
        d.fire_ticks = d.fire_ticks.max(300);
        if hurt(&mut d, 4.0) {
            let mut r = LegacyRandom::from_state(d.random_state);
            burn_pitch = Some(2.0 + r.next_float() * 0.4);
            d.random_state = r.state();
        }
    }
    if extinguish {
        d.fire_ticks = d.fire_ticks.min(0);
    }
    if let Some(pitch) = burn_pitch {
        level.play_sound(
            None,
            pos.x,
            pos.y,
            pos.z,
            crate::block::sound_id("minecraft:entity.generic.burn"),
            SOUND_AMBIENT,
            0.4,
            pitch,
        );
    }
    if removed {
        level.entities.despawn(id);
        return false;
    }
    if d.fire_ticks > 0 {
        // `isInRain`.
        let feet = BlockPos::new(floor(pos.x), floor(pos.y), floor(pos.z));
        let top = BlockPos::new(feet.x, floor(pos.y + 0.5), feet.z);
        if level.is_raining_at(feet) || level.is_raining_at(top) {
            d.fire_ticks = d.fire_ticks.min(0);
        }
    }
    if was_on_fire && d.fire_ticks <= 0 {
        // `playEntityOnFireExtinguishedSound`.
        let mut r = LegacyRandom::from_state(d.random_state);
        let pitch = 1.6 + (r.next_float() - r.next_float()) * 0.4;
        d.random_state = r.state();
        level.play_sound(
            None,
            pos.x,
            pos.y,
            pos.z,
            crate::block::sound_id("minecraft:entity.generic.extinguish_fire"),
            SOUND_AMBIENT,
            0.7,
            pitch,
        );
    }
    // Not burning and not ignited this tick: `-getFireImmuneTicks()`.
    if d.fire_ticks <= 0 && d.fire_ticks <= previous {
        d.fire_ticks = 0;
    }
    if let Some(slot) = level.entities.experience_orb_mut(id) {
        *slot = d;
    }
    true
}

/// `isInWater()` (`wasTouchingWater`): water reaches into the box shrunk by 0.001.
pub(crate) fn touching_water(level: &Level, b: &Aabb) -> bool {
    use vanillite_base::math::ceil;
    let min_y = b.min_y + 0.001;
    for x in floor(b.min_x + 0.001)..ceil(b.max_x - 0.001) {
        for y in floor(min_y)..ceil(b.max_y - 0.001) {
            for z in floor(b.min_z + 0.001)..ceil(b.max_z - 0.001) {
                let at = BlockPos::new(x, y, z);
                let fluid = level.fluid(at);
                if crate::block::fluid::is_water(fluid)
                    && f64::from(y) + f64::from(crate::block::fluid::height(level, fluid, at))
                        >= min_y
                {
                    return true;
                }
            }
        }
    }
    false
}

/// The scale of the lava current (`applyCurrentTo(LAVA, ...)`): faster where lava is fast.
pub(crate) fn lava_current_scale(level: &Level) -> f64 {
    if level.settings.fast_lava {
        0.007
    } else {
        0.002_333_333_333_333_333_5
    }
}

/// `scanForMerges`: orbs of the same value and merge group within `radius` (Vanilla half a
/// block) join this one.
fn scan_for_merges(entities: &mut Entities, id: EntityId, aabb: Aabb, radius: f64) {
    let Some(this) = entities.experience_orb(id).copied() else {
        return;
    };
    let Some(network_id) = entities.store.get(id).map(|e| e.network_id().0) else {
        return;
    };
    let mut near = Vec::new();
    entities
        .store
        .for_each_in(&aabb.inflate(radius, radius, radius), false, |other| {
            if other != id && can_merge(entities, other, network_id, this.value) {
                near.push(other);
            }
        });
    for other in near {
        let Some(that) = entities.experience_orb(other).copied() else {
            continue;
        };
        if let Some(d) = entities.experience_orb_mut(id) {
            d.count = d.count.saturating_add(that.count);
            d.age = d.age.min(that.age);
        }
        entities.despawn(other);
    }
}

/// `followNearbyPlayer`: keeps or finds the nearest non-spectator player within 8 blocks and
/// accelerates towards its middle.
fn follow_nearby_player(
    level: &mut Level,
    id: EntityId,
    pos: Vec3,
    v: &mut Vec3,
) -> Option<EntityId> {
    let entities = &level.entities;
    let mut following = entities.experience_orb(id).and_then(|d| d.following);
    let keep = following.and_then(|p| {
        let player = entities.player(p)?;
        let at = entities.store.get(p)?.pos();
        (player.game_mode != GameMode::Spectator && distance_sqr(at, pos) <= 64.0).then_some(at)
    });
    if keep.is_none() {
        following = nearest_player(entities, pos);
    }
    if let Some(d) = level.entities.experience_orb_mut(id) {
        d.following = following;
    }
    let player = following?;
    let entity = level.entities.store.get(player)?;
    let at = entity.pos();
    let eye = crate::survival::player::eye_height(entity.dims().height);
    let delta = Vec3::new(at.x - pos.x, at.y + eye / 2.0 - pos.y, at.z - pos.z);
    let length_sqr = delta.length_sqr();
    let power = 1.0 - length_sqr.sqrt() / 8.0;
    // `delta.normalize().scale(power * power * 0.1)`, in Vanilla's order of operations.
    let pull = delta.normalize() * (power * power * 0.1);
    *v = Vec3::new(v.x + pull.x, v.y + pull.y, v.z + pull.z);
    Some(player)
}

/// `level.getNearestPlayer(orb, 8.0)` of a player that is no spectator and alive.
fn nearest_player(entities: &Entities, pos: Vec3) -> Option<EntityId> {
    let d = MAX_FOLLOW_DIST;
    let area = Aabb::new(
        pos.x - d,
        pos.y - d,
        pos.z - d,
        pos.x + d,
        pos.y + d,
        pos.z + d,
    );
    let mut best: Option<(f64, EntityId)> = None;
    entities.store.for_each_in(&area, true, |id| {
        let Some(player) = entities.player(id) else {
            return;
        };
        if player.game_mode == GameMode::Spectator {
            return;
        }
        let Some(at) = entities.store.get(id).map(|e| e.pos()) else {
            return;
        };
        let dist = distance_sqr(at, pos);
        if dist < d * d && best.is_none_or(|(b, _)| dist < b) {
            best = Some((dist, id));
        }
    });
    let (_, id) = best?;
    let alive = entities.player(id).is_some_and(|p| p.survival.health > 0.0);
    alive.then_some(id)
}

fn distance_sqr(a: Vec3, b: Vec3) -> f64 {
    let (dx, dy, dz) = (a.x - b.x, a.y - b.y, a.z - b.z);
    dx * dx + dy * dy + dz * dz
}

/// The inventory index of an equipment slot.
fn inventory_index(inv: &PlayerInventory, slot: EquipmentSlot) -> Option<usize> {
    match slot {
        EquipmentSlot::MainHand => Some(inv.selected),
        EquipmentSlot::OffHand => Some(crate::inventory::OFFHAND),
        EquipmentSlot::Feet => Some(36),
        EquipmentSlot::Legs => Some(37),
        EquipmentSlot::Chest => Some(38),
        EquipmentSlot::Head => Some(39),
        EquipmentSlot::Body | EquipmentSlot::Saddle => None,
    }
}

/// `EquipmentSlot.VALUES`.
const SLOTS: [EquipmentSlot; 8] = [
    EquipmentSlot::MainHand,
    EquipmentSlot::OffHand,
    EquipmentSlot::Feet,
    EquipmentSlot::Legs,
    EquipmentSlot::Chest,
    EquipmentSlot::Head,
    EquipmentSlot::Body,
    EquipmentSlot::Saddle,
];

/// `isDamaged`.
fn is_damaged(stack: &ItemStack) -> bool {
    item::is_damageable(stack) && item::damage(stack) > 0
}

/// `ExperienceOrb.repairPlayerItems`: Mending repairs a random damaged item that has an
/// enchantment with `repair_with_xp` in a matching slot, recursively with the experience left;
/// returns the experience that is left for the player.
pub fn repair_player_items(
    inv: &mut PlayerInventory,
    enchanting: &Enchanting,
    amount: i32,
    random: &mut dyn FnMut(i32) -> i32,
) -> i32 {
    let mut amount = amount;
    loop {
        // `EnchantmentHelper.getRandomItemWith(REPAIR_WITH_XP, player, ItemStack::isDamaged)`.
        let mut candidates: Vec<usize> = Vec::new();
        for slot in SLOTS {
            let Some(index) = inventory_index(inv, slot) else {
                continue;
            };
            let stack = inv.get(index);
            if !is_damaged(stack) {
                continue;
            }
            for (enchantment, _) in item::enchantments(stack) {
                if enchanting
                    .get(enchantment)
                    .is_some_and(|e| e.repairs_with_xp() && e.matches_slot(slot))
                {
                    candidates.push(index);
                }
            }
        }
        if candidates.is_empty() {
            return amount;
        }
        let n = i32::try_from(candidates.len()).unwrap_or(i32::MAX);
        let Some(&index) = usize::try_from(random(n))
            .ok()
            .and_then(|i| candidates.get(i))
        else {
            return amount;
        };
        let Some(stack) = inv.get_mut(index) else {
            return amount;
        };
        // `EnchantmentHelper.modifyDurabilityToRepairFromXp`.
        let to_repair = enchanting.durability_to_repair_from_xp(stack, amount);
        let damage = item::damage(stack);
        let repair = to_repair.min(damage);
        item::set_damage(stack, damage - repair);
        if repair <= 0 || to_repair <= 0 {
            return 0;
        }
        let remaining = amount - repair.saturating_mul(amount) / to_repair;
        if remaining <= 0 {
            return 0;
        }
        amount = remaining;
    }
}

/// `ExperienceOrb.playerTouch` of a player with an empty `takeXpDelay`: one orb of the stack
/// is taken, repairs with Mending and gives the rest as experience points. Returns the points
/// to give (the caller runs `giveExperiencePoints`), `None` if the orb is gone.
pub fn take_one(
    entities: &mut Entities,
    orb: EntityId,
    inv: &mut PlayerInventory,
    enchanting: &Enchanting,
    random: &mut dyn FnMut(i32) -> i32,
) -> Option<i32> {
    let data = entities.experience_orb_mut(orb)?;
    if data.count <= 0 {
        return None;
    }
    data.count -= 1;
    let value = data.value;
    Some(repair_player_items(inv, enchanting, value, random))
}

/// The orbs a player's pickup box touches, in entity order.
pub fn touching(entities: &Entities, reach: &Aabb, out: &mut Vec<EntityId>) {
    out.clear();
    entities.store.for_each_non_player_in(reach, |id| {
        if entities.experience_orb(id).is_some_and(|d| d.count > 0) {
            out.push(id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_split_like_vanilla() {
        let split = |mut amount: i32| {
            let mut out = Vec::new();
            while amount > 0 {
                let v = experience_value(amount);
                amount -= v;
                out.push(v);
            }
            out
        };
        assert_eq!(split(1), vec![1]);
        assert_eq!(split(2), vec![1, 1]);
        assert_eq!(split(10), vec![7, 3]);
        assert_eq!(split(100), vec![73, 17, 7, 3]);
        assert_eq!(split(5000), vec![2477, 2477, 37, 7, 1, 1]);
        assert_eq!(icon(2477), 10);
        assert_eq!(icon(3), 1);
        assert_eq!(icon(2), 0);
    }

    #[test]
    fn awards_merge_into_orbs_of_the_same_group() {
        let ids = NetworkIds::new();
        let mut entities = Entities::new();
        let mut random = LegacyRandom::new(1);
        let pos = Vec3::new(0.5, 64.0, 0.5);
        // Spawn 40 orbs of value 1: one per merge group (network ids are consecutive).
        for _ in 0..40 {
            spawn(&mut entities, &ids, pos, Vec3::default(), 1);
        }
        let before = entities.experience_orbs.iter().count();
        award_in(&mut entities, &ids, &mut random, pos, Vec3::default(), 1);
        assert_eq!(entities.experience_orbs.iter().count(), before);
        let total: i32 = entities.experience_orbs.iter().map(|(_, d)| d.count).sum();
        assert_eq!(total, 41);
        // Another value never merges.
        award_in(&mut entities, &ids, &mut random, pos, Vec3::default(), 3);
        assert_eq!(entities.experience_orbs.iter().count(), before + 1);
    }

    #[test]
    fn mending_repairs_and_returns_the_rest() {
        use vanillite_data::item::ids as items;
        let mending: serde_json::Value = serde_json::from_str(
            r#"{"effects": {"minecraft:repair_with_xp": [{"effect":
                {"type": "minecraft:multiply", "factor": 2.0}}]},
                "slots": ["any"], "max_level": 1}"#,
        )
        .unwrap_or_default();
        let enchanting = Enchanting::load(
            &[("minecraft:mending".to_owned(), mending)],
            &|_| Vec::new(),
            &|_| Vec::new(),
        );
        let mut pick = ItemStack::new(items::DIAMOND_PICKAXE.0, 1);
        pick.set_component(
            vanillite_protocol::components::ids::ENCHANTMENTS,
            crate::enchant::write(&[(0, 1)]),
        );
        item::set_damage(&mut pick, 10);
        let mut inv = PlayerInventory::default();
        inv.set(0, pick);
        let mut random = |_| 0;
        // 3 experience repair 6 durability and are used up.
        assert_eq!(
            repair_player_items(&mut inv, &enchanting, 3, &mut random),
            0
        );
        assert_eq!(item::damage(inv.get(0)), 4);
        // 5 experience could repair 10: the 4 points of damage take 2, 3 are left.
        assert_eq!(
            repair_player_items(&mut inv, &enchanting, 5, &mut random),
            3
        );
        assert_eq!(item::damage(inv.get(0)), 0);
        // Without Mending everything is left.
        let mut plain = PlayerInventory::default();
        let mut damaged = ItemStack::new(items::DIAMOND_PICKAXE.0, 1);
        item::set_damage(&mut damaged, 10);
        plain.set(0, damaged);
        assert_eq!(
            repair_player_items(&mut plain, &enchanting, 5, &mut random),
            5
        );
    }
}
