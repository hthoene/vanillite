//! `Wolf`: taming with bones, sitting, collars, wolf armour, anger (`NeutralMob`), begging,
//! hunting prey, variants by biome and sound variants.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::Compound;
use vanillite_world::entity::EntityId;

use super::{
    Cond, G, Goal as FamGoal, Interact, MeleeKind, SpawnContext, common, consume, dirty, event,
    fam, guard, in_tag, item_name, owned_by, sound, st, st_mut,
};
use crate::item::EquipmentSlot;
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::common::{CommonGoal, TargetFilter};
use crate::mob::fam::{GroupData, Hurt, SpawnReason};
use crate::mob::goals::{flag, reduced};
use crate::mob::kinds::Kind;
use crate::mob::{MobData, Tick};
use crate::player::encode::{DataWriter, serializer};

/// `wolf_variant` registry ids (alphabetical): ashen, black, chestnut, pale, rusty, snowy,
/// spotted, striped, woods.
pub const VARIANT_PALE: i32 = 3;
/// `wolf_sound_variant` ids: angry, big, classic, cute, grumpy, puglin, sad.
pub const SOUND_CLASSIC: u8 = 2;
pub(crate) const SOUND_NAMES: [&str; 7] =
    ["angry", "big", "classic", "cute", "grumpy", "puglin", "sad"];
pub(crate) const VARIANT_NAMES: [&str; 9] = [
    "ashen", "black", "chestnut", "pale", "rusty", "snowy", "spotted", "striped", "woods",
];

/// The wolf goals.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// `WolfAvoidEntityGoal` (untamed wolves flee strong llamas).
    AvoidLlama(CommonGoal),
    /// `BegGoal`.
    Beg {
        player: Option<EntityId>,
        look_time: i32,
    },
}

pub fn flags(g: &Goal) -> u8 {
    match g {
        Goal::AvoidLlama(_) => flag::MOVE,
        Goal::Beg { .. } => flag::LOOK,
    }
}

fn llama(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .mob(id)
        .is_some_and(|m| matches!(m.kind, Kind::Llama | Kind::TraderLlama))
}

/// `PREY_SELECTOR`: sheep, rabbits and foxes.
fn prey(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .mob(id)
        .is_some_and(|m| matches!(m.kind, Kind::Sheep | Kind::Rabbit | Kind::Fox))
}

/// `AbstractSkeleton`.
fn skeleton(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.mob(id).is_some_and(|m| {
        matches!(
            m.kind,
            Kind::Skeleton | Kind::Stray | Kind::Bogged | Kind::Parched | Kind::WitherSkeleton
        )
    })
}

/// `registerGoals` of the wolf.
pub fn register_goals(m: &mut MobData) {
    let g = &mut m.goals;
    g.add(1, G::Float);
    g.add(
        1,
        fam(FamGoal::TamePanic {
            speed: 1.5,
            env_only: true,
            pos: Vec3::ZERO,
        }),
    );
    g.add(2, G::SitWhenOrdered);
    let llamas: TargetFilter = llama;
    g.add(
        3,
        fam(FamGoal::Wolf(Goal::AvoidLlama(CommonGoal::avoid(
            llamas, 24.0, 1.5, 1.5,
        )))),
    );
    g.add(
        4,
        G::Leap {
            yd: 0.4,
            target: None,
        },
    );
    g.add(5, G::melee(MeleeKind::Plain, 1.0, true));
    g.add(6, G::follow_owner(1.0, 10.0, 2.0));
    g.add(7, G::breed(1.0));
    g.add(8, G::stroll(1.0));
    g.add(
        9,
        fam(FamGoal::Wolf(Goal::Beg {
            player: None,
            look_time: 0,
        })),
    );
    g.add(10, G::look_at_player(8.0));
    g.add(10, G::random_look());
    let tg = &mut m.targets;
    tg.add(1, G::owner_hurt_by());
    tg.add(2, G::owner_hurt());
    tg.add(3, G::hurt_by(true));
    let angry: TargetFilter = crate::mob::common::angry_at;
    tg.add(4, common(CommonGoal::nearest_player(true, angry)));
    let preys: TargetFilter = prey;
    tg.add(
        5,
        guard(Cond::NotTame, CommonGoal::nearest(10, false, preys)),
    );
    let turtles: TargetFilter = crate::mob::common::baby_turtle_on_land;
    tg.add(
        6,
        guard(Cond::NotTame, CommonGoal::nearest(10, false, turtles)),
    );
    let skeletons: TargetFilter = skeleton;
    tg.add(7, common(CommonGoal::nearest(10, false, skeletons)));
    tg.add(
        8,
        common(CommonGoal::ResetUniversalAnger {
            alert_same_type: true,
            last_hurt: 0,
        }),
    );
    m.nav.can_float = true;
}

