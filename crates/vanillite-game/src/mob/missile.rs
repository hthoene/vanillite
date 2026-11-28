//! The projectiles besides arrows (`Projectile` subclasses): fireballs, shulker bullets,
//! evoker fangs, thrown potions, llama spit, snowballs, ender pearls, tridents and wind charges. They share
//! one entity store; each kind's flight and impact is implemented by the mob family that uses
//! it (`FamilyHooks::missile_tick`), so mobs and later players throw the same entities.

use vanillite_base::Vec3;
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_nbt::{Compound, Tag};
use vanillite_world::entity::{EntityId, EntityRef, NetworkIds, NewEntity, Rotation};

use super::fam::Family;
use super::{MobContext, Tick};
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::survival::player::Env;

/// The projectile kinds of the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MissileKind {
    /// `SmallFireball` (blazes).
    SmallFireball,
    /// `LargeFireball` (ghasts).
    Fireball,
    ShulkerBullet,
    EvokerFangs,
    /// `ThrownSplashPotion` (witches).
    SplashPotion,
    /// `ThrownLingeringPotion`.
    LingeringPotion,
    LlamaSpit,
    Snowball,
    /// `ThrownEnderpearl` (players).
    EnderPearl,
    /// `ThrownEgg`.
    Egg,
    /// `ThrownExperienceBottle`.
    ExperienceBottle,
    /// `EyeOfEnder` (no `Projectile`, but it flies like one).
    EyeOfEnder,
    /// `FireworkRocketEntity`.
    FireworkRocket,
    /// `FishingHook`.
    FishingBobber,
    /// `ThrownTrident` (drowned).
    Trident,
    WindCharge,
    BreezeWindCharge,
}

impl MissileKind {
    pub const ALL: [MissileKind; 17] = [
        MissileKind::SmallFireball,
        MissileKind::Fireball,
        MissileKind::ShulkerBullet,
        MissileKind::EvokerFangs,
        MissileKind::SplashPotion,
        MissileKind::LingeringPotion,
        MissileKind::LlamaSpit,
        MissileKind::Snowball,
        MissileKind::EnderPearl,
        MissileKind::Egg,
        MissileKind::ExperienceBottle,
        MissileKind::EyeOfEnder,
        MissileKind::FireworkRocket,
        MissileKind::FishingBobber,
        MissileKind::Trident,
        MissileKind::WindCharge,
        MissileKind::BreezeWindCharge,
    ];

    /// The entity type.
    pub fn type_id(self) -> EntityTypeId {
        match self {
            MissileKind::SmallFireball => ids::SMALL_FIREBALL,
            MissileKind::Fireball => ids::FIREBALL,
            MissileKind::ShulkerBullet => ids::SHULKER_BULLET,
            MissileKind::EvokerFangs => ids::EVOKER_FANGS,
            MissileKind::SplashPotion => ids::SPLASH_POTION,
            MissileKind::LingeringPotion => ids::LINGERING_POTION,
            MissileKind::LlamaSpit => ids::LLAMA_SPIT,
            MissileKind::Snowball => ids::SNOWBALL,
            MissileKind::EnderPearl => ids::ENDER_PEARL,
            MissileKind::Egg => ids::EGG,
            MissileKind::ExperienceBottle => ids::EXPERIENCE_BOTTLE,
            MissileKind::EyeOfEnder => ids::EYE_OF_ENDER,
            MissileKind::FireworkRocket => ids::FIREWORK_ROCKET,
            MissileKind::FishingBobber => ids::FISHING_BOBBER,
            MissileKind::Trident => ids::TRIDENT,
            MissileKind::WindCharge => ids::WIND_CHARGE,
            MissileKind::BreezeWindCharge => ids::BREEZE_WIND_CHARGE,
        }
    }

    /// A `ThrowableProjectile` (snowballs, ender pearls, thrown potions).
    pub fn is_throwable(self) -> bool {
        matches!(
            self,
            MissileKind::Snowball
                | MissileKind::EnderPearl
                | MissileKind::Egg
                | MissileKind::ExperienceBottle
                | MissileKind::SplashPotion
                | MissileKind::LingeringPotion
        )
    }

    /// The kind of an entity type.
    pub fn of(t: EntityTypeId) -> Option<MissileKind> {
        MissileKind::ALL.into_iter().find(|k| k.type_id() == t)
    }

