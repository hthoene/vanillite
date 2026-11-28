//! Thrown splash and lingering potions (`ThrownSplashPotion`, `ThrownLingeringPotion`,
//! `AbstractThrownPotion`, `ThrowableItemProjectile`, `ThrowableProjectile`): throwing them
//! (`ThrowablePotionItem.use`), their flight and what they do when they land
//! ([`super::cloud::potion_hit`]). Their data lives in the special entity store
//! ([`crate::mob::boss::special::SpecialData::ThrownPotion`]).

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::codec::Encode;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{EntityFlags, EntityId, NetworkIds, NewEntity};
use vanillite_worldgen::behaviour::StateExt;

use super::potion::PotionContents;
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::mob::WorldAction;
use crate::mob::boss::special::{SpecialData, SpecialTick};
use crate::mob::physics::{self, Blocks};
use crate::survival::play_sound;
use crate::survival::player::Env;

/// The kind data of a thrown potion.
#[derive(Clone, Debug, PartialEq)]
pub struct ThrownPotion {
    /// The thrower (`Projectile.owner`).
    pub owner: Option<EntityId>,
    /// The thrower's UUID as stored (`Owner`), until it is found.
    pub owner_uuid: Option<u128>,
    /// The potion item (`DATA_ITEM_STACK`, count 1).
    pub stack: ItemStack,
    /// `leftOwner`.
    pub left_owner: bool,
    /// `tickCount`.
    pub tick_count: i32,
    /// `hasBeenShot`: the `PROJECTILE_SHOOT` game event was posted.
    pub has_been_shot: bool,
}

/// Whether `kind` is a thrown potion.
pub fn is_thrown_potion(kind: EntityTypeId) -> bool {
    kind == ids::SPLASH_POTION || kind == ids::LINGERING_POTION
}

/// The entity type a potion item is thrown as, if it is throwable.
pub fn kind_of(stack: &ItemStack) -> Option<EntityTypeId> {
    match vanillite_data::registry::ITEM.name(stack.item)? {
        "minecraft:splash_potion" => Some(ids::SPLASH_POTION),
        "minecraft:lingering_potion" => Some(ids::LINGERING_POTION),
        _ => None,
    }
}

/// `ThrowablePotionItem.asProjectile` at `at` (a dispenser, an ominous item spawner) and
/// `Projectile.spawnProjectileUsingShoot` along `dir`: a potion of one of `stack` without an
/// owner. Returns the potion entity.
pub fn spawn_shot(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    at: Vec3,
    stack: &ItemStack,
    dir: Vec3,
    power: f32,
    uncertainty: f32,
) -> Option<EntityId> {
    let kind = kind_of(stack)?;
    // The potion's own random (its constructor takes the next entity seed), then `shoot`
    // draws the spread from it.
    let mut own = vanillite_base::random::LegacyRandom::new(crate::mob::new_entity_seed());
    let (velocity, rot) = crate::mob::projectile::shoot(&mut own, dir, power, uncertainty);
    let mut new = NewEntity::new(kind, network_ids.next(), crate::mob::new_uuid(), at);
    new.velocity = velocity;
    new.rot = rot;
    new.flags = EntityFlags::HAS_IMPULSE;
    let mut potion = stack.clone();
    potion.count = 1;
    let id = ents.spawn_special(
        new,
        SpecialData::ThrownPotion(Box::new(ThrownPotion {
            owner: None,
            owner_uuid: None,
            stack: potion,
            left_owner: false,
            tick_count: 0,
            has_been_shot: false,
        })),
    );
    ents.launched(id, None).then_some(id)
}