/// The wolf variant of a biome (`wolf_variant` spawn conditions).
pub fn variant_for(ctx: &SpawnContext<'_>, random: &mut LegacyRandom) -> i32 {
    super::pick_variant(
        ctx,
        random,
        &[
            (0, Some("minecraft:snowy_taiga")),
            (1, Some("minecraft:old_growth_pine_taiga")),
            (2, Some("minecraft:old_growth_spruce_taiga")),
            (4, Some("#minecraft:is_jungle")),
            (5, Some("minecraft:grove")),
            (6, Some("#minecraft:is_savanna")),
            (7, Some("#minecraft:is_badlands")),
            (8, Some("minecraft:forest")),
        ],
        VARIANT_PALE,
    )
}

/// `WolfSoundVariants.pickRandomSoundVariant`.
pub fn random_sound_variant(r: &mut LegacyRandom) -> u8 {
    u8::try_from(r.next_int_bounded(7)).unwrap_or(SOUND_CLASSIC)
}

/// `finalizeSpawn`: the pack variant of the biome and a random sound variant, before
/// `Mob.finalizeSpawn`; a pack (`WolfPackData`) has no babies.
pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    m.variant = if let GroupData::Wolf(v) = ctx.group {
        v
    } else {
        let v = variant_for(ctx, &mut m.random);
        ctx.group = GroupData::Wolf(v);
        v
    };
    st_mut(m).sound_variant = random_sound_variant(&mut m.random);
}

/// `applyTamingSideEffects`: 40 health when tame, 8 wild.
pub fn apply_taming_side_effects(m: &mut MobData) {
    if m.tame {
        m.attrs.max_health = 40.0;
        m.health = 40.0;
    } else {
        m.attrs.max_health = 8.0;
        m.health = m.health.min(8.0);
    }
}

fn sound_set(m: &MobData, sound: &str) -> &'static str {
    // The sound variants differ only in the event name prefix (`entity.wolf_<v>.*`).
    let v = SOUND_NAMES
        .get(usize::from(st(m).sound_variant))
        .copied()
        .unwrap_or("classic");
    let baby = m.baby;
    static TABLE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut out = Vec::new();
        for v in SOUND_NAMES {
            for baby in [false, true] {
                for s in ["ambient", "death", "growl", "hurt", "pant", "step", "whine"] {
                    let prefix = match (baby, v) {
                        (true, _) => "baby_wolf".to_owned(),
                        (false, "classic") => "wolf".to_owned(),
                        (false, v) => format!("wolf_{v}"),
                    };
                    out.push(format!("minecraft:entity.{prefix}.{s}"));
                }
            }
        }
        out
    });
    let vi = SOUND_NAMES.iter().position(|n| *n == v).unwrap_or(2);
    let si = ["ambient", "death", "growl", "hurt", "pant", "step", "whine"]
        .iter()
        .position(|n| *n == sound)
        .unwrap_or(0);
    let i = (vi * 2 + usize::from(baby)) * 7 + si;
    table
        .get(i)
        .map_or("minecraft:entity.wolf.ambient", String::as_str)
}

/// `getAmbientSound`: growls when angry, whines when hurt and tame, pants.
pub fn ambient_sound(m: &mut MobData) -> Option<&'static str> {
    if m.anger_end > 0 {
        return Some(sound_set(m, "growl"));
    }
    if m.random.next_int_bounded(3) == 0 {
        return Some(if m.tame && m.health < 20.0 {
            sound_set(m, "whine")
        } else {
            sound_set(m, "pant")
        });
    }
    Some(sound_set(m, "ambient"))
}

/// `getHurtSound`: the armour sound when the armour takes the hit.
pub fn hurt_sound(m: &MobData) -> &'static str {
    if wearing_armor(m) {
        "minecraft:entity.wolf.armor_damage"
    } else {
        sound_set(m, "hurt")
    }
}

pub fn death_sound(m: &MobData) -> &'static str {
    sound_set(m, "death")
}

fn wearing_armor(m: &MobData) -> bool {
    item_name(m.equipment.get(EquipmentSlot::Body)) == "minecraft:wolf_armor"
}

