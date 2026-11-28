//! `TeleportRandomlyConsumeEffect` of a player (chorus fruit): up to 16 tries of
//! `LivingEntity.randomTeleport` within the diameter around it.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::{BlockState, ids};
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::EntityId;

use crate::entities::Entities;
use crate::mob::physics::{self, Blocks as _};
use crate::survival::player::Env;

/// The default diameter of `teleport_randomly`.
pub const DIAMETER: f64 = 16.0;

/// The world's blocks for the collision checks (air where nothing is loaded).
struct EnvBlocks<'a>(&'a dyn crate::survival::blocks::BlockAccess);

impl physics::Blocks for EnvBlocks<'_> {
    fn state(&self, pos: BlockPos) -> BlockState {
        self.0.block_state(pos).unwrap_or(BlockState::AIR)
    }
}

/// `#entities_can_teleport_to` (without loaded tags: `blocksMotion`, solid and not cobweb or
/// bamboo sapling).
fn can_teleport_onto(state: BlockState) -> bool {
    const TAG: &str = "minecraft:entities_can_teleport_to";
    match crate::player::play::game_data() {
        Some(d) if d.block_tags.contains_key(TAG) => d.block_in_tag(state.block().0, TAG),
        _ => {
            state.block() != ids::COBWEB
                && state.block() != ids::BAMBOO_SAPLING
                && crate::mob::nav::is_solid(state)
        }
    }
}

/// `TeleportRandomlyConsumeEffect.apply` of a player. The tries draw from the player's own
/// random. Returns whether it teleported.
pub fn teleport_randomly(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    diameter: f64,
    directional_particles: bool,
) -> bool {
    let Some(e) = ents.store.get(player) else {
        return false;
    };
    let (old, dims) = (e.pos(), e.dims());
    let blocks = EnvBlocks(env.blocks);
    let min_y = f64::from(env.min_y);
    let max_y = f64::from(env.min_y) + f64::from(env.logical_height) - 1.0;
    let mut scratch = Vec::new();
    let mut found = None;
    for _ in 0..16 {
        let Some((dx, dy, dz)) = ents.player_mut(player).map(|p| {
            let r = &mut p.survival.random;
            (r.next_double(), r.next_double(), r.next_double())
        }) else {
            return false;
        };
        let x = old.x + (dx - 0.5) * diameter;
        let y = (old.y + (dy - 0.5) * diameter).clamp(min_y, max_y);
        let z = old.z + (dz - 0.5) * diameter;
        // `randomTeleport`: `worldBorder.clampVec3ToBound`.
        let (x, z) = match env.border {
            Some(b) => {
                let c = b.clamp_vec(x, y, z);
                (c.x, c.z)
            }
            None => (x, z),
        };
        // `stopRiding` comes before each try.
        if ents.riding.vehicle_of(player).is_some() {
            crate::riding::stop_riding(ents, env, player);
        }
        // `randomTeleport(x, y, z, true)`: down to the first block that blocks motion.
        let mut pos = BlockPos::containing(x, y, z);
        if env.blocks.block_state(pos).is_none() {
            continue;
        }
        let mut y = y;
        let mut landed = false;
        while !landed && pos.y > env.min_y {
            let below = BlockPos::new(pos.x, pos.y.saturating_sub(1), pos.z);
            if can_teleport_onto(blocks.state(below)) {
                landed = true;
            } else {
                y -= 1.0;
                pos = below;
            }
        }
        if !landed {
            continue;
        }
        let at = Vec3::new(x, y, z);
        let aabb = dims.bounding_box(at);
        if physics::no_collision(&blocks, &aabb, &mut scratch)
            && !physics::contains_any_liquid(&blocks, &aabb)
        {
            found = Some(at);
            break;
        }
    }
    let Some(at) = found else {
        return false;
    };
    let Some(to) = super::movement::teleport_within_world(ents, player, at, 0.0, 0.0, ROTATION)
    else {
        return false;
    };
    // The client keeps its motion (`DELTA`); a player the server moves itself starts from rest
    // like Vanilla's (recorded in the `item_food` golden).
    if ents
        .player(player)
        .is_some_and(|p| p.survival.simulate_movement)
        && let Some(mut e) = ents.store.get_mut(player)
    {
        e.set_velocity(Vec3::ZERO);
    }
    let network_id = ents.store.get(player).map_or(0, |e| e.network_id().0);
    // `broadcastEntityEvent(46)`: the teleport particles.
    env.out.push(crate::survival::Effect::Tracking {
        entity: player,
        with_self: true,
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
        Some(player),
    );
    crate::survival::play_sound(
        env.out,
        None,
        to,
        "minecraft:item.chorus_fruit.teleport",
        SoundSource::Players,
        1.0,
        1.0,
        crate::survival::sound_seed(),
    );
    if directional_particles {
        // `levelEvent(2017, origin, clampedPackDifferenceInPosition(origin, target, 127..))`.
        let origin = BlockPos::containing(old.x, old.y, old.z);
        let target = BlockPos::containing(to.x, to.y, to.z);
        let d = |a: i32, b: i32| b.saturating_sub(a).clamp(-127, 127).wrapping_add(127) & 0xFF;
        let data = d(origin.x, target.x) << 16 | d(origin.y, target.y) << 8 | d(origin.z, target.z);
        env.out.push(crate::survival::Effect::Nearby {
            pos: Vec3::new(
                f64::from(origin.x),
                f64::from(origin.y),
                f64::from(origin.z),
            ),
            range: 64.0,
            except: None,
            packet: vanillite_protocol::SharedPacket::of(
                &vanillite_protocol::packets::play::clientbound::LevelEvent {
                    event: 2017,
                    pos: vanillite_protocol::types::BlockPos {
                        x: origin.x,
                        y: origin.y,
                        z: origin.z,
                    },
                    data,
                    global: false,
                },
            ),
        });
    }
    if let Some(p) = ents.player_mut(player) {
        crate::advancements::tracking::reset_fall_distance(p);
        // `resetCurrentImpulseContext`.
        p.survival.impulse_impact = None;
        p.survival.impulse_grace = 0;
    }
    true
}

/// Rotation and motion stay (`teleportTo(x, y, z)`: `Relative.union(DELTA, ROTATION)`).
const ROTATION: i32 = {
    use vanillite_protocol::packets::play::clientbound::relative::*;
    Y_ROT | X_ROT | DELTA_X | DELTA_Y | DELTA_Z | ROTATE_DELTA
};
