//! What players see of a world tick's simulation (`ChunkHolder.broadcastChanges`,
//! `ServerLevel.levelEvent`, `ServerLevel.runBlockEvents`): light updates, changed blocks per
//! section, block events and level events. Each packet is encoded once and sent to the players
//! that see it.

use std::sync::Arc;

use vanillite_base::hash::FxHashMap;
use vanillite_base::{BlockPos, ChunkPos, SectionPos};
use vanillite_game::entities::Entities;
use vanillite_game::level::{Level, LevelOutput, SectionChanges};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::codec::{write_varint, write_varlong};
use vanillite_protocol::ids::play::clientbound as ids;
use vanillite_world::chunks::LightChange;

/// Range of level events and block events in blocks (`PlayerList.broadcast(..., 64.0, ...)`).
const EVENT_RANGE: f64 = 64.0;

/// Scratch space of the broadcast, kept between ticks.
#[derive(Debug, Default)]
pub(crate) struct WorldUpdates {
    light: Vec<(ChunkPos, LightChange)>,
    per_chunk: FxHashMap<ChunkPos, Vec<Arc<SharedPacket>>>,
    /// The block changes up to this tick's [`Self::chunk_source_tick`], sent by
    /// [`Self::broadcast`]; later ones wait in the level's output for the next tick.
    ready: Changes,
}

/// Changed positions per section, each once, in change order.
type Changes = FxHashMap<SectionPos, SectionChanges>;

impl WorldUpdates {
    /// The light changes of this tick, for the chunk packet cache and the broadcast, and the
    /// block changes this tick's broadcast sends.
    pub(crate) fn light_and_ready(&mut self) -> (&mut Vec<(ChunkPos, LightChange)>, &mut Changes) {
        (&mut self.light, &mut self.ready)
    }

    /// `broadcastChangedChunks` in `ServerChunkCache.tick`: the block changes made so far
    /// (the previous tick's after this point, this tick's packets, scheduled and random ticks)
    /// go out with this tick's [`Self::broadcast`]. Those of block events, the dragon fight,
    /// entities, block entities and players later in the tick go out the next tick.
    pub(crate) fn chunk_source_tick(&mut self, level: &mut Level) {
        let changed = std::mem::take(&mut level.output().changed);
        merge_changes(&mut self.ready, changed);
    }

