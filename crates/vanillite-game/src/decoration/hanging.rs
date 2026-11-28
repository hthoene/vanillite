//! Item frames, glow item frames and paintings (`HangingEntity`, `ItemFrame`, `Painting`):
//! placement, support checks, hits, item rotation, comparator output and framed maps.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{EntityId, Rotation};

use super::{Decor, DecorData, counter_clockwise, data_2d, from_3d, horizontal};
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::{MobContext, WorldAction};
use crate::player::encode::{DataWriter, serializer};
use crate::survival::damage::{DamageSource, tag};
use crate::survival::player::Env;

/// What every block-attached entity keeps (`BlockAttachedEntity`, `HangingEntity`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attached {
    /// `pos`: the block it hangs in.
    pub pos: BlockPos,
    /// `DATA_DIRECTION` (south for cushions, which have none).
    pub direction: Direction,
    /// `ticksSinceLastCheck`.
    pub checks: i32,
}

/// An item frame (`ItemFrame`, `GlowItemFrame` by its type).
#[derive(Clone, Debug)]
pub struct Frame {
    pub attached: Attached,
    /// `DATA_ITEM`.
    pub item: ItemStack,
    /// `DATA_ROTATION` (0–7).
    pub rotation: i32,
    /// `dropChance`.
    pub drop_chance: f32,
    /// `fixed`.
    pub fixed: bool,
}

impl PartialEq for Frame {
    fn eq(&self, other: &Frame) -> bool {
        self.attached == other.attached
            && self.item == other.item
            && self.rotation == other.rotation
            && self.drop_chance.to_bits() == other.drop_chance.to_bits()
            && self.fixed == other.fixed
    }
}

impl Frame {
    /// A new frame hanging in `pos` facing `direction`.
    pub fn new(pos: BlockPos, direction: Direction) -> Frame {
        Frame {
            attached: Attached {
                pos,
                direction,
                checks: 0,
            },
            item: ItemStack::EMPTY,
            rotation: 0,
            drop_chance: 1.0,
            fixed: false,
        }
    }

    /// `hasFramedMap`.
    pub fn has_framed_map(&self) -> bool {
        crate::map::map_id(&self.item).is_some()
    }

    /// `getAnalogOutput`.
    pub fn analog_output(&self) -> i32 {
        if self.item.is_empty() {
            0
        } else {
            self.rotation % 8 + 1
        }
    }
}

/// A painting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Painting {
    pub attached: Attached,
    /// `minecraft:painting_variant` id.
    pub variant: u32,
}

/// The rotation of a hanging entity facing `direction` (`setDirection`).
pub fn rotation_of(kind: EntityTypeId, direction: Direction) -> Rotation {
    #[allow(clippy::cast_precision_loss)]
    let y = (data_2d(direction).max(0) * 90) as f32;
    let frame = kind == ids::ITEM_FRAME || kind == ids::GLOW_ITEM_FRAME;
    let (y_rot, x_rot) = match direction {
        Direction::Up if frame => (0.0, -90.0),
        Direction::Down if frame => (0.0, 90.0),
        _ => (y, 0.0),
    };
    Rotation {
        y_rot,
        x_rot,
        y_head_rot: y_rot,
    }
}

fn step(d: Direction) -> Vec3 {
    let [x, y, z] = d.normal();
    Vec3::new(f64::from(x), f64::from(y), f64::from(z))
}

fn relative(v: Vec3, d: Direction, amount: f64) -> Vec3 {
    v + step(d) * amount
}

fn center_of(pos: BlockPos) -> Vec3 {
    Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    )
}

/// `AABB.ofSize(center, x, y, z)`.
fn of_size(c: Vec3, x: f64, y: f64, z: f64) -> Aabb {
    Aabb::new(
        c.x - x / 2.0,
        c.y - y / 2.0,
        c.z - z / 2.0,
        c.x + x / 2.0,
        c.y + y / 2.0,
        c.z + z / 2.0,
    )
}

