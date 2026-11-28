//! `Entity.checkInsideBlocks`: the blocks an entity's box passes on the movements of a tick
//! (`movementThisTick`), in Vanilla's order (`BlockGetter.forEachBlockIntersectedBetween`).
//!
//! A movement recorded by `Entity.move` is walked axis by axis (Y first, then the larger of X
//! and Z); every walk visits the blocks at its start, the blocks the leading corner of the box
//! enters on the way, then the rest of the box at its end. A block acts once per tick.

use vanillite_base::math::floor;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::{BlockKind, BlockState};
use vanillite_world::entity::Dimensions;
use vanillite_worldgen::behaviour::StateExt;

use super::blocks::{BlockAccess, BlockFacts, fluid_height};

/// `Entity.Movement`: one move of an entity within a tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Movement {
    pub from: Vec3,
    pub to: Vec3,
    /// `axisDependentOriginalMovement`: the movement `Entity.move` was asked for; such a
    /// movement is walked axis by axis.
    pub original: Option<Vec3>,
}

impl Movement {
    /// A movement that is walked straight.
    pub fn plain(from: Vec3, to: Vec3) -> Movement {
        Movement {
            from,
            to,
            original: None,
        }
    }

    /// The entity where it stands.
    pub fn still(pos: Vec3) -> Movement {
        Movement::plain(pos, pos)
    }
}

/// `addMovementThisTick`: at most 100 movements a tick, the two oldest join beyond that.
pub fn record(movements: &mut Vec<Movement>, movement: Movement) {
    if movements.len() >= 100 {
        let second = movements.remove(1);
        if let Some(first) = movements.first_mut() {
            *first = Movement::plain(first.from, second.to);
        }
    }
    movements.push(movement);
}

/// The list `applyEffectsFromBlocks()` walks: the recorded movements and the way from their
/// end to where the entity is now; without a movement the way since the tick started.
pub fn finish(movements: &mut Vec<Movement>, old_pos: Vec3, pos: Vec3) {
    match movements.last() {
        None => movements.push(Movement::plain(old_pos, pos)),
        Some(last) => {
            if last.to.distance_to_sqr(pos) > f64::from(9.999_999_4e-11_f32) {
                let from = last.to;
                movements.push(Movement::plain(from, pos));
            }
        }
    }
}

/// What a walk ([`Walker::walk`]) gave, worked out ahead on any thread (`value`): it stands
/// for the walk of exactly the same way, size and fall while the blocks keep the same
/// [`BlockAccess::stamp`], so the walk can be left out then ([`WalkAhead::get`]).
#[derive(Clone, Debug, Default)]
pub struct WalkAhead<T> {
    stamp: Option<u64>,
    dims: [u32; 2],
    fast_fall: bool,
    path: Vec<Movement>,
    value: T,
}

impl<T> WalkAhead<T> {
    /// Starts the walk of `path` finished at `pos` ([`finish`]) for an entity of the size
    /// `dims` against `blocks`: returns the way to walk and the value to fill, or `None` when
    /// `blocks` cannot tell when they change (nothing is kept then).
    pub fn start(
        &mut self,
        blocks: &dyn BlockAccess,
        dims: Dimensions,
        fast_fall: bool,
        path: &[Movement],
        pos: Vec3,
    ) -> Option<(&[Movement], &mut T)> {
        self.stamp = blocks.stamp();
        self.stamp?;
        self.dims = [dims.width.to_bits(), dims.height.to_bits()];
        self.fast_fall = fast_fall;
        self.path.clear();
        self.path.extend_from_slice(path);
        finish(&mut self.path, pos, pos);
        Some((&self.path, &mut self.value))
    }

    /// Keeps nothing: [`WalkAhead::get`] gives nothing until the next start.
    pub fn clear(&mut self) {
        self.stamp = None;
    }

