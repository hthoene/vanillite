//! Entity movement against blocks: the collision boxes around a box (`BlockCollisions`),
//! `Shapes.collide`, `Entity.collide` with stepping up, and block ray casts (`Level.clip` with
//! `ClipContext.Block.COLLIDER`).

use vanillite_base::math::{floor, frac};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::{BlockState, Shape, flags, ids};

use vanillite_worldgen::behaviour::StateExt;

use crate::level::Level;
use crate::level::border::WorldBorder;

/// `Shapes.EPSILON`.
const EPSILON: f64 = 1.0e-7;

/// Read access to the blocks of a world (the level, or a test grid).
pub trait Blocks {
    /// `getBlockState(pos)`.
    fn state(&self, pos: BlockPos) -> BlockState;
    /// [`Blocks::state`] of every position in the box from `min` to `max` (inclusive) into
    /// `out`, which holds exactly that many, in X, then Z, then Y order (Y varies fastest).
    fn states_in(&self, min: BlockPos, max: BlockPos, out: &mut [BlockState]) {
        let mut slots = out.iter_mut();
        for x in min.x..=max.x {
            for z in min.z..=max.z {
                for y in min.y..=max.y {
                    if let Some(slot) = slots.next() {
                        *slot = self.state(BlockPos::new(x, y, z));
                    }
                }
            }
        }
    }
    /// `getWorldBorder`, which entities collide with; `None` without one (tests).
    fn border(&self) -> Option<&WorldBorder> {
        None
    }
    /// The collision boxes of the `moving_piston` at `pos` relative to it
    /// (`MovingPistonBlock.getCollisionShape` from its block entity); none without access to
    /// block entities.
    fn moving_piston_boxes(&self, _pos: BlockPos, _out: &mut Vec<[f64; 6]>) {}
    /// Whether no block of the section at section coordinates (`x`, `y`, `z`) has a special
    /// collision shape (large, dynamic like a moving piston, or neither empty nor a full cube),
    /// so the ring and edge cells of [`block_boxes`] there add nothing. `false` = unknown.
    fn plain_collision(&self, _x: i32, _y: i32, _z: i32) -> bool {
        false
    }
}

impl Blocks for Level {
    #[inline]
    fn state(&self, pos: BlockPos) -> BlockState {
        self.block(pos)
    }

    fn states_in(&self, min: BlockPos, max: BlockPos, out: &mut [BlockState]) {
        let mut slots = out.iter_mut();
        let mut y = min.y;
        self.chunks.blocks_in(min, max, |state| {
            if let Some(slot) = slots.next() {
                *slot = match state {
                    Some(state) => BlockState(state),
                    None if self.in_height(y) => BlockState::AIR,
                    None => crate::block::VOID_AIR,
                };
            }
            y = if y == max.y { min.y } else { y + 1 };
        });
    }

    #[inline]
    fn border(&self) -> Option<&WorldBorder> {
        Some(&self.border)
    }

    fn moving_piston_boxes(&self, pos: BlockPos, out: &mut Vec<[f64; 6]>) {
        crate::block::piston::collision_boxes_at(&self.block_entities, pos, out);
    }
}

/// The world border's part of `Entity.collectColliders`: when the entity of `entity` box is
/// inside and close to the border for a move through `area` (`isInsideCloseToBorder`), the
/// border's collision shape (everything outside the block-aligned border box) as four boxes.
pub fn border_boxes(border: &WorldBorder, entity: &Aabb, area: &Aabb, out: &mut Vec<Aabb>) {
    let x = (entity.min_x + entity.max_x) / 2.0;
    let z = (entity.min_z + entity.max_z) / 2.0;
    if !border.is_inside_close_to_border(x, z, area.max_x - area.min_x, area.max_z - area.min_z) {
        return;
    }
    let (min_x, min_z, max_x, max_z) = border.collision_box();
    let inf = f64::INFINITY;
    out.push(Aabb::new(-inf, -inf, -inf, min_x, inf, inf));
    out.push(Aabb::new(max_x, -inf, -inf, inf, inf, inf));
    out.push(Aabb::new(min_x, -inf, -inf, max_x, inf, min_z));
    out.push(Aabb::new(min_x, -inf, max_z, max_x, inf, inf));
}