/// `ItemFrame.createBoundingBox(pos, direction, hasFramedMap)`.
fn frame_box(pos: BlockPos, direction: Direction, map: bool) -> Aabb {
    let position = relative(center_of(pos), direction, -0.46875);
    let size = if map { 1.0 } else { 0.75 };
    let [x, y, _] = direction.normal();
    let xs = if x != 0 { 0.0625 } else { size };
    let ys = if y != 0 { 0.0625 } else { size };
    let zs = if x == 0 && y == 0 { 0.0625 } else { size };
    of_size(position, xs, ys, zs)
}

/// The width and height of a painting variant in blocks.
pub fn variant_size(variant: u32) -> (i32, i32) {
    crate::player::play::game_data()
        .and_then(|d| d.painting_variants.get(variant as usize).copied())
        .unwrap_or((1, 1))
}

/// `Painting.calculateBoundingBox`.
fn painting_box(pos: BlockPos, direction: Direction, variant: u32) -> Aabb {
    let (w, h) = variant_size(variant);
    let wall = relative(center_of(pos), direction, -0.46875);
    let offset = |size: i32| if size % 2 == 0 { 0.5 } else { 0.0 };
    let position = relative(
        relative(wall, counter_clockwise(direction), offset(w)),
        Direction::Up,
        offset(h),
    );
    let [x, _, _] = direction.normal();
    let (w, h) = (f64::from(w), f64::from(h));
    let xs = if x != 0 { 0.0625 } else { w };
    let zs = if x == 0 { 0.0625 } else { w };
    of_size(position, xs, h, zs)
}

/// The bounding box of a hanging entity (`calculateBoundingBox`).
pub fn bounding_box(kind: EntityTypeId, data: &DecorData) -> Option<Aabb> {
    let _ = kind;
    match data {
        DecorData::Frame(f) => Some(frame_box(
            f.attached.pos,
            f.attached.direction,
            f.has_framed_map(),
        )),
        DecorData::Painting(p) => Some(painting_box(
            p.attached.pos,
            p.attached.direction,
            p.variant,
        )),
        _ => None,
    }
}

/// `getPopBox`: frames without the map's larger box.
fn pop_box(data: &DecorData) -> Option<Aabb> {
    match data {
        DecorData::Frame(f) => Some(frame_box(f.attached.pos, f.attached.direction, false)),
        DecorData::Painting(p) => Some(painting_box(
            p.attached.pos,
            p.attached.direction,
            p.variant,
        )),
        _ => None,
    }
}

/// `hasLevelCollision(box)` against blocks.
fn level_collision(level: &Level, aabb: &Aabb) -> bool {
    let mut scratch = Vec::new();
    !crate::mob::physics::no_collision(&level.view(), aabb, &mut scratch)
}

/// `isSolid() || DiodeBlock.isDiode(state)`.
fn supporting(state: vanillite_data::block::BlockState) -> bool {
    crate::block::is_solid(state) || crate::level::signal::is_diode(state)
}

/// `canCoexist(allowIntersectingSameType)`: no other hanging entity in the pop box of the same
/// type (unless allowed) or the same direction.
fn can_coexist(
    ents: &Entities,
    me: Option<EntityId>,
    kind: EntityTypeId,
    direction: Direction,
    pop: &Aabb,
    allow_same_type: bool,
) -> bool {
    let mut clear = true;
    ents.store.for_each_non_player_in(pop, |other| {
        if Some(other) == me || !clear {
            return;
        }
        let Some(e) = ents.store.get(other) else {
            return;
        };
        if !e.aabb().intersects(pop) {
            return;
        }
        let other_dir = match ents.decor(other).map(|d| &d.data) {
            Some(DecorData::Frame(f)) => f.attached.direction,
            Some(DecorData::Painting(p)) => p.attached.direction,
            _ => return,
        };
        let same_type = !allow_same_type && e.kind() == kind;
        if same_type || other_dir == direction {
            clear = false;
        }
    });
    clear
}

