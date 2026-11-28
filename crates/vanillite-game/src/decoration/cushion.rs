//! Cushions (`Cushion`, 26.x): placed on top faces, players sit on them, they break in fire,
//! lava and lightning and drop when their anchor goes away.

use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::entity::ids;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{LevelParticles, Particle};
use vanillite_world::entity::{Dimensions, EntityId, Rotation};

use super::hanging::{Attached, Placed};
use super::{Decor, DecorData};
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::mob::MobContext;
use crate::mob::animal::Interaction;
use crate::player::encode::{DataWriter, serializer};
use crate::survival::Effect;
use crate::survival::damage::DamageSource;
use crate::survival::player::Env;

/// The dye colours in `DyeColor` order.
pub const COLORS: [&str; 16] = [
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];

/// A cushion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cushion {
    pub attached: Attached,
    /// `DATA_COLOR` (`DyeColor` id).
    pub color: u8,
}

/// The item of a cushion colour (`Items.CUSHION.pick(color)`).
pub fn item_name(color: u8) -> String {
    format!(
        "minecraft:{}_cushion",
        COLORS.get(usize::from(color)).copied().unwrap_or("white")
    )
}

/// The colour of a cushion item, `None` for other items.
pub fn color_of_item(stack: &ItemStack) -> Option<u8> {
    let name = crate::item::item_id(stack).name();
    let color = name.strip_prefix("minecraft:")?.strip_suffix("_cushion")?;
    COLORS
        .iter()
        .position(|c| *c == color)
        .and_then(|i| u8::try_from(i).ok())
}

/// `BlockState.isSuffocating` without the world's block facts: `#causes_suffocation` blocks
/// with a full collision cube.
fn suffocating(state: vanillite_data::block::BlockState) -> bool {
    let full = state.has(vanillite_data::block::flags::COLLISION_FULL_BLOCK);
    full && !crate::survival::blocks::never_suffocating(state.block())
        && crate::player::play::game_data()
            .is_none_or(|d| d.block_in_tag(state.block().0, "minecraft:causes_suffocation"))
}

fn floor(v: f64) -> i32 {
    vanillite_base::math::floor(v)
}

/// `nextDeflated()`: the box shrunk by a hair (`1.0E-7`).
fn next_deflated(b: &Aabb) -> Aabb {
    let e = 1.0e-7;
    Aabb::new(
        b.min_x + e,
        b.min_y + e,
        b.min_z + e,
        b.max_x - e,
        b.max_y - e,
        b.max_z - e,
    )
}

fn shape_boxes(
    state: vanillite_data::block::BlockState,
    pos: BlockPos,
    collision: bool,
) -> Vec<Aabb> {
    let shape = if collision {
        state.collision_shape()
    } else {
        state.outline_shape()
    };
    let (x, y, z) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    shape
        .iter()
        .map(|b| Aabb::new(b[0] + x, b[1] + y, b[2] + z, b[3] + x, b[4] + y, b[5] + z))
        .collect()
}

/// `hasAnchorBelow`: a block shape right under the box.
fn has_anchor_below(level: &Level, b: &Aabb) -> bool {
    let next_down = |v: f64| f64::from_bits(v.to_bits() - 1);
    let anchor = Aabb::new(
        b.min_x,
        b.min_y - 0.015_625,
        b.min_z,
        next_down(b.max_x),
        b.min_y,
        next_down(b.max_z),
    );
    let search = anchor.expand_towards(0.0, -0.125, 0.0);
    for x in floor(search.min_x)..=floor(search.max_x) {
        for y in floor(search.min_y)..=floor(search.max_y) {
            for z in floor(search.min_z)..=floor(search.max_z) {
                let pos = BlockPos::new(x, y, z);
                let boxes = shape_boxes(level.block(pos), pos, false);
                if boxes.is_empty() {
                    continue;
                }
                // `shape.bounds().move(pos)`.
                let mut bounds = boxes[0];
                for o in &boxes[1..] {
                    bounds = Aabb::new(
                        bounds.min_x.min(o.min_x),
                        bounds.min_y.min(o.min_y),
                        bounds.min_z.min(o.min_z),
                        bounds.max_x.max(o.max_x),
                        bounds.max_y.max(o.max_y),
                        bounds.max_z.max(o.max_z),
                    );
                }
                if bounds.intersects(&anchor) {
                    return true;
                }
            }
        }
    }
    false
}