    /// The family that implements the projectile's flight and impact.
    pub fn family(self) -> Family {
        match self {
            MissileKind::SmallFireball
            | MissileKind::Fireball
            | MissileKind::ShulkerBullet
            | MissileKind::EvokerFangs
            | MissileKind::SplashPotion
            | MissileKind::LingeringPotion => Family::Illager,
            MissileKind::LlamaSpit => Family::Companion,
            MissileKind::Snowball
            | MissileKind::EnderPearl
            | MissileKind::Egg
            | MissileKind::ExperienceBottle
            | MissileKind::EyeOfEnder
            | MissileKind::FireworkRocket
            | MissileKind::FishingBobber => Family::Wildlife,
            MissileKind::Trident => Family::Undead,
            MissileKind::WindCharge | MissileKind::BreezeWindCharge => Family::Mount,
        }
    }
}

/// Whether entities of `t` keep their data in the missile store. Thrown potions are special
/// entities ([`crate::effect::thrown`]), not missiles.
pub fn is_missile_kind(t: EntityTypeId) -> bool {
    MissileKind::of(t).is_some() && !crate::effect::thrown::is_thrown_potion(t)
}

/// The data of a projectile. The fields beyond the common ones are used as each kind needs
/// them; the owning family documents its use.
#[derive(Clone, Debug)]
pub struct MissileData {
    pub kind: MissileKind,
    /// The entity that shot it (`Projectile.owner`), when loaded; looked up again by
    /// `owner_uuid` every tick ([`resolve_owner`]).
    pub owner: Option<EntityId>,
    /// The owner's UUID (`EntityReference`; kept when the owner is not loaded).
    pub owner_uuid: Option<u128>,
    /// `leftOwner`.
    pub left_owner: bool,
    /// Ticks lived (`tickCount` / `life`).
    pub life: i32,
    /// The item it shows or carries (snowball, potion, trident, fireball item).
    pub item: ItemStack,
    /// `AbstractHurtingProjectile.accelerationPower`.
    pub acceleration: f64,
    /// Explosion power of large fireballs (`ExplosionPower`).
    pub explosion_power: i32,
    /// The target of a shulker bullet or the attack target of fangs.
    pub target: Option<EntityId>,
    pub target_uuid: Option<u128>,
    /// Kind specific integers (bullet flight steps and direction, fang warmup, trident
    /// loyalty and return state).
    pub ints: [i32; 4],
    /// A kind specific vector (bullet target deltas).
    pub vec: Vec3,
    /// `inGround` of tridents.
    pub in_ground: bool,
    /// Removed during its tick; the world despawns it afterwards.
    pub removed: bool,
    /// `hasBeenShot`: the `PROJECTILE_SHOOT` game event was posted.
    pub has_been_shot: bool,
    /// The state of its own random ([`vanillite_base::random::LegacyRandom::state`]) for the
    /// kinds that draw from it (ender pearls, shulker bullets); the shooter seeds it with
    /// [`new_projectile_random`].
    pub random_state: i64,
    /// `AbstractArrow.pickup` of tridents: 0 disallowed, 1 allowed, 2 creative only.
    pub pickup: u8,
    /// The state of a fishing bobber.
    pub bobber: Option<Box<crate::fishing::Bobber>>,
    /// `Projectile.lastDeflectedBy`.
    pub last_deflected: Option<EntityId>,
    /// `AbstractArrow.shakeTime` of tridents: no pickup while above 0.
    pub shake: u8,
    /// `AbstractArrow.soundEvent` of tridents is `entity.arrow.hit` (after the first landing).
    pub arrow_hit_sound: bool,
    /// `ThrownTrident.ID_FOIL`: `hasFoil()` of the thrown item (never set again after a load).
    pub foil: bool,
}

impl MissileData {
    /// A new projectile of `kind` with defaults.
    pub fn new(kind: MissileKind) -> MissileData {
        MissileData {
            kind,
            owner: None,
            owner_uuid: None,
            left_owner: false,
            life: 0,
            has_been_shot: false,
            item: ItemStack::default(),
            acceleration: 0.1,
            explosion_power: 1,
            target: None,
            target_uuid: None,
            ints: [0; 4],
            vec: Vec3::ZERO,
            in_ground: false,
            removed: false,
            random_state: 0,
            pickup: 0,
            bobber: None,
            last_deflected: None,
            shake: 0,
            arrow_hit_sound: false,
            foil: false,
        }
    }
}