    /// The value of the walk worked out ahead, if it was for exactly this walk and the
    /// blocks did not change since.
    pub fn get(
        &self,
        blocks: &dyn BlockAccess,
        dims: Dimensions,
        fast_fall: bool,
        path: &[Movement],
    ) -> Option<&T> {
        let bits = |v: Vec3| [v.x.to_bits(), v.y.to_bits(), v.z.to_bits()];
        let same = |a: &Movement, b: &Movement| {
            bits(a.from) == bits(b.from)
                && bits(a.to) == bits(b.to)
                && a.original.map(bits) == b.original.map(bits)
        };
        (self.stamp.is_some()
            && self.dims == [dims.width.to_bits(), dims.height.to_bits()]
            && self.fast_fall == fast_fall
            && self.path.len() == path.len()
            && self.path.iter().zip(path).all(|(a, b)| same(a, b))
            && self.stamp == blocks.stamp())
        .then_some(&self.value)
    }
}

/// A block the entity is inside on its way.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub pos: BlockPos,
    pub state: BlockState,
    /// Inside the block (`getEntityInsideCollisionShape`): `entityInside` of the block runs.
    pub in_block: bool,
    /// Inside the fluid of the block: `entityInside` of the fluid runs.
    pub in_fluid: bool,
    /// The step of the walk (`advanceStep`): effects collected in a step apply together.
    pub step: i32,
    /// `isPrecise`: the box at the end of the walk touches the block, or the walk was long.
    pub precise: bool,
}

/// The entity whose way is walked.
#[derive(Clone, Copy)]
pub struct Walker<'a> {
    pub blocks: &'a dyn BlockAccess,
    /// For the fluids; without them only the blocks count.
    pub facts: Option<&'a BlockFacts>,
    pub dims: Dimensions,
    /// `fallDistance > 2.5`: powder snow only takes the entity below its top tenth.
    pub fast_fall: bool,
}

impl std::fmt::Debug for Walker<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Walker")
            .field("dims", &self.dims)
            .finish_non_exhaustive()
    }
}

/// `deflate(1.0E-5F)`.
const DEFLATE: f64 = 1.0e-5_f32 as f64;

