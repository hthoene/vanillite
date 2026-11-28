//! Fishing (`FishingRodItem`, `FishingHook`): a cast bobber flies until it lands in water, on
//! the ground or in an entity. In water it bobs; after `100..600` ticks (minus Lure) a fish
//! swims toward it and bites for `20..40` ticks; reeling in then rolls the `gameplay/fishing`
//! loot table (Luck of the Sea and luck raise the treasure chance, open water allows it). A
//! hooked entity is pulled toward the player.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_nbt::Compound;
use vanillite_protocol::packets::play::clientbound::{EntityEvent, SoundSource};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{EntityFlags, EntityId, NetworkIds, Rotation};
use vanillite_worldgen::behaviour::{PosExt, StateExt};

use crate::entities::Entities;
use crate::item::{self, ItemStack};
use crate::level::Level;
use crate::mob::MobContext;
use crate::mob::missile::{self, MissileData, MissileKind, MissileTick};
use crate::mob::physics::{self, Blocks};
use crate::player::encode::DataWriter;
use crate::player::item_use::Used;
use crate::survival::play_sound;
use crate::survival::player::Env;

/// `FishHookState`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HookState {
    #[default]
    Flying,
    HookedInEntity,
    Bobbing,
}

/// The state of a fishing bobber.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bobber {
    pub state: HookState,
    pub biting: bool,
    pub open_water: bool,
    /// Ticks on the ground (`life`).
    pub ground_life: i32,
    pub nibble: i32,
    pub time_until_lured: i32,
    pub time_until_hooked: i32,
    pub out_of_water_time: i32,
    pub fish_angle: f32,
    pub luck: i32,
    pub lure_speed: i32,
    /// `hookedIn`.
    pub hooked: Option<EntityId>,
    /// The hooked entity's network id plus one (`DATA_HOOKED_ENTITY`).
    pub hooked_network: i32,
}

/// `FishingRodItem.use`: reels in the player's bobber or casts a new one.
#[allow(clippy::too_many_arguments)]
pub fn use_rod(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    network_ids: &NetworkIds,
    player: EntityId,
    stack: &mut ItemStack,
    hand: Hand,
) -> Used {
    let Some(pos) = ents.store.get(player).map(|e| e.pos()) else {
        return Used::Pass;
    };
    let fishing = ents
        .player(player)
        .and_then(|p| p.survival.fishing)
        .filter(|&b| ents.missile(b).is_some_and(|d| !d.removed));
    let uuid = ents.store.get(player).map_or(0, |e| e.uuid());
    if let Some(bobber) = fishing {
        let damage = retrieve(
            level,
            ents,
            env,
            cx,
            network_ids,
            bobber,
            player,
            stack,
            hand,
        );
        hurt_rod(ents, env, player, stack, damage, hand);
        let pitch = 0.4 / (env.random.next_float() * 0.4 + 0.8);
        play_sound(
            env.out,
            None,
            pos,
            "minecraft:entity.fishing_bobber.retrieve",
            SoundSource::Neutral,
            1.0,
            pitch,
            crate::survival::sound_seed(),
        );
        return Used::Success;
    }
    if !env
        .hooks
        .player_fish(crate::hooks::HookWorld::split(level, ents), uuid, "fishing")
    {
        return Used::Pass;
    }
    let pitch = 0.4 / (env.random.next_float() * 0.4 + 0.8);
    play_sound(
        env.out,
        None,
        pos,
        "minecraft:entity.fishing_bobber.throw",
        SoundSource::Neutral,
        0.5,
        pitch,
        crate::survival::sound_seed(),
    );
    let combat = crate::mob::attack::combat_data();
    let enchants = &combat.enchants;
    let (lure, luck) = crate::experience_orb::with_entity_random(|r| {
        (
            enchants.fishing_time_reduction(stack, r),
            enchants.fishing_luck_bonus(stack, r),
        )
    });
    #[allow(clippy::cast_possible_truncation)]
    let lure_speed = (lure * 20.0) as i32;
    cast(ents, network_ids, player, luck, lure_speed);
    if let Some(p) = ents.player_mut(player) {
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, stack.item),
            1,
        );
    }
    Used::Success
}