    /// Sends the light changes collected with [`Self::light_and_ready`] and the level's output,
    /// then clears both; block changes since [`Self::chunk_source_tick`] stay for the next
    /// tick.
    pub(crate) fn broadcast(
        &mut self,
        level: &mut Level,
        global: &crate::global::Global,
        world: crate::world::WorldId,
    ) {
        let mut output = std::mem::take(level.output());
        let later = std::mem::replace(&mut output.changed, std::mem::take(&mut self.ready));
        self.encode_chunks(level, &mut output);
        let entities = &level.entities;
        if !output.global_level_events.is_empty() {
            let global_sounds = global
                .level
                .rules()
                .bool(vanillite_game::survival::rules::Rule::GLOBAL_SOUND_EVENTS);
            for (event, pos, data) in output.global_level_events.drain(..) {
                global_level_event(entities, global, world, global_sounds, (event, pos, data));
            }
        }
        // Vanilla sends level events when they happen, before the chunk holders broadcast the
        // block changes of the tick.
        for event in output.level_events.drain(..) {
            let packet = level_event(event.event, event.pos, event.data);
            send_near(entities, event.pos, event.source, &packet);
        }
        for particles in output.particles.drain(..) {
            send_particles(entities, &particles);
        }
        if !self.per_chunk.is_empty() {
            for (_, player) in entities.players.iter() {
                let chunks = &player.connection.chunks;
                let Some(view) = chunks.view() else {
                    continue;
                };
                for (&pos, packets) in &self.per_chunk {
                    // `ChunkMap.isChunkTracked`.
                    if view.contains(pos, true) && !chunks.is_pending(pos) {
                        for packet in packets {
                            player.connection.sink.send_shared(packet);
                        }
                    }
                }
            }
            self.per_chunk.clear();
        }
        // `sendBlockUpdated` of block entities: their data to the players tracking the chunk.
        let mut sent: Vec<vanillite_base::BlockPos> = Vec::new();
        for pos in output.block_entity_updates.drain(..) {
            if sent.contains(&pos) {
                continue;
            }
            sent.push(pos);
            let Some(entity) = level.block_entity(pos) else {
                continue;
            };
            let state = level.block(pos);
            let kind = state.block().info().block_entity;
            if kind == u16::MAX {
                continue;
            }
            let mut nbt = Vec::new();
            match vanillite_game::level::block_entity::update_tag(entity, state) {
                Some(tag)
                    if vanillite_nbt::write::write_network_compound(&tag, &mut nbt).is_ok() => {}
                _ => nbt = vec![0],
            }
            let packet = SharedPacket::of(
                &vanillite_protocol::packets::play::clientbound::BlockEntityData {
                    pos: vanillite_protocol::types::BlockPos {
                        x: pos.x,
                        y: pos.y,
                        z: pos.z,
                    },
                    kind: i32::from(kind),
                    nbt,
                },
            );
            let chunk = pos.chunk();
            for (_, player) in entities.players.iter() {
                let chunks = &player.connection.chunks;
                if chunks
                    .view()
                    .is_some_and(|v| v.contains(chunk, true) && !chunks.is_pending(chunk))
                {
                    player.connection.sink.send_shared(&packet);
                }
            }
        }
        for event in output.block_events.drain(..) {
            let packet = block_event(event.pos, event.block.0, event.a, event.b);
            send_near(entities, event.pos, None, &packet);
        }
        for explosion in output.explosions.drain(..) {
            send_explosion(entities, &explosion);
        }
        for sound in output.sounds.drain(..) {
            let packet = sound_packet(&sound);
            // `SoundEvent.getRange(volume)`.
            let range = if sound.volume > 1.0 {
                16.0 * f64::from(sound.volume)
            } else {
                16.0
            };
            send_within(
                entities,
                (sound.x, sound.y, sound.z),
                range,
                sound.except,
                &packet,
            );
        }
        for (pos, range, packet) in output.nearby_packets.drain(..) {
            send_within(entities, (pos.x, pos.y, pos.z), range, None, &packet);
        }
        // Hand the emptied buffers back to the level.
        let reuse = level.output();
        if reuse.changed.is_empty()
            && reuse.block_events.is_empty()
            && reuse.level_events.is_empty()
            && reuse.sounds.is_empty()
            && reuse.particles.is_empty()
            && reuse.explosions.is_empty()
            && reuse.global_level_events.is_empty()
        {
            output.changed.clear();
            *reuse = output;
        }
        merge_changes(&mut level.output().changed, later);
    }

    /// Light updates first, then the changed blocks of each section, per chunk.
    fn encode_chunks(&mut self, level: &Level, output: &mut LevelOutput) {
        for (pos, change) in self.light.drain(..) {
            let Some(chunk) = level.chunks.chunk(pos) else {
                continue;
            };
            let mut body = Vec::with_capacity(1024);
            write_varint(&mut body, ids::LIGHT_UPDATE);
            vanillite_chunk::packet::encode_light_update_filtered(
                &mut body,
                pos.x,
                pos.z,
                chunk.chunk.light(),
                &change.sky,
                &change.block,
            );
            self.per_chunk
                .entry(pos)
                .or_default()
                .push(Arc::new(SharedPacket::new(body)));
        }
        let mut sections: Vec<_> = output.changed.drain().collect();
        // Deterministic order for the clients: by section.
        sections.sort_unstable_by_key(|(s, _)| (s.x, s.z, s.y));
        for (section, positions) in sections {
            let chunk = section.chunk();
            if level.chunks.chunk(chunk).is_none() || positions.is_empty() {
                continue;
            }
            let packet = if let [pos] = positions.positions()[..] {
                block_update(pos, level.block(pos).0)
            } else {
                section_blocks_update(section, positions.positions(), |pos| level.block(pos).0)
            };
            self.per_chunk.entry(chunk).or_default().push(packet);
        }
    }
}

