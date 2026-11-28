//! `Panda`: genes (normal, lazy, worried, playful, brown, weak, aggressive), sitting and
//! eating bamboo, sneezing babies, rolling, lying on the back, breeding near bamboo.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::Compound;
use vanillite_world::entity::EntityId;

use super::{
    G, Goal as FamGoal, Interact, SpawnContext, TemptItems, consume, dirty, fam, in_tag, st, st_mut,
};
use crate::item::{EquipmentSlot, ItemStack};
use crate::mob::animal::Interaction;
use crate::mob::common::{CommonGoal, TargetFilter};
use crate::mob::equipment::{self, PickupRules};
use crate::mob::fam::Hurt;
use crate::mob::goals::{flag, reduced};
use crate::mob::kinds::Kind;
use crate::mob::{MobData, Tick, nav};
use crate::player::encode::DataWriter;
use crate::survival::rules::Rule;

const SNEEZING: u8 = 2;
const ROLLING: u8 = 4;
const SITTING: u8 = 8;
const ON_BACK: u8 = 16;

/// `Panda.Gene` ids.
pub mod gene {
    pub const NORMAL: u8 = 0;
    pub const LAZY: u8 = 1;
    pub const WORRIED: u8 = 2;
    pub const PLAYFUL: u8 = 3;
    pub const BROWN: u8 = 4;
    pub const WEAK: u8 = 5;
    pub const AGGRESSIVE: u8 = 6;
    pub const NAMES: [&str; 7] = [
        "normal",
        "lazy",
        "worried",
        "playful",
        "brown",
        "weak",
        "aggressive",
    ];
}

/// The panda goals.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// `PandaPanicGoal` (environmental causes; ends when sitting).
    Panic { pos: Vec3 },
    /// `PandaBreedGoal`'s bamboo check before the shared breed goal.
    BreedCheck { unhappy_cooldown: i32 },
    /// `PandaAvoidGoal` (worried pandas).
    Avoid(CommonGoal),
    /// `PandaSitGoal`.
    Sit { cooldown: i32 },
    /// `PandaLieOnBackGoal`.
    LieOnBack { cooldown: i32 },
    /// `PandaSneezeGoal`.
    Sneeze,
    /// `PandaRollGoal`.
    Roll,
}

pub fn flags(g: &Goal) -> u8 {
    match g {
        Goal::Panic { .. } | Goal::Avoid(_) | Goal::Sit { .. } => flag::MOVE,
        Goal::BreedCheck { .. } => flag::MOVE | flag::LOOK,
        Goal::Roll => flag::MOVE | flag::LOOK | flag::JUMP,
        Goal::LieOnBack { .. } | Goal::Sneeze => 0,
    }
}

pub fn every_tick(_g: &Goal) -> bool {
    false
}

/// `Gene.getVariantFromGenes`: brown and weak are recessive.
pub fn variant(main: u8, hidden: u8) -> u8 {
    if matches!(main, gene::BROWN | gene::WEAK) {
        if main == hidden { main } else { gene::NORMAL }
    } else {
        main
    }
}

/// `Panda.isAggressive`: the aggressive gene.
pub(crate) fn is_aggressive(m: &MobData) -> bool {
    kind_gene(m) == gene::AGGRESSIVE
}

fn kind_gene(m: &MobData) -> u8 {
    let s = st(m);
    variant(s.main_gene, s.hidden_gene)
}

/// `Gene.getRandom`.
pub fn random_gene(r: &mut LegacyRandom) -> u8 {
    match r.next_int_bounded(16) {
        0 => gene::LAZY,
        1 => gene::WORRIED,
        2 => gene::PLAYFUL,
        4 => gene::AGGRESSIVE,
        n if n < 9 => gene::WEAK,
        n if n < 11 => gene::BROWN,
        _ => gene::NORMAL,
    }
}

fn has(m: &MobData, f: u8) -> bool {
    st(m).panda_flags & f != 0
}

fn set(t: &mut Tick<'_, '_>, f: u8, on: bool) {
    let s = st_mut(t.mob());
    let before = s.panda_flags;
    if on {
        s.panda_flags |= f;
    } else {
        s.panda_flags &= !f;
    }
    if before != s.panda_flags {
        t.core_mut().mark_data_dirty();
    }
}