/// `new FishingHook(player, level, luck, lureSpeed)` and `addFreshEntity`.
fn cast(ents: &mut Entities, network_ids: &NetworkIds, player: EntityId, luck: i32, lure: i32) {
    let Some(e) = ents.store.get(player) else {
        return;
    };
    let (pos, rot, uuid) = (e.pos(), e.rot(), e.uuid());
    let eye = missile::eye_height(ents, player);
    let mut own = LegacyRandom::new(crate::mob::new_entity_seed());
    let hook_uuid = crate::mob::new_uuid();
    let rad = crate::mob::DEG_TO_RAD;
    let pi = std::f32::consts::PI;
    let sin = |a: f32| vanillite_base::math::sin(f64::from(a));
    let cos = |a: f32| vanillite_base::math::cos(f64::from(a));
    let y_cos = cos(-rot.y_rot * rad - pi);
    let y_sin = sin(-rot.y_rot * rad - pi);
    let x_cos = -cos(-rot.x_rot * rad);
    let x_sin = sin(-rot.x_rot * rad);
    let start = Vec3::new(
        pos.x - f64::from(y_sin) * 0.3,
        pos.y + eye,
        pos.z - f64::from(y_cos) * 0.3,
    );
    let mut m = Vec3::new(
        f64::from(-y_sin),
        f64::from((-(x_sin / x_cos)).clamp(-5.0, 5.0)),
        f64::from(-y_cos),
    );
    let dist = m.length();
    let fx = 0.6 / dist + own.triangle(0.5, 0.010_336_5);
    let fy = 0.6 / dist + own.triangle(0.5, 0.010_336_5);
    let fz = 0.6 / dist + own.triangle(0.5, 0.010_336_5);
    m = m.multiply(fx, fy, fz);
    let h = (m.x * m.x + m.z * m.z).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let r = Rotation {
        y_rot: crate::mob::atan2_deg(m.x, m.z) as f32,
        x_rot: crate::mob::atan2_deg(m.y, h) as f32,
        y_head_rot: 0.0,
    };
    let mut d = MissileData::new(MissileKind::FishingBobber);
    d.owner = Some(player);
    d.owner_uuid = Some(uuid);
    d.random_state = own.state();
    d.bobber = Some(Box::new(Bobber {
        open_water: true,
        luck: luck.max(0),
        lure_speed: lure.max(0),
        ..Bobber::default()
    }));
    let id = missile::spawn(ents, network_ids, hook_uuid, start, m, r, d);
    if let Some(p) = ents.player_mut(player) {
        p.survival.fishing = Some(id);
    }
}

/// `hurtAndBreak(damage, player, hand)` of the rod.
fn hurt_rod(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    rod: &mut ItemStack,
    damage: i32,
    hand: Hand,
) {
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    let points = crate::durability::change(rod, damage, creative, env.random);
    if points == 0 {
        return;
    }
    let d = item::damage(rod) + points;
    let broke = match ents.player_mut(player) {
        Some(p) => crate::advancements::view::apply_damage(
            Some(&mut p.advancements),
            Some(&mut p.stats),
            rod,
            d,
        ),
        None => crate::advancements::view::apply_damage(None, None, rod, d),
    };
    if broke {
        let slot = match hand {
            Hand::MainHand => item::EquipmentSlot::MainHand,
            Hand::OffHand => item::EquipmentSlot::OffHand,
        };
        crate::durability::broken_in_env(ents, env, player, slot);
    }
}

/// The player owning a bobber, if it is a loaded player.
fn player_owner(ents: &Entities, d: &MissileData) -> Option<EntityId> {
    d.owner.filter(|o| ents.player(*o).is_some())
}

/// `shouldStopFishing`: the owner must be alive, hold a rod and be within 32 blocks.
fn should_stop(ents: &Entities, bobber: EntityId, owner: EntityId) -> bool {
    let Some(p) = ents.player(owner) else {
        return true;
    };
    let usable = !p.survival.is_dead_or_dying() && p.game_mode != GameMode::Spectator;
    let is_rod =
        |s: &ItemStack| !s.is_empty() && item::item_id(s).name() == "minecraft:fishing_rod";
    let holds = is_rod(p.inventory.selected_item()) || is_rod(&p.inventory.offhand);
    let near = match (ents.store.get(bobber), ents.store.get(owner)) {
        (Some(b), Some(o)) => b.pos().distance_to_sqr(o.pos()) <= 1024.0,
        _ => false,
    };
    !(usable && holds && near)
}

