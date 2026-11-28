//! Encoding of the entity tracker's updates into play packets: each update is encoded once and
//! the same packets go to all its viewers ([`TrackerOutput::fan_out`]).
//!
//! [`TrackerOutput::fan_out`]: vanillite_world::tracking::TrackerOutput::fan_out

use std::sync::Arc;

use vanillite_base::Vec3;
use vanillite_data::entity::ids;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::codec::write_varint;
use vanillite_protocol::packets::play::clientbound::{
    AddEntity, BundleDelimiter, EntityPositionSync, MoveEntityPos, MoveEntityPosRot, MoveEntityRot,
    PositionPath, ProjectilePower, RemoveEntities, RotateHead, SetEntityData, SetEntityMotion,
    SetEquipment, VecDelta,
};
use vanillite_protocol::types::{self, LpVec3, RawBytes, Uuid, VarIntList};
use vanillite_world::entity::{EntityId, EntityRef, Pose};
use vanillite_world::tracking::{EntityUpdate, Message};

use super::MainHand;
use crate::entities::Entities;
use crate::item::{EquipmentSlot, ItemStack};
use vanillite_protocol::codec::Encode;

/// The packets of one update, shared by all its viewers.
pub type Encoded = Vec<Arc<SharedPacket>>;

/// Entity data ids (`EntityDataAccessor` ids of 26.3).
mod data_id {
    pub const SHARED_FLAGS: u8 = 0;
    pub const CUSTOM_NAME: u8 = 2;
    pub const AIR_SUPPLY: u8 = 1;
    pub const POSE: u8 = 6;
    pub const TICKS_FROZEN: u8 = 7;
    pub const ITEM_ENTITY_ITEM: u8 = 8;
    pub const HEALTH: u8 = 9;
    pub const EFFECT_PARTICLES: u8 = 10;
    pub const EFFECT_AMBIENCE: u8 = 11;
    pub const PLAYER_MAIN_HAND: u8 = 15;
    pub const TNT_FUSE: u8 = 8;
    pub const TNT_BLOCK_STATE: u8 = 9;
    pub const PLAYER_MODE_CUSTOMISATION: u8 = 16;
    pub const PLAYER_ABSORPTION: u8 = 17;
    pub const PLAYER_SCORE: u8 = 18;
    pub const PLAYER_SHOULDER_LEFT: u8 = 19;
    pub const PLAYER_SHOULDER_RIGHT: u8 = 20;
    pub const LIVING_FLAGS: u8 = 8;
    pub const MOB_FLAGS: u8 = 15;
    pub const KIND_16: u8 = 16;
    pub const KIND_17: u8 = 17;
    pub const KIND_18: u8 = 18;
    pub const KIND_19: u8 = 19;
    pub const KIND_20: u8 = 20;
    pub const ORB_VALUE: u8 = 8;
    pub const ARROW_FLAGS: u8 = 8;
    pub const ARROW_PIERCE_LEVEL: u8 = 9;
    pub const ARROW_IN_GROUND: u8 = 10;
}

/// Entity data serializer ids (`EntityDataSerializers` registration order of 26.3).
#[allow(dead_code)]
pub mod serializer {
    pub const BYTE: i32 = 0;
    pub const INT: i32 = 1;
    pub const LONG: i32 = 2;
    pub const FLOAT: i32 = 3;
    pub const STRING: i32 = 4;
    pub const COMPONENT: i32 = 5;
    pub const OPTIONAL_COMPONENT: i32 = 6;
    pub const ITEM_STACK: i32 = 7;
    pub const BOOLEAN: i32 = 8;
    pub const ROTATIONS: i32 = 9;
    pub const BLOCK_POS: i32 = 10;
    pub const OPTIONAL_BLOCK_POS: i32 = 11;
    pub const DIRECTION: i32 = 12;
    pub const OPTIONAL_LIVING_ENTITY_REFERENCE: i32 = 13;
    pub const BLOCK_STATE: i32 = 14;
    pub const OPTIONAL_BLOCK_STATE: i32 = 15;
    pub const PARTICLE: i32 = 16;
    pub const PARTICLES: i32 = 17;
    pub const VILLAGER_DATA: i32 = 18;
    pub const OPTIONAL_UNSIGNED_INT: i32 = 19;
    pub const POSE: i32 = 20;
    pub const CAT_VARIANT: i32 = 21;
    pub const CAT_SOUND_VARIANT: i32 = 22;
    pub const COW_VARIANT: i32 = 23;
    pub const COW_SOUND_VARIANT: i32 = 24;
    pub const WOLF_VARIANT: i32 = 25;
    pub const WOLF_SOUND_VARIANT: i32 = 26;
    pub const FROG_VARIANT: i32 = 27;
    pub const PIG_VARIANT: i32 = 28;
    pub const PIG_SOUND_VARIANT: i32 = 29;
    pub const CHICKEN_VARIANT: i32 = 30;
    pub const CHICKEN_SOUND_VARIANT: i32 = 31;
    pub const ZOMBIE_NAUTILUS_VARIANT: i32 = 32;
    pub const OPTIONAL_GLOBAL_POS: i32 = 33;
    pub const PAINTING_VARIANT: i32 = 34;
    pub const SNIFFER_STATE: i32 = 35;
    pub const ARMADILLO_STATE: i32 = 36;
    pub const COPPER_GOLEM_STATE: i32 = 37;
    pub const WEATHERING_COPPER_STATE: i32 = 38;
    pub const VECTOR3: i32 = 39;
    pub const QUATERNION: i32 = 40;
    pub const RESOLVABLE_PROFILE: i32 = 41;
    pub const HUMANOID_ARM: i32 = 42;
    pub const DYE_COLOR: i32 = 43;
}

