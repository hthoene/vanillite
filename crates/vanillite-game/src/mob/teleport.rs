//! `LivingEntity.randomTeleport`: a mob moves to a spot below a target position, on a block
//! of `#entities_can_teleport_to`, avoiding blocks a predicate rejects.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::EntityId;

use super::physics;
use crate::entities::Entities;
use crate::level::Level;
use crate::survival::player::Env;

/// `randomTeleport(x, y, z, showParticles, avoid)`: walks down from the target (clamped into
/// the world border) to the first block of `#entities_can_teleport_to`; that block and the
/// mob's box there must not be in the block tag `avoid`, the box free of collisions and
/// liquids. Returns whether the mob moved (`teleportTo` → `snapTo`: the old position moves
/// along, the motion stays). `show_particles` (entity event 46) is not done here.
pub fn random_teleport(
    level: &Level,
    ents: &mut Entities,
    id: EntityId,
    to: Vec3,
    avoid: &str,
) -> bool {
    let to = level.border.clamp_vec(to.x, to.y, to.z);
    let mut pos = BlockPos::containing(to.x, to.y, to.z);
    if !level.is_loaded(pos) {
        return false;
    }
    let mut y = to.y;
    while pos.y > level.min_y() {
        pos = BlockPos::new(pos.x, pos.y - 1, pos.z);
        let state = level.block(pos);
        if super::spawn::in_block_tag(state, "minecraft:entities_can_teleport_to") {
            if super::spawn::in_block_tag(state, avoid) {
                return false;
            }
            let Some(dims) = ents.store.get(id).map(|e| e.dims()) else {
                return false;
            };
            let dest = Vec3::new(to.x, y, to.z);
            let aabb = dims.bounding_box(dest);
            let mut scratch = Vec::new();
            if !physics::no_collision(&level.view(), &aabb, &mut scratch)
                || physics::contains_any_liquid(level, &aabb)
            {
                return false;
            }
            // `findBlocksIn(aabb).filterState(avoid).anyMatched()`.
            let (lo, hi) = (
                BlockPos::containing(aabb.min_x, aabb.min_y, aabb.min_z),
                BlockPos::containing(aabb.max_x, aabb.max_y, aabb.max_z),
            );
            for x in lo.x..=hi.x {
                for yy in lo.y..=hi.y {
                    for z in lo.z..=hi.z {
                        let b = level.block(BlockPos::new(x, yy, z));
                        if super::spawn::in_block_tag(b, avoid) {
                            return false;
                        }
                    }
                }
            }
            if let Some(mut e) = ents.store.get_mut(id) {
                e.set_pos(dest);
                e.set_old_pos();
            }
            if let Some(m) = ents.mob_mut(id) {
                m.nav.stop();
            }
            return true;
        }
        y -= 1.0;
    }
    false
}

/// `TeleportRandomlyConsumeEffect.apply` of a mob (a death protection item it holds): up to
/// 16 tries of [`random_teleport`] (avoiding `#consumable_does_not_teleport_to`) within the
/// diameter, drawn from the mob's random. Returns whether it teleported.
pub fn consume_teleport(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut super::MobContext,
    id: EntityId,
    diameter: f64,
    directional_particles: bool,
) -> bool {
    let min_y = f64::from(env.min_y);
    let max_y = min_y + f64::from(env.logical_height) - 1.0;
    for _ in 0..16 {
        let Some(old) = ents.store.get(id).map(|e| e.pos()) else {
            return false;
        };
        let Some(m) = ents.mob_mut(id) else {
            return false;
        };
        let x = old.x + (m.random.next_double() - 0.5) * diameter;
        let y = (old.y + (m.random.next_double() - 0.5) * diameter).clamp(min_y, max_y);
        let z = old.z + (m.random.next_double() - 0.5) * diameter;
        if ents.vehicle_of(id).is_some() {
            crate::riding::stop_riding(ents, env, id);
        }
        let Some(old) = ents.store.get(id).map(|e| e.pos()) else {
            return false;
        };
        if !random_teleport(
            level,
            ents,
            id,
            Vec3::new(x, y, z),
            "minecraft:consumable_does_not_teleport_to",
        ) {
            continue;
        }
        let Some(e) = ents.store.get(id) else {
            return false;
        };
        let (at, network_id, fox) = (
            e.pos(),
            e.network_id().0,
            e.kind() == vanillite_data::entity::ids::FOX,
        );
        // `broadcastEntityEvent(this, 46)`: the teleport particles.
        env.out.push(crate::survival::Effect::Tracking {
            entity: id,
            with_self: false,
            packet: vanillite_protocol::SharedPacket::of(
                &vanillite_protocol::packets::play::clientbound::EntityEvent {
                    entity_id: network_id,
                    event: 46,
                },
            ),
        });
        crate::survival::game_event(
            env.out,
            crate::level::game_event::events().teleport,
            old,
            Some(id),
        );
        let (sound, source) = if fox {
            ("minecraft:entity.fox.teleport", SoundSource::Neutral)
        } else {
            ("minecraft:item.chorus_fruit.teleport", SoundSource::Players)
        };
        crate::survival::play_sound(
            env.out,
            None,
            at,
            sound,
            source,
            1.0,
            1.0,
            crate::survival::sound_seed(),
        );
        if directional_particles {
            let origin = BlockPos::containing(old.x, old.y, old.z);
            let target = BlockPos::containing(at.x, at.y, at.z);
            // `BlockUtil.clampedPackDifferenceInPosition(origin, target, 127, 127, 127)`.
            let d = |a: i32, b: i32| b.saturating_sub(a).clamp(-127, 127).wrapping_add(127) & 0xFF;
            let data =
                d(origin.x, target.x) << 16 | d(origin.y, target.y) << 8 | d(origin.z, target.z);
            cx.actions
                .push(super::WorldAction::LevelEvent(2017, origin, data));
        }
        if let Some(m) = ents.mob_mut(id) {
            m.fall_distance = 0.0;
        }
        return true;
    }
    false
}
