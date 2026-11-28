//! `PistonMovingBlockEntity`: a block on its way, its progress and how it lands, plus the
//! entities it pushes.

use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockState, ids};
use vanillite_data::entity::ids as entity_ids;
use vanillite_world::entity::{EntityFlags, EntityId};
use vanillite_worldgen::behaviour::{self as shared, StateExt};

use super::{is_piston_base, resolver::axis};
use crate::level::{BlockEntity, Level, update};

/// The data of a `PistonMovingBlockEntity`.
#[derive(Clone, Debug, PartialEq)]
pub struct MovingPiston {
    /// `movedState`: the block that moves (the piston head of an extending piston, the piston
    /// base of a retracting one).
    pub moved_state: BlockState,
    /// `direction`: the piston's facing.
    pub direction: Direction,
    /// `extending`.
    pub extending: bool,
    /// `isSourcePiston`: the piston itself (its head or base), not a pushed block.
    pub source: bool,
    /// `progress`.
    pub progress: f32,
    /// `progressO`.
    pub progress_o: f32,
    /// `lastTicked`: the game time of the last tick.
    pub last_ticked: i64,
}

impl MovingPiston {
    /// `MovingPistonBlock.newMovingBlockEntity(...)`.
    pub fn new(
        moved_state: BlockState,
        direction: Direction,
        extending: bool,
        source: bool,
    ) -> MovingPiston {
        MovingPiston {
            moved_state,
            direction,
            extending,
            source,
            progress: 0.0,
            progress_o: 0.0,
            last_ticked: 0,
        }
    }

    /// `getProgress(a)`.
    pub fn progress(&self, a: f32) -> f32 {
        let a = a.min(1.0);
        self.progress_o + a * (self.progress - self.progress_o)
    }

    /// `getExtendedProgress(progress)`.
    fn extended_progress(&self, progress: f32) -> f32 {
        if self.extending {
            progress - 1.0
        } else {
            1.0 - progress
        }
    }

    /// `getMovementDirection()` / `getPushDirection()`.
    pub fn movement_direction(&self) -> Direction {
        if self.extending {
            self.direction
        } else {
            self.direction.opposite()
        }
    }

    /// `getCollisionRelatedBlockState()`: a retracting piston base collides like its head.
    fn collision_related_state(&self) -> BlockState {
        if !self.extending && self.source && is_piston_base(self.moved_state) {
            let sticky = self.moved_state.is(ids::STICKY_PISTON);
            let facing = self
                .moved_state
                .direction("facing")
                .unwrap_or(Direction::North);
            ids::PISTON_HEAD
                .default_state()
                .set_flag("short", self.progress > 0.25)
                .set("type", if sticky { "sticky" } else { "normal" })
                .set_direction("facing", facing)
        } else {
            self.moved_state
        }
    }
}

thread_local! {
    /// `PistonMovingBlockEntity.NOCLIP`: while a piston moves an entity, moving blocks of that
    /// direction do not collide with it.
    static NOCLIP: std::cell::Cell<Option<Direction>> = const { std::cell::Cell::new(None) };
}