impl Walker<'_> {
    /// `checkInsideBlocks(movements, collector)`: `visit` for every block the entity is inside
    /// on its way, once each; it returns whether the entity is still alive.
    pub fn walk(&self, movements: &[Movement], mut visit: impl FnMut(&Cell) -> bool) {
        let Some(bounds) = self.bounds(movements) else {
            return;
        };
        let mut run = Run {
            walker: self,
            seen: Seen::new(bounds),
            local: Seen::new(bounds),
            alive: true,
            visit: &mut visit,
        };
        for m in movements {
            let delta = m.to - m.from;
            let mut left = 16;
            match m.original {
                Some(original) if delta.length_sqr() > 0.0 => {
                    let mut pos = m.from;
                    for axis in axis_step_order(original) {
                        let moved = [delta.x, delta.y, delta.z][axis];
                        if moved != 0.0 {
                            let mut to = pos;
                            match axis {
                                0 => to.x += moved,
                                1 => to.y += moved,
                                _ => to.z += moved,
                            }
                            left -= run.segment(pos, to, left);
                            pos = to;
                        }
                    }
                }
                _ => left -= run.segment(m.from, m.to, 16),
            }
            if left <= 0 {
                run.segment(m.to, m.to, 1);
            }
            if !run.alive {
                return;
            }
        }
    }

    /// The block box all the movements stay in, one block wider on every side.
    fn bounds(&self, movements: &[Movement]) -> Option<([i32; 3], [i32; 3])> {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for m in movements {
            for p in [m.from, m.to] {
                let b = self.dims.bounding_box(p);
                min = [
                    min[0].min(b.min_x),
                    min[1].min(b.min_y),
                    min[2].min(b.min_z),
                ];
                max = [
                    max[0].max(b.max_x),
                    max[1].max(b.max_y),
                    max[2].max(b.max_z),
                ];
            }
        }
        if !min.iter().chain(&max).all(|v| v.is_finite()) {
            return None;
        }
        Some((
            min.map(|v| floor(v).saturating_sub(1)),
            max.map(|v| floor(v).saturating_add(1)),
        ))
    }

    /// `collidedWithShapeMovingFrom(from, to, [part])`.
    fn collided(&self, from: Vec3, to: Vec3, part: Aabb) -> bool {
        let b = self.dims.bounding_box(from);
        let center = b.center();
        let end = center + (to - from);
        let inflated = part.inflate(
            (b.max_x - b.min_x) * 0.5 - 1.0e-7,
            (b.max_y - b.min_y) * 0.5 - 1.0e-7,
            (b.max_z - b.min_z) * 0.5 - 1.0e-7,
        );
        inflated.contains(end.x, end.y, end.z)
            || inflated.contains(center.x, center.y, center.z)
            || crate::mob::projectile::aabb_clip(&inflated, center, end).is_some()
    }

    /// Inside `getEntityInsideCollisionShape` of the block: the whole block for all but
    /// tripwire and End portals (their outline), filled cauldrons (the hull and the content)
    /// and powder snow under a fast fall.
    fn in_block(&self, state: BlockState, pos: BlockPos, from: Vec3, to: Vec3) -> bool {
        let origin = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
        let part = |b: &[f64; 6]| {
            Aabb::new(
                origin.0 + b[0],
                origin.1 + b[1],
                origin.2 + b[2],
                origin.0 + b[3],
                origin.1 + b[4],
                origin.2 + b[5],
            )
        };
        let content = |top: f64| [0.125, 0.25, 0.125, 0.875, top / 16.0, 0.875];
        let shapes = |extra: Option<[f64; 6]>| {
            state
                .outline_shape()
                .iter()
                .chain(extra.iter())
                .any(|b| self.collided(from, to, part(b)))
        };
        match state.block().info().kind {
            BlockKind::TripWireBlock | BlockKind::EndPortalBlock => shapes(None),
            BlockKind::LavaCauldronBlock => shapes(Some(content(15.0))),
            BlockKind::LayeredCauldronBlock => {
                let level = state.int("level").unwrap_or(1);
                shapes(Some(content(f64::from(6 + level * 3))))
            }
            BlockKind::PowderSnowBlock if self.fast_fall => self.collided(
                from,
                to,
                part(&[0.0, 0.0, 0.0, 1.0, f64::from(0.9_f32), 1.0]),
            ),
            _ => true,
        }
    }

    /// `collidedWithFluid(state.getFluidState(), pos, from, to)`.
    fn in_fluid(&self, state: BlockState, pos: BlockPos, from: Vec3, to: Vec3) -> bool {
        let Some(facts) = self.facts else {
            return false;
        };
        let fluid = state.fluid_state();
        if fluid.is_empty() {
            return false;
        }
        let (x, y, z) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
        let height = f64::from(fluid_height(self.blocks, facts, pos, fluid));
        let part = Aabb::new(x, y, z, x + 1.0, y + height, z + 1.0);
        self.collided(from, to, part)
    }
}

/// One `checkInsideBlocks` of an entity.
struct Run<'a, 'w, F> {
    walker: &'a Walker<'w>,
    /// `visitedBlocks`: the blocks that acted already.
    seen: Seen,
    /// The blocks one walk visited.
    local: Seen,
    alive: bool,
    visit: &'a mut F,
}