/// Adds a projectile (`addFreshEntity`).
pub fn spawn(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    uuid: u128,
    pos: Vec3,
    velocity: Vec3,
    rot: Rotation,
    data: MissileData,
) -> EntityId {
    let mut new = NewEntity::new(data.kind.type_id(), network_ids.next(), uuid, pos);
    new.velocity = velocity;
    new.rot = rot;
    ents.spawn_missile(new, data)
}

/// Spawns a projectile from a mob's tick with a random UUID.
pub fn spawn_from(
    t: &mut Tick<'_, '_>,
    pos: Vec3,
    velocity: Vec3,
    rot: Rotation,
    data: MissileData,
) -> EntityId {
    let uuid = crate::mob::new_entity_uuid();
    spawn_owned(t, uuid, pos, velocity, rot, data)
}

/// `ProjectileUtil.computeMargin`: how much the boxes of the entities a projectile may hit
/// grow, from 0 in its first two ticks to 0.3 after eight.
pub fn margin(life: i32) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let m = ((life - 2) as f32 / 20.0).clamp(0.0, 0.3);
    f64::from(m)
}

/// The own random of a projectile a mob is about to shoot (`Entity.random`, seeded like every
/// new entity): `Projectile.shoot` draws its spread from it. Spawn the projectile with
/// [`spawn_shot`] then.
pub fn new_projectile_random() -> vanillite_base::random::LegacyRandom {
    vanillite_base::random::LegacyRandom::new(crate::mob::new_entity_seed())
}

/// [`spawn_from`] for a projectile that took its seed in [`new_projectile_random`].
pub fn spawn_shot(
    t: &mut Tick<'_, '_>,
    pos: Vec3,
    velocity: Vec3,
    rot: Rotation,
    data: MissileData,
) -> EntityId {
    spawn_owned(t, crate::mob::new_uuid(), pos, velocity, rot, data)
}

fn spawn_owned(
    t: &mut Tick<'_, '_>,
    uuid: u128,
    pos: Vec3,
    velocity: Vec3,
    rot: Rotation,
    mut data: MissileData,
) -> EntityId {
    if data.owner.is_none() {
        data.owner = Some(t.id);
    }
    if data.owner_uuid.is_none() {
        data.owner_uuid = t.ents.store.get(t.id).map(|e| e.uuid());
    }
    // Evoker fangs are no projectile: only their spawn.
    let fangs = data.kind == MissileKind::EvokerFangs;
    let owner = data.owner;
    let id = spawn(
        t.ents,
        t.level.network_ids(),
        uuid,
        pos,
        velocity,
        rot,
        data,
    );
    if fangs {
        let parent = owner
            .and_then(|o| t.ents.store.get(o))
            .map_or(0, |e| e.uuid());
        t.ents
            .spawned(id, vanillite_abi::spawn_reason::DEFAULT, parent);
    } else {
        t.ents.launched(id, owner);
    }
    id
}

/// `Projectile.spawnProjectileFromRotation` from `source`: the projectile takes its seed
/// (its constructor) and a UUID, `shootFromRotation(xRot, yRot, yOffset, pow, uncertainty)`
/// draws the spread from its random and adds the source's known movement (its vertical part
/// only off the ground), then it joins the world at `start`. Its random's state ends in
/// `data.random_state`; the owner is the source.
#[allow(clippy::too_many_arguments)]
pub fn spawn_from_rotation(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    source: EntityId,
    start: Vec3,
    y_offset: f32,
    pow: f32,
    uncertainty: f32,
    mut data: MissileData,
) -> Option<EntityId> {
    let e = ents.store.get(source)?;
    let (rot, on_ground, uuid) = (
        e.rot(),
        e.flags()
            .contains(vanillite_world::entity::EntityFlags::ON_GROUND),
        e.uuid(),
    );
    let known = crate::spear::known_movement(ents, source);
    let mut own = new_projectile_random();
    let projectile_uuid = crate::mob::new_uuid();
    let dir = direction_from_rotation(rot.x_rot, rot.y_rot, y_offset);
    let (velocity, shot_rot) = crate::mob::projectile::shoot(&mut own, dir, pow, uncertainty);
    let velocity = velocity + Vec3::new(known.x, if on_ground { 0.0 } else { known.y }, known.z);
    data.owner = Some(source);
    data.owner_uuid = Some(uuid);
    data.random_state = own.state();
    let id = spawn(
        ents,
        network_ids,
        projectile_uuid,
        start,
        velocity,
        Rotation {
            y_head_rot: 0.0,
            ..shot_rot
        },
        data,
    );
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_flags(vanillite_world::entity::EntityFlags::HAS_IMPULSE, true);
    }
    Some(id)
}