/// `Wolf.tick`: wet fur and shaking (the shake sound).
pub fn before_tick(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().is_alive() {
        return;
    }
    if super::in_water_or_rain(t) {
        let s = st_mut(t.mob());
        s.wet = true;
        if s.shaking {
            s.shaking = false;
            s.shake_anim = 0.0;
            let id = t.id;
            crate::mob::living::entity_event(t, id, 56);
        }
    } else if st(t.mob_ref()).shaking {
        if st(t.mob_ref()).shake_anim == 0.0 {
            let pitch = {
                let r = &mut t.mob().random;
                (r.next_float() - r.next_float()) * 0.2 + 1.0
            };
            crate::mob::kinds::play_sound(t, "minecraft:entity.wolf.shake", 0.4, pitch);
        }
        let s = st_mut(t.mob());
        let old = s.shake_anim;
        s.shake_anim += 0.05;
        if old >= 2.0 {
            s.wet = false;
            s.shaking = false;
            s.shake_anim = 0.0;
        }
    }
}

/// `Wolf.aiStep`: starts shaking when wet on the ground; the anger.
pub fn ai_step(t: &mut Tick<'_, '_>) {
    let s = st(t.mob_ref());
    if s.wet && !s.shaking && t.nav_ref().is_done() && t.on_ground() {
        let s = st_mut(t.mob());
        s.shaking = true;
        s.shake_anim = 0.0;
        let id = t.id;
        crate::mob::living::entity_event(t, id, 8);
    }
    let before = t.mob_ref().anger_end;
    crate::mob::common::update_persistent_anger(t, true, (400, 780));
    if t.mob_ref().anger_end != before {
        t.core_mut().mark_data_dirty();
    }
}

/// `hurtServer` and `actuallyHurt`: stands up; wolf armour takes the damage.
pub fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
    // `#bypasses_wolf_armor`.
    let bypasses = h.env.types.is(
        h.source.kind,
        crate::survival::damage::tag::BYPASSES_INVULNERABILITY,
    ) || matches!(
        h.env.types.get(h.source.kind).id.as_str(),
        "minecraft:cramming"
            | "minecraft:drown"
            | "minecraft:dry_out"
            | "minecraft:freeze"
            | "minecraft:in_wall"
            | "minecraft:indirect_magic"
            | "minecraft:magic"
            | "minecraft:outside_border"
            | "minecraft:starve"
            | "minecraft:thorns"
            | "minecraft:wither"
    );
    let m = h.ents.mob_mut(h.target)?;
    m.order_to_sit = false;
    if !wearing_armor(m) || bypasses {
        return Some(amount);
    }
    let mut armor = m.equipment.get(EquipmentSlot::Body).clone();
    let before = crate::item::damage(&armor);
    let max = crate::item::max_damage(&armor);
    // `hurtAndBreak(ceil(damage), this, BODY)`: Unbreaking from the level random.
    #[allow(clippy::cast_possible_truncation)]
    let change = crate::durability::change(&armor, amount.ceil() as i32, false, h.env.random);
    let d = before + change;
    let broke = change != 0 && crate::durability::apply(&mut armor, d);
    m.equipment.set(EquipmentSlot::Body, armor);
    // `Crackiness.WOLF_ARMOR`: low under 0.95, medium under 0.69, high under 0.32 of the
    // durability left.
    let crack = |d: i32| {
        #[allow(clippy::cast_precision_loss)]
        let f = (max - d) as f32 / max.max(1) as f32;
        if f < 0.32 {
            3
        } else if f < 0.69 {
            2
        } else if f < 0.95 {
            1
        } else {
            0
        }
    };
    if broke {
        // `onEquippedItemBroken`: the body slot's break event (`item.wolf_armor.break`).
        crate::mob::animal::entity_event(
            h.ents,
            h.env,
            h.target,
            crate::durability::break_event(EquipmentSlot::Body),
        );
    }
    // `Crackiness.WOLF_ARMOR` of the armour now (none once it broke).
    let after = if broke { 0 } else { crack(d) };
    if crack(before) != after {
        // `playSound(WOLF_ARMOR_CRACK)`: volume and pitch 1, no voice pitch draws.
        crate::mob::animal::sound_at(
            h.ents,
            h.env,
            h.target,
            "minecraft:entity.wolf.armor_crack",
            vanillite_protocol::packets::play::clientbound::SoundSource::Neutral,
            1.0,
        );
    }
    Some(0.0)
}

