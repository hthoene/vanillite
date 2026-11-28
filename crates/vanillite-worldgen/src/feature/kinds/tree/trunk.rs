//! Trunk placers (`trunkplacers/`): place a tree's logs and say where its foliage goes.

use serde_json::Value;
use vanillite_base::math::{cos, floor, floor_f};
use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Direction};

use super::foliage::FoliageAttachment;
use super::{
    Axis, TreeBlocks, TreeWriter, dist_manhattan, random_horizontal, relative, shuffle, with_axis,
};
use crate::Error;
use crate::blocks::BlockSet;
use crate::carver::providers::{IntProvider, random_between_inclusive};
use crate::feature::region::WorldGenRegion;
use crate::feature::{FeatureParser, field, float_field, int_field, int_or};

/// `TrunkPlacer`: the shared height parameters plus the placer type.
#[derive(Clone, Debug)]
pub struct TrunkPlacer {
    base_height: i32,
    height_rand_a: i32,
    height_rand_b: i32,
    kind: TrunkKind,
}

#[derive(Clone, Debug)]
enum TrunkKind {
    /// `StraightTrunkPlacer`.
    Straight,
    /// `ForkingTrunkPlacer`.
    Forking,
    /// `GiantTrunkPlacer`.
    Giant,
    /// `MegaJungleTrunkPlacer`.
    MegaJungle,
    /// `DarkOakTrunkPlacer`.
    DarkOak,
    /// `FancyTrunkPlacer`.
    Fancy,
    /// `BendingTrunkPlacer`.
    Bending {
        min_height_for_leaves: i32,
        bend_length: IntProvider,
    },
    /// `UpwardsBranchingTrunkPlacer`.
    UpwardsBranching {
        extra_branch_steps: IntProvider,
        place_branch_per_log_probability: f32,
        extra_branch_length: IntProvider,
        can_grow_through: BlockSet,
    },
    /// `CherryTrunkPlacer`.
    Cherry {
        branch_count: IntProvider,
        branch_horizontal_length: IntProvider,
        /// `branch_start_offset_from_top` (a `UniformInt`), inclusive bounds.
        branch_start_offset_from_top: (i32, i32),
        /// `secondBranchStartOffsetFromTop`: the same range one shorter at the top.
        second_branch_start_offset_from_top: (i32, i32),
        branch_end_offset_from_top: IntProvider,
    },
    /// `PoplarTrunkPlacer`.
    Poplar {
        trunk_height_above_branches: IntProvider,
        branch_amount: IntProvider,
    },
}

fn int_in(json: &Value, name: &str, min: i32, max: i32) -> Result<i32, Error> {
    let value = int_field(json, name)?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(Error::new(format!("{name} {value} outside [{min}, {max}]")))
    }
}

fn int_provider(json: &Value, name: &str, min: i32, max: i32) -> Result<IntProvider, Error> {
    IntProvider::from_json_in_range(field(json, name)?, min, max)
}