/// `isAnchorBuried`: block collisions cover the whole resting slice.
fn is_anchor_buried(level: &Level, b: &Aabb) -> bool {
    let slice = next_deflated(&Aabb::new(
        b.min_x,
        b.min_y,
        b.min_z,
        b.max_x,
        b.min_y + 0.015_625,
        b.max_z,
    ));
    let mut colliders = Vec::new();
    for x in floor(slice.min_x)..=floor(slice.max_x) {
        for y in floor(slice.min_y)..=floor(slice.max_y) {
            for z in floor(slice.min_z)..=floor(slice.max_z) {
                let pos = BlockPos::new(x, y, z);
                colliders.extend(
                    shape_boxes(level.block(pos), pos, true)
                        .into_iter()
                        .filter(|c| c.intersects(&slice)),
                );
            }
        }
    }
    if colliders.is_empty() {
        return false;
    }
    // The slice is covered when every sample of it lies in a collider.
    let n = 16;
    let y = (slice.min_y + slice.max_y) / 2.0;
    (0..n).all(|i| {
        (0..n).all(|k| {
            let x = slice.min_x + (slice.max_x - slice.min_x) * (f64::from(i) + 0.5) / f64::from(n);
            let z = slice.min_z + (slice.max_z - slice.min_z) * (f64::from(k) + 0.5) / f64::from(n);
            colliders.iter().any(|c| c.contains(x, y, z))
        })
    })
}

/// `isCoveredBySuffocatingBlocks`.
fn covered_by_suffocating_blocks(level: &Level, b: &Aabb) -> bool {
    let d = next_deflated(b);
    for x in floor(d.min_x)..=floor(d.max_x) {
        for y in floor(d.min_y)..=floor(d.max_y) {
            for z in floor(d.min_z)..=floor(d.max_z) {
                if !suffocating(level.block(BlockPos::new(x, y, z))) {
                    return false;
                }
            }
        }
    }
    true
}

/// `wouldSurviveAt`.
pub fn would_survive_at(level: &Level, b: &Aabb) -> bool {
    has_anchor_below(level, b) && !covered_by_suffocating_blocks(level, b)
}

/// `canBePlacedAt`.
pub fn can_be_placed_at(level: &Level, b: &Aabb) -> bool {
    would_survive_at(level, b) && !is_anchor_buried(level, b)
}