fn wire_vec(v: Vec3) -> types::Vec3 {
    types::Vec3 {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}

/// `Pose.id()`.
pub fn pose_id(pose: Pose) -> i32 {
    // `Pose` is `repr(u8)` in protocol order.
    #[allow(clippy::as_conversions)]
    let id = pose as u8;
    i32::from(id)
}

/// The synched data of a minecart (`VehicleEntity`, `AbstractMinecart` and the kinds).
fn minecart_data(out: &mut Vec<u8>, m: &crate::minecart::MinecartData, all: bool) {
    use crate::minecart::MinecartKind;
    if all || m.hurt_time != 0 {
        item(out, 8, serializer::INT, |o| write_varint(o, m.hurt_time));
    }
    if all || m.hurt_dir != 1 {
        item(out, 9, serializer::INT, |o| write_varint(o, m.hurt_dir));
    }
    if all || m.damage != 0.0 {
        item(out, 10, serializer::FLOAT, |o| {
            o.extend_from_slice(&m.damage.to_be_bytes());
        });
    }
    if all || m.display_state.is_some() {
        item(out, 11, serializer::OPTIONAL_BLOCK_STATE, |o| {
            write_varint(o, m.display_state.map_or(0, |s| i32::from(s.0)));
        });
    }
    if all || m.display_offset != m.kind.default_display_offset() {
        item(out, 12, serializer::INT, |o| {
            write_varint(o, m.display_offset)
        });
    }
    match m.kind {
        MinecartKind::Furnace => {
            if all || m.has_fuel {
                item(out, 13, serializer::BOOLEAN, |o| {
                    o.push(u8::from(m.has_fuel))
                });
            }
        }
        MinecartKind::CommandBlock => {
            let command = &m.command.command;
            if all || !command.is_empty() {
                item(out, 13, serializer::STRING, |o| {
                    write_varint(o, i32::try_from(command.len()).unwrap_or(0));
                    o.extend_from_slice(command.as_bytes());
                });
            }
            let output = m
                .command
                .last_output
                .as_ref()
                .and_then(|c| crate::mob::nbt::tag_component(&c.to_nbt()));
            if all || output.is_some() {
                item(out, 14, serializer::COMPONENT, |o| match &output {
                    Some(c) => o.extend_from_slice(c),
                    // `CommonComponents.EMPTY`: an empty literal.
                    None => {
                        let empty = vanillite_nbt::Tag::String(String::new());
                        let _ = vanillite_nbt::write::write_network(&empty, o);
                    }
                });
            }
        }
        _ => {}
    }
}

/// Appends one data item.
fn item(out: &mut Vec<u8>, id: u8, serializer: i32, value: impl FnOnce(&mut Vec<u8>)) {
    #[cfg(test)]
    layout_tests::record(id, serializer);
    out.push(id);
    write_varint(out, serializer);
    value(out);
}

/// The synched data of an entity: all values (`all`) or only those that differ from their
/// defaults (`getNonDefaultValues`, for pairing). Ends with the `0xFF` terminator; `None` when
/// nothing is to be sent.
fn entity_data(entity: &EntityRef<'_>, entities: &Entities, all: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(16);
    // `setSharedFlagOnFire`: clients also see the fire of `HasVisualFire`.
    let visual_fire = entity
        .flags()
        .contains(vanillite_world::entity::EntityFlags::VISUAL_FIRE);
    let flags = entity.flags().shared_byte() | u8::from(visual_fire);
    if all || flags != 0 {
        item(&mut out, data_id::SHARED_FLAGS, serializer::BYTE, |o| {
            o.push(flags);
        });
    }
    let player = if entity.kind() == ids::PLAYER {
        entities.player(entity.id())
    } else {
        None
    };
    if let Some(player) = player {
        let air = player.survival.air;
        if all || air != crate::survival::player::MAX_AIR {
            item(&mut out, data_id::AIR_SUPPLY, serializer::INT, |o| {
                write_varint(o, air);
            });
        }
    }
    let name = entities
        .mob(entity.id())
        .and_then(|m| m.custom_name.as_deref())
        .or_else(|| {
            entities
                .boat(entity.id())
                .and_then(|b| b.custom_name.as_deref())
        })
        .or_else(|| {
            entities
                .minecart(entity.id())
                .and_then(|m| m.custom_name.as_deref())
        })
        .or_else(|| crate::decoration::custom_name(entities, entity.id()))
        .or_else(|| {
            entities
                .cloud(entity.id())
                .and_then(|c| c.custom_name.as_deref())
        });
    if all || name.is_some() {
        item(
            &mut out,
            data_id::CUSTOM_NAME,
            serializer::OPTIONAL_COMPONENT,
            |o| {
                o.push(u8::from(name.is_some()));
                if let Some(n) = name {
                    o.extend_from_slice(n);
                }
            },
        );
    }
    let name_visible = crate::decoration::name_visible(entities, entity.id())
        || entity
            .flags()
            .contains(vanillite_world::entity::EntityFlags::NAME_VISIBLE);
    if all || name_visible {
        item(&mut out, 3, serializer::BOOLEAN, |o| {
            o.push(u8::from(name_visible));
        });
    }
    let silent = entities
        .decor(entity.id())
        .map(|d| d.silent)
        .or_else(|| entities.mob(entity.id()).map(|m| m.silent))
        .unwrap_or(false);
    if all || silent {
        item(&mut out, 4, serializer::BOOLEAN, |o| {
            o.push(u8::from(silent))
        });
    }
    let no_gravity = entity
        .flags()
        .contains(vanillite_world::entity::EntityFlags::NO_GRAVITY);
    if all || no_gravity {
        item(&mut out, 5, serializer::BOOLEAN, |o| {
            o.push(u8::from(no_gravity));
        });
    }
    let pose = entity.pose();
    if all || pose != Pose::Standing {
        item(&mut out, data_id::POSE, serializer::POSE, |o| {
            write_varint(o, pose_id(pose));
        });
    }
    let frozen = entities.mob(entity.id()).map_or(0, |m| m.ticks_frozen);
    if all || frozen != 0 {
        item(&mut out, data_id::TICKS_FROZEN, serializer::INT, |o| {
            write_varint(o, frozen);
        });
    }
    if let Some(player) = player {
        let living = crate::player::using::living_flags(player.survival.using_item);
        if all || living != 0 {
            item(&mut out, data_id::LIVING_FLAGS, serializer::BYTE, |o| {
                o.push(living);
            });
        }
        // `LivingEntity` defines the health as 1.0 and sets it to the maximum afterwards.
        let health = player.survival.health;
        if all || health != 1.0 {
            item(&mut out, data_id::HEALTH, serializer::FLOAT, |o| {
                o.extend_from_slice(&health.to_be_bytes());
            });
        }
        effect_data(&mut out, &player.survival.effects, all);
        arrow_count(&mut out, player.survival.arrows.count, all);
        DataWriter::new(&mut out, all).optional_block_pos(14, player.survival.sleeping_pos);
        let hand = player.client.main_hand;
        if all || hand != MainHand::Right {
            item(
                &mut out,
                data_id::PLAYER_MAIN_HAND,
                serializer::HUMANOID_ARM,
                |o| {
                    write_varint(o, i32::from(hand == MainHand::Right));
                },
            );
        }
        let parts = player.client.model_customisation;
        if all || parts != 0 {
            item(
                &mut out,
                data_id::PLAYER_MODE_CUSTOMISATION,
                serializer::BYTE,
                |o| o.push(parts),
            );
        }
        let absorption = player.survival.absorption;
        if all || absorption != 0.0 {
            item(
                &mut out,
                data_id::PLAYER_ABSORPTION,
                serializer::FLOAT,
                |o| o.extend_from_slice(&absorption.to_be_bytes()),
            );
        }
        let score = player.survival.score;
        if all || score != 0 {
            item(&mut out, data_id::PLAYER_SCORE, serializer::INT, |o| {
                write_varint(o, score);
            });
        }
        let shoulders = &player.survival.shoulders;
        for (id, variant) in [
            (data_id::PLAYER_SHOULDER_LEFT, shoulders.parrot_left()),
            (data_id::PLAYER_SHOULDER_RIGHT, shoulders.parrot_right()),
        ] {
            if all || variant.is_some() {
                item(&mut out, id, serializer::OPTIONAL_UNSIGNED_INT, |o| {
                    write_varint(o, variant.map_or(0, |v| v.saturating_add(1)));
                });
            }
        }
    }
    if let Some(mob) = entities.mob(entity.id()) {
        let arrows = entities
            .stuck_arrows
            .get(&entity.id())
            .map_or(0, |a| a.count);
        mob_data(&mut out, mob, arrows, all);
    }
    if let Some(special) = entities.special(entity.id()) {
        crate::mob::boss::special::entity_data(&mut out, special, all);
    }
    if let Some(d) = entities.decor(entity.id()) {
        crate::decoration::entity_data(&mut DataWriter { out: &mut out, all }, d);
    }
    if let Some(b) = entities.boat(entity.id()) {
        if all || b.hurt_time != 0 {
            item(&mut out, 8, serializer::INT, |o| {
                write_varint(o, b.hurt_time)
            });
        }
        if all || b.hurt_dir != 1 {
            item(&mut out, 9, serializer::INT, |o| {
                write_varint(o, b.hurt_dir)
            });
        }
        if all || b.damage != 0.0 {
            item(&mut out, 10, serializer::FLOAT, |o| {
                o.extend_from_slice(&b.damage.to_be_bytes());
            });
        }
        if all || b.paddles.0 {
            item(&mut out, 11, serializer::BOOLEAN, |o| {
                o.push(u8::from(b.paddles.0))
            });
        }
        if all || b.paddles.1 {
            item(&mut out, 12, serializer::BOOLEAN, |o| {
                o.push(u8::from(b.paddles.1))
            });
        }
        if all || b.bubble_time != 0 {
            item(&mut out, 13, serializer::INT, |o| {
                write_varint(o, b.bubble_time)
            });
        }
    }
    if let Some(m) = entities.minecart(entity.id()) {
        minecart_data(&mut out, m, all);
    }
    if let Some(orb) = entities.experience_orb(entity.id())
        && (all || orb.value != 0)
    {
        item(&mut out, data_id::ORB_VALUE, serializer::INT, |o| {
            write_varint(o, orb.value);
        });
    }
    if let Some(d) = entities.missile(entity.id()) {
        crate::mob::fam::missile_encode(&mut DataWriter { out: &mut out, all }, d);
    }
    if let Some(arrow) = entities.arrow(entity.id()) {
        if all || arrow.crit {
            item(&mut out, data_id::ARROW_FLAGS, serializer::BYTE, |o| {
                o.push(u8::from(arrow.crit));
            });
        }
        if all || arrow.pierce_level != 0 {
            item(
                &mut out,
                data_id::ARROW_PIERCE_LEVEL,
                serializer::BYTE,
                |o| {
                    o.push(arrow.pierce_level);
                },
            );
        }
        if all || arrow.in_ground {
            item(
                &mut out,
                data_id::ARROW_IN_GROUND,
                serializer::BOOLEAN,
                |o| {
                    o.push(u8::from(arrow.in_ground));
                },
            );
        }
        // `Arrow.ID_EFFECT_COLOR` (11); after 600 ticks in the ground it goes back to -1.
        if entity.kind() == ids::ARROW {
            let color = arrow.color();
            if all || color != -1 || arrow.in_ground_time >= 600 {
                item(&mut out, 11, serializer::INT, |o| write_varint(o, color));
            }
        }
    }
    if let Some(cloud) = entities.cloud(entity.id()) {
        if all || cloud.radius != 3.0 {
            item(&mut out, 8, serializer::FLOAT, |o| {
                o.extend_from_slice(&cloud.radius.to_be_bytes());
            });
        }
        if all || cloud.waiting {
            item(&mut out, 9, serializer::BOOLEAN, |o| {
                o.push(u8::from(cloud.waiting))
            });
        }
        let particle_id = |name: &str| {
            vanillite_data::registry::PARTICLE_TYPE
                .id(name)
                .map_or(0, i32::from)
        };
        // `PowerParticleOption` of `dragon_breath`, else `ColorParticleOption` of
        // `entity_effect`; the default is white.
        let color = cloud.particle_color();
        if cloud.dragon_breath {
            item(&mut out, 10, serializer::PARTICLE, |o| {
                write_varint(o, particle_id("minecraft:dragon_breath"));
                o.extend_from_slice(&1.0_f32.to_be_bytes());
            });
        } else if all || color != -1 {
            item(&mut out, 10, serializer::PARTICLE, |o| {
                write_varint(o, particle_id("minecraft:entity_effect"));
                o.extend_from_slice(&color.to_be_bytes());
            });
        }
    }
    if entity.kind() == ids::TNT
        && let Some(tnt) = entities.primed_tnt(entity.id())
    {
        if all || tnt.fuse != 80 {
            item(&mut out, data_id::TNT_FUSE, serializer::INT, |o| {
                write_varint(o, tnt.fuse);
            });
        }
        let default = vanillite_data::block::ids::TNT.default_state();
        if all || tnt.state != default {
            item(
                &mut out,
                data_id::TNT_BLOCK_STATE,
                serializer::BLOCK_STATE,
                |o| {
                    write_varint(o, i32::from(tnt.state.0));
                },
            );
        }
    }
    if entity.kind() == ids::ITEM
        && let Some(data) = entities.item(entity.id())
        && (all || !data.stack.is_empty())
    {
        item(
            &mut out,
            data_id::ITEM_ENTITY_ITEM,
            serializer::ITEM_STACK,
            |o| {
                data.stack.encode(o);
            },
        );
    }
    if out.is_empty() {
        return None;
    }
    out.push(0xFF);
    Some(out)
}

/// Every synched data item of an entity, as a `set_entity_data` of all values sends them
/// (ending with the `0xFF` terminator); `None` for an unknown entity.
pub fn all_entity_data(
    entities: &Entities,
    id: vanillite_world::entity::EntityId,
) -> Option<Vec<u8>> {
    entity_data(&entities.store.get(id)?, entities, true)
}

/// Writes synched data items of a mob kind: every value (`all`, updates) or only those that
/// differ from their defaults (pairing).
#[derive(Debug)]
pub struct DataWriter<'a> {
    out: &'a mut Vec<u8>,
    all: bool,
}

