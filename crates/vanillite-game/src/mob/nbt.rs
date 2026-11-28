//! Saving and loading mobs and experience orbs in Vanilla's entity NBT (`saveWithoutId` /
//! `load` with the keys of 26.3), as stored in the `entities/` region files.

use vanillite_base::Vec3;
use vanillite_base::random::LegacyRandom;
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_nbt::{Compound, List, Tag};
use vanillite_world::entity::{EntityFlags, EntityRef, Rotation};

use super::{Kind, MobData, entity_nbt};
use crate::experience_orb::ExperienceOrbData;

fn doubles(v: Vec3) -> List {
    List(vec![Tag::Double(v.x), Tag::Double(v.y), Tag::Double(v.z)])
}

fn uuid_ints(uuid: u128) -> Vec<i32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let part = |shift: u32| (uuid >> shift) as u32 as i32;
    vec![part(96), part(64), part(32), part(0)]
}

/// A UUID as four ints (`UUIDUtil.CODEC`).
pub fn uuid_tag(uuid: u128) -> Tag {
    Tag::IntArray(uuid_ints(uuid))
}

/// The UUID of an entity compound (`UUID` as four ints).
pub fn uuid_of(tag: &Compound) -> Option<u128> {
    uuid_from_tag(tag.get("UUID"))
}

/// A UUID stored as four ints.
pub fn uuid_from_tag(tag: Option<&Tag>) -> Option<u128> {
    match tag {
        Some(Tag::IntArray(v)) if v.len() == 4 => {
            #[allow(clippy::cast_sign_loss)]
            let part = |i: usize| u128::from(v[i] as u32);
            Some((part(0) << 96) | (part(1) << 64) | (part(2) << 32) | part(3))
        }
        _ => None,
    }
}

/// A UUID stored as four ints under `key`.
fn uuid_key(tag: &Compound, key: &str) -> Option<u128> {
    match tag.get(key) {
        Some(Tag::IntArray(v)) if v.len() == 4 => {
            #[allow(clippy::cast_sign_loss)]
            let part = |i: usize| u128::from(v[i] as u32);
            Some((part(0) << 96) | (part(1) << 64) | (part(2) << 32) | part(3))
        }
        _ => None,
    }
}

/// A text component of a `custom_name` component value (network NBT) as an NBT tag.
pub fn component_tag(encoded: &[u8]) -> Option<Tag> {
    let mut input = encoded;
    let mut budget = vanillite_nbt::read::Budget::new(1 << 16, 64);
    vanillite_nbt::read::read_network(&mut input, &mut budget)
        .ok()
        .flatten()
}

/// The `custom_name` component value (network NBT) of a text component NBT tag.
pub fn tag_component(tag: &Tag) -> Option<Box<[u8]>> {
    let mut out = Vec::new();
    vanillite_nbt::write::write_network(tag, &mut out).ok()?;
    Some(out.into())
}

fn vec3_of(tag: &Compound, key: &str) -> Option<Vec3> {
    entity_nbt::vec3(tag.get(key))
}

fn int(tag: &Compound, key: &str) -> Option<i32> {
    entity_nbt::get_int(tag, key)
}

fn boolean(tag: &Compound, key: &str) -> bool {
    entity_nbt::get_bool(tag, key).unwrap_or(false)
}

fn double(tag: &Compound, key: &str) -> Option<f64> {
    entity_nbt::get_double(tag, key)
}

fn float(tag: &Compound, key: &str) -> Option<f32> {
    entity_nbt::get_float(tag, key)
}

/// The entity type of a compound (`id`).
pub fn type_of(tag: &Compound) -> Option<EntityTypeId> {
    tag.get("id")
        .and_then(Tag::as_str)
        .and_then(EntityTypeId::by_name)
}

/// The position of an entity compound.
pub fn pos_of(tag: &Compound) -> Option<Vec3> {
    vec3_of(tag, "Pos")
}

/// `Entity.saveWithoutId` plus the id: the common entity keys.
/// The `Entity` part of entity NBT in a new compound.
pub fn save_entity_base(core: &EntityRef<'_>) -> Compound {
    let mut out = Compound::new();
    save_entity(core, &mut out);
    out
}

/// The `Entity` part of entity NBT.
pub fn load_entity_base(tag: &Compound) -> Option<LoadedEntity> {
    load_entity(tag)
}

/// A UUID int array under `key`.
pub fn uuid_key_of(tag: &Compound, key: &str) -> Option<u128> {
    uuid_key(tag, key)
}

fn save_entity(core: &EntityRef<'_>, out: &mut Compound) {
    let name = core.kind().info().name;
    out.insert(
        "id",
        format!(
            "minecraft:{}",
            name.strip_prefix("minecraft:").unwrap_or(name)
        ),
    );
    out.insert("Pos", doubles(core.pos()));
    out.insert("Motion", doubles(core.velocity()));
    let rot = core.rot();
    out.insert(
        "Rotation",
        List(vec![Tag::Float(rot.y_rot), Tag::Float(rot.x_rot)]),
    );
    out.insert("OnGround", core.flags().contains(EntityFlags::ON_GROUND));
    out.insert("PortalCooldown", 0);
    out.insert("UUID", uuid_ints(core.uuid()));
    entity_nbt::save_flags(core.flags(), out);
}

