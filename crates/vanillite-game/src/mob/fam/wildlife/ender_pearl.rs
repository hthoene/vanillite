//! Thrown ender pearls (`ThrownEnderpearl`, `EnderpearlItem`): a player throws one with a 20
//! tick cooldown; it flies like every throwable and, where it lands, its owner arrives with 5
//! damage (a 5 % chance leaves an endermite behind). While a player owns it, it keeps the
//! chunks around it loaded (`TicketType.ENDER_PEARL`); it vanishes when its owner dies
//! (`ender_pearls_vanish_on_death`) and leaves the world with its owner (stored in the
//! player's data).
//!
//! Kind specific data: `ints[0]` is `ticketTimer`, `ints[1]` is 1 while the owner is a player
//! of the server (in any world).

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{ChunkPos, Vec3};
use vanillite_config::GameMode;
use vanillite_data::entity::ids as entity_ids;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::packets::play::clientbound::{SoundSource, relative};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{EntityFlags, EntityId, NetworkIds, Rotation};

use super::snowball::{Flight, fly, thrown_source};
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::mob::missile::{self, MissileData, MissileKind, MissileTick};
use crate::portal::{PearlArrival, TravelRequest};
use crate::survival::damage::DamageSource;
use crate::survival::play_sound;
use crate::survival::player::Env;
use crate::survival::rules::{Difficulty, Rule};

/// The item's cooldown group.
pub const ITEM: &str = "minecraft:ender_pearl";
/// `TicketType.ENDER_PEARL.timeout()`.
pub const TICKET_TICKS: u64 = 40;
/// `ServerPlayer.ENDER_PEARL_TICKET_RADIUS`.
pub const TICKET_RADIUS: i32 = 2;

/// `EnderpearlItem.use` by `player` with `stack` in `hand`: the throw sound, the pearl
/// (`spawnProjectileFromRotation` with power 1.5 and uncertainty 1), one item less unless the
/// player has infinite materials, and the cooldown. Returns `false` while the item cools down.
pub fn throw(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &NetworkIds,
    player: EntityId,
    stack: &mut ItemStack,
    hand: Hand,
) -> bool {
    let _ = hand;
    let Some(p) = ents.player(player) else {
        return false;
    };
    if p.game_mode == GameMode::Spectator || p.survival.cooldowns.is_on_cooldown(stack) {
        return false;
    }
    let creative = p.game_mode == GameMode::Creative;
    let known = p.connection.movement.known_movement;
    let Some(e) = ents.store.get(player) else {
        return false;
    };
    let (pos, rot, on_ground, uuid) = (
        e.pos(),
        e.rot(),
        e.flags().contains(EntityFlags::ON_GROUND),
        e.uuid(),
    );
    let eye = crate::survival::player::eye_height(e.dims().height);
    let pitch = 0.4 / (env.random.next_float() * 0.4 + 0.8);
    play_sound(
        env.out,
        None,
        pos,
        "minecraft:entity.ender_pearl.throw",
        SoundSource::Neutral,
        0.5,
        pitch,
        crate::survival::sound_seed(),
    );
    // The pearl's own random (its constructor), then `shootFromRotation` draws from it.
    let mut own = LegacyRandom::new(crate::mob::new_entity_seed());
    let pearl_uuid = crate::mob::new_uuid();
    let rad = crate::mob::DEG_TO_RAD;
    let (y, x) = (rot.y_rot * rad, rot.x_rot * rad);
    let sin = |a: f32| vanillite_base::math::sin(f64::from(a));
    let cos = |a: f32| vanillite_base::math::cos(f64::from(a));
    let dir = Vec3::new(
        f64::from(-sin(y) * cos(x)),
        f64::from(-sin(x)),
        f64::from(cos(y) * cos(x)),
    );
    let (mut velocity, rot) = super::snowball::shoot(&mut own, dir, 1.5, 1.0);
    velocity = velocity + Vec3::new(known.x, if on_ground { 0.0 } else { known.y }, known.z);
    let mut data = MissileData::new(MissileKind::EnderPearl);
    data.owner = Some(player);
    data.owner_uuid = Some(uuid);
    data.item = stack.with_count(1);
    data.ints[1] = 1;
    data.random_state = own.state();
    #[allow(clippy::cast_possible_truncation)]
    let start = Vec3::new(pos.x, pos.y + eye - f64::from(0.1_f32), pos.z);
    let id = missile::spawn(
        ents,
        network_ids,
        pearl_uuid,
        start,
        velocity,
        Rotation {
            y_head_rot: 0.0,
            ..rot
        },
        data,
    );
    if !ents.launched(id, Some(player)) {
        // Bukkit's cancelled launch: the pearl stays in the hand, no cooldown.
        return true;
    }
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_flags(EntityFlags::HAS_IMPULSE, true);
    }
    if !creative {
        stack.shrink(1);
    }
    // The `use_cooldown` (1 s) follows in `ItemStack.use` ([`crate::player::item_use`]).
    true
}