/// `Wolf.mobInteract`.
pub fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
    let Some(m) = ix.ents.mob(ix.target) else {
        return Interaction::Pass;
    };
    let (tame, baby) = (m.tame, m.baby);
    let food = crate::mob::fam::is_food(Kind::Wolf, ix.stack.item);
    if tame {
        let owned = owned_by(ix);
        if food && m.health < m.max_health() {
            super::feed(ix, 2.0, 2.0, None);
            return Interaction::Success;
        }
        if in_tag(ix.stack, "minecraft:wolf_collar_dyes") && owned {
            if let Some(c) = super::dye_color(ix.stack)
                && c != st(m).collar
            {
                if let Some(m) = ix.ents.mob_mut(ix.target) {
                    st_mut(m).collar = c;
                }
                consume(ix);
                dirty(ix);
                return Interaction::Success;
            }
            return super::animal_interact(ix);
        }
        let wearing = !m.equipment.get(EquipmentSlot::Body).is_empty();
        if item_name(ix.stack) == "minecraft:wolf_armor" && !wearing && owned && !baby {
            let one = ix.stack.with_count(1);
            consume(ix);
            if let Some(m) = ix.ents.mob_mut(ix.target) {
                m.equipment.set(EquipmentSlot::Body, one);
                m.equipment.set_guaranteed_drop(EquipmentSlot::Body);
            }
            super::horse::send_equipment(ix);
            sound(ix, "minecraft:item.armor.equip_wolf", 1.0, 1.0);
            return Interaction::Success;
        }
        let m = ix.ents.mob(ix.target).map(|m| {
            (
                m.in_sitting_pose,
                m.equipment.get(EquipmentSlot::Body).clone(),
            )
        });
        if let Some((sitting, armor)) = m
            && sitting
            && !armor.is_empty()
            && owned
            && crate::item::damage(&armor) > 0
            && item_name(ix.stack) == "minecraft:armadillo_scute"
        {
            consume(ix);
            sound(ix, "minecraft:item.wolf_armor.repair", 1.0, 1.0);
            let mut armor = armor;
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let unit = (crate::item::max_damage(&armor) as f32 * 0.125) as i32;
            let d = (crate::item::damage(&armor) - unit).max(0);
            crate::item::set_damage(&mut armor, d);
            if let Some(m) = ix.ents.mob_mut(ix.target) {
                m.equipment.set(EquipmentSlot::Body, armor);
            }
            super::horse::send_equipment(ix);
            return Interaction::Success;
        }
        let r = super::animal_interact(ix);
        if !r.consumes() && owned {
            if let Some(m) = ix.ents.mob_mut(ix.target) {
                m.order_to_sit = !m.order_to_sit;
                m.jumping = false;
                m.nav.stop();
                m.target = None;
            }
            return Interaction::Success;
        }
        return r;
    }
    let angry = m.anger_end > 0 && m.anger_end > ix.env.game_time;
    if item_name(ix.stack) == "minecraft:bone" && !angry {
        consume(ix);
        let owner = super::player_uuid(ix);
        let tamed = ix
            .ents
            .mob_mut(ix.target)
            .is_some_and(|m| m.random.next_int_bounded(3) == 0)
            && owner.is_none_or(|o| crate::mob::tame::tame_event(ix.ents, ix.target, o));
        if tamed {
            if let (Some(m), Some(o)) = (ix.ents.mob_mut(ix.target), owner) {
                super::tame_by(m, o);
                apply_taming_side_effects(m);
                m.nav.stop();
                m.target = None;
                m.order_to_sit = true;
            }
            let (target, player) = (ix.target, ix.player);
            crate::mob::tame::award_tame(ix.ents, target, player);
            event(ix, 7);
        } else {
            event(ix, 6);
        }
        dirty(ix);
        return Interaction::Success;
    }
    super::animal_interact(ix)
}

/// The wolf's synched data.
pub fn encode(w: &mut DataWriter<'_>, m: &MobData) {
    super::encode_tamable(w, m);
    let s = st(m);
    w.boolean(20, s.interested, false);
    w.int(21, i32::from(s.collar), 14);
    super::encode_long(w, 22, m.anger_end, -1);
    super::encode_holder(w, 23, serializer::WOLF_VARIANT, m.variant);
    super::encode_holder(
        w,
        24,
        serializer::WOLF_SOUND_VARIANT,
        i32::from(s.sound_variant),
    );
}

/// `Wolf.addAdditionalSaveData`.
pub fn save(m: &MobData, out: &mut Compound) {
    out.insert("CollarColor", i8::try_from(st(m).collar).unwrap_or(14));
    let v = VARIANT_NAMES
        .get(usize::try_from(m.variant).unwrap_or(3))
        .copied()
        .unwrap_or("pale");
    out.insert("variant", format!("minecraft:{v}"));
    crate::mob::common::save_anger(m, out);
    let sv = SOUND_NAMES
        .get(usize::from(st(m).sound_variant))
        .copied()
        .unwrap_or("classic");
    out.insert("sound_variant", format!("minecraft:{sv}"));
}