/// Removes the bobber and clears the owner's `fishing`.
fn discard(ents: &mut Entities, bobber: EntityId) {
    let owner = ents.missile(bobber).and_then(|d| d.owner);
    if let Some(m) = ents.missile_mut(bobber) {
        m.removed = true;
    }
    if let Some(p) = owner.and_then(|o| ents.player_mut(o))
        && p.survival.fishing == Some(bobber)
    {
        p.survival.fishing = None;
    }
}

/// The water height at `pos` (`FluidState.getHeight` of water), 0 elsewhere.
fn water_height(level: &Level, pos: BlockPos) -> f32 {
    let fluid = level.block(pos).fluid_state();
    if fluid.is_empty() || !crate::mob::is_water(level.block(pos)) {
        return 0.0;
    }
    if crate::mob::is_water(level.block(pos.above())) {
        1.0
    } else {
        fluid.info().own_height
    }
}

/// `FishingHook.tick`.
pub fn tick(t: &mut MissileTick<'_, '_>) {
    let id = t.id;
    let Some(mut d) = t.ents.missile(id).cloned() else {
        return;
    };
    let Some(mut b) = d.bobber.take().map(|b| *b) else {
        discard(t.ents, id);
        return;
    };
    let Some(e) = t.core() else {
        return;
    };
    let (pos, aabb, rot, on_ground, uuid) = (
        e.pos(),
        e.aabb(),
        e.rot(),
        e.flags().contains(EntityFlags::ON_GROUND),
        e.uuid(),
    );
    let mut v = e.velocity();
    // `syncronizedRandom.setSeed(uuid.leastSignificantBits ^ gameTime)`.
    #[allow(clippy::cast_possible_truncation)]
    let mut synced = LegacyRandom::new((uuid as u64 as i64) ^ t.env.game_time);
    let mut own = LegacyRandom::from_state(d.random_state);
    // `Projectile.tick`: `checkLeftOwner`.
    if !d.left_owner {
        let area = aabb.expand_towards(v.x, v.y, v.z).inflate(1.0, 1.0, 1.0);
        d.left_owner = d
            .owner
            .and_then(|o| t.ents.store.get(o))
            .is_none_or(|o| !o.aabb().intersects(&area));
    }
    let Some(owner) = player_owner(t.ents, &d) else {
        store(t, d, b, &own);
        discard(t.ents, id);
        return;
    };
    if should_stop(t.ents, id, owner) {
        store(t, d, b, &own);
        discard(t.ents, id);
        return;
    }
    if on_ground {
        b.ground_life += 1;
        if b.ground_life >= 1200 {
            store(t, d, b, &own);
            discard(t.ents, id);
            return;
        }
    } else {
        b.ground_life = 0;
    }
    let block = BlockPos::containing(pos.x, pos.y, pos.z);
    let liquid = water_height(t.level, block);
    let in_water = liquid > 0.0;
    let mut data_dirty = false;
    match b.state {
        HookState::Flying => {
            if b.hooked.is_some() {
                set_velocity(t, Vec3::ZERO);
                b.state = HookState::HookedInEntity;
                store(t, d, b, &own);
                return;
            }
            if in_water {
                set_velocity(t, v.multiply(0.3, 0.2, 0.3));
                b.state = HookState::Bobbing;
                store(t, d, b, &own);
                return;
            }
            // `checkCollision`: the first block or entity on the way.
            match find_hit(t, &d, pos, v, aabb) {
                Hit::Entity(target) => {
                    b.hooked = Some(target);
                    b.hooked_network = t.ents.store.get(target).map_or(0, |e| e.network_id().0 + 1);
                    data_dirty = true;
                }
                Hit::Block(at) => {
                    // `onHitBlock`: `normalize().scale(hitResult.distanceTo(this))`, a squared
                    // distance.
                    let len = v.length();
                    v = if len < 1.0e-5 {
                        Vec3::ZERO
                    } else {
                        Vec3::new(v.x / len, v.y / len, v.z / len) * at.distance_to_sqr(pos)
                    };
                }
                Hit::Miss => {}
            }
        }
        HookState::HookedInEntity => {
            let target = b
                .hooked
                .and_then(|h| t.ents.store.get(h).map(|e| (e.pos(), e.dims().height)));
            match target {
                Some((p, h)) if alive(t.ents, b.hooked) => {
                    if let Some(mut e) = t.ents.store.get_mut(id) {
                        e.set_pos(Vec3::new(p.x, p.y + f64::from(h) * 0.8, p.z));
                    }
                }
                _ => {
                    b.hooked = None;
                    b.hooked_network = 0;
                    b.state = HookState::Flying;
                    data_dirty = true;
                }
            }
            store(t, d, b, &own);
            if data_dirty && let Some(mut e) = t.ents.store.get_mut(id) {
                e.mark_data_dirty();
            }
            return;
        }
        HookState::Bobbing => {
            let mut force = pos.y + v.y - f64::from(block.y) - f64::from(liquid);
            if force.abs() < 0.01 {
                force += force.signum() * 0.1;
            }
            v = Vec3::new(
                v.x * 0.9,
                v.y - force * f64::from(own.next_float()) * 0.2,
                v.z * 0.9,
            );
            if b.nibble <= 0 && b.time_until_hooked <= 0 {
                b.open_water = true;
            } else {
                b.open_water =
                    b.open_water && b.out_of_water_time < 10 && open_water(t.level, block);
            }
            if in_water {
                b.out_of_water_time = (b.out_of_water_time - 1).max(0);
                if b.biting {
                    let dy = -0.1 * f64::from(synced.next_float()) * f64::from(synced.next_float());
                    v = v + Vec3::new(0.0, dy, 0.0);
                }
                set_velocity(t, v);
                data_dirty |= catching_fish(t, &mut b, &mut own, &mut synced, block);
                v = t.core().map_or(v, |e| e.velocity());
            } else {
                b.out_of_water_time = (b.out_of_water_time + 1).min(10);
            }
        }
    }
    let fluid_water = crate::mob::is_water(t.level.block(block));
    if !fluid_water && !on_ground && b.hooked.is_none() {
        // `add(0, -gravity, 0)` (a negative zero becomes positive).
        v = v + Vec3::new(0.0, -f64::from(0.03_f32), 0.0);
    }
    // `move(SELF, movement)`.
    let mut scratch = Vec::new();
    let moved = physics::collide(
        t.level as &dyn Blocks,
        aabb,
        v,
        0.0,
        on_ground,
        &mut scratch,
    );
    let new_pos = pos + moved;
    let x_hit = (moved.x - v.x).abs() > 1.0e-7;
    let z_hit = (moved.z - v.z).abs() > 1.0e-7;
    let vertical = v.y != moved.y;
    let landed = vertical && v.y < 0.0;
    let mut after = Vec3::new(
        if x_hit { 0.0 } else { v.x },
        if vertical { 0.0 } else { v.y },
        if z_hit { 0.0 } else { v.z },
    );
    // `updateRotation`.
    let h = (after.x * after.x + after.z * after.z).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let new_rot = Rotation {
        y_rot: crate::mob::projectile::lerp_rotation(
            rot.y_rot,
            crate::mob::atan2_deg(after.x, after.z) as f32,
        ),
        x_rot: crate::mob::projectile::lerp_rotation(
            rot.x_rot,
            crate::mob::atan2_deg(after.y, h) as f32,
        ),
        y_head_rot: 0.0,
    };
    if b.state == HookState::Flying && (landed || x_hit || z_hit) {
        after = Vec3::ZERO;
    }
    after = after * 0.92;
    if let Some(mut e) = t.ents.store.get_mut(id) {
        e.set_pos(new_pos);
        e.set_velocity(after);
        e.set_rot(new_rot);
        e.set_flags(EntityFlags::ON_GROUND, landed);
        if data_dirty {
            e.mark_data_dirty();
        }
    }
    store(t, d, b, &own);
}