/// An area effect cloud as entity NBT.
pub fn save_cloud(core: &EntityRef<'_>, data: &crate::effect::cloud::CloudData) -> Compound {
    let mut out = Compound::new();
    save_entity(core, &mut out);
    data.save(&mut out);
    out
}

/// An area effect cloud loaded from entity NBT: position, UUID and data.
pub fn load_cloud(tag: &Compound) -> Option<(Vec3, u128, crate::effect::cloud::CloudData)> {
    if type_of(tag)? != vanillite_data::entity::ids::AREA_EFFECT_CLOUD {
        return None;
    }
    let pos = vec3_of(tag, "Pos")?;
    let uuid = uuid_of(tag)?;
    Some((pos, uuid, crate::effect::cloud::CloudData::load(tag)))
}

/// A special entity (see [`super::boss::special`]) as entity NBT; `None` when it is not
/// saved (`shouldBeSaved`).
pub fn save_special(
    core: &EntityRef<'_>,
    data: &super::boss::special::SpecialData,
) -> Option<Compound> {
    let mut out = Compound::new();
    save_entity(core, &mut out);
    super::boss::special::save(data, &mut out)?;
    Some(out)
}

/// A special entity loaded from entity NBT: type, position, motion, rotation, UUID and data.
pub fn load_special(
    tag: &Compound,
    seed: i64,
) -> Option<(
    EntityTypeId,
    Vec3,
    Vec3,
    Rotation,
    u128,
    super::boss::special::SpecialData,
)> {
    let kind = type_of(tag)?;
    let data = super::boss::special::load(kind, tag)?;
    let pos = vec3_of(tag, "Pos")?;
    let velocity = entity_nbt::motion(tag.get("Motion")).unwrap_or(Vec3::ZERO);
    let rot = entity_nbt::rotation(tag.get("Rotation"))
        .map_or_else(Rotation::default, |(y, x)| loaded_rotation(y, x));
    // Generated entities (the pillars' crystals) have no UUID yet.
    let uuid = uuid_of(tag).unwrap_or_else(|| {
        let mut r = vanillite_base::random::LegacyRandom::new(seed);
        super::random_uuid(&mut r)
    });
    Some((kind, pos, velocity, rot, uuid, data))
}

/// A mob as entity NBT.
pub fn save_mob(core: &EntityRef<'_>, m: &MobData) -> Compound {
    let mut out = Compound::new();
    save_entity(core, &mut out);
    out.insert("fall_distance", m.fall_distance);
    #[allow(clippy::cast_possible_truncation)]
    {
        out.insert("Fire", m.fire_ticks.clamp(-32768, 32767) as i16);
        out.insert("Air", m.air.clamp(-32768, 32767) as i16);
        if m.ticks_frozen > 0 {
            out.insert("TicksFrozen", m.ticks_frozen);
        }
        out.insert("HurtTime", m.hurt_time as i16);
        out.insert("DeathTime", m.death_time as i16);
    }
    if m.silent {
        out.insert("Silent", true);
    }
    out.insert("Health", m.health);
    out.insert("AbsorptionAmount", m.absorption);
    if let Some(list) = m.effects.save() {
        out.insert("active_effects", list);
    }
    out.insert("FallFlying", false);
    out.insert("CanPickUpLoot", m.can_pick_up_loot);
    out.insert("PersistenceRequired", m.persistent);
    out.insert("LeftHanded", m.left_handed);
    if m.has_restriction() {
        out.insert("home_radius", m.restrict_radius);
        let c = m.restrict_center;
        out.insert("home_pos", vec![c.x, c.y, c.z]);
    }
    if m.no_ai {
        out.insert("NoAI", true);
    }
    if let Some(name) = m.custom_name.as_deref()
        && let Some(tag) = component_tag(name)
    {
        out.insert("CustomName", tag);
    }
    {
        let mut equipment = m.equipment.clone();
        if m.saddled {
            equipment.set(
                crate::item::EquipmentSlot::Saddle,
                super::equipment::stack("minecraft:saddle"),
            );
        }
        super::equipment::save_equipment(&equipment, &mut out);
    }
    if let Some(owner) = m.owner {
        out.insert("Owner", uuid_ints(owner));
        out.insert("Sitting", m.order_to_sit);
    }
    if let Some(l) = m.pending_leash {
        match l {
            super::LeashRef::Entity(u) => {
                let mut c = Compound::new();
                c.insert("UUID", uuid_ints(u));
                out.insert("leash", c);
            }
            super::LeashRef::Knot(p) => out.insert("leash", vec![p.x, p.y, p.z]),
        }
    }
    match m.kind {
        Kind::Zombie => {
            out.insert("IsBaby", m.baby);
            out.insert("CanBreakDoors", m.can_break_doors);
            super::fam::undead::zombies::save_conversion(m, &mut out);
            super::fam::undead::zombies::save_attributes(m, &mut out);
        }
        Kind::Creeper => {
            let c = super::fam::classic::creeper(m);
            #[allow(clippy::cast_possible_truncation)]
            {
                out.insert("Fuse", c.max_swell as i16);
                out.insert("ExplosionRadius", c.explosion_radius as i8);
            }
            if c.powered {
                out.insert("powered", true);
            }
            out.insert("ignited", c.ignited);
        }
        Kind::Cow | Kind::Pig | Kind::Sheep | Kind::Chicken => {
            out.insert("Age", m.age);
            out.insert("ForcedAge", m.forced_age);
            out.insert("AgeLocked", m.age_locked);
            out.insert("InLove", m.in_love);
            if let Some(cause) = m.love_cause {
                out.insert("LoveCause", uuid_ints(cause));
            }
            match m.kind {
                Kind::Sheep => {
                    #[allow(clippy::cast_possible_wrap)]
                    out.insert("Color", super::fam::classic::sheep(m).color as i8);
                    out.insert("Sheared", m.sheared);
                }
                _ => {
                    out.insert("variant", variant_name(m.variant));
                    if let Some(v) =
                        super::kinds::sound_variant_names(m.kind).get(usize::from(m.sound_variant))
                    {
                        out.insert("sound_variant", format!("minecraft:{v}"));
                    }
                    if m.kind == Kind::Chicken {
                        let c = super::fam::classic::chicken(m);
                        out.insert("EggLayTime", c.egg_time);
                        out.insert("IsChickenJockey", c.jockey);
                    }
                }
            }
        }
        Kind::Skeleton => super::fam::undead::skeletons::save_conversion(m, &mut out),
        Kind::Enderman => {
            if let Some(state) = super::fam::classic::enderman(m).carried {
                out.insert(
                    "carriedBlockState",
                    crate::level::block_entity::write_state_codec(state),
                );
            }
        }
        Kind::Spider => {}
        _ => super::fam::save(m, &mut out),
    }
    // `LivingEntity.addAdditionalSaveData`: every living entity writes its brain (empty
    // without memories), its attribute instances and the impulse grace time.
    if out.get("Brain").is_none() {
        let mut brain = Compound::new();
        brain.insert("memories", Compound::new());
        out.insert("Brain", brain);
    }
    if out.get("attributes").is_none() {
        out.insert("attributes", vanillite_nbt::List(created_attributes(m)));
    }
    out.insert("current_impulse_context_reset_grace_time", 0);
    out
}