fn scared(t: &Tick<'_, '_>) -> bool {
    kind_gene(t.mob_ref()) == gene::WORRIED && t.cx.env.thundering
}

fn eating(m: &MobData) -> bool {
    st(m).eat > 0
}

/// `canPerformAction`.
fn can_perform(t: &Tick<'_, '_>) -> bool {
    let m = t.mob_ref();
    !has(m, ON_BACK) && !scared(t) && !eating(m) && !has(m, ROLLING) && !has(m, SITTING)
}

fn any_player(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .player(id)
        .is_some_and(|p| p.game_mode != vanillite_config::GameMode::Spectator)
}

/// `registerGoals` of the panda.
pub fn register_goals(m: &mut MobData) {
    let g = &mut m.goals;
    g.add(0, G::Float);
    g.add(2, fam(FamGoal::Panda(Goal::Panic { pos: Vec3::ZERO })));
    g.add(
        2,
        fam(FamGoal::Panda(Goal::BreedCheck {
            unhappy_cooldown: 0,
        })),
    );
    g.add(2, G::breed(1.0));
    g.add(
        3,
        G::melee(super::MeleeKind::Plain, f64::from(1.2_f32), true),
    );
    g.add(4, G::tempt(1.0, TemptItems::Tag("minecraft:panda_food")));
    let players: TargetFilter = any_player;
    let monsters: TargetFilter = crate::mob::common::enemy;
    g.add(
        6,
        fam(FamGoal::Panda(Goal::Avoid(CommonGoal::avoid(
            players, 8.0, 2.0, 2.0,
        )))),
    );
    g.add(
        6,
        fam(FamGoal::Panda(Goal::Avoid(CommonGoal::avoid(
            monsters, 4.0, 2.0, 2.0,
        )))),
    );
    g.add(7, fam(FamGoal::Panda(Goal::Sit { cooldown: 0 })));
    g.add(8, fam(FamGoal::Panda(Goal::LieOnBack { cooldown: 0 })));
    g.add(8, fam(FamGoal::Panda(Goal::Sneeze)));
    g.add(9, G::look_at_player(6.0));
    g.add(10, G::random_look());
    g.add(12, fam(FamGoal::Panda(Goal::Roll)));
    g.add(
        13,
        G::FollowParent {
            speed: 1.25,
            parent: None,
            recalc: 0,
        },
    );
    g.add(14, G::stroll(1.0));
    m.targets.add(1, G::hurt_by(true));
    m.can_pick_up_loot = true;
    m.nav.can_float = true;
}

/// `setAttributes`: weak pandas have 10 health, lazy ones are slow.
fn set_attributes(m: &mut MobData) {
    let g = kind_gene(m);
    if g == gene::WEAK {
        m.attrs.max_health = 10.0;
        m.health = m.health.min(10.0);
    }
    if g == gene::LAZY {
        m.attrs.movement_speed = f64::from(0.07_f32);
    }
}

/// `finalizeSpawn`: random genes.
pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    let main = random_gene(&mut m.random);
    let hidden = random_gene(&mut m.random);
    let s = st_mut(m);
    s.main_gene = main;
    s.hidden_gene = hidden;
    set_attributes(m);
    super::ageable_group(m, ctx, true, 0.2);
}

/// `getAmbientSound`.
pub fn ambient_sound(m: &MobData) -> Option<&'static str> {
    Some(match kind_gene(m) {
        gene::AGGRESSIVE => "minecraft:entity.panda.aggressive_ambient",
        gene::WORRIED => "minecraft:entity.panda.worried_ambient",
        _ => "minecraft:entity.panda.ambient",
    })
}

/// `PandaBreedGoal.canUse` after `super.canUse()`: not unhappy and bamboo near.
pub(crate) fn breed_allowed(t: &Tick<'_, '_>) -> bool {
    st(t.mob_ref()).unhappy == 0 && bamboo_nearby(t)
}