/// The direction of `shootFromRotation(xRot, yRot, yOffset)`.
pub fn direction_from_rotation(x_rot: f32, y_rot: f32, y_offset: f32) -> Vec3 {
    let rad = crate::mob::DEG_TO_RAD;
    let sin = |a: f32| vanillite_base::math::sin(f64::from(a));
    let cos = |a: f32| vanillite_base::math::cos(f64::from(a));
    Vec3::new(
        f64::from(-sin(y_rot * rad) * cos(x_rot * rad)),
        f64::from(-sin((x_rot + y_offset) * rad)),
        f64::from(cos(y_rot * rad) * cos(x_rot * rad)),
    )
}

/// The eye height of an entity (`getEyeY() - getY()`): players' from their pose, mobs' from
/// their kind.
pub fn eye_height(ents: &Entities, id: EntityId) -> f64 {
    let Some(e) = ents.store.get(id) else {
        return 0.0;
    };
    if ents.player(id).is_some() {
        return crate::survival::player::eye_height(e.dims().height);
    }
    ents.mob(id)
        .map_or_else(|| f64::from(e.dims().height * 0.85), crate::mob::eye_height)
}

/// What one projectile tick works on.
pub struct MissileTick<'a, 'e> {
    pub level: &'a Level,
    pub ents: &'a mut Entities,
    pub env: &'a mut Env<'e>,
    pub cx: &'a mut MobContext,
    pub id: EntityId,
}

impl std::fmt::Debug for MissileTick<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MissileTick")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl MissileTick<'_, '_> {
    /// The ticking projectile's data.
    pub fn data(&mut self) -> Option<&mut MissileData> {
        self.ents.missile_mut(self.id)
    }

    /// The ticking projectile's core columns.
    pub fn core(&self) -> Option<EntityRef<'_>> {
        self.ents.store.get(self.id)
    }
}

/// One projectile tick (`tickNonPassenger`); removed projectiles are despawned.
pub fn tick(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) {
    let Some((kind, owner, owner_uuid)) = ents.missile(id).map(|d| (d.kind, d.owner, d.owner_uuid))
    else {
        return;
    };
    // `getOwner()`: a respawned or relogged owner is a new entity, a gone one none.
    let resolved = resolve_owner(ents, owner, owner_uuid);
    if resolved != owner
        && let Some(d) = ents.missile_mut(id)
    {
        d.owner = resolved;
    }
    {
        let mut t = MissileTick {
            level,
            ents,
            env,
            cx,
            id,
        };
        // `Projectile.tick` (evoker fangs are no projectile).
        let unshot = t
            .ents
            .missile(id)
            // Evoker fangs and eyes of ender are no projectile.
            .filter(|d| {
                !d.has_been_shot
                    && d.kind != MissileKind::EvokerFangs
                    && d.kind != MissileKind::EyeOfEnder
            })
            .map(|d| d.owner);
        if let Some(owner) = unshot {
            if let Some(pos) = t.ents.store.get(id).map(|e| e.pos()) {
                shoot_event(t.env.out, pos, owner);
            }
            if let Some(d) = t.data() {
                d.has_been_shot = true;
            }
        }
        if let Some(d) = t.data() {
            d.life += 1;
        }
        super::fam::missile_tick(&mut t, kind);
    }
    if ents.missile(id).is_some_and(|d| d.removed) {
        cx.discarded.push(id);
    }
}

/// `Projectile.getOwner` (`EntityReference.getEntity`): the entity `owner` while it is still
/// the one with `uuid`, else the entity of this world with that UUID now (a respawned or
/// relogged player, a mob of a reloaded chunk); `None` while there is none (an offline owner).
/// Without a UUID the entity itself while it exists.
pub fn resolve_owner(
    ents: &Entities,
    owner: Option<EntityId>,
    uuid: Option<u128>,
) -> Option<EntityId> {
    let Some(uuid) = uuid else {
        return owner.filter(|&o| ents.store.contains(o));
    };
    owner
        .filter(|&o| ents.store.get(o).is_some_and(|e| e.uuid() == uuid))
        .or_else(|| ents.store.by_uuid(uuid))
}

