//! Raiders (witch, pillager, vindicator, evoker, illusioner, ravager), vexes,
//! ghasts, blazes and shulkers, with their projectiles (fireballs, shulker bullets, evoker
//! fangs, thrown potions) and the pillager patrols.
//!
//! Raids are in [`crate::raid`]; the raiders' part (joining, the raid goals, banners, death)
//! is in [`raider`].

pub mod blaze;
pub mod ghast;
pub mod missiles;
pub mod patrol;
pub mod pillager;
pub mod potion;
pub mod raider;
pub mod ravager;
pub mod shulker;
pub mod spellcaster;
pub mod vex;
pub mod witch;

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::entity::ATTRIBUTES;
use vanillite_nbt::{Compound, Tag};
use vanillite_world::entity::{Dimensions, EntityId};

use super::super::common::{self, CommonGoal};
use super::super::goals::{self, Goal as MobGoal, TargetState};
use super::super::kinds::{DefaultSound, Kind};
use super::super::nav::{LookKind, MoveKind, NavKind};
use super::super::path::{MalusTable, PathType};
use super::super::spawn;
use super::super::{Environment, MobData, Tick};
use super::{Ext, FamGoal, FamilyHooks, Hurt, SpawnContext, SpawnReason};
use crate::item::EquipmentSlot;
use crate::level::Level;
use crate::player::encode::DataWriter;
use crate::survival::damage::DamageSource;
use crate::survival::player::Env;
use crate::survival::rules::Difficulty;

/// The family's hooks.
#[derive(Debug)]
pub struct Hooks;

/// The kind state of the family's mobs (in [`Ext::Illager`]); each kind uses its part.
#[derive(Clone, Debug)]
pub struct State {
    // `PatrollingMonster` and `Raider`.
    pub patrol_target: Option<BlockPos>,
    pub patrol_leader: bool,
    pub patrolling: bool,
    pub celebrating: bool,
    pub can_join_raid: bool,
    pub wave: i32,
    /// `Raider.raid`: the id of the raid in the level's raids.
    pub raid: Option<i32>,
    /// `ticksOutsideRaid`.
    pub ticks_outside_raid: i32,
    /// `ObtainRaidLeaderBannerGoal.unreachableBannerCache`: banner item and until when.
    pub banner_cache: Vec<(EntityId, i64)>,
    // Witch.
    pub drinking: bool,
    pub using_time: i32,
    pub heal_cooldown: i32,
    pub speed_before_drinking: Option<f64>,
    // Pillager.
    pub charging_crossbow: bool,
    /// The loading sounds the crossbow played.
    pub crossbow_sounds: crate::crossbow::UseFlags,
    // Vindicator.
    pub johnny: bool,
    // Spellcasters.
    pub spell_ticks: i32,
    pub spell: u8,
    pub wololo_target: Option<EntityId>,
    // Vex.
    pub owner: Option<u128>,
    /// The owner entity once found (checked against `owner`).
    pub owner_id: Option<EntityId>,
    pub bound_origin: Option<BlockPos>,
    pub life_ticks: Option<i32>,
    pub vex_flags: u8,
    // Ravager.
    pub attack_tick: i32,
    pub stunned_tick: i32,
    pub roar_tick: i32,
    /// `randomStrollGoal.trigger()`.
    pub stroll_triggered: bool,
    // Ghast.
    pub ghast_charging: bool,
    pub explosion_power: i32,
    pub float_duration: i32,
    // Blaze.
    pub blaze_flags: u8,
    pub allowed_height_offset: f32,
    pub next_height_offset_change_tick: i32,
    // Shulker.
    /// `Direction` 3D id: down, up, north, south, west, east.
    pub attach_face: u8,
    pub peek: u8,
    pub color: u8,
    pub current_peek: f32,
    pub old_peek: f32,
    /// The closed shell's armour modifier is on.
    pub covered: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            patrol_target: None,
            patrol_leader: false,
            patrolling: false,
            celebrating: false,
            can_join_raid: false,
            wave: 0,
            raid: None,
            ticks_outside_raid: 0,
            banner_cache: Vec::new(),
            drinking: false,
            using_time: 0,
            heal_cooldown: 0,
            speed_before_drinking: None,
            charging_crossbow: false,
            crossbow_sounds: crate::crossbow::UseFlags::default(),
            johnny: false,
            spell_ticks: 0,
            spell: 0,
            wololo_target: None,
            owner: None,
            owner_id: None,
            bound_origin: None,
            life_ticks: None,
            vex_flags: 0,
            attack_tick: 0,
            stunned_tick: 0,
            roar_tick: 0,
            stroll_triggered: false,
            ghast_charging: false,
            explosion_power: 1,
            float_duration: 0,
            blaze_flags: 0,
            allowed_height_offset: 0.5,
            next_height_offset_change_tick: 0,
            attach_face: 0,
            peek: 0,
            color: 16,
            current_peek: 0.0,
            old_peek: 0.0,
            covered: false,
        }
    }
}

/// The family state of a mob (a default for mobs of other families).
pub fn st(m: &MobData) -> &State {
    static DEFAULT: std::sync::OnceLock<State> = std::sync::OnceLock::new();
    match &m.ext {
        Ext::Illager(s) => s,
        _ => DEFAULT.get_or_init(State::default),
    }
}