impl<'a> DataWriter<'a> {
    /// The buffer and whether every value is written (encoders of their own).
    pub fn parts(&mut self) -> (&mut Vec<u8>, bool) {
        (self.out, self.all)
    }

    /// A writer appending to `out` (`all`: every value, else only non-default ones).
    pub fn new(out: &'a mut Vec<u8>, all: bool) -> DataWriter<'a> {
        DataWriter { out, all }
    }

    /// A writer of every value into `out` (tests of the kind encoders).
    #[cfg(test)]
    pub fn all_for_test(out: &'a mut Vec<u8>) -> DataWriter<'a> {
        DataWriter { out, all: true }
    }

    /// An item with a custom encoding; `is_default` skips it when pairing.
    pub fn raw(
        &mut self,
        id: u8,
        serializer: i32,
        is_default: bool,
        value: impl FnOnce(&mut Vec<u8>),
    ) {
        if self.all || !is_default {
            item(self.out, id, serializer, value);
        }
    }

    pub fn byte(&mut self, id: u8, v: u8, default: u8) {
        self.raw(id, serializer::BYTE, v == default, |o| o.push(v));
    }

    pub fn boolean(&mut self, id: u8, v: bool, default: bool) {
        self.raw(id, serializer::BOOLEAN, v == default, |o| {
            o.push(u8::from(v))
        });
    }

    /// An `INT` (var int) item.
    pub fn int(&mut self, id: u8, v: i32, default: i32) {
        self.varint(id, serializer::INT, v, default);
    }

    /// A var int item of any serializer (registry holders, enums, `INT`).
    pub fn varint(&mut self, id: u8, serializer: i32, v: i32, default: i32) {
        self.raw(id, serializer, v == default, |o| write_varint(o, v));
    }

    pub fn float(&mut self, id: u8, v: f32, default: f32) {
        #[allow(clippy::float_cmp)]
        let is_default = v == default;
        self.raw(id, serializer::FLOAT, is_default, |o| {
            o.extend_from_slice(&v.to_be_bytes());
        });
    }

    /// An `ITEM_STACK` item.
    pub fn stack(&mut self, id: u8, v: &ItemStack) {
        self.raw(id, serializer::ITEM_STACK, v.is_empty(), |o| v.encode(o));
    }

    /// An `OPTIONAL_BLOCK_POS` item.
    pub fn optional_block_pos(&mut self, id: u8, v: Option<vanillite_base::BlockPos>) {
        self.raw(id, serializer::OPTIONAL_BLOCK_POS, v.is_none(), |o| {
            o.push(u8::from(v.is_some()));
            if let Some(p) = v {
                o.extend_from_slice(&p.as_long().to_be_bytes());
            }
        });
    }

    /// An `OPTIONAL_LIVING_ENTITY_REFERENCE` item (a UUID).
    pub fn optional_uuid(&mut self, id: u8, v: Option<u128>) {
        self.raw(
            id,
            serializer::OPTIONAL_LIVING_ENTITY_REFERENCE,
            v.is_none(),
            |o| {
                o.push(u8::from(v.is_some()));
                if let Some(u) = v {
                    o.extend_from_slice(&u.to_be_bytes());
                }
            },
        );
    }
}

/// `DATA_ARROW_COUNT_ID` of a living entity.
fn arrow_count(out: &mut Vec<u8>, count: u16, all: bool) {
    if all || count != 0 {
        item(out, 12, serializer::INT, |o| {
            write_varint(o, i32::from(count))
        });
    }
}

/// `DATA_EFFECT_PARTICLES` and `DATA_EFFECT_AMBIENCE_ID` of a living entity.
fn effect_data(out: &mut Vec<u8>, fx: &crate::effect::ActiveEffects, all: bool) {
    let any_visible = fx.iter().any(|e| e.visible);
    if all || any_visible {
        item(out, data_id::EFFECT_PARTICLES, serializer::PARTICLES, |o| {
            fx.encode_particles(o);
        });
    }
    // `areAllEffectsAmbient` of no effects is true, but the data starts false and is only set
    // while effects are active.
    let ambient = !fx.is_empty() && fx.all_ambient();
    if all || ambient {
        item(out, data_id::EFFECT_AMBIENCE, serializer::BOOLEAN, |o| {
            o.push(u8::from(ambient));
        });
    }
}

/// The synched data of a mob after the entity's own: health, mob flags and the kind's values.
fn mob_data(out: &mut Vec<u8>, mob: &crate::mob::MobData, arrows: u16, all: bool) {
    use crate::mob::Kind;
    let living = crate::mob::kinds::living_flags(mob);
    if all || living != 0 {
        item(out, data_id::LIVING_FLAGS, serializer::BYTE, |o| {
            o.push(living)
        });
    }
    if all || mob.health != 1.0 {
        item(out, data_id::HEALTH, serializer::FLOAT, |o| {
            o.extend_from_slice(&mob.health.to_be_bytes());
        });
    }
    effect_data(out, &mob.effects, all);
    arrow_count(out, arrows, all);
    let flags =
        u8::from(mob.no_ai) | (u8::from(mob.left_handed) << 1) | (u8::from(mob.aggressive) << 2);
    // Armor stands and mannequins are no `Mob`: their own data starts at 15.
    if (all || flags != 0) && mob.kind.family() != crate::mob::Family::Decor {
        item(out, data_id::MOB_FLAGS, serializer::BYTE, |o| o.push(flags));
    }
    let boolean = |out: &mut Vec<u8>, id: u8, v: bool| {
        if all || v {
            item(out, id, serializer::BOOLEAN, |o| o.push(u8::from(v)));
        }
    };
    let variant = |out: &mut Vec<u8>, id: u8, serializer: i32, v: i32| {
        // The default variant is `temperate` (id 1 of `cold`, `temperate`, `warm`).
        if all || v != 1 {
            item(out, id, serializer, |o| write_varint(o, v));
        }
    };
    // `DATA_SOUND_VARIANT_ID` of pigs, cows and chickens; the default is `classic`.
    let sound = |out: &mut Vec<u8>, id: u8, serializer: i32| {
        let v = mob.sound_variant;
        if all || v != crate::mob::kinds::classic_sound_variant(mob.kind) {
            item(out, id, serializer, |o| write_varint(o, i32::from(v)));
        }
    };
    if mob.kind.family() != crate::mob::Family::Classic {
        crate::mob::fam::encode_data(&mut DataWriter { out, all }, mob);
        return;
    }
    match mob.kind {
        Kind::Zombie => {
            boolean(out, data_id::KIND_16, mob.baby);
            let converting =
                crate::mob::fam::undead::zombies::zs(mob).is_some_and(|z| z.converting);
            boolean(out, data_id::KIND_18, converting);
        }
        Kind::Creeper => {
            let c = crate::mob::fam::classic::creeper(mob);
            if all || c.swell_dir != -1 {
                item(out, data_id::KIND_16, serializer::INT, |o| {
                    write_varint(o, c.swell_dir);
                });
            }
            boolean(out, data_id::KIND_17, c.powered);
            boolean(out, data_id::KIND_18, c.ignited);
        }
        Kind::Spider => {
            let f = u8::from(mob.climbing);
            if all || f != 0 {
                item(out, data_id::KIND_16, serializer::BYTE, |o| o.push(f));
            }
        }
        Kind::Enderman => {
            let e = crate::mob::fam::classic::enderman(mob);
            if all || e.carried.is_some() {
                item(
                    out,
                    data_id::KIND_16,
                    serializer::OPTIONAL_BLOCK_STATE,
                    |o| {
                        write_varint(o, e.carried.map_or(0, |s| i32::from(s.0)));
                    },
                );
            }
            boolean(out, data_id::KIND_17, e.creepy);
        }
        Kind::Skeleton => boolean(
            out,
            data_id::KIND_16,
            crate::mob::fam::undead::skeletons::is_converting(mob),
        ),
        Kind::Cow | Kind::Pig | Kind::Sheep | Kind::Chicken => {
            boolean(out, data_id::KIND_16, mob.baby);
            match mob.kind {
                Kind::Sheep => {
                    let color = crate::mob::fam::classic::sheep(mob).color;
                    let wool = (color & 0x0F) | if mob.sheared { 0x10 } else { 0 };
                    if all || wool != 0 {
                        item(out, data_id::KIND_18, serializer::BYTE, |o| o.push(wool));
                    }
                }
                Kind::Pig => {
                    if all || mob.boost_total != 0 {
                        item(out, data_id::KIND_18, serializer::INT, |o| {
                            write_varint(o, mob.boost_total);
                        });
                    }
                    variant(out, data_id::KIND_19, serializer::PIG_VARIANT, mob.variant);
                    sound(out, data_id::KIND_20, serializer::PIG_SOUND_VARIANT);
                }
                Kind::Cow => {
                    variant(out, data_id::KIND_18, serializer::COW_VARIANT, mob.variant);
                    sound(out, data_id::KIND_19, serializer::COW_SOUND_VARIANT);
                }
                _ => {
                    variant(
                        out,
                        data_id::KIND_18,
                        serializer::CHICKEN_VARIANT,
                        mob.variant,
                    );
                    sound(out, data_id::KIND_19, serializer::CHICKEN_SOUND_VARIANT);
                }
            }
        }
        _ => {}
    }
}

/// `set_equipment` with every non-empty equipment slot of a player (pairing data).
fn equipment(entity: &EntityRef<'_>, entities: &Entities) -> Option<SetEquipment> {
    if let Some(mob) = entities.mob(entity.id()) {
        let mut slots = crate::mob::equipment::visible(mob);
        if mob.saddled {
            let saddle = vanillite_data::registry::ITEM.id("minecraft:saddle")?;
            slots.push((EquipmentSlot::Saddle.ordinal(), ItemStack::new(saddle, 1)));
        }
        return (!slots.is_empty()).then(|| SetEquipment {
            entity_id: entity.network_id().0,
            slots,
        });
    }
    let player = entities.player(entity.id())?;
    let slots: Vec<(u8, ItemStack)> = EQUIPMENT_SLOTS
        .iter()
        .map(|&slot| (slot.ordinal(), player.inventory.equipment(slot).clone()))
        .filter(|(_, stack)| !stack.is_empty())
        .collect();
    (!slots.is_empty()).then(|| SetEquipment {
        entity_id: entity.network_id().0,
        slots,
    })
}

/// The equipment slots of a player in Vanilla order.
pub const EQUIPMENT_SLOTS: [EquipmentSlot; 6] = [
    EquipmentSlot::MainHand,
    EquipmentSlot::OffHand,
    EquipmentSlot::Feet,
    EquipmentSlot::Legs,
    EquipmentSlot::Chest,
    EquipmentSlot::Head,
];

/// `AbstractHurtingProjectile.accelerationPower` of fireballs, wither skulls, dragon fireballs
/// and wind charges; `None` for other entities.
fn hurting_projectile_power(entities: &Entities, entity: EntityId) -> Option<f64> {
    use crate::mob::boss::special::SpecialData;
    use crate::mob::missile::MissileKind;
    if let Some(m) = entities.missile(entity) {
        return matches!(
            m.kind,
            MissileKind::SmallFireball
                | MissileKind::Fireball
                | MissileKind::WindCharge
                | MissileKind::BreezeWindCharge
        )
        .then_some(m.acceleration);
    }
    match entities.special(entity)? {
        SpecialData::WitherSkull(s) => Some(s.acceleration_power),
        SpecialData::DragonFireball(f) => Some(f.acceleration_power),
        _ => None,
    }
}

/// Encodes one tracker message. `None` skips it (the entity is gone and nothing is left to
/// send).
pub fn encode_update(message: &Message, entities: &Entities) -> Option<Encoded> {
    let id = message.network_id.0;
    let one = |p: Arc<SharedPacket>| Some(vec![p]);
    match message.update {
        EntityUpdate::Spawn {
            kind,
            uuid,
            pos,
            velocity,
            y_rot,
            x_rot,
            y_head_rot,
        } => {
            // `HangingEntity.getAddEntityPacket`: the direction and the block position.
            let hanging = crate::decoration::add_entity_packet(entities, message.entity);
            let pos = hanging.map_or(pos, |(_, p)| p);
            let add = SharedPacket::of(&AddEntity {
                entity_id: id,
                uuid: Uuid(uuid),
                entity_type: i32::from(kind.0),
                position: wire_vec(pos),
                movement: LpVec3(wire_vec(velocity)),
                x_rot,
                y_rot,
                y_head_rot,
                // `getAddEntityPacket` data: the block state of a falling block, the owner
                // of a projectile.
                data: hanging
                    .map(|(d, _)| d)
                    .or_else(|| {
                        entities
                            .falling_block(message.entity)
                            .map(|f| i32::from(f.state.0))
                    })
                    .or_else(|| {
                        let owner = entities.arrow(message.entity)?.owner?;
                        Some(entities.store.get(owner)?.network_id().0)
                    })
                    .or_else(|| {
                        let owner = entities.missile(message.entity)?.owner?;
                        Some(entities.store.get(owner)?.network_id().0)
                    })
                    .or_else(|| {
                        let special = entities.special(message.entity)?;
                        let owner = crate::mob::boss::special::spawn_owner(special)?;
                        Some(entities.store.get(owner)?.network_id().0)
                    })
                    .unwrap_or(0),
            });
            let entity = entities.store.get(message.entity);
            let data = entity
                .as_ref()
                .and_then(|e| entity_data(e, entities, false));
            let gear = entity.as_ref().and_then(|e| equipment(e, entities));
            // Passengers and the lead (`sendPairingData`).
            let passengers = crate::riding::passengers_packet(entities, message.entity);
            let riding = entities
                .vehicle_of(message.entity)
                .and_then(|v| crate::riding::passengers_packet(entities, v));
            let link = crate::mob::tame::link_pairing_packet(entities, message.entity);
            let attributes = match entities.mob(message.entity) {
                Some(m) => entities.extra(message.entity).and_then(|x| {
                    crate::effect::living::mob_pairing_attributes(id, m, &x.attributes)
                }),
                None => entities
                    .player(message.entity)
                    .and_then(|p| crate::effect::living::player_pairing_attributes(id, p)),
            };
            if data.is_none()
                && attributes.is_none()
                && gear.is_none()
                && passengers.is_none()
                && riding.is_none()
                && link.is_none()
            {
                return Some(vec![add]);
            }
            // `sendPairingData` bundles the spawn with its data and equipment.
            let mut packets = vec![SharedPacket::of(&BundleDelimiter), add];
            if let Some(items) = data {
                packets.push(SharedPacket::of(&SetEntityData {
                    entity_id: id,
                    items: RawBytes(items),
                }));
            }
            if let Some(attributes) = attributes {
                packets.push(SharedPacket::of(&attributes));
            }
            if let Some(gear) = gear {
                packets.push(SharedPacket::of(&gear));
            }
            for p in [passengers, riding].into_iter().flatten() {
                packets.push(SharedPacket::of(&p));
            }
            if let Some(link) = link {
                packets.push(SharedPacket::of(&link));
            }
            packets.push(SharedPacket::of(&BundleDelimiter));
            Some(packets)
        }
        EntityUpdate::Remove => one(SharedPacket::of(&RemoveEntities {
            ids: VarIntList(vec![id]),
        })),
        EntityUpdate::Sync {
            pos,
            velocity: _,
            y_rot,
            x_rot,
            on_ground,
        } => one(SharedPacket::of(&EntityPositionSync {
            entity_id: id,
            path: PositionPath::Linear(wire_vec(pos)),
            y_rot,
            x_rot,
            on_ground,
        })),
        EntityUpdate::Pos {
            dx,
            dy,
            dz,
            on_ground,
        } => one(SharedPacket::of(&MoveEntityPos {
            entity_id: id,
            delta: VecDelta::Linear([dx, dy, dz]),
            on_ground,
        })),
        EntityUpdate::PosRot {
            dx,
            dy,
            dz,
            y_rot,
            x_rot,
            on_ground,
        } => one(SharedPacket::of(&MoveEntityPosRot {
            entity_id: id,
            delta: VecDelta::Linear([dx, dy, dz]),
            y_rot,
            x_rot,
            on_ground,
        })),
        EntityUpdate::Rot {
            y_rot,
            x_rot,
            on_ground,
        } => one(SharedPacket::of(&MoveEntityRot {
            entity_id: id,
            on_ground,
            y_rot,
            x_rot,
        })),
        EntityUpdate::HeadRot(head) => one(SharedPacket::of(&RotateHead {
            entity_id: id,
            head_y_rot: head,
        })),
        EntityUpdate::Motion(velocity) => one(SharedPacket::of(&SetEntityMotion {
            entity_id: id,
            movement: LpVec3(wire_vec(velocity)),
        })),
        EntityUpdate::TrackedMotion(velocity) => {
            let motion = SharedPacket::of(&SetEntityMotion {
                entity_id: id,
                movement: LpVec3(wire_vec(velocity)),
            });
            // `ServerEntity.sendChanges`: hurting projectiles bundle their acceleration power.
            match hurting_projectile_power(entities, message.entity) {
                Some(power) => Some(vec![
                    SharedPacket::of(&BundleDelimiter),
                    motion,
                    SharedPacket::of(&ProjectilePower {
                        entity_id: id,
                        power,
                    }),
                    SharedPacket::of(&BundleDelimiter),
                ]),
                None => one(motion),
            }
        }
        EntityUpdate::Data => {
            let entity = entities.store.get(message.entity)?;
            let items = entity_data(&entity, entities, true)?;
            one(SharedPacket::of(&SetEntityData {
                entity_id: id,
                items: RawBytes(items),
            }))
        }
    }
}

#[cfg(test)]
pub(crate) mod layout_tests {
    #![allow(clippy::expect_used, clippy::indexing_slicing)]

    use std::cell::RefCell;

    use serde_json::Value;

    thread_local! {
        static WRITTEN: RefCell<Vec<(u8, i32)>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn record(id: u8, serializer: i32) {
        WRITTEN.with_borrow_mut(|w| w.push((id, serializer)));
    }

    /// Every synched data item a mob writes has the serializer Vanilla declares at its index:
    /// the client disconnects on any other.
    #[test]
    fn mob_data_matches_vanilla_layout() {
        let json = std::fs::read_to_string(format!(
            "{}/entity_data.json",
            vanillite_data::VANILLA_DATA_DIR
        ))
        .expect("entity_data.json");
        let layout: Value = serde_json::from_str(&json).expect("entity_data.json");
        let mut wrong = Vec::new();
        for &kind in crate::mob::Kind::ALL {
            let name = kind.type_id().info().name;
            let fields = layout["types"][name].as_array().expect(name);
            WRITTEN.with_borrow_mut(Vec::clear);
            super::mob_data(&mut Vec::new(), &crate::mob::MobData::new(kind, 1), 0, true);
            for (id, serializer) in WRITTEN.take() {
                let expected = fields.iter().find(|f| f[0].as_u64() == Some(u64::from(id)));
                let actual = usize::try_from(serializer)
                    .ok()
                    .and_then(|i| layout["serializers"][i].as_str());
                if expected.and_then(|f| f[1].as_str()) != actual {
                    wrong.push(format!("{name} {id}: writes {actual:?}, is {expected:?}"));
                }
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