impl<F: FnMut(&Cell) -> bool> Run<'_, '_, F> {
    /// `checkInsideBlocks(from, to, collector, visited, maxMovementIterations)`: the number of
    /// steps the walk took.
    fn segment(&mut self, from: Vec3, to: Vec3, max_iterations: i32) -> i32 {
        let w = self.walker;
        let target = w
            .dims
            .bounding_box(to)
            .inflate(-DEFLATE, -DEFLATE, -DEFLATE);
        let far = 0.999_990_000_000_252_6_f64;
        let moved_far = from.distance_to_sqr(to) > far * far;
        let mut last = 0;
        let (seen, alive, visit) = (&mut self.seen, &mut self.alive, &mut *self.visit);
        for_each_between(from, to, target, &mut self.local, &mut |pos, iteration| {
            if !*alive || iteration >= max_iterations {
                return false;
            }
            last = iteration;
            let Some(state) = w.blocks.block_state(pos) else {
                return true;
            };
            if state.is_air() {
                return true;
            }
            let in_block = w.in_block(state, pos, from, to);
            let in_fluid = w.in_fluid(state, pos, from, to);
            if (in_block || in_fluid) && seen.insert(pos) {
                let cell = Cell {
                    pos,
                    state,
                    in_block,
                    in_fluid,
                    step: iteration,
                    precise: moved_far || target.intersects(&Aabb::of_block(pos)),
                };
                *alive = visit(&cell);
            }
            true
        });
        last + 1
    }
}

/// `Direction.axisStepOrder`: Y, then Z before X when the movement is longer on Z.
fn axis_step_order(movement: Vec3) -> [usize; 3] {
    if movement.x.abs() < movement.z.abs() {
        [1, 2, 0]
    } else {
        [1, 0, 2]
    }
}

/// `BlockGetter.forEachBlockIntersectedBetween(from, to, aabbAtTarget, visitor)`.
fn for_each_between(
    from: Vec3,
    to: Vec3,
    target: Aabb,
    local: &mut Seen,
    visit: &mut impl FnMut(BlockPos, i32) -> bool,
) -> bool {
    let travel = to - from;
    if travel.length_sqr() < f64::from(1.0e-5_f32 * 1.0e-5_f32) {
        // `BlockPos.betweenClosed`: X runs first, then Y, then Z.
        for z in floor(target.min_z)..=floor(target.max_z) {
            for y in floor(target.min_y)..=floor(target.max_y) {
                for x in floor(target.min_x)..=floor(target.max_x) {
                    if !visit(BlockPos::new(x, y, z), 0) {
                        return false;
                    }
                }
            }
        }
        return true;
    }
    local.clear();
    let start = target.offset(-travel.x, -travel.y, -travel.z);
    if !corners_in_direction(corners(&start), travel, &mut |pos| {
        if !visit(pos, 0) {
            return false;
        }
        local.insert(pos);
        true
    }) {
        return false;
    }
    let Some(iterations) = collisions_along_travel(local, travel, &target, visit) else {
        return false;
    };
    corners_in_direction(corners(&target), travel, &mut |pos| {
        !local.insert(pos) || visit(pos, iterations + 1)
    })
}

/// The blocks of the corners of a box.
fn corners(b: &Aabb) -> ([i32; 3], [i32; 3]) {
    (
        [floor(b.min_x), floor(b.min_y), floor(b.min_z)],
        [floor(b.max_x), floor(b.max_y), floor(b.max_z)],
    )
}

/// `BlockPos.betweenCornersInDirection`: the blocks between two corners, starting at the
/// corner the direction comes from, the last axis of [`axis_step_order`] running first.
fn corners_in_direction(
    (a, b): ([i32; 3], [i32; 3]),
    direction: Vec3,
    visit: &mut impl FnMut(BlockPos) -> bool,
) -> bool {
    let dir = [direction.x, direction.y, direction.z];
    let mut start = [0; 3];
    let mut diff = [0; 3];
    let mut sign = [0; 3];
    for i in 0..3 {
        let (min, max) = (a[i].min(b[i]), a[i].max(b[i]));
        diff[i] = max.saturating_sub(min);
        (start[i], sign[i]) = if dir[i] >= 0.0 { (min, 1) } else { (max, -1) };
    }
    let [first, second, third] = axis_step_order(direction);
    for i in 0..=diff[first] {
        for j in 0..=diff[second] {
            for k in 0..=diff[third] {
                let mut p = start;
                p[first] += sign[first] * i;
                p[second] += sign[second] * j;
                p[third] += sign[third] * k;
                if !visit(BlockPos::new(p[0], p[1], p[2])) {
                    return false;
                }
            }
        }
    }
    true
}

