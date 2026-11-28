//! The living entity tick of a mob in Vanilla's order: `Entity.baseTick`,
//! `LivingEntity.baseTick`, `Mob.baseTick`, `LivingEntity.aiStep` with `Mob.serverAiStep`,
//! `travel` and `Entity.move`, then the body rotation.

use vanillite_base::math::floor;
use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::EntityEvent;
use vanillite_world::entity::{EntityFlags, EntityId};

use super::physics::{self, Blocks};
use super::{Tick, combat, goals, kinds, nav};
use crate::survival::Effect;
use crate::survival::blocks::{Landing, fluid_contact, inside_blocks, is_in_wall};
use crate::survival::damage::DamageSource;
use crate::survival::rules::Rule;

/// `Entity.EntityEvent` ids used by mobs.
pub mod event {
    pub const DEATH: i8 = 3;
    pub const EAT_GRASS: i8 = 10;
    pub const POOF: i8 = 60;
    pub const DROWN: i8 = 67;
    pub const SPAWN_POOF: i8 = 20;
}

/// Broadcasts an entity event of the ticking mob to the players tracking it.
pub fn entity_event(t: &mut Tick<'_, '_>, id: EntityId, event: i8) {
    let Some(e) = t.ents.store.get(id) else {
        return;
    };
    let network_id = e.network_id().0;
    t.env.out.push(Effect::Tracking {
        entity: id,
        with_self: false,
        packet: SharedPacket::of(&EntityEvent {
            entity_id: network_id,
            event,
        }),
    });
}

/// `Mob.tick` (with the kind specific parts before and after `LivingEntity.tick`).
pub fn tick(t: &mut Tick<'_, '_>) {
    // `Entity.baseTick`: `computeSpeed`.
    let pos = t.pos();
    let m = t.mob();
    m.known_speed = pos - m.last_known_pos.unwrap_or(pos);
    m.last_known_pos = Some(pos);
    kinds::before_tick(t);
    if t.mob_ref().removed {
        return;
    }
    base_tick(t);
    if t.mob_ref().removed {
        return;
    }
    // `updatingUsingItem` of the zombies' spears.
    if matches!(
        t.mob_ref().kind,
        super::Kind::Zombie
            | super::Kind::Husk
            | super::Kind::ZombieVillager
            | super::Kind::ZombifiedPiglin
    ) {
        super::fam::undead::zombies::spear_use_tick(t);
        if !t.exists() || t.mob_ref().removed {
            return;
        }
    }
    // `detectEquipmentUpdates`.
    super::fam::undead::cubes::detect_equipment(t);
    ai_step(t);
    if t.mob_ref().removed || !t.exists() {
        return;
    }
    // `tickHeadTurn` → `BodyRotationControl.clientTick` (`CamelBodyRotationControl`, the
    // armadillo's: none while it is scared; `ShulkerBodyRotationControl`: never).
    let turns_body = match t.mob_ref().kind {
        super::Kind::Shulker => false,
        super::Kind::Camel | super::Kind::CamelHusk => super::fam::mount::camel::turns_body(t),
        super::Kind::Armadillo => !super::fam::brainy::armadillo::is_scared(t.mob_ref()),
        // `CreakingBodyRotationControl`: only while it can move.
        super::Kind::Creaking => super::fam::mount::creaking::can_move(t.mob_ref()),
        _ => true,
    };
    if t.mob_ref().kind == super::Kind::ArmorStand {
        // `ArmorStand.tickHeadTurn`: the body follows the yaw.
        let y = t.core().rot().y_rot;
        crate::decoration::stand::tick_head_turn(t.mob(), y);
    } else if turns_body {
        nav::tick_body_rotation(t);
    }
    // `refreshDirtyAttributes`: a lowered max health caps the health.
    {
        let m = t.mob();
        #[allow(clippy::cast_possible_truncation)]
        let max = m.attrs.max_health as f32;
        if m.health > max {
            m.health = max;
        }
    }
    if t.mob_ref().tick_count % 5 == 0 {
        update_control_flags(t);
    }
    kinds::after_tick(t);
}

/// `Mob.updateControlFlags`: a mob a mob rider steers runs no move or look goals of its own;
/// a mob in a boat does not jump.
fn update_control_flags(t: &mut Tick<'_, '_>) {
    use super::goals::flag;
    let no_controller = super::controlling_mob_passenger(t.ents, t.id).is_none();
    let not_in_boat = t.ents.vehicle_of(t.id).is_none_or(|v| {
        t.ents
            .store
            .get(v)
            .is_none_or(|e| crate::vehicle::BoatKind::of(e.kind()).is_none())
    });
    let goals = &mut t.mob().goals;
    goals.set_control_flags(flag::MOVE | flag::LOOK, no_controller);
    goals.set_control_flags(flag::JUMP, no_controller && not_in_boat);
}

/// `Entity.baseTick` + `LivingEntity.baseTick` + `Mob.baseTick`.
fn base_tick(t: &mut Tick<'_, '_>) {
    {
        let m = t.mob();
        m.was_in_powder_snow = m.in_powder_snow;
        m.in_powder_snow = false;
    }
    sprint_particle(t);
    update_fluids(t);
    let fire_immune = t.core().kind().info().fire_immune;
    let in_lava = t.mob_ref().fluids.in_lava();
    if t.mob_ref().fire_ticks > 0 {
        if fire_immune {
            t.mob().fire_ticks = 0;
        } else {
            if t.mob_ref().fire_ticks % 20 == 0 && !in_lava {
                let source = DamageSource::of(t.env.types.on_fire);
                hurt_self(t, source, 1.0);
            }
            t.mob().fire_ticks -= 1;
        }
    }
    if in_lava {
        t.mob().fall_distance *= 0.5;
    }
    if t.pos().y < f64::from(t.level.min_y() - 64) {
        let source = DamageSource::of(t.env.types.out_of_world);
        hurt_self(t, source, 4.0);
    }
    let on_fire = t.mob_ref().fire_ticks > 0;
    t.core_mut().set_flags(EntityFlags::ON_FIRE, on_fire);
    if !t.exists() {
        return;
    }
    // The end of `Entity.baseTick`: `Leashable.tickLeash`.
    super::tame::tick_leash(t);
    if !t.exists() {
        return;
    }
    // `LivingEntity.baseTick`.
    if t.mob_ref().is_alive() {
        let eye = Vec3::new(t.pos().x, t.eye_y(), t.pos().z);
        let width = t.core().dims().width;
        if is_in_wall(t.env.blocks, t.env.facts, eye, width) {
            let source = DamageSource::of(t.env.types.in_wall);
            hurt_self(t, source, 1.0);
        }
        if !t.exists() {
            return;
        }
        let eye_in_water = t.mob_ref().fluids.eye_in_water;
        let bubble = {
            let p = t.pos();
            t.env
                .facts
                .is_bubble_column(t.level.block(BlockPos::containing(p.x, t.eye_y(), p.z)))
        };
        if eye_in_water && !bubble {
            // `SulfurCube.canBreatheUnderwater`: also while holding a block.
            let breathes = t.mob_ref().kind.breathes_underwater()
                || (t.mob_ref().kind == super::Kind::SulfurCube
                    && super::fam::undead::cubes::has_body_item(t.mob_ref()));
            if !breathes && !t.mob_ref().effects.has_water_breathing() {
                // `decreaseAirSupply`: Respiration (`OXYGEN_BONUS`) may keep the air.
                let m = t.mob();
                let bonus =
                    crate::mob::equipment::attribute(m, "minecraft:oxygen_bonus", 0.0, 0.0, 1024.0);
                if !(bonus > 0.0 && m.random.next_double() >= 1.0 / (bonus + 1.0)) {
                    m.air -= 1;
                }
                if t.mob_ref().air <= -20 {
                    t.mob().air = 0;
                    let id = t.id;
                    entity_event(t, id, event::DROWN);
                    let source = DamageSource::of(t.env.types.drown);
                    hurt_self(t, source, 2.0);
                }
            } else if t.mob_ref().air < 300 && t.mob_ref().effects.refills_air() {
                let m = t.mob();
                m.air = (m.air + 4).min(300);
            }
            if t.exists() {
                let id = t.id;
                crate::riding::dismount_underwater(t.ents, t.env, id);
            }
        } else if t.mob_ref().air < 300 {
            let m = t.mob();
            m.air = (m.air + 4).min(300);
        }
        if !t.exists() {
            return;
        }
        let p = t.block_pos();
        t.mob().last_pos = p;
    }
    {
        let m = t.mob();
        if m.hurt_time > 0 {
            m.hurt_time -= 1;
        }
        if m.damage_cooldown > 0 {
            m.damage_cooldown -= 1;
        }
    }
    // `LivingEntity.tick`: stuck arrows fall out one by one.
    if !t.ents.stuck_arrows.is_empty() {
        let id = t.id;
        let changed = t
            .ents
            .stuck_arrows
            .get_mut(&id)
            .is_some_and(super::projectile::StuckArrows::tick);
        if t.ents.stuck_arrows.get(&id).is_some_and(|a| a.count == 0) {
            t.ents.stuck_arrows.remove(&id);
        }
        if changed {
            t.core_mut().mark_data_dirty();
        }
    }
    if t.mob_ref().is_dead_or_dying() {
        tick_death(t);
        if t.mob_ref().removed {
            return;
        }
    }
    {
        let m = t.mob();
        if m.last_hurt_by_player_time > 0 {
            m.last_hurt_by_player_time -= 1;
        } else {
            m.last_hurt_by_player = None;
        }
    }
    if let Some(mob) = t.mob_ref().last_hurt_mob
        && t.other(mob).is_none()
    {
        t.mob().last_hurt_mob = None;
    }
    if let Some(by) = t.mob_ref().last_hurt_by_mob {
        let expired = t.mob_ref().tick_count - t.mob_ref().last_hurt_by_mob_time > 100;
        if t.other(by).is_none() || expired {
            t.mob().last_hurt_by_mob = None;
        }
    }
    // `tickEffects`.
    crate::effect::living::tick_mob(t);
    if !t.exists() || t.mob_ref().removed {
        return;
    }
    // `Mob.baseTick`: the ambient sound (armor stands and mannequins are no `Mob`).
    if t.mob_ref().is_alive() && t.mob_ref().kind.family() != super::Family::Decor {
        let m = t.mob();
        let roll = m.random.next_int_bounded(1000);
        let time = m.ambient_sound_time;
        m.ambient_sound_time += 1;
        if roll < time {
            m.ambient_sound_time = -m.kind.ambient_sound_interval();
            kinds::play_ambient_sound(t);
        }
    }
    if t.exists() && t.mob_ref().kind.family() != super::Family::Classic {
        super::fam::after_base_tick(t);
    }
}

/// `LivingEntity.tickDeath`.
fn tick_death(t: &mut Tick<'_, '_>) {
    if super::boss::tick_death(t) {
        return;
    }
    t.mob().death_time += 1;
    if t.mob_ref().death_time >= 20 {
        let id = t.id;
        entity_event(t, id, event::POOF);
        // `remove(KILLED)` → `triggerOnDeathMobEffects`.
        crate::effect::living::on_mob_killed(t);
        t.mob().removed = true;
    }
}

/// `LivingEntity.heal(amount)`.
pub fn heal(t: &mut Tick<'_, '_>, amount: f32) {
    let m = t.mob();
    if m.health > 0.0 {
        let max = m.max_health();
        m.health = (m.health + amount).clamp(0.0, max);
        t.core_mut().mark_data_dirty();
    }
}

/// Hurts the ticking mob itself (environment damage).
pub fn hurt_self(t: &mut Tick<'_, '_>, source: DamageSource, amount: f32) -> bool {
    let id = t.id;
    combat::hurt(t.level, t.ents, t.env, t.cx, id, source, amount)
}

/// `updateFluidInteraction` for water and lava: the heights, whether the eyes are inside and
/// the push of flowing fluids (`EntityFluidInteraction.applyCurrentTo`) unless the kind is
/// not pushed (`isPushedByFluid`).
fn update_fluids(t: &mut Tick<'_, '_>) {
    let p = t.pos();
    let eye = Vec3::new(p.x, t.eye_y(), p.z);
    let pushed = super::fam::pushed_by_fluid(t.mob_ref());
    let (contact, currents) = if pushed {
        fluid_currents(t.level, t.env.facts, t.aabb(), eye)
    } else {
        (
            fluid_contact(t.env.blocks, t.env.facts, t.aabb(), eye),
            Currents::default(),
        )
    };
    let first_tick = t.mob_ref().tick_count <= 1;
    let m = t.mob();
    let was_in_water = m.fluids.in_water();
    m.fluids = contact;
    if let Some(r) = &mut m.move_ctl.rabbit {
        r.in_water = contact.in_water();
    }
    if contact.in_water() {
        // `updateInWaterStateAndDoWaterCurrentPushing`: water ends the fall and the fire.
        m.fall_distance = 0.0;
        if m.fire_ticks > 0 {
            m.fire_ticks = 0;
        }
        if !was_in_water && !first_tick {
            water_splash_effect(t);
        }
    }
    if pushed {
        if contact.in_water() {
            apply_current(t, currents.water, 0.014);
        }
        if contact.in_lava() {
            let scale = if t.level.settings.fast_lava {
                0.007
            } else {
                0.002_333_333_333_333_333_5
            };
            apply_current(t, currents.lava, scale);
        }
    }
}

/// The draws of `Entity.doWaterSplashEffect` for an entity of `width` moving with `v`: the
/// volume and the pitch of the splash sound, then the bubble and splash particles (the
/// server adds none, but they draw from the entity's random). `modifier` is 0.2, or 0.9
/// when a passenger controls the entity.
pub fn water_splash_draws(
    random: &mut impl RandomSource,
    v: Vec3,
    width: f32,
    modifier: f32,
) -> (f32, f32) {
    let point_two = f64::from(0.2_f32);
    #[allow(clippy::cast_possible_truncation)]
    let volume = ((v.x * v.x * point_two + v.y * v.y + v.z * v.z * point_two).sqrt() as f32
        * modifier)
        .min(1.0);
    let pitch = 1.0 + (random.next_float() - random.next_float()) * 0.4;
    // The bubbles' x, z and y speed, then the splashes' x and z.
    for draws in [3, 2] {
        let mut i = 0_u16;
        while f32::from(i) < 1.0 + width * 20.0 {
            for _ in 0..draws {
                random.next_double();
            }
            i += 1;
        }
    }
    (volume, pitch)
}

/// `Entity.doWaterSplashEffect`: the splash sound and particles of a mob falling into water.
fn water_splash_effect(t: &mut Tick<'_, '_>) {
    let v = t.velocity();
    let width = t.core().dims().width;
    let (speed, pitch) = water_splash_draws(&mut t.mob().random, v, width, 0.2);
    let kind = t.mob_ref().kind;
    let sound = match kind {
        super::Kind::Axolotl if speed < 0.25 => "minecraft:entity.axolotl.splash",
        super::Kind::Dolphin if speed < 0.25 => "minecraft:entity.dolphin.splash",
        _ if speed < 0.25 && hostile_swimmer(kind) => "minecraft:entity.hostile.splash",
        _ => "minecraft:entity.generic.splash",
    };
    kinds::play_sound(t, sound, speed, pitch);
    let pos = t.pos();
    post_game_event(t, crate::level::game_event::events().splash, pos, None);
}