/// Appends the collision boxes (world coordinates) of the blocks around `area`, like
/// `BlockCollisions`: the cells the box touches plus a ring around it, where ring cells only
/// count with a large collision shape (fences, walls) and edge cells only for moving pistons.
pub fn block_boxes(blocks: &dyn Blocks, area: &Aabb, out: &mut Vec<Aabb>) {
    block_boxes_for(blocks, area, out, None, SnowCollision::None, None);
}

/// The collision shape of `state` at height `oy` for an entity with its feet at `bottom`
/// that does not descend (`EntityCollisionContext`; `None`: the empty context): scaffolding
/// holds such an entity only on its top (`SHAPE_STABLE`) or, for an unstable bottom piece, on
/// its bottom plate (`ScaffoldingBlock.getCollisionShape`).
pub fn context_shape(state: BlockState, oy: f64, bottom: Option<f64>) -> Shape {
    let Some(bottom) = bottom else {
        return state.collision_shape();
    };
    if state.block() != ids::SCAFFOLDING {
        return state.collision_shape();
    }
    let eps = f64::from(1.0e-5_f32);
    if bottom > oy + 1.0 - eps {
        state.collision_shape()
    } else if state.int("distance") != Some(0) && state.flag("bottom") && bottom > oy - eps {
        &[[0.0, 0.0, 0.0, 1.0, 0.125, 1.0]]
    } else {
        &[]
    }
}

/// `state.getOffset(pos)` in X and Z of the blocks whose collision shape moves with their
/// random offset (`OffsetType.XZ` with a collision shape: bamboo, pointed dripstone, sulfur
/// spikes; other blocks have no offset or no collision): `(0, 0)` for every other block.
pub fn collision_offset(state: BlockState, x: i32, z: i32) -> (f64, f64) {
    let max = f64::from(state.block().info().max_horizontal_offset);
    if max == 0.0 {
        return (0.0, 0.0);
    }
    let seed = vanillite_base::math::get_seed(x, 0, z);
    #[allow(clippy::cast_precision_loss)]
    let at = |bits: i64| ((f64::from((bits & 15) as f32 / 15.0) - 0.5) * 0.5).clamp(-max, max);
    (at(seed), at(seed >> 8))
}

/// How powder snow collides with an entity (`PowderSnowBlock.getCollisionShape` with the
/// entity's context); its plain collision shape is empty.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum SnowCollision {
    /// Entities sink in.
    #[default]
    None,
    /// `fallDistance > 2.5`: the falling shape, 0.9 blocks high.
    Falling,
    /// `canEntityWalkOnPowderSnow` (powder snow walkable mobs, leather boots) with the feet
    /// at this height: a full block below the feet, none above.
    Walker(f64),
}

/// `PowderSnowBlock.FALLING_COLLISION_SHAPE` top: `0.9F`.
const SNOW_FALLING_TOP: f64 = 0.899_999_976_158_142_1;

/// `getLiquidCollisionShape` of the mobs that stand on lava (`Strider`): the lower half of a
/// lava source.
const LAVA_STABLE_TOP: f64 = 0.5;

/// [`block_boxes`] for an entity that stands on lava (`canStandOnFluid`) with its feet at
/// `lava_bottom` (`EntityCollisionContext.entityBottom`): lava sources without lava above
/// collide with their lower half when the feet are above it (`LiquidBlock.getCollisionShape`).
/// Blocks of a collision box read into a buffer on the stack; larger boxes spill to the heap.
const BOX_READ_INLINE: usize = 512;