/// `ThrowablePotionItem.use` (with the throw sound of `SplashPotionItem` /
/// `LingeringPotionItem`): throws a copy of `stack` from the eyes of `thrower` and shrinks the
/// stack unless the thrower has infinite materials. Returns the potion entity.
pub fn throw(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &NetworkIds,
    thrower: EntityId,
    stack: &mut ItemStack,
    infinite: bool,
) -> Option<EntityId> {
    let kind = kind_of(stack)?;
    let known_movement = crate::spear::known_movement(ents, thrower);
    let e = ents.store.get(thrower)?;
    let (pos, rot, flags) = (e.pos(), e.rot(), e.flags());
    let eye = f64::from(e.dims().height / 1.8 * 1.62);
    let (sound, source) = if kind == ids::SPLASH_POTION {
        ("minecraft:entity.splash_potion.throw", SoundSource::Players)
    } else {
        (
            "minecraft:entity.lingering_potion.throw",
            SoundSource::Neutral,
        )
    };
    let pitch = 0.4 / (env.random.next_float() * 0.4 + 0.8);
    let seed = env.random.next_long();
    play_sound(env.out, None, pos, sound, source, 0.5, pitch, seed);
    // `shootFromRotation(source, xRot, yRot, -20, 0.5, 1)`.
    let rad = std::f32::consts::PI / 180.0;
    let (x_rot, y_rot) = (rot.x_rot, rot.y_rot);
    let sin = |a: f32| vanillite_base::math::sin(f64::from(a));
    let cos = |a: f32| vanillite_base::math::cos(f64::from(a));
    let dir = Vec3::new(
        f64::from(-sin(y_rot * rad) * cos(x_rot * rad)),
        f64::from(-sin((x_rot - 20.0) * rad)),
        f64::from(cos(y_rot * rad) * cos(x_rot * rad)),
    );
    let (velocity, rot, uuid) = crate::experience_orb::with_entity_random(|r| {
        let (v, rot) = crate::mob::projectile::shoot(r, dir, 0.5, 1.0);
        (v, rot, crate::mob::random_uuid(r))
    });
    let on_ground = flags.contains(EntityFlags::ON_GROUND);
    let velocity = velocity
        + Vec3::new(
            known_movement.x,
            if on_ground { 0.0 } else { known_movement.y },
            known_movement.z,
        );
    let start = Vec3::new(pos.x, pos.y + eye - f64::from(0.1_f32), pos.z);
    let mut new = NewEntity::new(kind, network_ids.next(), uuid, start);
    new.velocity = velocity;
    new.rot = rot;
    new.flags = EntityFlags::HAS_IMPULSE;
    let mut potion = stack.clone();
    potion.count = 1;
    let owner_uuid = ents.store.get(thrower).map(|e| e.uuid());
    let id = ents.spawn_special(
        new,
        SpecialData::ThrownPotion(Box::new(ThrownPotion {
            owner: Some(thrower),
            owner_uuid,
            stack: potion,
            left_owner: false,
            tick_count: 0,
            has_been_shot: false,
        })),
    );
    if !ents.launched(id, Some(thrower)) {
        return None;
    }
    if !infinite {
        stack.shrink(1);
    }
    Some(id)
}

fn thrown_mut(ents: &mut Entities, id: EntityId) -> Option<&mut ThrownPotion> {
    match ents.special_mut(id)? {
        SpecialData::ThrownPotion(p) => Some(p),
        _ => None,
    }
}

/// `Entity.canBeHitByProjectile`: alive and pickable (living entities but spectators,
/// vehicles, end crystals, item frames, paintings and cushions).
pub fn can_be_hit(ents: &Entities, id: EntityId) -> bool {
    if let Some(m) = ents.mob(id) {
        return m.is_alive();
    }
    if let Some(p) = ents.player(id) {
        return !p.survival.is_dead_or_dying()
            && p.game_mode != vanillite_config::GameMode::Spectator;
    }
    if ents.boat(id).is_some() || ents.minecart(id).is_some() {
        return true;
    }
    if crate::decoration::pickable(ents, id) {
        return true;
    }
    matches!(ents.special(id), Some(SpecialData::EndCrystal(c)) if !c.removed)
}

/// Whether `id` is a living entity (mob or player).
pub fn is_living(ents: &Entities, id: EntityId) -> bool {
    ents.mob(id).is_some() || ents.player(id).is_some()
}

/// `ProjectileUtil.computeMargin`.
pub fn margin(tick_count: i32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let m = (tick_count - 2) as f32 / 20.0;
    m.clamp(0.0, 0.3)
}