/// `survives` of an item frame or painting.
pub fn survives(
    level: &Level,
    ents: &Entities,
    me: Option<EntityId>,
    kind: EntityTypeId,
    data: &DecorData,
) -> bool {
    let Some(pop) = pop_box(data) else {
        return true;
    };
    match data {
        DecorData::Frame(f) => {
            if f.fixed {
                return true;
            }
            if level_collision(level, &pop) {
                return false;
            }
            let dir = f.attached.direction;
            let state = level.block(f.attached.pos.relative(dir.opposite()));
            let ok = crate::block::is_solid(state)
                || horizontal(dir) && crate::level::signal::is_diode(state);
            ok && can_coexist(ents, me, kind, dir, &pop, true)
        }
        DecorData::Painting(p) => {
            if level_collision(level, &pop) {
                return false;
            }
            let dir = p.attached.direction;
            // `calculateSupportBox`: the box moved half a block into the wall.
            let s = step(dir) * -0.5;
            let e = 1.0e-7;
            let b = pop.offset(s.x, s.y, s.z);
            let floor = vanillite_base::math::floor;
            let (x0, y0, z0) = (floor(b.min_x + e), floor(b.min_y + e), floor(b.min_z + e));
            let (x1, y1, z1) = (floor(b.max_x - e), floor(b.max_y - e), floor(b.max_z - e));
            for x in x0..=x1 {
                for y in y0..=y1 {
                    for z in z0..=z1 {
                        if !supporting(level.block(BlockPos::new(x, y, z))) {
                            return false;
                        }
                    }
                }
            }
            can_coexist(ents, me, kind, dir, &pop, false)
        }
        _ => true,
    }
}

/// What a placement did (`InteractionResult`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    /// Placed: the item is used and the hand swings.
    Success,
    /// Consumed without placing.
    Consume,
    /// Nothing happened.
    Fail,
}

/// `HangingEntityItem.useOn` / `ItemFrameItem.useOn`: places a painting or item frame on the
/// clicked face of `pos`. `may_build`: the player may build (`mayUseItemAt`).
#[allow(clippy::too_many_arguments)]
pub fn use_on(
    level: &mut Level,
    kind: EntityTypeId,
    pos: BlockPos,
    face: Direction,
    stack: &mut ItemStack,
    player: Option<EntityId>,
    may_build: bool,
    infinite: bool,
) -> Placed {
    let at = pos.relative(face);
    let frame = kind == ids::ITEM_FRAME || kind == ids::GLOW_ITEM_FRAME;
    let may_place = if frame {
        super::in_build_height(level, at) && may_build
    } else {
        horizontal(face) && may_build
    };
    if !may_place {
        return Placed::Fail;
    }
    let mut decor = if frame {
        Decor::new(DecorData::Frame(Box::new(Frame::new(at, face))))
    } else {
        // The candidate painting is the new entity: its random picked the variant.
        match create_painting(level, at, face) {
            Some((p, random)) => Decor {
                random,
                ..Decor::new_without_seed(DecorData::Painting(p))
            },
            None => return Placed::Consume,
        }
    };
    // `createDefaultStackConfig`: the custom name and the painting variant of the item.
    decor.name = stack
        .component(vanillite_protocol::components::ids::CUSTOM_NAME)
        .map(Box::from);
    if let DecorData::Painting(p) = &mut decor.data
        && let Some(v) = item_variant(stack)
    {
        p.variant = v;
    }
    if !survives(level, &level.entities, None, kind, &decor.data) {
        return Placed::Consume;
    }
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    let uuid = crate::mob::new_uuid();
    let rot = match &decor.data {
        DecorData::Frame(f) => rotation_of(kind, f.attached.direction),
        DecorData::Painting(p) => rotation_of(kind, p.attached.direction),
        _ => Rotation::default(),
    };
    let id = super::spawn(
        &mut level.entities,
        &network_ids,
        kind,
        uuid,
        center_of(at),
        rot,
        decor,
    );
    super::stand::apply_entity_data_component(&mut level.entities, id, stack);
    if !level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0)
    {
        return Placed::Fail;
    }
    let pos = level
        .entities
        .store
        .get(id)
        .map_or(center_of(at), |e| e.pos());
    // `playPlacementSound`, `gameEvent(ENTITY_PLACE)`.
    let sound = if kind == ids::PAINTING {
        "minecraft:entity.painting.place"
    } else if kind == ids::GLOW_ITEM_FRAME {
        "minecraft:entity.glow_item_frame.place"
    } else {
        "minecraft:entity.item_frame.place"
    };
    let silent = level.entities.decor(id).is_some_and(|d| d.silent);
    if !silent {
        level.play_sound(
            None,
            pos.x,
            pos.y,
            pos.z,
            sound_id(sound),
            crate::level::SOUND_NEUTRAL,
            1.0,
            1.0,
        );
    }
    let event = crate::level::game_event::events().entity_place;
    level.game_event(
        event,
        pos,
        crate::level::game_event::GameEventContext::of(player),
    );
    if !infinite {
        stack.shrink(1);
    }
    Placed::Success
}

