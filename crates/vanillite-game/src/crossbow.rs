//! Crossbows (`CrossbowItem`, `ProjectileWeaponItem`) of players and mobs: while a crossbow is
//! used it plays its loading sounds and, once fully drawn, loads projectiles into its
//! `charged_projectiles` component (Multishot draws extra free copies, Quick Charge shortens
//! the charge and changes the sounds); a loaded crossbow fires all its projectiles at once,
//! spread by Multishot's angle, as arrows that pierce (Piercing) and hit with the crossbow
//! sound, or as firework rockets shot at an angle ([`crate::firework::crossbow_rocket`]).
//!
//! Spectral and tipped arrows fly as plain arrows (as from bows).

use vanillite_base::Vec3;
use vanillite_base::random::LegacyRandom;
use vanillite_protocol::codec::{Reader, write_varint};
use vanillite_protocol::components::ids;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{EntityId, NetworkIds};

use crate::enchant::FloatRandom;
use crate::entities::Entities;
use crate::item::{self, ItemStack};
use crate::survival::Effect;

pub const CROSSBOW: &str = "minecraft:crossbow";
const FIREWORK_ROCKET: &str = "minecraft:firework_rocket";
/// `MAX_CHARGE_DURATION` in seconds.
const MAX_CHARGE_DURATION: f32 = 1.25;
/// `ARROW_POWER`.
pub const ARROW_POWER: f32 = 3.15;
/// `FIREWORK_POWER`.
pub const FIREWORK_POWER: f32 = 1.6;
/// `MOB_ARROW_POWER` (`performCrossbowAttack(body, 1.6F)`).
pub const MOB_ARROW_POWER: f32 = 1.6;

/// Whether the stack is a crossbow.
pub fn is_crossbow(stack: &ItemStack) -> bool {
    !stack.is_empty() && item::item_id(stack).name() == CROSSBOW
}

fn is_firework(stack: &ItemStack) -> bool {
    item::item_id(stack).name() == FIREWORK_ROCKET
}

/// `ARROW_ONLY`: the `#minecraft:arrows` items.
pub fn is_arrow(stack: &ItemStack) -> bool {
    !stack.is_empty()
        && crate::player::play::game_data().map_or_else(
            || item::item_id(stack).name() == "minecraft:arrow",
            |d| d.item_in_tag(stack.item, "minecraft:arrows"),
        )
}

/// `ARROW_OR_FIREWORK`: what a crossbow takes from the hands.
pub fn is_held_projectile(stack: &ItemStack) -> bool {
    is_arrow(stack) || (!stack.is_empty() && is_firework(stack))
}

/// The loaded projectiles (`charged_projectiles`).
pub fn charged(stack: &ItemStack) -> Vec<ItemStack> {
    let Some(value) = stack.component(ids::CHARGED_PROJECTILES) else {
        return Vec::new();
    };
    let mut r = Reader::new(value);
    let Ok(n) = r.varint() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for _ in 0..n.clamp(0, 1024) {
        match ItemStack::decode_template(&mut r) {
            Ok(s) => out.push(s),
            Err(_) => break,
        }
    }
    out
}

/// `CrossbowItem.isCharged`.
pub fn is_charged(stack: &ItemStack) -> bool {
    stack
        .component(ids::CHARGED_PROJECTILES)
        .is_some_and(|v| v.first().is_some_and(|&n| n != 0))
}

/// Sets `charged_projectiles`; no projectiles is the crossbow's default (empty) value.
pub fn set_charged(stack: &mut ItemStack, projectiles: &[ItemStack]) {
    if projectiles.is_empty() {
        if let Some(p) = stack.patch.as_deref_mut() {
            p.unset(ids::CHARGED_PROJECTILES);
            if p.is_empty() {
                stack.patch = None;
            }
        }
        return;
    }
    let mut out = Vec::new();
    write_varint(&mut out, i32::try_from(projectiles.len()).unwrap_or(0));
    for s in projectiles {
        s.encode_template(&mut out);
    }
    stack.set_component(ids::CHARGED_PROJECTILES, out.into_boxed_slice());
}

