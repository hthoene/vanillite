//! `BlockState.canSurvive(level, pos)`: the `canSurvive` and `mayPlaceOn` overrides of every
//! block class, read from a [`WorldGenRegion`].

use vanillite_base::{BlockPos, Direction};
use vanillite_data::block::{BlockState, ids};

use super::BlockLevel;
use super::class::{Class, class_of};
use super::geometry::{is_face_empty, is_face_full};
use super::props::{HORIZONTAL, PosExt, StateExt, clockwise, counter_clockwise};
use super::tables::{can_burn, tags};
use super::{
    SupportType, can_attach_multiface, can_support_center, can_support_rigid_block, face_sturdy_at,
    is_face_sturdy, is_solid, raw_brightness,
};

/// `state.canSurvive(level, pos)`.
pub(crate) fn can_survive(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> bool {
    let class = class_of(state);
    let t = &tags().blocks;
    match class {
        Class::Plain => true,

        // `VegetationBlock` and its subclasses: the block below must suit `mayPlaceOn`.
        Class::Vegetation
        | Class::Azalea
        | Class::AttachedStem
        | Class::CactusFlower
        | Class::DryVegetation
        | Class::FlowerBed
        | Class::LilyPad
        | Class::NetherFungus
        | Class::NetherRoots
        | Class::NetherSprouts
        | Class::NetherWart
        | Class::Seagrass
        | Class::Stem
        | Class::WitherRose => vegetation_can_survive(level, class, state, pos),
        Class::Crop => {
            raw_brightness(level, pos) >= 8 && vegetation_can_survive(level, class, state, pos)
        }
        Class::Mushroom => {
            let below_pos = pos.below();
            let below = level.block(below_pos);
            t.overrides_mushroom_light_requirement.contains(below)
                || raw_brightness(level, pos) < 13
                    && may_place_on(level, class, state, below, below_pos)
        }
        Class::MangrovePropagule => {
            if state.flag("hanging") {
                t.supports_hanging_mangrove_propagule
                    .contains(level.block(pos.above()))
            } else {
                vegetation_can_survive(level, class, state, pos)
            }
        }
        Class::SeaPickle => {
            let below_pos = pos.below();
            may_place_on(level, class, state, level.block(below_pos), below_pos)
        }
        Class::LeafLitter => sturdy_at(level, pos.below(), Direction::Up),
        Class::DoublePlant => double_plant_can_survive(level, class, state, pos),
        Class::SmallDripleaf => {
            if state.value("half") == Some("upper") {
                double_plant_can_survive(level, class, state, pos)
            } else {
                let below_pos = pos.below();
                may_place_on(level, class, state, level.block(below_pos), below_pos)
            }
        }
        Class::TallSeagrass => {
            if state.value("half") == Some("upper") {
                upper_half_on_lower(level, state, pos)
            } else {
                let fluid = level.fluid(pos);
                double_plant_can_survive(level, class, state, pos)
                    && tags().fluids.water.contains(fluid)
                    && fluid.info().amount == 8
            }
        }
        Class::PitcherCrop => {
            let lower = state.value("half") == Some("lower");
            !(lower && raw_brightness(level, pos) < 8)
                && double_plant_can_survive(level, class, state, pos)
        }

        Class::AmethystCluster
        | Class::BaseCoralWallFan
        | Class::CoralWallFan
        | Class::ShelfMushroom
        | Class::WallTorch
        | Class::RedstoneWallTorch
        | Class::Ladder => match state.direction("facing") {
            // `WallTorchBlock.canSurvive(level, pos, facing)` and its copies.
            Some(facing) => sturdy_at(level, pos.relative(facing.opposite()), facing),
            None => true,
        },
        Class::TripWireHook => match state.direction("facing") {
            Some(facing) => {
                super::props::is_horizontal(facing)
                    && sturdy_at(level, pos.relative(facing.opposite()), facing)
            }
            None => true,
        },
        Class::BambooSapling | Class::BambooStalk => {
            t.supports_bamboo.contains(level.block(pos.below()))
        }
        Class::Banner | Class::Cake | Class::CandleCake | Class::StandingSign => {
            is_solid(level.block(pos.below()))
        }
        Class::WallBanner | Class::WallSign => match state.direction("facing") {
            Some(facing) => is_solid(level.block(pos.relative(facing.opposite()))),
            None => true,
        },
        Class::BaseCoralPlantType | Class::CoralPlant | Class::CoralFan => {
            sturdy_at(level, pos.below(), Direction::Up)
        }
        Class::BasePressurePlate => {
            let below = pos.below();
            can_support_rigid_block(level, below) || can_support_center(level, below, Direction::Up)
        }
        Class::Rail | Class::PoweredRail | Class::DetectorRail => {
            can_support_rigid_block(level, pos.below())
        }
        Class::BaseTorch | Class::Candle => can_support_center(level, pos.below(), Direction::Up),
        Class::Bell => {
            let connection = bell_connected_direction(state).opposite();
            if connection == Direction::Up {
                can_support_center(level, pos.above(), Direction::Down)
            } else {
                can_attach(level, pos, connection)
            }
        }
        Class::BigDripleaf => {
            let below = level.block(pos.below());
            below.is(ids::BIG_DRIPLEAF)
                || below.is(ids::BIG_DRIPLEAF_STEM)
                || t.supports_big_dripleaf.contains(below)
        }
        Class::BigDripleafStem => {
            let below = level.block(pos.below());
            let above = level.block(pos.above());
            (below.is(ids::BIG_DRIPLEAF_STEM) || t.supports_big_dripleaf.contains(below))
                && (above.is(ids::BIG_DRIPLEAF_STEM) || above.is(ids::BIG_DRIPLEAF))
        }
        Class::BubbleColumn => {
            let below = level.block(pos.below());
            below.is(ids::BUBBLE_COLUMN)
                || t.enables_bubble_column_push_up.contains(below)
                || t.enables_bubble_column_drag_down.contains(below)
        }
        Class::Cactus => cactus_can_survive(level, pos),
        Class::Carpet => !level.is_empty_block(pos.below()),
        Class::CeilingHangingSign => face_sturdy_at(
            level,
            pos.above(),
            level.block(pos.above()),
            Direction::Down,
            SupportType::Center,
        ),
        Class::ChorusFlower => chorus_flower_can_survive(level, pos),
        Class::ChorusPlant => chorus_plant_can_survive(level, pos),
        Class::Cocoa => match state.direction("facing") {
            Some(facing) => t.supports_cocoa.contains(level.block(pos.relative(facing))),
            None => true,
        },
        Class::Comparator | Class::Repeater => {
            diode_can_survive_on(level, pos.below(), level.block(pos.below()))
        }
        Class::Door => {
            let below_pos = pos.below();
            let below = level.block(below_pos);
            if state.value("half") == Some("lower") {
                face_sturdy_at(level, below_pos, below, Direction::Up, SupportType::Full)
            } else {
                below.block() == state.block()
            }
        }
        Class::FaceAttachedHorizontalDirectional => {
            can_attach(level, pos, face_connected_direction(state).opposite())
        }
        Class::Farmland => {
            let above = level.block(pos.above());
            !is_solid(above) || t.maintains_farmland.contains(above)
        }
        Class::Fire => {
            sturdy_at(level, pos.below(), Direction::Up) || is_valid_fire_location(level, pos)
        }
        Class::SoulFire => t.soul_fire_base_blocks.contains(level.block(pos.below())),
        Class::Frogspawn => {
            let below = pos.below();
            let fluid = level.fluid(below);
            (tags().fluids.supports_frogspawn.contains(fluid)
                || t.supports_frogspawn.contains(level.block(below)))
                && level.fluid(below.above()).is_empty()
        }
        Class::Kelp
        | Class::KelpPlant
        | Class::CaveVines
        | Class::CaveVinesPlant
        | Class::WeepingVines
        | Class::WeepingVinesPlant
        | Class::TwistingVines
        | Class::TwistingVinesPlant => growing_plant_can_survive(level, class, pos),
        Class::HangingMoss => hanging_moss_can_stay(level, pos),
        Class::HangingRoots => sturdy_at(level, pos.above(), Direction::Down),
        Class::Lantern => {
            let direction = lantern_connected_direction(state).opposite();
            can_support_center(level, pos.relative(direction), direction.opposite())
        }
        Class::MossyCarpet => {
            let below = level.block(pos.below());
            if state.flag("bottom") {
                !below.is_air()
            } else {
                below.is(ids::PALE_MOSS_CARPET) && below.flag("bottom")
            }
        }
        Class::Multiface => multiface_can_survive(level, state, pos),
        Class::Path => {
            let above = level.block(pos.above());
            !is_solid(above) || class_of(above) == Class::FenceGate
        }
        Class::PistonHead => piston_head_can_survive(level, state, pos),
        Class::RedstoneWire => {
            redstone_wire_can_survive_on(level, pos.below(), level.block(pos.below()))
        }
        Class::Scaffolding => scaffolding_distance(level, pos) < 7,
        Class::SnowLayer => snow_layer_can_survive(level, pos),
        Class::Speleothem => match state.direction("vertical_direction") {
            Some(tip) => is_valid_speleothem_placement(level, state, pos, tip),
            None => true,
        },
        Class::SporeBlossom => {
            can_support_center(level, pos.above(), Direction::Down)
                && !tags().fluids.water.contains(level.fluid(pos))
        }
        Class::SugarCane => sugar_cane_can_survive(level, pos),
        Class::Vine => vine_has_faces(super::update::vine_updated_state(level, state, pos)),
        _ => true,
    }
}

/// The face of `pos` towards `direction` is sturdy (`isFaceSturdy` with `SupportType.FULL`).
#[inline]
fn sturdy_at(level: &impl BlockLevel, pos: BlockPos, direction: Direction) -> bool {
    face_sturdy_at(level, pos, level.block(pos), direction, SupportType::Full)
}

/// `VegetationBlock.canSurvive`: `mayPlaceOn` the block below.
fn vegetation_can_survive(
    level: &impl BlockLevel,
    class: Class,
    state: BlockState,
    pos: BlockPos,
) -> bool {
    let below_pos = pos.below();
    may_place_on(level, class, state, level.block(below_pos), below_pos)
}

/// `DoublePlantBlock.canSurvive`: the lower half like vegetation, the upper half on its lower
/// half.
fn double_plant_can_survive(
    level: &impl BlockLevel,
    class: Class,
    state: BlockState,
    pos: BlockPos,
) -> bool {
    if state.value("half") == Some("upper") {
        upper_half_on_lower(level, state, pos)
    } else {
        vegetation_can_survive(level, class, state, pos)
    }
}

/// The block below is the lower half of the same double block.
fn upper_half_on_lower(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> bool {
    let below = level.block(pos.below());
    below.block() == state.block() && below.value("half") == Some("lower")
}

/// `VegetationBlock.mayPlaceOn(below, level, belowPos)` with the overrides of its subclasses.
pub(crate) fn may_place_on(
    level: &impl BlockLevel,
    class: Class,
    state: BlockState,
    below: BlockState,
    below_pos: BlockPos,
) -> bool {
    let t = &tags().blocks;
    match class {
        Class::Azalea => t.supports_azalea.contains(below),
        Class::AttachedStem | Class::Stem => {
            if state.is(ids::ATTACHED_MELON_STEM) || state.is(ids::MELON_STEM) {
                t.supports_melon_stem.contains(below)
            } else {
                t.supports_pumpkin_stem.contains(below)
            }
        }
        Class::CactusFlower => {
            t.support_override_cactus_flower.contains(below)
                || is_face_sturdy(below, Direction::Up, SupportType::Center)
        }
        Class::Crop | Class::PitcherCrop => t.supports_crops.contains(below),
        Class::DryVegetation => t.supports_dry_vegetation.contains(below),
        Class::LilyPad => {
            (tags()
                .fluids
                .supports_lily_pad
                .contains(level.fluid(below_pos))
                || t.supports_lily_pad.contains(below))
                && level.fluid(below_pos.above()).is_empty()
        }
        Class::MangrovePropagule => t.supports_mangrove_propagule.contains(below),
        Class::Mushroom => super::is_solid_render(below),
        Class::NetherFungus => {
            if state.is(ids::CRIMSON_FUNGUS) {
                t.supports_crimson_fungus.contains(below)
            } else {
                t.supports_warped_fungus.contains(below)
            }
        }
        Class::NetherRoots => {
            if state.is(ids::CRIMSON_ROOTS) {
                t.supports_crimson_roots.contains(below)
            } else {
                t.supports_warped_roots.contains(below)
            }
        }
        Class::NetherSprouts => t.supports_nether_sprouts.contains(below),
        Class::NetherWart => t.supports_nether_wart.contains(below),
        Class::SeaPickle => {
            !is_face_empty(below.collision_shape(), Direction::Up)
                || is_face_sturdy(below, Direction::Up, SupportType::Full)
        }
        Class::Seagrass | Class::TallSeagrass => {
            is_face_sturdy(below, Direction::Up, SupportType::Full)
                && !t.cannot_support_seagrass.contains(below)
        }
        Class::SmallDripleaf => {
            t.supports_small_dripleaf.contains(below)
                || is_water_source(level.fluid(below_pos.above()))
                    && t.supports_vegetation.contains(below)
        }
        Class::WitherRose => t.supports_wither_rose.contains(below),
        _ => t.supports_vegetation.contains(below),
    }
}

/// `fluidState.isSourceOfType(Fluids.WATER)`.
pub(crate) fn is_water_source(fluid: vanillite_data::fluid::FluidState) -> bool {
    fluid.block_state().is(ids::WATER) && fluid.info().source
}

/// `FaceAttachedHorizontalDirectionalBlock.canAttach(level, pos, direction)`.
pub(crate) fn can_attach(level: &impl BlockLevel, pos: BlockPos, direction: Direction) -> bool {
    sturdy_at(level, pos.relative(direction), direction.opposite())
}

/// `FaceAttachedHorizontalDirectionalBlock.getConnectedDirection(state)`.
pub(crate) fn face_connected_direction(state: BlockState) -> Direction {
    match state.value("face") {
        Some("ceiling") => Direction::Down,
        Some("floor") => Direction::Up,
        _ => state.direction("facing").unwrap_or(Direction::North),
    }
}

/// `BellBlock.getConnectedDirection(state)`.
pub(crate) fn bell_connected_direction(state: BlockState) -> Direction {
    match state.value("attachment") {
        Some("floor") => Direction::Up,
        Some("ceiling") => Direction::Down,
        _ => state
            .direction("facing")
            .unwrap_or(Direction::North)
            .opposite(),
    }
}

/// `LanternBlock.getConnectedDirection(state)`.
pub(crate) fn lantern_connected_direction(state: BlockState) -> Direction {
    if state.flag("hanging") {
        Direction::Down
    } else {
        Direction::Up
    }
}

/// `DiodeBlock.canSurviveOn`: the block `below` at `below_pos` is rigid at its top.
pub(crate) fn diode_can_survive_on(
    level: &impl BlockLevel,
    below_pos: BlockPos,
    below: BlockState,
) -> bool {
    face_sturdy_at(level, below_pos, below, Direction::Up, SupportType::Rigid)
}

/// `RedstoneWireBlock.canSurviveOn` on the block `below` at `below_pos`.
pub fn redstone_wire_can_survive_on(
    level: &impl BlockLevel,
    below_pos: BlockPos,
    below: BlockState,
) -> bool {
    face_sturdy_at(level, below_pos, below, Direction::Up, SupportType::Full)
        || below.is(ids::HOPPER)
}

/// `CactusBlock.canSurvive`.
fn cactus_can_survive(level: &impl BlockLevel, pos: BlockPos) -> bool {
    for direction in HORIZONTAL {
        let side = pos.relative(direction);
        if is_solid(level.block(side)) || tags().fluids.lava.contains(level.fluid(side)) {
            return false;
        }
    }
    let below = level.block(pos.below());
    (below.is(ids::CACTUS) || tags().blocks.supports_cactus.contains(below))
        && !level
            .block(pos.above())
            .has(vanillite_data::block::flags::LIQUID)
}

/// `ChorusFlowerBlock.canSurvive`.
fn chorus_flower_can_survive(level: &impl BlockLevel, pos: BlockPos) -> bool {
    let below = level.block(pos.below());
    if below.is(ids::CHORUS_PLANT) || tags().blocks.supports_chorus_flower.contains(below) {
        return true;
    }
    if !below.is_air() {
        return false;
    }
    let mut one_neighbour = false;
    for direction in HORIZONTAL {
        let neighbour = level.block(pos.relative(direction));
        if neighbour.is(ids::CHORUS_PLANT) {
            if one_neighbour {
                return false;
            }
            one_neighbour = true;
        } else if !neighbour.is_air() {
            return false;
        }
    }
    one_neighbour
}

/// `ChorusPlantBlock.canSurvive`.
fn chorus_plant_can_survive(level: &impl BlockLevel, pos: BlockPos) -> bool {
    let supports = &tags().blocks.supports_chorus_plant;
    let below = level.block(pos.below());
    let above_or_below = !level.block(pos.above()).is_air() && !below.is_air();
    for direction in HORIZONTAL {
        let neighbour_pos = pos.relative(direction);
        if level.block(neighbour_pos).is(ids::CHORUS_PLANT) {
            if above_or_below {
                return false;
            }
            let under = level.block(neighbour_pos.below());
            if under.is(ids::CHORUS_PLANT) || supports.contains(under) {
                return true;
            }
        }
    }
    below.is(ids::CHORUS_PLANT) || supports.contains(below)
}

/// `FireBlock.isValidFireLocation`: a flammable block next to the position.
fn is_valid_fire_location(level: &impl BlockLevel, pos: BlockPos) -> bool {
    Direction::ALL
        .into_iter()
        .any(|d| can_burn(level.block(pos.relative(d))))
}

/// `GrowingPlantBlock` head and body blocks: `(growth direction, head, body)`.
pub(crate) fn growing_plant(
    class: Class,
) -> Option<(
    Direction,
    vanillite_data::block::BlockId,
    vanillite_data::block::BlockId,
)> {
    Some(match class {
        Class::Kelp | Class::KelpPlant => (Direction::Up, ids::KELP, ids::KELP_PLANT),
        Class::CaveVines | Class::CaveVinesPlant => {
            (Direction::Down, ids::CAVE_VINES, ids::CAVE_VINES_PLANT)
        }
        Class::WeepingVines | Class::WeepingVinesPlant => (
            Direction::Down,
            ids::WEEPING_VINES,
            ids::WEEPING_VINES_PLANT,
        ),
        Class::TwistingVines | Class::TwistingVinesPlant => (
            Direction::Up,
            ids::TWISTING_VINES,
            ids::TWISTING_VINES_PLANT,
        ),
        _ => return None,
    })
}

/// `GrowingPlantBlock.canSurvive`.
fn growing_plant_can_survive(level: &impl BlockLevel, class: Class, pos: BlockPos) -> bool {
    let Some((growth, head, body)) = growing_plant(class) else {
        return true;
    };
    let attached_pos = pos.relative(growth.opposite());
    let attached = level.block(attached_pos);
    // `KelpBlock.canAttachTo` (and the kelp plant's copy); others attach to anything.
    let can_attach = !matches!(class, Class::Kelp | Class::KelpPlant)
        || !tags().blocks.cannot_support_kelp.contains(attached);
    can_attach
        && (attached.is(head)
            || attached.is(body)
            || face_sturdy_at(level, attached_pos, attached, growth, SupportType::Full))
}

/// `HangingMossBlock.canStayAtPosition`.
pub(crate) fn hanging_moss_can_stay(level: &impl BlockLevel, pos: BlockPos) -> bool {
    let above = level.block(pos.above());
    can_attach_multiface(above, Direction::Up) || above.is(ids::PALE_HANGING_MOSS)
}

/// The face properties of `MultifaceBlock` (`PipeBlock.PROPERTY_BY_DIRECTION` names).
pub(crate) fn multiface_has_face(state: BlockState, direction: Direction) -> bool {
    state.flag(super::props::direction_name(direction))
}

/// `MultifaceBlock.canSurvive`.
fn multiface_can_survive(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> bool {
    let mut any = false;
    for direction in Direction::ALL {
        if multiface_has_face(state, direction) {
            if !can_attach_multiface(level.block(pos.relative(direction)), direction) {
                return false;
            }
            any = true;
        }
    }
    any
}

/// `PistonHeadBlock.canSurvive`.
fn piston_head_can_survive(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> bool {
    let Some(facing) = state.direction("facing") else {
        return true;
    };
    let base = level.block(pos.relative(facing.opposite()));
    let base_block = if state.value("type") == Some("sticky") {
        ids::STICKY_PISTON
    } else {
        ids::PISTON
    };
    let fitting =
        base.is(base_block) && base.flag("extended") && base.direction("facing") == Some(facing);
    fitting || base.is(ids::MOVING_PISTON) && base.direction("facing") == Some(facing)
}

/// `ScaffoldingBlock.getDistance`.
pub fn scaffolding_distance(level: &impl BlockLevel, pos: BlockPos) -> i32 {
    let below_pos = pos.below();
    let below = level.block(below_pos);
    let mut distance = 7;
    if below.is(ids::SCAFFOLDING) {
        distance = below.int("distance").unwrap_or(7);
    } else if face_sturdy_at(level, below_pos, below, Direction::Up, SupportType::Full) {
        return 0;
    }
    for direction in HORIZONTAL {
        let side = level.block(pos.relative(direction));
        if side.is(ids::SCAFFOLDING) {
            distance = distance.min(side.int("distance").unwrap_or(7) + 1);
            if distance == 1 {
                break;
            }
        }
    }
    distance
}

/// `SnowLayerBlock.canSurvive`.
fn snow_layer_can_survive(level: &impl BlockLevel, pos: BlockPos) -> bool {
    let t = &tags().blocks;
    let below = level.block(pos.below());
    if t.cannot_support_snow_layer.contains(below) {
        return false;
    }
    t.support_override_snow_layer.contains(below)
        || is_face_full(below.collision_shape(), Direction::Up)
        || below.is(ids::SNOW) && below.int("layers") == Some(8)
}

/// `SpeleothemBlock.isSpeleothemWithDirection`.
pub(crate) fn is_speleothem_with_direction(state: BlockState, tip: Direction) -> bool {
    tags().blocks.speleothems.contains(state) && state.direction("vertical_direction") == Some(tip)
}

/// `SpeleothemBlock.isValidSpeleothemPlacement`.
fn is_valid_speleothem_placement(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    tip: Direction,
) -> bool {
    let behind_pos = pos.relative(tip.opposite());
    let behind = level.block(behind_pos);
    face_sturdy_at(level, behind_pos, behind, tip, SupportType::Full)
        || is_speleothem_with_direction(behind, tip) && behind.block() == state.block()
}

/// `SugarCaneBlock.canSurvive`.
fn sugar_cane_can_survive(level: &impl BlockLevel, pos: BlockPos) -> bool {
    let below = level.block(pos.below());
    if below.is(ids::SUGAR_CANE) {
        return true;
    }
    let t = &tags().blocks;
    if t.supports_sugar_cane.contains(below) {
        let below_pos = pos.below();
        for direction in HORIZONTAL {
            let side = below_pos.relative(direction);
            if tags()
                .fluids
                .supports_sugar_cane_adjacently
                .contains(level.fluid(side))
                || t.supports_sugar_cane_adjacently.contains(level.block(side))
            {
                return true;
            }
        }
    }
    false
}

/// `VineBlock.hasFaces`.
pub(crate) fn vine_has_faces(state: BlockState) -> bool {
    ["up", "north", "east", "south", "west"]
        .into_iter()
        .any(|p| state.flag(p))
}

/// `ChestBlock.getConnectedDirection(state)`.
pub(crate) fn chest_connected_direction(state: BlockState) -> Direction {
    let facing = state.direction("facing").unwrap_or(Direction::North);
    if state.value("type") == Some("left") {
        clockwise(facing)
    } else {
        counter_clockwise(facing)
    }
}