pub fn block_boxes_for(
    blocks: &dyn Blocks,
    area: &Aabb,
    out: &mut Vec<Aabb>,
    lava_bottom: Option<f64>,
    snow: SnowCollision,
    entity_bottom: Option<f64>,
) {
    let min_x = floor(area.min_x - EPSILON) - 1;
    let max_x = floor(area.max_x + EPSILON) + 1;
    let min_y = floor(area.min_y - EPSILON) - 1;
    let max_y = floor(area.max_y + EPSILON) + 1;
    let min_z = floor(area.min_z - EPSILON) - 1;
    let max_z = floor(area.max_z + EPSILON) + 1;
    // Ring and edge cells only count with a large collision shape or a moving piston: without
    // such blocks in the sections around, only the inner cells are read (same boxes, same
    // order).
    let inner = (min_x >> 4..=max_x >> 4).all(|sx| {
        (min_z >> 4..=max_z >> 4)
            .all(|sz| (min_y >> 4..=max_y >> 4).all(|sy| blocks.plain_collision(sx, sy, sz)))
    });
    let shrink = i32::from(inner);
    let (read_min, read_max) = (
        BlockPos::new(min_x + shrink, min_y + shrink, min_z + shrink),
        BlockPos::new(max_x - shrink, max_y - shrink, max_z - shrink),
    );
    // The box's states in one read, in the loops' order.
    let volume = [
        read_max.x - read_min.x,
        read_max.y - read_min.y,
        read_max.z - read_min.z,
    ]
    .iter()
    .map(|&d| usize::try_from(d + 1).unwrap_or(0))
    .product::<usize>();
    let mut inline = [BlockState::AIR; BOX_READ_INLINE];
    let mut spilled = Vec::new();
    let states: &mut [BlockState] = if volume <= BOX_READ_INLINE {
        &mut inline[..volume]
    } else {
        spilled.resize(volume, BlockState::AIR);
        &mut spilled
    };
    blocks.states_in(read_min, read_max, states);
    let mut states = states.iter().copied();
    for x in read_min.x..=read_max.x {
        for z in read_min.z..=read_max.z {
            for y in read_min.y..=read_max.y {
                let Some(state) = states.next() else {
                    return;
                };
                let edges = i32::from(x == min_x || x == max_x)
                    + i32::from(y == min_y || y == max_y)
                    + i32::from(z == min_z || z == max_z);
                if edges == 3 {
                    continue;
                }
                let pos = BlockPos::new(x, y, z);
                if state.is_air() {
                    continue;
                }
                if edges == 1 && !state.has(flags::LARGE_COLLISION_SHAPE) {
                    continue;
                }
                if edges == 2 && state.block() != ids::MOVING_PISTON {
                    continue;
                }
                let (ox, oy, oz) = (f64::from(x), f64::from(y), f64::from(z));
                if let Some(bottom) = lava_bottom
                    && state.block() == ids::LAVA
                    && state.int("level") == Some(0)
                    && bottom > oy + LAVA_STABLE_TOP - f64::from(1.0e-5_f32)
                    && blocks.state(BlockPos::new(x, y + 1, z)).block() != ids::LAVA
                {
                    let shape = Aabb {
                        min_x: ox,
                        min_y: oy,
                        min_z: oz,
                        max_x: ox + 1.0,
                        max_y: oy + LAVA_STABLE_TOP,
                        max_z: oz + 1.0,
                    };
                    if shape.intersects(area) {
                        out.push(shape);
                    }
                    continue;
                }
                if snow != SnowCollision::None && state.block() == ids::POWDER_SNOW {
                    let top = match snow {
                        SnowCollision::Falling => Some(SNOW_FALLING_TOP),
                        // `isAbove(Shapes.block(), pos, false)`.
                        SnowCollision::Walker(bottom)
                            if bottom > oy + 1.0 - f64::from(1.0e-5_f32) =>
                        {
                            Some(1.0)
                        }
                        _ => None,
                    };
                    if let Some(top) = top {
                        let shape = Aabb {
                            min_x: ox,
                            min_y: oy,
                            min_z: oz,
                            max_x: ox + 1.0,
                            max_y: oy + top,
                            max_z: oz + 1.0,
                        };
                        if shape.intersects(area) {
                            out.push(shape);
                        }
                    }
                    continue;
                }
                if state.block() == ids::MOVING_PISTON {
                    let mut moving = Vec::new();
                    blocks.moving_piston_boxes(pos, &mut moving);
                    for b in moving {
                        let shape = Aabb {
                            min_x: ox + b[0],
                            min_y: oy + b[1],
                            min_z: oz + b[2],
                            max_x: ox + b[3],
                            max_y: oy + b[4],
                            max_z: oz + b[5],
                        };
                        if shape.intersects(area) {
                            out.push(shape);
                        }
                    }
                    continue;
                }
                let (dx, dz) = collision_offset(state, x, z);
                for b in context_shape(state, oy, entity_bottom) {
                    let shape = Aabb {
                        min_x: ox + (b[0] + dx),
                        min_y: oy + b[1],
                        min_z: oz + (b[2] + dz),
                        max_x: ox + (b[3] + dx),
                        max_y: oy + b[4],
                        max_z: oz + (b[5] + dz),
                    };
                    if shape.intersects(area) {
                        out.push(shape);
                    }
                }
            }
        }
    }
}