/// `Monster` (and hoglins) with the hostile swim and splash sounds.
fn hostile_swimmer(kind: super::Kind) -> bool {
    use super::Kind;
    !matches!(
        kind,
        Kind::Slime | Kind::MagmaCube | Kind::Ghast | Kind::Phantom
    ) && kind.is_monster()
}

/// The summed flows of the water and lava an entity touches
/// (`EntityFluidInteraction.CurrentAccumulator`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Currents {
    pub water: Current,
    pub lava: Current,
}

/// One fluid's accumulated current.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Current {
    pub sum: Vec3,
    pub count: u32,
}

/// `FluidState.getHeight`: full under the same fluid, else its own height.
fn fluid_height(
    level: &crate::level::Level,
    facts: &crate::survival::blocks::BlockFacts,
    pos: BlockPos,
    fluid: vanillite_data::fluid::FluidState,
) -> f32 {
    let above = level
        .block(BlockPos::new(pos.x, pos.y + 1, pos.z))
        .fluid_state();
    let same = (facts.is_water(fluid) && facts.is_water(above))
        || (facts.is_lava(fluid) && facts.is_lava(above));
    if same { 1.0 } else { fluid.info().own_height }
}

/// `EntityFluidInteraction.update` with the currents: the contact like
/// [`fluid_contact`], and per fluid the flow of every block the box touches (scaled by the
/// height while it is below 0.4).
pub fn fluid_currents(
    level: &crate::level::Level,
    facts: &crate::survival::blocks::BlockFacts,
    aabb: Aabb,
    eye: Vec3,
) -> (crate::survival::blocks::FluidContact, Currents) {
    let mut contact = crate::survival::blocks::FluidContact::default();
    let mut currents = Currents::default();
    let b = aabb.inflate(-0.001, -0.001, -0.001);
    let ceil = |v: f64| vanillite_base::math::ceil(v);
    let (x0, y0, z0) = (floor(b.min_x), floor(b.min_y), floor(b.min_z));
    let (x1, y1, z1) = (ceil(b.max_x) - 1, ceil(b.max_y) - 1, ceil(b.max_z) - 1);
    // `hasFluidAndLoaded`: the chunks around the box are loaded.
    for (x, z) in [
        (x0 - 1, z0 - 1),
        (x1 + 1, z1 + 1),
        (x0 - 1, z1 + 1),
        (x1 + 1, z0 - 1),
    ] {
        if !level.is_loaded(BlockPos::new(x, y0, z)) {
            return (contact, currents);
        }
    }
    // Sections without a fluid add nothing; skipping their blocks only drops reads, which have
    // no effect while reads are pure.
    if level.chunks.reads_are_pure() {
        let view = level.view().chunks();
        let dry = (x0 >> 4..=x1 >> 4).all(|cx| {
            (z0 >> 4..=z1 >> 4).all(|cz| {
                (y0 >> 4..=y1 >> 4).all(|sy| {
                    view.section_summary(vanillite_base::ChunkPos::new(cx, cz), sy)
                        .is_none_or(|summary| summary.fluids == 0)
                })
            })
        });
        if dry {
            return (contact, currents);
        }
    }
    let (eye_x, eye_z) = (floor(eye.x), floor(eye.z));
    let (mut water_height, mut lava_height) = (0.0_f64, 0.0_f64);
    for x in x0..=x1 {
        for y in y0..=y1 {
            for z in z0..=z1 {
                let pos = BlockPos::new(x, y, z);
                let state = level.block(pos);
                let fluid = state.fluid_state();
                if fluid.is_empty() {
                    continue;
                }
                let water = facts.is_water(fluid);
                if !water && !facts.is_lava(fluid) {
                    continue;
                }
                let bottom = f64::from(y);
                let top = bottom + f64::from(fluid_height(level, facts, pos, fluid));
                if top < b.min_y {
                    continue;
                }
                let eyes = x == eye_x && z == eye_z && eye.y >= bottom && {
                    // `getHeightForCamera`: a source under a sturdy bottom face is full.
                    let above = level.block(BlockPos::new(x, y + 1, z));
                    let camera = if fluid.info().source && above.misc().sturdy_full & 1 != 0 {
                        1.0
                    } else {
                        fluid_height(level, facts, pos, fluid)
                    };
                    eye.y <= bottom + f64::from(camera)
                };
                let height = top - aabb.min_y;
                let (running, current) = if water {
                    contact.water_height = contact.water_height.max(height);
                    contact.eye_in_water |= eyes;
                    water_height = water_height.max(height);
                    (water_height, &mut currents.water)
                } else {
                    contact.lava_height = contact.lava_height.max(height);
                    contact.eye_in_lava |= eyes;
                    lava_height = lava_height.max(height);
                    (lava_height, &mut currents.lava)
                };
                let mut flow = fluid_flow(level, facts, pos, fluid, water);
                if running < 0.4 {
                    flow = flow * running;
                }
                current.sum = current.sum + flow;
                current.count += 1;
            }
        }
    }
    (contact, currents)
}

/// `FlowingFluid.getFlow(level, pos, fluid)`.
pub fn fluid_flow(
    level: &dyn Blocks,
    facts: &crate::survival::blocks::BlockFacts,
    pos: BlockPos,
    fluid: vanillite_data::fluid::FluidState,
    water: bool,
) -> Vec3 {
    let same = |f: vanillite_data::fluid::FluidState| {
        if water {
            facts.is_water(f)
        } else {
            facts.is_lava(f)
        }
    };
    // `affectsFlow`: empty or the same fluid.
    let affects = |f: vanillite_data::fluid::FluidState| f.is_empty() || same(f);
    let own = fluid.info().own_height;
    // `Direction.Plane.HORIZONTAL`: north, east, south, west.
    const HORIZONTAL: [(i32, i32); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)];
    let (mut fx, mut fz) = (0.0_f64, 0.0_f64);
    for (dx, dz) in HORIZONTAL {
        let n = BlockPos::new(pos.x + dx, pos.y, pos.z + dz);
        let state = level.state(n);
        let nf = state.fluid_state();
        if !affects(nf) {
            continue;
        }
        let mut height = if nf.is_empty() {
            0.0
        } else {
            nf.info().own_height
        };
        let mut distance = 0.0_f32;
        if height == 0.0 {
            if !blocks_fluid_flow(state) {
                let below = level.state(BlockPos::new(n.x, n.y - 1, n.z)).fluid_state();
                if affects(below) {
                    height = if below.is_empty() {
                        0.0
                    } else {
                        below.info().own_height
                    };
                    if height > 0.0 {
                        distance = own - (height - 0.888_888_9);
                    }
                }
            }
        } else if height > 0.0 {
            distance = own - height;
        }
        if distance != 0.0 {
            #[allow(clippy::cast_precision_loss)]
            {
                fx += f64::from(dx as f32 * distance);
                fz += f64::from(dz as f32 * distance);
            }
        }
    }
    let mut flow = Vec3::new(fx, 0.0, fz);
    if fluid.info().falling {
        // `isSolidFace` towards each side at the block or the one above.
        let solid_face = |p: BlockPos, face_bit: u8| {
            let state = level.state(p);
            let f = state.fluid_state();
            if same(f) {
                return false;
            }
            // `IceBlock` (ice, frosted ice) never counts.
            if matches!(
                state.block().name(),
                "minecraft:ice" | "minecraft:frosted_ice"
            ) {
                return false;
            }
            state.misc().sturdy_full & face_bit != 0
        };
        // `Direction` 3D data values: north 2, south 3, west 4, east 5.
        const FACES: [(i32, i32, u8); 4] = [
            (0, -1, 1 << 2),
            (1, 0, 1 << 5),
            (0, 1, 1 << 3),
            (-1, 0, 1 << 4),
        ];
        for (dx, dz, bit) in FACES {
            let p = BlockPos::new(pos.x + dx, pos.y, pos.z + dz);
            let up = BlockPos::new(p.x, p.y + 1, p.z);
            if solid_face(p, bit) || solid_face(up, bit) {
                flow = flow.normalize() + Vec3::new(0.0, -6.0, 0.0);
                break;
            }
        }
    }
    flow.normalize()
}

/// `#minecraft:blocks_fluid_flow` (`#blocks_motion` and the signs): blocks that stop the flow
/// towards a lower neighbour.
fn blocks_fluid_flow(state: BlockState) -> bool {
    let block = state.block();
    let name = block.name();
    if name.ends_with("_sign") {
        return true;
    }
    if block == vanillite_data::block::ids::COBWEB || name == "minecraft:bamboo_sapling" {
        return false;
    }
    state.has(vanillite_data::block::flags::SOLID)
}

/// `CurrentAccumulator.applyTo(entity, scale)` for a mob.
fn apply_current(t: &mut Tick<'_, '_>, current: Current, scale: f64) {
    if let Some(pushed) = current.push(t.velocity(), scale) {
        t.set_velocity(pushed);
    }
}

impl Current {
    /// `CurrentAccumulator.applyTo(entity, scale)` for an entity that is no player: the
    /// movement `old` plus the normalized summed flow, at least 0.0045 long when the entity
    /// barely moves. `None` without a current.
    pub fn push(self, old: Vec3, scale: f64) -> Option<Vec3> {
        if self.count == 0 || self.sum.length_sqr() < f64::from(1.0e-5_f32) {
            return None;
        }
        let mut impulse = self.sum.normalize() * scale;
        if old.x.abs() < 0.003
            && old.z.abs() < 0.003
            && impulse.length() < 0.004_500_000_000_000_000_5
        {
            impulse = impulse.normalize() * 0.004_500_000_000_000_000_5;
        }
        Some(old + impulse)
    }
}

/// `LivingEntity.travelFlying(input, waterSpeed, lavaSpeed, airSpeed)`: flying mobs without
/// gravity (ghasts, phantoms, allays, happy ghasts).
pub fn travel_flying(
    t: &mut Tick<'_, '_>,
    input: Vec3,
    water_speed: f32,
    lava_speed: f32,
    air_speed: f32,
) {
    let fluids = t.mob_ref().fluids;
    let (speed, drag) = if fluids.in_water() {
        (water_speed, f64::from(0.8_f32))
    } else if fluids.in_lava() {
        (lava_speed, 0.5)
    } else {
        (air_speed, f64::from(0.91_f32))
    };
    move_relative(t, speed, input);
    let v = t.velocity();
    move_self(t, v);
    if !t.exists() {
        return;
    }
    let v = t.velocity();
    t.set_velocity(v * drag);
}

/// `getFluidJumpThreshold`: 0.4 blocks, or the eye height for small mobs.
fn fluid_jump_threshold(t: &Tick<'_, '_>) -> f64 {
    if let Some(v) =
        super::fam::undead::cubes::fluid_jump_threshold(t.mob_ref(), t.core().dims().height)
    {
        return v;
    }
    let eye = super::eye_height(t.mob_ref());
    if eye < 0.4 { 0.0 } else { 0.4 }
}

/// `LivingEntity.aiStep` with `Mob.aiStep`'s additions.
fn ai_step(t: &mut Tick<'_, '_>) {
    // The kind's `aiStep` override before it calls `super.aiStep()`.
    if t.mob_ref().kind.family() != super::Family::Classic {
        super::fam::ai_step_start(t);
        if !t.exists() || t.mob_ref().removed {
            return;
        }
    } else if t.mob_ref().kind == super::Kind::Enderman {
        // `EnderMan.aiStep`: `updatePersistentAnger(level, true)` (20 to 39 seconds).
        t.mob().jumping = false;
        super::common::update_persistent_anger(t, true, (400, 780));
    }
    // `Monster.updateNoActionTime` (`Raider`: always).
    let kind = t.mob_ref().kind;
    if kind.extends_monster() && (kind.is_raider() || t.light_magic_value() > 0.5) {
        t.mob().no_action_time += 2;
    }
    {
        let m = t.mob();
        if m.no_jump_delay > 0 {
            m.no_jump_delay -= 1;
        }
    }
    let mut v = t.velocity();
    if v.x.abs() < 0.003 {
        v.x = 0.0;
    }
    if v.z.abs() < 0.003 {
        v.z = 0.0;
    }
    if v.y.abs() < 0.003 {
        v.y = 0.0;
    }
    t.set_velocity(v);
    {
        let m = t.mob();
        m.xxa *= 0.98;
        m.zza *= 0.98;
    }
    if super::boss::ai_step_override(t) {
        return;
    }
    let decor = t.mob_ref().kind.family() == super::Family::Decor;
    // `isEffectiveAi`: armor stands move with physics, mannequins unless immovable; neither
    // has a `serverAiStep`.
    // A mob its player rider steers (`getControllingPassenger` is a player) is the client's
    // authority: no `serverAiStep` (`isEffectiveAi`) and no movement of its own on the server.
    let rider = t
        .ents
        .riding
        .passengers_of(t.id)
        .first()
        .copied()
        .filter(|&p| crate::riding::controls(t.ents, p, t.id));
    let effective_ai = if decor {
        let no_gravity = t.core().flags().contains(EntityFlags::NO_GRAVITY);
        crate::decoration::stand::moves(t.mob_ref(), no_gravity)
    } else {
        !t.mob_ref().no_ai && rider.is_none()
    };
    // `isImmobile`: dead or held by the kind (a stunned ravager).
    if t.mob_ref().is_dead_or_dying() || super::fam::is_immobile(t.mob_ref()) {
        let m = t.mob();
        m.jumping = false;
        m.xxa = 0.0;
        m.zza = 0.0;
    } else if effective_ai && !decor {
        server_ai_step(t);
        if !t.exists() || t.mob_ref().removed {
            return;
        }
    }
    // Jumping.
    if t.mob_ref().jumping {
        let fluids = t.mob_ref().fluids;
        let height = if fluids.in_lava() {
            fluids.lava_height
        } else {
            fluids.water_height
        };
        let in_water_with_height = fluids.in_water() && height > 0.0;
        let threshold = fluid_jump_threshold(t);
        let on_ground = t.on_ground();
        if !in_water_with_height || on_ground && height <= threshold {
            if !fluids.in_lava() || on_ground && fluids.lava_height <= threshold {
                if (on_ground || in_water_with_height && height <= threshold)
                    && t.mob_ref().no_jump_delay == 0
                {
                    jump_from_ground(t);
                    t.mob().no_jump_delay = 10;
                }
            } else {
                jump_in_liquid(t);
            }
        } else {
            jump_in_liquid(t);
        }
    } else {
        t.mob().no_jump_delay = 0;
    }
    {
        // Slow Falling and Levitation reset the fall distance before travelling.
        let m = t.mob();
        if m.effects.has(crate::effect::MobEffect::SLOW_FALLING)
            || m.effects.has(crate::effect::MobEffect::LEVITATION)
        {
            m.fall_distance = 0.0;
        }
    }
    t.cx.movements.clear();
    if let Some(rider) = rider {
        // `travelRidden`: `tickRidden`, then (`canSimulateMovement` is false on the server) no
        // motion; the rider's client moves it.
        if t.mob_ref().is_alive() {
            tick_ridden(t, rider);
            t.set_velocity(Vec3::ZERO);
        }
    } else if effective_ai {
        let m = t.mob_ref();
        let input = Vec3::new(f64::from(m.xxa), f64::from(m.yya), f64::from(m.zza));
        travel(t, input);
    }
    apply_effects_from_blocks(t);
    if !t.exists() {
        return;
    }
    tick_freezing(t);
    if !t.exists() {
        return;
    }
    push_entities(t);
    if !t.exists() {
        return;
    }
    if t.mob_ref().kind.sensitive_to_water() {
        let p = t.pos();
        let rain = (t.env.raining_at)(BlockPos::containing(p.x, p.y, p.z))
            || (t.env.raining_at)(BlockPos::containing(p.x, t.aabb().max_y, p.z));
        if t.mob_ref().fluids.in_water() || rain {
            let source = DamageSource::of(t.env.types.drown);
            hurt_self(t, source, 1.0);
        }
    }
    if !t.exists() {
        return;
    }
    if t.mob_ref().kind.burns_in_daylight() && t.mob_ref().is_alive() && is_sun_burn_tick(t) {
        // `burnUndead`: a helmet takes the sun (and wears), else `igniteForSeconds(8)`.
        // `sunProtectionSlot`: the body armour of zombie horses and zombie nautiluses.
        let head = if matches!(
            t.mob_ref().kind,
            super::Kind::ZombieHorse | super::Kind::ZombieNautilus
        ) {
            crate::item::EquipmentSlot::Body
        } else {
            crate::item::EquipmentSlot::Head
        };
        let helmet = t.mob_ref().equipment.get(head).clone();
        if helmet.is_empty() {
            if super::attack::ignite_mob(t.mob(), 160) {
                t.core_mut().mark_data_dirty();
            }
        } else if crate::item::is_damageable(&helmet) {
            let amount = t.mob().random.next_int_bounded(2);
            hurt_equipment(t, head, amount);
        }
    }
    // `Mob.aiStep`'s looting.
    if t.mob_ref().kind == super::Kind::Skeleton {
        // `AbstractSkeleton`'s pickup rules and `onEquipItem` reassessment.
        super::fam::undead::skeletons::pick_up_items(t);
    } else if t.mob_ref().kind.family() == super::Family::Classic {
        let rules = super::equipment::PickupRules::default();
        super::equipment::pick_up_items(t, &rules, None, (1.0, 0.0, 1.0));
    } else {
        super::fam::pick_up_items(t);
    }
    if !t.exists() {
        return;
    }
    kinds::ai_step(t);
}

