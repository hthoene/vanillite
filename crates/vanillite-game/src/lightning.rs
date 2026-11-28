//! Lightning: `ServerLevel.tickThunder` (where a strike lands) and the `LightningBolt` entity
//! (fire, lightning rods, copper, and the entities it hits).

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, ChunkPos, Vec3};
use vanillite_chunk::HeightmapKind;
use vanillite_data::entity::ids;
use vanillite_world::entity::{EntityId, NewEntity};
use vanillite_worldgen::behaviour::PosExt;

use crate::block::redstone::{is_lightning_rod, strike_lightning_rod};
use crate::block::{fire, place, weather};
use crate::entities::Entities;
use crate::level::Level;
use crate::level::game_event::{GameEventContext, events};
use crate::mob::{Kind, MobContext, combat};
use crate::survival::damage::DamageSource;
use crate::survival::player::{self, Env};
use crate::survival::rules::Difficulty;

/// `ServerLevel.tickThunder`: one strike in `1 / THUNDER_CHANCE` per spawning chunk and tick.
const THUNDER_CHANCE: i32 = 100_000;

/// `findLightningRod`: the search radius of the `lightning_rod` points of interest.
const ROD_RADIUS: i32 = 128;

/// `DimensionType.MOON_BRIGHTNESS_PER_PHASE`.
const MOON_BRIGHTNESS: [f32; 8] = [1.0, 0.75, 0.5, 0.25, 0.0, 0.25, 0.5, 0.75];

/// The kind data of a `lightning_bolt` (`LightningBolt`).
#[derive(Clone, Debug)]
pub struct LightningData {
    /// Ticks of the current flash left (`life`).
    pub life: i32,
    /// The seed of the bolt's shape on clients (`seed`).
    pub seed: i64,
    /// Flashes left after the current one.
    pub flashes: i32,
    /// Neither fire nor damage (the bolt of a skeleton trap).
    pub visual_only: bool,
    /// The entity's own random (`Entity.random`).
    pub random: LegacyRandom,
    /// `blocksSetOnFire`.
    pub blocks_set_on_fire: i32,
    /// `hitEntities`: every entity a flash struck.
    pub hit_entities: Vec<EntityId>,
    /// `cause`: the player whose Channeling summoned the bolt.
    pub cause: Option<EntityId>,
}

/// What a strike needs to know about the world besides the level.
#[derive(Clone, Copy, Debug)]
pub struct StrikeInput {
    /// The `spawn_mobs` game rule: strikes may be skeleton traps.
    pub spawn_mobs: bool,
    /// `getOverworldClockTime`: the overworld clock's ticks (local difficulty, moon phase).
    pub clock_time: i64,
}

/// `ServerLevel.tickThunder(chunk)` of one spawning chunk in entity ticking range.
pub fn tick_thunder(level: &mut Level, chunk: ChunkPos, input: StrikeInput) -> Option<EntityId> {
    let raining = level.is_raining();
    if !(raining && level.is_thundering() && level.random().next_int_bounded(THUNDER_CHANCE) == 0) {
        return None;
    }
    let column = level.block_random_pos(chunk.x * 16, 0, chunk.z * 16, 15);
    strike(level, column, input)
}

/// The strike of `tickThunder` in the column of `column` after its chance roll: the target
/// (`findLightningTargetAround`), `isRainingAt`, the skeleton trap and the bolt.
pub fn strike(level: &mut Level, column: BlockPos, input: StrikeInput) -> Option<EntityId> {
    let pos = find_target_around(level, column);
    if !level.is_raining_at(pos) {
        return None;
    }
    let trap = input.spawn_mobs
        && level.random().next_double()
            < f64::from(effective_difficulty(level, pos, input.clock_time)) * 0.01
        && !is_lightning_rod(level.block(pos.below()));
    if trap {
        // `SkeletonHorse` (`setTrap(true)`, `setAge(0)`) at the block corner, without
        // `finalizeSpawn`: its `SkeletonTrapGoal` springs when a player comes near.
        let mut horse =
            crate::mob::MobData::new(Kind::SkeletonHorse, crate::mob::new_entity_seed());
        crate::mob::fam::companion::st_mut(&mut horse).trap = true;
        horse.age = 0;
        horse.baby = false;
        let uuid = crate::mob::new_uuid();
        let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
        let network_ids = std::sync::Arc::clone(level.network_ids_arc());
        let rot = vanillite_world::entity::Rotation::default();
        crate::mob::spawn(&mut level.entities, &network_ids, uuid, at, rot, horse);
    }
    let at = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    );
    let id = spawn(level, at, trap);
    level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0)
        .then_some(id)
}