fn axis_min(b: &Aabb, axis: usize) -> f64 {
    match axis {
        0 => b.min_x,
        1 => b.min_y,
        _ => b.min_z,
    }
}

fn axis_max(b: &Aabb, axis: usize) -> f64 {
    match axis {
        0 => b.max_x,
        1 => b.max_y,
        _ => b.max_z,
    }
}

/// `Shapes.collide(axis, aabb, shapes, distance)`: how far `aabb` moves along `axis` (0 = X,
/// 1 = Y, 2 = Z) before it touches one of `boxes`.
pub fn collide_axis(boxes: &[Aabb], aabb: &Aabb, axis: usize, delta: f64) -> f64 {
    if delta.abs() < EPSILON {
        return 0.0;
    }
    let mut delta = delta;
    for b in boxes {
        let overlaps = (0..3).filter(|&a| a != axis).all(|a| {
            axis_max(b, a) - EPSILON > axis_min(aabb, a)
                && axis_min(b, a) + EPSILON < axis_max(aabb, a)
        });
        if !overlaps {
            continue;
        }
        if delta > 0.0 && axis_min(b, axis) >= axis_max(aabb, axis) - EPSILON {
            delta = delta.min(axis_min(b, axis) - axis_max(aabb, axis));
        } else if delta < 0.0 && axis_max(b, axis) <= axis_min(aabb, axis) + EPSILON {
            delta = delta.max(axis_max(b, axis) - axis_min(aabb, axis));
        }
        if delta.abs() < EPSILON {
            return 0.0;
        }
    }
    delta
}

fn offset_axis(aabb: Aabb, axis: usize, d: f64) -> Aabb {
    match axis {
        0 => aabb.offset(d, 0.0, 0.0),
        1 => aabb.offset(0.0, d, 0.0),
        _ => aabb.offset(0.0, 0.0, d),
    }
}

/// `Entity.collideWithShapes`: Y first, then the larger of X and Z (`Direction.axisStepOrder`).
pub fn collide_with_shapes(movement: Vec3, aabb: Aabb, boxes: &[Aabb]) -> Vec3 {
    if boxes.is_empty() {
        return movement;
    }
    let order = if movement.x.abs() < movement.z.abs() {
        [1, 2, 0]
    } else {
        [1, 0, 2]
    };
    let wanted = [movement.x, movement.y, movement.z];
    let mut out = [0.0; 3];
    let mut moved = aabb;
    for axis in order {
        let d = wanted[axis];
        if d != 0.0 {
            let c = collide_axis(boxes, &moved, axis, d);
            out[axis] = c;
            moved = offset_axis(moved, axis, c);
        }
    }
    Vec3::new(out[0], out[1], out[2])
}

/// `Entity.collide`: the movement left after the blocks in the way, stepping up to
/// `max_up_step` blocks when that gets further horizontally. `scratch` is reused between
/// calls.
pub fn collide(
    blocks: &dyn Blocks,
    aabb: Aabb,
    movement: Vec3,
    max_up_step: f32,
    on_ground: bool,
    scratch: &mut Vec<Aabb>,
) -> Vec3 {
    collide_for(
        blocks,
        aabb,
        movement,
        max_up_step,
        on_ground,
        scratch,
        false,
    )
}

/// [`collide`] for an entity that stands on lava when `on_lava` (`Strider.canStandOnFluid`):
/// its feet are the bottom of `aabb`.
pub fn collide_for(
    blocks: &dyn Blocks,
    aabb: Aabb,
    movement: Vec3,
    max_up_step: f32,
    on_ground: bool,
    scratch: &mut Vec<Aabb>,
    on_lava: bool,
) -> Vec3 {
    collide_in_context(
        blocks,
        aabb,
        movement,
        max_up_step,
        on_ground,
        scratch,
        on_lava,
        SnowCollision::None,
        false,
    )
}