/// `CrossbowItem.getChargeDuration`: 25 ticks, shortened by Quick Charge.
pub fn charge_duration(crossbow: &ItemStack, random: &mut dyn FloatRandom) -> i32 {
    let secs = match crate::player::play::game_data() {
        Some(d) => d
            .enchants
            .crossbow_charge_time(crossbow, random, MAX_CHARGE_DURATION),
        None => MAX_CHARGE_DURATION,
    };
    vanillite_base::math::floor(f64::from(secs * 20.0))
}

/// `CrossbowItem.ChargingSounds`.
#[derive(Clone, Debug)]
pub struct Sounds {
    pub start: Option<String>,
    pub mid: Option<String>,
    pub end: Option<String>,
}

/// `getChargingSounds`: the sounds of the highest enchantment that has some, else the default.
pub fn sounds(crossbow: &ItemStack) -> Sounds {
    if let Some(d) = crate::player::play::game_data()
        && let Some(s) = d.enchants.crossbow_charging_sounds(crossbow)
    {
        return Sounds {
            start: s.start.clone(),
            mid: s.mid.clone(),
            end: s.end.clone(),
        };
    }
    Sounds {
        start: Some("minecraft:item.crossbow.loading_start".to_owned()),
        mid: Some("minecraft:item.crossbow.loading_middle".to_owned()),
        end: Some("minecraft:item.crossbow.loading_end".to_owned()),
    }
}

/// `startSoundPlayed` and `midLoadSoundPlayed` (fields of the shared `CrossbowItem` in
/// Vanilla; kept per user here).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UseFlags {
    pub start: bool,
    pub mid: bool,
}

/// What one `CrossbowItem.onUseTick` does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UseTick {
    pub start_sound: bool,
    pub mid_sound: bool,
    /// Fully drawn: load if not charged yet.
    pub full: bool,
}

/// The stages of `onUseTick` after `used` ticks of a charge of `duration` ticks.
pub fn use_tick(flags: &mut UseFlags, used: i32, duration: i32) -> UseTick {
    #[allow(clippy::cast_precision_loss)]
    let percent = used as f32 / duration as f32;
    if percent < 0.2 {
        flags.start = false;
        flags.mid = false;
    }
    let mut r = UseTick::default();
    if percent >= 0.2 && !flags.start {
        flags.start = true;
        r.start_sound = true;
    }
    if percent >= 0.5 && !flags.mid {
        flags.mid = true;
        r.mid_sound = true;
    }
    r.full = percent >= 1.0;
    r
}

/// The intangible marker (`intangible_projectile`, a unit as an empty compound tag).
fn set_intangible(stack: &mut ItemStack) {
    stack.set_component(ids::INTANGIBLE_PROJECTILE, vec![10, 0].into_boxed_slice());
}

/// Whether the stack was drawn for free (`intangible_projectile`).
pub fn is_intangible(stack: &ItemStack) -> bool {
    stack.component(ids::INTANGIBLE_PROJECTILE).is_some()
}

/// `ProjectileWeaponItem.useAmmo`: what one projectile takes from `projectile`.
fn use_ammo(
    weapon: &ItemStack,
    projectile: &mut ItemStack,
    infinite: bool,
    force_infinite: bool,
    random: &mut dyn FloatRandom,
) -> ItemStack {
    let ammo = if force_infinite || infinite {
        0
    } else {
        crate::player::play::game_data()
            .map_or(1, |d| d.enchants.ammo_use(weapon, projectile, random, 1))
    };
    if ammo > projectile.count {
        ItemStack::EMPTY
    } else if ammo == 0 {
        let mut copy = projectile.with_count(1);
        set_intangible(&mut copy);
        copy
    } else {
        projectile.split(ammo)
    }
}