/// What a projectile is about to hit, for [`hit_event`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HitWhat {
    /// An entity.
    Entity(EntityId),
    /// A block, at a point on it.
    Block(vanillite_base::BlockPos, Vec3),
}

/// `Entity.deflection(projectile)`: a breeze (`#deflects_projectiles`) reverses every
/// projectile but wind charges.
pub fn deflects(ents: &Entities, target: EntityId, projectile: EntityTypeId) -> bool {
    if projectile == ids::WIND_CHARGE || projectile == ids::BREEZE_WIND_CHARGE {
        return false;
    }
    ents.store
        .get(target)
        .is_some_and(|e| crate::effect::entity_in_tag(e.kind(), "minecraft:deflects_projectiles"))
}

/// `Breeze.PROJECTILE_DEFLECTION`: the deflect sound at the breeze, then
/// `ProjectileDeflection.REVERSE` with power 1: half the speed backwards, turned by 170° to
/// 190° (the projectile's random).
pub fn reverse(
    ents: &Entities,
    env: &mut Env<'_>,
    deflector: EntityId,
    velocity: Vec3,
    rot: Rotation,
    random: &mut impl vanillite_base::random::RandomSource,
) -> (Vec3, Rotation) {
    crate::survival::play_entity_sound(
        env.out,
        ents,
        None,
        deflector,
        "minecraft:entity.breeze.deflect",
        vanillite_protocol::packets::play::clientbound::SoundSource::Hostile,
        1.0,
        1.0,
    );
    let turn = 170.0 + random.next_float() * 20.0;
    (
        velocity * -0.5,
        Rotation {
            y_rot: rot.y_rot + turn,
            ..rot
        },
    )
}

/// The plugins' hit event (`vanillite_abi::event::PROJECTILE_HIT`, Bukkit's
/// `preHitTargetOrDeflectSelf`) of `projectile`, shot by `shooter`: whether the hit happens
/// (`false`: a plugin cancelled it and the projectile ignores it). Without listeners one
/// branch.
pub fn hit_event(
    level: &Level,
    ents: &Entities,
    projectile: EntityId,
    shooter: Option<EntityId>,
    hit: HitWhat,
) -> bool {
    let Some(hooks) = ents.hooks_for(vanillite_abi::event::PROJECTILE_HIT) else {
        return true;
    };
    let Some(e) = ents.store.get(projectile) else {
        return true;
    };
    let uuid_of = |id: EntityId| ents.store.get(id).map_or(0, |e| e.uuid());
    let mut ev = crate::hooks::Event {
        entity: e.uuid(),
        entity_type: e.kind().name().to_owned(),
        source: shooter.map_or(0, uuid_of),
        to: crate::hooks::Place::at(e.pos()),
        ..crate::hooks::Event::new(vanillite_abi::event::PROJECTILE_HIT)
    };
    match hit {
        HitWhat::Entity(target) => {
            ev.other = uuid_of(target);
            ev.other_type = ents
                .store
                .get(target)
                .map_or_else(String::new, |t| t.kind().name().to_owned());
        }
        HitWhat::Block(pos, at) => {
            ev.pos = pos;
            ev.state = Some(level.block(pos));
            ev.cause = face_at(pos, at);
            ev.to = crate::hooks::Place::at(at);
        }
    }
    hooks.event(crate::hooks::HookWorld::split(level, ents), &mut ev);
    !ev.cancelled
}

/// The face of the block at `pos` nearest to the point `at` on it (`Direction` 3D data value:
/// down, up, north, south, west, east).
fn face_at(pos: vanillite_base::BlockPos, at: Vec3) -> u32 {
    let (x, y, z) = (
        at.x - f64::from(pos.x),
        at.y - f64::from(pos.y),
        at.z - f64::from(pos.z),
    );
    let faces = [
        (y, 0),
        (1.0 - y, 1),
        (z, 2),
        (1.0 - z, 3),
        (x, 4),
        (1.0 - x, 5),
    ];
    faces
        .into_iter()
        .min_by(|a, b| a.0.abs().total_cmp(&b.0.abs()))
        .map_or(1, |f| f.1)
}