/// `Panda.tick` after `super.tick()`: worry in thunder, unhappiness, sneezing, rolling and
/// eating.
pub fn after_tick(t: &mut Tick<'_, '_>) {
    if kind_gene(t.mob_ref()) == gene::WORRIED {
        if t.cx.env.thundering && !t.mob_ref().fluids.in_water() {
            set(t, SITTING, true);
            st_mut(t.mob()).eat = 0;
        } else if !eating(t.mob_ref()) {
            set(t, SITTING, false);
        }
    }
    let unhappy = st(t.mob_ref()).unhappy;
    if unhappy > 0 {
        if unhappy == 29 || unhappy == 14 {
            crate::mob::kinds::play_sound(t, "minecraft:entity.panda.cant_breed", 1.0, 1.0);
        }
        st_mut(t.mob()).unhappy -= 1;
        t.core_mut().mark_data_dirty();
    }
    if has(t.mob_ref(), SNEEZING) {
        st_mut(t.mob()).sneeze += 1;
        let c = st(t.mob_ref()).sneeze;
        if c > 20 {
            set(t, SNEEZING, false);
            st_mut(t.mob()).sneeze = 0;
            after_sneeze(t);
        } else if c == 1 {
            crate::mob::kinds::play_sound(t, "minecraft:entity.panda.pre_sneeze", 1.0, 1.0);
        }
        t.core_mut().mark_data_dirty();
    }
    if has(t.mob_ref(), ROLLING) {
        handle_roll(t);
    } else {
        st_mut(t.mob()).roll_counter = 0;
    }
    handle_eating(t);
}

/// `afterSneeze`: the sneeze sound, nearby adults jump, maybe a slime ball.
fn after_sneeze(t: &mut Tick<'_, '_>) {
    crate::mob::kinds::play_sound(t, "minecraft:entity.panda.sneeze", 1.0, 1.0);
    let area = t.aabb().inflate(10.0, 10.0, 10.0);
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&area, false, |id| ids.push(id));
    for &id in &ids {
        let ok = t.ents.mob(id).is_some_and(|m| {
            m.kind == Kind::Panda && !m.baby && !m.fluids.in_water() && st(m).panda_flags == 0
        });
        let on_ground = t.ents.store.get(id).is_some_and(|e| {
            e.flags()
                .contains(vanillite_world::entity::EntityFlags::ON_GROUND)
        });
        if ok
            && on_ground
            && let Some(jump) = t.ents.mob(id).map(|m| m.attrs.jump_strength)
            && let Some(mut e) = t.ents.store.get_mut(id)
        {
            let v = e.as_ref().velocity();
            e.set_velocity(Vec3::new(v.x, jump.max(v.y), v.z));
        }
    }
    t.cx.ids = ids;
    // `gameplay/panda_sneeze`: a slime ball one in 700.
    if t.env.rules.bool(Rule::MOB_DROPS) && t.mob().random.next_int_bounded(700) == 0 {
        crate::mob::equipment::spawn_at_location(t, equipment::stack("minecraft:slime_ball"), 0.0);
    }
}

/// `handleRoll`.
fn handle_roll(t: &mut Tick<'_, '_>) {
    let s = st_mut(t.mob());
    s.roll_counter += 1;
    let c = s.roll_counter;
    if c > 32 {
        set(t, ROLLING, false);
        return;
    }
    let v = t.velocity();
    if c == 1 {
        let angle = f64::from(t.core().rot().y_rot).to_radians();
        let mult = if t.mob_ref().baby { 0.1 } else { 0.2 };
        let delta = Vec3::new(v.x - angle.sin() * mult, 0.0, v.z + angle.cos() * mult);
        st_mut(t.mob()).roll_delta = (delta.x, delta.z);
        t.set_velocity(Vec3::new(delta.x, 0.27, delta.z));
    } else if c != 7 && c != 15 && c != 23 {
        let (dx, dz) = st(t.mob_ref()).roll_delta;
        t.set_velocity(Vec3::new(dx, v.y, dz));
    } else {
        let y = if t.on_ground() { 0.27 } else { v.y };
        t.set_velocity(Vec3::new(0.0, y, 0.0));
    }
}