fn alive(ents: &Entities, id: Option<EntityId>) -> bool {
    let Some(id) = id else {
        return false;
    };
    if let Some(m) = ents.mob(id) {
        return m.is_alive();
    }
    if let Some(p) = ents.player(id) {
        return !p.survival.is_dead_or_dying() && p.game_mode != GameMode::Spectator;
    }
    ents.store.get(id).is_some()
}

fn set_velocity(t: &mut MissileTick<'_, '_>, v: Vec3) {
    if let Some(mut e) = t.ents.store.get_mut(t.id) {
        e.set_velocity(v);
    }
}

/// Writes the bobber state back.
fn store(t: &mut MissileTick<'_, '_>, mut d: MissileData, b: Bobber, own: &LegacyRandom) {
    d.bobber = Some(Box::new(b));
    d.random_state = own.state();
    if let Some(m) = t.ents.missile_mut(t.id) {
        let removed = m.removed;
        *m = d;
        m.removed |= removed;
    }
}

/// What a flying bobber hits.
enum Hit {
    Miss,
    Entity(EntityId),
    Block(Vec3),
}

/// `ProjectileUtil.getHitResultOnMoveVector(this, canHitEntity)`: living entities, vehicles
/// and item entities.
fn find_hit(t: &mut MissileTick<'_, '_>, d: &MissileData, pos: Vec3, v: Vec3, aabb: Aabb) -> Hit {
    let to = pos + v;
    let block_hit = physics::clip(t.level as &dyn Blocks, pos, to);
    let end = block_hit.map_or(to, |(_, p)| p);
    let m = missile::margin(d.life);
    let area = aabb.expand_towards(v.x, v.y, v.z).inflate(1.0, 1.0, 1.0);
    let mut candidates = std::mem::take(&mut t.cx.ids);
    candidates.clear();
    let id = t.id;
    t.ents.store.for_each_in(&area, false, |other| {
        if other != id {
            candidates.push(other);
        }
    });
    let mut best: Option<(EntityId, f64)> = None;
    for &other in &candidates {
        let hittable = crate::effect::thrown::can_be_hit(t.ents, other)
            && (d.left_owner || Some(other) != d.owner);
        let item = t.ents.item(other).is_some_and(|i| !i.stack.is_empty());
        if !(hittable || item) {
            continue;
        }
        let Some(oe) = t.ents.store.get(other) else {
            continue;
        };
        let grown = oe.aabb().inflate(m, m, m);
        if let Some(f) = crate::mob::projectile::segment_box(pos, end, &grown)
            && best.is_none_or(|(_, b)| f < b)
        {
            best = Some((other, f));
        }
    }
    t.cx.ids = candidates;
    match (best, block_hit) {
        (Some((e, _)), _) => Hit::Entity(e),
        (None, Some((_, at))) => Hit::Block(at),
        (None, None) => Hit::Miss,
    }
}