pub(crate) fn sound_id(name: &str) -> u16 {
    u16::try_from(crate::survival::sound_event(name).0).unwrap_or(0)
}

/// The `painting/variant` component of an item: a registry variant.
fn item_variant(stack: &ItemStack) -> Option<u32> {
    let ty = vanillite_data::registry::DATA_COMPONENT_TYPE.id("minecraft:painting/variant")?;
    let value = stack.component(ty)?;
    let mut r = vanillite_protocol::codec::Reader::new(value);
    let id = r.varint().ok()?;
    u32::try_from(id - 1).ok()
}

/// `Painting.create(level, pos, direction)`: the largest placeable variants that fit, one at
/// random.
fn create_painting(
    level: &Level,
    pos: BlockPos,
    direction: Direction,
) -> Option<(Painting, LegacyRandom)> {
    let data = crate::player::play::game_data()?;
    let placeable: Vec<u32> = data
        .registry_tags
        .get("minecraft:painting_variant")
        .and_then(|t| t.get("minecraft:placeable"))
        .cloned()
        .unwrap_or_default();
    if placeable.is_empty() {
        return None;
    }
    let mut random = LegacyRandom::new(crate::mob::new_entity_seed());
    let mut fits: Vec<u32> = placeable
        .into_iter()
        .filter(|&v| {
            let p = DecorData::Painting(Painting {
                attached: Attached {
                    pos,
                    direction,
                    checks: 0,
                },
                variant: v,
            });
            survives(level, &level.entities, None, ids::PAINTING, &p)
        })
        .collect();
    let area = |v: u32| {
        let (w, h) = variant_size(v);
        w * h
    };
    let largest = fits.iter().map(|&v| area(v)).max()?;
    fits.retain(|&v| area(v) >= largest);
    // `Util.getRandomSafe(list, random)`.
    let n = i32::try_from(fits.len()).ok()?;
    let pick = fits[usize::try_from(random.next_int_bounded(n)).ok()?];
    let p = Painting {
        attached: Attached {
            pos,
            direction,
            checks: 0,
        },
        variant: pick,
    };
    Some((p, random))
}

/// The item of an empty frame (`getFrameItemStack`).
pub fn frame_item(kind: EntityTypeId) -> ItemStack {
    let name = if kind == ids::GLOW_ITEM_FRAME {
        "minecraft:glow_item_frame"
    } else {
        "minecraft:item_frame"
    };
    super::stack_of(name).unwrap_or_default()
}

fn frame_sound(kind: EntityTypeId, what: &str) -> String {
    let base = if kind == ids::GLOW_ITEM_FRAME {
        "glow_item_frame"
    } else {
        "item_frame"
    };
    format!("minecraft:entity.{base}.{what}")
}