/// Adds the changed positions of `from` to `into`, each position once per section.
fn merge_changes(into: &mut Changes, from: Changes) {
    if into.is_empty() {
        *into = from;
        return;
    }
    for (section, positions) in from {
        let list = into.entry(section).or_default();
        for &pos in positions.positions() {
            list.insert(pos);
        }
    }
}

/// Sends `packet` to the players within [`EVENT_RANGE`] of `pos`, except `except`.
fn send_near(
    entities: &Entities,
    pos: BlockPos,
    except: Option<vanillite_world::entity::EntityId>,
    packet: &Arc<SharedPacket>,
) {
    let at = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    send_within(entities, at, EVENT_RANGE, except, packet);
}

/// `ServerLevel.globalLevelEvent`: with `global_sound_events` every player of the server hears
/// it, players of this world from at most 32 blocks away in its direction, the others at their
/// own position; without, it is an ordinary level event.
fn global_level_event(
    entities: &Entities,
    global: &crate::global::Global,
    world: crate::world::WorldId,
    global_sounds: bool,
    (event, pos, data): (i32, BlockPos, i32),
) {
    if !global_sounds {
        let packet = level_event(event, pos, data);
        send_near(entities, pos, None, &packet);
        return;
    }
    let center = vanillite_base::Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    for (id, player) in entities.players.iter() {
        let Some(p) = entities.store.get(id).map(|e| e.pos()) else {
            continue;
        };
        let at = if p.distance_to_sqr(center) < 32.0 * 32.0 {
            center
        } else {
            p + (center - p).normalize() * 32.0
        };
        let at = BlockPos::containing(at.x, at.y, at.z);
        player
            .connection
            .sink
            .send_shared(&Arc::new(level_event_packet(event, at, data, true)));
    }
    for index in global.dimensions.worlds().map(|w| w.0) {
        let other = crate::world::WorldId(index);
        if other == world {
            continue;
        }
        global.run_in_world(other, move |w| {
            let entities = w.entities();
            for (id, player) in entities.players.iter() {
                if let Some(p) = entities.store.get(id).map(|e| e.pos()) {
                    let at = BlockPos::containing(p.x, p.y, p.z);
                    player
                        .connection
                        .sink
                        .send_shared(&Arc::new(level_event_packet(event, at, data, true)));
                }
            }
        });
    }
}

/// `ServerLevel.sendParticles`: to the players whose block centre is closer than 32 blocks
/// (512 when the packet overrides the client's limiter).
fn send_particles(
    entities: &Entities,
    particles: &vanillite_game::level::particles::LevelParticles,
) {
    let packet = SharedPacket::of(particles);
    let range = if particles.override_limiter {
        512.0
    } else {
        32.0
    };
    for (id, player) in entities.players.iter() {
        let Some(entity) = entities.store.get(id) else {
            continue;
        };
        // `BlockPos.closerToCenterThan`.
        let p = entity.pos();
        let (dx, dy, dz) = (
            p.x.floor() + 0.5 - particles.x,
            p.y.floor() + 0.5 - particles.y,
            p.z.floor() + 0.5 - particles.z,
        );
        if dx * dx + dy * dy + dz * dz < range * range {
            player.connection.sink.send_shared(&packet);
        }
    }
}

/// `PlayerList.broadcast(except, x, y, z, range, dimension, packet)`.
fn send_within(
    entities: &Entities,
    (x, y, z): (f64, f64, f64),
    range: f64,
    except: Option<vanillite_world::entity::EntityId>,
    packet: &Arc<SharedPacket>,
) {
    for (id, player) in entities.players.iter() {
        if Some(id) == except {
            continue;
        }
        let Some(entity) = entities.store.get(id) else {
            continue;
        };
        let p = entity.pos();
        let (dx, dy, dz) = (x - p.x, y - p.y, z - p.z);
        if dx * dx + dy * dy + dz * dz < range * range {
            player.connection.sink.send_shared(packet);
        }
    }
}

/// `BlockPos.asLong`.
fn block_pos_long(pos: BlockPos) -> i64 {
    ((i64::from(pos.x) & 0x3FF_FFFF) << 38)
        | ((i64::from(pos.z) & 0x3FF_FFFF) << 12)
        | (i64::from(pos.y) & 0xFFF)
}