/// The owner of a pearl as its world sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    None,
    /// A player of this world (alive or dead).
    Player(EntityId),
    /// A player of another world.
    Away(u128),
    /// Another entity of this world.
    Entity(EntityId),
}

/// `findOwnerIncludingDeadPlayer` / `getOwner`.
fn owner(t: &mut MissileTick<'_, '_>) -> Owner {
    let Some(d) = t.ents.missile(t.id) else {
        return Owner::None;
    };
    let (local, uuid, player) = (d.owner, d.owner_uuid, d.ints[1] == 1);
    let Some(uuid) = uuid else {
        return Owner::None;
    };
    if let Some(id) = t.ents.player_by_uuid(uuid) {
        if let Some(d) = t.data() {
            d.owner = Some(id);
            d.ints[1] = 1;
        }
        return Owner::Player(id);
    }
    if let Some(id) = local.filter(|&id| t.ents.store.get(id).is_some_and(|e| e.uuid() == uuid)) {
        return Owner::Entity(id);
    }
    if player {
        Owner::Away(uuid)
    } else {
        Owner::None
    }
}

/// `ThrownEnderpearl.tick`.
pub(super) fn tick(t: &mut MissileTick<'_, '_>) {
    let Some(pos) = t.core().map(|e| e.pos()) else {
        return;
    };
    let chunk = ChunkPos::new(
        vanillite_base::math::floor(pos.x) >> 4,
        vanillite_base::math::floor(pos.z) >> 4,
    );
    let owner = owner(t);
    let vanish = match owner {
        Owner::Player(id) => t.ents.player(id).is_some_and(|p| {
            p.survival.is_dead_or_dying()
                && !p.survival.portal.won_game
                && t.env.rules.bool(Rule::ENDER_PEARLS_VANISH_ON_DEATH)
        }),
        _ => false,
    };
    if vanish {
        if let Some(d) = t.data() {
            d.removed = true;
        }
    } else {
        let owner_id = t.ents.missile(t.id).and_then(|d| d.owner);
        match fly(t) {
            Flight::Hit(target, block, at) => {
                // `Projectile.onHit`: `onHitEntity`, then the `PROJECTILE_LAND` game event.
                if let Some(target) = target {
                    let source = thrown_source(t, owner_id);
                    crate::mob::combat::hurt(t.level, t.ents, t.env, t.cx, target, source, 0.0);
                }
                if let Some(block) = block {
                    crate::mob::missile::hit_block(t.level, t.cx, t.id, block, at);
                }
                crate::mob::missile::land_event(t.env.out, t.level, t.id, owner_id, block, at);
                on_hit(t, owner);
            }
            Flight::Miss | Flight::Gone => {}
            // Its new world moves it; no ticket stays here.
            Flight::Away => return,
        }
    }
    // The ticket that keeps the pearl's chunks loaded for its owner.
    let Some(d) = t.ents.missile(t.id) else {
        return;
    };
    if d.removed {
        return;
    }
    let Some(now) = t.core().map(|e| e.pos()) else {
        return;
    };
    let at = vanillite_base::BlockPos::containing(now.x, now.y, now.z);
    let moved = at.x >> 4 != chunk.x || at.z >> 4 != chunk.z;
    let timer = d.ints[0] - 1;
    let is_player = matches!(owner, Owner::Player(_) | Owner::Away(_));
    let renew = (timer <= 0 || moved) && is_player;
    if renew {
        // `registerAndUpdateEnderPearlTicket`.
        t.cx.travel.push(TravelRequest::PearlTicket(at.chunk()));
    }
    if let Some(d) = t.data() {
        #[allow(clippy::cast_possible_truncation)]
        let next = if renew {
            TICKET_TICKS as i32 - 1
        } else {
            timer
        };
        d.ints[0] = next;
    }
}