/// `HangingEntity.spawnAtLocation`: in front of the wall.
fn spawn_at_location(ents: &Entities, env: &mut Env<'_>, id: EntityId, stack: ItemStack) {
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let direction = match ents.decor(id).map(|d| &d.data) {
        Some(DecorData::Frame(f)) => f.attached.direction,
        Some(DecorData::Painting(p)) => p.attached.direction,
        _ => Direction::South,
    };
    let [sx, _, sz] = direction.normal();
    let p = e.pos();
    let pos = Vec3::new(
        p.x + f64::from(sx as f32 * 0.15),
        p.y,
        p.z + f64::from(sz as f32 * 0.15),
    );
    super::spawn_item(env, pos, stack);
}

/// `ItemFrame.removeFramedMap`.
pub fn remove_framed_map(level: &Level, ents: &Entities, id: EntityId, stack: &ItemStack) {
    let Some(map) = crate::map::map_id(stack) else {
        return;
    };
    let (Some(pos), Some(network_id)) = (
        match ents.decor(id).map(|d| &d.data) {
            Some(DecorData::Frame(f)) => Some(f.attached.pos),
            _ => None,
        },
        ents.store.get(id).map(|e| e.network_id().0),
    ) else {
        return;
    };
    if let Some(maps) = level.maps() {
        maps.with(map, |d| d.removed_from_frame(pos, network_id));
    }
}

/// `ItemFrame.setItem(stack, updateNeighbours)`.
fn set_item(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    stack: ItemStack,
) {
    let Some(DecorData::Frame(f)) = ents.decor_mut(id).map(|d| &mut d.data) else {
        return;
    };
    let stack = if stack.is_empty() {
        ItemStack::EMPTY
    } else {
        stack.with_count(1)
    };
    let added = !stack.is_empty();
    f.item = stack;
    let pos = f.attached.pos;
    if let Some(mut e) = ents.store.get_mut(id) {
        e.mark_data_dirty();
    }
    super::refresh_box(ents, id);
    if added && let Some(kind) = ents.store.get(id).map(|e| e.kind()) {
        super::play_sound(ents, env, id, &frame_sound(kind, "add_item"), 1.0, 1.0);
    }
    cx.actions.push(WorldAction::OutputSignal(pos));
}

/// `dropItem(level, causedBy)` of a painting or frame (the break sound, the item, the event).
pub fn drop_item(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    caused_by: Option<EntityId>,
) {
    let Some(kind) = ents.store.get(id).map(|e| e.kind()) else {
        return;
    };
    if kind == ids::PAINTING {
        if !env.rules.bool(crate::survival::rules::Rule::ENTITY_DROPS) {
            return;
        }
        super::play_sound(ents, env, id, "minecraft:entity.painting.break", 1.0, 1.0);
        if !super::creative(ents, caused_by) {
            let name = ents.decor(id).and_then(|d| d.name.clone());
            if let Some(stack) = super::stack_of("minecraft:painting") {
                spawn_at_location(ents, env, id, super::with_name(stack, name.as_deref()));
            }
        }
        return;
    }
    super::play_sound(ents, env, id, &frame_sound(kind, "break"), 1.0, 1.0);
    drop_frame_item(level, ents, env, cx, id, caused_by, true);
    game_event(
        cx,
        ents,
        id,
        crate::level::game_event::events().block_change,
        caused_by,
    );
}