impl TrunkPlacer {
    /// Parses `TrunkPlacer.CODEC`.
    pub fn parse(json: &Value, parser: &mut FeatureParser<'_>) -> Result<Self, Error> {
        let kind = json
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new("trunk placer without type"))?;
        let kind = match kind.strip_prefix("minecraft:").unwrap_or(kind) {
            "straight_trunk_placer" => TrunkKind::Straight,
            "forking_trunk_placer" => TrunkKind::Forking,
            "giant_trunk_placer" => TrunkKind::Giant,
            "mega_jungle_trunk_placer" => TrunkKind::MegaJungle,
            "dark_oak_trunk_placer" => TrunkKind::DarkOak,
            "fancy_trunk_placer" => TrunkKind::Fancy,
            "bending_trunk_placer" => TrunkKind::Bending {
                min_height_for_leaves: {
                    let value = int_or(json, "min_height_for_leaves", 1);
                    if value < 1 {
                        return Err(Error::new("min_height_for_leaves must be positive"));
                    }
                    value
                },
                bend_length: int_provider(json, "bend_length", 1, 64)?,
            },
            "upwards_branching_trunk_placer" => TrunkKind::UpwardsBranching {
                extra_branch_steps: int_provider(json, "extra_branch_steps", 1, i32::MAX)?,
                place_branch_per_log_probability: {
                    let p = float_field(json, "place_branch_per_log_probability")?;
                    if !(0.0..=1.0).contains(&p) {
                        return Err(Error::new(
                            "place_branch_per_log_probability outside [0, 1]",
                        ));
                    }
                    p
                },
                extra_branch_length: int_provider(json, "extra_branch_length", 0, i32::MAX)?,
                can_grow_through: parser.block_set(field(json, "can_grow_through")?)?,
            },
            "cherry_trunk_placer" => {
                let start = field(json, "branch_start_offset_from_top")?;
                let min = int_field(start, "min_inclusive")?;
                let max = int_field(start, "max_inclusive")?;
                if max - min < 1 || min < -16 || max > 0 {
                    return Err(Error::new("invalid branch_start_offset_from_top"));
                }
                TrunkKind::Cherry {
                    branch_count: int_provider(json, "branch_count", 1, 3)?,
                    branch_horizontal_length: int_provider(
                        json,
                        "branch_horizontal_length",
                        2,
                        16,
                    )?,
                    branch_start_offset_from_top: (min, max),
                    second_branch_start_offset_from_top: (min, max - 1),
                    branch_end_offset_from_top: int_provider(
                        json,
                        "branch_end_offset_from_top",
                        -16,
                        16,
                    )?,
                }
            }
            "poplar_trunk_placer" => TrunkKind::Poplar {
                trunk_height_above_branches: int_provider(
                    json,
                    "trunk_height_above_branches",
                    0,
                    8,
                )?,
                branch_amount: int_provider(json, "branch_amount", 1, 4)?,
            },
            other => return Err(Error::new(format!("unknown trunk placer type {other}"))),
        };
        Ok(Self {
            base_height: int_in(json, "base_height", 0, 32)?,
            height_rand_a: int_in(json, "height_rand_a", 0, 24)?,
            height_rand_b: int_in(json, "height_rand_b", 0, 24)?,
            kind,
        })
    }

    /// `getTreeHeight(random)`.
    pub fn tree_height<R: RandomSource>(&self, random: &mut R) -> i32 {
        let a = random.next_int_bounded(self.height_rand_a + 1);
        let b = random.next_int_bounded(self.height_rand_b + 1);
        self.base_height + a + b
    }

    /// `validTreePos(level, pos)` (with `UpwardsBranchingTrunkPlacer`'s extra blocks).
    fn valid_tree_pos(
        &self,
        level: &WorldGenRegion<'_>,
        blocks: &TreeBlocks,
        pos: BlockPos,
    ) -> bool {
        blocks.valid_tree_pos(level, pos)
            || matches!(&self.kind, TrunkKind::UpwardsBranching { can_grow_through, .. }
                if can_grow_through.contains(level.block(pos)))
    }

    /// `isFree(level, pos)`: a valid tree position or a log.
    pub fn is_free(&self, level: &WorldGenRegion<'_>, blocks: &TreeBlocks, pos: BlockPos) -> bool {
        self.valid_tree_pos(level, blocks, pos) || blocks.logs.contains(level.block(pos))
    }

    /// `placeLog(level, trunkSetter, random, pos, tree, stateModifier)`; the modifier is either
    /// the identity (`None`) or `trySetValue(AXIS, axis)`.
    fn place_log(&self, w: &mut TreeWriter<'_, '_, '_>, pos: BlockPos, axis: Option<Axis>) -> bool {
        if !self.valid_tree_pos(w.level(), w.blocks(), pos) {
            return false;
        }
        let state = w.trunk_state(pos);
        let state = match axis {
            Some(axis) => with_axis(state, axis),
            None => state,
        };
        w.set_trunk(pos, state);
        true
    }

    /// `placeLogIfFree`.
    fn place_log_if_free(&self, w: &mut TreeWriter<'_, '_, '_>, pos: BlockPos) {
        if self.is_free(w.level(), w.blocks(), pos) {
            self.place_log(w, pos, None);
        }
    }

    /// `placeTrunk(level, trunkSetter, random, treeHeight, origin, tree)`.
    pub fn place_trunk(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
    ) -> Vec<FoliageAttachment> {
        match &self.kind {
            TrunkKind::Straight => self.straight(w, tree_height, origin),
            TrunkKind::Forking => self.forking(w, tree_height, origin),
            TrunkKind::Giant => self.giant(w, tree_height, origin),
            TrunkKind::MegaJungle => self.mega_jungle(w, tree_height, origin),
            TrunkKind::DarkOak => self.dark_oak(w, tree_height, origin),
            TrunkKind::Fancy => self.fancy(w, tree_height, origin),
            TrunkKind::Bending {
                min_height_for_leaves,
                bend_length,
            } => self.bending(w, tree_height, origin, *min_height_for_leaves, bend_length),
            TrunkKind::UpwardsBranching {
                extra_branch_steps,
                place_branch_per_log_probability,
                extra_branch_length,
                ..
            } => self.upwards_branching(
                w,
                tree_height,
                origin,
                extra_branch_steps,
                *place_branch_per_log_probability,
                extra_branch_length,
            ),
            TrunkKind::Cherry { .. } => self.cherry(w, tree_height, origin),
            TrunkKind::Poplar {
                trunk_height_above_branches,
                branch_amount,
            } => self.poplar(
                w,
                tree_height,
                origin,
                trunk_height_above_branches,
                branch_amount,
            ),
        }
    }

    /// `StraightTrunkPlacer.placeTrunk`.
    fn straight(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
    ) -> Vec<FoliageAttachment> {
        w.place_below_trunk_block(origin.offset(0, -1, 0));
        for y in 0..tree_height {
            self.place_log(w, origin.offset(0, y, 0), None);
        }
        vec![FoliageAttachment::new(
            origin.offset(0, tree_height, 0),
            0,
            false,
        )]
    }

    /// `ForkingTrunkPlacer.placeTrunk`.
    fn forking(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
    ) -> Vec<FoliageAttachment> {
        w.place_below_trunk_block(origin.offset(0, -1, 0));
        let mut attachments = Vec::new();
        let lean_direction = random_horizontal(w.random());
        let lean_height = tree_height - w.random().next_int_bounded(4) - 1;
        let mut lean_steps = 3 - w.random().next_int_bounded(3);
        let [lean_x, _, lean_z] = lean_direction.normal();
        let mut tx = origin.x;
        let mut tz = origin.z;
        let mut ey = None;
        for yo in 0..tree_height {
            let yy = origin.y + yo;
            if yo >= lean_height && lean_steps > 0 {
                tx += lean_x;
                tz += lean_z;
                lean_steps -= 1;
            }
            if self.place_log(w, BlockPos::new(tx, yy, tz), None) {
                ey = Some(yy + 1);
            }
        }
        if let Some(ey) = ey {
            attachments.push(FoliageAttachment::new(BlockPos::new(tx, ey, tz), 1, false));
        }
        tx = origin.x;
        tz = origin.z;
        let branch_direction = random_horizontal(w.random());
        if branch_direction != lean_direction {
            let branch_pos = lean_height - w.random().next_int_bounded(2) - 1;
            let mut branch_steps = 1 + w.random().next_int_bounded(3);
            let [branch_x, _, branch_z] = branch_direction.normal();
            let mut ey = None;
            let mut yo = branch_pos;
            while yo < tree_height && branch_steps > 0 {
                if yo >= 1 {
                    let yy = origin.y + yo;
                    tx += branch_x;
                    tz += branch_z;
                    if self.place_log(w, BlockPos::new(tx, yy, tz), None) {
                        ey = Some(yy + 1);
                    }
                }
                yo += 1;
                branch_steps -= 1;
            }
            if let Some(ey) = ey {
                attachments.push(FoliageAttachment::new(BlockPos::new(tx, ey, tz), 0, false));
            }
        }
        attachments
    }

    /// `GiantTrunkPlacer.placeTrunk`.
    fn giant(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
    ) -> Vec<FoliageAttachment> {
        let below = origin.offset(0, -1, 0);
        w.place_below_trunk_block(below);
        w.place_below_trunk_block(below.offset(1, 0, 0));
        w.place_below_trunk_block(below.offset(0, 0, 1));
        w.place_below_trunk_block(below.offset(1, 0, 1));
        for hh in 0..tree_height {
            self.place_log_if_free(w, origin.offset(0, hh, 0));
            if hh < tree_height - 1 {
                self.place_log_if_free(w, origin.offset(1, hh, 0));
                self.place_log_if_free(w, origin.offset(1, hh, 1));
                self.place_log_if_free(w, origin.offset(0, hh, 1));
            }
        }
        vec![FoliageAttachment::new(
            origin.offset(0, tree_height, 0),
            0,
            true,
        )]
    }

    /// `MegaJungleTrunkPlacer.placeTrunk`.
    fn mega_jungle(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
    ) -> Vec<FoliageAttachment> {
        let mut attachments = self.giant(w, tree_height, origin);
        let mut branch_height = tree_height - 2 - w.random().next_int_bounded(4);
        while branch_height > tree_height / 2 {
            let angle = w.random().next_float() * std::f64::consts::TAU as f32;
            let mut bx = 0;
            let mut bz = 0;
            for b in 0..5 {
                bx = (1.5_f32 + cos(f64::from(angle)) * b as f32) as i32;
                bz = (1.5_f32 + vanillite_base::math::sin(f64::from(angle)) * b as f32) as i32;
                let pos = origin.offset(bx, branch_height - 3 + b / 2, bz);
                self.place_log(w, pos, None);
            }
            attachments.push(FoliageAttachment::new(
                origin.offset(bx, branch_height, bz),
                -2,
                false,
            ));
            branch_height -= 2 + w.random().next_int_bounded(4);
        }
        attachments
    }

    /// `DarkOakTrunkPlacer.placeTrunk`.
    fn dark_oak(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
    ) -> Vec<FoliageAttachment> {
        let mut attachments = Vec::new();
        let below = origin.offset(0, -1, 0);
        w.place_below_trunk_block(below);
        w.place_below_trunk_block(below.offset(1, 0, 0));
        w.place_below_trunk_block(below.offset(0, 0, 1));
        w.place_below_trunk_block(below.offset(1, 0, 1));
        let lean_direction = random_horizontal(w.random());
        let lean_height = tree_height - w.random().next_int_bounded(4);
        let mut lean_steps = 2 - w.random().next_int_bounded(3);
        let [lean_x, _, lean_z] = lean_direction.normal();
        let (x, y, z) = (origin.x, origin.y, origin.z);
        let mut tx = x;
        let mut tz = z;
        let ey = y + tree_height - 1;
        for dy in 0..tree_height {
            if dy >= lean_height && lean_steps > 0 {
                tx += lean_x;
                tz += lean_z;
                lean_steps -= 1;
            }
            let pos = BlockPos::new(tx, y + dy, tz);
            if w.blocks().is_air_or_leaves(w.level(), pos) {
                self.place_log(w, pos, None);
                self.place_log(w, pos.offset(1, 0, 0), None);
                self.place_log(w, pos.offset(0, 0, 1), None);
                self.place_log(w, pos.offset(1, 0, 1), None);
            }
        }
        attachments.push(FoliageAttachment::new(BlockPos::new(tx, ey, tz), 0, true));
        for ox in -1..=2 {
            for oz in -1..=2 {
                if (!(0..=1).contains(&ox) || !(0..=1).contains(&oz))
                    && w.random().next_int_bounded(3) <= 0
                {
                    let length = w.random().next_int_bounded(3) + 2;
                    for branch_y in 0..length {
                        self.place_log(w, BlockPos::new(x + ox, ey - branch_y - 1, z + oz), None);
                    }
                    attachments.push(FoliageAttachment::new(
                        BlockPos::new(x + ox, ey, z + oz),
                        0,
                        false,
                    ));
                }
            }
        }
        attachments
    }

    /// `BendingTrunkPlacer.placeTrunk`.
    fn bending(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
        min_height_for_leaves: i32,
        bend_length: &IntProvider,
    ) -> Vec<FoliageAttachment> {
        let direction = random_horizontal(w.random());
        let log_height = tree_height - 1;
        let mut pos = origin;
        w.place_below_trunk_block(pos.offset(0, -1, 0));
        let mut attachments = Vec::new();
        for i in 0..=log_height {
            if i + 1 >= log_height + w.random().next_int_bounded(2) {
                pos = pos.relative(direction);
            }
            if w.valid_tree_pos(pos) {
                self.place_log(w, pos, None);
            }
            if i >= min_height_for_leaves {
                attachments.push(FoliageAttachment::new(pos, 0, false));
            }
            pos = pos.offset(0, 1, 0);
        }
        let dir_length = bend_length.sample(w.random());
        for _ in 0..=dir_length {
            if w.valid_tree_pos(pos) {
                self.place_log(w, pos, None);
            }
            attachments.push(FoliageAttachment::new(pos, 0, false));
            pos = pos.relative(direction);
        }
        attachments
    }

    /// `UpwardsBranchingTrunkPlacer.placeTrunk`.
    fn upwards_branching(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
        extra_branch_steps: &IntProvider,
        place_branch_per_log_probability: f32,
        extra_branch_length: &IntProvider,
    ) -> Vec<FoliageAttachment> {
        let mut attachments = Vec::new();
        for height_pos in 0..tree_height {
            let current_height = origin.y + height_pos;
            let log_pos = BlockPos::new(origin.x, current_height, origin.z);
            if self.place_log(w, log_pos, None)
                && height_pos < tree_height - 1
                && w.random().next_float() < place_branch_per_log_probability
            {
                let branch_dir = random_horizontal(w.random());
                let branch_len = extra_branch_length.sample(w.random());
                let branch_pos = 0.max(branch_len - extra_branch_length.sample(w.random()) - 1);
                let branch_steps = extra_branch_steps.sample(w.random());
                self.place_branch(
                    w,
                    tree_height,
                    &mut attachments,
                    log_pos,
                    current_height,
                    branch_dir,
                    branch_pos,
                    branch_steps,
                );
            }
            if height_pos == tree_height - 1 {
                attachments.push(FoliageAttachment::new(
                    BlockPos::new(origin.x, current_height + 1, origin.z),
                    0,
                    false,
                ));
            }
        }
        attachments
    }

    /// `UpwardsBranchingTrunkPlacer.placeBranch`.
    #[allow(clippy::too_many_arguments)]
    fn place_branch(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        attachments: &mut Vec<FoliageAttachment>,
        log_pos: BlockPos,
        current_height: i32,
        branch_dir: Direction,
        branch_pos: i32,
        mut branch_steps: i32,
    ) {
        let mut height_along_branch = current_height + branch_pos;
        let mut log_x = log_pos.x;
        let mut log_z = log_pos.z;
        let [step_x, _, step_z] = branch_dir.normal();
        let mut index = branch_pos;
        while index < tree_height && branch_steps > 0 {
            if index >= 1 {
                let placement_height = current_height + index;
                log_x += step_x;
                log_z += step_z;
                height_along_branch = placement_height;
                let pos = BlockPos::new(log_x, placement_height, log_z);
                if self.place_log(w, pos, None) {
                    height_along_branch = placement_height + 1;
                }
                attachments.push(FoliageAttachment::new(pos, 0, false));
            }
            index += 1;
            branch_steps -= 1;
        }
        if height_along_branch - current_height > 1 {
            let foliage_pos = BlockPos::new(log_x, height_along_branch, log_z);
            attachments.push(FoliageAttachment::new(foliage_pos, 0, false));
            attachments.push(FoliageAttachment::new(
                foliage_pos.offset(0, -2, 0),
                0,
                false,
            ));
        }
    }

    /// `CherryTrunkPlacer.placeTrunk`.
    fn cherry(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
    ) -> Vec<FoliageAttachment> {
        let TrunkKind::Cherry {
            branch_count,
            branch_start_offset_from_top: (start_min, start_max),
            second_branch_start_offset_from_top: (second_min, second_max),
            ..
        } = &self.kind
        else {
            unreachable!("cherry trunk placer");
        };
        w.place_below_trunk_block(origin.offset(0, -1, 0));
        let first_branch_offset =
            0.max(tree_height - 1 + random_between_inclusive(w.random(), *start_min, *start_max));
        let mut second_branch_offset =
            0.max(tree_height - 1 + random_between_inclusive(w.random(), *second_min, *second_max));
        if second_branch_offset >= first_branch_offset {
            second_branch_offset += 1;
        }
        let branch_count = branch_count.sample(w.random());
        let has_middle_branch = branch_count == 3;
        let has_both_side_branches = branch_count >= 2;
        let trunk_height = if has_middle_branch {
            tree_height
        } else if has_both_side_branches {
            first_branch_offset.max(second_branch_offset) + 1
        } else {
            first_branch_offset + 1
        };
        for y in 0..trunk_height {
            self.place_log(w, origin.offset(0, y, 0), None);
        }
        let mut attachments = Vec::new();
        if has_middle_branch {
            attachments.push(FoliageAttachment::new(
                origin.offset(0, trunk_height, 0),
                0,
                false,
            ));
        }
        let tree_direction = random_horizontal(w.random());
        let sideways = Axis::of(tree_direction);
        attachments.push(self.cherry_branch(
            w,
            tree_height,
            origin,
            sideways,
            tree_direction,
            first_branch_offset,
            first_branch_offset < trunk_height - 1,
        ));
        if has_both_side_branches {
            attachments.push(self.cherry_branch(
                w,
                tree_height,
                origin,
                sideways,
                tree_direction.opposite(),
                second_branch_offset,
                second_branch_offset < trunk_height - 1,
            ));
        }
        attachments
    }

    /// `CherryTrunkPlacer.generateBranch`.
    #[allow(clippy::too_many_arguments)]
    fn cherry_branch(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
        sideways: Axis,
        branch_direction: Direction,
        offset_from_origin: i32,
        middle_continues_upwards: bool,
    ) -> FoliageAttachment {
        let TrunkKind::Cherry {
            branch_horizontal_length,
            branch_end_offset_from_top,
            ..
        } = &self.kind
        else {
            unreachable!("cherry trunk placer");
        };
        let mut log_pos = origin.offset(0, offset_from_origin, 0);
        let branch_end_offset = tree_height - 1 + branch_end_offset_from_top.sample(w.random());
        let extend_away = middle_continues_upwards || branch_end_offset < offset_from_origin;
        let distance_to_trunk =
            branch_horizontal_length.sample(w.random()) + i32::from(extend_away);
        let branch_end =
            relative(origin, branch_direction, distance_to_trunk).offset(0, branch_end_offset, 0);
        let steps_horizontally = if extend_away { 2 } else { 1 };
        for _ in 0..steps_horizontally {
            log_pos = log_pos.relative(branch_direction);
            self.place_log(w, log_pos, Some(sideways));
        }
        let vertical_direction = if branch_end.y > log_pos.y {
            Direction::Up
        } else {
            Direction::Down
        };
        loop {
            let distance = dist_manhattan(log_pos, branch_end);
            if distance == 0 {
                return FoliageAttachment::new(branch_end.offset(0, 1, 0), 0, false);
            }
            let chance = (branch_end.y - log_pos.y).abs() as f32 / distance as f32;
            let grow_vertically = w.random().next_float() < chance;
            log_pos = log_pos.relative(if grow_vertically {
                vertical_direction
            } else {
                branch_direction
            });
            self.place_log(w, log_pos, (!grow_vertically).then_some(sideways));
        }
    }

    /// `PoplarTrunkPlacer.placeTrunk`.
    fn poplar(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
        trunk_height_above_branches: &IntProvider,
        branch_amount: &IntProvider,
    ) -> Vec<FoliageAttachment> {
        w.place_below_trunk_block(origin.offset(0, -1, 0));
        let up_to_branches = tree_height - trunk_height_above_branches.sample(w.random());
        for y in 0..tree_height {
            let pos = origin.offset(0, y, 0);
            self.place_log(w, pos, None);
            // `getShuffledBranchDirections`: all six directions shuffled, horizontal ones kept.
            let mut directions = Direction::ALL;
            shuffle(&mut directions, w.random());
            if up_to_branches - 1 == y {
                let branches = branch_amount.sample(w.random());
                let horizontal = directions
                    .iter()
                    .copied()
                    .filter(|d| !matches!(d, Direction::Up | Direction::Down));
                for branch_direction in horizontal.take(branches.max(0) as usize) {
                    self.place_log(
                        w,
                        pos.relative(branch_direction),
                        Some(Axis::of(branch_direction)),
                    );
                }
            }
        }
        vec![FoliageAttachment::new(
            origin.offset(0, up_to_branches, 0),
            0,
            false,
        )]
    }

    /// `FancyTrunkPlacer.placeTrunk`.
    fn fancy(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        tree_height: i32,
        origin: BlockPos,
    ) -> Vec<FoliageAttachment> {
        let height = tree_height + 2;
        let trunk_height = floor(f64::from(height) * 0.618);
        w.place_below_trunk_block(origin.offset(0, -1, 0));
        let clusters_per_y = 1.min(floor(
            1.382 + (1.0 * f64::from(height) / 13.0) * (1.0 * f64::from(height) / 13.0),
        ));
        let trunk_top = origin.y + trunk_height;
        let mut relative_y = height - 5;
        // (attachment position, branch base)
        let mut foliage_coords = vec![(origin.offset(0, relative_y, 0), trunk_top)];
        while relative_y >= 0 {
            let tree_shape = fancy_tree_shape(height, relative_y);
            if tree_shape >= 0.0 || tree_shape.is_nan() {
                for _ in 0..clusters_per_y {
                    let radius =
                        1.0 * f64::from(tree_shape) * (f64::from(w.random().next_float()) + 0.328);
                    let angle = f64::from(w.random().next_float() * 2.0_f32) * std::f64::consts::PI;
                    let x = radius * crate::mth::math_sin(angle) + 0.5;
                    let z = radius * crate::mth::math_cos(angle) + 0.5;
                    let check_start = origin.offset(floor(x), relative_y - 1, floor(z));
                    let check_end = check_start.offset(0, 5, 0);
                    if self.make_limb(w, check_start, check_end, false) {
                        let dx = origin.x - check_start.x;
                        let dz = origin.z - check_start.z;
                        let branch_height =
                            f64::from(check_start.y) - f64::from(dx * dx + dz * dz).sqrt() * 0.381;
                        let branch_top = if branch_height > f64::from(trunk_top) {
                            trunk_top
                        } else {
                            branch_height as i32
                        };
                        let check_branch_base = BlockPos::new(origin.x, branch_top, origin.z);
                        if self.make_limb(w, check_branch_base, check_start, false) {
                            foliage_coords.push((check_start, check_branch_base.y));
                        }
                    }
                }
            }
            relative_y -= 1;
        }
        self.make_limb(w, origin, origin.offset(0, trunk_height, 0), true);
        // `makeBranches`.
        for &(end, branch_base) in &foliage_coords {
            let base = BlockPos::new(origin.x, branch_base, origin.z);
            if base != end && fancy_trim_branches(height, branch_base - origin.y) {
                self.make_limb(w, base, end, true);
            }
        }
        foliage_coords
            .iter()
            .filter(|&&(_, branch_base)| fancy_trim_branches(height, branch_base - origin.y))
            .map(|&(pos, _)| FoliageAttachment::new(pos, 0, false))
            .collect()
    }

    /// `FancyTrunkPlacer.makeLimb`.
    fn make_limb(
        &self,
        w: &mut TreeWriter<'_, '_, '_>,
        start: BlockPos,
        end: BlockPos,
        do_place: bool,
    ) -> bool {
        if !do_place && start == end {
            return true;
        }
        let delta = end.offset(-start.x, -start.y, -start.z);
        let steps = delta.x.abs().max(delta.y.abs().max(delta.z.abs()));
        let dx = delta.x as f32 / steps as f32;
        let dy = delta.y as f32 / steps as f32;
        let dz = delta.z as f32 / steps as f32;
        for i in 0..=steps {
            let fi = i as f32;
            let pos = start.offset(
                floor_f(0.5 + fi * dx),
                floor_f(0.5 + fi * dy),
                floor_f(0.5 + fi * dz),
            );
            if do_place {
                self.place_log(w, pos, Some(fancy_log_axis(start, pos)));
            } else if !self.is_free(w.level(), w.blocks(), pos) {
                return false;
            }
        }
        true
    }
}

/// `FancyTrunkPlacer.getLogAxis`.
fn fancy_log_axis(start: BlockPos, pos: BlockPos) -> Axis {
    let x_diff = (pos.x - start.x).abs();
    let z_diff = (pos.z - start.z).abs();
    let max_diff = x_diff.max(z_diff);
    if max_diff > 0 {
        if x_diff == max_diff { Axis::X } else { Axis::Z }
    } else {
        Axis::Y
    }
}

/// `FancyTrunkPlacer.trimBranches`.
fn fancy_trim_branches(height: i32, local_y: i32) -> bool {
    f64::from(local_y) >= f64::from(height) * 0.2
}

/// `FancyTrunkPlacer.treeShape`.
fn fancy_tree_shape(height: i32, y: i32) -> f32 {
    if (y as f32) < height as f32 * 0.3_f32 {
        return -1.0;
    }
    let radius = height as f32 / 2.0;
    let adjacent = radius - y as f32;
    let mut distance = crate::mth::sqrt_f(radius * radius - adjacent * adjacent);
    if adjacent == 0.0 {
        distance = radius;
    } else if adjacent.abs() >= radius {
        return 0.0;
    }
    distance * 0.5
}