/// Adds a lightning bolt at `pos` (`EntityTypes.LIGHTNING_BOLT.create`): it draws its seed and
/// flashes from its own random.
pub fn spawn(level: &mut Level, pos: Vec3, visual_only: bool) -> EntityId {
    let mut random = level.new_entity_random();
    let seed = random.next_long();
    let flashes = random.next_int_bounded(3) + 1;
    // The bolt's random took the entity's seed: the UUID takes none.
    let uuid = crate::mob::new_uuid();
    let network_id = level.network_ids().next();
    level.entities.spawn_bolt(
        NewEntity::new(ids::LIGHTNING_BOLT, network_id, uuid, pos),
        LightningData {
            life: 2,
            seed,
            flashes,
            visual_only,
            random,
            blocks_set_on_fire: 0,
            hit_entities: Vec::new(),
            cause: None,
        },
    )
}

/// `ServerLevel.findLightningTargetAround`: the nearest lightning rod on top of its column, else
/// a random living entity under the open sky above the column, else the column's top.
pub fn find_target_around(level: &mut Level, column: BlockPos) -> BlockPos {
    let mut center = BlockPos::new(
        column.x,
        level.heightmap_y(HeightmapKind::MotionBlocking, column),
        column.z,
    );
    if let Some(rod) = find_lightning_rod(level, center) {
        return rod.above();
    }
    // `AABB.encapsulatingFullBlocks(center, center.atY(maxY + 1)).inflate(3)`.
    let area = Aabb::new(
        f64::from(center.x) - 3.0,
        f64::from(center.y) - 3.0,
        f64::from(center.z) - 3.0,
        f64::from(center.x) + 4.0,
        f64::from(level.max_y()) + 2.0 + 3.0,
        f64::from(center.z) + 4.0,
    );
    let mut found = Vec::new();
    level
        .entities
        .store
        .for_each_in(&area, false, |id| found.push(id));
    found.retain(|&id| {
        living_alive(&level.entities, id)
            && level
                .entities
                .store
                .get(id)
                .is_some_and(|e| level.light(block_position(e.pos())).0 >= 15)
    });
    if !found.is_empty() {
        let pick = level
            .random()
            .next_int_bounded(i32::try_from(found.len()).unwrap_or(i32::MAX));
        let id = found[usize::try_from(pick).unwrap_or(0)];
        if let Some(e) = level.entities.store.get(id) {
            return block_position(e.pos());
        }
    }
    if center.y == level.min_y() - 1 {
        center = center.above().above();
    }
    center
}

/// `findLightningRod`: the closest lightning rod point of interest within 128 blocks that is
/// the top block of its column (`WORLD_SURFACE`). A rod can only pass that test on top of its
/// column, so the columns' tops of the loaded chunks are searched; ties go to the first in
/// Vanilla's chunk order (`ChunkPos.rangeClosed`: x fastest).
fn find_lightning_rod(level: &Level, center: BlockPos) -> Option<BlockPos> {
    let chunk_radius = ROD_RADIUS.div_euclid(16) + 1;
    let c = center.chunk();
    let mut best: Option<(i64, BlockPos)> = None;
    for cz in c.z - chunk_radius..=c.z + chunk_radius {
        for cx in c.x - chunk_radius..=c.x + chunk_radius {
            let Some(chunk) = level.chunks.chunk(ChunkPos::new(cx, cz)) else {
                continue;
            };
            let Some(heights) = chunk.chunk.heightmaps().get(HeightmapKind::WorldSurface) else {
                continue;
            };
            let min_y = chunk.chunk.min_y();
            for z in 0..16u32 {
                for x in 0..16u32 {
                    #[allow(clippy::cast_possible_wrap, clippy::as_conversions)]
                    let pos = BlockPos::new(
                        cx * 16 + x as i32,
                        heights.get(x, z) as i32 + min_y - 1,
                        cz * 16 + z as i32,
                    );
                    let (dx, dy, dz) = (
                        i64::from(pos.x - center.x),
                        i64::from(pos.y - center.y),
                        i64::from(pos.z - center.z),
                    );
                    let radius = i64::from(ROD_RADIUS);
                    if dx.abs() > radius || dz.abs() > radius {
                        continue;
                    }
                    let distance = dx * dx + dy * dy + dz * dz;
                    if distance > radius * radius
                        || best.is_some_and(|(d, _)| d <= distance)
                        || !is_lightning_rod(level.block(pos))
                    {
                        continue;
                    }
                    best = Some((distance, pos));
                }
            }
        }
    }
    best.map(|(_, pos)| pos)
}