/// The attribute instances `AttributeMap.pack` has of a mob that nothing else touched: the
/// follow range `Mob.finalizeSpawn` gave its `random_spawn_bonus` and the movement speed
/// `LivingEntity.aiStep`'s `removeFrost` looks up from the first tick on.
fn created_attributes(m: &MobData) -> Vec<Tag> {
    let mut list = Vec::new();
    if m.spawn_bonus != 0.0 {
        let mut bonus = Compound::new();
        bonus.insert("id", "minecraft:random_spawn_bonus");
        bonus.insert("amount", m.spawn_bonus);
        bonus.insert("operation", "add_multiplied_base");
        let mut c = Compound::new();
        c.insert("id", "minecraft:follow_range");
        c.insert("base", super::Attrs::of(m.kind.type_id()).follow_range);
        c.insert("modifiers", vanillite_nbt::List(vec![Tag::Compound(bonus)]));
        list.push(Tag::Compound(c));
    }
    if m.tick_count > 0 {
        let base = crate::effect::living::mob_attribute_base(
            m,
            crate::effect::attribute::Attribute::MOVEMENT_SPEED,
        );
        if let Some(base) = base {
            let mut c = Compound::new();
            c.insert("id", "minecraft:movement_speed");
            c.insert("base", base);
            list.push(Tag::Compound(c));
        }
    }
    list
}

fn variant_name(v: i32) -> &'static str {
    match v {
        0 => "minecraft:cold",
        2 => "minecraft:warm",
        _ => "minecraft:temperate",
    }
}

/// A mob loaded from entity NBT: what its entity starts with and its kind data.
#[derive(Debug)]
pub struct LoadedMob {
    pub pos: Vec3,
    pub velocity: Vec3,
    pub rot: Rotation,
    pub uuid: u128,
    pub on_ground: bool,
    pub data: MobData,
}

/// `Entity.load` of a mob of a kind with behaviour; `None` for other types or broken data.
pub fn load_mob(tag: &Compound, seed: i64) -> Option<LoadedMob> {
    let kind = Kind::of(type_of(tag)?)?;
    let pos = vec3_of(tag, "Pos")?;
    let velocity = entity_nbt::motion(tag.get("Motion")).unwrap_or(Vec3::ZERO);
    let (y_rot, x_rot) = entity_nbt::rotation(tag.get("Rotation")).unwrap_or_default();
    let uuid = uuid_of(tag).unwrap_or_else(|| {
        let mut r = vanillite_base::random::LegacyRandom::new(seed);
        super::random_uuid(&mut r)
    });
    let mut m = MobData::new(kind, seed);
    read_mob(&mut m, tag);
    // `setYBodyRot(getYRot())`: an armor stand's override sets the old rotations only.
    m.y_body_rot = if kind == Kind::ArmorStand { 0.0 } else { y_rot };
    Some(LoadedMob {
        pos,
        velocity,
        rot: loaded_rotation(y_rot, x_rot),
        uuid,
        on_ground: boolean(tag, "OnGround"),
        data: m,
    })
}