/// [`collide_for`] with the entity's powder snow collision.
#[allow(clippy::too_many_arguments)]
pub fn collide_in_context(
    blocks: &dyn Blocks,
    aabb: Aabb,
    movement: Vec3,
    max_up_step: f32,
    on_ground: bool,
    scratch: &mut Vec<Aabb>,
    on_lava: bool,
    snow: SnowCollision,
    entity: bool,
) -> Vec3 {
    let lava_bottom = on_lava.then_some(aabb.min_y);
    let entity_bottom = entity.then_some(aabb.min_y);
    let step = if movement.length_sqr() == 0.0 {
        movement
    } else {
        scratch.clear();
        let area = aabb.expand_towards(movement.x, movement.y, movement.z);
        block_boxes_for(blocks, &area, scratch, lava_bottom, snow, entity_bottom);
        if let Some(border) = blocks.border() {
            border_boxes(border, &aabb, &area, scratch);
        }
        collide_with_shapes(movement, aabb, scratch)
    };
    let x_collision = movement.x != step.x;
    let y_collision = movement.y != step.y;
    let z_collision = movement.z != step.z;
    let grounded_after = y_collision && movement.y < 0.0;
    if max_up_step > 0.0 && (grounded_after || on_ground) && (x_collision || z_collision) {
        let grounded = if grounded_after {
            aabb.offset(0.0, step.y, 0.0)
        } else {
            aabb
        };
        let mut area = grounded.expand_towards(movement.x, f64::from(max_up_step), movement.z);
        if !grounded_after {
            area = area.expand_towards(0.0, -1.0e-5, 0.0);
        }
        scratch.clear();
        block_boxes_for(blocks, &area, scratch, lava_bottom, snow, entity_bottom);
        if let Some(border) = blocks.border() {
            border_boxes(border, &aabb, &area, scratch);
        }
        // `(float)movementStep.y`.
        #[allow(clippy::cast_possible_truncation)]
        let skip = step.y as f32;
        let mut candidates: Vec<f32> = Vec::with_capacity(4);
        for b in scratch.iter() {
            for coord in [b.min_y, b.max_y] {
                #[allow(clippy::cast_possible_truncation)]
                let relative = (coord - grounded.min_y) as f32;
                if relative < 0.0 || relative == skip || relative > max_up_step {
                    continue;
                }
                if !candidates.contains(&relative) {
                    candidates.push(relative);
                }
            }
        }
        candidates.sort_by(f32::total_cmp);
        for height in candidates {
            let try_step = collide_with_shapes(
                Vec3::new(movement.x, f64::from(height), movement.z),
                grounded,
                scratch,
            );
            let horizontal = |v: Vec3| v.x * v.x + v.z * v.z;
            if horizontal(try_step) > horizontal(step) {
                let to_ground = aabb.min_y - grounded.min_y;
                return Vec3::new(try_step.x, try_step.y - to_ground, try_step.z);
            }
        }
    }
    step
}

/// `ServerGamePacketListenerImpl.isEntityCollidingWithAnythingNew` against blocks: whether
/// the box `new` (deflated by `1.0E-5F`) collides with a block whose collision shape does not
/// already collide with `old` (deflated the same way), i.e. the move enters a block.
pub fn collides_with_anything_new(blocks: &dyn Blocks, old: &Aabb, new: &Aabb) -> bool {
    let d = f64::from(1.0e-5_f32);
    let (old, new) = (old.inflate(-d, -d, -d), new.inflate(-d, -d, -d));
    let min_x = floor(new.min_x - EPSILON) - 1;
    let max_x = floor(new.max_x + EPSILON) + 1;
    let min_y = floor(new.min_y - EPSILON) - 1;
    let max_y = floor(new.max_y + EPSILON) + 1;
    let min_z = floor(new.min_z - EPSILON) - 1;
    let max_z = floor(new.max_z + EPSILON) + 1;
    for x in min_x..=max_x {
        for z in min_z..=max_z {
            for y in min_y..=max_y {
                let edges = i32::from(x == min_x || x == max_x)
                    + i32::from(y == min_y || y == max_y)
                    + i32::from(z == min_z || z == max_z);
                if edges == 3 {
                    continue;
                }
                let state = blocks.state(BlockPos::new(x, y, z));
                if state.is_air()
                    || (edges == 1 && !state.has(flags::LARGE_COLLISION_SHAPE))
                    || (edges == 2 && state.block() != ids::MOVING_PISTON)
                {
                    continue;
                }
                let (ox, oy, oz) = (f64::from(x), f64::from(y), f64::from(z));
                let boxes = state.collision_shape().iter().map(|b| Aabb {
                    min_x: ox + b[0],
                    min_y: oy + b[1],
                    min_z: oz + b[2],
                    max_x: ox + b[3],
                    max_y: oy + b[4],
                    max_z: oz + b[5],
                });
                // The whole block shape counts: new when it touches `new` but not `old`.
                let mut hits_new = false;
                let mut hits_old = false;
                for b in boxes {
                    hits_new |= b.intersects(&new);
                    hits_old |= b.intersects(&old);
                }
                if hits_new && !hits_old {
                    return true;
                }
            }
        }
    }
    false
}