/// `getCurrentDifficultyAt(pos).getEffectiveDifficulty()`.
fn effective_difficulty(level: &Level, pos: BlockPos, clock_time: i64) -> f32 {
    let (inhabited, moon) = level
        .chunks
        .inhabited_time(pos.chunk())
        .map_or((0, 0.0), |t| {
            let phase = usize::try_from(clock_time.rem_euclid(8 * 24_000) / 24_000).unwrap_or(0);
            (t, MOON_BRIGHTNESS.get(phase).copied().unwrap_or(1.0))
        });
    crate::level::difficulty::DifficultyInstance::new(
        level.settings.difficulty,
        clock_time,
        inhabited,
        moon,
    )
    .effective
}

/// `Entity.blockPosition()`.
fn block_position(p: Vec3) -> BlockPos {
    BlockPos::containing(p.x, p.y, p.z)
}

/// `instanceof LivingEntity && isAlive()`.
fn living_alive(ents: &Entities, id: EntityId) -> bool {
    if let Some(p) = ents.player(id) {
        return p.survival.health > 0.0;
    }
    ents.mob(id).is_some_and(|m| m.is_alive())
}

/// `isAlive()` of the entities lightning can hit.
pub(crate) fn alive(ents: &Entities, id: EntityId) -> bool {
    if let Some(item) = ents.item(id) {
        return item.health > 0;
    }
    living_alive(ents, id)
        || ents.experience_orb(id).is_some()
        || ents.boat(id).is_some()
        || ents.minecart(id).is_some()
        || ents.decor(id).is_some()
}

/// `LightningBolt.tick`. Fills `hits` with the entities to strike (`thunderHit`, see
/// [`thunder_hit`]) in Vanilla's order; the bolt removes itself when its last flash ends.
pub fn tick(level: &mut Level, id: EntityId, hits: &mut Vec<EntityId>) {
    hits.clear();
    let Some(pos) = level.entities.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let Some(first) = level.entities.bolt_mut(id).map(|b| b.life == 2) else {
        return;
    };
    if first {
        if matches!(
            level.settings.difficulty,
            Difficulty::Normal | Difficulty::Hard
        ) {
            spawn_fire(level, id, pos, 4);
        }
        // `getStrikePosition`: the block the bolt stands on.
        let strike = BlockPos::containing(pos.x, pos.y - 1.0e-6, pos.z);
        strike_lightning_rod(level, strike);
        clear_copper(level, strike);
        level.game_event(
            events().lightning_strike,
            pos,
            GameEventContext::of(Some(id)),
        );
    }
    let Some(bolt) = level.entities.bolt_mut(id) else {
        return;
    };
    bolt.life -= 1;
    if bolt.life < 0 {
        if bolt.flashes == 0 {
            strike_triggers(&mut level.entities, id, pos);
            level.entities.despawn(id);
            return;
        }
        if bolt.life < -bolt.random.next_int_bounded(10) {
            bolt.flashes -= 1;
            bolt.life = 1;
            bolt.seed = bolt.random.next_long();
            spawn_fire(level, id, pos, 0);
        }
    }
    let Some(bolt) = level.entities.bolt_mut(id) else {
        return;
    };
    if bolt.life < 0 || bolt.visual_only {
        return;
    }
    let area = Aabb::new(
        pos.x - 3.0,
        pos.y - 3.0,
        pos.z - 3.0,
        pos.x + 3.0,
        pos.y + 6.0 + 3.0,
        pos.z + 3.0,
    );
    let ents = &level.entities;
    ents.store.for_each_in(&area, false, |other| {
        if other != id && alive(ents, other) {
            hits.push(other);
        }
    });
    let mut cause = None;
    if let Some(bolt) = level.entities.bolt_mut(id) {
        for &h in hits.iter() {
            if !bolt.hit_entities.contains(&h) {
                bolt.hit_entities.push(h);
            }
        }
        cause = bolt.cause;
    }
    // `CHANNELED_LIGHTNING` of the player who caused the bolt, with the entities of this flash.
    if let Some(player) = cause {
        let kind = crate::advancements::trigger::TriggerKind::CHANNELED_LIGHTNING;
        crate::advancements::view::fire(&mut level.entities, player, kind, |ents| {
            crate::advancements::trigger::Event::ChanneledLightning(
                crate::advancements::view::snapshots(ents, hits.as_slice()),
            )
        });
    }
}