/// `Entity.load` on a living mob (`/data merge entity`, `/data modify entity`): the saved
/// state is read again from `tag`; what is not saved (its random, running goals, path) stays.
pub fn apply_mob(m: &mut MobData, tag: &Compound, game_time: i64, day_time: i64) {
    read_mob(m, tag);
    // `Villager.registerBrainGoals` of the brain the load made: the scheduled activity.
    if m.kind == Kind::Villager
        && tag.get("Brain").is_some()
        && let Some(b) = m.brain.as_deref_mut()
    {
        b.update_activity_from_schedule(game_time, day_time);
    }
    // `Entity.load` ends with `reapplyPosition`: the known speed starts over.
    m.last_known_pos = None;
}

/// The saved state of a mob (`readAdditionalSaveData` of the entity classes) from `tag`.
fn read_mob(m: &mut MobData, tag: &Compound) {
    let kind = m.kind;
    // `LivingEntity.readAdditionalSaveData`: the attributes and effects first, their values
    // and modifiers (Health Boost) set the maximum health.
    if super::fam::undead::zombies::is_zombie(kind) {
        super::fam::undead::zombies::load_attributes(m, tag);
    } else {
        entity_nbt::load_attributes(m, tag.get("attributes"));
    }
    m.effects.load(tag.get("active_effects"));
    if !m.effects.is_empty() {
        let dirty = m.effects.modifiers.take_dirty();
        crate::effect::living::apply_mob_modifiers(m, &dirty);
        m.absorption = float(tag, "AbsorptionAmount").unwrap_or(0.0).max(0.0);
        crate::effect::living::apply_mob_modifiers(m, &[]);
    }
    let health = float(tag, "Health");
    if let Some(h) = health {
        m.health = h.clamp(0.0, m.max_health());
    }
    m.fire_ticks = int(tag, "Fire").unwrap_or(0);
    m.air = int(tag, "Air").unwrap_or(300);
    m.ticks_frozen = int(tag, "TicksFrozen").unwrap_or(0);
    m.fall_distance = double(tag, "fall_distance")
        .or_else(|| double(tag, "FallDistance"))
        .unwrap_or(0.0);
    m.hurt_time = int(tag, "HurtTime").unwrap_or(0);
    m.death_time = int(tag, "DeathTime").unwrap_or(0);
    // Elder guardians are persistent from birth (`setPersistenceRequired`), also when
    // generated monuments store them without the key.
    m.persistent = boolean(tag, "PersistenceRequired")
        || kind == Kind::ElderGuardian && tag.get("PersistenceRequired").is_none();
    m.no_ai = boolean(tag, "NoAI");
    m.silent = boolean(tag, "Silent");
    m.custom_name = tag.get("CustomName").and_then(tag_component);
    super::fam::reload_brain(m, tag);
    super::equipment::load(m, tag);
    {
        use crate::item::EquipmentSlot;
        let saddle = m.equipment.get(EquipmentSlot::Saddle);
        // The classic pig keeps its saddle in `saddled`; family mobs in the slot.
        m.saddled = m.kind == Kind::Pig
            && !saddle.is_empty()
            && crate::item::item_id(saddle).name() == "minecraft:saddle";
        if m.saddled {
            m.equipment
                .set(EquipmentSlot::Saddle, crate::item::ItemStack::default());
        }
    }
    m.can_pick_up_loot = boolean(tag, "CanPickUpLoot");
    m.left_handed = boolean(tag, "LeftHanded");
    if let Some(r) = int(tag, "home_radius")
        && r >= 0
        && let Some(Tag::IntArray(v)) = tag.get("home_pos")
        && v.len() == 3
    {
        m.restrict_radius = r;
        m.restrict_center = vanillite_base::BlockPos::new(v[0], v[1], v[2]);
    }
    m.owner = uuid_key(tag, "Owner");
    m.tame = m.owner.is_some();
    m.order_to_sit = boolean(tag, "Sitting");
    m.in_sitting_pose = m.order_to_sit;
    m.pending_leash = leash_ref(tag);
    match kind {
        Kind::Zombie => {
            super::fam::undead::set_baby(m, boolean(tag, "IsBaby"));
            super::movement::set_can_break_doors(m, boolean(tag, "CanBreakDoors"));
            super::fam::undead::zombies::load_conversion(m, tag);
        }
        Kind::Creeper => {
            let c = super::fam::classic::creeper_mut(m);
            c.max_swell = int(tag, "Fuse").unwrap_or(30);
            c.explosion_radius = int(tag, "ExplosionRadius").unwrap_or(3);
            c.powered = boolean(tag, "powered");
            c.ignited = boolean(tag, "ignited");
        }
        Kind::Cow | Kind::Pig | Kind::Sheep | Kind::Chicken => {
            m.age = int(tag, "Age").unwrap_or(0);
            m.baby = m.age < 0;
            m.in_love = int(tag, "InLove").unwrap_or(0);
            m.love_cause = uuid_key(tag, "LoveCause");
            m.forced_age = int(tag, "ForcedAge").unwrap_or(0);
            m.age_locked = boolean(tag, "AgeLocked");
            if kind == Kind::Sheep {
                super::fam::classic::sheep_mut(m).color =
                    u8::try_from(int(tag, "Color").unwrap_or(0) & 15).unwrap_or(0);
                m.sheared = boolean(tag, "Sheared");
            } else {
                m.variant = match tag.get("variant").and_then(Tag::as_str) {
                    Some("minecraft:cold" | "cold") => 0,
                    Some("minecraft:warm" | "warm") => 2,
                    _ => 1,
                };
                // An unknown or missing key keeps the default (`classic`), like Vanilla.
                if let Some(v) = tag.get("sound_variant").and_then(Tag::as_str) {
                    let v = v.strip_prefix("minecraft:").unwrap_or(v);
                    if let Some(i) = super::kinds::sound_variant_names(kind)
                        .iter()
                        .position(|n| *n == v)
                    {
                        m.sound_variant = u8::try_from(i).unwrap_or(0);
                    }
                }
                if kind == Kind::Chicken {
                    let c = super::fam::classic::chicken_mut(m);
                    if let Some(t) = int(tag, "EggLayTime") {
                        c.egg_time = t;
                    }
                    c.jockey = boolean(tag, "IsChickenJockey");
                }
            }
        }
        Kind::Skeleton => super::fam::undead::skeletons::load_conversion(m, tag),
        Kind::Enderman => {
            let carried = tag
                .get("carriedBlockState")
                .and_then(crate::level::block_entity::read_state_codec)
                .filter(|s| !s.is_air());
            super::fam::classic::enderman_mut(m).carried = carried;
        }
        Kind::Spider => {}
        _ => super::fam::load(m, tag),
    }
    // `AbstractCubeMob` sets its size before `super` reads the attributes and health.
    let cube = matches!(kind, Kind::Slime | Kind::MagmaCube | Kind::SulfurCube);
    if cube {
        entity_nbt::load_attributes(m, tag.get("attributes"));
    }
    // Without `Health` the mob has its full health, of the maximum its kind data set.
    match health {
        None => m.health = m.max_health(),
        Some(h) if cube => m.health = h.clamp(0.0, m.max_health()),
        Some(_) => {}
    }
}