/// `PistonMovingBlockEntity.getCollisionShape(level, pos)`: the boxes of the moving block
/// relative to its position (the head of a moving source piston, else the moved block, at its
/// progress; a retracting source piston also its extended base).
pub fn collision_boxes(m: &MovingPiston, out: &mut Vec<[f64; 6]>) {
    let start = out.len();
    let base = !m.extending && m.source && is_piston_base(m.moved_state);
    if base {
        out.extend_from_slice(m.moved_state.set_flag("extended", true).collision_shape());
    }
    if m.progress < 1.0 && NOCLIP.get() == Some(m.movement_direction()) {
        return;
    }
    let state = if m.source {
        ids::PISTON_HEAD
            .default_state()
            .set_direction("facing", m.direction)
            .set_flag("short", m.extending != (1.0 - m.progress < 0.25))
    } else {
        m.moved_state
    };
    let progress = f64::from(m.extended_progress(m.progress));
    let [sx, sy, sz] = m.direction.normal();
    let (xo, yo, zo) = (
        f64::from(sx) * progress,
        f64::from(sy) * progress,
        f64::from(sz) * progress,
    );
    for b in state.collision_shape() {
        out.push([
            b[0] + xo,
            b[1] + yo,
            b[2] + zo,
            b[3] + xo,
            b[4] + yo,
            b[5] + zo,
        ]);
    }
    // `Shapes.or(base, head).optimize()`: a union that fills the block is `Shapes.block()`,
    // whose only cell lines are the block's faces (entities inside it are not stopped
    // halfway).
    if base && fills_block(&out[start..]) {
        out.truncate(start);
        out.push([0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
    }
}

/// Whether `boxes` lie in the unit cube and cover all of it.
fn fills_block(boxes: &[[f64; 6]]) -> bool {
    if boxes
        .iter()
        .any(|b| (0..3).any(|a| b[a] < -1.0e-7 || b[a + 3] > 1.0 + 1.0e-7))
    {
        return false;
    }
    let lines = |a: usize| {
        let mut v: Vec<f64> = boxes.iter().flat_map(|b| [b[a], b[a + 3]]).collect();
        v.extend([0.0, 1.0]);
        v.sort_by(f64::total_cmp);
        v.dedup_by(|x, y| (*x - *y).abs() < 1.0e-7);
        v
    };
    let (xs, ys, zs) = (lines(0), lines(1), lines(2));
    let mids = |v: &[f64]| -> Vec<f64> { v.windows(2).map(|w| (w[0] + w[1]) / 2.0).collect() };
    let (mx, my, mz) = (mids(&xs), mids(&ys), mids(&zs));
    mx.iter().all(|&x| {
        my.iter().all(|&y| {
            mz.iter().all(|&z| {
                boxes
                    .iter()
                    .any(|b| b[0] < x && x < b[3] && b[1] < y && y < b[4] && b[2] < z && z < b[5])
            })
        })
    })
}

/// The collision boxes of the moving block at `pos`, if the block entity is there.
pub fn collision_boxes_at(
    block_entities: &crate::level::block_entity::BlockEntities,
    pos: BlockPos,
    out: &mut Vec<[f64; 6]>,
) {
    if let Some(BlockEntity::MovingPiston(m)) = block_entities.get(pos) {
        collision_boxes(m, out);
    }
}

fn moving_mut(level: &mut Level, pos: BlockPos) -> Option<&mut MovingPiston> {
    match level.block_entity_mut(pos) {
        Some(BlockEntity::MovingPiston(m)) => Some(m),
        _ => None,
    }
}

/// `PistonMovingBlockEntity.tick(level, pos, state, entity)`.
pub fn moving_piston_tick(level: &mut Level, pos: BlockPos, _state: BlockState) {
    let game_time = level.game_time();
    let Some(entity) = moving_mut(level, pos) else {
        return;
    };
    entity.last_ticked = game_time;
    entity.progress_o = entity.progress;
    if entity.progress_o >= 1.0 {
        let moved = entity.moved_state;
        level.remove_block_entity(pos);
        if level.block(pos).is(ids::MOVING_PISTON) {
            let new = shared::update_from_neighbour_shapes(level, moved, pos);
            if new.is_air() {
                level.set_block(
                    pos,
                    moved,
                    update::NONE | update::MOVE_BY_PISTON | update::SKIP_BLOCK_ENTITY_SIDEEFFECTS,
                );
                level.update_or_destroy(moved, new, pos, update::ALL, crate::level::UPDATE_LIMIT);
            } else {
                let new = if new.waterlogged() {
                    new.set_flag("waterlogged", false)
                } else {
                    new
                };
                level.set_block(pos, new, update::ALL | update::MOVE_BY_PISTON);
                level.neighbor_changed(pos, new.block());
            }
        }
    } else {
        let new_progress = entity.progress + 0.5;
        let snapshot = entity.clone();
        move_collided_entities(level, pos, new_progress, &snapshot);
        move_stuck_entities(level, pos, new_progress, &snapshot);
        if let Some(entity) = moving_mut(level, pos) {
            entity.progress = new_progress.min(1.0);
        }
    }
}

/// `PistonMovingBlockEntity.finalTick()`: the block lands at once.
pub fn final_tick(level: &mut Level, pos: BlockPos) {
    let Some(entity) = moving_mut(level, pos) else {
        return;
    };
    if entity.progress_o >= 1.0 {
        return;
    }
    entity.progress = 1.0;
    entity.progress_o = 1.0;
    let (moved, source) = (entity.moved_state, entity.source);
    level.remove_block_entity(pos);
    if level.block(pos).is(ids::MOVING_PISTON) {
        let new = if source {
            BlockState::AIR
        } else {
            shared::update_from_neighbour_shapes(level, moved, pos)
        };
        level.pistons.in_final_tick += 1;
        level.set_block_and_update(pos, new);
        level.pistons.in_final_tick -= 1;
        level.neighbor_changed(pos, new.block());
    }
}

/// The block shape boxes of `state` moved to `pos` and the entity's progress
/// (`moveByPositionAndProgress`).
fn moved_box(pos: BlockPos, b: &[f64; 6], entity: &MovingPiston) -> Aabb {
    let current = f64::from(entity.extended_progress(entity.progress));
    let [sx, sy, sz] = entity.direction.normal();
    let (dx, dy, dz) = (
        f64::from(pos.x) + current * f64::from(sx),
        f64::from(pos.y) + current * f64::from(sy),
        f64::from(pos.z) + current * f64::from(sz),
    );
    Aabb::new(
        b[0] + dx,
        b[1] + dy,
        b[2] + dz,
        b[3] + dx,
        b[4] + dy,
        b[5] + dz,
    )
}

/// `PistonMath.getMovementArea(aabb, direction, amount)`.
fn movement_area(aabb: Aabb, direction: Direction, amount: f64) -> Aabb {
    let step = match direction {
        Direction::Down | Direction::North | Direction::West => -1.0,
        _ => 1.0,
    };
    let delta = amount * step;
    let (min, max) = (delta.min(0.0), delta.max(0.0));
    let a = aabb;
    match direction {
        Direction::West => Aabb::new(
            a.min_x + min,
            a.min_y,
            a.min_z,
            a.min_x + max,
            a.max_y,
            a.max_z,
        ),
        Direction::East => Aabb::new(
            a.max_x + min,
            a.min_y,
            a.min_z,
            a.max_x + max,
            a.max_y,
            a.max_z,
        ),
        Direction::Down => Aabb::new(
            a.min_x,
            a.min_y + min,
            a.min_z,
            a.max_x,
            a.min_y + max,
            a.max_z,
        ),
        Direction::Up => Aabb::new(
            a.min_x,
            a.max_y + min,
            a.min_z,
            a.max_x,
            a.max_y + max,
            a.max_z,
        ),
        Direction::North => Aabb::new(
            a.min_x,
            a.min_y,
            a.min_z + min,
            a.max_x,
            a.max_y,
            a.min_z + max,
        ),
        Direction::South => Aabb::new(
            a.min_x,
            a.min_y,
            a.max_z + min,
            a.max_x,
            a.max_y,
            a.max_z + max,
        ),
    }
}

/// `AABB.minmax(other)`.
fn minmax(a: Aabb, b: Aabb) -> Aabb {
    Aabb::new(
        a.min_x.min(b.min_x),
        a.min_y.min(b.min_y),
        a.min_z.min(b.min_z),
        a.max_x.max(b.max_x),
        a.max_y.max(b.max_y),
        a.max_z.max(b.max_z),
    )
}

/// `getMovement(aabbToBeOutsideOf, movement, aabb)`.
fn movement(outside: &Aabb, direction: Direction, aabb: &Aabb) -> f64 {
    match direction {
        Direction::East => outside.max_x - aabb.min_x,
        Direction::West => aabb.max_x - outside.min_x,
        Direction::Up => outside.max_y - aabb.min_y,
        Direction::Down => aabb.max_y - outside.min_y,
        Direction::South => outside.max_z - aabb.min_z,
        Direction::North => aabb.max_z - outside.min_z,
    }
}

/// Whether the server moves `id` when a piston pushes it. Players move themselves: their client
/// simulates pistons and the server accepts the resulting movement. Entities that ignore
/// pistons (`PushReaction.IGNORE`: markers, area effect clouds, hanging entities) stay.
fn pushed_by_server(level: &Level, id: EntityId) -> bool {
    let Some(entity) = level.entities.store.get(id) else {
        return false;
    };
    let kind = entity.kind();
    // `PushReaction.IGNORE_ENTITY`: markers, displays, interactions, marker armor stands.
    let ignores = kind == entity_ids::MARKER
        || kind == entity_ids::INTERACTION
        || crate::decoration::display::is_display_kind(kind)
        || level
            .entities
            .mob(id)
            .is_some_and(crate::decoration::stand::is_marker);
    kind != entity_ids::PLAYER
        && !ignores
        && kind != entity_ids::AREA_EFFECT_CLOUD
        && kind != entity_ids::LEASH_KNOT
}

/// `moveEntityByPiston`: `entity.move(MoverType.PISTON, delta)` (at most 0.51 per axis and
/// game tick over all moving blocks, then the block collisions; on the ground only after
/// a collision below), then `applyEffectsFromBlocks` along the way. The entity moves along
/// `direction`; blocks moving along `piston` do not collide (`NOCLIP`), also when
/// `fixEntityWithinPistonBase` moves the entity the opposite way.
fn move_entity(
    level: &mut Level,
    id: EntityId,
    delta: f64,
    piston: Direction,
    direction: Direction,
) {
    // `BlockAttachedEntity.move`: item frames, paintings and cushions break.
    if crate::decoration::ticks(
        level
            .entities
            .store
            .get(id)
            .map_or(entity_ids::PLAYER, |e| e.kind()),
    ) {
        if delta != 0.0 {
            crate::decoration::break_in_level(level, id);
        }
        return;
    }
    let [sx, sy, sz] = direction.normal();
    let wanted = Vec3::new(
        delta * f64::from(sx),
        delta * f64::from(sy),
        delta * f64::from(sz),
    );
    if level.entities.item(id).is_some_and(|d| d.no_physics) {
        // `Entity.move` with `noPhysics` (an item inside a block): no piston limit, no
        // collisions, then `applyEffectsFromBlocks` along the way.
        let Some((pos, dims)) = level.entities.store.get(id).map(|e| (e.pos(), e.dims())) else {
            return;
        };
        let to = pos + wanted;
        if let Some(mut entity) = level.entities.store.get_mut(id) {
            entity.set_pos(to);
        }
        effects_along(level, id, dims, pos, to);
        return;
    }
    let Some(wanted) = limit_piston_movement(level, id, wanted) else {
        return;
    };
    if level.entities.mob(id).is_some() {
        // A mob moves like in its own tick (`LivingEntity` collision, steps, restitution).
        crate::level::mob_env::with_mob_env(level, |level, ents, env, cx| {
            let mut t = crate::mob::Tick {
                level,
                ents,
                env,
                cx,
                id,
            };
            NOCLIP.set(Some(piston));
            crate::mob::living::move_by_piston(&mut t, wanted);
            NOCLIP.set(None);
        });
        return;
    }
    let Some((pos, aabb, dims, on_ground)) = level.entities.store.get(id).map(|e| {
        (
            e.pos(),
            e.aabb(),
            e.dims(),
            e.flags().contains(EntityFlags::ON_GROUND),
        )
    }) else {
        return;
    };
    #[allow(clippy::cast_possible_truncation)]
    let step = level
        .entities
        .mob(id)
        .map_or(0.0, |m| m.attrs.step_height as f32);
    let mut boxes = std::mem::take(&mut level.pistons.boxes);
    let solid = entity_collisions(level, id, aabb, wanted);
    NOCLIP.set(Some(piston));
    let moved = if solid.is_empty() {
        crate::mob::physics::collide_in_context(
            &level.view(),
            aabb,
            wanted,
            step,
            on_ground,
            &mut boxes,
            false,
            crate::mob::physics::SnowCollision::None,
            true,
        )
    } else {
        // `collideBoundingBox` with the entity collisions (no step up off a mob).
        use crate::mob::physics;
        boxes.clear();
        boxes.extend_from_slice(&solid);
        let area = aabb.expand_towards(wanted.x, wanted.y, wanted.z);
        physics::border_boxes(&level.border, &aabb, &area, &mut boxes);
        physics::block_boxes_for(
            &level.view(),
            &area,
            &mut boxes,
            None,
            physics::SnowCollision::None,
            Some(aabb.min_y),
        );
        physics::collide_with_shapes(wanted, aabb, &boxes)
    };
    NOCLIP.set(None);
    level.pistons.boxes = boxes;
    let length = moved.length_sqr();
    let to = pos + moved;
    if (length > 1.0e-7 || wanted.length_sqr() - length < 1.0e-7)
        && let Some(mut entity) = level.entities.store.get_mut(id)
    {
        entity.set_pos(to);
    }
    // `setOnGroundWithMovement(verticalCollision && delta.y < 0.0, ...)`, then
    // `restituteMovementAfterCollisions` without bounciness (`-x * 0.0` and `(0.0 - y) * 0.0`
    // keep Java's signed zeros).
    let vertical = wanted.y != moved.y;
    let below = vertical && wanted.y < 0.0;
    let x_collision = !java_equal(wanted.x, moved.x);
    let z_collision = !java_equal(wanted.z, moved.z);
    if let Some(mut entity) = level.entities.store.get_mut(id) {
        entity.set_flags(EntityFlags::ON_GROUND, below);
        if vertical && wanted.y != 0.0 || x_collision || z_collision {
            let mut v = entity.as_ref().velocity();
            if x_collision {
                v.x = -v.x * 0.0;
            }
            if z_collision {
                v.z = -v.z * 0.0;
            }
            if vertical {
                v.y = (0.0 - v.y) * 0.0;
            }
            entity.set_velocity(v);
        }
    }
    effects_along(level, id, dims, pos, to);
}

/// `applyEffectsFromBlocks(previousPos, position())`.
fn effects_along(
    level: &mut Level,
    id: EntityId,
    dims: vanillite_world::entity::Dimensions,
    from: Vec3,
    to: Vec3,
) {
    let way = [crate::survival::inside::Movement::plain(from, to)];
    let mut cells = Vec::new();
    crate::block::plate::for_each_inside(level, dims, &way, |p| cells.push(p));
    for p in cells {
        if level.entities.store.contains(id) {
            level.entity_inside(p, id);
        }
    }
}

/// `getEntityCollisions(entity, box.expandTowards(movement))` of an entity that is not a mob:
/// boats and minecarts collide with what `AbstractBoat.canVehicleCollide` allows, other
/// entities with boats, live shulkers and adult happy ghasts (`canBeCollidedWith`); never
/// with an entity of the same root vehicle.
fn entity_collisions(level: &Level, id: EntityId, aabb: Aabb, movement: Vec3) -> Vec<Aabb> {
    let area = aabb.expand_towards(movement.x, movement.y, movement.z);
    let size = (area.max_x - area.min_x + area.max_y - area.min_y + area.max_z - area.min_z) / 3.0;
    let mut out = Vec::new();
    if size < 1.0e-7 {
        return out;
    }
    let ents = &level.entities;
    let root = |mut e: EntityId| {
        while let Some(v) = ents.vehicle_of(e) {
            e = v;
        }
        e
    };
    let is_vehicle = |o: EntityId| {
        ents.store.get(o).is_some_and(|e| {
            crate::vehicle::BoatKind::of(e.kind()).is_some()
                || crate::minecart::MinecartKind::of(e.kind()).is_some()
        })
    };
    let vehicle = is_vehicle(id);
    let own_root = root(id);
    let test = area.inflate(1.0e-7, 1.0e-7, 1.0e-7);
    ents.store.for_each_in(&test, false, |o| {
        if o == id || root(o) == own_root {
            return;
        }
        let solid = if vehicle {
            crate::vehicle::blocks_boat(ents, o)
        } else {
            ents.store
                .get(o)
                .is_some_and(|e| crate::vehicle::BoatKind::of(e.kind()).is_some())
                || ents.mob(o).is_some_and(|m| {
                    m.is_alive()
                        && (m.kind == crate::mob::Kind::Shulker
                            || m.kind == crate::mob::Kind::HappyGhast && !m.baby)
                })
        };
        if solid && let Some(e) = ents.store.get(o) {
            out.push(e.aabb());
        }
    });
    out
}

/// `Mth.equal`: within `1.0E-5F`.
fn java_equal(a: f64, b: f64) -> bool {
    (b - a).abs() < f64::from(1.0e-5_f32)
}

/// `Entity.limitPistonMovement`: the movement of one axis, capped so the entity moves at most
/// 0.51 along each axis per game tick; `None` when nothing is left.
fn limit_piston_movement(level: &mut Level, id: EntityId, v: Vec3) -> Option<Vec3> {
    if v.length_sqr() <= 1.0e-7 {
        return Some(v);
    }
    let now = level.game_time();
    let scratch = &mut level.pistons;
    if scratch.deltas_time != now {
        scratch.deltas.clear();
        scratch.deltas_time = now;
    }
    let deltas = scratch.deltas.entry(id).or_insert([0.0; 3]);
    let (axis, amount) = if v.x != 0.0 {
        (0, v.x)
    } else if v.y != 0.0 {
        (1, v.y)
    } else if v.z != 0.0 {
        (2, v.z)
    } else {
        return None;
    };
    let min = (amount + deltas[axis]).clamp(-0.51, 0.51);
    let amount = min - deltas[axis];
    deltas[axis] = min;
    if amount.abs() <= f64::from(1.0e-5_f32) {
        return None;
    }
    let mut out = Vec3::ZERO;
    match axis {
        0 => out.x = amount,
        1 => out.y = amount,
        _ => out.z = amount,
    }
    Some(out)
}

/// `moveCollidedEntities(level, pos, newProgress, self)`.
fn move_collided_entities(level: &mut Level, pos: BlockPos, new_progress: f32, me: &MovingPiston) {
    let direction = me.movement_direction();
    let delta_progress = f64::from(new_progress - me.progress);
    let shape = me.collision_related_state().collision_shape();
    if shape.is_empty() {
        return;
    }
    let mut bounds = shape[0];
    for b in &shape[1..] {
        for i in 0..3 {
            bounds[i] = bounds[i].min(b[i]);
            bounds[i + 3] = bounds[i + 3].max(b[i + 3]);
        }
    }
    let aabb = moved_box(pos, &bounds, me);
    let area = minmax(movement_area(aabb, direction, delta_progress), aabb);
    let mut ids = std::mem::take(&mut level.pistons.entities);
    ids.clear();
    level
        .entities
        .store
        .for_each_in(&area, false, |id| ids.push(id));
    let bounce = me.moved_state.is(ids::SLIME_BLOCK);
    for &id in &ids {
        if !pushed_by_server(level, id) {
            continue;
        }
        if bounce && let Some(mut entity) = level.entities.store.get_mut(id) {
            let mut v = entity.as_ref().velocity();
            let [sx, sy, sz] = direction.normal();
            match axis(direction) {
                0 => v.x = f64::from(sx),
                1 => v.y = f64::from(sy),
                _ => v.z = f64::from(sz),
            }
            entity.set_velocity(v);
        }
        let Some(entity_box) = level.entities.store.get(id).map(|e| e.aabb()) else {
            continue;
        };
        let mut delta: f64 = 0.0;
        for b in shape {
            let moving = movement_area(moved_box(pos, b, me), direction, delta_progress);
            if moving.intersects(&entity_box) {
                delta = delta.max(movement(&moving, direction, &entity_box));
                if delta >= delta_progress {
                    break;
                }
            }
        }
        if delta > 0.0 {
            let delta = delta.min(delta_progress) + 0.01;
            move_entity(level, id, delta, direction, direction);
            if !me.extending && me.source {
                fix_entity_within_piston_base(level, pos, id, direction, delta_progress);
            }
        }
    }
    level.pistons.entities = ids;
}

/// `fixEntityWithinPistonBase(pos, entity, direction, deltaProgress)`.
fn fix_entity_within_piston_base(
    level: &mut Level,
    pos: BlockPos,
    id: EntityId,
    direction: Direction,
    delta_progress: f64,
) {
    let Some(entity_box) = level.entities.store.get(id).map(|e| e.aabb()) else {
        return;
    };
    let block = Aabb::of_block(pos);
    if !entity_box.intersects(&block) {
        return;
    }
    let opposite = direction.opposite();
    let delta = movement(&block, opposite, &entity_box) + 0.01;
    let intersected = Aabb::new(
        entity_box.min_x.max(block.min_x),
        entity_box.min_y.max(block.min_y),
        entity_box.min_z.max(block.min_z),
        entity_box.max_x.min(block.max_x),
        entity_box.max_y.min(block.max_y),
        entity_box.max_z.min(block.max_z),
    );
    let delta_intersected = movement(&block, opposite, &intersected) + 0.01;
    if (delta - delta_intersected).abs() < 0.01 {
        let delta = delta.min(delta_progress) + 0.01;
        move_entity(level, id, delta, direction, opposite);
    }
}

/// `moveStuckEntities(level, pos, newProgress, self)`: honey carries entities standing on it
/// sideways.
fn move_stuck_entities(level: &mut Level, pos: BlockPos, new_progress: f32, me: &MovingPiston) {
    if !me.moved_state.is(ids::HONEY_BLOCK) {
        return;
    }
    let direction = me.movement_direction();
    if axis(direction) == 1 {
        return;
    }
    let sticky_top = me
        .moved_state
        .collision_shape()
        .iter()
        .map(|b| b[4])
        .fold(f64::NEG_INFINITY, f64::max);
    let aabb = moved_box(pos, &[0.0, sticky_top, 0.0, 1.0, 1.500_001, 1.0], me);
    let delta_progress = f64::from(new_progress - me.progress);
    let mut ids = std::mem::take(&mut level.pistons.entities);
    ids.clear();
    level
        .entities
        .store
        .for_each_in(&aabb, false, |id| ids.push(id));
    for &id in &ids {
        // `matchesStickyCritera`: on the ground and above the block (`isSupportedBy` is
        // approximated by the position test until entities know their supporting block).
        let matches = level.entities.store.get(id).is_some_and(|e| {
            let p = e.pos();
            e.flags().contains(EntityFlags::ON_GROUND)
                && p.x >= aabb.min_x
                && p.x <= aabb.max_x
                && p.z >= aabb.min_z
                && p.z <= aabb.max_z
        });
        if matches && pushed_by_server(level, id) {
            move_entity(level, id, delta_progress, direction, direction);
        }
    }
    level.pistons.entities = ids;
}