/// The `lightning_strike` trigger when the bolt goes: the players closer than 256 blocks, with
/// the alive entities around that no flash struck.
fn strike_triggers(ents: &mut Entities, id: EntityId, pos: Vec3) {
    use crate::advancements::trigger::{Event, TriggerKind};
    use crate::advancements::view;
    let kind = TriggerKind::LIGHTNING_STRIKE;
    let players: Vec<EntityId> = ents
        .players
        .iter()
        .map(|(p, _)| p)
        .filter(|&p| {
            view::listens(ents, p, kind)
                && ents
                    .store
                    .get(p)
                    .is_some_and(|e| e.pos().distance_to_sqr(pos) < 256.0 * 256.0)
        })
        .collect();
    if players.is_empty() {
        return;
    }
    let area = Aabb::new(
        pos.x - 15.0,
        pos.y - 15.0,
        pos.z - 15.0,
        pos.x + 15.0,
        pos.y + 6.0 + 15.0,
        pos.z + 15.0,
    );
    let hit = ents
        .bolt(id)
        .map(|b| b.hit_entities.clone())
        .unwrap_or_default();
    let mut viewers = Vec::new();
    ents.store.for_each_in(&area, false, |other| {
        if other != id && alive(ents, other) && !hit.contains(&other) {
            viewers.push(other);
        }
    });
    let Some(bolt) = view::snapshot(ents, id) else {
        return;
    };
    let around = view::snapshots(ents, &viewers);
    for p in players {
        if let Some(p) = ents.player_mut(p) {
            p.advancements
                .trigger(Event::LightningStrike(bolt.clone(), around.clone()));
        }
    }
}

/// `LightningBolt.spawnFire(additionalSources)`: fire at the bolt and at random blocks around
/// it, drawn from the bolt's random.
fn spawn_fire(level: &mut Level, id: EntityId, pos: Vec3, additional: i32) {
    let Some(mut random) = level
        .entities
        .bolt_mut(id)
        .filter(|b| !b.visual_only)
        .map(|b| b.random.clone())
    else {
        return;
    };
    let at = block_position(pos);
    if !fire::can_spread_fire_around(level, at) {
        return;
    }
    let bolt = level.entities.store.get(id).map_or(0, |e| e.uuid());
    let place_fire = |level: &mut Level, target: BlockPos| {
        let state = place::fire_state(&*level, target);
        if level.block(target).is_air()
            && level.can_survive(state, target)
            && crate::block::hook_events::may_ignite(
                level,
                target,
                vanillite_abi::ignite_cause::LIGHTNING,
                bolt,
                0,
            )
        {
            level.set_block_and_update(target, state);
            return 1;
        }
        0
    };
    let mut set = place_fire(level, at);
    for _ in 0..additional {
        let x = random.next_int_bounded(3) - 1;
        let y = random.next_int_bounded(3) - 1;
        let z = random.next_int_bounded(3) - 1;
        set += place_fire(level, at.offset(x, y, z));
    }
    if let Some(bolt) = level.entities.bolt_mut(id) {
        bolt.random = random;
        bolt.blocks_set_on_fire += set;
    }
}

