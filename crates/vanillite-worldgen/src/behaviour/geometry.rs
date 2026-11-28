//! Face geometry of block shapes: `VoxelShape.getFaceShape` and the `Shapes.joinIsNotEmpty`
//! tests built on it (`Block.isFaceFull`, `WallBlock.isCovered`, …).
//!
//! A face shape is the slice of a shape at the face (`findIndex(axis, 0.9999999)` or `1.0E-7`),
//! extruded over the whole block along the face's axis, so every test reduces to rectangles in
//! the plane of the face.

use vanillite_base::Direction;
use vanillite_data::block::Shape;

use super::SupportType;
use super::props::{Axis, axis};

/// A rectangle `[min_u, min_v, max_u, max_v]` in the plane of a face (for vertical faces `u` is
/// X and `v` is Z).
pub(crate) type Rect = [f64; 4];

/// The full face.
pub(crate) const FULL: Rect = [0.0, 0.0, 1.0, 1.0];

/// Iterates the rectangles of `shape`'s face slice in `direction`.
fn face_rects(shape: &[[f64; 6]], direction: Direction) -> impl Iterator<Item = Rect> {
    // Index of the box coordinates along the face's axis and of the two in-plane axes.
    let (along, u, v) = match axis(direction) {
        Axis::X => (0, 1, 2),
        Axis::Y => (1, 0, 2),
        Axis::Z => (2, 0, 1),
    };
    let positive = matches!(
        direction,
        Direction::Up | Direction::South | Direction::East
    );
    let at = if positive { 0.9999999 } else { 1.0E-7 };
    shape.iter().filter_map(move |b| {
        (b[along] <= at && b[along + 3] > at).then_some([b[u], b[v], b[u + 3], b[v + 3]])
    })
}

/// Whether the union of `rects` covers all of `target`.
fn covers(rects: &[Rect], target: Rect) -> bool {
    // Split the target at every rectangle edge inside it and test each cell's centre.
    let mut us = vec![target[0], target[2]];
    let mut vs = vec![target[1], target[3]];
    for r in rects {
        for u in [r[0], r[2]] {
            if u > target[0] && u < target[2] {
                us.push(u);
            }
        }
        for v in [r[1], r[3]] {
            if v > target[1] && v < target[3] {
                vs.push(v);
            }
        }
    }
    us.sort_by(f64::total_cmp);
    vs.sort_by(f64::total_cmp);
    for cu in us.windows(2).filter(|w| w[1] > w[0]) {
        let mu = (cu[0] + cu[1]) * 0.5;
        for cv in vs.windows(2).filter(|w| w[1] > w[0]) {
            let mv = (cv[0] + cv[1]) * 0.5;
            if !rects
                .iter()
                .any(|r| r[0] <= mu && mu <= r[2] && r[1] <= mv && mv <= r[3])
            {
                return false;
            }
        }
    }
    true
}

/// `!Shapes.joinIsNotEmpty(target, shape.getFaceShape(direction), ONLY_FIRST)` for a target
/// prism given by its rectangle: whether the face of `shape` covers `target`.
pub(crate) fn face_covers(shape: Shape, direction: Direction, target: Rect) -> bool {
    // Fast paths without allocating: no face at all, or one rectangle covering the target.
    let mut count = 0;
    for r in face_rects(shape, direction) {
        if r[0] <= target[0] && r[1] <= target[1] && r[2] >= target[2] && r[3] >= target[3] {
            return true;
        }
        count += 1;
    }
    if count < 2 {
        return false;
    }
    let rects: Vec<Rect> = face_rects(shape, direction).collect();
    covers(&rects, target)
}