/// `ProjectileWeaponItem.draw`: the projectiles one load takes from `projectile`; the first
/// costs ammo (unless the holder has infinite materials), Multishot's extra ones are free.
pub fn draw(
    weapon: &ItemStack,
    projectile: &mut ItemStack,
    infinite: bool,
    random: &mut dyn FloatRandom,
) -> Vec<ItemStack> {
    if projectile.is_empty() {
        return Vec::new();
    }
    let n = crate::player::play::game_data()
        .map_or(1, |d| d.enchants.projectile_count(weapon, random, 1));
    let mut copy = projectile.clone();
    let mut drawn = Vec::with_capacity(usize::try_from(n).unwrap_or(1));
    for i in 0..n {
        let s = if i == 0 {
            use_ammo(weapon, projectile, infinite, false, random)
        } else {
            use_ammo(weapon, &mut copy, infinite, true, random)
        };
        if !s.is_empty() {
            drawn.push(s);
        }
    }
    drawn
}

/// `Monster.getProjectile(crossbow)`: an arrow or rocket in a hand (off hand first), else a
/// new arrow.
pub fn mob_projectile(main: &ItemStack, off: &ItemStack) -> ItemStack {
    if is_held_projectile(off) {
        off.clone()
    } else if is_held_projectile(main) {
        main.clone()
    } else {
        vanillite_data::registry::ITEM
            .id("minecraft:arrow")
            .map_or(ItemStack::EMPTY, |a| ItemStack::new(a, 1))
    }
}

/// `getShotPitch(random, index)`.
pub fn shot_pitch(random: &mut LegacyRandom, index: usize) -> f32 {
    if index == 0 {
        return 1.0;
    }
    let range = if index & 1 == 1 { 0.63 } else { 0.43 };
    1.0 / (vanillite_base::random::RandomSource::next_float(random) * 0.5 + 1.8) + range
}

// --- JOML float vector math (`Vector3f`, `Quaternionf`) ---------------------------------------

type V3 = [f32; 3];

fn jsin(a: f32) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    let s = f64::from(a).sin() as f32;
    s
}

/// `Math.cosFromSin(sin, angle)`.
fn cos_from_sin(sin: f32, angle: f32) -> f32 {
    const PI: f32 = std::f32::consts::PI;
    const PI2: f32 = PI * 2.0;
    const PI_HALF: f32 = PI * 0.5;
    #[allow(clippy::cast_possible_truncation)]
    let cos = f64::from(1.0 - sin * sin).sqrt() as f32;
    let a = angle + PI_HALF;
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let mut b = a - ((a / PI2) as i32) as f32 * PI2;
    if b < 0.0 {
        b += PI2;
    }
    if b >= PI { -cos } else { cos }
}

fn normalize(v: V3) -> V3 {
    let len_sqr = v[0] * v[0] + (v[1] * v[1] + v[2] * v[2]);
    #[allow(clippy::cast_possible_truncation)]
    let scalar = 1.0 / f64::from(len_sqr).sqrt() as f32;
    [v[0] * scalar, v[1] * scalar, v[2] * scalar]
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] + -(a[2] * b[1]),
        a[2] * b[0] + -(a[0] * b[2]),
        a[0] * b[1] + -(a[1] * b[0]),
    ]
}

/// `Vector3f.rotateAxis(angle, x, y, z)`.
fn rotate_axis(v: V3, angle: f32, a: V3) -> V3 {
    let one = |x: f32| (x.abs() - 1.0).abs() < f32::EPSILON;
    let [x, y, z] = v;
    if a[1] == 0.0 && a[2] == 0.0 && one(a[0]) {
        let ang = a[0] * angle;
        let (s, c) = (jsin(ang), cos_from_sin(jsin(ang), ang));
        return [x, y * c - z * s, y * s + z * c];
    }
    if a[0] == 0.0 && a[2] == 0.0 && one(a[1]) {
        let ang = a[1] * angle;
        let (s, c) = (jsin(ang), cos_from_sin(jsin(ang), ang));
        return [x * c + z * s, y, -x * s + z * c];
    }
    if a[0] == 0.0 && a[1] == 0.0 && one(a[2]) {
        let ang = a[2] * angle;
        let (s, c) = (jsin(ang), cos_from_sin(jsin(ang), ang));
        return [x * c - y * s, x * s + y * c, z];
    }
    let h = angle * 0.5;
    let s = jsin(h);
    let (qx, qy, qz) = (a[0] * s, a[1] * s, a[2] * s);
    let qw = cos_from_sin(s, h);
    let (w2, x2, y2, z2, zw) = (qw * qw, qx * qx, qy * qy, qz * qz, qz * qw);
    let (xy, xz, yw, yz, xw) = (qx * qy, qx * qz, qy * qw, qy * qz, qx * qw);
    [
        (w2 + x2 - z2 - y2) * x + (-zw + xy - zw + xy) * y + (yw + xz + xz + yw) * z,
        (xy + zw + zw + xy) * x + (y2 - z2 + w2 - x2) * y + (yz + yz - xw - xw) * z,
        (xz - yw + xz - yw) * x + (yz + yz + xw + xw) * y + (z2 - y2 - x2 + w2) * z,
    ]
}