/// `tickRidden` of a mob its player `rider` steers: it turns to the rider's view
/// (`getRiddenRotation`: the rider's yaw, half its pitch).
fn tick_ridden(t: &mut Tick<'_, '_>, rider: EntityId) {
    use super::Kind;
    let Some(r) = t.ents.store.get(rider).map(|e| e.rot()) else {
        return;
    };
    let kind = t.mob_ref().kind;
    let mut rot = t.core().rot();
    let y_rot = match kind {
        // `AbstractNautilus` and `HappyGhast`: a part of the way each tick.
        Kind::Nautilus | Kind::ZombieNautilus | Kind::HappyGhast => {
            let turn = if kind == Kind::HappyGhast { 0.08 } else { 0.5 };
            rot.y_rot + vanillite_base::math::wrap_degrees_f(r.y_rot - rot.y_rot) * turn
        }
        // `Camel.getRiddenRotation`: its own while it refuses to move.
        Kind::Camel | Kind::CamelHusk
            if super::fam::mount::camel::refuse_to_move(t.mob_ref(), t.env.game_time) =>
        {
            return;
        }
        _ => r.y_rot,
    };
    rot.y_rot = y_rot;
    rot.x_rot = r.x_rot * 0.5;
    rot.y_head_rot = y_rot;
    t.core_mut().set_rot(rot);
    t.mob().y_body_rot = y_rot;
}

/// `hurtAndBreak(amount, mob, slot)` of a mob's equipment: the `item_damage` effects
/// (Unbreaking) from the level random, and the slot's break event when it breaks.
pub fn hurt_equipment(t: &mut Tick<'_, '_>, slot: crate::item::EquipmentSlot, amount: i32) {
    let mut s = t.mob_ref().equipment.get(slot).clone();
    let change = crate::durability::change(&s, amount, false, &mut *t.env.random);
    if change == 0 {
        return;
    }
    let d = crate::item::damage(&s) + change;
    let broke = crate::durability::apply(&mut s, d);
    t.mob().equipment.set(slot, s);
    if broke {
        let id = t.id;
        entity_event(t, id, crate::durability::break_event(slot));
    }
    super::equipment::broadcast(t);
}

/// `Mob.jumpInLiquid`.
fn jump_in_liquid(t: &mut Tick<'_, '_>) {
    if t.mob_ref().kind.family() != super::Family::Classic && super::fam::jump_in_liquid(t) {
        return;
    }
    let dy = if t.mob_ref().nav.can_float {
        f64::from(0.04_f32)
    } else {
        0.3
    };
    let v = t.velocity();
    t.set_velocity(Vec3::new(v.x, v.y + dy, v.z));
}

/// `jumpFromGround`.
fn jump_from_ground(t: &mut Tick<'_, '_>) {
    let rabbit = t.mob_ref().kind == super::Kind::Rabbit;
    let multiplier = if rabbit {
        super::fam::companion::rabbit::jump_multiplier(t)
    } else {
        1.0
    };
    #[allow(clippy::cast_possible_truncation)]
    let mut power = t.mob_ref().attrs.jump_strength as f32 * multiplier * block_jump_factor(t)
        + t.mob_ref().effects.jump_boost_power();
    if power <= 1.0e-5 {
        return;
    }
    if t.mob_ref().kind.family() != super::Family::Classic {
        power += super::fam::jump_power_bonus(t.mob_ref());
    }
    let v = t.velocity();
    t.set_velocity(Vec3::new(v.x, f64::from(power).max(v.y), v.z));
    t.core_mut().set_flags(EntityFlags::HAS_IMPULSE, true);
    if rabbit {
        super::fam::companion::rabbit::after_jump(t);
    }
}

fn block_jump_factor(t: &Tick<'_, '_>) -> f32 {
    let here = t.level.block(t.block_pos()).block().info().jump_factor;
    let below = t
        .level
        .block(pos_below_affecting_movement(t))
        .block()
        .info()
        .jump_factor;
    if here == 1.0 { below } else { here }
}

fn block_speed_factor(t: &Tick<'_, '_>) -> f32 {
    let state = t.level.block(t.block_pos());
    let here = state.block().info().speed_factor;
    let water = super::is_water(state) && state.block().name() == "minecraft:water";
    let factor = if water || t.env.facts.is_bubble_column(state) || here != 1.0 {
        here
    } else {
        t.level
            .block(pos_below_affecting_movement(t))
            .block()
            .info()
            .speed_factor
    };
    #[allow(clippy::cast_possible_truncation)]
    let efficiency = t.mob_ref().attrs.movement_efficiency as f32;
    factor + efficiency * (1.0 - factor)
}

/// `canSpawnSprintParticle` → `spawnSprintParticle`: the particles are the client's, the
/// server still draws their position.
fn sprint_particle(t: &mut Tick<'_, '_>) {
    // `canSpawnSprintParticle`: iron golems walking one in five ticks, rabbits never.
    let can = match t.mob_ref().kind {
        super::Kind::Rabbit => false,
        super::Kind::IronGolem => {
            let v = t.velocity();
            v.x * v.x + v.z * v.z > f64::from(2.500_000_3e-7_f32)
                && t.mob().random.next_int_bounded(5) == 0
        }
        _ => {
            let flags = t.core().flags();
            let fluids = t.mob_ref().fluids;
            flags.contains(EntityFlags::SPRINTING)
                && !flags.contains(EntityFlags::CROUCHING)
                && !fluids.in_water()
                && !fluids.in_lava()
                && t.mob_ref().is_alive()
        }
    };
    if !can {
        return;
    }
    use vanillite_data::block::BlockKind as K;
    let state = t.level.block(on_pos(t, f64::from(0.2_f32)));
    let invisible = matches!(
        state.block().info().kind,
        K::AirBlock
            | K::BarrierBlock
            | K::LiquidBlock
            | K::BubbleColumnBlock
            | K::LightBlock
            | K::EndGatewayBlock
            | K::EndPortalBlock
            | K::StructureVoidBlock
            | K::MovingPistonBlock
    );
    if !invisible {
        let r = &mut t.mob().random;
        let _ = (r.next_double(), r.next_double());
    }
}

/// `getOnPos(offset)` without the supporting block search.
fn on_pos(t: &Tick<'_, '_>, offset: f64) -> BlockPos {
    let p = t.pos();
    BlockPos::new(floor(p.x), floor(p.y - offset), floor(p.z))
}

/// `getBlockPosBelowThatAffectsMyMovement`.
fn pos_below_affecting_movement(t: &Tick<'_, '_>) -> BlockPos {
    on_pos(t, f64::from(0.500_001_f32))
}

/// `computeModifiedFriction`.
pub fn modified_friction(friction: f32, modifier: f32) -> f32 {
    (1.0 - (1.0 - friction) * modifier).clamp(0.0, 1.0)
}

/// `moveRelative(speed, input)`.
pub fn move_relative(t: &mut Tick<'_, '_>, speed: f32, input: Vec3) {
    let length = input.length_sqr();
    if length < 1.0e-7 {
        return;
    }
    let movement = if length > 1.0 {
        input.normalize()
    } else {
        input
    } * f64::from(speed);
    let rad = f64::from(t.core().rot().y_rot * super::DEG_TO_RAD);
    let sin = f64::from(vanillite_base::math::sin(rad));
    let cos = f64::from(vanillite_base::math::cos(rad));
    let delta = Vec3::new(
        movement.x * cos - movement.z * sin,
        movement.y,
        movement.z * cos + movement.x * sin,
    );
    let v = t.velocity();
    t.set_velocity(v + delta);
}

/// `onClimbable`: spiders climbing walls, or a `#climbable` block at the feet.
pub fn on_climbable(t: &Tick<'_, '_>) -> bool {
    if matches!(
        t.mob_ref().kind,
        super::Kind::Spider | super::Kind::CaveSpider
    ) {
        return t.mob_ref().climbing;
    }
    let state = t.level.block(t.block_pos());
    t.env.facts.is_climbable(state.block())
}

/// `LivingEntity.travel`.
fn travel(t: &mut Tick<'_, '_>, input: Vec3) {
    if t.mob_ref().kind.family() != super::Family::Classic && super::fam::travel(t, input) {
        return;
    }
    base_travel(t, input);
}

/// `LivingEntity.travel` without the kind's override (`super.travel(input)`).
pub fn base_travel(t: &mut Tick<'_, '_>, input: Vec3) {
    let fluids = t.mob_ref().fluids;
    // `shouldTravelInFluid`: in a liquid it cannot stand on (striders walk on lava).
    let stands = t.mob_ref().nav.stands_on_lava && {
        let p = t.pos();
        super::is_lava(t.level.block(BlockPos::containing(p.x, p.y, p.z)))
    };
    if (fluids.in_water() || fluids.in_lava()) && !stands {
        travel_in_fluid(t, input);
        if !t.exists() {
            return;
        }
        super::fam::undead::cubes::buoyancy(t);
        float_in_liquid_while_ridden(t);
    } else {
        travel_in_air(t, input);
    }
}

/// `floatInLiquidWhileRidden`: ridden horses and camels (`#can_float_while_ridden`) rise.
fn float_in_liquid_while_ridden(t: &mut Tick<'_, '_>) {
    use super::Kind;
    let floats = matches!(
        t.mob_ref().kind,
        Kind::Horse | Kind::ZombieHorse | Kind::Mule | Kind::Donkey | Kind::Camel | Kind::CamelHusk
    );
    if !floats || t.ents.riding.passengers_of(t.id).is_empty() {
        return;
    }
    if t.mob_ref().fluids.water_height > fluid_jump_threshold(t) {
        let v = t.velocity();
        t.set_velocity(Vec3::new(v.x, v.y + f64::from(0.04_f32), v.z));
    }
}

/// `getEffectiveGravity`: Slow Falling caps it while falling.
pub fn gravity(t: &Tick<'_, '_>) -> f64 {
    let gravity = if t.core().flags().contains(EntityFlags::NO_GRAVITY) {
        0.0
    } else {
        t.mob_ref().attrs.gravity
    };
    let falling = t.velocity().y <= 0.0;
    t.mob_ref().effects.effective_gravity(gravity, falling)
}

fn travel_in_air(t: &mut Tick<'_, '_>, input: Vec3) {
    let below = pos_below_affecting_movement(t);
    #[allow(clippy::cast_possible_truncation)]
    let friction_modifier = t.mob_ref().attrs.friction_modifier as f32;
    let block_friction = if t.on_ground() {
        modified_friction(
            t.level.block(below).block().info().friction,
            friction_modifier,
        )
    } else {
        1.0
    };
    // `handleRelativeFrictionAndCalculateMovement`.
    let speed = if t.on_ground() {
        let s = t.mob_ref().speed;
        if block_friction > 0.6 {
            s * (0.216_000_02 / (block_friction * block_friction * block_friction))
        } else {
            s
        }
    } else {
        0.02
    };
    move_relative(t, speed, input);
    let climbable = on_climbable(t);
    if climbable {
        t.mob().fall_distance = 0.0;
        let v = t.velocity();
        let max = f64::from(0.15_f32);
        t.set_velocity(Vec3::new(
            v.x.clamp(-max, max),
            v.y.max(-max),
            v.z.clamp(-max, max),
        ));
    }
    let v = t.velocity();
    move_self(t, v);
    if !t.exists() {
        return;
    }
    let mut movement = t.velocity();
    // `onClimbable() || wasInPowderSnow && PowderSnowBlock.canEntityWalkOnPowderSnow(this)`.
    let snow_climb = t.mob_ref().was_in_powder_snow && {
        let m = t.mob_ref();
        crate::effect::entity_in_tag(m.kind.type_id(), "minecraft:powder_snow_walkable_mobs")
            || m.equipment.get(crate::item::EquipmentSlot::Feet).item
                == vanillite_data::item::ids::LEATHER_BOOTS.0
    };
    if (t.mob_ref().horizontal_collision || t.mob_ref().jumping) && (climbable || snow_climb) {
        movement.y = 0.2;
    }
    let mut y = movement.y;
    if let Some(a) = t
        .mob_ref()
        .effects
        .amplifier(crate::effect::MobEffect::LEVITATION)
    {
        y += (0.05 * f64::from(a + 1) - movement.y) * 0.2;
    } else if t.level.is_loaded(below) {
        y -= gravity(t);
    } else if t.pos().y > f64::from(t.level.min_y()) {
        y = -0.1;
    } else {
        y = 0.0;
    }
    #[allow(clippy::cast_possible_truncation)]
    let drag_modifier = t.mob_ref().attrs.air_drag_modifier as f32;
    let air_drag = modified_friction(0.91, drag_modifier);
    let friction = block_friction * air_drag;
    let vertical = if omnidirectional_air_mover(t.mob_ref()) {
        air_drag
    } else {
        modified_friction(0.98, drag_modifier)
    };
    if t.mob_ref().discard_friction {
        // `shouldDiscardFriction` (long jumps).
        t.set_velocity(Vec3::new(movement.x, y, movement.z));
        return;
    }
    t.set_velocity(Vec3::new(
        movement.x * f64::from(friction),
        y * f64::from(vertical),
        movement.z * f64::from(friction),
    ));
}