/// An experience orb as entity NBT.
pub fn save_orb(core: &EntityRef<'_>, o: &ExperienceOrbData) -> Compound {
    let mut out = Compound::new();
    save_entity(core, &mut out);
    out.insert("fall_distance", o.fall_distance);
    out.insert("Fire", -1_i16);
    out.insert("Air", 300_i16);
    #[allow(clippy::cast_possible_truncation)]
    out.insert("Health", o.health.clamp(0, 32767) as i16);
    #[allow(clippy::cast_possible_truncation)]
    out.insert("Age", o.age.min(32767) as i16);
    #[allow(clippy::cast_possible_truncation)]
    out.insert("Value", o.value.clamp(0, 32767) as i16);
    out.insert("Count", o.count);
    out
}

/// An experience orb from entity NBT.
pub fn load_orb(tag: &Compound) -> Option<(Vec3, Vec3, u128, ExperienceOrbData)> {
    if type_of(tag)? != ids::EXPERIENCE_ORB {
        return None;
    }
    let pos = vec3_of(tag, "Pos")?;
    let velocity = vec3_of(tag, "Motion").unwrap_or(Vec3::ZERO);
    let uuid = uuid_of(tag)?;
    Some((
        pos,
        velocity,
        uuid,
        ExperienceOrbData {
            value: int(tag, "Value").unwrap_or(1),
            count: int(tag, "Count").unwrap_or(1).max(1),
            age: int(tag, "Age").unwrap_or(0),
            health: int(tag, "Health").unwrap_or(5),
            following: None,
            tick_count: 0,
            fire_ticks: 0,
            // A summon replaces it with the seed its constructor took.
            random_state: vanillite_base::random::LegacyRandom::new(crate::mob::new_unique_seed())
                .state(),
            fall_distance: double(tag, "fall_distance").unwrap_or(0.0),
        },
    ))
}

/// The common part of an entity loaded from NBT.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoadedEntity {
    pub pos: Vec3,
    pub velocity: Vec3,
    pub rot: Rotation,
    pub uuid: u128,
    pub on_ground: bool,
}

/// `Entity.load`: the head takes the stored yaw, then `setRot` keeps yaw and pitch `% 360`.
fn loaded_rotation(y_rot: f32, x_rot: f32) -> Rotation {
    Rotation {
        y_rot: y_rot % 360.0,
        x_rot: x_rot % 360.0,
        y_head_rot: y_rot,
    }
}

fn load_entity(tag: &Compound) -> Option<LoadedEntity> {
    let pos = vec3_of(tag, "Pos")?;
    let (y_rot, x_rot) = entity_nbt::rotation(tag.get("Rotation")).unwrap_or_default();
    Some(LoadedEntity {
        pos,
        velocity: entity_nbt::motion(tag.get("Motion")).unwrap_or(Vec3::ZERO),
        rot: loaded_rotation(y_rot, x_rot),
        uuid: uuid_of(tag)?,
        on_ground: boolean(tag, "OnGround"),
    })
}

/// An item entity as entity NBT (`ItemEntity.addAdditionalSaveData`).
pub fn save_item(core: &EntityRef<'_>, d: &crate::item_entity::ItemData) -> Compound {
    let mut out = Compound::new();
    save_entity(core, &mut out);
    out.insert("fall_distance", 0.0_f64);
    #[allow(clippy::cast_possible_truncation)]
    out.insert("Fire", d.fire_ticks.clamp(-32768, 32767) as i16);
    out.insert("Air", 300_i16);
    out.insert("Health", d.health);
    out.insert("Age", d.age);
    out.insert("PickupDelay", d.pickup_delay);
    if let Some(t) = d.thrower {
        out.insert("Thrower", uuid_ints(t));
    }
    if let Some(t) = d.target {
        out.insert("Owner", uuid_ints(t));
    }
    out.insert("Item", crate::inventory::stack_to_nbt(&d.stack));
    out
}