/// The rest of `onHit` after the damage-free hit of `onHitEntity`: the portal particles draw
/// from the pearl's random, then the owner arrives where the pearl was at the start of the tick
/// and the pearl is gone.
fn on_hit(t: &mut MissileTick<'_, '_>, owner: Owner) {
    let Some(d) = t.ents.missile(t.id) else {
        return;
    };
    if d.removed {
        return;
    }
    let mut random = LegacyRandom::from_state(d.random_state);
    // The 32 portal particles (`addParticle` draws, the server shows nothing).
    for _ in 0..32 {
        random.next_double();
        random.next_gaussian();
        random.next_gaussian();
    }
    let Some(e) = t.core() else {
        return;
    };
    let (to, cooldown) = (
        e.old_pos(),
        t.ents.portals.get(&t.id).is_some_and(|p| p.cooldown > 0),
    );
    let spawn_monsters =
        t.env.rules.bool(Rule::SPAWN_MONSTERS) && t.env.difficulty != Difficulty::Peaceful;
    match owner {
        Owner::Player(id) => {
            let allowed = t.ents.player(id).is_some_and(|p| {
                // `isAllowedToTeleportOwner`: alive and not sleeping.
                !p.survival.is_dead_or_dying()
                    && p.survival.sleeping_pos.is_none()
                    && !p.connection.sink.is_closed()
            });
            // Bukkit's `PlayerTeleportEvent` (`ENDER_PEARL`): cancelled, the owner stays; a
            // handler may move the landing within this world.
            let mut turn = (0.0, 0.0);
            let landing = if allowed {
                pearl_event(t, id, to, &mut turn)
            } else {
                None
            };
            if let Some(to) = landing {
                let endermite = random.next_float() < 0.05 && spawn_monsters;
                if endermite && let Some(e) = t.ents.store.get(id) {
                    t.env.out.push(crate::survival::Effect::SpawnMob {
                        kind: entity_ids::ENDERMITE,
                        pos: e.pos(),
                        y_rot: e.rot().y_rot,
                        velocity: Vec3::ZERO,
                    });
                }
                if let Some(p) = t.ents.player_mut(id)
                    && cooldown
                {
                    p.survival.portal.cooldown = crate::portal::PLAYER_PORTAL_COOLDOWN;
                }
                arrive(t.ents, t.env, id, to, turn);
                teleport_sound(t, to);
            }
        }
        Owner::Away(uuid) => {
            // Taken as alive and connected: its own world checks when it gets the request.
            let endermite = random.next_float() < 0.05 && spawn_monsters;
            t.cx.travel.push(TravelRequest::PearlOwner {
                owner: uuid,
                arrival: PearlArrival {
                    pos: to,
                    endermite,
                    cooldown,
                },
            });
            teleport_sound(t, to);
        }
        Owner::Entity(id) => {
            let alive = t.ents.mob(id).is_none_or(crate::mob::MobData::is_alive);
            if alive {
                if let Some(mut e) = t.ents.store.get_mut(id) {
                    e.set_pos(to);
                    e.set_velocity(Vec3::ZERO);
                    e.set_old_pos();
                }
                if let Some(m) = t.ents.mob_mut(id) {
                    m.fall_distance = 0.0;
                }
                teleport_sound(t, to);
            }
        }
        Owner::None => {}
    }
    if let Some(d) = t.data() {
        d.random_state = random.state();
        d.removed = true;
    }
}

/// The plugins' teleport event of the owner landing at `to` in the pearl's world: `None`
/// when cancelled, else where it lands; `turn` gets the change of its rotation a handler made.
fn pearl_event(
    t: &MissileTick<'_, '_>,
    player: EntityId,
    to: Vec3,
    turn: &mut (f32, f32),
) -> Option<Vec3> {
    if !t.env.hooks.wants(vanillite_abi::event::PLAYER_TELEPORT) {
        return Some(to);
    }
    let rot = t.ents.store.get(player)?.rot();
    let place = crate::hooks::Place::looking(to, rot.y_rot, rot.x_rot);
    let landing = crate::player::movement::teleport_event(
        Some(t.env.hooks),
        t.ents,
        player,
        vanillite_abi::teleport_cause::ENDER_PEARL,
        place,
    )?;
    if landing.world.is_some() {
        return Some(to);
    }
    *turn = (landing.yaw - rot.y_rot, landing.pitch - rot.x_rot);
    Some(landing.vec())
}