/// `ItemFrame.dropItem(level, causedBy, withFrame)`.
fn drop_frame_item(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    caused_by: Option<EntityId>,
    with_frame: bool,
) {
    let Some(kind) = ents.store.get(id).map(|e| e.kind()) else {
        return;
    };
    let (stack, fixed) = match ents.decor(id).map(|d| &d.data) {
        Some(DecorData::Frame(f)) => (f.item.clone(), f.fixed),
        _ => return,
    };
    if fixed {
        return;
    }
    set_item(ents, env, cx, id, ItemStack::EMPTY);
    if !env.rules.bool(crate::survival::rules::Rule::ENTITY_DROPS) {
        if caused_by.is_none() {
            remove_framed_map(level, ents, id, &stack);
        }
        return;
    }
    if super::creative(ents, caused_by) {
        remove_framed_map(level, ents, id, &stack);
        return;
    }
    if with_frame {
        let name = ents.decor(id).and_then(|d| d.name.clone());
        spawn_at_location(
            ents,
            env,
            id,
            super::with_name(frame_item(kind), name.as_deref()),
        );
    }
    if !stack.is_empty() {
        remove_framed_map(level, ents, id, &stack);
        let roll = match ents.decor_mut(id) {
            Some(d) => match &d.data {
                DecorData::Frame(f) => {
                    let chance = f.drop_chance;
                    (d.random.next_float(), chance)
                }
                _ => return,
            },
            None => return,
        };
        if roll.0 < roll.1 {
            spawn_at_location(ents, env, id, stack);
        }
    }
}

fn game_event(
    cx: &mut MobContext,
    ents: &Entities,
    id: EntityId,
    event: u16,
    source: Option<EntityId>,
) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    cx.actions.push(WorldAction::Level(
        crate::mob::boss::warden::LevelAction::GameEvent {
            event,
            pos,
            context: crate::level::game_event::GameEventContext::of(source),
        },
    ));
}

/// `ItemFrame.hurtServer`.
pub fn hurt_frame(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    source: &DamageSource,
) -> bool {
    let Some(d) = ents.decor(id) else {
        return false;
    };
    let DecorData::Frame(f) = &d.data else {
        return false;
    };
    let can_hurt_fixed = env.types.is(source.kind, tag::BYPASSES_INVULNERABILITY)
        || super::creative(ents, source.cause);
    if f.fixed {
        return can_hurt_fixed && super::hurt_attached(level, ents, env, cx, id, source);
    }
    if super::invulnerable_to_base(ents, env, d, source) {
        return false;
    }
    let drops_item = !env.types.is(source.kind, tag::IS_EXPLOSION) && !f.item.is_empty();
    if drops_item {
        let kind = ents.store.get(id).map_or(ids::ITEM_FRAME, |e| e.kind());
        drop_frame_item(level, ents, env, cx, id, source.cause, false);
        game_event(
            cx,
            ents,
            id,
            crate::level::game_event::events().block_change,
            source.cause,
        );
        super::play_sound(ents, env, id, &frame_sound(kind, "remove_item"), 1.0, 1.0);
        return true;
    }
    super::hurt_attached(level, ents, env, cx, id, source)
}

/// `ItemFrame.interact`: puts the held item in, or turns the framed item.
pub fn interact_frame(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    player: EntityId,
    id: EntityId,
    hand: Hand,
) -> Interaction {
    let Some(DecorData::Frame(f)) = ents.decor(id).map(|d| &d.data) else {
        return Interaction::Pass;
    };
    if f.fixed {
        return Interaction::Pass;
    }
    let has_item = !f.item.is_empty();
    let held = crate::mob::animal::held(ents, player, hand);
    let kind = ents.store.get(id).map_or(ids::ITEM_FRAME, |e| e.kind());
    if !has_item {
        if held.is_empty() || cx.discarded.contains(&id) {
            return Interaction::Pass;
        }
        if let Some(map) = crate::map::map_id(&held)
            && let Some(maps) = level.maps()
            && maps
                .with(map, |d| d.is_tracked_count_over_limit(256))
                .unwrap_or(false)
        {
            // `InteractionResult.FAIL`.
            return Interaction::Consume;
        }
        set_item(ents, env, cx, id, held);
        game_event(
            cx,
            ents,
            id,
            crate::level::game_event::events().block_change,
            Some(player),
        );
        crate::mob::animal::use_item(ents, player, hand);
        return Interaction::Success;
    }
    super::play_sound(ents, env, id, &frame_sound(kind, "rotate_item"), 1.0, 1.0);
    if let Some(DecorData::Frame(f)) = ents.decor_mut(id).map(|d| &mut d.data) {
        f.rotation = (f.rotation + 1) % 8;
        let pos = f.attached.pos;
        cx.actions.push(WorldAction::OutputSignal(pos));
    }
    if let Some(mut e) = ents.store.get_mut(id) {
        e.mark_data_dirty();
    }
    game_event(
        cx,
        ents,
        id,
        crate::level::game_event::events().block_change,
        Some(player),
    );
    Interaction::Success
}