#[allow(clippy::cast_possible_truncation)]
fn to_v3(v: Vec3) -> V3 {
    [v.x as f32, v.y as f32, v.z as f32]
}

fn to_vec3(v: V3) -> Vec3 {
    Vec3::new(f64::from(v[0]), f64::from(v[1]), f64::from(v[2]))
}

/// `Entity.getUpVector`: the view vector 90° above.
fn up_vector(x_rot: f32, y_rot: f32) -> Vec3 {
    crate::mob::view_vector(x_rot - 90.0, y_rot)
}

/// `CrossbowItem.getProjectileShotVector(body, originalVector, angle)`.
fn shot_vector_at(x_rot: f32, y_rot: f32, original: Vec3, angle: f32) -> V3 {
    let view = normalize(to_v3(original));
    let mut right = cross(view, [0.0, 1.0, 0.0]);
    if f64::from(right[0] * right[0] + right[1] * right[1] + right[2] * right[2]) <= 1.0e-7 {
        right = cross(view, to_v3(up_vector(x_rot, y_rot)));
    }
    #[allow(clippy::cast_possible_truncation)]
    let half_pi = (std::f64::consts::PI / 2.0) as f32;
    let up = rotate_axis(view, half_pi, right);
    rotate_axis(view, angle * crate::mob::DEG_TO_RAD, up)
}

/// The view vector turned by `angle` degrees around the up vector (`Quaternionf
/// .setAngleAxis` and `Vector3f.rotate`).
fn shot_vector_view(x_rot: f32, y_rot: f32, angle: f32) -> V3 {
    let up = up_vector(x_rot, y_rot);
    let half = f64::from(angle * crate::mob::DEG_TO_RAD) * 0.5;
    let s = half.sin();
    #[allow(clippy::cast_possible_truncation)]
    let (qx, qy, qz) = ((up.x * s) as f32, (up.y * s) as f32, (up.z * s) as f32);
    #[allow(clippy::cast_possible_truncation)]
    let qw = half.cos() as f32;
    let [x, y, z] = to_v3(crate::mob::view_vector(x_rot, y_rot));
    let (xx, yy, zz, ww) = (qx * qx, qy * qy, qz * qz, qw * qw);
    let (xy, xz, yz, xw) = (qx * qy, qx * qz, qy * qz, qx * qw);
    let (zw, yw) = (qz * qw, qy * qw);
    let k = 1.0 / (xx + yy + zz + ww);
    [
        (xx - yy - zz + ww) * k * x + (2.0 * (xy - zw) * k * y + (2.0 * (xz + yw) * k) * z),
        2.0 * (xy + zw) * k * x + ((yy - xx - zz + ww) * k * y + (2.0 * (yz - xw) * k) * z),
        2.0 * (xz - yw) * k * x + (2.0 * (yz + xw) * k * y + ((zz - xx - yy + ww) * k) * z),
    ]
}

/// Who shoots (`LivingEntity`).
#[derive(Clone, Copy, Debug)]
pub struct Shooter {
    pub id: EntityId,
    pub pos: Vec3,
    pub eye_y: f64,
    pub x_rot: f32,
    pub y_rot: f32,
    /// Players shoot critical arrows anyone may pick up; mobs' arrows cannot be picked up.
    pub player: bool,
    /// The shooter's sound source (`getSoundSource`).
    pub source: SoundSource,
}