/// `CushionItem.useOn`: the clicked top face (blocks of `#cushion_uses_collision_shape` by
/// their collision top), `click_y` the y of the click.
#[allow(clippy::too_many_arguments)]
pub fn use_on(
    level: &mut Level,
    pos: BlockPos,
    face: Direction,
    click_y: f64,
    y_rot: f32,
    stack: &mut ItemStack,
    player: Option<EntityId>,
    infinite: bool,
) -> Placed {
    let Some(color) = color_of_item(stack) else {
        return Placed::Fail;
    };
    let state = level.block(pos);
    let (face, click_y) = match crate::player::play::game_data() {
        Some(d) if d.block_in_tag(state.block().0, "minecraft:cushion_uses_collision_shape") => {
            match state
                .collision_shape()
                .iter()
                .map(|b| b[4])
                .reduce(f64::max)
            {
                Some(top) => (Direction::Up, f64::from(pos.y) + top),
                None => (face, click_y),
            }
        }
        _ => (face, click_y),
    };
    if face != Direction::Up {
        return Placed::Fail;
    }
    // `BlockPlaceContext.getClickedPos`: a replaceable clicked block is replaced.
    let at = if state.has(vanillite_data::block::flags::REPLACEABLE) {
        pos
    } else {
        pos.relative(face)
    };
    let entity_pos = Vec3::new(f64::from(at.x) + 0.5, click_y, f64::from(at.z) + 0.5);
    let aabb = Dimensions::of(ids::CUSHION).bounding_box(entity_pos);
    if !can_be_placed_at(level, &aabb) {
        return Placed::Fail;
    }
    let mut other = false;
    let ents = &level.entities;
    ents.store.for_each_non_player_in(&aabb, |id| {
        if ents
            .store
            .get(id)
            .is_some_and(|e| e.kind() == ids::CUSHION && e.aabb().intersects(&aabb))
        {
            other = true;
        }
    });
    if other {
        return Placed::Fail;
    }
    let mut decor = Decor::new(DecorData::Cushion(Cushion {
        attached: Attached {
            pos: BlockPos::containing(entity_pos.x, entity_pos.y, entity_pos.z),
            direction: Direction::South,
            checks: 0,
        },
        color,
    }));
    decor.name = stack
        .component(vanillite_protocol::components::ids::CUSTOM_NAME)
        .map(Box::from);
    #[allow(clippy::cast_precision_loss)]
    let yaw = (super::data_2d(super::from_y_rot(y_rot)) * 90) as f32;
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    let uuid = crate::mob::new_uuid();
    let id = super::spawn(
        &mut level.entities,
        &network_ids,
        ids::CUSHION,
        uuid,
        entity_pos,
        Rotation {
            y_rot: yaw,
            x_rot: 0.0,
            y_head_rot: yaw,
        },
        decor,
    );
    super::stand::apply_entity_data_component(&mut level.entities, id, stack);
    if !level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0)
    {
        return Placed::Fail;
    }
    if in_fire(level, &aabb) {
        // `destroyIfInFire` right away: killed by `inFire`, dropped without a cause.
        let name = level.entities.decor(id).and_then(|d| d.name.clone());
        level.entities.despawn(id);
        level.play_sound(
            None,
            entity_pos.x,
            entity_pos.y,
            entity_pos.z,
            super::hanging::sound_id("minecraft:entity.cushion.break"),
            crate::level::SOUND_NEUTRAL,
            1.0,
            1.0,
        );
        if level.settings.entity_drops
            && let Some(item) = super::stack_of(&item_name(color))
        {
            let item = super::with_name(item, name.as_deref());
            crate::block::drops::spawn_item_moving(level, entity_pos, item, None, 10, None);
        }
    }
    level.play_sound(
        None,
        entity_pos.x,
        entity_pos.y,
        entity_pos.z,
        super::hanging::sound_id("minecraft:entity.cushion.place"),
        crate::level::SOUND_BLOCKS,
        0.75,
        0.8,
    );
    let event = crate::level::game_event::events().entity_place;
    level.game_event(
        event,
        entity_pos,
        crate::level::game_event::GameEventContext::of(player),
    );
    if !infinite {
        stack.shrink(1);
    }
    Placed::Success
}

/// `isBreakingDeniedFor`: players that may not build cannot break cushions.
pub fn may_break(ents: &Entities, source: &DamageSource) -> bool {
    !source.cause.and_then(|c| ents.player(c)).is_some_and(|p| {
        matches!(
            p.game_mode,
            vanillite_config::GameMode::Adventure | vanillite_config::GameMode::Spectator
        )
    })
}

/// `Cushion.interact`: the player sits down.
pub fn interact(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    id: EntityId,
    sneaking: bool,
) -> Interaction {
    if sneaking || !ents.riding.passengers_of(id).is_empty() {
        return Interaction::Pass;
    }
    if crate::riding::start_riding(ents, env, player, id, false) {
        super::play_sound(ents, env, id, "minecraft:entity.cushion.sit", 1.0, 1.0);
        return Interaction::Success;
    }
    Interaction::Consume
}

/// `removePassenger` of a cushion that stays: the get-up sound.
pub fn passenger_left(ents: &Entities, env: &mut Env<'_>, id: EntityId) {
    if ents.store.get(id).is_some_and(|e| e.kind() == ids::CUSHION) {
        super::play_sound(ents, env, id, "minecraft:entity.cushion.get_up", 1.0, 1.0);
    }
}

/// `tickAtCheckInterval`: lava and fire at the cushion destroy it.
pub fn tick_at_check_interval(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) {
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let (pos, aabb) = (e.pos(), e.aabb());
    let block = BlockPos::containing(pos.x, pos.y, pos.z);
    let fluid = level.block(block).fluid_state();
    if env.facts.is_lava(fluid) {
        let source = DamageSource::of(env.types.lava);
        super::hurt(level, ents, env, cx, id, &source);
        if cx.discarded.contains(&id) {
            return;
        }
    }
    destroy_if_in_fire(level, ents, env, cx, id, &aabb);
}