/// `ServerEntity.sendChanges` of a frame with a map (every 10 ticks): every player of the
/// level gets the map, the map gets the frame marker.
pub fn tick_framed_map(level: &Level, ents: &mut Entities, env: &Env<'_>, id: EntityId) {
    let Some(DecorData::Frame(f)) = ents.decor(id).map(|d| &d.data) else {
        return;
    };
    if !crate::map::is_map_item(f.item.item) {
        return;
    }
    let Some(map) = crate::map::map_id(&f.item) else {
        return;
    };
    let Some(maps) = level.maps() else {
        return;
    };
    let Some(network_id) = ents.store.get(id).map(|e| e.network_id().0) else {
        return;
    };
    let frame = crate::map::Frame {
        pos: f.attached.pos,
        // `getDirection().get2DDataValue() * 90`: -90 for floor and ceiling frames.
        rotation: data_2d(f.attached.direction) * 90,
        entity_id: network_id,
    };
    let statics = crate::map::static_decorations(&f.item);
    let players: Vec<EntityId> = ents.players.iter().map(|(p, _)| p).collect();
    for p in players {
        let Some(e) = ents.store.get(p) else {
            continue;
        };
        let (pos, y_rot, uuid) = (e.pos(), e.rot().y_rot, e.uuid());
        let Some(player) = ents.player(p) else {
            continue;
        };
        let inv = &player.inventory;
        let has_map = (0..=crate::inventory::OFFHAND)
            .chain(36..40)
            .any(|i| crate::map::map_id(inv.get(i)) == Some(map));
        let carrier = crate::map::Carrier {
            uuid,
            name: player.profile.name.clone(),
            dimension: env.dimension.to_owned(),
            x: pos.x,
            z: pos.z,
            y_rot,
            has_map,
            invisible: crate::carried::map_invisible(inv),
        };
        let packet = maps.with(map, |d| {
            d.tick_carried_in(&carrier, &statics, env.game_time, Some(frame.clone()));
            d.next_update_packet(uuid, map)
        });
        if let Some(Some(packet)) = packet {
            player.connection.sink.send(&packet);
        }
    }
}

/// The synched data of a frame (`DATA_DIRECTION`, `DATA_ITEM`, `DATA_ROTATION`).
pub fn frame_data(w: &mut DataWriter<'_>, f: &Frame) {
    direction_data(w, f.attached.direction);
    w.stack(9, &f.item);
    w.int(10, f.rotation, 0);
}

/// The synched data of a painting (`DATA_DIRECTION`, `DATA_PAINTING_VARIANT_ID`).
pub fn painting_data(w: &mut DataWriter<'_>, p: &Painting) {
    direction_data(w, p.attached.direction);
    // `VariantUtils.getAny` is the first entry: id 0, sent as holder id + 1.
    w.varint(
        9,
        serializer::PAINTING_VARIANT,
        i32::try_from(p.variant).unwrap_or(0) + 1,
        1,
    );
}

fn direction_data(w: &mut DataWriter<'_>, d: Direction) {
    w.varint(
        8,
        serializer::DIRECTION,
        i32::from(d as u8),
        i32::from(Direction::South as u8),
    );
}