/// `noCollision(entity, box)` against blocks only.
pub fn no_collision(blocks: &dyn Blocks, aabb: &Aabb, scratch: &mut Vec<Aabb>) -> bool {
    scratch.clear();
    block_boxes(blocks, aabb, scratch);
    // `block_boxes` keeps boxes that intersect; `noCollision` ignores touching faces.
    !scratch.iter().any(|b| {
        b.max_x - EPSILON > aabb.min_x
            && b.min_x + EPSILON < aabb.max_x
            && b.max_y - EPSILON > aabb.min_y
            && b.min_y + EPSILON < aabb.max_y
            && b.max_z - EPSILON > aabb.min_z
            && b.min_z + EPSILON < aabb.max_z
    })
}

/// `containsAnyLiquid(box)`.
pub fn contains_any_liquid(blocks: &dyn Blocks, aabb: &Aabb) -> bool {
    let (x0, y0, z0) = (floor(aabb.min_x), floor(aabb.min_y), floor(aabb.min_z));
    let (x1, y1, z1) = (
        vanillite_base::math::ceil(aabb.max_x),
        vanillite_base::math::ceil(aabb.max_y),
        vanillite_base::math::ceil(aabb.max_z),
    );
    for x in x0..x1 {
        for y in y0..y1 {
            for z in z0..z1 {
                if !blocks
                    .state(BlockPos::new(x, y, z))
                    .fluid_state()
                    .is_empty()
                {
                    return true;
                }
            }
        }
    }
    false
}

/// `VoxelShape.clip(from, to, pos)` of a shape made of `boxes` (block-local): the fraction of
/// the segment where it hits the shape. A segment that starts inside (its point at 0.001)
/// hits at once; otherwise it must enter a box through a face (`AABB.clip`), so a segment
/// leaving a surface it starts on does not hit.
fn shape_clip(from: Vec3, to: Vec3, pos: BlockPos, boxes: &[[f64; 6]]) -> Option<f64> {
    if boxes.is_empty() {
        return None;
    }
    let d = [to.x - from.x, to.y - from.y, to.z - from.z];
    if d[0] * d[0] + d[1] * d[1] + d[2] * d[2] < 1.0e-7 {
        return None;
    }
    let o = [f64::from(pos.x), f64::from(pos.y), f64::from(pos.z)];
    let f = [from.x, from.y, from.z];
    let test = [
        f[0] + d[0] * 0.001 - o[0],
        f[1] + d[1] * 0.001 - o[1],
        f[2] + d[2] * 0.001 - o[2],
    ];
    if boxes
        .iter()
        .any(|b| (0..3).all(|a| b[a] <= test[a] && test[a] < b[a + 3]))
    {
        return Some(0.001);
    }
    // `AABB.clip` → `getDirection` → `clipPoint` for each box.
    let mut scale = 1.0;
    let mut hit = false;
    for b in boxes {
        for a in 0..3 {
            let face = if d[a] > 1.0e-7 {
                b[a] + o[a]
            } else if d[a] < -1.0e-7 {
                b[a + 3] + o[a]
            } else {
                continue;
            };
            let s = (face - f[a]) / d[a];
            let (ib, ic) = ((a + 1) % 3, (a + 2) % 3);
            let pb = f[ib] + s * d[ib];
            let pc = f[ic] + s * d[ic];
            if 0.0 < s
                && s < scale
                && b[ib] + o[ib] - 1.0e-7 < pb
                && pb < b[ib + 3] + o[ib] + 1.0e-7
                && b[ic] + o[ic] - 1.0e-7 < pc
                && pc < b[ic + 3] + o[ic] + 1.0e-7
            {
                scale = s;
                hit = true;
            }
        }
    }
    hit.then_some(scale)
}