/// `ThrowableProjectile.tick` of a thrown potion.
pub fn tick(t: &mut SpecialTick<'_, '_>) {
    let id = t.id;
    let Some(e) = t.ents.store.get(id) else {
        return;
    };
    let (pos, aabb, mut v, mut rot) = (e.pos(), e.aabb(), e.velocity(), e.rot());
    // `getOwner()`: the owner by its UUID (a loaded potion, a respawned or relogged thrower).
    let found = match t.ents.special(id) {
        Some(SpecialData::ThrownPotion(p)) => {
            crate::mob::missile::resolve_owner(t.ents, p.owner, p.owner_uuid)
        }
        _ => None,
    };
    let Some(p) = thrown_mut(t.ents, id) else {
        return;
    };
    p.owner = found;
    p.tick_count = p.tick_count.saturating_add(1);
    // `Projectile.tick`: the first tick posts `PROJECTILE_SHOOT` from the thrower.
    if !std::mem::replace(&mut p.has_been_shot, true) {
        let owner = p.owner;
        crate::mob::missile::shoot_event(t.env.out, pos, owner);
    }
    let Some(p) = thrown_mut(t.ents, id) else {
        return;
    };
    let (owner, left_owner, tick_count) = (p.owner, p.left_owner, p.tick_count);
    if !t.level.is_loaded(BlockPos::containing(pos.x, pos.y, pos.z)) {
        return;
    }
    let drag = crate::mob::projectile::BubbleDrag::Projectile;
    // `handleFirstTickBubbleColumn`.
    if tick_count == 1 {
        crate::mob::projectile::bubble_columns(t.level, t.ents, t.env.random, t.cx, id, None, drag);
        v = t.ents.store.get(id).map_or(v, |e| e.velocity());
    }
    // `applyGravity`, `applyInertia`.
    v = v + Vec3::new(0.0, -0.05, 0.0);
    // `isInWater()`: water reaches into the box (where the last tick's `baseTick` saw it).
    let in_water = crate::experience_orb::touching_water(t.level, &aabb);
    v = v * f64::from(if in_water { 0.8_f32 } else { 0.99_f32 });
    // `ProjectileUtil.getHitResultOnMoveVector`.
    let to = pos + v;
    let block_hit = physics::clip(t.level as &dyn Blocks, pos, to);
    let end = block_hit.map_or(to, |(_, p)| p);
    let m = margin(tick_count);
    let area = aabb.expand_towards(v.x, v.y, v.z).inflate(1.0, 1.0, 1.0);
    let mut candidates = Vec::new();
    t.ents.store.for_each_in(&area, false, |other| {
        if other != id {
            candidates.push(other);
        }
    });
    let mut best: Option<(EntityId, f64, Vec3)> = None;
    for other in candidates {
        // `canHitEntity`: the owner (and what rides with it) only after the potion left it.
        if !can_be_hit(t.ents, other) || (!left_owner && Some(other) == owner) {
            continue;
        }
        let Some(oe) = t.ents.store.get(other) else {
            continue;
        };
        let grown = oe.aabb().inflate(f64::from(m), f64::from(m), f64::from(m));
        if grown.contains(pos.x, pos.y, pos.z) {
            if best.is_none_or(|(_, d, _)| d >= 0.0) {
                best = Some((other, 0.0, pos));
            }
        } else if let Some(frac) = crate::mob::projectile::segment_box(pos, end, &grown) {
            let at = pos + (end - pos) * frac;
            let d = pos.distance_to_sqr(at);
            if best.is_none_or(|(_, bd, _)| d < bd || bd == 0.0) {
                best = Some((other, d, at));
            }
        }
    }
    let hit_entity = best.map(|(e, _, at)| (e, at));
    // Bukkit's `ProjectileHitEvent`: a cancelled hit is ignored, the potion flies on.
    let hit_event = |what| crate::mob::missile::hit_event(t.level, t.ents, id, owner, what);
    let hit_entity =
        hit_entity.filter(|&(target, _)| hit_event(crate::mob::missile::HitWhat::Entity(target)));
    let block_hit = block_hit.filter(|&(block, at)| {
        hit_entity.is_some() || hit_event(crate::mob::missile::HitWhat::Block(block, at))
    });
    let hit = hit_entity.map(|(_, at)| at).or(block_hit.map(|(_, at)| at));
    let new_pos = hit.unwrap_or(to);
    // `updateRotation`.
    let hd = (v.x * v.x + v.z * v.z).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    {
        rot.x_rot = lerp_rotation(rot.x_rot, crate::mob::atan2_deg(v.y, hd) as f32);
        rot.y_rot = lerp_rotation(rot.y_rot, crate::mob::atan2_deg(v.x, v.z) as f32);
    }
    let new_box = {
        let Some(mut e) = t.ents.store.get_mut(id) else {
            return;
        };
        e.set_pos(new_pos);
        e.set_velocity(v);
        e.set_rot(rot);
        e.as_ref().aabb()
    };
    // `applyEffectsFromBlocks`: the bubble columns on the way.
    crate::mob::projectile::bubble_columns(
        t.level,
        t.ents,
        t.env.random,
        t.cx,
        id,
        Some(pos),
        drag,
    );
    v = t.ents.store.get(id).map_or(v, |e| e.velocity());
    // `checkLeftOwner`.
    if !left_owner {
        let probe = new_box.expand_towards(v.x, v.y, v.z).inflate(1.0, 1.0, 1.0);
        let inside = owner.is_some_and(|o| {
            t.ents
                .store
                .get(o)
                .is_some_and(|oe| oe.aabb().intersects(&probe) && can_be_hit(t.ents, o))
        });
        if let Some(p) = thrown_mut(t.ents, id) {
            p.left_owner = !inside;
        }
    }
    if hit.is_none() {
        return;
    }
    let block = if hit_entity.is_none() {
        block_hit.map(|(b, at)| (b, hit_face(pos, at, b)))
    } else {
        None
    };
    on_hit(t, new_box, m, hit_entity.map(|(e, _)| e), block, new_pos);
}