/// `BlockGetter.addCollisionsAlongTravel`: the blocks the leading corner of the box enters on
/// its way, each with the blocks the box covers there. The number of steps, `None` once the
/// visitor stops.
fn collisions_along_travel(
    local: &mut Seen,
    delta: Vec3,
    target: &Aabb,
    visit: &mut impl FnMut(BlockPos, i32) -> bool,
) -> Option<i32> {
    let size = [
        target.max_x - target.min_x,
        target.max_y - target.min_y,
        target.max_z - target.min_z,
    ];
    let corner = furthest_corner(delta);
    let center = target.center();
    let to_corner = Vec3::new(
        center.x + size[0] * 0.5 * f64::from(corner[0]),
        center.y + size[1] * 0.5 * f64::from(corner[1]),
        center.z + size[2] * 0.5 * f64::from(corner[2]),
    );
    let from_corner = to_corner - delta;
    let from = [from_corner.x, from_corner.y, from_corner.z];
    let d = [delta.x, delta.y, delta.z];
    let mut block = from.map(floor);
    let mut sign = [0_i32; 3];
    let mut t_delta = [f64::MAX; 3];
    let mut t = [0.0_f64; 3];
    for i in 0..3 {
        sign[i] = if d[i] == 0.0 {
            0
        } else if d[i] > 0.0 {
            1
        } else {
            -1
        };
        if sign[i] != 0 {
            t_delta[i] = f64::from(sign[i]) / d[i];
        }
        let frac = from[i] - from[i].floor();
        t[i] = t_delta[i] * if sign[i] > 0 { 1.0 - frac } else { frac };
    }
    let mut iterations = 0;
    while t[0] <= 1.0 || t[1] <= 1.0 || t[2] <= 1.0 {
        let axis = if t[0] < t[1] {
            if t[0] < t[2] { 0 } else { 2 }
        } else if t[1] < t[2] {
            1
        } else {
            2
        };
        block[axis] = block[axis].saturating_add(sign[axis]);
        t[axis] += t_delta[axis];
        let cube = Aabb::new(
            f64::from(block[0]),
            f64::from(block[1]),
            f64::from(block[2]),
            f64::from(block[0].saturating_add(1)),
            f64::from(block[1].saturating_add(1)),
            f64::from(block[2].saturating_add(1)),
        );
        let Some(hit) = crate::mob::projectile::aabb_clip(&cube, from_corner, to_corner) else {
            continue;
        };
        iterations += 1;
        let hit = [hit.x, hit.y, hit.z];
        let mut opposite = [0; 3];
        for i in 0..3 {
            // `Mth.clamp(hit, block + 1.0E-5F, block + 1.0 - 1.0E-5F)`: the lower bound is a
            // float sum.
            #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
            let low = f64::from(block[i] as f32 + 1.0e-5_f32);
            let high = f64::from(block[i]) + 1.0 - DEFLATE;
            let clamped = if hit[i] < low { low } else { hit[i].min(high) };
            opposite[i] = floor(clamped - size[i] * f64::from(corner[i]));
        }
        let step = iterations;
        if !corners_in_direction((block, opposite), delta, &mut |pos| {
            !local.insert(pos) || visit(pos, step)
        }) {
            return None;
        }
    }
    Some(iterations)
}

/// `BlockGetter.getFurthestCorner`.
fn furthest_corner(direction: Vec3) -> [i32; 3] {
    let (x_dot, y_dot, z_dot) = (direction.x.abs(), direction.y.abs(), direction.z.abs());
    let sign = |v: f64| if v >= 0.0 { 1 } else { -1 };
    let (x, y, z) = (sign(direction.x), sign(direction.y), sign(direction.z));
    if x_dot <= y_dot && x_dot <= z_dot {
        [-x, -z, y]
    } else if y_dot <= z_dot {
        [z, -y, -x]
    } else {
        [-y, x, -z]
    }
}