/// `omnidirectionalAirMover`: parrots, bees and sulfur cubes holding a block slow down
/// vertically like horizontally in the air.
fn omnidirectional_air_mover(m: &super::MobData) -> bool {
    match m.kind {
        super::Kind::Parrot | super::Kind::Bee => true,
        super::Kind::SulfurCube => !m.equipment.get(crate::item::EquipmentSlot::Body).is_empty(),
        _ => false,
    }
}

fn travel_in_fluid(t: &mut Tick<'_, '_>, input: Vec3) {
    let falling = t.velocity().y <= 0.0;
    let old_y = t.pos().y;
    let base_gravity = gravity(t);
    if t.mob_ref().fluids.in_water() {
        let mut slow_down = super::fam::water_slow_down(t.mob_ref().kind);
        let mut speed = 0.02_f32;
        #[allow(clippy::cast_possible_truncation)]
        let mut walker = t.mob_ref().attrs.water_movement_efficiency as f32;
        if !t.on_ground() {
            walker *= 0.5;
        }
        if walker > 0.0 {
            slow_down += (0.546_000_06 - slow_down) * walker;
            speed += (t.mob_ref().speed - speed) * walker;
        }
        if t.mob_ref()
            .effects
            .has(crate::effect::MobEffect::DOLPHINS_GRACE)
        {
            slow_down = 0.96;
        }
        move_relative(t, speed, input);
        let v = t.velocity();
        move_self(t, v);
        if !t.exists() {
            return;
        }
        let mut movement = t.velocity();
        if t.mob_ref().horizontal_collision && on_climbable(t) {
            movement.y = 0.2;
        }
        movement = movement.multiply(
            f64::from(slow_down),
            f64::from(0.8_f32),
            f64::from(slow_down),
        );
        t.set_velocity(fluid_falling_adjusted(base_gravity, falling, movement));
    } else {
        move_relative(t, 0.02, input);
        let v = t.velocity();
        move_self(t, v);
        if !t.exists() {
            return;
        }
        let threshold = fluid_jump_threshold(t);
        if t.mob_ref().fluids.lava_height <= threshold {
            let v = t.velocity().multiply(0.5, f64::from(0.8_f32), 0.5);
            t.set_velocity(fluid_falling_adjusted(base_gravity, falling, v));
        } else {
            let v = t.velocity() * 0.5;
            t.set_velocity(v);
        }
        if base_gravity != 0.0 {
            let v = t.velocity();
            t.set_velocity(Vec3::new(v.x, v.y - base_gravity / 4.0, v.z));
        }
    }
    // `jumpOutOfFluid`.
    let v = t.velocity();
    if t.mob_ref().horizontal_collision {
        let probe = t
            .aabb()
            .offset(v.x, v.y + f64::from(0.6_f32) - t.pos().y + old_y, v.z);
        if physics::no_collision(&t.level.view(), &probe, &mut t.cx.boxes)
            && !physics::contains_any_liquid(t.level, &probe)
        {
            t.set_velocity(Vec3::new(v.x, f64::from(0.3_f32), v.z));
        }
    }
}

/// `getFluidFallingAdjustedMovement`.
fn fluid_falling_adjusted(gravity: f64, falling: bool, movement: Vec3) -> Vec3 {
    if gravity == 0.0 {
        return movement;
    }
    let y = if falling
        && (movement.y - 0.005).abs() >= 0.003
        && (movement.y - gravity / 16.0).abs() < 0.003
    {
        -0.003
    } else {
        movement.y - gravity / 16.0
    };
    Vec3::new(movement.x, y, movement.z)
}

/// `Entity.move(MoverType.SELF, delta)`.
pub fn move_self(t: &mut Tick<'_, '_>, delta: Vec3) {
    let mut delta = delta;
    let stuck = t.mob_ref().stuck_speed;
    if stuck.length_sqr() > 1.0e-7 {
        delta = delta.multiply(stuck.x, stuck.y, stuck.z);
        let m = t.mob();
        m.stuck_speed = Vec3::ZERO;
        t.set_velocity(Vec3::ZERO);
    }
    move_and_collide(t, delta);
}

/// `PistonMovingBlockEntity.moveEntityByPiston` of a mob: `move(MoverType.PISTON, delta)` (the
/// stuck speed is dropped, not applied), then `applyEffectsFromBlocks(previousPos, position)`
/// without keeping the move for the mob's own tick (`removeLatestMovementRecording`).
pub fn move_by_piston(t: &mut Tick<'_, '_>, delta: Vec3) {
    if t.mob_ref().stuck_speed.length_sqr() > 1.0e-7 {
        t.mob().stuck_speed = Vec3::ZERO;
        t.set_velocity(Vec3::ZERO);
    }
    let from = t.pos();
    let kept = std::mem::take(&mut t.cx.movements);
    move_and_collide(t, delta);
    if !t.exists() || t.mob_ref().removed {
        t.cx.movements = kept;
        return;
    }
    let to = t.pos();
    t.cx.movements.clear();
    t.cx.movements
        .push(crate::survival::inside::Movement::plain(from, to));
    apply_effects_from_blocks(t);
    t.cx.movements = kept;
}

/// The part of `Entity.move` after the mover's own adjustments: collide, move, the collision
/// flags, fall damage, restitution, step effects and the block speed factor.
fn move_and_collide(t: &mut Tick<'_, '_>, delta: Vec3) {
    #[allow(clippy::cast_possible_truncation)]
    let step = if super::fam::undead::cubes::steps_up(t.mob_ref()) {
        t.mob_ref().attrs.step_height as f32
    } else {
        0.0
    };
    let on_ground = t.on_ground();
    let aabb = t.aabb();
    let on_lava = t.mob_ref().nav.stands_on_lava;
    let snow = snow_collision(t.mob_ref(), aabb);
    let movement = physics::collide_in_context(
        &t.level.view(),
        aabb,
        delta,
        step,
        on_ground,
        &mut t.cx.boxes,
        on_lava,
        snow,
        true,
    );
    let length = movement.length_sqr();
    if length > 1.0e-7 || delta.length_sqr() - length < 1.0e-7 {
        let p = t.pos();
        crate::survival::inside::record(
            &mut t.cx.movements,
            crate::survival::inside::Movement {
                from: p,
                to: p + movement,
                original: Some(delta),
            },
        );
        t.core_mut().set_pos(p + movement);
    }
    let x_collision = !java_equal(delta.x, movement.x);
    let z_collision = !java_equal(delta.z, movement.z);
    let horizontal = x_collision || z_collision;
    t.mob().horizontal_collision = horizontal;
    let moved_vertically = delta.y.abs() > 0.0;
    let mut vertical = t.mob_ref().vertical_collision;
    // `isLocalInstanceAuthoritative`: always, unless a player steers the mob.
    if moved_vertically || !crate::vehicle::player_controlled(t.ents, t.id) {
        vertical = delta.y != movement.y;
        let below = vertical && delta.y < 0.0;
        t.mob().vertical_collision = vertical;
        t.core_mut().set_flags(EntityFlags::ON_GROUND, below);
    }
    let effect_pos = on_pos(t, f64::from(0.2_f32));
    let effect_state = t.level.block(effect_pos);
    check_fall_damage(t, movement.y, t.on_ground(), effect_state, effect_pos);
    if !t.exists() || t.mob_ref().removed {
        return;
    }
    if moved_vertically && vertical || horizontal {
        restitute_after_collisions(
            t,
            effect_state,
            (
                x_collision,
                z_collision,
                vertical,
                vertical && delta.y < 0.0,
            ),
            movement,
        );
    }
    if t.mob_ref().kind != super::Kind::EnderDragon {
        movement_emission(t, movement, effect_pos, effect_state);
    }
    let factor = f64::from(block_speed_factor(t));
    let v = t.velocity();
    t.set_velocity(v.multiply(factor, 1.0, factor));
}

/// `Entity.restituteMovementAfterCollisions`: the colliding axes stop, or bounce back with the
/// mob's `bounciness` attribute; landing fast enough on a slime block, bed or shelf mushroom
/// bounces with the block's restitution. `collisions` is x, z, vertical, vertical below.
fn restitute_after_collisions(
    t: &mut Tick<'_, '_>,
    effect_state: BlockState,
    collisions: (bool, bool, bool, bool),
    movement: Vec3,
) {
    let (x_collision, z_collision, vertical, below) = collisions;
    // Mobs never suppress the bounce (`isShiftKeyDown`).
    let mut restitution = if t.mob_ref().kind == super::Kind::SulfurCube {
        super::fam::undead::cubes::bounciness(t.mob_ref())
    } else {
        entity_bounciness(t.mob_ref().kind)
    };
    let current = t.velocity();
    let mut after = current;
    if x_collision {
        after.x = -current.x * restitution;
    }
    if z_collision {
        after.z = -current.z * restitution;
    }
    let mut bounced = restitution > 0.0 && (x_collision || z_collision);
    if vertical {
        if below {
            let block = effect_state.block();
            restitution =
                if -current.y > gravity(t) && block != vanillite_data::block::ids::HONEY_BLOCK {
                    restitution.max(f64::from(crate::survival::blocks::bounce_restitution(
                        block,
                    )))
                } else {
                    0.0
                };
        }
        let (compensation, drag) = if restitution > 0.0 {
            let portion = movement.y / current.y;
            #[allow(clippy::cast_possible_truncation)]
            let drag_modifier = t.mob_ref().attrs.air_drag_modifier as f32;
            let air_drag = if omnidirectional_air_mover(t.mob_ref()) {
                modified_friction(0.91, drag_modifier)
            } else {
                modified_friction(0.98, drag_modifier)
            };
            bounced = true;
            (
                portion * gravity(t),
                1.0 + portion * (f64::from(air_drag) - 1.0),
            )
        } else {
            (0.0, 1.0)
        };
        after.y = (compensation - current.y) * drag * restitution;
    }
    if bounced {
        // `bounceOn` (the shelf mushroom's sound) and the `bounce` game event.
        if effect_state.block() == vanillite_data::block::ids::SHELF_MUSHROOM {
            let pos = pos_below_affecting_movement(t);
            crate::survival::play_sound(
                t.env.out,
                None,
                Vec3::new(
                    f64::from(pos.x) + 0.5,
                    f64::from(pos.y) + 0.5,
                    f64::from(pos.z) + 0.5,
                ),
                "minecraft:block.shelf_mushroom.bounce",
                vanillite_protocol::packets::play::clientbound::SoundSource::Blocks,
                1.0,
                1.0,
                crate::survival::sound_seed(),
            );
        }
        let pos = t.pos();
        post_game_event(t, crate::level::game_event::events().bounce, pos, None);
    }
    t.set_velocity(after);
}

/// The mob's powder snow collision: falling more than 2.5 blocks lands on it, powder snow
/// walkers (`#powder_snow_walkable_mobs`, leather boots) stand on it.
fn snow_collision(m: &super::MobData, aabb: Aabb) -> physics::SnowCollision {
    use super::Kind;
    if m.fall_distance > 2.5 {
        physics::SnowCollision::Falling
    } else if matches!(
        m.kind,
        Kind::Rabbit | Kind::Endermite | Kind::Silverfish | Kind::Fox
    ) || crate::item::item_id(m.equipment.get(crate::item::EquipmentSlot::Feet))
        == vanillite_data::item::ids::LEATHER_BOOTS
    {
        physics::SnowCollision::Walker(aabb.min_y)
    } else {
        physics::SnowCollision::None
    }
}

/// `LivingEntity.getEntityBounciness`: the kind's `bounciness` attribute.
pub(crate) fn entity_bounciness(kind: super::Kind) -> f64 {
    use vanillite_data::entity::ATTRIBUTES;
    let info = kind.type_id().info();
    let Some(id) = ATTRIBUTES
        .iter()
        .position(|a| a.name == "minecraft:bounciness")
    else {
        return 0.0;
    };
    info.attributes
        .iter()
        .find(|(a, _)| usize::from(*a) == id)
        .map_or(ATTRIBUTES[id].default, |&(_, v)| v)
}

/// Queues `gameEvent(event, pos, Context.of(this mob, affected))`.
pub(crate) fn post_game_event(
    t: &mut Tick<'_, '_>,
    event: u16,
    pos: Vec3,
    affected: Option<BlockState>,
) {
    let context = crate::level::game_event::GameEventContext {
        source: Some(t.id),
        affected,
        owner: None,
    };
    t.cx.actions.push(super::WorldAction::Level(
        super::boss::warden::LevelAction::GameEvent {
            event,
            pos,
            context,
        },
    ));
}

/// `Entity.applyMovementEmissionAndPlaySound`: the step sounds and the `STEP` and `SWIM` game
/// events of a mob's move.
fn movement_emission(
    t: &mut Tick<'_, '_>,
    movement: Vec3,
    effect_pos: BlockPos,
    effect_state: BlockState,
) {
    #[allow(clippy::cast_possible_truncation)]
    let (moved, horizontal) = (
        (movement.length() * f64::from(0.6_f32)) as f32,
        ((movement.x * movement.x + movement.z * movement.z).sqrt() * f64::from(0.6_f32)) as f32,
    );
    let supporting_pos = on_pos(t, 1.0e-5);
    let supporting = t.level.block(supporting_pos);
    let climbing = t.env.facts.is_climbable(supporting.block());
    {
        let m = t.mob();
        m.move_dist += if climbing { moved } else { horizontal };
        m.fly_dist += moved;
    }
    if supporting.is_air() {
        process_flapping(t);
        return;
    }
    if t.mob_ref().move_dist <= t.mob_ref().next_step {
        return;
    }
    let on_ground = t.on_ground();
    let swimming = t
        .core()
        .flags()
        .contains(vanillite_world::entity::EntityFlags::SWIMMING);
    let pos = t.pos();
    let steps = |t: &Tick<'_, '_>, st: BlockState| {
        !st.is_air() && (on_ground || t.env.facts.is_climbable(st.block())) && !swimming
    };
    let only_effect = supporting_pos == effect_pos;
    let mut produced = false;
    if steps(t, effect_state) {
        produced = true;
        if emits_sounds(t.mob_ref().kind) {
            walking_step_sound(t, effect_pos, effect_state);
        }
        if only_effect {
            post_game_event(
                t,
                crate::level::game_event::events().step,
                pos,
                Some(effect_state),
            );
        }
    }
    if !only_effect && steps(t, supporting) {
        produced = true;
        post_game_event(
            t,
            crate::level::game_event::events().step,
            pos,
            Some(supporting),
        );
    }
    let in_water = t.mob_ref().fluids.in_water();
    let m = t.mob();
    if produced || in_water {
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let next = (m.move_dist as i32).saturating_add(1) as f32;
        m.next_step = next;
    }
    if !produced && in_water {
        if emits_sounds(t.mob_ref().kind) {
            water_swim_sound(t);
        }
        post_game_event(t, crate::level::game_event::events().swim, pos, None);
    }
}