/// The family state of a mob, created when missing.
pub fn st_mut(m: &mut MobData) -> &mut State {
    if !matches!(m.ext, Ext::Illager(_)) {
        m.ext = Ext::Illager(State::default());
    }
    match &mut m.ext {
        Ext::Illager(s) => s,
        _ => unreachable!("set above"),
    }
}

/// A gate of a target goal (`canUse` checks before the search).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// `NearestAttackableWitchTargetGoal`: only while the witch heals no raider.
    WitchCanAttack,
    /// `VindicatorJohnnyAttackGoal`: only for Johnny.
    Johnny,
    /// `ShulkerNearestAttackGoal`: never in peaceful.
    NotPeaceful,
    /// `ShulkerDefenseAttackGoal`: only for a shulker in a scoreboard team.
    InTeam,
}

/// A goal of the family.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// Never runs.
    Idle,
    /// `PatrollingMonster.LongDistancePatrolGoal`.
    Patrol(raider::Patrol),
    /// `Raider.HoldGroundAttackGoal`.
    HoldGround { radius_sqr: f32 },
    /// `HurtByTargetGoal(this, Raider.class)` (and other ignore lists).
    HurtBy(raider::HurtBy),
    /// `LookAtPlayerGoal(this, Mob.class or Guardian.class, distance, probability)`.
    LookAt(raider::LookAt),
    /// `RandomStrollGoal` (no water avoidance).
    Stroll(raider::Stroll),
    /// A shared goal that also locks looking (`setFlags(MOVE, LOOK)`).
    Locked(CommonGoal),
    /// A `NearestAttackableTargetGoal` with a gate.
    Target { inner: CommonGoal, gate: Gate },
    /// `VindicatorBreakDoorGoal` (`breaker`) and `RaiderOpenDoorGoal`: door goals that only
    /// run during an active raid.
    RaidDoor { inner: CommonGoal, breaker: bool },
    /// `Raider.ObtainRaidLeaderBannerGoal`.
    ObtainBanner { banner: Option<EntityId> },
    /// `PathfindToRaidGoal`.
    PathfindToRaid { recruitment_tick: i32 },
    /// `Raider.RaiderMoveThroughVillageGoal(1.05, 1)`.
    MoveThroughVillage(raider::MoveThroughVillage),
    /// `Raider.RaiderCelebration`.
    Celebration,
    /// `NearestHealableRaiderTargetGoal`.
    WitchHeal(CommonGoal),
    /// `RangedCrossbowAttackGoal`.
    Crossbow(pillager::Crossbow),
    /// `SpellcasterCastingSpellGoal`.
    SpellCasting,
    /// `SpellcasterUseSpellGoal` and its spells.
    Spell(spellcaster::UseSpell),
    /// `VexChargeAttackGoal`.
    VexCharge,
    /// `VexRandomMoveGoal`.
    VexRandomMove,
    /// `VexCopyOwnerTargetGoal`.
    VexCopyOwner(TargetState),
    /// `Ghast.RandomFloatAroundGoal`.
    GhastFloat,
    /// `Ghast.GhastLookGoal`.
    GhastLook,
    /// `Ghast.GhastShootFireballGoal`.
    GhastShoot { charge_time: i32 },
    /// `Blaze.BlazeAttackGoal`.
    BlazeAttack(blaze::Attack),
    /// `Shulker.ShulkerAttackGoal`.
    ShulkerAttack { attack_time: i32 },
    /// `Shulker.ShulkerPeekGoal`.
    ShulkerPeek { peek_time: i32 },
}

fn fam(g: Goal) -> MobGoal {
    MobGoal::Fam(FamGoal::Illager(g))
}

/// Whether the kind is an `AbstractIllager`.
pub fn is_illager(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Pillager | Kind::Vindicator | Kind::Evoker | Kind::Illusioner
    )
}

/// Whether the kind is a `Raider`.
pub fn is_raider(kind: Kind) -> bool {
    is_illager(kind) || matches!(kind, Kind::Witch | Kind::Ravager)
}

/// The default value of an attribute of a kind.
pub fn attr_default(kind: Kind, name: &str) -> f64 {
    let info = kind.type_id().info();
    let Some(id) = ATTRIBUTES.iter().position(|a| a.name == name) else {
        return 0.0;
    };
    info.attributes
        .iter()
        .find(|(a, _)| usize::from(*a) == id)
        .map_or(ATTRIBUTES[id].default, |&(_, v)| v)
}

/// `Mob.getBaseExperienceReward` with `xpReward`: one to three more per worn item.
fn reward_with_equipment(m: &mut MobData, base: i32) -> i32 {
    let mut result = base;
    for slot in [
        EquipmentSlot::MainHand,
        EquipmentSlot::OffHand,
        EquipmentSlot::Feet,
        EquipmentSlot::Legs,
        EquipmentSlot::Chest,
        EquipmentSlot::Head,
    ] {
        if !m.equipment.get(slot).is_empty() && m.equipment.drop_chance(slot) <= 1.0 {
            result += 1 + m.random.next_int_bounded(3);
        }
    }
    result
}

/// Marks the ticking mob's synched data dirty.
pub fn dirty(t: &mut Tick<'_, '_>) {
    t.core_mut().mark_data_dirty();
}

/// Plays a sound of the ticking mob.
pub fn sound(t: &mut Tick<'_, '_>, name: &str, volume: f32, pitch: f32) {
    super::super::kinds::play_sound(t, name, volume, pitch);
}