/// The face of block `b` a segment from `from` entered it through at `at`.
fn hit_face(from: Vec3, at: Vec3, b: BlockPos) -> Direction {
    let local = at - Vec3::new(f64::from(b.x), f64::from(b.y), f64::from(b.z));
    let d = at - from;
    // The axis whose face the hit point lies on (closest to a face of the cube along the
    // movement).
    let faces = [
        (local.x.abs(), d.x > 0.0, Direction::West),
        ((1.0 - local.x).abs(), d.x < 0.0, Direction::East),
        (local.y.abs(), d.y > 0.0, Direction::Down),
        ((1.0 - local.y).abs(), d.y < 0.0, Direction::Up),
        (local.z.abs(), d.z > 0.0, Direction::North),
        ((1.0 - local.z).abs(), d.z < 0.0, Direction::South),
    ];
    faces
        .iter()
        .filter(|f| f.1)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(Direction::Up, |f| f.2)
}

/// `AbstractThrownPotion.onHit` (with `onHitBlock`'s fire dousing), then the potion is gone.
fn on_hit(
    t: &mut SpecialTick<'_, '_>,
    potion_box: Aabb,
    margin: f32,
    hit_entity: Option<EntityId>,
    block: Option<(BlockPos, Direction)>,
    at: Vec3,
) {
    let id = t.id;
    let Some(SpecialData::ThrownPotion(p)) = t.ents.special(id) else {
        return;
    };
    let (owner, stack) = (p.owner, p.stack.clone());
    // `super.onHitBlock`: the target block reacts first (applied after the tick).
    if let Some((pos, _)) = block {
        crate::mob::missile::hit_block(t.level, t.cx, id, pos, at);
    }
    // `super.onHit`: the land game event comes first.
    crate::mob::missile::land_event(t.env.out, t.level, id, owner, block.map(|(b, _)| b), at);
    if let Some((pos, face)) = block
        && PotionContents::of_stack(&stack).is_water()
    {
        // `douseFire` around the block in front of the hit face.
        let at = pos.relative(face);
        douse_fire(t, at, owner);
        douse_fire(t, at.relative(face.opposite()), owner);
        for d in [
            Direction::North,
            Direction::East,
            Direction::South,
            Direction::West,
        ] {
            douse_fire(t, at.relative(d), owner);
        }
    }
    super::cloud::potion_hit(
        t.level,
        t.ents,
        t.env,
        t.cx,
        Some(id),
        owner,
        &stack,
        potion_box,
        margin,
        hit_entity,
    );
    t.cx.discarded.push(id);
}