/// `Entity.walkingStepSound`: the step sound, then the amethyst chime on
/// `#crystal_sound_blocks` at most once a second.
fn walking_step_sound(t: &mut Tick<'_, '_>, pos: BlockPos, state: BlockState) {
    play_step_sound(t, pos, state);
    let m = t.mob_ref();
    if m.tick_count >= m.last_crystal_tick.saturating_add(20)
        && super::spawn::in_block_tag(state, "minecraft:crystal_sound_blocks")
    {
        // `playAmethystStepSound`.
        let m = t.mob();
        #[allow(clippy::cast_possible_truncation)]
        let decay = 0.997_f64.powi(m.tick_count - m.last_crystal_tick) as f32;
        m.crystal_intensity = (m.crystal_intensity * decay + 0.07).min(1.0);
        let pitch = 0.5 + m.crystal_intensity * m.random.next_float() * 1.2;
        let volume = 0.1 + m.crystal_intensity * 1.2;
        m.last_crystal_tick = m.tick_count;
        kinds::play_sound(t, "minecraft:block.amethyst_block.chime", volume, pitch);
    }
}

/// `Entity.playStepSound` (the block's step sound) and the kinds' overrides.
fn play_step_sound(t: &mut Tick<'_, '_>, pos: BlockPos, state: BlockState) {
    use super::Kind;
    let m = t.mob_ref();
    let baby = m.baby;
    let (sound, volume) = match m.kind {
        Kind::Cod
        | Kind::Salmon
        | Kind::Pufferfish
        | Kind::TropicalFish
        | Kind::Tadpole
        | Kind::HappyGhast
        | Kind::Nautilus
        | Kind::ZombieNautilus
        | Kind::Allay
        | Kind::Bee => return,
        Kind::Armadillo => ("minecraft:entity.armadillo.step", 0.15),
        Kind::Pig if baby => ("minecraft:entity.baby_pig.step", 0.15),
        Kind::Pig => ("minecraft:entity.pig.step", 0.15),
        Kind::Chicken if baby => ("minecraft:entity.baby_chicken.step", 0.15),
        Kind::Chicken => ("minecraft:entity.chicken.step", 0.15),
        Kind::Wolf if baby => ("minecraft:entity.baby_wolf.step", 0.15),
        Kind::Wolf => ("minecraft:entity.wolf.step", 0.15),
        Kind::Cow
            if kinds::sound_variant_names(Kind::Cow).get(usize::from(m.sound_variant))
                == Some(&"moody") =>
        {
            ("minecraft:entity.cow_moody.step", 0.15)
        }
        Kind::Cow | Kind::Mooshroom => ("minecraft:entity.cow.step", 0.15),
        Kind::Parrot => ("minecraft:entity.parrot.step", 0.15),
        Kind::Camel | Kind::CamelHusk => {
            let sand = super::spawn::in_block_tag(state, "minecraft:camel_sand_step_sound_blocks");
            match (m.kind == Kind::Camel, sand) {
                (true, true) => ("minecraft:entity.camel.step_sand", 1.0),
                (true, false) => ("minecraft:entity.camel.step", 1.0),
                (false, true) => ("minecraft:entity.camel_husk.step_sand", 0.4),
                (false, false) => ("minecraft:entity.camel_husk.step", 0.4),
            }
        }
        Kind::Sniffer => ("minecraft:entity.sniffer.step", 0.15),
        Kind::Strider if m.fluids.in_lava() => ("minecraft:entity.strider.step_lava", 1.0),
        Kind::Strider => ("minecraft:entity.strider.step", 1.0),
        Kind::Piglin => ("minecraft:entity.piglin.step", 0.15),
        Kind::PiglinBrute => ("minecraft:entity.piglin_brute.step", 0.15),
        Kind::IronGolem => ("minecraft:entity.iron_golem.step", 1.0),
        Kind::Sheep => ("minecraft:entity.sheep.step", 0.15),
        Kind::CopperGolem => {
            use super::fam::mount::copper_golem::{Weather, golem};
            match golem(m).weather {
                Weather::Weathered => ("minecraft:entity.copper_golem_weathered.step", 1.0),
                Weather::Oxidized => ("minecraft:entity.copper_golem_oxidized.step", 1.0),
                _ => ("minecraft:entity.copper_golem.step", 1.0),
            }
        }
        Kind::Frog => ("minecraft:entity.frog.step", 0.15),
        Kind::Goat => ("minecraft:entity.goat.step", 0.15),
        Kind::Zombie | Kind::ZombifiedPiglin | Kind::Giant => {
            ("minecraft:entity.zombie.step", 0.15)
        }
        Kind::Husk => ("minecraft:entity.husk.step", 0.15),
        Kind::Drowned => ("minecraft:entity.drowned.step", 0.15),
        Kind::ZombieVillager => ("minecraft:entity.zombie_villager.step", 0.15),
        Kind::Skeleton => ("minecraft:entity.skeleton.step", 0.15),
        Kind::WitherSkeleton => ("minecraft:entity.wither_skeleton.step", 0.15),
        Kind::Stray => ("minecraft:entity.stray.step", 0.15),
        Kind::Bogged => ("minecraft:entity.bogged.step", 0.15),
        Kind::Parched => ("minecraft:entity.parched.step", 0.15),
        Kind::Turtle if baby => ("minecraft:entity.turtle.shamble_baby", 0.15),
        Kind::Turtle => ("minecraft:entity.turtle.shamble", 0.15),
        Kind::Endermite => ("minecraft:entity.endermite.step", 0.15),
        Kind::Silverfish => ("minecraft:entity.silverfish.step", 0.15),
        Kind::Ravager => ("minecraft:entity.ravager.step", 0.15),
        Kind::Zoglin => ("minecraft:entity.zoglin.step", 0.15),
        Kind::Hoglin => ("minecraft:entity.hoglin.step", 0.15),
        Kind::Creaking => ("minecraft:entity.creaking.step", 0.15),
        Kind::Spider | Kind::CaveSpider => ("minecraft:entity.spider.step", 0.15),
        Kind::Warden => ("minecraft:entity.warden.step", 10.0),
        Kind::Llama | Kind::TraderLlama => ("minecraft:entity.llama.step", 0.15),
        Kind::PolarBear => ("minecraft:entity.polar_bear.step", 0.15),
        Kind::Panda => ("minecraft:entity.panda.step", 0.15),
        Kind::Horse | Kind::Donkey | Kind::Mule | Kind::SkeletonHorse | Kind::ZombieHorse => {
            horse_step_sound(t, pos, state);
            return;
        }
        Kind::SulfurCube if super::fam::undead::cubes::has_body_item(m) => return,
        _ => {
            let st = state.block().sound();
            let id = vanillite_protocol::packets::play::clientbound::SoundEvent(i32::from(
                st.step_sound,
            ));
            play_sound_event(t, id, st.volume * 0.15, st.pitch);
            return;
        }
    };
    kinds::play_sound(t, sound, volume, 1.0);
}

/// `AbstractHorse.playStepSound`: the block's sound type (snow on top wins), galloping under a
/// rider (`Horse.playGallopSound` breathes one time in ten), wood or the horse's own step.
fn horse_step_sound(t: &mut Tick<'_, '_>, pos: BlockPos, state: BlockState) {
    use super::Kind;
    use vanillite_data::block::ids;
    // `BlockState.liquid()`: water and lava blocks.
    if state.block() == ids::WATER || state.block() == ids::LAVA {
        return;
    }
    let above = t.level.block(BlockPos::new(pos.x, pos.y + 1, pos.z));
    let st = if above.block() == ids::SNOW {
        above.block().sound()
    } else {
        state.block().sound()
    };
    let sound_id = |name: &str| crate::survival::sound_event(name);
    let kind = t.mob_ref().kind;
    let gallops = matches!(kind, Kind::Horse | Kind::SkeletonHorse | Kind::ZombieHorse);
    let wood = [
        "minecraft:block.wood.step",
        "minecraft:block.nether_wood.step",
        "minecraft:block.stem.step",
        "minecraft:block.cherry_wood.step",
        "minecraft:block.bamboo_wood.step",
    ]
    .iter()
    .any(|n| crate::survival::sound_event(n).0 == i32::from(st.step_sound));
    if gallops && !t.ents.riding.passengers_of(t.id).is_empty() {
        let h = crate::mob::fam::companion::st_mut(t.mob());
        h.gallop_counter += 1;
        let counter = h.gallop_counter;
        if counter > 5 && counter % 3 == 0 {
            play_sound_event(
                t,
                sound_id("minecraft:entity.horse.gallop"),
                st.volume * 0.15,
                st.pitch,
            );
            if kind == Kind::Horse && t.mob().random.next_int_bounded(10) == 0 {
                let breathe = if t.mob_ref().baby {
                    "minecraft:entity.baby_horse.breathe"
                } else {
                    "minecraft:entity.horse.breathe"
                };
                play_sound_event(t, sound_id(breathe), st.volume * 0.6, st.pitch);
            }
        } else if counter <= 5 {
            play_sound_event(
                t,
                sound_id("minecraft:entity.horse.step_wood"),
                st.volume * 0.15,
                st.pitch,
            );
        }
    } else if wood {
        play_sound_event(
            t,
            sound_id("minecraft:entity.horse.step_wood"),
            st.volume * 0.15,
            st.pitch,
        );
    } else {
        let step = if t.mob_ref().baby {
            "minecraft:entity.baby_horse.step"
        } else {
            "minecraft:entity.horse.step"
        };
        play_sound_event(t, sound_id(step), st.volume * 0.15, st.pitch);
    }
}

/// `Entity.playSound` of a sound event id.
fn play_sound_event(
    t: &mut Tick<'_, '_>,
    sound: vanillite_protocol::packets::play::clientbound::SoundEvent,
    volume: f32,
    pitch: f32,
) {
    if t.mob_ref().silent {
        return;
    }
    let pos = t.pos();
    let source = t.mob_ref().sound_source();
    let seed = crate::survival::sound_seed();
    crate::survival::play_sound_event(t.env.out, None, pos, sound, source, volume, pitch, seed);
}

/// `Entity.processFlappingMovement`: bees flap every other tick in the air, chickens and
/// parrots whenever their flight passes `nextFlap`, allays in the air, bats and phantoms on
/// their beat; each flap is a `flap` game event.
fn process_flapping(t: &mut Tick<'_, '_>) {
    use super::Kind;
    let kind = t.mob_ref().kind;
    let flapping = match kind {
        // `Bee.isFlapping`: `TICKS_PER_FLAP` is 2.
        Kind::Bee => !t.on_ground() && t.mob_ref().tick_count % 2 == 0,
        Kind::Chicken | Kind::Parrot => t.mob_ref().fly_dist > t.mob_ref().next_flap,
        Kind::Allay => !t.on_ground(),
        Kind::Bat => {
            !super::fam::wildlife::st(t.mob_ref()).resting && t.mob_ref().tick_count % 10 == 0
        }
        // `getUniqueFlapTickOffset` is the entity id times 3; `TICKS_PER_FLAP` is 25.
        Kind::Phantom => {
            let id = t.core().network_id().0;
            id.wrapping_mul(3).wrapping_add(t.mob_ref().tick_count) % 25 == 0
        }
        // Vexes move without physics: their moves make no events.
        _ => false,
    };
    if !flapping {
        return;
    }
    match kind {
        Kind::Chicken => {
            let m = t.mob();
            m.next_flap = m.fly_dist + super::fam::classic::chicken(m).flap_speed / 2.0;
        }
        Kind::Parrot => {
            kinds::play_sound(t, "minecraft:entity.parrot.fly", 0.15, 1.0);
            let m = t.mob();
            m.next_flap = m.fly_dist + super::fam::companion::st(m).flap / 2.0;
        }
        _ => {}
    }
    let pos = t.pos();
    post_game_event(t, crate::level::game_event::events().flap, pos, None);
}

/// `getMovementEmission().emitsSounds()`: kinds whose moves make no sound.
fn emits_sounds(kind: super::Kind) -> bool {
    use super::Kind;
    !matches!(
        kind,
        Kind::Silverfish
            | Kind::Breeze
            | Kind::Squid
            | Kind::GlowSquid
            | Kind::Bat
            | Kind::Guardian
            | Kind::ElderGuardian
            | Kind::Endermite
            | Kind::Shulker
    )
}

/// `Entity.waterSwimSound` → `playSwimSound(volume)`: the swim sound of a move in water.
fn water_swim_sound(t: &mut Tick<'_, '_>) {
    use super::Kind;
    let v = t.velocity();
    let point_two = f64::from(0.2_f32);
    #[allow(clippy::cast_possible_truncation)]
    let speed =
        ((v.x * v.x * point_two + v.y * v.y + v.z * v.z * point_two).sqrt() as f32 * 0.35).min(1.0);
    let kind = t.mob_ref().kind;
    let volume = match kind {
        Kind::Turtle => speed * 1.5,
        Kind::SkeletonHorse if t.on_ground() => 0.3,
        Kind::SkeletonHorse => (speed * 25.0).min(0.1),
        _ => speed,
    };
    let baby = t.mob_ref().baby;
    let sound = match kind {
        Kind::Cod | Kind::Salmon | Kind::Pufferfish | Kind::TropicalFish | Kind::Tadpole => {
            "minecraft:entity.fish.swim"
        }
        Kind::Axolotl => "minecraft:entity.axolotl.swim",
        Kind::Drowned => "minecraft:entity.drowned.swim",
        Kind::Dolphin => "minecraft:entity.dolphin.swim",
        Kind::Turtle => "minecraft:entity.turtle.swim",
        Kind::SkeletonHorse => "minecraft:entity.skeleton_horse.swim",
        Kind::Nautilus if baby => "minecraft:entity.baby_nautilus.swim",
        Kind::Nautilus => "minecraft:entity.nautilus.swim",
        Kind::ZombieNautilus => "minecraft:entity.zombie_nautilus.swim",
        _ if hostile_swimmer(kind) => "minecraft:entity.hostile.swim",
        _ => "minecraft:entity.generic.swim",
    };
    let r = &mut t.mob().random;
    let pitch = 1.0 + (r.next_float() - r.next_float()) * 0.4;
    super::kinds::play_sound(t, sound, volume, pitch);
}

/// `Mth.equal(double, double)`.
fn java_equal(a: f64, b: f64) -> bool {
    (b - a).abs() < 1.0e-5
}