/// `catchingFish`: the lure and hook countdowns, the fish's approach and the bite. Returns
/// whether the biting state changed.
fn catching_fish(
    t: &mut MissileTick<'_, '_>,
    b: &mut Bobber,
    own: &mut LegacyRandom,
    synced: &mut LegacyRandom,
    block: BlockPos,
) -> bool {
    let above = block.above();
    let mut speed = 1;
    if own.next_float() < 0.25 && (t.env.raining_at)(above) {
        speed += 1;
    }
    if own.next_float() < 0.5 && !crate::mob::can_see_sky(t.level, above) {
        speed -= 1;
    }
    let pos = t.core().map_or(Vec3::ZERO, |e| e.pos());
    let width = t.core().map_or(0.25, |e| e.dims().width);
    if b.nibble > 0 {
        b.nibble -= 1;
        if b.nibble <= 0 {
            b.time_until_lured = 0;
            b.time_until_hooked = 0;
            b.biting = false;
            return true;
        }
    } else if b.time_until_hooked > 0 {
        b.time_until_hooked -= speed;
        if b.time_until_hooked > 0 {
            #[allow(clippy::cast_possible_truncation)]
            {
                b.fish_angle += own.triangle(0.0, 9.188) as f32;
            }
            let angle = b.fish_angle * crate::mob::DEG_TO_RAD;
            let (s, c) = (
                vanillite_base::math::sin(f64::from(angle)),
                vanillite_base::math::cos(f64::from(angle)),
            );
            #[allow(clippy::cast_precision_loss)]
            let dist = b.time_until_hooked as f32 * 0.1;
            let fish = Vec3::new(
                pos.x + f64::from(s * dist),
                f64::from(vanillite_base::math::floor(pos.y)) + 1.0,
                pos.z + f64::from(c * dist),
            );
            let below = BlockPos::containing(fish.x, fish.y - 1.0, fish.z);
            if t.level.block(below).is(vanillite_data::block::ids::WATER) {
                if own.next_float() < 0.15 {
                    t.cx.send_particles(
                        crate::level::particles::simple("minecraft:bubble"),
                        Vec3::new(fish.x, fish.y - f64::from(0.1_f32), fish.z),
                        1,
                        Vec3::new(f64::from(s), 0.1, f64::from(c)),
                        0.0,
                    );
                }
                let (px, pz) = (s * 0.04, c * 0.04);
                t.cx.send_particles(
                    crate::level::particles::simple("minecraft:fishing"),
                    fish,
                    0,
                    Vec3::new(f64::from(pz), 0.01, f64::from(-px)),
                    1.0,
                );
                t.cx.send_particles(
                    crate::level::particles::simple("minecraft:fishing"),
                    fish,
                    0,
                    Vec3::new(f64::from(-pz), 0.01, f64::from(px)),
                    1.0,
                );
            }
        } else {
            let pitch = 1.0 + (own.next_float() - own.next_float()) * 0.4;
            play_sound(
                t.env.out,
                None,
                pos,
                "minecraft:entity.fishing_bobber.splash",
                SoundSource::Neutral,
                0.25,
                pitch,
                crate::survival::sound_seed(),
            );
            let y = pos.y + 0.5;
            #[allow(clippy::cast_possible_truncation)]
            let count = (1.0 + width * 20.0) as i32;
            for particle in ["minecraft:bubble", "minecraft:fishing"] {
                t.cx.send_particles(
                    crate::level::particles::simple(particle),
                    Vec3::new(pos.x, y, pos.z),
                    count,
                    Vec3::new(f64::from(width), 0.0, f64::from(width)),
                    f64::from(0.2_f32),
                );
            }
            b.nibble = own.next_int_between_inclusive(20, 40);
            b.biting = true;
            // `onSyncedDataUpdated(DATA_BITING)`: the bobber dips.
            if let Some(mut e) = t.ents.store.get_mut(t.id) {
                let v = e.as_ref().velocity();
                let dip = -0.4 * (0.6 + synced.next_float() * 0.4);
                e.set_velocity(Vec3::new(v.x, f64::from(dip), v.z));
            }
            return true;
        }
    } else if b.time_until_lured > 0 {
        b.time_until_lured -= speed;
        let mut tease = 0.15_f32;
        #[allow(clippy::cast_precision_loss)]
        if b.time_until_lured < 20 {
            tease += (20 - b.time_until_lured) as f32 * 0.05;
        } else if b.time_until_lured < 40 {
            tease += (40 - b.time_until_lured) as f32 * 0.02;
        } else if b.time_until_lured < 60 {
            tease += (60 - b.time_until_lured) as f32 * 0.01;
        }
        if own.next_float() < tease {
            let angle = own.next_float() * 360.0 * crate::mob::DEG_TO_RAD;
            let dist = 25.0 + own.next_float() * 35.0;
            let fish = Vec3::new(
                pos.x
                    + f64::from(vanillite_base::math::sin(f64::from(angle)))
                        * f64::from(dist)
                        * 0.1,
                f64::from(vanillite_base::math::floor(pos.y)) + 1.0,
                pos.z
                    + f64::from(vanillite_base::math::cos(f64::from(angle)))
                        * f64::from(dist)
                        * 0.1,
            );
            let below = BlockPos::containing(fish.x, fish.y - 1.0, fish.z);
            if t.level.block(below).is(vanillite_data::block::ids::WATER) {
                let count = 2 + own.next_int_bounded(2);
                t.cx.send_particles(
                    crate::level::particles::simple("minecraft:splash"),
                    fish,
                    count,
                    Vec3::new(f64::from(0.1_f32), 0.0, f64::from(0.1_f32)),
                    0.0,
                );
            }
        }
        if b.time_until_lured <= 0 {
            b.fish_angle = own.next_float() * 360.0;
            b.time_until_hooked = own.next_int_between_inclusive(20, 80);
        }
    } else {
        b.time_until_lured = own.next_int_between_inclusive(100, 600) - b.lure_speed;
    }
    false
}