/// `Level.clip` with `ClipContext.Block.COLLIDER` and `Fluid.NONE`: the first block whose
/// collision shape the segment hits (`BlockGetter.traverseBlocks`), with the hit point.
pub fn clip(blocks: &dyn Blocks, from: Vec3, to: Vec3) -> Option<(BlockPos, Vec3)> {
    clip_with(blocks, from, to, Fluids::None, Shapes::Collider)
}

/// `Level.clip` with `ClipContext.Block.VISUAL` and `Fluid.NONE`: glass, iron bars, powder
/// snow and the like do not stop it.
pub fn clip_visual(blocks: &dyn Blocks, from: Vec3, to: Vec3) -> Option<(BlockPos, Vec3)> {
    clip_with(blocks, from, to, Fluids::None, Shapes::Visual)
}

/// `Level.clip` with `ClipContext.Block.COLLIDER` and `Fluid.SOURCE_ONLY`: also stops at the
/// surfaces of source fluids.
pub fn clip_source_fluids(blocks: &dyn Blocks, from: Vec3, to: Vec3) -> Option<(BlockPos, Vec3)> {
    clip_with(blocks, from, to, Fluids::Source, Shapes::Collider)
}

/// `ClipContext.Fluid`.
#[derive(Clone, Copy, PartialEq)]
enum Fluids {
    None,
    Source,
    Any,
}

/// `Level.clip` with `ClipContext.Block.COLLIDER` and `Fluid.ANY`: also stops at fluid
/// surfaces (a fluid's shape is its height in the block).
pub fn clip_fluids(blocks: &dyn Blocks, from: Vec3, to: Vec3) -> Option<(BlockPos, Vec3)> {
    clip_with(blocks, from, to, Fluids::Any, Shapes::Collider)
}

/// `Level.clip` with `ClipContext.Block.OUTLINE` and `Fluid.ANY` (`BoatItem.use`).
pub fn clip_outline_fluids(blocks: &dyn Blocks, from: Vec3, to: Vec3) -> Option<(BlockPos, Vec3)> {
    clip_with(blocks, from, to, Fluids::Any, Shapes::Outline)
}

/// `ClipContext.Block`.
#[derive(Clone, Copy, PartialEq)]
enum Shapes {
    Collider,
    Visual,
    Outline,
}