/// `Mob.checkMobSpawnRules`: a valid spawn floor (spawners skip it).
pub fn check_mob_spawn_rules(
    kind: Kind,
    level: &Level,
    pos: BlockPos,
    reason: SpawnReason,
) -> bool {
    if matches!(reason, SpawnReason::Spawner | SpawnReason::TrialSpawner) {
        return true;
    }
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    spawn::valid_spawn_floor(level.block(below), kind.type_id())
}

/// `Monster.checkMonsterSpawnRules`.
fn check_monster_spawn_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    let ignores_light = matches!(reason, SpawnReason::TrialSpawner);
    (ignores_light || spawn::is_dark_enough_to_spawn(level, env, pos, random))
        && check_mob_spawn_rules(kind, level, pos, reason)
}

impl FamilyHooks for Hooks {
    /// `PatrollingMonster.removeWhenFarAway`: patrols only despawn beyond 128 blocks.
    fn remove_when_far_away_mob(m: &MobData, dist_sqr: f64) -> bool {
        if is_raider(m.kind) {
            // `Raider.removeWhenFarAway`: never while in a raid.
            if st(m).raid.is_some() {
                return false;
            }
            return !st(m).patrolling || dist_sqr > 16384.0;
        }
        Self::remove_when_far_away(m.kind)
    }

    /// `Raider.requiresCustomPersistence`: raiders of a raid stay.
    fn requires_custom_persistence(m: &MobData) -> bool {
        is_raider(m.kind) && st(m).raid.is_some()
    }

    type Goal = Goal;

    fn sensitive_to_water(kind: Kind) -> bool {
        kind == Kind::Blaze
    }

    fn fall_damage_immune(kind: Kind) -> bool {
        matches!(kind, Kind::Shulker | Kind::Blaze | Kind::Ghast)
    }