/// `LivingEntity.checkFallDamage` → `Entity.checkFallDamage` → `Block.fallOn`.
fn check_fall_damage(
    t: &mut Tick<'_, '_>,
    ya: f64,
    on_ground: bool,
    on_state: BlockState,
    on_pos: BlockPos,
) {
    if !t.mob_ref().fluids.in_water() {
        update_fluids(t);
    }
    if on_ground {
        let distance = t.mob_ref().fall_distance;
        if distance > 0.0 {
            fall_particles(t, distance, on_state, on_pos);
        }
    }
    if !t.mob_ref().fluids.in_water() && ya < 0.0 {
        // `fallDistance -= (float) ya`.
        #[allow(clippy::cast_possible_truncation)]
        let ya = f64::from(ya as f32);
        t.mob().fall_distance -= ya;
    }
    if on_ground {
        let distance = t.mob_ref().fall_distance;
        if distance > 0.0 {
            // `TurtleEggBlock.fallOn`: zombies land on eggs without breaking them.
            if on_state.block() == vanillite_data::block::ids::TURTLE_EGG
                && !crate::mob::fam::undead::zombies::is_zombie(t.mob_ref().kind)
            {
                trample_turtle_egg(t, 3);
            }
            let pos = t.pos();
            post_game_event(
                t,
                crate::level::game_event::events().hit_ground,
                pos,
                Some(on_state),
            );
            match t.env.facts.landing(on_state) {
                Landing::Fall(m) => {
                    cause_fall_damage(t, distance, m);
                }
                // `SlimeBlock.fallOn`: mobs never suppress the bounce.
                Landing::Slime => {
                    cause_fall_damage(t, distance, 0.0);
                }
                Landing::Honey => {
                    // `HoneyBlock.fallOn`.
                    kinds::play_sound(t, "minecraft:block.honey_block.slide", 1.0, 1.0);
                    let id = t.id;
                    entity_event(t, id, 54);
                    if cause_fall_damage(t, distance, 0.2) && t.exists() {
                        kinds::play_sound(t, "minecraft:block.honey_block.fall", 0.5, 0.75);
                    }
                }
                Landing::Stalagmite => {
                    cause_fall_damage(t, distance + 2.5, 2.0);
                }
                Landing::Bed => {
                    cause_fall_damage(t, distance * f64::from(1.0_f32 - 0.5), 1.0);
                }
                Landing::Farmland => {
                    // `FarmlandBlock.fallOn`: the level random decides, mob griefing allows.
                    if f64::from(t.env.random.next_float()) < distance - 0.5
                        && t.env.rules.bool(Rule::MOB_GRIEFING)
                    {
                        let dims = t.core().dims();
                        if crate::block::contact::can_trample(dims.width, dims.height) {
                            let id = t.id;
                            t.cx.actions
                                .push(super::WorldAction::TrampleFarmland(on_pos, id));
                        }
                    }
                    cause_fall_damage(t, distance, 1.0);
                }
                Landing::PowderSnow => {
                    // `PowderSnowBlock.fallOn`: no damage, only the fall sound.
                    if distance >= 4.0 {
                        let sound = if distance < 7.0 {
                            t.mob_ref().kind.fall_sound_small()
                        } else {
                            t.mob_ref().kind.fall_sound_big()
                        };
                        kinds::play_sound(t, sound, 1.0, 1.0);
                    }
                }
            }
        }
        if t.exists() {
            t.mob().fall_distance = 0.0;
        }
    }
}

/// `LivingEntity.checkFallDamage`: the landing dust of a fall of at least one damaging
/// block, more for higher falls.
fn fall_particles(t: &mut Tick<'_, '_>, distance: f64, on_state: BlockState, on_pos: BlockPos) {
    let power = floor(distance + 1.0e-6 - t.mob_ref().attrs.safe_fall_distance).max(0);
    if power <= 0 || on_state.is_air() {
        return;
    }
    let p = t.pos();
    let (mut x, mut z) = (p.x, p.z);
    let here = t.block_pos();
    if on_pos.x != here.x || on_pos.z != here.z {
        let x_diff = x - f64::from(on_pos.x) - 0.5;
        let z_diff = z - f64::from(on_pos.z) - 0.5;
        let max = x_diff.abs().max(z_diff.abs());
        x = f64::from(on_pos.x) + 0.5 + x_diff / max * 0.5;
        z = f64::from(on_pos.z) + 0.5 + z_diff / max * 0.5;
    }
    let scale = (f64::from(0.2_f32) + f64::from(power) / 15.0).min(2.5);
    #[allow(clippy::cast_possible_truncation)]
    let count = (150.0 * scale) as i32;
    let mut particle = crate::level::particles::simple("minecraft:block");
    vanillite_protocol::codec::write_varint(&mut particle.options, i32::from(on_state.0));
    t.cx.send_particles(
        particle,
        Vec3::new(x, p.y, z),
        count,
        Vec3::ZERO,
        f64::from(0.15_f32),
    );
}

/// `LivingEntity.causeFallDamage`; returns whether the mob took damage.
fn cause_fall_damage(t: &mut Tick<'_, '_>, distance: f64, multiplier: f32) -> bool {
    if t.mob_ref().kind.fall_damage_immune() {
        return false;
    }
    // `AbstractHorse.causeFallDamage` passes the fall on only after taking damage itself;
    // `Entity.causeFallDamage` first.
    let kind = t.mob_ref().kind;
    let horse = crate::mob::fam::companion::is_horse(kind)
        || matches!(kind, super::Kind::Camel | super::Kind::CamelHusk);
    if !horse {
        propagate_fall(t, distance, multiplier);
    }
    let attrs = t.mob_ref().attrs;
    let power = distance + 1.0e-6 - attrs.safe_fall_distance;
    let damage = floor(power * f64::from(multiplier) * attrs.fall_damage_multiplier);
    if damage > 0 {
        let sound = if damage > 4 {
            t.mob_ref().kind.fall_sound_big()
        } else {
            t.mob_ref().kind.fall_sound_small()
        };
        kinds::play_sound(t, sound, 1.0, 1.0);
        play_block_fall_sound(t);
        let source = DamageSource::of(t.env.types.fall);
        #[allow(clippy::cast_precision_loss)]
        let hurt = hurt_self(t, source, damage as f32);
        if horse && t.exists() {
            propagate_fall(t, distance, multiplier);
        }
        kinds::on_fall_damage(t, distance);
        return hurt;
    }
    kinds::on_fall_damage(t, distance);
    false
}

/// `Entity.propagateFallToPassengers`: every passenger takes the fall too.
fn propagate_fall(t: &mut Tick<'_, '_>, distance: f64, multiplier: f32) {
    let passengers = t.ents.riding.passengers_of(t.id).to_vec();
    for p in passengers {
        if t.ents.mob(p).is_some() {
            let mut pt = Tick {
                level: t.level,
                ents: &mut *t.ents,
                env: &mut *t.env,
                cx: &mut *t.cx,
                id: p,
            };
            cause_fall_damage(&mut pt, distance, multiplier);
        } else if t.ents.player(p).is_some() {
            let kind = t.env.types.fall;
            crate::survival::player::passenger_fall(t.ents, t.env, p, distance, multiplier, kind);
        }
    }
}

/// `LivingEntity.playBlockFallSound`: the fall sound of the block 0.2 below the feet.
fn play_block_fall_sound(t: &mut Tick<'_, '_>) {
    let p = t.pos();
    let state = t.level.block(BlockPos::new(
        floor(p.x),
        floor(p.y - f64::from(0.2_f32)),
        floor(p.z),
    ));
    if state.is_air() {
        return;
    }
    let st = state.block().sound();
    let sound =
        vanillite_protocol::packets::play::clientbound::SoundEvent(i32::from(st.fall_sound));
    play_sound_event(t, sound, st.volume * 0.5, st.pitch * 0.75);
}

/// Queues `TurtleEggBlock.destroyEgg` for the eggs the mob stands on.
fn trample_turtle_egg(t: &mut Tick<'_, '_>, randomness: i32) {
    let pos = on_pos(t, f64::from(0.2_f32));
    // `canDestroyEgg`: living entities but turtles and bats, with `mob_griefing`.
    let can_destroy = !matches!(t.mob_ref().kind, super::Kind::Turtle | super::Kind::Bat)
        && t.env.rules.bool(Rule::MOB_GRIEFING);
    t.cx.actions.push(super::WorldAction::TrampleTurtleEgg {
        pos,
        can_destroy,
        randomness,
    });
}

/// `applyEffectsFromBlocks` / `checkInsideBlocks`: `stepOn` of the block below, the contact
/// blocks (cactus, berry bushes, campfires, wither roses, powder snow, honey, crops under
/// ravagers), fire, lava, cobwebs and bubble columns.
fn apply_effects_from_blocks(t: &mut Tick<'_, '_>) {
    use vanillite_data::block::ids;
    // `stepOn` of the block below (mobs are never careful).
    if t.on_ground() {
        let on = on_pos(t, f64::from(0.2_f32));
        let block = t.level.block(on).block();
        if block == ids::TURTLE_EGG {
            trample_turtle_egg(t, 100);
        } else if crate::block::contact::has_step_on(block) {
            t.cx.actions.push(super::WorldAction::StepOn(on, t.id));
        } else if block == ids::MAGMA_BLOCK {
            // `MagmaBlock.stepOn`.
            let source = DamageSource::of(t.env.types.hot_floor);
            hurt_self(t, source, 1.0);
        } else if block == ids::SLIME_BLOCK {
            // `SlimeBlock.stepOn`.
            let v = t.velocity();
            if v.y.abs() < 0.1 {
                let scale = 0.4 + v.y.abs() * 0.2;
                t.set_velocity(Vec3::new(v.x * scale, v.y, v.z * scale));
            }
        }
    }
    if !t.exists() {
        return;
    }
    let was_freezing = t.mob_ref().ticks_frozen > 0;
    let kind = t.mob_ref().kind;
    // The moves of this tick, then the way to where the mob is now; a mob that did not
    // move: where it stands, for a passenger the way its vehicle took it since the tick
    // started (`applyEffectsFromBlocks`; a teleport starts anew, `snapTo`).
    let mut path = std::mem::take(&mut t.cx.movements);
    let pos = t.pos();
    let old_pos = if t.ents.riding.vehicle_of(t.id).is_some() {
        t.core().old_pos()
    } else {
        pos
    };
    crate::survival::inside::finish(&mut path, old_pos, pos);
    let feet_in_snow = t.level.block(t.block_pos()).block() == ids::POWDER_SNOW;
    let ravager_griefs = kind == super::Kind::Ravager && t.env.rules.bool(Rule::MOB_GRIEFING);
    let mut stuck = None;
    let mut honey = [BlockPos::new(0, 0, 0); 8];
    let mut honey_count = 0;
    let mut bubbles = [(BlockPos::new(0, 0, 0), false); 4];
    let mut bubble_count = 0;
    let mut eyeblossom = false;
    let dims = t.core().dims();
    let fast_fall = t.mob_ref().fall_distance > 2.5;
    // `WebBlock.entityInside`: Weaving halves the slowdown.
    let web = if t.mob_ref().effects.has(crate::effect::MobEffect::WEAVING) {
        Vec3::new(0.5, 0.25, 0.5)
    } else {
        Vec3::new(0.25, f64::from(0.05_f32), 0.25)
    };
    let (id, actions, level) = (t.id, &mut t.cx.actions, t.level);
    // `Spider.makeStuckInBlock`: spiders and cave spiders pass cobwebs.
    let spider = matches!(kind, super::Kind::Spider | super::Kind::CaveSpider);
    let inside = inside_blocks(t.env.blocks, t.env.facts, dims, fast_fall, &path, |cell| {
        // `entityInside` of the blocks that slow, carry or react to the mob, in the
        // order it meets them.
        let (pos, block) = (cell.pos, cell.state.block());
        if level.has_entity_inside(cell.state) {
            // The blocks with a behaviour of their own: pressure plates, tripwire, buttons,
            // detector rails, big dripleaves, lily pads.
            actions.push(super::WorldAction::EntityInside(pos, id));
        }
        if block == ids::COBWEB {
            if !spider {
                stuck = Some(web);
            }
        } else if block == ids::SWEET_BERRY_BUSH {
            if !matches!(kind, super::Kind::Fox | super::Kind::Bee) {
                stuck = Some(Vec3::new(f64::from(0.8_f32), 0.75, f64::from(0.8_f32)));
            }
        } else if block == ids::POWDER_SNOW {
            if feet_in_snow {
                stuck = Some(Vec3::new(f64::from(0.9_f32), 1.5, f64::from(0.9_f32)));
            }
        } else if block == ids::HONEY_BLOCK {
            if let Some(slot) = honey.get_mut(honey_count) {
                *slot = pos;
                honey_count += 1;
            }
        } else if block == ids::OPEN_EYEBLOSSOM {
            // `EyeblossomBlock.entityInside` (in `#bee_attractive`).
            eyeblossom |= kind == super::Kind::Bee;
        } else if block == ids::BUBBLE_COLUMN {
            // `BubbleColumnBlock.entityInside`: only where the mob ends up.
            if cell.precise
                && let Some(slot) = bubbles.get_mut(bubble_count)
            {
                *slot = (pos, cell.state.value("drag") == Some("true"));
                bubble_count += 1;
            }
        } else if ravager_griefs
            && matches!(
                vanillite_worldgen::behaviour::class_of_block(block),
                vanillite_worldgen::behaviour::Class::Crop
                    | vanillite_worldgen::behaviour::Class::PitcherCrop
            )
        {
            // `CropBlock.entityInside` / `PitcherCropBlock.entityInside`.
            actions.push(super::WorldAction::DestroyBlockBy(pos, true, id));
        }
    });
    path.clear();
    t.cx.movements = path;
    // The `entityInside` effects that act at once.
    if inside.cactus {
        let source = DamageSource::of(t.env.types.cactus);
        hurt_self(t, source, 1.0);
    }
    if let Some(grown) = inside.berry_bush
        && grown
        && !matches!(kind, super::Kind::Fox | super::Kind::Bee)
        && t.exists()
    {
        // `SweetBerryBushBlock.entityInside`: the movement since the tick started.
        let moved = t.core().old_pos() - t.pos();
        if moved.x * moved.x + moved.z * moved.z > 0.0
            && (moved.x.abs() >= f64::from(0.003_f32) || moved.z.abs() >= f64::from(0.003_f32))
        {
            let source = DamageSource::of(t.env.types.sweet_berry_bush);
            hurt_self(t, source, 1.0);
        }
    }
    if let Some(damage) = inside.campfire
        && t.exists()
    {
        let source = DamageSource::of(t.env.types.campfire);
        hurt_self(t, source, damage);
    }
    if inside.wither_rose
        && t.exists()
        && t.env.difficulty != crate::survival::rules::Difficulty::Peaceful
    {
        // `WitherRoseBlock.entityInside`.
        let effect = crate::effect::EffectInstance::new(crate::effect::MobEffect::WITHER, 40, 0);
        let type_id = kind.type_id();
        crate::effect::consume::add_effect(
            type_id,
            effect,
            &mut crate::effect::living::MobHost { t },
        );
    }
    if eyeblossom
        && t.exists()
        && t.env.difficulty != crate::survival::rules::Difficulty::Peaceful
        && !t.mob_ref().effects.has(crate::effect::MobEffect::POISON)
    {
        // `EyeblossomBlock.entityInside`: a bee in an open eyeblossom is poisoned.
        let effect = crate::effect::EffectInstance::new(crate::effect::MobEffect::POISON, 25, 0);
        let type_id = kind.type_id();
        crate::effect::consume::add_effect(
            type_id,
            effect,
            &mut crate::effect::living::MobHost { t },
        );
    }
    if !t.exists() {
        return;
    }
    if let Some(snow) = inside.powder_snow {
        // `PowderSnowBlock.entityInside`: the snowflakes of a moving entity.
        let v = t.velocity();
        if (v.x != 0.0 || v.z != 0.0) && t.env.random.next_boolean() {
            let r = &mut *t.env.random;
            let vx = f64::from((r.next_float() * 2.0 - 1.0) * 0.083_333_336);
            let vz = f64::from((r.next_float() * 2.0 - 1.0) * 0.083_333_336);
            let p = t.pos();
            t.cx.send_particles(
                crate::level::particles::simple("minecraft:snowflake"),
                Vec3::new(p.x, f64::from(snow.y + 1), p.z),
                0,
                Vec3::new(vx, f64::from(0.05_f32), vz),
                1.0,
            );
        }
    }
    let fire_immune = t.core().kind().info().fire_immune;
    let previous_fire = t.mob_ref().fire_ticks;
    // `wasOnFire = isOnFire()`.
    let was_on_fire = !fire_immune && previous_fire > 0;
    // The collected effects, step by step (`StepBasedCollector.applyAndClear`).
    for step in inside.steps() {
        if step.freeze {
            // `InsideBlockEffectType.FREEZE`.
            t.mob().in_powder_snow = true;
            if can_freeze(t.mob_ref()) {
                let m = t.mob();
                m.ticks_frozen = (m.ticks_frozen + 1).min(TICKS_TO_FREEZE);
                t.core_mut().mark_data_dirty();
            }
        }
        if step.clear_freeze && t.mob_ref().ticks_frozen != 0 {
            // `InsideBlockEffectType.CLEAR_FREEZE`.
            t.mob().ticks_frozen = 0;
            t.core_mut().mark_data_dirty();
        }
        if let Some(damage) = step.fire
            && !fire_immune
        {
            // `BaseFireBlock.fireIgnite`.
            let m = t.mob();
            if m.fire_ticks < 0 {
                m.fire_ticks += 1;
            }
            if m.fire_ticks >= 0 && super::attack::ignite_mob(m, 160) {
                t.core_mut().mark_data_dirty();
            }
            let source = DamageSource::of(t.env.types.in_fire);
            hurt_self(t, source, damage);
        }
        if !t.exists() {
            return;
        }
        if step.lava_blocks > 0 && !fire_immune {
            // `lavaIgnite` and `lavaHurt`.
            if super::attack::ignite_mob(t.mob(), 300) {
                t.core_mut().mark_data_dirty();
            }
            let source = DamageSource::of(t.env.types.lava);
            if hurt_self(t, source, 4.0) {
                kinds::play_sound(t, "minecraft:entity.generic.burn", 0.4, 2.0);
            }
        }
        if !t.exists() {
            return;
        }
        if let Some(snow) = step.melt_snow
            && t.mob_ref().fire_ticks > 0
            && t.env.rules.bool(Rule::MOB_GRIEFING)
        {
            // `PowderSnowBlock.entityInside`'s `runBefore(EXTINGUISH)`: a burning mob melts it.
            t.cx.actions
                .push(super::WorldAction::DestroyBlock(snow, false));
        }
        if let Some(cauldron) = step.cauldron
            && t.mob_ref().fire_ticks > 0
        {
            t.cx.actions
                .push(super::WorldAction::BurningInCauldron(cauldron));
        }
        let m = t.mob();
        if step.extinguish && m.fire_ticks > 0 {
            // `clearFire`.
            m.fire_ticks = 0;
        }
    }
    // `isInRain`: the feet's block or the column at the top of the box.
    let p = t.pos();
    let feet = BlockPos::containing(p.x, p.y, p.z);
    let in_rain = (t.env.raining_at)(feet)
        || (t.env.raining_at)(BlockPos::containing(
            f64::from(feet.x),
            t.aabb().max_y,
            f64::from(feet.z),
        ));
    let m = t.mob();
    if in_rain && m.fire_ticks > 0 {
        // `clearFire`.
        m.fire_ticks = 0;
    }
    // Not burning and not ignited this tick: `-getFireImmuneTicks()`, 0 for mobs.
    if !(m.fire_ticks > 0 && !fire_immune) && m.fire_ticks <= previous_fire {
        m.fire_ticks = 0;
    }
    // `playEntityOnFireExtinguishedSound`: the fire went out or the freezing ended.
    if (was_on_fire && m.fire_ticks <= 0) || (was_freezing && m.ticks_frozen <= 0) {
        let r = &mut m.random;
        let pitch = 1.6 + (r.next_float() - r.next_float()) * 0.4;
        kinds::play_sound(t, "minecraft:entity.generic.extinguish_fire", 0.7, pitch);
    }
    // Cobwebs, sweet berry bushes and powder snow slow the next move (`makeStuckInBlock`);
    // `WitherBoss.makeStuckInBlock` does nothing.
    if let Some(s) = stuck
        && kind != super::Kind::Wither
    {
        t.mob().fall_distance = 0.0;
        t.mob().stuck_speed = s;
    }
    // `HoneyBlock.entityInside` of each honey block touched.
    for pos in honey.into_iter().take(honey_count) {
        honey_slide(t, pos);
    }
    for (pos, drag_down) in bubbles.into_iter().take(bubble_count) {
        bubble_column(t, pos, drag_down);
    }
}