/// The target of a mob's shot (`targetOverride`): position and `getBbHeight`.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub pos: Vec3,
    pub height: f32,
}

/// `ProjectileWeaponItem.shoot` with `CrossbowItem.createProjectile` and `shootProjectile`:
/// every loaded projectile flies off (Multishot spreads them by its angle), each with its
/// shot sound, and costs the weapon durability through `hurt_weapon` (1, 3 for a rocket),
/// which reports whether the weapon broke. `random` is the shooter's.
#[allow(clippy::too_many_arguments)]
pub fn shoot(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    out: &mut Vec<Effect>,
    random: &mut LegacyRandom,
    shooter: Shooter,
    weapon: &ItemStack,
    projectiles: &[ItemStack],
    power: f32,
    uncertainty: f32,
    target: Option<Target>,
    mut hurt_weapon: impl FnMut(&mut Entities, i32) -> bool,
) {
    let max_angle = crate::player::play::game_data()
        .map_or(0.0, |d| d.enchants.projectile_spread(weapon, random, 0.0));
    #[allow(clippy::cast_precision_loss)]
    let (step, offset) = {
        let n = projectiles.len();
        let step = if n == 1 {
            0.0
        } else {
            2.0 * max_angle / (n as f32 - 1.0)
        };
        (step, ((n as i32 - 1) % 2) as f32 * step / 2.0)
    };
    let mut direction = 1.0_f32;
    for (i, projectile) in projectiles.iter().enumerate() {
        if projectile.is_empty() {
            continue;
        }
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
        let angle = offset + direction * ((i as i32 + 1) / 2) as f32 * step;
        direction = -direction;
        let rocket = is_firework(projectile);
        // The projectile starts at the eyes (`AbstractArrow(owner)`: 0.1 below, rockets 0.15).
        let start_y = shooter.eye_y - f64::from(if rocket { 0.15_f32 } else { 0.1_f32 });
        let dir = match target {
            Some(t) => {
                let xd = t.pos.x - shooter.pos.x;
                let zd = t.pos.z - shooter.pos.z;
                let dist = (xd * xd + zd * zd).sqrt();
                let yd = t.pos.y + f64::from(t.height) * 0.333_333_333_333_333_3 - start_y
                    + dist * f64::from(0.2_f32);
                shot_vector_at(shooter.x_rot, shooter.y_rot, Vec3::new(xd, yd, zd), angle)
            }
            None => shot_vector_view(shooter.x_rot, shooter.y_rot, angle),
        };
        let start = Vec3::new(shooter.pos.x, start_y, shooter.pos.z);
        if rocket {
            // `new FireworkRocketEntity(level, projectile, shooter, x, eyeY - 0.15, z, true)`.
            let id = crate::firework::crossbow_rocket(
                ents,
                network_ids,
                shooter.id,
                start,
                projectile,
                to_vec3(dir),
                power,
                uncertainty,
            );
            ents.launched(id, Some(shooter.id));
        } else {
            // `Arrow(owner)`: players' arrows can be picked up, free ones in creative only.
            let pickup = if !shooter.player {
                0
            } else if is_intangible(projectile) {
                2
            } else {
                1
            };
            let pierce = crate::player::play::game_data()
                .map_or(0, |d| d.enchants.piercing_count(weapon, projectile, random));
            let arrow = crate::mob::projectile::shoot_arrow(
                ents,
                network_ids,
                start,
                to_vec3(dir),
                power,
                uncertainty,
                Some(shooter.id),
                Some(weapon),
                projectile,
                shooter.player,
                pickup,
            );
            if let Some(a) = ents.arrow_mut(arrow) {
                a.pierce_level = u8::try_from(pierce.clamp(0, 127)).unwrap_or(0);
                a.crossbow_sound = true;
            }
        }
        let pitch = shot_pitch(random, i);
        let seed = crate::survival::sound_seed();
        crate::survival::play_sound(
            out,
            None,
            shooter.pos,
            "minecraft:item.crossbow.shoot",
            shooter.source,
            1.0,
            pitch,
            seed,
        );
        if hurt_weapon(ents, if rocket { 3 } else { 1 }) {
            break;
        }
    }
}