/// An item entity from entity NBT; `None` without a valid item.
pub fn load_item(
    tag: &Compound,
    seed: i64,
) -> Option<(LoadedEntity, crate::item_entity::ItemData)> {
    if type_of(tag)? != ids::ITEM {
        return None;
    }
    let e = load_entity(tag)?;
    let stack = match tag.get("Item") {
        Some(Tag::Compound(c)) => crate::inventory::stack_from_nbt(c),
        _ => return None,
    };
    if stack.is_empty() {
        return None;
    }
    let short = |k: &str| int(tag, k).map_or(0, |v| i16::try_from(v).unwrap_or(0));
    let mut d = crate::item_entity::ItemData::new(
        stack,
        short("PickupDelay"),
        uuid_key(tag, "Thrower"),
        crate::item_entity::loaded_random(seed),
    );
    d.age = short("Age");
    d.health = int(tag, "Health").map_or(5, |v| i16::try_from(v).unwrap_or(5));
    d.fire_ticks = int(tag, "Fire").unwrap_or(0).max(0);
    d.target = uuid_key(tag, "Owner");
    Some((e, d))
}

/// An arrow as entity NBT (`AbstractArrow.addAdditionalSaveData`); `owner` is the owner's
/// UUID.
pub fn save_arrow(
    core: &EntityRef<'_>,
    a: &super::projectile::ArrowData,
    owner: Option<u128>,
) -> Compound {
    let mut out = Compound::new();
    save_entity(core, &mut out);
    out.insert("fall_distance", 0.0_f64);
    #[allow(clippy::cast_possible_truncation)]
    out.insert(
        "Fire",
        if a.fire > 0 {
            a.fire.min(i32::from(i16::MAX)) as i16
        } else {
            -1_i16
        },
    );
    out.insert("Air", 300_i16);
    #[allow(clippy::cast_possible_truncation)]
    {
        out.insert("life", a.life.clamp(-32768, 32767) as i16);
        out.insert("shake", a.shake.clamp(0, 127) as i8);
    }
    out.insert("inGround", a.in_ground);
    #[allow(clippy::cast_possible_wrap)]
    out.insert("pickup", a.pickup as i8);
    out.insert("damage", a.base_damage);
    out.insert("crit", a.crit);
    #[allow(clippy::cast_possible_wrap)]
    out.insert("PierceLevel", a.pierce_level as i8);
    out.insert("SoundEvent", hit_sound(a));
    if let Some(o) = owner.or(a.owner_uuid) {
        out.insert("Owner", uuid_ints(o));
    }
    out.insert("HasBeenShot", a.has_been_shot);
    out.insert("LeftOwner", a.left_owner);
    out.insert("item", crate::item_nbt::stack_to_nbt(&a.item));
    if let Some(w) = a.weapon.as_ref().filter(|w| !w.is_empty()) {
        out.insert("weapon", crate::item_nbt::stack_to_nbt(w));
    }
    if core.kind() == ids::SPECTRAL_ARROW {
        out.insert("Duration", a.spectral_duration);
    }
    out
}

/// The sound an arrow makes when it hits (`soundEvent`).
fn hit_sound(a: &super::projectile::ArrowData) -> &'static str {
    if a.crossbow_sound {
        "minecraft:item.crossbow.hit"
    } else {
        "minecraft:entity.arrow.hit"
    }
}

/// An arrow from entity NBT (its owner is not resolved). Its own random starts from `seed`,
/// the seed its constructor already took, or else from the next entity seed.
pub fn load_arrow(
    tag: &Compound,
    seed: Option<i64>,
) -> Option<(LoadedEntity, super::projectile::ArrowData)> {
    let kind = type_of(tag)?;
    if !super::projectile::is_arrow_kind(kind) {
        return None;
    }
    // The pickup item; a missing one is the kind's default item.
    let item = match tag.get("item") {
        Some(Tag::Compound(c)) => crate::item_nbt::stack_from_nbt(c),
        _ => crate::item::ItemStack::default(),
    };
    let item = if item.is_empty() {
        let name = if kind == ids::SPECTRAL_ARROW {
            "minecraft:spectral_arrow"
        } else {
            "minecraft:arrow"
        };
        crate::item::ItemStack::new(vanillite_data::registry::ITEM.id(name).unwrap_or(0), 1)
    } else {
        item
    };
    let e = load_entity(tag)?;
    let damage = double(tag, "damage").unwrap_or(2.0);
    Some((
        e,
        super::projectile::ArrowData {
            owner: None,
            owner_uuid: uuid_key(tag, "Owner"),
            base_damage: damage,
            in_ground: boolean(tag, "inGround"),
            life: int(tag, "life").unwrap_or(0),
            shake: int(tag, "shake").unwrap_or(0),
            crit: boolean(tag, "crit"),
            left_owner: boolean(tag, "LeftOwner"),
            last_state: None,
            tick_count: 0,
            pickup: u8::try_from(int(tag, "pickup").unwrap_or(0).clamp(0, 2)).unwrap_or(0),
            random_state: LegacyRandom::new(seed.unwrap_or_else(crate::mob::new_entity_seed))
                .state(),
            knockback: 0.0,
            fire: int(tag, "Fire").unwrap_or(0).max(0),
            touching_water: false,
            item,
            in_ground_time: 0,
            spectral_duration: int(tag, "Duration").unwrap_or(200),
            has_been_shot: boolean(tag, "HasBeenShot"),
            pierce_level: u8::try_from(int(tag, "PierceLevel").unwrap_or(0).clamp(0, 127))
                .unwrap_or(0),
            pierced: Vec::new(),
            pierced_killed: Vec::new(),
            crossbow_sound: matches!(
                tag.get("SoundEvent"),
                Some(Tag::String(s)) if s.as_str() == "minecraft:item.crossbow.hit"
            ),
            weapon: match tag.get("weapon") {
                Some(Tag::Compound(c)) => Some(crate::item_nbt::stack_from_nbt(c)),
                _ => None,
            },
            last_deflected: None,
        },
    ))
}