/// `handleEating`.
fn handle_eating(t: &mut Tick<'_, '_>) {
    let holding = !t
        .mob_ref()
        .equipment
        .get(EquipmentSlot::MainHand)
        .is_empty();
    let sitting = has(t.mob_ref(), SITTING);
    if !eating(t.mob_ref())
        && sitting
        && !scared(t)
        && holding
        && t.mob().random.next_int_bounded(80) == 1
    {
        st_mut(t.mob()).eat = 1;
        t.core_mut().mark_data_dirty();
    } else if (!holding || !sitting) && st(t.mob_ref()).eat != 0 {
        st_mut(t.mob()).eat = 0;
        t.core_mut().mark_data_dirty();
    }
    if !eating(t.mob_ref()) {
        return;
    }
    let counter = st(t.mob_ref()).eat;
    if counter % 5 == 0 {
        let (v, p) = {
            let r = &mut t.mob().random;
            #[allow(clippy::cast_precision_loss)]
            let v = 0.5 + 0.5 * r.next_int_bounded(2) as f32;
            (v, (r.next_float() - r.next_float()) * 0.2 + 1.0)
        };
        crate::mob::kinds::play_sound(t, "minecraft:entity.panda.eat", v, p);
    }
    if counter > 80 && t.mob().random.next_int_bounded(20) == 1 {
        let held = t.mob_ref().equipment.get(EquipmentSlot::MainHand).clone();
        if counter > 100 && in_tag(&held, "minecraft:panda_eats_from_ground") {
            t.mob()
                .equipment
                .set(EquipmentSlot::MainHand, ItemStack::default());
            crate::mob::equipment::broadcast(t);
            let at = t.pos();
            let eat = crate::level::game_event::events().eat;
            crate::mob::living::post_game_event(t, eat, at, None);
            set(t, SITTING, false);
        }
        st_mut(t.mob()).eat = 0;
        t.core_mut().mark_data_dirty();
        return;
    }
    st_mut(t.mob()).eat += 1;
}

/// Pandas need no work in `aiStep` beyond their tick.
pub fn ai_step(_t: &mut Tick<'_, '_>) {}

/// `Panda.doHurtTarget`: a peaceful panda bites once and forgets.
pub fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
    if kind_gene(t.mob_ref()) != gene::AGGRESSIVE {
        // `didBite`: the hurt-by goal ends with the target.
        let m = t.mob();
        let running = m
            .targets
            .slots
            .iter()
            .position(|s| s.running && matches!(s.goal, G::HurtBy { .. }));
        if let Some(i) = running {
            m.targets.slots[i].running = false;
        }
    }
    let r = crate::mob::combat::base_mob_attack(t, target);
    crate::mob::kinds::play_sound(t, "minecraft:entity.panda.bite", 1.0, 1.0);
    if kind_gene(t.mob_ref()) != gene::AGGRESSIVE {
        t.mob().target = None;
    }
    Some(r)
}

/// `hurtServer`: a hurt panda stands up.
pub fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
    if let Some(m) = h.ents.mob_mut(h.target) {
        st_mut(m).panda_flags &= !SITTING;
    }
    Some(amount)
}

/// `PandaBreedGoal` partners: the shared breeding rules.
pub fn can_mate(me: &MobData, partner: &MobData) -> bool {
    partner.kind == Kind::Panda && me.in_love > 0 && partner.in_love > 0
}

/// `setGeneFromParents`.
pub fn breed_offspring(
    parent: &MobData,
    partner: &MobData,
    child: &mut MobData,
    r: &mut LegacyRandom,
) {
    let one = |m: &MobData, r: &mut LegacyRandom| {
        if r.next_boolean() {
            st(m).main_gene
        } else {
            st(m).hidden_gene
        }
    };
    let (main, hidden) = if r.next_boolean() {
        let a = one(parent, r);
        (a, one(partner, r))
    } else {
        let a = one(partner, r);
        (a, one(parent, r))
    };
    let mut main = main;
    let mut hidden = hidden;
    if r.next_int_bounded(32) == 0 {
        main = random_gene(r);
    }
    if r.next_int_bounded(32) == 0 {
        hidden = random_gene(r);
    }
    let s = st_mut(child);
    s.main_gene = main;
    s.hidden_gene = hidden;
    set_attributes(child);
}