/// `Wolf.readAdditionalSaveData`.
pub fn load(m: &mut MobData, tag: &Compound) {
    if let Some(v) = super::string(tag, "variant") {
        let v = v.strip_prefix("minecraft:").unwrap_or(v);
        if let Some(i) = VARIANT_NAMES.iter().position(|n| *n == v) {
            m.variant = i32::try_from(i).unwrap_or(3);
        }
    }
    st_mut(m).collar =
        u8::try_from(super::int(tag, "CollarColor").unwrap_or(14).clamp(0, 15)).unwrap_or(14);
    crate::mob::common::load_anger(m, tag, 0);
    m.target = None;
    // `TamableAnimal.readAdditionalSaveData`: a wolf without an owner gets
    // `setTame(false, true)`, whose side effect sets the wild maximum health (after the
    // attributes and the health were read); a tamed one keeps its saved value.
    if !m.tame {
        use crate::effect::attribute::Attribute;
        crate::effect::living::set_mob_attribute_base(m, Attribute::MAX_HEALTH, 8.0);
        let dirty = m.effects.modifiers.take_dirty();
        crate::effect::living::apply_mob_modifiers(m, &dirty);
        for a in dirty {
            m.effects.modifiers.touch(a);
        }
        m.health = m.health.min(m.max_health());
    }
    if let Some(v) = super::string(tag, "sound_variant") {
        let v = v.strip_prefix("minecraft:").unwrap_or(v);
        if let Some(i) = SOUND_NAMES.iter().position(|n| *n == v) {
            st_mut(m).sound_variant = u8::try_from(i).unwrap_or(SOUND_CLASSIC);
        }
    }
}

/// `checkWolfSpawnRules`: `#wolves_spawnable_on` below and bright enough.
pub fn spawn_rules(level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    super::below_is(
        level,
        pos,
        &[
            "minecraft:grass_block",
            "minecraft:snow",
            "minecraft:snow_block",
            "minecraft:coarse_dirt",
            "minecraft:podzol",
        ],
    ) && super::bright_enough(level, pos, reason)
}

// --- Goals --------------------------------------------------------------------------------

/// The things a wolf begs for: bones and its food.
fn interesting(pd: &crate::player::PlayerData) -> bool {
    super::holds(pd, |s| {
        item_name(s) == "minecraft:bone" || crate::mob::fam::is_food(Kind::Wolf, s.item)
    })
}

pub fn can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::AvoidLlama(cg) => {
            if !crate::mob::common::can_use(t, cg) {
                return false;
            }
            let CommonGoal::Avoid { to_avoid, .. } = cg else {
                return false;
            };
            let strength = to_avoid
                .and_then(|id| t.ents.mob(id))
                .map_or(0, |m| st(m).strength);
            !t.mob_ref().tame && strength >= t.mob().random.next_int_bounded(5)
        }
        Goal::Beg { player, .. } => {
            *player = super::nearest_player(t, 8.0, |_| true);
            player.is_some_and(|p| t.ents.player(p).is_some_and(interesting))
        }
    }
}

pub fn can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::AvoidLlama(cg) => crate::mob::common::can_continue(t, cg),
        Goal::Beg { player, look_time } => {
            let Some(p) = *player else {
                return false;
            };
            let Some(o) = t.other(p) else {
                return false;
            };
            if o.pos.distance_to_sqr(t.pos()) > 64.0 {
                return false;
            }
            *look_time > 0 && t.ents.player(p).is_some_and(interesting)
        }
    }
}

pub fn start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::AvoidLlama(cg) => {
            t.mob().target = None;
            crate::mob::common::start(t, cg);
        }
        Goal::Beg { look_time, .. } => {
            st_mut(t.mob()).interested = true;
            t.core_mut().mark_data_dirty();
            *look_time = reduced(40 + t.mob().random.next_int_bounded(40));
        }
    }
}

pub fn stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::AvoidLlama(cg) => crate::mob::common::stop(t, cg),
        Goal::Beg { player, .. } => {
            st_mut(t.mob()).interested = false;
            t.core_mut().mark_data_dirty();
            *player = None;
        }
    }
}

pub fn tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::AvoidLlama(cg) => crate::mob::common::tick(t, cg),
        Goal::Beg { player, look_time } => {
            if let Some(o) = player.and_then(|p| t.other(p)) {
                let max_x = if t.mob_ref().in_sitting_pose {
                    20.0
                } else {
                    40.0
                };
                t.mob()
                    .look
                    .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 10.0, max_x);
            }
            *look_time -= 1;
        }
    }
}