/// A boat as entity NBT.
pub fn save_boat(
    core: &EntityRef<'_>,
    b: &crate::vehicle::BoatData,
    leash: Option<super::LeashRef>,
) -> Compound {
    let mut out = Compound::new();
    save_entity(core, &mut out);
    out.insert("fall_distance", b.fall_distance);
    out.insert("Fire", -1_i16);
    out.insert("Air", 300_i16);
    if let Some(name) = b.custom_name.as_deref()
        && let Some(tag) = component_tag(name)
    {
        out.insert("CustomName", tag);
    }
    if b.kind.chest {
        save_chest_vehicle(&b.items, b.loot_table.as_ref(), &mut out);
    }
    match leash {
        Some(super::LeashRef::Entity(u)) => {
            let mut c = Compound::new();
            c.insert("UUID", uuid_ints(u));
            out.insert("leash", c);
        }
        Some(super::LeashRef::Knot(p)) => out.insert("leash", vec![p.x, p.y, p.z]),
        None => {}
    }
    out
}

/// The `leash` of entity NBT: a holder UUID or a knot position.
fn leash_ref(tag: &Compound) -> Option<super::LeashRef> {
    match tag.get("leash") {
        Some(Tag::Compound(c)) => uuid_of(c).map(super::LeashRef::Entity),
        Some(Tag::IntArray(v)) if v.len() == 3 => Some(super::LeashRef::Knot(
            vanillite_base::BlockPos::new(v[0], v[1], v[2]),
        )),
        _ => None,
    }
}

/// A boat from entity NBT.
pub fn load_boat(tag: &Compound, seed: i64) -> Option<(LoadedEntity, crate::vehicle::BoatData)> {
    let kind = crate::vehicle::BoatKind::of(type_of(tag)?)?;
    let e = load_entity(tag)?;
    let mut b = crate::vehicle::BoatData::new(kind, seed);
    b.custom_name = tag.get("CustomName").and_then(tag_component);
    b.pending_leash = leash_ref(tag);
    if kind.chest {
        b.loot_table = load_chest_vehicle(tag, &mut b.items);
    }
    Some((e, b))
}

/// `Entity.load` on a boat (`/data`, `entity_data`): its name and chest contents from `tag`.
pub fn apply_boat(b: &mut crate::vehicle::BoatData, tag: &Compound) {
    b.custom_name = tag.get("CustomName").and_then(tag_component);
    if b.kind.chest {
        b.items
            .iter_mut()
            .for_each(|s| *s = crate::item::ItemStack::EMPTY);
        b.loot_table = load_chest_vehicle(tag, &mut b.items);
    }
}

/// `ContainerEntity.addChestVehicleSaveData`: the loot table, or the items.
fn save_chest_vehicle(
    items: &[crate::item::ItemStack],
    loot_table: Option<&(String, i64)>,
    out: &mut Compound,
) {
    match loot_table {
        Some((table, seed)) => {
            out.insert("LootTable", table.clone());
            if *seed != 0 {
                out.insert("LootTableSeed", *seed);
            }
        }
        None => crate::block::container::save_items_always(items, out),
    }
}

/// `ContainerEntity.readChestVehicleSaveData`.
fn load_chest_vehicle(
    tag: &Compound,
    items: &mut [crate::item::ItemStack],
) -> Option<(String, i64)> {
    if let Some(Tag::String(table)) = tag.get("LootTable") {
        let seed = tag.get("LootTableSeed").and_then(Tag::as_i64).unwrap_or(0);
        return Some((table.clone(), seed));
    }
    crate::block::container::load_items(tag, items);
    None
}