/// `Panda.mobInteract`.
pub fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
    let Some(m) = ix.ents.mob(ix.target) else {
        return Interaction::Pass;
    };
    let worried = kind_gene(m) == gene::WORRIED;
    if worried && ix.cx.env.thundering {
        return Interaction::Pass;
    }
    if st(m).panda_flags & ON_BACK != 0 {
        if let Some(m) = ix.ents.mob_mut(ix.target) {
            st_mut(m).panda_flags &= !ON_BACK;
        }
        dirty(ix);
        return Interaction::Success;
    }
    if !crate::mob::fam::is_food(Kind::Panda, ix.stack.item) {
        let dandelion = super::item_name(ix.stack) == "minecraft:golden_dandelion";
        return if m.baby && dandelion {
            super::animal_interact(ix)
        } else {
            Interaction::Pass
        };
    }
    let (baby, age, in_love, sitting) =
        (m.baby, m.age, m.in_love, st(m).panda_flags & SITTING != 0);
    if baby && !m.age_locked {
        consume(ix);
        if let Some(m) = ix.ents.mob_mut(ix.target) {
            // `ageUp((int)(-getAge() / 20 * 0.1F), true)`.
            let seconds = crate::mob::animal::speed_up_seconds_when_feeding(-age);
            crate::mob::animal::age_up(m, seconds, true);
        }
        return Interaction::Success;
    }
    if age == 0 && in_love <= 0 {
        consume(ix);
        let uuid = super::player_uuid(ix);
        if let Some(m) = ix.ents.mob_mut(ix.target) {
            m.in_love = 600;
            m.love_cause = uuid;
        }
        super::event(ix, 18);
        return Interaction::Success;
    }
    let in_water = m.fluids.in_water();
    if sitting || in_water {
        return Interaction::Pass;
    }
    let infinite = crate::mob::animal::infinite(ix.ents, ix.player);
    let one = ix.stack.with_count(1);
    let current = ix.ents.mob_mut(ix.target).map(|m| {
        m.zza = 0.0;
        m.nav.stop();
        let s = st_mut(m);
        s.panda_flags |= SITTING;
        s.eat = 1;
        std::mem::replace(&mut m.equipment.items[0], one)
    });
    if let Some(current) = current
        && !current.is_empty()
        && !infinite
        && let Some(pos) = ix.ents.store.get(ix.target).map(|e| e.pos())
    {
        super::horse::drop_item(ix.env, pos, current);
    }
    consume(ix);
    super::horse::send_equipment(ix);
    dirty(ix);
    Interaction::Success
}

/// The panda's synched data.
pub fn encode(w: &mut DataWriter<'_>, m: &MobData) {
    let s = st(m);
    w.int(18, s.unhappy, 0);
    w.int(19, s.sneeze, 0);
    w.int(20, s.eat, 0);
    w.byte(21, s.main_gene, 0);
    w.byte(22, s.hidden_gene, 0);
    w.byte(23, s.panda_flags, 0);
}

/// `Panda.addAdditionalSaveData`.
pub fn save(m: &MobData, out: &mut Compound) {
    let s = st(m);
    out.insert("MainGene", gene::NAMES[usize::from(s.main_gene.min(6))]);
    out.insert("HiddenGene", gene::NAMES[usize::from(s.hidden_gene.min(6))]);
}

/// `Panda.readAdditionalSaveData`.
pub fn load(m: &mut MobData, tag: &Compound) {
    let read = |k: &str| {
        super::string(tag, k)
            .and_then(|n| gene::NAMES.iter().position(|g| *g == n))
            .and_then(|i| u8::try_from(i).ok())
            .unwrap_or(gene::NORMAL)
    };
    let (main, hidden) = (read("MainGene"), read("HiddenGene"));
    let s = st_mut(m);
    s.main_gene = main;
    s.hidden_gene = hidden;
}

/// `canPickUpAndEat`: food from the ground (`#panda_eats_from_ground`).
fn eats_from_ground(_m: &MobData, s: &ItemStack) -> bool {
    in_tag(s, "minecraft:panda_eats_from_ground")
}

/// The panda's pickup rules.
pub fn pickup_rules() -> PickupRules {
    PickupRules {
        wants: eats_from_ground,
        preferred_weapon: None,
    }
}