/// `Entity.getTicksRequiredToFreeze`.
pub const TICKS_TO_FREEZE: i32 = 140;

/// `LivingEntity.canFreeze`: not a `#freeze_immune_entity_types` kind and no
/// `#freeze_immune_wearables` (leather) armor.
fn can_freeze(m: &super::MobData) -> bool {
    use crate::item::EquipmentSlot;
    use vanillite_data::item::ids;
    if matches!(
        m.kind,
        super::Kind::Stray | super::Kind::PolarBear | super::Kind::SnowGolem | super::Kind::Wither
    ) || (m.kind == super::Kind::SulfurCube && super::fam::undead::cubes::has_body_item(m))
    {
        return false;
    }
    let leather = [
        ids::LEATHER_BOOTS,
        ids::LEATHER_LEGGINGS,
        ids::LEATHER_CHESTPLATE,
        ids::LEATHER_HELMET,
        ids::LEATHER_HORSE_ARMOR,
    ];
    ![
        EquipmentSlot::Feet,
        EquipmentSlot::Legs,
        EquipmentSlot::Chest,
        EquipmentSlot::Head,
        EquipmentSlot::Body,
    ]
    .into_iter()
    .any(|slot| leather.contains(&crate::item::item_id(m.equipment.get(slot))))
}

/// `LivingEntity.aiStep`'s freezing after `applyEffectsFromBlocks`: thawing outside powder
/// snow, the speed penalty (`removeFrost`, `tryAddFrost`) and the freeze damage every two
/// seconds once fully frozen.
fn tick_freezing(t: &mut Tick<'_, '_>) {
    use crate::effect::Operation;
    use crate::effect::attribute::Attribute;
    let freezes = can_freeze(t.mob_ref());
    if !t.mob_ref().in_powder_snow || !freezes {
        let m = t.mob();
        let before = m.ticks_frozen;
        m.ticks_frozen = (m.ticks_frozen - 2).max(0);
        if m.ticks_frozen != before {
            t.core_mut().mark_data_dirty();
        }
    }
    // `removeFrost`, then `tryAddFrost` on a block that is not air.
    let mut changed = t
        .mob()
        .effects
        .modifiers
        .remove(Attribute::MOVEMENT_SPEED, POWDER_SNOW_SPEED);
    let frozen = t.mob_ref().ticks_frozen;
    if frozen > 0 && !t.level.block(on_pos(t, f64::from(0.2_f32))).is_air() {
        #[allow(clippy::cast_precision_loss)]
        let percent = frozen.min(TICKS_TO_FREEZE) as f32 / TICKS_TO_FREEZE as f32;
        t.mob().effects.modifiers.set(
            Attribute::MOVEMENT_SPEED,
            POWDER_SNOW_SPEED,
            f64::from(-0.05 * percent),
            Operation::AddValue,
        );
        changed = true;
    }
    if changed {
        let id = t.id;
        crate::effect::living::sync_mob(t.ents, t.env.out, id);
    }
    if t.mob_ref().tick_count % 40 == 0 && frozen >= TICKS_TO_FREEZE && freezes {
        // `freeze` damage, five times for `#freeze_hurts_extra_types`.
        let amount = if matches!(
            t.mob_ref().kind,
            super::Kind::Strider | super::Kind::Blaze | super::Kind::MagmaCube
        ) {
            5.0
        } else {
            1.0
        };
        let source = DamageSource::of(t.env.types.freeze);
        hurt_self(t, source, amount);
    }
}

/// `SPEED_MODIFIER_POWDER_SNOW_ID`.
const POWDER_SNOW_SPEED: &str = "minecraft:powder_snow";

/// `HoneyBlock.entityInside`: a mob sliding down the side of a honey block.
fn honey_slide(t: &mut Tick<'_, '_>, pos: BlockPos) {
    let old_dy = |dy: f64| dy / f64::from(0.98_f32) + 0.08;
    let new_dy = |dy: f64| (dy - 0.08) * f64::from(0.98_f32);
    let p = t.pos();
    let v = t.velocity();
    // `isSlidingDown`.
    if t.on_ground() || p.y > f64::from(pos.y) + 0.9375 - 1.0e-7 || old_dy(v.y) >= -0.08 {
        return;
    }
    let dx = (f64::from(pos.x) + 0.5 - p.x).abs();
    let dz = (f64::from(pos.z) + 0.5 - p.z).abs();
    let overlap = 0.4375 + f64::from(t.core().dims().width / 2.0);
    if !(dx + 1.0e-7 > overlap || dz + 1.0e-7 > overlap) {
        return;
    }
    // `doSlideMovement`.
    let old = old_dy(v.y);
    let velocity = if old < -0.13 {
        let factor = -0.05 / old;
        Vec3::new(v.x * factor, new_dy(-0.05), v.z * factor)
    } else {
        Vec3::new(v.x, new_dy(-0.05), v.z)
    };
    t.set_velocity(velocity);
    t.mob().fall_distance = 0.0;
    // `maybeDoSlideEffects`: living entities.
    if t.env.random.next_int_bounded(5) == 0 {
        kinds::play_sound(t, "minecraft:block.honey_block.slide", 1.0, 1.0);
    }
    if t.env.random.next_int_bounded(5) == 0 {
        let id = t.id;
        entity_event(t, id, 53);
    }
}

/// `BubbleColumnBlock.entityInside`: an upward or downward drag, stronger at the top
/// (`onAboveBubbleColumn`) than inside (`onInsideBubbleColumn`).
fn bubble_column(t: &mut Tick<'_, '_>, pos: BlockPos, drag_down: bool) {
    let above = t.level.block(BlockPos::new(pos.x, pos.y + 1, pos.z));
    let nothing_above = above.collision_shape().is_empty() && above.fluid_state().is_empty();
    let v = t.velocity();
    let yd = match (nothing_above, drag_down) {
        (true, true) => (v.y - 0.03).max(-0.9),
        (true, false) => (v.y + 0.1).min(1.8),
        (false, true) => (v.y - 0.03).max(-0.3),
        (false, false) => (v.y + 0.06).min(0.7),
    };
    t.set_velocity(Vec3::new(v.x, yd, v.z));
    if nothing_above {
        send_bubble_column_particles(t, pos);
    } else {
        t.mob().fall_distance = 0.0;
    }
}

/// `Entity.sendBubbleColumnParticles`: splashes and bubbles at random spots on the column's
/// top, drawn from the level's random.
fn send_bubble_column_particles(t: &mut Tick<'_, '_>, pos: BlockPos) {
    use crate::level::particles::simple;
    let (x, y, z) = (f64::from(pos.x), f64::from(pos.y + 1), f64::from(pos.z));
    for _ in 0..2 {
        let at = Vec3::new(
            x + t.env.random.next_double(),
            y,
            z + t.env.random.next_double(),
        );
        t.cx.send_particles(simple("minecraft:splash"), at, 1, Vec3::ZERO, 1.0);
        let at = Vec3::new(
            x + t.env.random.next_double(),
            y,
            z + t.env.random.next_double(),
        );
        t.cx.send_particles(
            simple("minecraft:bubble"),
            at,
            1,
            Vec3::new(0.0, 0.01, 0.0),
            0.2,
        );
    }
}