/// A minecart as entity NBT (`AbstractMinecart.addAdditionalSaveData` and the kinds').
pub fn save_minecart(core: &EntityRef<'_>, m: &crate::minecart::MinecartData) -> Compound {
    use crate::minecart::MinecartKind;
    let mut out = Compound::new();
    save_entity(core, &mut out);
    out.insert("fall_distance", m.fall_distance);
    out.insert("Fire", -1_i16);
    out.insert("Air", 300_i16);
    if let Some(name) = m.custom_name.as_deref()
        && let Some(tag) = component_tag(name)
    {
        out.insert("CustomName", tag);
    }
    if let Some(state) = m.display_state {
        out.insert(
            "DisplayState",
            crate::level::block_entity::write_state_codec(state),
        );
    }
    if m.display_offset != m.kind.default_display_offset() {
        out.insert("DisplayOffset", m.display_offset);
    }
    out.insert("FlippedRotation", m.flipped);
    out.insert("HasTicked", m.first_tick);
    match m.kind {
        MinecartKind::Chest | MinecartKind::Hopper => {
            save_chest_vehicle(&m.items, m.loot_table.as_ref(), &mut out);
            if m.kind == MinecartKind::Hopper {
                out.insert("Enabled", m.enabled);
            }
        }
        MinecartKind::Furnace => {
            out.insert("PushX", m.push.x);
            out.insert("PushZ", m.push.z);
            #[allow(clippy::cast_possible_truncation)]
            out.insert("Fuel", m.fuel as i16);
        }
        MinecartKind::Tnt => {
            out.insert("fuse", m.fuse);
            if m.explosion_power != 4.0 {
                out.insert("explosion_power", m.explosion_power);
            }
            if m.explosion_speed_factor != 1.0 {
                out.insert("explosion_speed_factor", m.explosion_speed_factor);
            }
        }
        MinecartKind::CommandBlock => {
            let mut base = Compound::new();
            m.command.save(&mut base);
            for (key, value) in base.iter() {
                out.insert(key, value.clone());
            }
        }
        MinecartKind::Spawner => {
            if let Some(spawner) = &m.spawner {
                spawner.save(&mut out, true);
            }
        }
        MinecartKind::Plain => {}
    }
    for (key, value) in m.extra.iter() {
        if out.get(key).is_none() {
            out.insert(key, value.clone());
        }
    }
    out
}

/// A minecart from entity NBT.
pub fn load_minecart(
    tag: &Compound,
    seed: i64,
) -> Option<(LoadedEntity, crate::minecart::MinecartData)> {
    use crate::minecart::{MinecartData, MinecartKind};
    let kind = MinecartKind::of(type_of(tag)?)?;
    let e = load_entity(tag)?;
    let mut m = MinecartData::new(kind, seed);
    apply_minecart(&mut m, tag);
    Some((e, m))
}

/// `Entity.load` on a minecart (also `/data merge entity`): its saved state from `tag`.
pub fn apply_minecart(m: &mut crate::minecart::MinecartData, tag: &Compound) {
    use crate::minecart::MinecartKind;
    let kind = m.kind;
    m.custom_name = tag.get("CustomName").and_then(tag_component);
    m.fall_distance = double(tag, "fall_distance").unwrap_or(0.0);
    if let Some(state) = tag
        .get("DisplayState")
        .and_then(crate::level::block_entity::read_state_codec)
    {
        m.display_state = Some(state);
    }
    m.display_offset = int(tag, "DisplayOffset").unwrap_or(kind.default_display_offset());
    m.flipped = boolean(tag, "FlippedRotation");
    m.first_tick = boolean(tag, "HasTicked");
    match kind {
        MinecartKind::Chest | MinecartKind::Hopper => {
            m.loot_table = load_chest_vehicle(tag, &mut m.items);
            if kind == MinecartKind::Hopper {
                m.enabled = tag
                    .get("Enabled")
                    .and_then(Tag::as_i64)
                    .is_none_or(|v| v != 0);
            }
        }
        MinecartKind::Furnace => {
            let x = double(tag, "PushX").unwrap_or(0.0);
            let z = double(tag, "PushZ").unwrap_or(0.0);
            m.push = Vec3::new(x, 0.0, z);
            m.fuel = int(tag, "Fuel").unwrap_or(0);
            m.has_fuel = m.fuel > 0;
        }
        MinecartKind::Tnt => {
            m.fuse = int(tag, "fuse").unwrap_or(-1);
            m.explosion_power = float(tag, "explosion_power")
                .unwrap_or(4.0)
                .clamp(0.0, 128.0);
            m.explosion_speed_factor = float(tag, "explosion_speed_factor")
                .unwrap_or(1.0)
                .clamp(0.0, 128.0);
        }
        MinecartKind::CommandBlock => m.command.load(tag),
        MinecartKind::Spawner => {
            m.spawner = Some(Box::new(crate::block::spawner::MobSpawner::load(tag)));
        }
        MinecartKind::Plain => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AbstractArrow` keeps its owner as an `EntityReference`: a loaded arrow knows the
    /// owner's UUID (`Owner`) and saves it again even while the owner is not here (#272, I3-19).
    #[test]
    fn arrow_owner_uuid_round_trip() {
        let owner = 0x5641_4e49_0000_0001_0000_0000_0000_0007_u128;
        let mut tag = Compound::new();
        tag.insert("id", "minecraft:arrow");
        tag.insert(
            "Pos",
            Tag::List(vanillite_nbt::List(vec![
                Tag::Double(0.5),
                Tag::Double(64.0),
                Tag::Double(0.5),
            ])),
        );
        tag.insert("Owner", Tag::IntArray(uuid_ints(owner)));
        tag.insert("UUID", Tag::IntArray(uuid_ints(9)));
        let (_, arrow) = load_arrow(&tag, Some(1)).unwrap_or_else(|| panic!("arrow"));
        assert_eq!(arrow.owner, None);
        assert_eq!(arrow.owner_uuid, Some(owner));
    }
}