/// `Projectile.onHitBlock`: `onProjectileHit` of the block `projectile` hit at `at`, applied
/// with the world actions after the tick.
pub fn hit_block(
    level: &Level,
    cx: &mut MobContext,
    projectile: EntityId,
    block: vanillite_base::BlockPos,
    at: Vec3,
) {
    hit_block_at_speed(level, cx, projectile, block, at, 0.0);
}

/// [`hit_block`] of a projectile that hit with `speed` (tridents break pointed dripstone).
pub fn hit_block_at_speed(
    level: &Level,
    cx: &mut MobContext,
    projectile: EntityId,
    block: vanillite_base::BlockPos,
    at: Vec3,
    speed: f64,
) {
    let state = level.block(block);
    if level
        .behaviours()
        .get(state.block())
        .projectile_hit
        .is_some()
    {
        cx.actions.push(super::WorldAction::ProjectileHitBlock(
            block, at, projectile, speed,
        ));
    }
}

/// `Projectile.onHit`'s `PROJECTILE_LAND` game event of `projectile`: at the hit point `at`
/// for an entity hit (`block` = `None`), at the centre of the hit block with its state for a
/// block hit. `owner` is the projectile's owner, which listeners still know once it is gone.
pub fn land_event(
    out: &mut Vec<crate::survival::Effect>,
    level: &Level,
    projectile: EntityId,
    owner: Option<EntityId>,
    block: Option<vanillite_base::BlockPos>,
    at: Vec3,
) {
    use crate::level::game_event::{GameEventContext, events};
    let (pos, context) = match block {
        Some(b) => (
            Vec3::new(
                f64::from(b.x) + 0.5,
                f64::from(b.y) + 0.5,
                f64::from(b.z) + 0.5,
            ),
            GameEventContext::with_state(Some(projectile), level.block(b)),
        ),
        None => (at, GameEventContext::of(Some(projectile))),
    };
    let context = GameEventContext { owner, ..context };
    out.push(crate::survival::Effect::GameEvent {
        event: events().projectile_land,
        pos,
        context,
    });
}

/// `Projectile.tick`'s first `PROJECTILE_SHOOT` (`hasBeenShot`), from the projectile with its
/// owner as the source.
pub fn shoot_event(out: &mut Vec<crate::survival::Effect>, pos: Vec3, owner: Option<EntityId>) {
    crate::survival::game_event(
        out,
        crate::level::game_event::events().projectile_shoot,
        pos,
        owner,
    );
}

/// The owner as a UUID array.
fn uuid_ints(uuid: u128) -> Vec<i32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let v = vec![
        (uuid >> 96) as i32,
        (uuid >> 64) as i32,
        (uuid >> 32) as i32,
        uuid as i32,
    ];
    v
}

/// A projectile as entity NBT: the entity, the owner, the item, then the kind's own data.
pub fn save(core: &EntityRef<'_>, d: &MissileData, owner: Option<u128>) -> Compound {
    let mut out = super::nbt::save_entity_base(core);
    if let Some(o) = owner.or(d.owner_uuid) {
        out.insert("Owner", uuid_ints(o));
    }
    out.insert("LeftOwner", d.left_owner);
    out.insert("HasBeenShot", d.has_been_shot);
    // Tridents keep theirs as `item` (`AbstractArrow`).
    if !d.item.is_empty() && d.kind != MissileKind::Trident {
        out.insert("Item", crate::inventory::stack_to_nbt(&d.item));
    }
    super::fam::missile_save(d, &mut out);
    out
}

/// A projectile from entity NBT; `None` for other types.
pub fn load(tag: &Compound) -> Option<(super::nbt::LoadedEntity, MissileData)> {
    let kind = MissileKind::of(super::nbt::type_of(tag)?)?;
    let e = super::nbt::load_entity_base(tag)?;
    let mut d = MissileData::new(kind);
    d.owner_uuid = super::nbt::uuid_key_of(tag, "Owner");
    d.left_owner = crate::mob::entity_nbt::get_bool(tag, "LeftOwner").unwrap_or(false);
    d.has_been_shot = crate::mob::entity_nbt::get_bool(tag, "HasBeenShot").unwrap_or(false);
    if let Some(Tag::Compound(c)) = tag.get("Item") {
        d.item = crate::inventory::stack_from_nbt(c);
    }
    super::fam::missile_load(&mut d, tag);
    Some((e, d))
}