fn clip_with(
    blocks: &dyn Blocks,
    from: Vec3,
    to: Vec3,
    fluids: Fluids,
    shapes: Shapes,
) -> Option<(BlockPos, Vec3)> {
    if from == to {
        return None;
    }
    let lerp = |t: f64, a: f64, b: f64| a + t * (b - a);
    let to_x = lerp(-1.0e-7, to.x, from.x);
    let to_y = lerp(-1.0e-7, to.y, from.y);
    let to_z = lerp(-1.0e-7, to.z, from.z);
    let from_x = lerp(-1.0e-7, from.x, to.x);
    let from_y = lerp(-1.0e-7, from.y, to.y);
    let from_z = lerp(-1.0e-7, from.z, to.z);
    let mut bx = floor(from_x);
    let mut by = floor(from_y);
    let mut bz = floor(from_z);
    let hit = |pos: BlockPos| -> Option<(BlockPos, Vec3)> {
        let state = blocks.state(pos);
        let shape = match shapes {
            Shapes::Visual => state.visual_shape(),
            Shapes::Outline => state.outline_shape(),
            Shapes::Collider => state.collision_shape(),
        };
        let block = shape_clip(from, to, pos, shape);
        let fluid = state.fluid_state();
        let takes = match fluids {
            Fluids::None => false,
            Fluids::Any => !fluid.is_empty(),
            Fluids::Source => !fluid.is_empty() && fluid.info().source,
        };
        let liquid = if takes {
            let above = blocks
                .state(BlockPos::new(pos.x, pos.y + 1, pos.z))
                .fluid_state();
            let height = if !above.is_empty() && above.info().fluid == fluid.info().fluid {
                1.0
            } else {
                f64::from(fluid.info().own_height)
            };
            shape_clip(from, to, pos, &[[0.0, 0.0, 0.0, 1.0, height, 1.0]])
        } else {
            None
        };
        // The nearer of the two, the block's on a tie.
        let best = match (block, liquid) {
            (Some(b), Some(l)) => Some(b.min(l)),
            (b, l) => b.or(l),
        };
        best.map(|t| {
            (
                pos,
                Vec3::new(
                    from.x + (to.x - from.x) * t,
                    from.y + (to.y - from.y) * t,
                    from.z + (to.z - from.z) * t,
                ),
            )
        })
    };
    if let Some(h) = hit(BlockPos::new(bx, by, bz)) {
        return Some(h);
    }
    let (dx, dy, dz) = (to_x - from_x, to_y - from_y, to_z - from_z);
    let sign = |d: f64| -> i32 {
        if d > 0.0 {
            1
        } else if d < 0.0 {
            -1
        } else {
            0
        }
    };
    let (sx, sy, sz) = (sign(dx), sign(dy), sign(dz));
    let delta = |s: i32, d: f64| {
        if s == 0 { f64::MAX } else { f64::from(s) / d }
    };
    let (tdx, tdy, tdz) = (delta(sx, dx), delta(sy, dy), delta(sz, dz));
    let start = |s: i32, td: f64, f: f64| td * if s > 0 { 1.0 - frac(f) } else { frac(f) };
    let mut tx = start(sx, tdx, from_x);
    let mut ty = start(sy, tdy, from_y);
    let mut tz = start(sz, tdz, from_z);
    while tx <= 1.0 || ty <= 1.0 || tz <= 1.0 {
        if tx < ty {
            if tx < tz {
                bx += sx;
                tx += tdx;
            } else {
                bz += sz;
                tz += tdz;
            }
        } else if ty < tz {
            by += sy;
            ty += tdy;
        } else {
            bz += sz;
            tz += tdz;
        }
        if let Some(h) = hit(BlockPos::new(bx, by, bz)) {
            return Some(h);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Floor;
    impl Blocks for Floor {
        fn state(&self, pos: BlockPos) -> BlockState {
            if pos.y < 64 || (pos.y == 64 && pos.x == 2) {
                vanillite_data::block::ids::STONE.default_state()
            } else {
                BlockState::AIR
            }
        }
    }

    fn mob_box(pos: Vec3) -> Aabb {
        Aabb::new(
            pos.x - 0.3,
            pos.y,
            pos.z - 0.3,
            pos.x + 0.3,
            pos.y + 1.8,
            pos.z + 0.3,
        )
    }

    #[test]
    fn lands_on_the_floor() {
        let mut scratch = Vec::new();
        let moved = collide(
            &Floor,
            mob_box(Vec3::new(0.5, 64.5, 0.5)),
            Vec3::new(0.0, -1.0, 0.0),
            0.6,
            false,
            &mut scratch,
        );
        assert!((moved.y + 0.5).abs() < 1.0e-9);
    }

    #[test]
    fn steps_up_a_block_only_with_jump_height() {
        let mut scratch = Vec::new();
        // A full block is higher than the 0.6 step height: the move stops at the wall.
        let moved = collide(
            &Floor,
            mob_box(Vec3::new(1.5, 64.0, 0.5)),
            Vec3::new(0.3, 0.0, 0.0),
            0.6,
            true,
            &mut scratch,
        );
        assert!((moved.x - 0.2).abs() < 1.0e-9);
        assert_eq!(moved.y, 0.0);
    }

    #[test]
    fn clip_hits_the_wall() {
        let hit = clip(&Floor, Vec3::new(0.5, 64.5, 0.5), Vec3::new(4.5, 64.5, 0.5));
        assert_eq!(hit.map(|h| h.0), Some(BlockPos::new(2, 64, 0)));
        assert!(clip(&Floor, Vec3::new(0.5, 65.5, 0.5), Vec3::new(4.5, 65.5, 0.5)).is_none());
    }
}