/// `LightningBolt.clearCopperOnLightningStrike`: struck copper loses its oxidation, and random
/// walks from it (level random) scrape one stage off the copper they reach.
fn clear_copper(level: &mut Level, struck: BlockPos) {
    let block = level.block(struck).block();
    let weathering = weather::is_weathering_copper(block);
    if !weathering && !weather::is_waxed_copper(block) {
        return;
    }
    if weathering && let Some(first) = weather::copper_first(level.block(struck)) {
        level.set_block_and_update(struck, first);
    }
    let strikes = level.random().next_int_bounded(3) + 3;
    for _ in 0..strikes {
        let steps = level.random().next_int_bounded(8) + 1;
        let mut work = struck;
        for _ in 0..steps {
            match random_step_cleaning_copper(level, work) {
                Some(next) => work = next,
                None => break,
            }
        }
    }
}

/// `randomStepCleaningCopper`: up to 10 random blocks of the cube around `pos`
/// (`BlockPos.randomInCube`); the first copper one goes back one stage.
fn random_step_cleaning_copper(level: &mut Level, pos: BlockPos) -> Option<BlockPos> {
    for _ in 0..10 {
        let x = level.random().next_int_bounded(3) - 1;
        let y = level.random().next_int_bounded(3) - 1;
        let z = level.random().next_int_bounded(3) - 1;
        let candidate = pos.offset(x, y, z);
        let state = level.block(candidate);
        if weather::is_weathering_copper(state.block()) {
            if let Some(previous) = weather::copper_previous(state) {
                level.set_block_and_update(candidate, previous);
            }
            level.level_event(None, 3002, candidate, -1);
            return Some(candidate);
        }
    }
    None
}