/// `calculateOpenWater`: the 5×5 layers from one below to two above are water then air.
fn open_water(level: &Level, pos: BlockPos) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Layer {
        Above,
        Inside,
        Invalid,
    }
    let block_type = |p: BlockPos| {
        let state = level.block(p);
        if state.is_air() || state.is(vanillite_data::block::ids::LILY_PAD) {
            return Layer::Above;
        }
        let fluid = state.fluid_state();
        let source = !fluid.is_empty() && fluid.info().amount == 8 && !fluid.info().falling;
        if crate::mob::is_water(state) && source && state.collision_shape().is_empty() {
            Layer::Inside
        } else {
            Layer::Invalid
        }
    };
    let mut previous = Layer::Invalid;
    for y in -1..=2 {
        let mut layer: Option<Layer> = None;
        for dx in -2..=2 {
            for dz in -2..=2 {
                let t = block_type(BlockPos::new(pos.x + dx, pos.y + y, pos.z + dz));
                layer = Some(match layer {
                    None => t,
                    Some(l) if l == t => l,
                    Some(_) => Layer::Invalid,
                });
            }
        }
        match layer.unwrap_or(Layer::Invalid) {
            Layer::Above if previous == Layer::Invalid => return false,
            Layer::Inside if previous == Layer::Above => return false,
            Layer::Invalid => return false,
            l => previous = l,
        }
    }
    true
}