/// `Panda.pickUpItem`: the whole stack into the empty hand.
pub fn pick_up(t: &mut Tick<'_, '_>, item: EntityId) -> bool {
    if !t
        .mob_ref()
        .equipment
        .get(EquipmentSlot::MainHand)
        .is_empty()
    {
        return false;
    }
    let Some(stack) = t.ents.item(item).map(|d| d.stack.clone()) else {
        return false;
    };
    let count = stack.count;
    let m = t.mob();
    m.equipment.set(EquipmentSlot::MainHand, stack);
    m.equipment.set_guaranteed_drop(EquipmentSlot::MainHand);
    crate::mob::equipment::take(t, item, count);
    t.cx.discarded.push(item);
    crate::mob::equipment::broadcast(t);
    true
}

// --- Goals --------------------------------------------------------------------------------

/// `PandaBreedGoal.canFindBamboo`.
fn bamboo_nearby(t: &Tick<'_, '_>) -> bool {
    let p = t.block_pos();
    for y in 0..3 {
        for r in 0..8 {
            let mut x = 0;
            while x <= r {
                let mut z = if x < r && x > -r { r } else { 0 };
                while z <= r {
                    let pos = BlockPos::new(p.x + x, p.y + y, p.z + z);
                    if t.level.block(pos).block().name() == "minecraft:bamboo" {
                        return true;
                    }
                    z = if z > 0 { -z } else { 1 - z };
                }
                x = if x > 0 { -x } else { 1 - x };
            }
        }
    }
    false
}

fn ground_food_nearby(t: &Tick<'_, '_>, r: f64) -> Option<Vec3> {
    let area = t.aabb().inflate(r, r, r);
    let ents = &*t.ents;
    let mut first = None;
    ents.store.for_each_in(&area, false, |id| {
        if first.is_some() {
            return;
        }
        if let Some(d) = ents.item(id)
            && d.pickup_delay <= 0
            && in_tag(&d.stack, "minecraft:panda_eats_from_ground")
        {
            first = ents.store.get(id).map(|e| e.pos());
        }
    });
    first
}

fn try_to_sit(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().fluids.in_water() {
        t.mob().zza = 0.0;
        t.nav().stop();
        set(t, SITTING, true);
    }
}

pub fn can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::Panic { pos } => {
            let env = t
                .mob_ref()
                .last_damage_source(t.game_time())
                .is_some_and(|k| {
                    t.env
                        .types
                        .is(k, crate::survival::damage::tag::PANIC_ENVIRONMENTAL_CAUSES)
                });
            if !env {
                return false;
            }
            match crate::mob::animal::panic_pos(t) {
                Some(p) => {
                    *pos = p;
                    true
                }
                None => false,
            }
        }
        Goal::BreedCheck { unhappy_cooldown } => {
            // Holds the breed goal back while no bamboo is near (it never runs itself).
            // `super.canUse()`: in love with a free partner near.
            if t.mob_ref().in_love <= 0 || crate::mob::animal::free_partner(t).is_none() {
                return false;
            }
            let m = t.mob_ref();
            if st(m).unhappy != 0 || bamboo_nearby(t) {
                return false;
            }
            if *unhappy_cooldown <= m.tick_count {
                st_mut(t.mob()).unhappy = 32;
                *unhappy_cooldown = t.mob_ref().tick_count + 600;
                t.core_mut().mark_data_dirty();
            }
            false
        }
        Goal::Avoid(cg) => {
            kind_gene(t.mob_ref()) == gene::WORRIED
                && can_perform(t)
                && crate::mob::common::can_use(t, cg)
        }
        Goal::Sit { cooldown } => {
            let m = t.mob_ref();
            if *cooldown > m.tick_count
                || m.baby
                || m.fluids.in_water()
                || !can_perform(t)
                || st(m).unhappy > 0
            {
                return false;
            }
            !m.equipment.get(EquipmentSlot::MainHand).is_empty()
                || ground_food_nearby(t, 6.0).is_some()
        }
        Goal::LieOnBack { cooldown } => {
            *cooldown < t.mob_ref().tick_count
                && kind_gene(t.mob_ref()) == gene::LAZY
                && can_perform(t)
                && t.mob().random.next_int_bounded(reduced(400)) == 1
        }
        Goal::Sneeze => {
            if !(t.mob_ref().baby && can_perform(t)) {
                return false;
            }
            (kind_gene(t.mob_ref()) == gene::WEAK
                && t.mob().random.next_int_bounded(reduced(500)) == 1)
                || t.mob().random.next_int_bounded(reduced(6000)) == 1
        }
        Goal::Roll => {
            let m = t.mob_ref();
            if !((m.baby || kind_gene(m) == gene::PLAYFUL) && t.on_ground()) || !can_perform(t) {
                return false;
            }
            let angle = f64::from(t.core().rot().y_rot).to_radians();
            let (xd, zd) = (-angle.sin(), angle.cos());
            let step = |d: f64| if d.abs() > 0.5 { d.signum() as i32 } else { 0 };
            let p = t.block_pos();
            let below = BlockPos::new(p.x + step(xd), p.y - 1, p.z + step(zd));
            if t.level.block(below).is_air() {
                return true;
            }
            (kind_gene(t.mob_ref()) == gene::PLAYFUL
                && t.mob().random.next_int_bounded(reduced(60)) == 1)
                || t.mob().random.next_int_bounded(reduced(500)) == 1
        }
    }
}