    fn malus(kind: Kind) -> &'static MalusTable {
        const BLAZE: &MalusTable = &[
            (PathType::Water, -1.0),
            (PathType::Lava, 8.0),
            (PathType::FireInNeighbor, 0.0),
            (PathType::Fire, 0.0),
        ];
        const RAVAGER: &MalusTable = &[(PathType::Leaves, 0.0)];
        match kind {
            Kind::Blaze => BLAZE,
            Kind::Ravager => RAVAGER,
            _ => &[],
        }
    }

    fn sound_volume(kind: Kind) -> f32 {
        if kind == Kind::Ghast { 5.0 } else { 1.0 }
    }

    fn ambient_sound(m: &mut MobData) -> Option<&'static str> {
        match m.kind {
            Kind::Shulker if st(m).peek == 0 => None,
            _ => Some(m.kind.default_sound(DefaultSound::Ambient)),
        }
    }

    fn hurt_sound(m: &MobData) -> &'static str {
        match m.kind {
            Kind::Shulker if st(m).peek == 0 => "minecraft:entity.shulker.hurt_closed",
            _ => m.kind.default_sound(DefaultSound::Hurt),
        }
    }

    fn death_sound(m: &MobData) -> &'static str {
        m.kind.default_sound(DefaultSound::Death)
    }

    fn experience_reward(m: &mut MobData) -> i32 {
        let base = match m.kind {
            Kind::Evoker | Kind::Blaze => 10,
            Kind::Ravager => 20,
            Kind::Vex => 3,
            _ => 5,
        };
        reward_with_equipment(m, base)
    }

    fn dimensions(m: &MobData) -> Option<Dimensions> {
        if m.kind != Kind::Shulker {
            return None;
        }
        let d = Dimensions::of(m.kind.type_id());
        let s = st(m);
        // `getDefaultDimensions`: a shulker on the floor grows with its lid.
        (s.attach_face == 0 && s.current_peek > 0.0).then_some(Dimensions {
            width: d.width,
            height: d.height * (1.0 + s.current_peek),
        })
    }

    fn walk_target_value(kind: Kind, level: &Level, env: &Environment, pos: BlockPos) -> f64 {
        match kind {
            Kind::Pillager => 0.0,
            _ => super::super::kinds::base_walk_target_value(true, level, env, pos),
        }
    }

    fn check_spawn_rules(
        kind: Kind,
        level: &Level,
        env: &Environment,
        pos: BlockPos,
        random: &mut LegacyRandom,
        reason: SpawnReason,
    ) -> bool {
        match kind {
            Kind::Pillager => {
                // `checkPatrollingMonsterSpawnRules`: dark enough for block light, any sky
                // light.
                level.light(pos).1 <= 8 && check_mob_spawn_rules(kind, level, pos, reason)
            }
            Kind::Blaze => check_mob_spawn_rules(kind, level, pos, reason),
            Kind::Shulker => check_mob_spawn_rules(kind, level, pos, reason),
            Kind::Ghast => ghast::check_spawn_rules(kind, level, pos, random, reason),
            _ => check_monster_spawn_rules(kind, level, env, pos, random, reason),
        }
    }

    fn init(m: &mut MobData) {
        m.ext = Ext::Illager(State::default());
        match m.kind {
            Kind::Vex => {
                m.move_ctl.kind = MoveKind::Custom;
            }
            Kind::Ghast => {
                m.nav.kind = NavKind::None;
                m.move_ctl.kind = MoveKind::Custom;
            }
            Kind::Shulker => {
                m.nav.kind = NavKind::None;
                m.look.kind = LookKind::Custom;
            }
            _ => {}
        }
    }

    fn register_goals(m: &mut MobData) {
        match m.kind {
            Kind::Witch => witch::register_goals(m),
            Kind::Pillager => pillager::register_goals(m),
            Kind::Vindicator => raider::register_vindicator(m),
            Kind::Evoker | Kind::Illusioner => spellcaster::register_goals(m),
            Kind::Ravager => ravager::register_goals(m),
            Kind::Vex => vex::register_goals(m),
            Kind::Ghast => ghast::register_goals(m),
            Kind::Blaze => blaze::register_goals(m),
            Kind::Shulker => shulker::register_goals(m),
            _ => {}
        }
    }

    fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
        // Each kind's own part runs before or after `super.finalizeSpawn` like Vanilla's.
        match m.kind {
            Kind::Pillager => {
                pillager::finalize_spawn(m, ctx);
                raider::finalize_spawn(m, ctx);
            }
            Kind::Vindicator => {
                raider::finalize_spawn(m, ctx);
                raider::finalize_vindicator(m, ctx);
            }
            Kind::Illusioner => {
                m.equipment.set(
                    EquipmentSlot::MainHand,
                    super::super::equipment::stack("minecraft:bow"),
                );
                raider::finalize_spawn(m, ctx);
            }
            Kind::Vex => vex::finalize_spawn(m, ctx),
            Kind::Shulker => m.y_body_rot = 0.0,
            kind if is_raider(kind) => raider::finalize_spawn(m, ctx),
            _ => {}
        }
    }

    fn pick_up_items(t: &mut Tick<'_, '_>) {
        // `Pillager.pickUpItem`: only banners (white banners of a raid go to the inventory,
        // which is not modelled).
        let rules = if t.mob_ref().kind == Kind::Pillager {
            super::super::equipment::PickupRules {
                wants: |_, s| crate::item::item_id(s).name().ends_with("_banner"),
                preferred_weapon: Some("minecraft:pillager_preferred_weapons"),
            }
        } else {
            super::super::equipment::PickupRules::default()
        };
        let pick_up: Option<fn(&mut Tick<'_, '_>, EntityId) -> bool> =
            if is_raider(t.mob_ref().kind) {
                Some(raider::pick_up_item)
            } else {
                None
            };
        super::super::equipment::pick_up_items(t, &rules, pick_up, (1.0, 0.0, 1.0));
    }

    fn before_tick(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            Kind::Pillager | Kind::Illusioner => {
                let m = t.mob();
                if m.using_item >= 0 {
                    m.using_item += 1;
                }
                if t.mob_ref().kind == Kind::Pillager {
                    pillager::use_tick(t);
                }
            }
            Kind::Vex => vex::before_tick(t),
            Kind::Shulker => shulker::before_tick(t),
            _ => {}
        }
    }

    fn after_tick(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            Kind::Vex => vex::after_tick(t),
            Kind::Shulker => shulker::after_tick(t),
            _ => {}
        }
    }

    fn ai_step_start(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            Kind::Witch => witch::ai_step_start(t),
            Kind::Blaze => blaze::ai_step_start(t),
            _ => {}
        }
        if t.exists() && is_raider(t.mob_ref().kind) {
            raider::ai_step(t);
        }
    }

    fn ai_step(t: &mut Tick<'_, '_>) {
        if t.mob_ref().kind == Kind::Ravager {
            ravager::ai_step(t);
        }
    }

    fn is_immobile(m: &MobData) -> bool {
        m.kind == Kind::Ravager && {
            let s = st(m);
            s.attack_tick > 0 || s.stunned_tick > 0 || s.roar_tick > 0
        }
    }

    fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            Kind::Blaze => blaze::custom_server_ai_step(t),
            Kind::Vindicator => {
                // `setCanOpenDoors(level.isRaided(pos))`.
                if !t.mob_ref().no_ai && t.mob_ref().nav.is_ground() {
                    let raided = t.ents.raids.is_raided(t.block_pos());
                    t.mob().nav.can_open_doors = raided;
                }
            }
            Kind::Evoker | Kind::Illusioner => {
                let s = st_mut(t.mob());
                if s.spell_ticks > 0 {
                    s.spell_ticks -= 1;
                }
            }
            _ => {}
        }
    }

    fn travel(t: &mut Tick<'_, '_>, input: Vec3) -> bool {
        match t.mob_ref().kind {
            Kind::Ghast => {
                travel_flying(t, input, 0.02);
                true
            }
            Kind::Vex => {
                vex::travel(t, input);
                true
            }
            Kind::Shulker => {
                t.set_velocity(Vec3::ZERO);
                true
            }
            _ => false,
        }
    }

    fn move_control(t: &mut Tick<'_, '_>) -> bool {
        match t.mob_ref().kind {
            Kind::Ghast => ghast::move_control(t),
            Kind::Vex => vex::move_control(t),
            _ => return false,
        }
        true
    }

    fn look_control(t: &mut Tick<'_, '_>) -> bool {
        if t.mob_ref().kind == Kind::Shulker {
            shulker::look_control(t);
            return true;
        }
        false
    }

    fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
        if t.mob_ref().kind == Kind::Ravager {
            return Some(ravager::do_hurt_target(t, target));
        }
        None
    }

    fn perform_ranged_attack(t: &mut Tick<'_, '_>, target: EntityId, power: f32) {
        match t.mob_ref().kind {
            Kind::Witch => witch::perform_ranged_attack(t, target),
            Kind::Pillager => pillager::perform_crossbow_attack(t, target),
            Kind::Illusioner => super::super::projectile::skeleton_shoot(t, target, power),
            _ => {}
        }
    }

    fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
        let kind = h.ents.mob(h.target)?.kind;
        if is_raider(kind) {
            // `Raider.hurtServer`: the raid bar before the damage.
            crate::raid::on_raider_hurt(h.ents, h.target, h.env.out);
        }
        match kind {
            // `noPhysics` during its tick: a vex never suffocates in blocks.
            Kind::Vex if h.source.kind == h.env.types.in_wall => None,
            Kind::Ghast => ghast::hurt_filter(h, amount),
            Kind::Shulker => shulker::hurt_filter(h, amount),
            _ => Some(amount),
        }
    }

    fn absorb(m: &MobData, source: &DamageSource, env: &Env<'_>, damage: f32) -> f32 {
        if m.kind == Kind::Witch {
            return witch::absorb(source, env.types, damage);
        }
        damage
    }

    fn on_die(h: &mut Hurt<'_, '_>) {
        if h.ents.mob(h.target).is_some_and(|m| is_raider(m.kind)) {
            crate::raid::on_raider_death(h.ents, h.target, h.source.cause, h.env.out);
        }
    }

    fn after_hurt(h: &mut Hurt<'_, '_>, amount: f32) {
        let Some(kind) = h.ents.mob(h.target).map(|m| m.kind) else {
            return;
        };
        if kind == Kind::Shulker {
            shulker::after_hurt(h, amount);
        }
    }

    fn encode_data(w: &mut DataWriter<'_>, m: &MobData) {
        let s = st(m);
        if is_raider(m.kind) {
            w.boolean(16, s.celebrating, false);
        }
        match m.kind {
            Kind::Witch => w.boolean(17, s.drinking, false),
            Kind::Pillager => w.boolean(17, s.charging_crossbow, false),
            Kind::Evoker | Kind::Illusioner => w.byte(17, s.spell, 0),
            Kind::Vex => w.byte(16, s.vex_flags, 0),
            Kind::Ghast => w.boolean(16, s.ghast_charging, false),
            Kind::Blaze => w.byte(16, s.blaze_flags, 0),
            Kind::Shulker => {
                w.varint(
                    16,
                    crate::player::encode::serializer::DIRECTION,
                    i32::from(s.attach_face),
                    0,
                );
                w.byte(17, s.peek, 0);
                w.byte(18, s.color, 16);
            }
            _ => {}
        }
    }

    fn living_flags(m: &MobData) -> u8 {
        u8::from(m.using_item >= 0)
    }

    fn save(m: &MobData, out: &mut Compound) {
        let s = st(m);
        if is_raider(m.kind) {
            if let Some(p) = s.patrol_target {
                out.insert("patrol_target", vec![p.x, p.y, p.z]);
            }
            out.insert("PatrolLeader", s.patrol_leader);
            out.insert("Patrolling", s.patrolling);
            out.insert("Wave", s.wave);
            out.insert("CanJoinRaid", s.can_join_raid);
            if let Some(id) = s.raid {
                out.insert("RaidId", id);
            }
        }
        match m.kind {
            Kind::Vindicator if s.johnny => out.insert("Johnny", true),
            Kind::Evoker | Kind::Illusioner => out.insert("SpellTicks", s.spell_ticks),
            Kind::Vex => vex::save(s, out),
            Kind::Ravager => {
                out.insert("AttackTick", s.attack_tick);
                out.insert("StunTick", s.stunned_tick);
                out.insert("RoarTick", s.roar_tick);
            }
            Kind::Ghast => out.insert(
                "ExplosionPower",
                i8::try_from(s.explosion_power).unwrap_or(1),
            ),
            Kind::Shulker => {
                out.insert("AttachFace", i8::try_from(s.attach_face).unwrap_or(0));
                out.insert("Peek", i8::try_from(s.peek).unwrap_or(0));
                out.insert("Color", i8::try_from(s.color).unwrap_or(16));
            }
            _ => {}
        }
    }

    fn load(m: &mut MobData, tag: &Compound) {
        let kind = m.kind;
        let s = st_mut(m);
        let int = |k: &str| crate::mob::entity_nbt::get_int(tag, k);
        let boolean = |k: &str| crate::mob::entity_nbt::get_bool(tag, k).unwrap_or(false);
        if is_raider(kind) {
            s.patrol_target = match tag.get("patrol_target") {
                Some(Tag::IntArray(v)) if v.len() == 3 => Some(BlockPos::new(v[0], v[1], v[2])),
                _ => None,
            };
            s.patrol_leader = boolean("PatrolLeader");
            s.patrolling = boolean("Patrolling");
            s.wave = int("Wave").unwrap_or(0);
            s.can_join_raid = boolean("CanJoinRaid");
            // Rejoined when the chunk is live (`crate::raid::rejoin_loaded`).
            s.raid = int("RaidId");
        }
        match kind {
            Kind::Vindicator => s.johnny = boolean("Johnny"),
            Kind::Evoker | Kind::Illusioner => s.spell_ticks = int("SpellTicks").unwrap_or(0),
            Kind::Vex => vex::load(s, tag),
            Kind::Ravager => {
                s.attack_tick = int("AttackTick").unwrap_or(0);
                s.stunned_tick = int("StunTick").unwrap_or(0);
                s.roar_tick = int("RoarTick").unwrap_or(0);
            }
            Kind::Ghast => s.explosion_power = int("ExplosionPower").unwrap_or(1),
            Kind::Shulker => {
                s.attach_face =
                    u8::try_from(int("AttachFace").unwrap_or(0).clamp(0, 5)).unwrap_or(0);
                s.peek = u8::try_from(int("Peek").unwrap_or(0).clamp(0, 100)).unwrap_or(0);
                s.color = u8::try_from(int("Color").unwrap_or(16).clamp(0, 16)).unwrap_or(16);
                s.current_peek = f32::from(s.peek) * 0.01;
            }
            _ => {}
        }
        if kind == Kind::Pillager {
            // `readAdditionalSaveData` turns pickup on.
            m.can_pick_up_loot = true;
        }
        if kind == Kind::Vindicator
            && let Some(name) = m.custom_name.as_deref()
            && raider::is_johnny_name(name)
        {
            st_mut(m).johnny = true;
        }
    }

    // --- Projectiles ----------------------------------------------------------------------

    fn missile_tick(t: &mut super::super::missile::MissileTick<'_, '_>) {
        missiles::tick(t);
    }

    fn missile_save(d: &super::super::missile::MissileData, out: &mut Compound) {
        missiles::save(d, out);
    }

    fn missile_load(d: &mut super::super::missile::MissileData, tag: &Compound) {
        missiles::load(d, tag);
    }

    fn missile_encode(w: &mut DataWriter<'_>, d: &super::super::missile::MissileData) {
        missiles::encode(w, d);
    }

    // --- Goals ------------------------------------------------------------------------

    fn goal_flags(g: &Goal) -> u8 {
        use goals::flag::*;
        match g {
            Goal::Idle | Goal::Spell(_) => 0,
            Goal::WitchHeal(_) => TARGET,
            Goal::Stroll(s) if s.look => MOVE | LOOK,
            Goal::Locked(_) => MOVE | LOOK,
            Goal::Patrol(_) | Goal::Stroll(_) | Goal::GhastFloat | Goal::VexCharge => MOVE,
            Goal::RaidDoor { breaker, .. } => {
                if *breaker {
                    MOVE
                } else {
                    0
                }
            }
            Goal::ObtainBanner { .. }
            | Goal::PathfindToRaid { .. }
            | Goal::MoveThroughVillage(_)
            | Goal::Celebration => MOVE,
            Goal::VexRandomMove => MOVE,
            Goal::HoldGround { .. }
            | Goal::Crossbow(_)
            | Goal::SpellCasting
            | Goal::BlazeAttack(_)
            | Goal::ShulkerAttack { .. } => MOVE | LOOK,
            Goal::HurtBy(_) | Goal::Target { .. } => TARGET,
            Goal::LookAt(_) | Goal::GhastLook => LOOK,
            // `VexCopyOwnerTargetGoal` sets no flags: it does not lock the target slot.
            Goal::GhastShoot { .. } | Goal::ShulkerPeek { .. } | Goal::VexCopyOwner(_) => 0,
        }
    }

    fn goal_every_tick(g: &Goal) -> bool {
        match g {
            Goal::HoldGround { .. }
            | Goal::Crossbow(_)
            | Goal::VexCharge
            | Goal::GhastLook
            | Goal::GhastShoot { .. }
            | Goal::BlazeAttack(_)
            | Goal::ShulkerAttack { .. } => true,
            Goal::Target { inner, .. } | Goal::RaidDoor { inner, .. } => inner.every_tick(),
            _ => false,
        }
    }

    fn goal_can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
        match g {
            Goal::Idle => false,
            Goal::Patrol(p) => raider::patrol_can_use(t, p),
            Goal::HoldGround { .. } => raider::hold_ground_can_use(t),
            Goal::HurtBy(h) => raider::hurt_by_can_use(t, h),
            Goal::LookAt(l) => raider::look_at_can_use(t, l),
            Goal::Stroll(s) => raider::stroll_can_use(t, s),
            Goal::Locked(inner) => common::can_use(t, inner),
            Goal::Target { inner, gate } => {
                let open = match gate {
                    Gate::WitchCanAttack => st(t.mob_ref()).heal_cooldown <= 0,
                    Gate::Johnny => {
                        // `setCustomName("Johnny")` makes it Johnny for good.
                        let named = t
                            .mob_ref()
                            .custom_name
                            .as_deref()
                            .is_some_and(raider::is_johnny_name);
                        if named {
                            st_mut(t.mob()).johnny = true;
                        }
                        st(t.mob_ref()).johnny
                    }
                    Gate::NotPeaceful => t.env.difficulty != Difficulty::Peaceful,
                    Gate::InTeam => shulker::team_of(t, t.id).is_some(),
                };
                open && common::can_use(t, inner)
            }
            Goal::RaidDoor { inner, breaker } => {
                let raid = crate::raid::has_active_raid(t.ents, t.mob_ref());
                if *breaker {
                    raid && {
                        let r = goals::reduced(10).max(1);
                        t.mob().random.next_int_bounded(r) == 0
                    } && common::can_use(t, inner)
                } else {
                    common::can_use(t, inner) && raid
                }
            }
            Goal::ObtainBanner { banner } => raider::banner_can_use(t, banner),
            Goal::PathfindToRaid { .. } => raider::pathfind_can_use(t),
            Goal::MoveThroughVillage(v) => raider::move_through_can_use(t, v),
            Goal::Celebration => raider::celebration_can_use(t),
            Goal::WitchHeal(inner) => witch::heal_can_use(t, inner),
            Goal::Crossbow(_) => pillager::crossbow_can_use(t),
            Goal::SpellCasting => st(t.mob_ref()).spell_ticks > 0,
            Goal::Spell(s) => spellcaster::can_use(t, s),
            Goal::VexCharge => vex::charge_can_use(t),
            Goal::VexRandomMove => vex::random_move_can_use(t),
            Goal::VexCopyOwner(_) => vex::copy_owner_can_use(t),
            Goal::GhastFloat => ghast::float_can_use(t),
            Goal::GhastLook => true,
            Goal::GhastShoot { .. } => t.mob_ref().target.is_some(),
            Goal::BlazeAttack(_) => blaze::attack_can_use(t),
            Goal::ShulkerAttack { .. } => shulker::attack_can_use(t),
            Goal::ShulkerPeek { .. } => shulker::peek_can_use(t),
        }
    }

    fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
        match g {
            Goal::Patrol(p) => raider::patrol_can_use(t, p),
            Goal::HurtBy(h) => goals::target_can_continue(t, &mut h.state),
            Goal::LookAt(l) => raider::look_at_can_continue(t, l),
            Goal::Stroll(_) => !t.nav_ref().is_done(),
            Goal::Locked(inner) => common::can_continue(t, inner),
            Goal::Target { inner, .. } => common::can_continue(t, inner),
            Goal::RaidDoor { inner, breaker } => {
                if *breaker {
                    crate::raid::has_active_raid(t.ents, t.mob_ref())
                        && common::can_continue(t, inner)
                } else {
                    common::can_continue(t, inner)
                }
            }
            Goal::ObtainBanner { banner } => raider::banner_can_continue(t, banner),
            Goal::WitchHeal(inner) => common::can_continue(t, inner),
            Goal::PathfindToRaid { .. } => raider::pathfind_can_continue(t),
            Goal::MoveThroughVillage(v) => raider::move_through_can_continue(t, v),
            Goal::Crossbow(_) => pillager::crossbow_can_continue(t),
            Goal::Spell(s) => spellcaster::can_continue(t, s),
            Goal::VexCharge => vex::charge_can_continue(t),
            Goal::VexRandomMove | Goal::GhastFloat => false,
            Goal::VexCopyOwner(state) => goals::target_can_continue(t, state),
            Goal::ShulkerPeek { peek_time } => t.mob_ref().target.is_none() && *peek_time > 0,
            _ => Self::goal_can_use(t, g),
        }
    }

    fn goal_start(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::HoldGround { .. } => raider::hold_ground_start(t),
            Goal::HurtBy(h) => raider::hurt_by_start(t, h),
            Goal::LookAt(l) => {
                let r = t.mob().random.next_int_bounded(40);
                l.time = goals::reduced(40 + r);
            }
            Goal::Stroll(s) => {
                let (pos, speed) = (s.wanted, s.speed);
                super::super::nav::move_to_pos(t, pos, speed);
            }
            Goal::Locked(inner) => common::start(t, inner),
            Goal::Target { inner, .. } => {
                common::start(t, inner);
                if matches!(
                    g,
                    Goal::Target {
                        gate: Gate::Johnny,
                        ..
                    }
                ) {
                    t.mob().no_action_time = 0;
                }
            }
            Goal::RaidDoor { inner, .. } => common::start(t, inner),
            Goal::WitchHeal(inner) => {
                st_mut(t.mob()).heal_cooldown = goals::reduced(200);
                common::start(t, inner);
            }
            Goal::ObtainBanner { .. } => raider::banner_start(t),
            Goal::MoveThroughVillage(v) => raider::move_through_start(t, v),
            Goal::Celebration => raider::set_celebrating(t, true),
            Goal::SpellCasting => t.nav().stop(),
            Goal::Spell(s) => spellcaster::start(t, s),
            Goal::VexCharge => vex::charge_start(t),
            Goal::VexCopyOwner(state) => vex::copy_owner_start(t, state),
            Goal::GhastFloat => ghast::float_start(t),
            Goal::GhastShoot { charge_time } => *charge_time = 0,
            Goal::BlazeAttack(a) => a.attack_step = 0,
            Goal::ShulkerAttack { attack_time } => {
                *attack_time = 20;
                shulker::set_raw_peek(t, 100);
            }
            Goal::ShulkerPeek { peek_time } => {
                let r = t.mob().random.next_int_bounded(3);
                *peek_time = goals::reduced(20 * (1 + r));
                shulker::set_raw_peek(t, 30);
            }
            _ => {}
        }
    }

    fn goal_stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::HoldGround { .. } => raider::hold_ground_stop(t),
            Goal::HurtBy(h) => {
                goals::set_target(t, None);
                h.state.target_mob = None;
            }
            Goal::LookAt(l) => l.look_at = None,
            Goal::Stroll(_) => t.nav().stop(),
            Goal::Locked(inner) => common::stop(t, inner),
            Goal::Target { inner, .. } => common::stop(t, inner),
            Goal::RaidDoor { inner, .. } | Goal::WitchHeal(inner) => common::stop(t, inner),
            Goal::ObtainBanner { banner } => {
                t.mob().goal_path = None;
                *banner = None;
            }
            Goal::MoveThroughVillage(v) => raider::move_through_stop(t, v),
            Goal::Celebration => raider::set_celebrating(t, false),
            Goal::Crossbow(c) => pillager::crossbow_stop(t, c),
            Goal::SpellCasting => {
                st_mut(t.mob()).spell = 0;
                dirty(t);
            }
            Goal::Spell(s) => spellcaster::stop(t, s),
            Goal::VexCharge => vex::set_charging(t, false),
            Goal::VexCopyOwner(state) => {
                goals::set_target(t, None);
                state.target_mob = None;
            }
            Goal::GhastShoot { .. } => ghast::set_charging(t, false),
            Goal::BlazeAttack(a) => {
                blaze::set_charged(t, false);
                a.last_seen = 0;
            }
            Goal::ShulkerAttack { .. } => shulker::set_raw_peek(t, 0),
            Goal::ShulkerPeek { .. } if t.mob_ref().target.is_none() => {
                shulker::set_raw_peek(t, 0);
            }
            _ => {}
        }
    }

    fn goal_tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::Patrol(p) => raider::patrol_tick(t, p),
            Goal::HoldGround { radius_sqr } => raider::hold_ground_tick(t, *radius_sqr),
            Goal::LookAt(l) => raider::look_at_tick(t, l),
            Goal::Locked(inner) => common::tick(t, inner),
            Goal::Target { inner, .. } => common::tick(t, inner),
            Goal::RaidDoor { inner, .. } | Goal::WitchHeal(inner) => common::tick(t, inner),
            Goal::ObtainBanner { banner } => raider::banner_tick(t, banner),
            Goal::PathfindToRaid { recruitment_tick } => {
                raider::pathfind_tick(t, recruitment_tick);
            }
            Goal::MoveThroughVillage(v) => raider::move_through_tick(t, v),
            Goal::Celebration => raider::celebration_tick(t),
            Goal::Crossbow(c) => pillager::crossbow_tick(t, c),
            Goal::SpellCasting => spellcaster::casting_tick(t),
            Goal::Spell(s) => spellcaster::tick(t, s),
            Goal::VexCharge => vex::charge_tick(t),
            Goal::VexRandomMove => vex::random_move_tick(t),
            Goal::GhastLook => ghast::face_movement_direction(t),
            Goal::GhastShoot { charge_time } => ghast::shoot_tick(t, charge_time),
            Goal::BlazeAttack(a) => blaze::attack_tick(t, a),
            Goal::ShulkerAttack { attack_time } => shulker::attack_tick(t, attack_time),
            Goal::ShulkerPeek { peek_time } => *peek_time -= 1,
            _ => {}
        }
    }
}