/// `SectionPos.asLong`.
fn section_pos_long(pos: SectionPos) -> i64 {
    ((i64::from(pos.x) & 0x3F_FFFF) << 42)
        | ((i64::from(pos.z) & 0x3F_FFFF) << 20)
        | (i64::from(pos.y) & 0xF_FFFF)
}

/// `block_update`.
fn block_update(pos: BlockPos, state: u16) -> Arc<SharedPacket> {
    let mut body = Vec::with_capacity(16);
    write_varint(&mut body, ids::BLOCK_UPDATE);
    body.extend_from_slice(&block_pos_long(pos).to_be_bytes());
    write_varint(&mut body, i32::from(state));
    Arc::new(SharedPacket::new(body))
}

/// `section_blocks_update`.
fn section_blocks_update(
    section: SectionPos,
    positions: &[BlockPos],
    state: impl Fn(BlockPos) -> u16,
) -> Arc<SharedPacket> {
    let mut body = Vec::with_capacity(16 + positions.len() * 4);
    write_varint(&mut body, ids::SECTION_BLOCKS_UPDATE);
    body.extend_from_slice(&section_pos_long(section).to_be_bytes());
    write_varint(
        &mut body,
        i32::try_from(positions.len()).unwrap_or(i32::MAX),
    );
    for &pos in positions {
        // `SectionPos.sectionRelativePos`.
        let relative = ((pos.x & 15) << 8) | ((pos.z & 15) << 4) | (pos.y & 15);
        write_varlong(
            &mut body,
            (i64::from(state(pos)) << 12) | i64::from(relative),
        );
    }
    Arc::new(SharedPacket::new(body))
}

/// `block_event`: the block's registry id.
fn block_event(pos: BlockPos, block: u16, a: i32, b: i32) -> Arc<SharedPacket> {
    let mut body = Vec::with_capacity(16);
    write_varint(&mut body, ids::BLOCK_EVENT);
    body.extend_from_slice(&block_pos_long(pos).to_be_bytes());
    body.push(a as u8);
    body.push(b as u8);
    write_varint(&mut body, i32::from(block));
    Arc::new(SharedPacket::new(body))
}

/// `ServerLevel.explode`: the explosion packet to every player within 64 blocks of the centre,
/// each with its own knockback.
fn send_explosion(
    entities: &Entities,
    explosion: &vanillite_game::level::explosion::ExplosionEvent,
) {
    let center = explosion.center;
    for (id, player) in entities.players.iter() {
        let Some(entity) = entities.store.get(id) else {
            continue;
        };
        if entity.pos().distance_to_sqr(center) >= 4096.0 {
            continue;
        }
        let knockback = explosion
            .hit_players
            .iter()
            .find(|(hit, _)| *hit == id)
            .map(|(_, k)| *k);
        player
            .connection
            .sink
            .send_shared(&explode_packet(explosion, knockback));
    }
}