/// `pushEntities`: pushes and is pushed by the entities in its box; too many hurt
/// (`maxEntityCramming`).
fn push_entities(t: &mut Tick<'_, '_>) {
    let aabb = t.aabb();
    let id = t.id;
    if t.mob_ref().kind == super::Kind::ArmorStand {
        crate::decoration::stand::push_minecarts(t.ents, id);
        return;
    }
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&aabb, false, |other| {
        if other != id {
            ids.push(other);
        }
    });
    ids.retain(|&o| {
        t.ents.mob(o).is_some_and(|m| {
            m.is_alive()
                && crate::decoration::stand::pushable(m)
                // `AbstractHorse.isPushable`: not while carrying someone.
                && !(m.kind.is_abstract_horse() && !t.ents.riding.passengers_of(o).is_empty())
        }) || t.ents.minecart(o).is_some()
            || t.ents.boat(o).is_some()
            || t.ents.player(o).is_some_and(|p| {
                !p.survival.is_dead_or_dying()
                    && p.game_mode != vanillite_config::GameMode::Spectator
            })
    });
    if ids.is_empty() {
        t.cx.ids = ids;
        return;
    }
    let max_cramming = t.env.rules.get(Rule::MAX_ENTITY_CRAMMING);
    let cramming_limit = usize::try_from(max_cramming - 1).unwrap_or(usize::MAX);
    if max_cramming > 0
        && ids.len() > cramming_limit
        && t.mob().random.next_int_bounded(4) == 0
        // Passengers do not count.
        && ids
            .iter()
            .filter(|&&o| t.ents.vehicle_of(o).is_none())
            .count()
            > cramming_limit
    {
        let source = DamageSource::of(t.env.types.cramming);
        hurt_self(t, source, 6.0);
        if !t.exists() {
            t.cx.ids = ids;
            return;
        }
    }
    // `Entity.push` does nothing while either side has `noPhysics`: a vex has it during its
    // own tick, so it only gets pushed by the others' ticks.
    if t.mob_ref().kind == super::Kind::Vex {
        t.cx.ids = ids;
        return;
    }
    let pushable_self =
        t.mob_ref().is_alive() && !on_climbable(t) && t.ents.riding.passengers_of(id).is_empty();
    let root = super::root_vehicle(t.ents, id);
    // `max-entity-collisions`: only the first ones push.
    let limit =
        t.cx.settings
            .max_entity_collisions
            .map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
    for &other in ids.iter().take(limit) {
        // `IronGolem.doPush`: now and then turns on the enemy it bumps into.
        if t.mob_ref().kind == super::Kind::IronGolem
            && t.ents
                .mob(other)
                .is_some_and(|m| m.kind.is_monster() && m.kind != super::Kind::Creeper)
            && t.mob().random.next_int_bounded(20) == 0
        {
            super::goals::set_target(t, Some(other));
        }
        if t.ents.minecart(other).is_some() {
            // `AbstractMinecart.push(this)`.
            crate::minecart::push_by(t.ents, other, id);
            sulfur_contact(t, other);
            continue;
        }
        if t.ents.boat(other).is_some() {
            // `AbstractBoat.push(this)`: a mob below the boat's top pushes and is pushed.
            let below = t
                .ents
                .store
                .get(other)
                .is_some_and(|b| aabb.min_y < b.aabb().max_y);
            if below && t.ents.vehicle_of(id) != Some(other) {
                crate::vehicle::push_apart(t.ents, other, id);
            }
            sulfur_contact(t, other);
            continue;
        }
        // `Entity.push`: nothing between passengers of the same vehicle.
        if super::root_vehicle(t.ents, other) == root {
            sulfur_contact(t, other);
            if !t.exists() {
                break;
            }
            continue;
        }
        let Some(o) = t.ents.store.get(other) else {
            continue;
        };
        let op = o.pos();
        let p = t.pos();
        // `other.push(this)`.
        let mut xa = p.x - op.x;
        let mut za = p.z - op.z;
        let mut dd = xa.abs().max(za.abs());
        if dd < f64::from(0.01_f32) {
            sulfur_contact(t, other);
            if !t.exists() {
                break;
            }
            continue;
        }
        dd = dd.sqrt();
        xa /= dd;
        za /= dd;
        let pow = (1.0 / dd).min(1.0);
        // `xa *= pow; xa *= 0.05F`: two roundings.
        xa = xa * pow * f64::from(0.05_f32);
        za = za * pow * f64::from(0.05_f32);
        if t.mob_ref().kind == super::Kind::Warden {
            // `Warden.doPush`.
            super::boss::warden::do_push(t, other);
        }
        // Players take the push in their server-side motion too; only one the server moves
        // itself is synced (a client moves the others).
        let synced = t
            .ents
            .player(other)
            .is_none_or(|p| p.survival.simulate_movement);
        // `Entity.push(entity)`: each side moves only if it is pushable and carries nobody (a
        // dying mob pushes but stays where it is).
        if t.ents.riding.passengers_of(other).is_empty()
            && let Some(mut e) = t.ents.store.get_mut(other)
        {
            let v = e.as_ref().velocity();
            e.set_velocity(Vec3::new(v.x - xa, v.y, v.z - za));
            if synced {
                e.set_flags(EntityFlags::HAS_IMPULSE, true);
            }
        }
        if pushable_self {
            let v = t.velocity();
            t.set_velocity(Vec3::new(v.x + xa, v.y, v.z + za));
            t.core_mut().set_flags(EntityFlags::HAS_IMPULSE, true);
        }
        sulfur_contact(t, other);
        if !t.exists() {
            break;
        }
    }
    t.cx.ids = ids;
}

/// `SulfurCube.doPush`: after the push, a hot cube burns the pushed entity.
fn sulfur_contact(t: &mut Tick<'_, '_>, other: EntityId) {
    if t.mob_ref().kind == super::Kind::SulfurCube {
        let id = t.id;
        super::fam::undead::cubes::contact_damage(t.level, t.ents, t.env, t.cx, id, other);
    }
}

/// The end of `Player.aiStep` after [`crate::survival::player::do_tick_ai`]: `pushEntities`
/// and `touch` (the cubes, then what `touch_more` touches) from where the player's
/// server-side travel took it ([`crate::survival::player::Survival::server_step`]), then the
/// player goes back where it was (`ServerGamePacketListenerImpl.tickPlayer` puts it back
/// where its client's validated movement has it).
pub fn player_ai_step_end(
    level: &crate::level::Level,
    ents: &mut super::Entities,
    env: &mut crate::survival::player::Env<'_>,
    cx: &mut super::MobContext,
    player: EntityId,
    touch_more: impl FnOnce(&mut super::Entities, &vanillite_base::Aabb),
) {
    let Some(p) = ents.player(player) else {
        return;
    };
    let spectator = p.game_mode == vanillite_config::GameMode::Spectator;
    let step = p.survival.server_step;
    let Some(home) = ents.store.get(player).map(|e| e.pos()) else {
        return;
    };
    if step != Vec3::ZERO
        && let Some(mut e) = ents.store.get_mut(player)
    {
        e.set_pos(home + step);
    }
    // `Entity.push` does nothing for a spectator (`noPhysics`).
    if !spectator {
        player_push_entities(ents, player);
    }
    let touches = !spectator && ents.player(player).is_some_and(|p| p.survival.health > 0.0);
    if touches {
        super::fam::undead::cubes::player_touch(level, ents, env, cx, player);
        if let Some(reach) = ents
            .store
            .get(player)
            .map(|e| e.aabb().inflate(1.0, 0.5, 1.0))
        {
            touch_more(ents, &reach);
        }
    }
    if step != Vec3::ZERO
        && let Some(mut e) = ents.store.get_mut(player)
    {
        e.set_pos(home);
    }
}

/// `LivingEntity.pushEntities` of a player ([`player_ai_step_end`]): it pushes and is pushed
/// by the mobs, minecarts and players in its box.
pub fn player_push_entities(ents: &mut super::Entities, player: EntityId) {
    let Some(aabb) = ents.store.get(player).map(|e| e.aabb()) else {
        return;
    };
    let mut ids = Vec::new();
    ents.store.for_each_in(&aabb, false, |other| {
        if other != player {
            ids.push(other);
        }
    });
    ids.retain(|&o| {
        ents.mob(o)
            .is_some_and(|m| m.is_alive() && crate::decoration::stand::pushable(m))
            || ents.minecart(o).is_some()
            || ents.player(o).is_some_and(|p| {
                !p.survival.is_dead_or_dying()
                    && p.game_mode != vanillite_config::GameMode::Spectator
            })
    });
    for other in ids {
        if ents.minecart(other).is_some() {
            crate::minecart::push_by(ents, other, player);
            continue;
        }
        let (Some(o), Some(p)) = (ents.store.get(other), ents.store.get(player)) else {
            continue;
        };
        // `other.push(player)`.
        let (op, pp) = (o.pos(), p.pos());
        let mut xa = pp.x - op.x;
        let mut za = pp.z - op.z;
        let mut dd = xa.abs().max(za.abs());
        if dd < f64::from(0.01_f32) {
            continue;
        }
        dd = dd.sqrt();
        xa /= dd;
        za /= dd;
        let pow = (1.0 / dd).min(1.0);
        // `xa *= pow; xa *= 0.05F`: two roundings.
        xa = xa * pow * f64::from(0.05_f32);
        za = za * pow * f64::from(0.05_f32);
        // A player its client moves takes the push in its server-side motion; only one the
        // server moves itself is synced.
        let synced = ents
            .player(other)
            .is_none_or(|p| p.survival.simulate_movement);
        for (id, x, z, sync) in [(other, -xa, -za, synced), (player, xa, za, true)] {
            if let Some(mut e) = ents.store.get_mut(id) {
                let v = e.as_ref().velocity();
                e.set_velocity(Vec3::new(v.x + x, v.y, v.z + z));
                if sync {
                    e.set_flags(EntityFlags::HAS_IMPULSE, true);
                }
            }
        }
    }
}

/// `Mob.serverAiStep`.
fn server_ai_step(t: &mut Tick<'_, '_>) {
    {
        let m = t.mob();
        m.no_action_time += 1;
        m.seen.clear();
        m.unseen.clear();
    }
    let id_based = t.mob_ref().tick_count + t.core().network_id().0;
    let full = id_based % 2 == 0 || t.mob_ref().tick_count <= 1;
    if full {
        goals::tick_selector(t, goals::Which::Targets);
        goals::tick_selector(t, goals::Which::Goals);
    } else {
        goals::tick_running(t, goals::Which::Targets, false);
        goals::tick_running(t, goals::Which::Goals, false);
    }
    if !t.exists() || t.mob_ref().removed {
        return;
    }
    nav::tick(t);
    kinds::custom_server_ai_step(t);
    if !t.exists() || t.mob_ref().removed {
        return;
    }
    if !super::boss::move_control(t) {
        nav::tick_move_control(t);
    }
    nav::tick_look_control(t);
    if t.mob_ref().kind == super::Kind::Rabbit {
        super::fam::companion::rabbit::jump_control(t);
        return;
    }
    let m = t.mob();
    m.jumping = m.jump;
    m.jump = false;
}

/// `Mob.isSunBurnTick`.
fn is_sun_burn_tick(t: &mut Tick<'_, '_>) -> bool {
    if !t.cx.env.monsters_burn || !t.cx.env.has_sky_light {
        return false;
    }
    let br = t.light_magic_value();
    let p = t.pos();
    let rounded = BlockPos::containing(p.x, t.eye_y(), p.z);
    let rain = (t.env.raining_at)(t.block_pos())
        || (t.env.raining_at)(BlockPos::containing(p.x, t.aabb().max_y, p.z));
    // `isInNonBurnableBlock`: water, rain or powder snow (this tick or the last).
    let m = t.mob_ref();
    let wet = m.fluids.in_water() || rain || m.in_powder_snow || m.was_in_powder_snow;
    // The roll is only drawn in bright light.
    br > 0.5
        && t.mob().random.next_float() * 30.0 < (br - 0.4) * 2.0
        && !wet
        && super::can_see_sky(t.level, rounded)
}

/// `Mob.checkDespawn`.
pub fn check_despawn(t: &mut Tick<'_, '_>) {
    // `Mob.checkDespawn`: armor stands and mannequins are no `Mob`.
    if t.mob_ref().kind.family() == super::Family::Decor {
        return;
    }
    if t.env.difficulty == crate::survival::rules::Difficulty::Peaceful
        && !t.mob_ref().kind.allowed_in_peaceful()
    {
        t.mob().removed = true;
        return;
    }
    if !t.cx.has_players {
        return;
    }
    // `isPersistenceRequired || requiresCustomPersistence` (a passenger or leashed).
    let persistent = t.mob_ref().persistent
        || super::fam::requires_custom_persistence(t.mob_ref())
        || t.ents.riding.vehicle_of(t.id).is_some()
        || t.ents.leashes.contains_key(&t.id);
    let category = super::spawn::Category::of(t.core().kind());
    let despawn = f64::from(category.map_or(128, super::spawn::Category::despawn_distance));
    let p = t.pos();
    let hard = t.cx.settings.hard_despawn_range.unwrap_or(despawn);
    let soft = t.cx.settings.soft_despawn_range;
    // `getNearestPlayer(this, -1.0)`: the distance is only compared with the two ranges, so
    // players farther than both on an axis need not be found.
    let r = hard.max(soft).max(128.0);
    let mut nearest = f64::MAX;
    t.ents.store.players_around(p, r, |player| {
        if let Some(e) = t.ents.store.get(player) {
            let d = e.pos().distance_to_sqr(p);
            // `getNearestPlayer` skips spectators (looked up only for a closer player).
            if d < nearest
                && !t
                    .ents
                    .player(player)
                    .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Spectator)
            {
                nearest = d;
            }
        }
        false
    });
    let far_away_ok = super::fam::remove_when_far_away_mob(t.mob_ref(), nearest);
    if !persistent && nearest > hard * hard && far_away_ok {
        t.mob().removed = true;
        return;
    }
    let m = t.mob();
    if !persistent
        && m.no_action_time > 600
        && m.random.next_int_bounded(800) == 0
        && nearest > soft * soft
        && far_away_ok
    {
        m.removed = true;
    } else if nearest < soft * soft {
        m.no_action_time = 0;
    }
}

/// `Mob.getMaxFallDistance`.
pub fn max_fall_distance(t: &Tick<'_, '_>) -> i32 {
    let m = t.mob_ref();
    if m.target.is_none() {
        return 3;
    }
    if m.kind == super::Kind::Creeper {
        return floor(f64::from(m.health - 1.0 + 3.0));
    }
    #[allow(clippy::cast_possible_truncation)]
    let mut sacrifice = (m.health - m.max_health() * 0.33) as i32;
    sacrifice -= (3 - t.env.difficulty as i32) * 4;
    if sacrifice < 0 {
        sacrifice = 0;
    }
    floor(f64::from(sacrifice as f32 + 3.0))
}

/// `Sensing.hasLineOfSight` with its per-tick cache.
pub fn has_line_of_sight(t: &mut Tick<'_, '_>, target: EntityId) -> bool {
    if t.mob_ref().seen.contains(&target) {
        return true;
    }
    if t.mob_ref().unseen.contains(&target) {
        return false;
    }
    let Some(other) = t.other(target) else {
        return false;
    };
    let p = t.pos();
    let from = Vec3::new(p.x, t.eye_y(), p.z);
    let to = Vec3::new(other.pos.x, other.eye_y, other.pos.z);
    let can_see = to.distance_to_sqr(from) <= 128.0 * 128.0
        && physics::clip(t.level as &dyn Blocks, from, to).is_none();
    let m = t.mob();
    if can_see {
        m.seen.push(target);
    } else {
        m.unseen.push(target);
    }
    can_see
}

#[cfg(test)]
mod tests {
    use super::*;
    use vanillite_data::block::ids;

    /// A water source at the origin flowing east into a level 1 block, on stone.
    struct Stream;
    impl Blocks for Stream {
        fn state(&self, pos: BlockPos) -> BlockState {
            if pos.y < 64 {
                return ids::STONE.default_state();
            }
            if pos.y == 64 && pos.z == 0 {
                let water = ids::WATER.default_state();
                match pos.x {
                    0 => return water,
                    1 => return water.with("level", "1").unwrap_or(water),
                    _ => {}
                }
            }
            BlockState::AIR
        }
    }

    fn facts() -> crate::survival::blocks::BlockFacts {
        use crate::block::fluid::{FLOWING_LAVA, FLOWING_WATER, LAVA, WATER};
        crate::survival::blocks::BlockFacts::new(
            &[],
            &[],
            &[],
            [FLOWING_WATER, WATER],
            [FLOWING_LAVA, LAVA],
        )
    }

    #[test]
    fn water_flows_down_its_slope() {
        let facts = facts();
        let pos = BlockPos::new(0, 64, 0);
        let fluid = Stream.state(pos).fluid_state();
        let flow = fluid_flow(&Stream, &facts, pos, fluid, true);
        assert!((flow.x - 1.0).abs() < 1.0e-9, "{flow:?}");
        assert!(flow.y.abs() < 1.0e-9 && flow.z.abs() < 1.0e-9);
        // The flowing block pushes on to the east too (its east neighbour is lower).
        let next = BlockPos::new(1, 64, 0);
        let flow = fluid_flow(
            &Stream,
            &facts,
            next,
            Stream.state(next).fluid_state(),
            true,
        );
        assert!(flow.x > 0.0, "{flow:?}");
    }
}