/// `ItemFrame.addAdditionalSaveData`.
pub fn save_frame(f: &Frame, invisible: bool, out: &mut Compound) {
    out.insert("block_pos", super::block_pos_tag(f.attached.pos));
    if f.item.is_empty() {
        out.remove("Item");
    } else {
        out.insert("Item", crate::inventory::stack_to_nbt(&f.item));
    }
    #[allow(clippy::cast_possible_truncation)]
    out.insert("ItemRotation", f.rotation as i8);
    out.insert("ItemDropChance", f.drop_chance);
    #[allow(clippy::cast_possible_wrap)]
    out.insert("Facing", f.attached.direction as u8 as i8);
    out.insert("Invisible", invisible);
    out.insert("Fixed", f.fixed);
}

/// `ItemFrame.readAdditionalSaveData`; also whether it is invisible.
pub fn load_frame(tag: &Compound, pos: Vec3) -> (Frame, bool) {
    let direction = super::int(tag, "Facing").map_or(Direction::Down, from_3d);
    let mut f = Frame::new(attached_pos(tag, pos), direction);
    f.item = match tag.get("Item") {
        Some(Tag::Compound(c)) => {
            let s = crate::inventory::stack_from_nbt(c);
            if s.is_empty() { s } else { s.with_count(1) }
        }
        _ => ItemStack::EMPTY,
    };
    f.rotation = super::int(tag, "ItemRotation").unwrap_or(0).rem_euclid(8);
    f.drop_chance = super::float(tag, "ItemDropChance").unwrap_or(1.0);
    f.fixed = super::flag(tag, "Fixed");
    (f, super::flag(tag, "Invisible"))
}

/// `BlockAttachedEntity.readAdditionalSaveData`: the stored block when it is within 16 blocks
/// of the entity, else the entity's own block.
pub(crate) fn attached_pos(tag: &Compound, pos: Vec3) -> BlockPos {
    let own = BlockPos::containing(pos.x, pos.y, pos.z);
    match super::block_pos_of(tag, "block_pos") {
        Some(p) if p.dist_sqr(own) < 256.0 => p,
        _ => own,
    }
}

/// `Painting.addAdditionalSaveData`.
pub fn save_painting(p: &Painting, out: &mut Compound) {
    #[allow(clippy::cast_possible_truncation)]
    out.insert("facing", data_2d(p.attached.direction) as i8);
    out.insert("block_pos", super::block_pos_tag(p.attached.pos));
    use vanillite_protocol::components_nbt::RegistryNames;
    if let Some(name) = vanillite_protocol::item::InstalledRegistries
        .entry_name("minecraft:painting_variant", p.variant as usize)
    {
        out.insert("variant", name.to_owned());
    }
}

/// `Painting.readAdditionalSaveData`.
pub fn load_painting(tag: &Compound, pos: Vec3) -> Painting {
    use vanillite_protocol::components_nbt::RegistryNames;
    let direction = super::int(tag, "facing").map_or(Direction::South, super::from_2d);
    let variant = tag
        .get("variant")
        .and_then(Tag::as_str)
        .and_then(|n| {
            let full = if n.contains(':') {
                n.to_owned()
            } else {
                format!("minecraft:{n}")
            };
            vanillite_protocol::item::InstalledRegistries
                .entry_id("minecraft:painting_variant", &full)
        })
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    Painting {
        attached: Attached {
            pos: attached_pos(tag, pos),
            direction,
            checks: 0,
        },
        variant,
    }
}

/// `ComparatorBlock.getItemFrame`: the single frame facing `direction` in the block at `pos`.
pub fn comparator_frame(level: &Level, pos: BlockPos, direction: Direction) -> Option<i32> {
    let b = Aabb::of_block(pos);
    let ents = &level.entities;
    let mut found = None;
    let mut count = 0;
    ents.store.for_each_non_player_in(&b, |id| {
        let Some(e) = ents.store.get(id) else {
            return;
        };
        if !e.aabb().intersects(&b) {
            return;
        }
        if let Some(DecorData::Frame(f)) = ents.decor(id).map(|d| &d.data)
            && f.attached.direction == direction
        {
            count += 1;
            found = Some(f.analog_output());
        }
    });
    if count == 1 { found } else { None }
}