/// `AbstractThrownPotion.douseFire`: fire goes out, lit candles and campfires are put out.
fn douse_fire(t: &mut SpecialTick<'_, '_>, pos: BlockPos, owner: Option<EntityId>) {
    use vanillite_data::block::ids as block_ids;
    let state = t.level.block(pos);
    let blocks = &vanillite_worldgen::behaviour::tags().blocks;
    let lit = state.value("lit") == Some("true");
    if state.is(block_ids::FIRE) || state.is(block_ids::SOUL_FIRE) {
        t.cx.actions.push(WorldAction::DestroyBlock(pos, false));
    } else if lit && (blocks.candles.contains(state) || blocks.candle_cakes.contains(state)) {
        // `AbstractCandleBlock.extinguish`.
        if let Some(off) = state.with("lit", "false") {
            t.cx.actions
                .push(WorldAction::SetBlock(pos, off, crate::level::update::ALL));
        }
        let seed = t.env.random.next_long();
        play_sound(
            t.env.out,
            None,
            Vec3::new(
                f64::from(pos.x) + 0.5,
                f64::from(pos.y) + 0.5,
                f64::from(pos.z) + 0.5,
            ),
            "minecraft:block.candle.extinguish",
            SoundSource::Blocks,
            1.0,
            1.0,
            seed,
        );
    } else if lit && blocks.campfires.contains(state) {
        // The extinguish level event, `CampfireBlock.douse` (`BLOCK_CHANGE` by the owner),
        // then unlit.
        t.cx.actions.push(WorldAction::LevelEvent(1009, pos, 0));
        t.cx.actions.push(WorldAction::Level(
            crate::mob::boss::warden::LevelAction::GameEvent {
                event: crate::level::game_event::events().block_change,
                pos: Vec3::new(
                    f64::from(pos.x) + 0.5,
                    f64::from(pos.y) + 0.5,
                    f64::from(pos.z) + 0.5,
                ),
                context: crate::level::game_event::GameEventContext::of(owner),
            },
        ));
        if let Some(off) = state.with("lit", "false") {
            t.cx.actions
                .push(WorldAction::SetBlock(pos, off, crate::level::update::ALL));
        }
    }
}

/// `Projectile.lerpRotation`.
fn lerp_rotation(old: f32, new: f32) -> f32 {
    let mut old = old;
    while new - old < -180.0 {
        old -= 360.0;
    }
    while new - old >= 180.0 {
        old += 360.0;
    }
    old + (new - old) * 0.2
}

/// The synched data of a thrown potion: the item (`DATA_ITEM_STACK`, 8).
pub fn entity_data(out: &mut Vec<u8>, p: &ThrownPotion, all: bool) {
    if all {
        // `EntityDataSerializers.ITEM_STACK`.
        crate::mob::boss::data::item(out, 8, 7, |o| p.stack.encode(o));
    }
}

/// The potion's NBT (`Projectile` and `ThrowableItemProjectile` fields).
pub fn save(p: &ThrownPotion, out: &mut Compound) {
    if let Some(u) = p.owner_uuid {
        #[allow(clippy::cast_possible_truncation)]
        out.insert(
            "Owner",
            Tag::IntArray(vec![
                (u >> 96) as i32,
                (u >> 64) as i32,
                (u >> 32) as i32,
                u as i32,
            ]),
        );
    }
    out.insert("LeftOwner", p.left_owner);
    out.insert("HasBeenShot", p.has_been_shot);
    out.insert(
        "Item",
        Tag::Compound(crate::item_nbt::stack_to_nbt(&p.stack)),
    );
}

/// Reads a thrown potion's NBT; a missing item is the default potion item of `kind`.
pub fn load(kind: EntityTypeId, tag: &Compound) -> ThrownPotion {
    let owner_uuid = match tag.get("Owner") {
        #[allow(clippy::cast_sign_loss)]
        Some(Tag::IntArray(a)) if a.len() == 4 => Some(
            a.iter()
                .fold(0_u128, |acc, &v| (acc << 32) | u128::from(v as u32)),
        ),
        _ => None,
    };
    let stack = match tag.get("Item") {
        Some(Tag::Compound(c)) => crate::item_nbt::stack_from_nbt(c),
        _ => ItemStack::default(),
    };
    let stack = if stack.is_empty() {
        let name = if kind == ids::LINGERING_POTION {
            "minecraft:lingering_potion"
        } else {
            "minecraft:splash_potion"
        };
        ItemStack::new(vanillite_data::registry::ITEM.id(name).unwrap_or(0), 1)
    } else {
        stack
    };
    ThrownPotion {
        owner: None,
        owner_uuid,
        stack,
        left_owner: tag
            .get("LeftOwner")
            .and_then(Tag::as_i64)
            .is_some_and(|v| v != 0),
        tick_count: 0,
        has_been_shot: tag
            .get("HasBeenShot")
            .and_then(Tag::as_i64)
            .is_some_and(|v| v != 0),
    }
}