/// `FishingHook.retrieve(rod)`: pulls a hooked entity, or catches the loot of a biting fish;
/// returns the rod damage.
#[allow(clippy::too_many_arguments)]
fn retrieve(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    network_ids: &NetworkIds,
    bobber: EntityId,
    owner: EntityId,
    rod: &ItemStack,
    hand: Hand,
) -> i32 {
    let _ = (hand, cx);
    if should_stop(ents, bobber, owner) {
        discard(ents, bobber);
        return 0;
    }
    let Some(d) = ents.missile(bobber).cloned() else {
        return 0;
    };
    let Some(b) = d.bobber.as_deref().cloned() else {
        return 0;
    };
    let Some((bpos, on_ground, network_id)) = ents.store.get(bobber).map(|e| {
        (
            e.pos(),
            e.flags().contains(EntityFlags::ON_GROUND),
            e.network_id().0,
        )
    }) else {
        return 0;
    };
    let opos = ents.store.get(owner).map_or(bpos, |e| e.pos());
    let state = if b.hooked.is_some() {
        "caught_entity"
    } else if b.nibble > 0 {
        "caught_fish"
    } else if on_ground {
        "in_ground"
    } else {
        "reel_in"
    };
    let uuid = ents.store.get(owner).map_or(0, |e| e.uuid());
    if !env
        .hooks
        .player_fish(crate::hooks::HookWorld::split(level, ents), uuid, state)
    {
        return 0;
    }
    let mut damage = 0;
    if let Some(hooked) = b.hooked {
        // `pullEntity`: only entities the server moves.
        let simulated = ents
            .player(hooked)
            .is_none_or(|p| p.survival.simulate_movement);
        if simulated && let Some(mut e) = ents.store.get_mut(hooked) {
            let v = e.as_ref().velocity();
            e.set_velocity(v + (opos - bpos) * 0.1);
        }
        let view = crate::advancements::view::snapshot(ents, hooked);
        let hooked_item = ents.item(hooked).map(|i| i.stack.clone());
        if let Some(view) = view
            && let Some(p) = ents.player_mut(owner)
        {
            p.advancements
                .trigger(crate::advancements::trigger::Event::FishingRodHooked(
                    rod.clone(),
                    view,
                    hooked_item,
                    Vec::new(),
                ));
        }
        env.out.push(crate::survival::Effect::Tracking {
            entity: bobber,
            with_self: false,
            packet: vanillite_protocol::SharedPacket::of(&EntityEvent {
                entity_id: network_id,
                event: 31,
            }),
        });
        damage = if ents.item(hooked).is_some() { 3 } else { 5 };
    } else if b.nibble > 0 {
        let items = fishing_loot(level, ents, env.types, bobber, owner, rod, &b, bpos);
        if let Some(view) = crate::advancements::view::snapshot(ents, bobber)
            && let Some(p) = ents.player_mut(owner)
        {
            p.advancements
                .trigger(crate::advancements::trigger::Event::FishingRodHooked(
                    rod.clone(),
                    view,
                    None,
                    items.clone(),
                ));
        }
        let mut own = LegacyRandom::from_state(d.random_state);
        for stack in items {
            let (uuid, y_rot, _, item_random) = crate::mob::new_item_entity();
            let (xa, ya, za) = (opos.x - bpos.x, opos.y - bpos.y, opos.z - bpos.z);
            let velocity = Vec3::new(
                xa * 0.1,
                ya * 0.1 + (xa * xa + ya * ya + za * za).sqrt().sqrt() * 0.08,
                za * 0.1,
            );
            let fish = crate::player::play::game_data()
                .is_some_and(|data| data.item_in_tag(stack.item, "minecraft:fishes"));
            env.out.push(crate::survival::Effect::DropItem {
                random: item_random,
                uuid,
                pos: bpos,
                velocity,
                y_rot,
                pickup_delay: 0,
                stack,
            });
            let amount = own.next_int_bounded(6) + 1;
            crate::experience_orb::spawn(
                ents,
                network_ids,
                Vec3::new(opos.x, opos.y + 0.5, opos.z + 0.5),
                Vec3::ZERO,
                amount,
            );
            if fish && let Some(p) = ents.player_mut(owner) {
                p.stats.award(crate::stats::custom::FISH_CAUGHT, 1);
            }
        }
        if let Some(m) = ents.missile_mut(bobber) {
            m.random_state = own.state();
        }
        damage = 1;
    }
    if on_ground {
        damage = 2;
    }
    discard(ents, bobber);
    damage
}