/// The owner's part of the landing in the pearl's world: `teleport` to `pos` keeping its
/// rotation (turned by `turn`, a plugin's change) and stopping (`Relative.ROTATION`), then
/// [`land`].
fn arrive(ents: &mut Entities, env: &mut Env<'_>, player: EntityId, pos: Vec3, turn: (f32, f32)) {
    let rotation = relative::Y_ROT | relative::X_ROT;
    // `ServerPlayer.teleport`: not `asPassenger`, so `removeVehicle` first.
    if ents.riding.vehicle_of(player).is_some() {
        crate::riding::stop_riding(ents, env, player);
    }
    if crate::player::movement::teleport_within_world(ents, player, pos, turn.0, turn.1, rotation)
        .is_some()
    {
        land(ents, env, player);
    }
}

/// After the owner's teleport to a pearl (in the pearl's world or brought over from another
/// one): `resetFallDistance`, `resetCurrentImpulseContext`, then 5 ender pearl damage.
pub fn land(ents: &mut Entities, env: &mut Env<'_>, player: EntityId) {
    let Entities { store, players, .. } = ents;
    let Some(mut entity) = store.get_mut(player) else {
        return;
    };
    let row = entity.as_ref().kind_row();
    let Some(data) = players.get_mut(row) else {
        return;
    };
    // `ServerPlayer.resetFallDistance`: `FALL_FROM_HEIGHT` and the start of the fall.
    crate::advancements::tracking::reset_fall_distance(data);
    data.survival.impulse_impact = None;
    data.survival.impulse_grace = 0;
    let source = DamageSource::of(env.types.named("minecraft:ender_pearl"));
    crate::survival::player::hurt(&mut entity, data, env, source, 5.0);
}

/// `playSound(PLAYER_TELEPORT)` at the landing.
fn teleport_sound(t: &mut MissileTick<'_, '_>, pos: Vec3) {
    play_sound(
        t.env.out,
        None,
        pos,
        "minecraft:entity.player.teleport",
        SoundSource::Players,
        1.0,
        1.0,
        crate::survival::sound_seed(),
    );
}

/// `ServerPlayer.ENDER_PEARL_DIMENSION_TAG`.
pub const DIMENSION_TAG: &str = "ender_pearl_dimension";

/// `saveEnderPearls` and `setRemoved(UNLOADED_WITH_PLAYER)` when `owner` leaves: its pearls in
/// this world as entity NBT with their dimension, removed from the world.
pub fn take_owned(ents: &mut Entities, owner: u128, dimension: &str) -> Vec<Compound> {
    let ids: Vec<EntityId> = ents
        .missiles
        .iter()
        .filter(|(_, d)| {
            d.kind == MissileKind::EnderPearl && !d.removed && d.owner_uuid == Some(owner)
        })
        .map(|(id, _)| id)
        .collect();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let (Some(core), Some(d)) = (ents.store.get(id), ents.missile(id)) {
            let mut tag = missile::save(&core, d, None);
            tag.insert(DIMENSION_TAG, Tag::String(dimension.to_owned()));
            out.push(tag);
        }
        ents.despawn(id);
    }
    out
}

/// An ender pearl that came from another world (`teleportCrossDimension`): its owner `is_player`
/// (`findOwnerIncludingDeadPlayer` finds players of every world).
pub fn arrived(ents: &mut Entities, id: EntityId, is_player: impl FnOnce(u128) -> bool) {
    let Some(d) = ents
        .missile_mut(id)
        .filter(|d| d.kind == missile::MissileKind::EnderPearl)
    else {
        return;
    };
    if d.owner_uuid.is_some_and(is_player) {
        d.ints[1] = 1;
    }
}

/// `loadAndSpawnEnderPearl` when its owner joins: the pearl back in this world. Returns its
/// chunk for the ender pearl ticket.
pub fn spawn_saved(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    tag: &Compound,
) -> Option<ChunkPos> {
    let (e, mut data) = missile::load(tag)?;
    data.ints[1] = 1;
    let id = missile::spawn(ents, network_ids, e.uuid, e.pos, e.velocity, e.rot, data);
    let pos = ents.store.get(id)?.pos();
    Some(ChunkPos::new(
        vanillite_base::math::floor(pos.x) >> 4,
        vanillite_base::math::floor(pos.z) >> 4,
    ))
}