/// A set of block positions inside a box: a bit per block, without allocating for the boxes
/// of all but huge entities; a list where the box is too large for that.
struct Seen {
    min: [i32; 3],
    size: [usize; 3],
    words: usize,
    inline: [u64; Seen::INLINE],
    heap: Vec<u64>,
    /// The positions when the box is too large for bits (a way of many blocks).
    list: Option<Vec<BlockPos>>,
}

impl Seen {
    const INLINE: usize = 8;
    /// Bits beyond which the set lists its positions.
    const MAX_BITS: usize = 1 << 16;

    fn new((min, max): ([i32; 3], [i32; 3])) -> Seen {
        let mut size = [0_usize; 3];
        for i in 0..3 {
            let span = i64::from(max[i]) - i64::from(min[i]) + 1;
            size[i] = usize::try_from(span).unwrap_or(0);
        }
        let bits = size[0]
            .checked_mul(size[1])
            .and_then(|b| b.checked_mul(size[2]))
            .filter(|b| *b <= Seen::MAX_BITS);
        let words = bits.map_or(0, |b| b.div_ceil(64));
        Seen {
            min,
            size,
            words,
            inline: [0; Seen::INLINE],
            heap: if words > Seen::INLINE {
                vec![0; words]
            } else {
                Vec::new()
            },
            list: bits.is_none().then(Vec::new),
        }
    }

    fn clear(&mut self) {
        if let Some(list) = &mut self.list {
            list.clear();
        } else if self.words > Seen::INLINE {
            self.heap.fill(0);
        } else {
            self.inline = [0; Seen::INLINE];
        }
    }