/// `LivingEntity.travelFlying(input, speed)`: flight without gravity, slowed in fluids.
pub fn travel_flying(t: &mut Tick<'_, '_>, input: Vec3, speed: f32) {
    let fluids = t.mob_ref().fluids;
    let (s, scale) = if fluids.in_water() {
        (0.02, f64::from(0.8_f32))
    } else if fluids.in_lava() {
        (0.02, 0.5)
    } else {
        (speed, f64::from(0.91_f32))
    };
    super::super::living::move_relative(t, s, input);
    let v = t.velocity();
    super::super::living::move_self(t, v);
    if !t.exists() {
        return;
    }
    let v = t.velocity();
    t.set_velocity(v * scale);
}

/// Common goal constructors of the family.
fn look_at_player(distance: f32, probability: f32) -> MobGoal {
    MobGoal::LookAtPlayer {
        distance,
        probability,
        look_at: None,
        time: 0,
    }
}

/// `NearestAttackableTargetGoal(this, Player.class, mustSee)`.
fn target_players(must_see: bool) -> MobGoal {
    MobGoal::Common(CommonGoal::nearest_player(must_see, common::any_player))
}

/// `NearestAttackableTargetGoal(this, type, mustSee)` over living entities.
fn target_mobs(must_see: bool, filter: common::TargetFilter) -> MobGoal {
    MobGoal::Common(CommonGoal::nearest(10, must_see, filter))
}

/// `setUnseenMemoryTicks(300)` of a target goal.
fn with_memory(g: MobGoal, ticks: i32) -> MobGoal {
    match g {
        MobGoal::Common(CommonGoal::NearestAttackable {
            mut state,
            interval,
            found,
            filter,
            kinds,
            players_only,
            precheck,
        }) => {
            state.unseen_memory = ticks;
            MobGoal::Common(CommonGoal::NearestAttackable {
                state,
                interval,
                found,
                filter,
                kinds,
                players_only,
                precheck,
            })
        }
        other => other,
    }
}

/// `AvoidEntityGoal<>(this, Creaking.class, 8.0F, walk, sprint)`.
fn avoid_creaking(walk: f64, sprint: f64) -> MobGoal {
    MobGoal::Common(CommonGoal::avoid(
        |t, id| common::mob_of(t, id, Kind::Creaking),
        8.0,
        walk,
        sprint,
    ))
}

#[cfg(test)]
mod tests;