/// `explode` (`ClientboundExplodePacket`) with Vanilla's default particles and sound.
fn explode_packet(
    explosion: &vanillite_game::level::explosion::ExplosionEvent,
    knockback: Option<vanillite_base::Vec3>,
) -> Arc<SharedPacket> {
    use vanillite_data::registry::{PARTICLE_TYPE, SOUND_EVENT};
    let particle = |name: &str| i32::from(PARTICLE_TYPE.id(name).unwrap_or(0));
    let mut body = Vec::with_capacity(96);
    write_varint(&mut body, ids::EXPLODE);
    for v in [explosion.center.x, explosion.center.y, explosion.center.z] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&explosion.radius.to_be_bytes());
    body.extend_from_slice(&explosion.block_count.to_be_bytes());
    match knockback {
        Some(k) => {
            body.push(1);
            for v in [k.x, k.y, k.z] {
                body.extend_from_slice(&v.to_be_bytes());
            }
        }
        None => body.push(0),
    }
    if let Some(look) = &explosion.look {
        // An enchantment's `explode`: its particles and sound.
        let main = if explosion.small {
            &look.small
        } else {
            &look.large
        };
        write_varint(&mut body, main.kind);
        body.extend_from_slice(&main.options);
        match SOUND_EVENT.id(&look.sound) {
            Some(id) => write_varint(&mut body, i32::from(id) + 1),
            None => {
                // A direct `SoundEvent` without a fixed range.
                write_varint(&mut body, 0);
                write_varint(&mut body, i32::try_from(look.sound.len()).unwrap_or(0));
                body.extend_from_slice(look.sound.as_bytes());
                body.push(0);
            }
        }
        write_varint(
            &mut body,
            i32::try_from(look.block_particles.len()).unwrap_or(0),
        );
        for (p, scaling, speed, weight) in &look.block_particles {
            write_varint(&mut body, p.kind);
            body.extend_from_slice(&p.options);
            body.extend_from_slice(&scaling.to_be_bytes());
            body.extend_from_slice(&speed.to_be_bytes());
            write_varint(&mut body, *weight);
        }
        body.push(1);
        return Arc::new(SharedPacket::new(body));
    }
    // `isSmall() ? EXPLOSION : EXPLOSION_EMITTER` (simple particles without options).
    let main = match (explosion.wind_sound.is_some(), explosion.small) {
        (true, true) => "minecraft:gust_emitter_small",
        (true, false) => "minecraft:gust_emitter_large",
        (false, true) => "minecraft:explosion",
        (false, false) => "minecraft:explosion_emitter",
    };
    write_varint(&mut body, particle(main));
    // `SoundEvents.GENERIC_EXPLODE` (or the wind burst) as a registry holder.
    let sound = SOUND_EVENT
        .id(explosion
            .wind_sound
            .unwrap_or("minecraft:entity.generic.explode"))
        .unwrap_or(0);
    write_varint(&mut body, i32::from(sound) + 1);
    if explosion.wind_sound.is_some() {
        // Wind charges have no block particles (`WeightedList.of()`).
        write_varint(&mut body, 0);
    } else {
        // `DEFAULT_EXPLOSION_BLOCK_PARTICLES`: poof (0.5, 1.0) and smoke (1.0, 1.0), weight 1.
        write_varint(&mut body, 2);
        for (name, scaling) in [("minecraft:poof", 0.5_f32), ("minecraft:smoke", 1.0_f32)] {
            write_varint(&mut body, particle(name));
            body.extend_from_slice(&scaling.to_be_bytes());
            body.extend_from_slice(&1.0_f32.to_be_bytes());
            write_varint(&mut body, 1);
        }
    }
    // `playSound`: the source is not silent.
    body.push(1);
    Arc::new(SharedPacket::new(body))
}

/// `sound`: a registry sound event at a position.
fn sound_packet(sound: &vanillite_game::level::Sound) -> Arc<SharedPacket> {
    let mut body = Vec::with_capacity(40);
    write_varint(&mut body, ids::SOUND);
    write_varint(&mut body, i32::from(sound.sound) + 1);
    write_varint(&mut body, i32::from(sound.source));
    for v in [sound.x, sound.y, sound.z] {
        body.extend_from_slice(&((v * 8.0) as i32).to_be_bytes());
    }
    body.extend_from_slice(&sound.volume.to_be_bytes());
    body.extend_from_slice(&sound.pitch.to_be_bytes());
    body.extend_from_slice(&sound.seed.to_be_bytes());
    Arc::new(SharedPacket::new(body))
}

/// `level_event` (not global).
fn level_event(event: i32, pos: BlockPos, data: i32) -> Arc<SharedPacket> {
    Arc::new(level_event_packet(event, pos, data, false))
}

/// `ClientboundLevelEventPacket`; `global` events are heard everywhere.
pub(crate) fn level_event_packet(
    event: i32,
    pos: BlockPos,
    data: i32,
    global: bool,
) -> SharedPacket {
    let mut body = Vec::with_capacity(20);
    write_varint(&mut body, ids::LEVEL_EVENT);
    body.extend_from_slice(&event.to_be_bytes());
    body.extend_from_slice(&block_pos_long(pos).to_be_bytes());
    body.extend_from_slice(&data.to_be_bytes());
    body.push(u8::from(global));
    SharedPacket::new(body)
}