/// `getShootingPower`: rockets fly slower.
pub fn shooting_power(projectiles: &[ItemStack]) -> f32 {
    if projectiles.iter().any(is_firework) {
        FIREWORK_POWER
    } else {
        ARROW_POWER
    }
}

// --- Mobs (`CrossbowAttackMob`) ---------------------------------------------------------

/// `ProjectileUtil.getWeaponHoldingHand(mob, CROSSBOW)`.
pub fn mob_hand(m: &crate::mob::MobData) -> item::EquipmentSlot {
    if is_crossbow(m.equipment.get(item::EquipmentSlot::MainHand)) {
        item::EquipmentSlot::MainHand
    } else {
        item::EquipmentSlot::OffHand
    }
}

/// Whether the mob's crossbow is loaded (`CrossbowItem.isCharged(getWeaponItem())`).
pub fn mob_charged(m: &crate::mob::MobData) -> bool {
    is_charged(m.equipment.get(mob_hand(m)))
}

/// `CrossbowItem.onUseTick` of a mob that used its crossbow for `used` ticks: the loading
/// sounds and, fully drawn, the load. `flags` are the mob's sound flags.
pub fn mob_use_tick(t: &mut crate::mob::Tick<'_, '_>, flags: &mut UseFlags, used: i32) {
    let slot = mob_hand(t.mob_ref());
    let weapon = t.mob_ref().equipment.get(slot).clone();
    if !is_crossbow(&weapon) {
        return;
    }
    let duration = charge_duration(&weapon, &mut t.mob().random);
    let stage = use_tick(flags, used, duration);
    let pos = t.pos();
    let sounds = sounds(&weapon);
    for (on, sound) in [
        (stage.start_sound, sounds.start.as_deref()),
        (stage.mid_sound, sounds.mid.as_deref()),
    ] {
        if on && let Some(sound) = sound {
            let seed = crate::survival::sound_seed();
            crate::survival::play_sound(
                t.env.out,
                None,
                pos,
                sound,
                SoundSource::Players,
                0.5,
                1.0,
                seed,
            );
        }
    }
    if stage.full
        && !is_charged(&weapon)
        && mob_load(t, slot, weapon)
        && let Some(sound) = sounds.end.as_deref()
    {
        // `level.getRandom()`.
        let pitch = 1.0
            / (vanillite_base::random::RandomSource::next_float(&mut *t.env.random) * 0.5 + 1.0)
            + 0.2;
        let seed = crate::survival::sound_seed();
        let source = t.mob_ref().sound_source();
        crate::survival::play_sound(t.env.out, None, pos, sound, source, 1.0, pitch, seed);
    }
}

/// `tryLoadProjectiles` of a mob: `Monster.getProjectile` (a held arrow or rocket, else a new
/// arrow) drawn into the crossbow.
fn mob_load(
    t: &mut crate::mob::Tick<'_, '_>,
    slot: item::EquipmentSlot,
    weapon: ItemStack,
) -> bool {
    use item::EquipmentSlot as S;
    let m = t.mob_ref();
    let (main, off) = (m.equipment.get(S::MainHand), m.equipment.get(S::OffHand));
    let from = if is_held_projectile(off) {
        Some(S::OffHand)
    } else if is_held_projectile(main) {
        Some(S::MainHand)
    } else {
        None
    };
    let mut projectile = mob_projectile(main, off);
    let mut weapon = weapon;
    let drawn = draw(&weapon, &mut projectile, false, &mut *t.env.random);
    if let Some(from) = from {
        t.mob().equipment.set(from, projectile);
    }
    if drawn.is_empty() {
        return false;
    }
    set_charged(&mut weapon, &drawn);
    t.mob().equipment.set(slot, weapon);
    crate::mob::equipment::broadcast(t);
    true
}