/// `SupportType.isSupporting` for a shape that is not a block state's own (a moving piston's,
/// from its block entity; it may reach out of the block): full support needs the face to be
/// exactly the block's face (`Block.isFaceFull`), centre and rigid support need it to cover
/// `Block.column(2, 0, 10)` or `Shapes.block()` without `Block.column(12, 0, 16)`.
pub(crate) fn boxes_support(
    boxes: &[[f64; 6]],
    direction: Direction,
    support: SupportType,
) -> bool {
    let rects: Vec<Rect> = face_rects(boxes, direction).collect();
    let (lo, hi, top) = (7.0 / 16.0, 9.0 / 16.0, 10.0 / 16.0);
    let (rim_lo, rim_hi) = (2.0 / 16.0, 14.0 / 16.0);
    match (support, axis(direction)) {
        (SupportType::Full, _) => {
            rects
                .iter()
                .all(|r| r[0] >= 0.0 && r[1] >= 0.0 && r[2] <= 1.0 && r[3] <= 1.0)
                && covers(&rects, FULL)
        }
        // The column stands along Y: on the X faces `u` is Y and `v` Z, on the Z faces `u` is
        // X and `v` Y.
        (SupportType::Center, Axis::Y) => covers(&rects, [lo, lo, hi, hi]),
        (SupportType::Center, Axis::X) => covers(&rects, [0.0, lo, top, hi]),
        (SupportType::Center, Axis::Z) => covers(&rects, [lo, 0.0, hi, top]),
        (SupportType::Rigid, Axis::Y) => [
            [0.0, 0.0, 1.0, rim_lo],
            [0.0, rim_hi, 1.0, 1.0],
            [0.0, 0.0, rim_lo, 1.0],
            [rim_hi, 0.0, 1.0, 1.0],
        ]
        .into_iter()
        .all(|target| covers(&rects, target)),
        // The rim reaches every point of a side face.
        (SupportType::Rigid, _) => covers(&rects, FULL),
    }
}

/// `Block.isFaceFull(shape, direction)`.
pub(crate) fn is_face_full(shape: Shape, direction: Direction) -> bool {
    face_covers(shape, direction, FULL)
}

/// `Shapes.mergedFaceOccludes(shape, occluder, direction)`: whether the face of `shape` towards
/// `direction` and the opposite face of `occluder` together cover the whole face.
pub fn merged_face_occludes(shape: Shape, occluder: Shape, direction: Direction) -> bool {
    if is_full_block(shape) || is_full_block(occluder) {
        return true;
    }
    let positive = matches!(
        direction,
        Direction::Up | Direction::South | Direction::East
    );
    let (first, second) = if positive {
        (shape, occluder)
    } else {
        (occluder, shape)
    };
    let (up, down) = match axis(direction) {
        Axis::X => (Direction::East, Direction::West),
        Axis::Y => (Direction::Up, Direction::Down),
        Axis::Z => (Direction::South, Direction::North),
    };
    let rects: Vec<Rect> = face_rects(first, up)
        .chain(face_rects(second, down))
        .collect();
    covers(&rects, FULL)
}

/// Whether a shape is the full block (`shape == Shapes.block()`).
pub fn is_full_block(shape: Shape) -> bool {
    shape == [[0.0, 0.0, 0.0, 1.0, 1.0, 1.0]]
}

/// `shape.getFaceShape(direction).isEmpty()`.
pub(crate) fn is_face_empty(shape: Shape, direction: Direction) -> bool {
    face_rects(shape, direction).next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: Shape = &[[0.0, 0.0, 0.0, 1.0, 1.0, 1.0]];
    const BOTTOM_SLAB: Shape = &[[0.0, 0.0, 0.0, 1.0, 0.5, 1.0]];
    const HALVES: Shape = &[
        [0.0, 0.0, 0.0, 0.5, 1.0, 1.0],
        [0.5, 0.5, 0.0, 1.0, 1.0, 1.0],
    ];

    #[test]
    fn full_faces() {
        assert!(is_face_full(BLOCK, Direction::Up));
        assert!(is_face_full(BOTTOM_SLAB, Direction::Down));
        assert!(!is_face_full(BOTTOM_SLAB, Direction::Up));
        assert!(is_face_empty(BOTTOM_SLAB, Direction::Up));
        assert!(is_face_full(HALVES, Direction::Up));
        assert!(!is_face_full(HALVES, Direction::Down));
        assert!(face_covers(
            HALVES,
            Direction::Down,
            [0.4375, 0.4375, 0.5, 0.5625]
        ));
        assert!(!is_face_full(&[], Direction::Up));
    }
}