    /// Adds the position; whether it is new.
    fn insert(&mut self, pos: BlockPos) -> bool {
        if let Some(list) = &mut self.list {
            if list.contains(&pos) {
                return false;
            }
            list.push(pos);
            return true;
        }
        let mut index = 0_usize;
        for (i, v) in [pos.x, pos.y, pos.z].into_iter().enumerate() {
            let off = i64::from(v) - i64::from(self.min[i]);
            let Some(off) = usize::try_from(off).ok().filter(|o| *o < self.size[i]) else {
                // Outside the box of the movements: nothing to remember it by.
                return true;
            };
            index = index * self.size[i] + off;
        }
        let words = if self.words > Seen::INLINE {
            &mut self.heap[..]
        } else {
            &mut self.inline[..]
        };
        let Some(word) = words.get_mut(index / 64) else {
            return true;
        };
        let bit = 1_u64 << (index % 64);
        let new = *word & bit == 0;
        *word |= bit;
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Blocks(Vec<(BlockPos, BlockState)>);

    impl BlockAccess for Blocks {
        fn block_state(&self, pos: BlockPos) -> Option<BlockState> {
            let air = vanillite_data::block::ids::AIR.default_state();
            Some(self.0.iter().find(|(p, _)| *p == pos).map_or(air, |b| b.1))
        }
    }

    fn visited(blocks: &Blocks, movements: &[Movement]) -> Vec<(BlockPos, i32)> {
        let walker = Walker {
            blocks,
            facts: None,
            dims: Dimensions {
                width: 0.6,
                height: 1.8,
            },
            fast_fall: false,
        };
        let mut out = Vec::new();
        walker.walk(movements, |cell| {
            out.push((cell.pos, cell.step));
            true
        });
        out
    }

    /// A bush the box only passes (it is neither in the box at the start nor at the end)
    /// acts, once, and not when the entity stands still beside it (#180).
    #[test]
    fn a_block_on_the_way_acts_once() {
        let bush = vanillite_data::block::ids::SWEET_BERRY_BUSH.default_state();
        let pos = BlockPos::new(2, -60, 8);
        let blocks = Blocks(vec![(pos, bush)]);
        let (from, to) = (Vec3::new(3.5, -57.0, 8.5), Vec3::new(1.19, -59.69, 8.56));
        let passed = visited(&blocks, &[Movement::plain(from, to)]);
        assert_eq!(passed.len(), 1);
        assert_eq!(passed[0].0, pos);
        assert!(passed[0].1 > 0, "entered on the way");
        assert!(visited(&blocks, &[Movement::still(to)]).is_empty());
        // The same way in two movements: the bush still acts once.
        let twice = [Movement::plain(from, to), Movement::plain(to, from)];
        assert_eq!(visited(&blocks, &twice).len(), 1);
    }

    /// `Entity.move` records the wanted movement: the way is walked down first, then along
    /// the longer horizontal axis, so a block beside the end of a diagonal fall acts.
    #[test]
    fn a_recorded_movement_is_walked_axis_by_axis() {
        let web = vanillite_data::block::ids::COBWEB.default_state();
        let pos = BlockPos::new(11, -60, 6);
        let blocks = Blocks(vec![(pos, web)]);
        let (from, to) = (Vec3::new(12.25, -58.2, 6.5), Vec3::new(12.4, -59.2, 6.5));
        let delta = to - from;
        let axes = Movement {
            from,
            to,
            original: Some(delta),
        };
        assert_eq!(visited(&blocks, &[axes]).len(), 1);
        assert!(visited(&blocks, &[Movement::still(to)]).is_empty());
    }

    /// [`Blocks`] that tell when they change.
    struct Stamped(Blocks, std::cell::Cell<u64>);

    impl BlockAccess for Stamped {
        fn block_state(&self, pos: BlockPos) -> Option<BlockState> {
            self.0.block_state(pos)
        }
        fn stamp(&self) -> Option<u64> {
            Some(self.1.get())
        }
    }

    /// A walk worked out ahead gives what the walk gives, and stands only for the same way,
    /// size and fall while the blocks keep their stamp.
    #[test]
    fn a_walk_worked_out_ahead_stands_only_for_the_same_walk() {
        use super::super::blocks::{BlockFacts, InsideAhead, inside_blocks, inside_blocks_ahead};
        let bush = vanillite_data::block::ids::SWEET_BERRY_BUSH.default_state();
        let blocks = Stamped(
            Blocks(vec![(BlockPos::new(2, -60, 8), bush)]),
            std::cell::Cell::new(0),
        );
        let dims = Dimensions {
            width: 0.6,
            height: 1.8,
        };
        let facts = BlockFacts::default();
        let (from, to) = (Vec3::new(3.5, -57.0, 8.5), Vec3::new(1.19, -59.69, 8.56));
        let path = [Movement::plain(from, to)];
        let mut ahead = InsideAhead::default();
        inside_blocks_ahead(&blocks, &facts, dims, false, &path, to, &mut ahead);
        let mut way = path.to_vec();
        finish(&mut way, to, to);
        let mut cells = Vec::new();
        let inside = inside_blocks(&blocks, &facts, dims, false, &way, |c| {
            cells.push((c.pos, c.step));
        });
        assert!(inside.berry_bush.is_some());
        let (ahead_inside, ahead_cells) = ahead.get(&blocks, dims, false, &way).unwrap();
        assert_eq!(*ahead_inside, inside);
        let ahead_cells: Vec<_> = ahead_cells.iter().map(|c| (c.pos, c.step)).collect();
        assert_eq!(ahead_cells, cells);
        // Another fall, size or way: walked anew.
        assert!(ahead.get(&blocks, dims, true, &way).is_none());
        let small = Dimensions {
            width: 0.6,
            height: 1.5,
        };
        assert!(ahead.get(&blocks, small, false, &way).is_none());
        let mut longer = way.clone();
        finish(&mut longer, to, from);
        assert!(ahead.get(&blocks, dims, false, &longer).is_none());
        // Changed blocks: walked anew.
        blocks.1.set(1);
        assert!(ahead.get(&blocks, dims, false, &way).is_none());
    }
}