/// `CrossbowAttackMob.performCrossbowAttack(body, power)`: a loaded crossbow fires at the
/// mob's target (`getTarget`), with `rangedAttackUncertainty`.
pub fn mob_shoot(t: &mut crate::mob::Tick<'_, '_>, target: Option<EntityId>, power: f32) {
    let slot = mob_hand(t.mob_ref());
    let mut weapon = t.mob_ref().equipment.get(slot).clone();
    if !is_crossbow(&weapon) {
        return;
    }
    let projectiles = charged(&weapon);
    set_charged(&mut weapon, &[]);
    t.mob().equipment.set(slot, weapon.clone());
    if projectiles.is_empty() {
        return;
    }
    let target = target.and_then(|id| t.other(id)).map(|o| Target {
        pos: o.pos,
        height: o.height,
    });
    let rot = t.ents.store.get(t.id).map(|e| e.rot()).unwrap_or_default();
    let shooter = Shooter {
        id: t.id,
        pos: t.pos(),
        eye_y: t.eye_y(),
        x_rot: rot.x_rot,
        y_rot: rot.y_rot,
        player: false,
        source: t.mob_ref().sound_source(),
    };
    #[allow(clippy::cast_precision_loss)]
    let uncertainty = (14 - t.env.difficulty as i32 * 4) as f32;
    let network_ids = std::sync::Arc::clone(t.level.network_ids_arc());
    let mut random = t.mob_ref().random.clone();
    let id = t.id;
    let mut broke = false;
    let level_random = &mut *t.env.random;
    shoot(
        t.ents,
        &network_ids,
        t.env.out,
        &mut random,
        shooter,
        &weapon,
        &projectiles,
        power,
        uncertainty,
        target,
        |ents, amount| {
            // `hurtAndBreak` of the crossbow (as `living::hurt_equipment`).
            let Some(m) = ents.mob_mut(id) else {
                return false;
            };
            let mut s = m.equipment.get(slot).clone();
            let change = crate::durability::change(&s, amount, false, &mut *level_random);
            if change == 0 {
                return false;
            }
            let d = item::damage(&s) + change;
            broke |= crate::durability::apply(&mut s, d);
            m.equipment.set(slot, s);
            broke
        },
    );
    t.mob().random = random;
    if broke {
        crate::mob::living::entity_event(t, id, crate::durability::break_event(slot));
    }
    crate::mob::equipment::broadcast(t);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charged_component_round_trip() {
        let mut bow = ItemStack::new(1, 1);
        assert!(!is_charged(&bow));
        let arrows = [ItemStack::new(5, 1), ItemStack::new(5, 1)];
        set_charged(&mut bow, &arrows);
        assert!(is_charged(&bow));
        assert_eq!(charged(&bow), arrows);
        set_charged(&mut bow, &[]);
        assert!(!is_charged(&bow));
        assert_eq!(bow, ItemStack::new(1, 1));
    }

    #[test]
    fn use_tick_stages() {
        let mut f = UseFlags::default();
        assert_eq!(use_tick(&mut f, 4, 25), UseTick::default());
        assert!(use_tick(&mut f, 5, 25).start_sound);
        assert!(!use_tick(&mut f, 6, 25).start_sound);
        assert!(use_tick(&mut f, 13, 25).mid_sound);
        assert!(use_tick(&mut f, 25, 25).full);
    }

    #[test]
    fn straight_shot_keeps_the_direction() {
        let v = shot_vector_at(0.0, 0.0, Vec3::new(3.0, 1.0, 4.0), 0.0);
        let n = normalize(to_v3(Vec3::new(3.0, 1.0, 4.0)));
        assert_eq!(v, n);
        let s = shot_vector_view(10.0, 30.0, 0.0);
        assert_eq!(s, to_v3(crate::mob::view_vector(10.0, 30.0)));
    }

    #[test]
    fn multishot_angles_mirror() {
        let a = shot_vector_at(0.0, 0.0, Vec3::new(0.0, 0.0, 1.0), 10.0);
        let b = shot_vector_at(0.0, 0.0, Vec3::new(0.0, 0.0, 1.0), -10.0);
        assert!((a[0] + b[0]).abs() < 1e-6 && (a[2] - b[2]).abs() < 1e-6);
        assert!((a[0].abs() - 10f32.to_radians().sin()).abs() < 1e-5);
    }
}