/// `Entity.thunderHit` and its overrides: one more fire tick (eight seconds of fire when that
/// reached zero) and 5 lightning damage; creepers become charged.
///
/// Pigs and villagers convert (zombified piglin, witch); mooshrooms, turtles and copper golems
/// react on their own. `bolt` is the striking bolt (a copper golem loses one oxidation stage
/// per bolt, not per flash).
pub fn thunder_hit(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    bolt: EntityId,
) {
    if ents.player(target).is_some() {
        let Entities { store, players, .. } = ents;
        if let Some(mut entity) = store.get_mut(target) {
            let row = entity.as_ref().kind_row();
            if let Some(data) = players.get_mut(row) {
                player::thunder_hit(&mut entity, data, env);
            }
        }
        return;
    }
    let source = DamageSource::of(env.types.lightning_bolt);
    let invulnerable = crate::mob::entity_nbt::is_invulnerable(ents, target);
    if let Some(d) = ents.item_mut(target) {
        // One more fire tick; a fresh item (-1 in Vanilla, kept as 0 here) burns 8 seconds.
        if d.fire_ticks <= 0 {
            d.fire_ticks = 160;
        } else {
            d.fire_ticks += 1;
        }
        // `hurtServer(lightningBolt, 5)`: `Invulnerable` and `damage_resistant` keep it off.
        if invulnerable
            || !crate::item::can_be_hurt_by(&d.stack, env.types, env.types.lightning_bolt)
        {
            return;
        }
        d.health -= 5;
        let destroyed = d.health <= 0;
        let contents = if destroyed {
            crate::item_entity::take_destroyed_contents(&mut d.stack)
        } else {
            Vec::new()
        };
        let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
            return;
        };
        crate::survival::game_event(env.out, events().entity_damage, pos, None);
        if destroyed {
            // `Item.onDestroyed`: a bundle's or a shulker box's contents drop.
            for stack in contents {
                let (uuid, y_rot, velocity, random) = crate::mob::new_item_entity();
                env.out.push(crate::survival::Effect::DropItem {
                    random,
                    uuid,
                    pos,
                    velocity,
                    y_rot,
                    pickup_delay: 0,
                    stack,
                });
            }
            cx.discarded.push(target);
        }
        return;
    }
    if let Some(d) = ents.experience_orb_mut(target) {
        // `ExperienceOrb.hurtServer`: the bolt's 5 damage is an orb's whole health.
        d.health -= 5;
        if d.health <= 0 {
            cx.discarded.push(target);
        }
        return;
    }
    if ents.minecart(target).is_some() {
        crate::minecart::hurt(ents, env, cx, target, source, 5.0);
        return;
    }
    if ents.boat(target).is_some() {
        crate::vehicle::hurt_boat(ents, env, cx, target, source, 5.0);
        return;
    }
    if ents.decor(target).is_some() {
        // `BlockAttachedEntity.thunderHit` does nothing, cushions break; markers, displays and
        // interactions take no damage (`Entity.thunderHit` ignites them only).
        crate::decoration::thunder_hit(level, ents, env, cx, target, None);
        return;
    }
    let Some(kind) = ents.mob(target).filter(|m| m.is_alive()).map(|m| m.kind) else {
        return;
    };
    // `MushroomCow.thunderHit`: a bolt converts a mooshroom only once over all its flashes.
    if kind == Kind::Mooshroom {
        let uuid = ents.store.get(bolt).map_or(0, |e| e.uuid());
        if !crate::mob::fam::wildlife::mooshroom::new_bolt(ents, target, uuid) {
            return;
        }
    }
    if kind.family() != crate::mob::Family::Classic {
        let mut h = crate::mob::fam::Hurt {
            level,
            ents,
            env,
            cx,
            target,
            source,
        };
        if crate::mob::fam::thunder_hit(&mut h, kind) {
            discard_converted(ents, cx, target);
            return;
        }
    }
    if kind == Kind::Pig && pig_struck(level, ents, env, cx, target) {
        discard_converted(ents, cx, target);
        return;
    }
    let Some(m) = ents.mob_mut(target) else {
        return;
    };
    m.fire_ticks += 1;
    if m.fire_ticks == 0 {
        m.fire_ticks = 160;
    }
    let creeper = m.kind == Kind::Creeper;
    if let Some(mut e) = ents.store.get_mut(target) {
        e.mark_data_dirty();
    }
    combat::hurt(level, ents, env, cx, target, source, 5.0);
    if creeper
        && let Some(c) = ents
            .mob_mut(target)
            .and_then(crate::mob::fam::classic::try_creeper_mut)
    {
        c.powered = true;
    }
    if kind == Kind::CopperGolem {
        let bolt = ents.store.get(bolt).map_or(0, |e| e.network_id().0);
        crate::mob::fam::mount::copper_golem::thunder_hit(ents, target, bolt);
    }
}

/// `convertTo` discards the struck mob at once (it does not wait for its next tick).
fn discard_converted(ents: &Entities, cx: &mut MobContext, target: EntityId) {
    if ents.mob(target).is_some_and(|m| m.removed) && !cx.discarded.contains(&target) {
        cx.discarded.push(target);
    }
}

/// `Pig.thunderHit`: a zombified piglin outside peaceful (with the equipment drawn from the
/// pig's random, persistent). `false`: the pig takes the plain hit.
fn pig_struck(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
) -> bool {
    if env.difficulty == Difficulty::Peaceful {
        return false;
    }
    let Some(mut random) = ents.mob_mut(target).map(|m| m.random.clone()) else {
        return false;
    };
    let mut t = crate::mob::Tick {
        level,
        ents,
        env,
        cx,
        id: target,
    };
    let new = crate::mob::fam::undead::convert_to(
        &mut t,
        Kind::ZombifiedPiglin,
        false,
        true,
        vanillite_abi::transform_reason::LIGHTNING,
        |z| {
            // `populateDefaultEquipmentSlots(pig.getRandom(), ...)`.
            let item = if random.next_int_bounded(20) == 0 {
                "minecraft:golden_spear"
            } else {
                "minecraft:golden_sword"
            };
            z.equipment.set(
                crate::item::EquipmentSlot::MainHand,
                crate::mob::equipment::stack(item),
            );
            z.persistent = true;
        },
    );
    new.is_some()
}