pub fn can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::Panic { .. } => {
            if has(t.mob_ref(), SITTING) {
                t.nav().stop();
                return false;
            }
            !t.nav_ref().is_done()
        }
        Goal::Avoid(cg) => crate::mob::common::can_continue(t, cg),
        Goal::Sit { .. } | Goal::LieOnBack { .. } => {
            let lazy = kind_gene(t.mob_ref()) == gene::LAZY;
            if t.mob_ref().fluids.in_water()
                || !(lazy || t.mob().random.next_int_bounded(reduced(600)) != 1)
            {
                return false;
            }
            t.mob().random.next_int_bounded(reduced(2000)) != 1
        }
        _ => false,
    }
}

pub fn start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::Panic { pos } => {
            nav::move_to_pos(t, *pos, 2.0);
        }
        Goal::Avoid(cg) => crate::mob::common::start(t, cg),
        Goal::Sit { cooldown } => {
            if t.mob_ref()
                .equipment
                .get(EquipmentSlot::MainHand)
                .is_empty()
            {
                if let Some(p) = ground_food_nearby(t, 8.0) {
                    nav::move_to_pos(t, p, f64::from(1.2_f32));
                }
            } else {
                try_to_sit(t);
            }
            *cooldown = 0;
        }
        Goal::LieOnBack { cooldown } => {
            set(t, ON_BACK, true);
            *cooldown = 0;
        }
        Goal::Sneeze => set(t, SNEEZING, true),
        Goal::Roll => set(t, ROLLING, true),
        Goal::BreedCheck { .. } => {}
    }
}

pub fn stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::Avoid(cg) => crate::mob::common::stop(t, cg),
        Goal::Sit { cooldown } => {
            let held = t.mob_ref().equipment.get(EquipmentSlot::MainHand).clone();
            if !held.is_empty() {
                crate::mob::equipment::spawn_at_location(t, held, 0.0);
                t.mob()
                    .equipment
                    .set(EquipmentSlot::MainHand, ItemStack::default());
                crate::mob::equipment::broadcast(t);
                let lazy = kind_gene(t.mob_ref()) == gene::LAZY;
                let r = &mut t.mob().random;
                let wait = if lazy {
                    r.next_int_bounded(50) + 10
                } else {
                    r.next_int_bounded(150) + 10
                };
                *cooldown = t.mob_ref().tick_count + wait * 20;
            }
            set(t, SITTING, false);
        }
        Goal::LieOnBack { cooldown } => {
            set(t, ON_BACK, false);
            *cooldown = t.mob_ref().tick_count + 200;
        }
        Goal::Panic { .. } => t.nav().stop(),
        _ => {}
    }
}

pub fn tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::Avoid(cg) => crate::mob::common::tick(t, cg),
        Goal::Sit { .. }
            if !has(t.mob_ref(), SITTING)
                && !t
                    .mob_ref()
                    .equipment
                    .get(EquipmentSlot::MainHand)
                    .is_empty() =>
        {
            try_to_sit(t);
        }
        _ => {}
    }
}