/// The `gameplay/fishing` loot of a catch (`LootContextParamSets.FISHING`).
#[allow(clippy::too_many_arguments)]
fn fishing_loot(
    level: &Level,
    ents: &Entities,
    types: &crate::survival::damage::DamageTypes,
    bobber: EntityId,
    owner: EntityId,
    rod: &ItemStack,
    b: &Bobber,
    origin: Vec3,
) -> Vec<ItemStack> {
    let Some(data) = crate::player::play::game_data() else {
        return Vec::new();
    };
    let table = "minecraft:gameplay/fishing";
    let luck_attr =
        crate::mob::attack::player_attribute(ents, owner, "minecraft:luck", 0.0, -1024.0, 1024.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let luck = b.luck as f32 + luck_attr as f32;
    let at = BlockPos::containing(origin.x, origin.y, origin.z);
    let biome: Option<String> = level
        .climate()
        .map(|c| c.biomes.get(level.biome(at)).name.clone());
    // Biome tags are not resolved here (the fishing tables list plain biomes).
    let biome_matches = |entry: &str| biome.as_deref() == Some(entry);
    let item_in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let holder_set = |registry: &str, entry: &str| data.holder_set(registry, entry);
    let enchant_level = |name: &str| data.enchantment_level(rod, name);
    let this = crate::predicate::EntityFacts {
        kind: Some(vanillite_data::entity::ids::FISHING_BOBBER),
        fishing_hook: Some(b.open_water),
        ..Default::default()
    };
    let none_level = |_: &str| 0;
    let entity = crate::loot::EntityLootContext {
        this,
        attacker: None,
        direct_attacker: None,
        last_damage_player: None,
        source: None,
        types,
        tags: &*data,
        attacker_enchantment_level: &none_level,
    };
    let ctx = crate::loot::LootContext {
        origin: Some(origin),
        this_entity: Some(bobber),
        tool: Some(rod),
        luck,
        enchantment_level: &enchant_level,
        item_in_tag: &item_in_tag,
        enchanting: Some(&data.enchanting),
        holder_set: &holder_set,
        biome: Some(&biome_matches),
        this_present: true,
        entity: Some(&entity),
        ..crate::loot::LootContext::empty()
    };
    let mut out = Vec::new();
    match data.loot.random_sequence(table) {
        Some(key) => level.with_random_sequence(key, |r| data.loot.roll(table, &ctx, r, &mut out)),
        None => data
            .loot
            .roll(table, &ctx, &mut LegacyRandom::new(0), &mut out),
    }
    out
}

/// The bobber has no NBT of its own (`addAdditionalSaveData` is empty).
pub fn save(_d: &MissileData, _out: &mut Compound) {}

/// A loaded bobber has no owner and goes on its first tick.
pub fn load(_d: &mut MissileData, _tag: &Compound) {}

/// The synched data: the hooked entity (8, network id plus one) and biting (9).
pub fn encode(w: &mut DataWriter<'_>, d: &MissileData) {
    let (hooked, biting) = d
        .bobber
        .as_deref()
        .map_or((0, false), |b| (b.hooked_network, b.biting));
    w.raw(
        8,
        crate::player::encode::serializer::INT,
        hooked == 0,
        |o| vanillite_protocol::codec::write_varint(o, hooked),
    );
    w.boolean(9, biting, false);
}

/// The bobber of a player that leaves or dies goes with it.
pub fn drop_player_bobber(ents: &mut Entities, player: EntityId) {
    if let Some(b) = ents.player(player).and_then(|p| p.survival.fishing) {
        discard(ents, b);
    }
}