/// Whether a `#minecraft:fire` block is in the box.
fn in_fire(level: &Level, aabb: &Aabb) -> bool {
    let d = next_deflated(aabb);
    let Some(g) = crate::player::play::game_data() else {
        return false;
    };
    for x in floor(d.min_x)..=floor(d.max_x) {
        for y in floor(d.min_y)..=floor(d.max_y) {
            for z in floor(d.min_z)..=floor(d.max_z) {
                let state = level.block(BlockPos::new(x, y, z));
                if g.block_in_tag(state.block().0, "minecraft:fire") {
                    return true;
                }
            }
        }
    }
    false
}

/// `destroyIfInFire`.
pub fn destroy_if_in_fire(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    aabb: &Aabb,
) {
    if in_fire(level, aabb) {
        let source = DamageSource::of(env.types.in_fire);
        super::hurt(level, ents, env, cx, id, &source);
    }
}

/// `Cushion.dropItem`: the break sound and particles, then the cushion item.
pub fn drop_item(ents: &Entities, env: &mut Env<'_>, id: EntityId, caused_by: Option<EntityId>) {
    let Some(DecorData::Cushion(c)) = ents.decor(id).map(|d| &d.data) else {
        return;
    };
    let color = c.color;
    super::play_sound(ents, env, id, "minecraft:entity.cushion.break", 1.0, 1.0);
    if let Some(e) = ents.store.get(id) {
        let pos = e.pos();
        let dims = e.dims();
        let wool = vanillite_data::block::BlockId::by_name(&format!(
            "minecraft:{}_wool",
            COLORS.get(usize::from(color)).copied().unwrap_or("white")
        ))
        .map_or(0, |b| i32::from(b.default_state().0));
        let kind = vanillite_data::registry::PARTICLE_TYPE
            .id("minecraft:block")
            .map_or(0, i32::from);
        let mut options = Vec::new();
        vanillite_protocol::codec::write_varint(&mut options, wool);
        env.out.push(Effect::Nearby {
            pos,
            range: 32.0,
            except: None,
            packet: SharedPacket::of(&LevelParticles {
                particle: Particle { kind, options },
                override_limiter: false,
                always_show: false,
                x: pos.x,
                y: pos.y + f64::from(dims.height) * 0.666_666_666_666_666_6,
                z: pos.z,
                x_dist: dims.width / 4.0,
                y_dist: dims.height / 4.0,
                z_dist: dims.width / 4.0,
                x_max_speed: 0.05,
                y_max_speed: 0.05,
                z_max_speed: 0.05,
                count: 10,
                randomization: 0,
            }),
        });
    }
    if !env.rules.bool(crate::survival::rules::Rule::ENTITY_DROPS)
        || super::creative(ents, caused_by)
    {
        return;
    }
    let name = ents.decor(id).and_then(|d| d.name.clone());
    if let (Some(stack), Some(pos)) = (
        super::stack_of(&item_name(color)),
        ents.store.get(id).map(|e| e.pos()),
    ) {
        super::spawn_item(env, pos, super::with_name(stack, name.as_deref()));
    }
}

/// The synched data of a cushion (`DATA_COLOR`).
pub fn entity_data(w: &mut DataWriter<'_>, c: &Cushion) {
    w.varint(8, serializer::DYE_COLOR, i32::from(c.color), 0);
}

/// `Cushion.addAdditionalSaveData`.
pub fn save(c: &Cushion, out: &mut Compound) {
    out.insert("block_pos", super::block_pos_tag(c.attached.pos));
    out.insert(
        "color",
        COLORS.get(usize::from(c.color)).copied().unwrap_or("white"),
    );
}

/// `Cushion.readAdditionalSaveData`.
pub fn load(tag: &Compound, pos: Vec3) -> Cushion {
    let color = tag
        .get("color")
        .and_then(Tag::as_str)
        .and_then(|c| COLORS.iter().position(|n| *n == c))
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(0);
    Cushion {
        attached: Attached {
            pos: super::hanging::attached_pos(tag, pos),
            direction: Direction::South,
            checks: 0,
        },
        color,
    }
}
