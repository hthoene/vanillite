//! `BlockState.updateShape`: the `updateShape` overrides of every block class, with the ticks
//! they schedule going to the level ([`BlockLevel`]) and `level.getRandom()` as their random.
//!
//! Each class's chain of `super.updateShape` calls is spelled out; the base
//! (`BlockBehaviour.updateShape`) returns the state unchanged.

use vanillite_base::{BlockPos, Direction};
use vanillite_data::block::{BlockId, BlockState, INSTRUMENTS, flags, ids};

use super::BlockLevel;
use super::class::{Class, class_of};
use super::geometry::{Rect, face_covers};
use super::props::{
    Axis, HORIZONTAL, IntProperty, PosExt, StateExt, axis, clockwise, counter_clockwise, cycle,
    direction_name, is_horizontal, wire_properties, with_properties_of,
};
use super::survive::{
    bell_connected_direction, can_survive, chest_connected_direction, diode_can_survive_on,
    face_connected_direction, growing_plant, hanging_moss_can_stay, is_speleothem_with_direction,
    is_water_source, lantern_connected_direction, multiface_has_face, redstone_wire_can_survive_on,
    vine_has_faces,
};
use super::tables::{can_burn, tags};
use super::{SupportType, can_attach_multiface, face_sturdy_at, is_face_sturdy};

const AIR: BlockState = BlockState::AIR;
/// `Fluids.WATER` (`minecraft:fluid` id).
const FLUID_WATER: u16 = 2;

/// `state.updateShape(...)`.
pub(crate) fn update_shape(
    level: &mut impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    _neighbour_pos: BlockPos,
    neighbour: BlockState,
) -> BlockState {
    let class = class_of(state);
    match class {
        Class::Plain => state,

        // Classes whose only behaviour is `SimpleWaterloggedBlock`'s water tick.
        Class::Barrier
        | Class::Candle
        | Class::Chain
        | Class::Conduit
        | Class::CopperGolemStatue
        | Class::DecoratedPot
        | Class::DriedGhast
        | Class::EnderChest
        | Class::HeavyCore
        | Class::Light
        | Class::LightningRod
        | Class::MangroveRoots
        | Class::SculkSensor
        | Class::CalibratedSculkSensor
        | Class::SculkShrieker
        | Class::Shelf
        | Class::Slab
        | Class::TrapDoor
        | Class::WaterloggedTransparent
        | Class::Rail
        | Class::PoweredRail
        | Class::DetectorRail
        | Class::WallHangingSign => {
            water_tick_if_waterlogged(level, state, pos);
            state
        }

        // `VegetationBlock.updateShape`: plants that cannot survive break.
        Class::Vegetation
        | Class::Azalea
        | Class::CactusFlower
        | Class::Crop
        | Class::DryVegetation
        | Class::FlowerBed
        | Class::LeafLitter
        | Class::LilyPad
        | Class::Mushroom
        | Class::NetherFungus
        | Class::NetherRoots
        | Class::NetherSprouts
        | Class::NetherWart
        | Class::Stem
        | Class::WitherRose
        | Class::Carpet
        | Class::Frogspawn
        | Class::SnowLayer => break_unless_survives(level, state, pos),

        // Break when the block they hang on or stand on (in a fixed direction) goes away.
        Class::Banner
        | Class::BasePressurePlate
        | Class::BaseTorch
        | Class::Cake
        | Class::CandleCake => break_if(level, state, pos, dir == Direction::Down),
        Class::SporeBlossom => break_if(level, state, pos, dir == Direction::Up),
        Class::Cocoa => break_if(level, state, pos, Some(dir) == state.direction("facing")),
        Class::WallBanner | Class::ShelfMushroom => break_if(
            level,
            state,
            pos,
            Some(dir.opposite()) == state.direction("facing"),
        ),
        Class::WallTorch | Class::RedstoneWallTorch | Class::TripWireHook | Class::PistonHead => {
            break_if(
                level,
                state,
                pos,
                Some(dir.opposite()) == state.direction("facing"),
            )
        }
        Class::FaceAttachedHorizontalDirectional | Class::Grindstone => break_if(
            level,
            state,
            pos,
            face_connected_direction(state).opposite() == dir,
        ),
        Class::StandingSign => {
            if dir == Direction::Down && !can_survive(level, state, pos) {
                return AIR;
            }
            water_tick_if_waterlogged(level, state, pos);
            state
        }
        Class::WallSign => {
            if Some(dir.opposite()) == state.direction("facing") && !can_survive(level, state, pos)
            {
                return AIR;
            }
            water_tick_if_waterlogged(level, state, pos);
            state
        }
        Class::CeilingHangingSign => {
            if dir == Direction::Up && !can_survive(level, state, pos) {
                return AIR;
            }
            water_tick_if_waterlogged(level, state, pos);
            state
        }
        Class::AmethystCluster => {
            water_tick_if_waterlogged(level, state, pos);
            break_if(
                level,
                state,
                pos,
                Some(dir) == state.direction("facing").map(Direction::opposite),
            )
        }
        Class::Lantern => {
            water_tick_if_waterlogged(level, state, pos);
            break_if(
                level,
                state,
                pos,
                lantern_connected_direction(state).opposite() == dir,
            )
        }
        Class::HangingRoots => {
            if dir == Direction::Up && !can_survive(level, state, pos) {
                return AIR;
            }
            water_tick_if_waterlogged(level, state, pos);
            state
        }
        Class::Ladder => {
            if Some(dir.opposite()) == state.direction("facing") && !can_survive(level, state, pos)
            {
                return AIR;
            }
            water_tick_if_waterlogged(level, state, pos);
            state
        }
        Class::MangrovePropagule => {
            water_tick_if_waterlogged(level, state, pos);
            if dir == Direction::Up && !can_survive(level, state, pos) {
                return AIR;
            }
            break_unless_survives(level, state, pos)
        }
        Class::SeaPickle => {
            if !can_survive(level, state, pos) {
                return AIR;
            }
            water_tick_if_waterlogged(level, state, pos);
            state
        }
        Class::Seagrass => {
            let result = break_unless_survives(level, state, pos);
            if !result.is_air() {
                water_tick(level, pos);
            }
            result
        }

        // Blocks that schedule a block tick to break or fall later.
        Class::Falling | Class::Anvil | Class::Brushable | Class::CreakingHeart => {
            // `getDelayAfterPlace()` of falling blocks (5 for the dragon egg), 2 for brushable
            // blocks, 1 for the creaking heart.
            let delay = match class {
                Class::Falling | Class::Anvil if state.is(ids::DRAGON_EGG) => 5,
                Class::Falling | Class::Anvil | Class::Brushable => 2,
                _ => 1,
            };
            level.schedule_block_tick(pos, state.block(), delay);
            state
        }
        Class::Cactus | Class::SugarCane => {
            tick_unless_survives(level, state, pos);
            state
        }
        Class::ChorusFlower => {
            if dir != Direction::Up {
                tick_unless_survives(level, state, pos);
            }
            state
        }
        Class::Farmland | Class::Path => {
            if dir == Direction::Up {
                tick_unless_survives(level, state, pos);
            }
            state
        }
        Class::Scaffolding => {
            water_tick_if_waterlogged(level, state, pos);
            level.schedule_block_tick(pos, state.block(), 1);
            state
        }
        Class::ConcretePowder => match concrete_of(state) {
            Some(concrete) if touches_liquid(level, pos) => concrete.default_state(),
            _ => {
                level.schedule_block_tick(pos, state.block(), 2);
                state
            }
        },
        Class::Observer => {
            if Some(dir) == state.direction("facing")
                && !state.flag("powered")
                && !level.has_block_tick(pos, state.block())
            {
                level.schedule_block_tick(pos, state.block(), 2);
            }
            state
        }

        Class::AbstractBed => bed(state, dir, neighbour),
        Class::AttachedStem => {
            let (fruit, stem) = if state.is(ids::ATTACHED_MELON_STEM) {
                (ids::MELON, ids::MELON_STEM)
            } else {
                (ids::PUMPKIN, ids::PUMPKIN_STEM)
            };
            if !neighbour.is(fruit) && Some(dir) == state.direction("facing") {
                return stem.default_state().set_int("age", 7);
            }
            break_unless_survives(level, state, pos)
        }
        Class::BambooSapling => {
            if !can_survive(level, state, pos) {
                AIR
            } else if dir == Direction::Up && neighbour.is(ids::BAMBOO) {
                ids::BAMBOO.default_state()
            } else {
                state
            }
        }
        Class::BambooStalk => {
            tick_unless_survives(level, state, pos);
            if dir == Direction::Up
                && neighbour.is(ids::BAMBOO)
                && neighbour.int("age") > state.int("age")
            {
                cycle(state, "age")
            } else {
                state
            }
        }
        Class::BaseCoralPlantType => {
            water_tick_if_waterlogged(level, state, pos);
            break_if(level, state, pos, dir == Direction::Down)
        }
        Class::BaseCoralWallFan => {
            water_tick_if_waterlogged(level, state, pos);
            break_if(
                level,
                state,
                pos,
                Some(dir.opposite()) == state.direction("facing"),
            )
        }
        Class::CoralPlant | Class::CoralFan => {
            if dir == Direction::Down && !can_survive(level, state, pos) {
                return AIR;
            }
            try_schedule_coral_die_tick(level, state, pos);
            water_tick_if_waterlogged(level, state, pos);
            // `BaseCoralPlantTypeBlock.updateShape`.
            water_tick_if_waterlogged(level, state, pos);
            break_if(level, state, pos, dir == Direction::Down)
        }
        Class::CoralWallFan => {
            let facing = state.direction("facing");
            if Some(dir.opposite()) == facing && !can_survive(level, state, pos) {
                return AIR;
            }
            water_tick_if_waterlogged(level, state, pos);
            try_schedule_coral_die_tick(level, state, pos);
            // `BaseCoralWallFanBlock.updateShape`.
            water_tick_if_waterlogged(level, state, pos);
            break_if(level, state, pos, Some(dir.opposite()) == facing)
        }
        Class::Coral => {
            if !Direction::ALL
                .into_iter()
                .any(|d| tags().fluids.water.contains(level.fluid(pos.relative(d))))
            {
                schedule_coral_die_tick(level, state, pos);
            }
            state
        }
        Class::Bell => bell(level, state, pos, dir, neighbour),
        Class::BigDripleaf => {
            if dir == Direction::Down && !can_survive(level, state, pos) {
                return AIR;
            }
            water_tick_if_waterlogged(level, state, pos);
            if dir == Direction::Up && neighbour.is(ids::BIG_DRIPLEAF) {
                with_properties_of(ids::BIG_DRIPLEAF_STEM, state)
            } else {
                state
            }
        }
        Class::BigDripleafStem => {
            if matches!(dir, Direction::Down | Direction::Up) {
                tick_unless_survives(level, state, pos);
            }
            water_tick_if_waterlogged(level, state, pos);
            state
        }
        Class::BubbleColumn => {
            water_tick(level, pos);
            if !can_survive(level, state, pos)
                || dir == Direction::Down
                || dir == Direction::Up
                    && !neighbour.is(ids::BUBBLE_COLUMN)
                    && bubble_column_can_occupy(neighbour)
            {
                level.schedule_block_tick(pos, state.block(), 5);
            }
            state
        }
        Class::Campfire => {
            water_tick_if_waterlogged(level, state, pos);
            if dir == Direction::Down {
                state.set_flag("signal_fire", neighbour.is(ids::HAY_BLOCK))
            } else {
                state
            }
        }
        Class::Chest => chest(level, state, pos, dir, neighbour),
        Class::CopperChest => {
            let result = chest(level, state, pos, dir, neighbour);
            if chest_can_connect_to(state, neighbour)
                && result.value("type") != Some("single")
                && chest_connected_direction(result) == dir
            {
                return with_properties_of(neighbour.block(), result);
            }
            result
        }
        Class::ChorusPlant => {
            if !can_survive(level, state, pos) {
                level.schedule_block_tick(pos, state.block(), 1);
                return state;
            }
            let connect = neighbour.is(ids::CHORUS_PLANT)
                || neighbour.is(ids::CHORUS_FLOWER)
                || dir == Direction::Down
                    && tags().blocks.supports_chorus_plant.contains(neighbour);
            state.set_flag(direction_name(dir), connect)
        }
        Class::Comparator => {
            if dir == Direction::Down && !diode_can_survive_on(level, pos.below(), neighbour) {
                AIR
            } else {
                state
            }
        }
        Class::Repeater => {
            let facing = state.direction("facing").unwrap_or(Direction::North);
            if dir == Direction::Down && !diode_can_survive_on(level, pos.below(), neighbour) {
                AIR
            } else if axis(dir) != axis(facing) {
                state.set_flag("locked", repeater_alternate_signal(level, pos, facing) > 0)
            } else {
                state
            }
        }
        Class::Door => door(level, state, pos, dir, neighbour),
        Class::DoublePlant | Class::TallSeagrass => double_plant(level, state, pos, dir, neighbour),
        Class::SmallDripleaf => {
            water_tick_if_waterlogged(level, state, pos);
            double_plant(level, state, pos, dir, neighbour)
        }
        Class::PitcherCrop => {
            if state.int("age").is_some_and(|age| age >= 3) {
                double_plant(level, state, pos, dir, neighbour)
            } else {
                break_unless_survives(level, state, pos)
            }
        }
        Class::Fence => {
            water_tick_if_waterlogged(level, state, pos);
            if !is_horizontal(dir) {
                return state;
            }
            let face = dir.opposite();
            let solid =
                face_sturdy_at(level, pos.relative(dir), neighbour, face, SupportType::Full);
            state.set_flag(
                direction_name(dir),
                fence_connects_to(state, neighbour, solid, face),
            )
        }
        Class::IronBars => {
            water_tick_if_waterlogged(level, state, pos);
            if !is_horizontal(dir) {
                return state;
            }
            let solid = face_sturdy_at(
                level,
                pos.relative(dir),
                neighbour,
                dir.opposite(),
                SupportType::Full,
            );
            let attaches = !is_exception_for_connection(neighbour) && solid
                || class_of(neighbour) == Class::IronBars
                || tags().blocks.walls.contains(neighbour);
            state.set_flag(direction_name(dir), attaches)
        }
        Class::FenceGate => {
            let facing = state.direction("facing").unwrap_or(Direction::North);
            if axis(clockwise(facing)) != axis(dir) {
                return state;
            }
            let walls = &tags().blocks.walls;
            let in_wall = walls.contains(neighbour)
                || walls.contains(level.block(pos.relative(dir.opposite())));
            state.set_flag("in_wall", in_wall)
        }
        Class::Wall => wall(level, state, pos, dir, neighbour),
        Class::Stair => {
            water_tick_if_waterlogged(level, state, pos);
            if is_horizontal(dir) {
                state.set("shape", stairs_shape(level, state, pos))
            } else {
                state
            }
        }
        Class::NetherPortal => {
            // `NetherPortalBlock.updateShape`: a portal block without a complete frame breaks.
            let portal_axis = super::HorizontalAxis::of(state).unwrap_or(super::HorizontalAxis::X);
            let wrong_axis = super::HorizontalAxis::of_direction(dir)
                .is_some_and(|update_axis| update_axis != portal_axis);
            if !wrong_axis
                && !neighbour.is(ids::NETHER_PORTAL)
                && !super::PortalShape::find_any(&*level, pos, portal_axis).is_complete()
            {
                AIR
            } else {
                state
            }
        }
        Class::Fire => {
            if can_survive(level, state, pos) {
                fire_state_with_age(level, pos, state.int("age").unwrap_or(0))
            } else {
                AIR
            }
        }
        Class::SoulFire => {
            if can_survive(level, state, pos) {
                state.block().default_state()
            } else {
                AIR
            }
        }
        Class::Kelp | Class::CaveVines | Class::WeepingVines | Class::TwistingVines => {
            growing_plant_head(level, class, state, pos, dir, neighbour)
        }
        Class::KelpPlant
        | Class::CaveVinesPlant
        | Class::WeepingVinesPlant
        | Class::TwistingVinesPlant => growing_plant_body(level, class, state, pos, dir, neighbour),
        Class::HangingMoss => {
            if !hanging_moss_can_stay(level, pos) {
                level.schedule_block_tick(pos, state.block(), 1);
            }
            let below = level.block(pos.below());
            state.set_flag("tip", !below.is(state.block()))
        }
        Class::HugeMushroom => {
            if neighbour.block() == state.block() {
                state.set_flag(direction_name(dir), false)
            } else {
                state
            }
        }
        Class::Leaves => {
            water_tick_if_waterlogged(level, state, pos);
            let distance = leaves_distance_at(neighbour) + 1;
            if distance != 1 || IntProperty::distance().get(state) != Some(distance) {
                level.schedule_block_tick(pos, state.block(), 1);
            }
            state
        }
        Class::Liquid => {
            let fluid = state.fluid_state();
            if fluid.info().source || neighbour.fluid_state().info().source {
                level.schedule_fluid_tick(pos, fluid.info().fluid);
            }
            if dir == Direction::Down
                && tags().fluids.bubble_column_can_occupy.contains(fluid)
                && fluid.info().source
                && fluid.info().amount == 8
            {
                let t = &tags().blocks;
                if t.enables_bubble_column_drag_down.contains(neighbour)
                    || t.enables_bubble_column_push_up.contains(neighbour)
                {
                    // `LiquidBlock.BUBBLE_COLUMN_CHECK_DELAY`.
                    level.schedule_block_tick(pos, state.block(), 20);
                }
            }
            state
        }
        Class::MossyCarpet => {
            if !can_survive(level, state, pos) {
                return AIR;
            }
            let updated = mossy_carpet_updated_state(level, state, pos);
            let has_faces = updated.flag("bottom")
                || HORIZONTAL
                    .into_iter()
                    .any(|d| updated.value(direction_name(d)) != Some("none"));
            if has_faces { updated } else { AIR }
        }
        Class::Multiface => {
            water_tick_if_waterlogged(level, state, pos);
            if !Direction::ALL
                .into_iter()
                .any(|d| multiface_has_face(state, d))
            {
                return AIR;
            }
            if multiface_has_face(state, dir) && !can_attach_multiface(neighbour, dir) {
                let removed = state.set_flag(direction_name(dir), false);
                let any = Direction::ALL
                    .into_iter()
                    .any(|d| multiface_has_face(removed, d));
                return if any { removed } else { AIR };
            }
            state
        }
        Class::NoteBlock => {
            if axis(dir) == Axis::Y {
                note_block_instrument(level, state, pos)
            } else {
                state
            }
        }
        Class::PotentSulfur => potent_sulfur_state(level, state, pos),
        Class::RedstoneWire => redstone_wire(level, state, pos, dir, neighbour),
        Class::Snowy => {
            if dir == Direction::Up {
                state.set_flag("snowy", tags().blocks.snow.contains(neighbour))
            } else {
                state
            }
        }
        Class::Speleothem => speleothem(level, state, pos, dir),
        Class::TripWire => {
            if !is_horizontal(dir) {
                return state;
            }
            let connect = if neighbour.is(ids::TRIPWIRE_HOOK) {
                neighbour.direction("facing") == Some(dir.opposite())
            } else {
                neighbour.is(ids::TRIPWIRE)
            };
            state.set_flag(direction_name(dir), connect)
        }
        Class::Vine => {
            if dir == Direction::Down {
                return state;
            }
            let updated = vine_updated_state(level, state, pos);
            if vine_has_faces(updated) {
                updated
            } else {
                AIR
            }
        }

        // No `updateShape` override (only `rotate`/`mirror`), or not ported (see the module
        // documentation).
        Class::AbstractFurnace
        | Class::Barrel
        | Class::Beehive
        | Class::ChiseledBookShelf
        | Class::Command
        | Class::Crafter
        | Class::Dispenser
        | Class::EndPortalFrame
        | Class::FlowerPot
        | Class::Hopper
        | Class::HorizontalDirectional
        | Class::InfestedRotatedPillar
        | Class::Jigsaw
        | Class::Lectern
        | Class::MovingPiston
        | Class::PistonBase
        | Class::Rod
        | Class::RotatedPillar
        | Class::ShulkerBox
        | Class::Skull
        | Class::Stonecutter
        | Class::Vault
        | Class::WallSkull => state,
    }
}

/// `ticks.scheduleTick(pos, Fluids.WATER, ...)`.
fn water_tick(level: &mut impl BlockLevel, pos: BlockPos) {
    level.schedule_fluid_tick(pos, FLUID_WATER);
}

/// The water tick of `SimpleWaterloggedBlock`s: `if (state.getValue(WATERLOGGED))`.
fn water_tick_if_waterlogged(level: &mut impl BlockLevel, state: BlockState, pos: BlockPos) {
    if state.waterlogged() {
        water_tick(level, pos);
    }
}

/// `!state.canSurvive(level, pos) ? AIR : state`.
fn break_unless_survives(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> BlockState {
    if can_survive(level, state, pos) {
        state
    } else {
        AIR
    }
}

/// `condition && !state.canSurvive(level, pos) ? AIR : state`.
fn break_if(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    condition: bool,
) -> BlockState {
    if condition && !can_survive(level, state, pos) {
        AIR
    } else {
        state
    }
}

/// `if (!state.canSurvive(level, pos)) ticks.scheduleTick(pos, this, 1)`.
fn tick_unless_survives(level: &mut impl BlockLevel, state: BlockState, pos: BlockPos) {
    if !can_survive(level, state, pos) {
        level.schedule_block_tick(pos, state.block(), 1);
    }
}

/// `scheduleTick(pos, this, 60 + random.nextInt(40))` of corals.
fn schedule_coral_die_tick(level: &mut impl BlockLevel, state: BlockState, pos: BlockPos) {
    let delay = 60 + level.random_int(40);
    level.schedule_block_tick(pos, state.block(), delay);
}

/// `BaseCoralPlantTypeBlock.tryScheduleDieTick`.
fn try_schedule_coral_die_tick(level: &mut impl BlockLevel, state: BlockState, pos: BlockPos) {
    let has_water = state.waterlogged()
        || Direction::ALL
            .into_iter()
            .any(|d| tags().fluids.water.contains(level.fluid(pos.relative(d))));
    if !has_water {
        schedule_coral_die_tick(level, state, pos);
    }
}

/// `AbstractBedBlock.updateShape`.
fn bed(state: BlockState, dir: Direction, neighbour: BlockState) -> BlockState {
    let part = state.value("part");
    let facing = state.direction("facing").unwrap_or(Direction::North);
    let towards_other_half = if part == Some("foot") {
        facing
    } else {
        facing.opposite()
    };
    if dir != towards_other_half {
        return state;
    }
    if neighbour.block() == state.block() && neighbour.value("part") != part {
        state.set_flag("occupied", neighbour.flag("occupied"))
    } else {
        AIR
    }
}

/// `BellBlock.updateShape`.
fn bell(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    neighbour: BlockState,
) -> BlockState {
    let attachment = state.value("attachment");
    let connected = bell_connected_direction(state).opposite();
    if connected == dir && !can_survive(level, state, pos) && attachment != Some("double_wall") {
        return AIR;
    }
    let facing = state.direction("facing").unwrap_or(Direction::North);
    if axis(dir) == axis(facing) {
        if attachment == Some("double_wall") && !is_face_sturdy(neighbour, dir, SupportType::Full) {
            return state
                .set("attachment", "single_wall")
                .set_direction("facing", dir.opposite());
        }
        if attachment == Some("single_wall")
            && connected.opposite() == dir
            && is_face_sturdy(neighbour, facing, SupportType::Full)
        {
            return state.set("attachment", "double_wall");
        }
    }
    state
}

/// `ChestBlock.chestCanConnectTo` (and `CopperChestBlock`'s override).
fn chest_can_connect_to(state: BlockState, other: BlockState) -> bool {
    if class_of(state) == Class::CopperChest {
        tags().blocks.copper_chests.contains(other) && other.value("type").is_some()
    } else {
        other.block() == state.block()
    }
}

/// `ChestBlock.updateShape`.
fn chest(
    level: &mut impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    neighbour: BlockState,
) -> BlockState {
    water_tick_if_waterlogged(level, state, pos);
    if chest_can_connect_to(state, neighbour) && is_horizontal(dir) {
        let neighbour_type = neighbour.value("type");
        if state.value("type") == Some("single")
            && neighbour_type != Some("single")
            && state.direction("facing") == neighbour.direction("facing")
            && chest_connected_direction(neighbour) == dir.opposite()
        {
            let opposite = match neighbour_type {
                Some("left") => "right",
                Some("right") => "left",
                _ => "single",
            };
            return state.set("type", opposite);
        }
    } else if chest_connected_direction(state) == dir {
        return state.set("type", "single");
    }
    state
}

/// `DoorBlock.updateShape`.
fn door(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    neighbour: BlockState,
) -> BlockState {
    let half = state.value("half").unwrap_or("lower");
    let lower = half == "lower";
    if axis(dir) != Axis::Y || lower != (dir == Direction::Up) {
        break_if(level, state, pos, lower && dir == Direction::Down)
    } else if class_of(neighbour) == Class::Door && neighbour.value("half") != Some(half) {
        neighbour.set("half", half)
    } else {
        AIR
    }
}

/// `DoublePlantBlock.updateShape`.
fn double_plant(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    neighbour: BlockState,
) -> BlockState {
    let half = state.value("half");
    let lower = half == Some("lower");
    if axis(dir) != Axis::Y
        || lower != (dir == Direction::Up)
        || neighbour.block() == state.block() && neighbour.value("half") != half
    {
        if lower && dir == Direction::Down && !can_survive(level, state, pos) {
            AIR
        } else {
            break_unless_survives(level, state, pos)
        }
    } else {
        AIR
    }
}

/// `Block.isExceptionForConnection`.
fn is_exception_for_connection(state: BlockState) -> bool {
    let block = state.block();
    class_of(state) == Class::Leaves
        || block == ids::BARRIER
        || block == ids::CARVED_PUMPKIN
        || block == ids::JACK_O_LANTERN
        || block == ids::MELON
        || block == ids::PUMPKIN
        || tags().blocks.shulker_boxes.contains(state)
}

/// `FenceGateBlock.connectsToDirection(state, direction)`.
fn fence_gate_connects_to(state: BlockState, direction: Direction) -> bool {
    class_of(state) == Class::FenceGate
        && state
            .direction("facing")
            .is_some_and(|facing| axis(facing) == axis(clockwise(direction)))
}

/// `FenceBlock.connectsTo(neighbour, faceSolid, direction)`.
fn fence_connects_to(
    fence: BlockState,
    neighbour: BlockState,
    face_solid: bool,
    direction: Direction,
) -> bool {
    let t = &tags().blocks;
    let same_fence = t.fences.contains(neighbour)
        && t.wooden_fences.contains(neighbour) == t.wooden_fences.contains(fence);
    !is_exception_for_connection(neighbour) && face_solid
        || same_fence
        || fence_gate_connects_to(neighbour, direction)
}

/// `WallBlock.connectsTo(neighbour, faceSolid, direction)`.
fn wall_connects_to(neighbour: BlockState, face_solid: bool, direction: Direction) -> bool {
    tags().blocks.walls.contains(neighbour)
        || !is_exception_for_connection(neighbour) && face_solid
        || class_of(neighbour) == Class::IronBars
        || fence_gate_connects_to(neighbour, direction)
}

/// `WallBlock.TEST_SHAPE_POST` in the plane of the face below the block above.
const WALL_POST: Rect = [7.0 / 16.0, 7.0 / 16.0, 9.0 / 16.0, 9.0 / 16.0];

/// `WallBlock.TEST_SHAPES_WALL` for north, east, south and west.
const WALL_SIDES: [Rect; 4] = [
    [7.0 / 16.0, 0.0, 9.0 / 16.0, 9.0 / 16.0],
    [7.0 / 16.0, 7.0 / 16.0, 1.0, 9.0 / 16.0],
    [7.0 / 16.0, 7.0 / 16.0, 9.0 / 16.0, 1.0],
    [0.0, 7.0 / 16.0, 9.0 / 16.0, 9.0 / 16.0],
];

/// `WallBlock.updateShape`.
fn wall(
    level: &mut impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    neighbour: BlockState,
) -> BlockState {
    water_tick_if_waterlogged(level, state, pos);
    let connected = |d: Direction| state.value(direction_name(d)) != Some("none");
    match dir {
        Direction::Down => state,
        Direction::Up => {
            let sides = HORIZONTAL.map(connected);
            wall_shape(state, neighbour, sides)
        }
        _ => {
            let opposite = dir.opposite();
            let sides = HORIZONTAL.map(|d| {
                if d == dir {
                    wall_connects_to(
                        neighbour,
                        is_face_sturdy(neighbour, opposite, SupportType::Full),
                        opposite,
                    )
                } else {
                    connected(d)
                }
            });
            let above = level.block(pos.above());
            wall_shape(state, above, sides)
        }
    }
}

/// `WallBlock.updateShape(level, state, topPos, topNeighbour, north, east, south, west)`.
fn wall_shape(state: BlockState, top: BlockState, sides: [bool; 4]) -> BlockState {
    let above = top.collision_shape();
    let covered = |test: Rect| face_covers(above, Direction::Down, test);
    let mut result = state;
    for (i, direction) in HORIZONTAL.into_iter().enumerate() {
        let side = if !sides[i] {
            "none"
        } else if covered(WALL_SIDES[i]) {
            "tall"
        } else {
            "low"
        };
        result = result.set(direction_name(direction), side);
    }
    // `shouldRaisePost`.
    let raise = if class_of(top) == Class::Wall && top.flag("up") {
        true
    } else {
        let [north, east, south, west] = HORIZONTAL.map(|d| result.value(direction_name(d)));
        let none = |v: Option<&str>| v == Some("none");
        let corner = none(north) && none(south) && none(west) && none(east)
            || none(north) != none(south)
            || none(west) != none(east);
        if corner {
            true
        } else {
            let tall = |v: Option<&str>| v == Some("tall");
            let high_wall = tall(north) && tall(south) || tall(east) && tall(west);
            !high_wall && (tags().blocks.wall_post_override.contains(top) || covered(WALL_POST))
        }
    };
    result.set_flag("up", raise)
}

/// `StairBlock.getStairsShape`.
fn stairs_shape(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> &'static str {
    let Some(facing) = state.direction("facing") else {
        return "straight";
    };
    let half = state.value("half");
    let is_stairs = |s: BlockState| class_of(s) == Class::Stair;
    // `canTakeShape`.
    let can_take_shape = |side: Direction| {
        let other = level.block(pos.relative(side));
        !is_stairs(other)
            || other.direction("facing") != Some(facing)
            || other.value("half") != half
    };
    let behind = level.block(pos.relative(facing));
    if is_stairs(behind)
        && behind.value("half") == half
        && let Some(behind_facing) = behind.direction("facing")
        && axis(behind_facing) != axis(facing)
        && can_take_shape(behind_facing.opposite())
    {
        return if behind_facing == counter_clockwise(facing) {
            "outer_left"
        } else {
            "outer_right"
        };
    }

    let front = level.block(pos.relative(facing.opposite()));
    if is_stairs(front)
        && front.value("half") == half
        && let Some(front_facing) = front.direction("facing")
        && axis(front_facing) != axis(facing)
        && can_take_shape(front_facing)
    {
        return if front_facing == counter_clockwise(facing) {
            "inner_left"
        } else {
            "inner_right"
        };
    }

    "straight"
}

/// `BubbleColumnBlock.canOccupy(bubbleColumn, state)` for a state other than a bubble column.
fn bubble_column_can_occupy(state: BlockState) -> bool {
    let fluid = state.fluid_state();
    tags().fluids.bubble_column_can_occupy.contains(fluid)
        && class_of(state) == Class::Liquid
        && fluid.info().source
        && fluid.info().amount >= 8
}

/// `ConcretePowderBlock.concrete`: the concrete a powder hardens to.
pub fn concrete_of(state: BlockState) -> Option<BlockId> {
    BlockId::by_name(state.block().name().strip_suffix("_powder")?)
}

/// `ConcretePowderBlock.touchesLiquid`, including its reuse of the last tested position.
pub fn touches_liquid(level: &impl BlockLevel, pos: BlockPos) -> bool {
    let can_solidify = |s: BlockState| tags().fluids.water.contains(s.fluid_state());
    let mut test = pos;
    for direction in Direction::ALL {
        let current = level.block(test);
        if direction != Direction::Down || can_solidify(current) {
            test = pos.relative(direction);
            let state = level.block(test);
            if can_solidify(state)
                && !is_face_sturdy(state, direction.opposite(), SupportType::Full)
            {
                return true;
            }
        }
    }
    false
}

/// `FireBlock.getStateWithAge`: `BaseFireBlock.getState` with the age kept for fire.
pub fn fire_state_with_age(level: &impl BlockLevel, pos: BlockPos, age: i32) -> BlockState {
    let below_pos = pos.below();
    let below = level.block(below_pos);
    if tags().blocks.soul_fire_base_blocks.contains(below) {
        return ids::SOUL_FIRE.default_state();
    }
    // `FireBlock.getStateForPlacement(level, pos)`.
    let mut fire = ids::FIRE.default_state();
    if !can_burn(below)
        && !face_sturdy_at(level, below_pos, below, Direction::Up, SupportType::Full)
    {
        for direction in Direction::ALL {
            if direction != Direction::Down {
                let burns = can_burn(level.block(pos.relative(direction)));
                fire = fire.set_flag(direction_name(direction), burns);
            }
        }
    }
    fire.set_int("age", age)
}

/// `GrowingPlantHeadBlock.updateShape`.
fn growing_plant_head(
    level: &mut impl BlockLevel,
    class: Class,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    neighbour: BlockState,
) -> BlockState {
    let Some((growth, head, body)) = growing_plant(class) else {
        return state;
    };
    // `updateBodyAfterConvertedFromHead`: cave vines keep their berries.
    let to_body = |head_state: BlockState| {
        let body_state = body.default_state();
        if class == Class::CaveVines {
            body_state.set_flag("berries", head_state.flag("berries"))
        } else {
            body_state
        }
    };
    if dir == growth.opposite() {
        if !can_survive(level, state, pos) {
            level.schedule_block_tick(pos, head, 1);
        } else {
            let ahead = level.block(pos.relative(growth));
            if ahead.is(head) || ahead.is(body) {
                return to_body(state);
            }
        }
    }
    if dir != growth || !neighbour.is(head) && !neighbour.is(body) {
        if class == Class::Kelp {
            water_tick(level, pos);
        }
        state
    } else {
        to_body(state)
    }
}

/// `GrowingPlantBodyBlock.updateShape`.
fn growing_plant_body(
    level: &mut impl BlockLevel,
    class: Class,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    neighbour: BlockState,
) -> BlockState {
    let Some((growth, head, body)) = growing_plant(class) else {
        return state;
    };
    if dir == growth.opposite() && !can_survive(level, state, pos) {
        level.schedule_block_tick(pos, body, 1);
    }
    if dir == growth && !neighbour.is(body) && !neighbour.is(head) {
        // `headBlock.getStateForPlacement(random)`, then `updateHeadAfterConvertedFromBody`.
        let age = level.random_int(25);
        let head_state = head.default_state().set_int("age", age);
        return if class == Class::CaveVinesPlant {
            head_state.set_flag("berries", state.flag("berries"))
        } else {
            head_state
        };
    }
    if class == Class::KelpPlant {
        water_tick(level, pos);
    }
    state
}

/// `LeavesBlock.getDistanceAt(state)`.
fn leaves_distance_at(state: BlockState) -> i32 {
    if tags().blocks.prevents_nearby_leaf_decay.contains(state) {
        0
    } else if class_of(state) == Class::Leaves {
        IntProperty::distance().get(state).unwrap_or(7)
    } else {
        7
    }
}

/// `MossyCarpetBlock.getUpdatedState(state, level, pos, false)`.
fn mossy_carpet_updated_state(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
) -> BlockState {
    let base = state.flag("bottom");
    let mut above: Option<BlockState> = None;
    let mut below: Option<BlockState> = None;
    let mut result = state;
    for direction in HORIZONTAL {
        let property = direction_name(direction);
        // `canSupportAtFace`: never up (only horizontal directions are asked here).
        let supported = can_attach_multiface(level.block(pos.relative(direction)), direction);
        let mut side = if supported {
            if base {
                "low"
            } else {
                state.value(property).unwrap_or("none")
            }
        } else {
            "none"
        };
        if side == "low" {
            let above_state = *above.get_or_insert_with(|| level.block(pos.above()));
            if above_state.is(ids::PALE_MOSS_CARPET)
                && above_state.value(property) != Some("none")
                && !above_state.flag("bottom")
            {
                side = "tall";
            }
            if !base {
                let below_state = *below.get_or_insert_with(|| level.block(pos.below()));
                if below_state.is(ids::PALE_MOSS_CARPET)
                    && below_state.value(property) == Some("none")
                {
                    side = "none";
                }
            }
        }
        result = result.set(property, side);
    }
    result
}

/// `VineBlock.getUpdatedState`.
pub(crate) fn vine_updated_state(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
) -> BlockState {
    let above_pos = pos.above();
    let mut result = state;
    if result.flag("up") {
        result = result.set_flag(
            "up",
            can_attach_multiface(level.block(above_pos), Direction::Down),
        );
    }
    let mut above: Option<BlockState> = None;
    for direction in HORIZONTAL {
        let property = direction_name(direction);
        if result.flag(property) {
            // `canSupportAtFace`, then the vine above as a fallback (both test the same).
            let supported = can_attach_multiface(level.block(pos.relative(direction)), direction)
                || {
                    let above_state = *above.get_or_insert_with(|| level.block(above_pos));
                    above_state.is(ids::VINE) && above_state.flag(property)
                };
            result = result.set_flag(property, supported);
        }
    }
    result
}

/// `NoteBlock.setInstrument`.
fn note_block_instrument(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> BlockState {
    let instrument = |s: BlockState| INSTRUMENTS[usize::from(s.misc().instrument)];
    // `NoteBlockInstrument.worksAboveNoteBlock`: mob heads and custom heads.
    let works_above = |name: &str| {
        matches!(
            name,
            "zombie"
                | "skeleton"
                | "creeper"
                | "dragon"
                | "wither_skeleton"
                | "piglin"
                | "custom_head"
        )
    };
    let above = instrument(level.block(pos.above()));
    if works_above(above) {
        return state.set("instrument", above);
    }
    let below = instrument(level.block(pos.below()));
    state.set(
        "instrument",
        if works_above(below) { "harp" } else { below },
    )
}

/// `PotentSulfurBlock.validBlockState` (without the geyser countdown reset of its block
/// entity).
fn potent_sulfur_state(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> BlockState {
    const PROPERTY: &str = "potent_sulfur_state";
    if !is_water_source(level.fluid(pos.above())) {
        return state.set(PROPERTY, "dry");
    }
    let below = level.block(pos.below());
    let fluid = below.fluid_state();
    let source_if_fluid = fluid.is_empty() || fluid.info().source;
    let t = &tags().blocks;
    if t.causes_continuous_geyser_eruptions.contains(below) && source_if_fluid {
        state.set(PROPERTY, "continuous")
    } else if t.causes_periodic_geyser_eruptions.contains(below) && source_if_fluid {
        if state.value(PROPERTY) == Some("erupting") {
            state
        } else {
            state.set(PROPERTY, "dormant")
        }
    } else {
        state.set(PROPERTY, "wet")
    }
}

/// `DiodeBlock.getAlternateSignal` of a repeater (`sideInputDiodesOnly`): the strongest
/// signal a diode beside it outputs towards it.
fn repeater_alternate_signal(level: &impl BlockLevel, pos: BlockPos, facing: Direction) -> i32 {
    [clockwise(facing), counter_clockwise(facing)]
        .into_iter()
        .map(|side| {
            let side_pos = pos.relative(side);
            let other = level.block(side_pos);
            let output = match class_of(other) {
                Class::Repeater => 15,
                Class::Comparator => level.comparator_output(side_pos),
                _ => return 0,
            };
            // `DiodeBlock.getSignal`: `ownSignal` towards its facing.
            if other.direction("facing") == Some(side) && other.flag("powered") {
                output
            } else {
                0
            }
        })
        .max()
        .unwrap_or(0)
}

/// `RedstoneWireBlock.updateShape`.
fn redstone_wire(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
    neighbour: BlockState,
) -> BlockState {
    match dir {
        Direction::Down => {
            if redstone_wire_can_survive_on(level, pos.below(), neighbour) {
                state
            } else {
                AIR
            }
        }
        Direction::Up => wire_connection_state(level, state, pos),
        _ => {
            let w = wire_properties();
            let property = w.side_of(dir);
            let side = w.side_index(wire_connecting_side(
                level,
                pos,
                dir,
                wire_can_connect_up(level, pos),
            ));
            let is_cross = HORIZONTAL.into_iter().all(|d| w.connected(state, d));
            if (side != w.none) == (property.index(state) != w.none) && !is_cross {
                property.with_index(state, side)
            } else {
                let cross = property.with_index(wire_with_sides(state, w.side), side);
                wire_connection_state(level, cross, pos)
            }
        }
    }
}

/// The wire with power of `state` and every side set to the side value `side` (`crossState`
/// for `side`).
fn wire_with_sides(state: BlockState, side: u16) -> BlockState {
    let w = wire_properties();
    let power = w.power.index(state);
    let wire = HORIZONTAL
        .into_iter()
        .fold(ids::REDSTONE_WIRE.default_state(), |s, d| {
            w.side_of(d).with_index(s, side)
        });
    w.power.with_index(wire, power)
}

/// `!level.getBlockState(pos.above()).isRedstoneConductor(level, pos)`.
fn wire_can_connect_up(level: &impl BlockLevel, pos: BlockPos) -> bool {
    !level.block(pos.above()).has(flags::REDSTONE_CONDUCTOR)
}

/// `BlockState.shouldRedstoneWireConnectTo(level, pos, direction)`.
pub fn should_wire_connect_to(state: BlockState, direction: Option<Direction>) -> bool {
    match class_of(state) {
        Class::RedstoneWire => true,
        Class::Repeater => {
            let facing = state.direction("facing");
            direction.is_some()
                && (facing == direction || facing.map(Direction::opposite) == direction)
        }
        Class::Observer => direction.is_some() && state.direction("facing") == direction,
        _ => state.has(flags::SIGNAL_SOURCE) && direction.is_some(),
    }
}

/// `RedstoneWireBlock.getConnectingSide(level, pos, direction, canConnectUp)`.
pub fn wire_connecting_side(
    level: &impl BlockLevel,
    pos: BlockPos,
    direction: Direction,
    can_connect_up: bool,
) -> &'static str {
    let side_pos = pos.relative(direction);
    let side = level.block(side_pos);
    if can_connect_up {
        let placeable_above = class_of(side) == Class::TrapDoor
            || redstone_wire_can_survive_on(level, side_pos, side);
        if placeable_above && should_wire_connect_to(level.block(side_pos.above()), None) {
            return if face_sturdy_at(
                level,
                side_pos,
                side,
                direction.opposite(),
                SupportType::Full,
            ) {
                "up"
            } else {
                "side"
            };
        }
    }
    if !should_wire_connect_to(side, Some(direction))
        && (side.has(flags::REDSTONE_CONDUCTOR)
            || !should_wire_connect_to(level.block(side_pos.below()), None))
    {
        "none"
    } else {
        "side"
    }
}

/// `RedstoneWireBlock.getConnectionState`.
pub fn wire_connection_state(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
) -> BlockState {
    debug_assert!(state.is(ids::REDSTONE_WIRE));
    let w = wire_properties();
    let is_dot = |s: BlockState| HORIZONTAL.into_iter().all(|d| !w.connected(s, d));
    let was_dot = is_dot(state);
    // `getMissingConnections` on the default state with the power of `state`.
    let can_connect_up = wire_can_connect_up(level, pos);
    let mut result = wire_with_sides(state, w.none);
    for direction in HORIZONTAL {
        let side = wire_connecting_side(level, pos, direction, can_connect_up);
        result = w.side_of(direction).with_index(result, w.side_index(side));
    }
    if was_dot && is_dot(result) {
        return result;
    }
    let (north, south, east, west) = (
        w.connected(result, Direction::North),
        w.connected(result, Direction::South),
        w.connected(result, Direction::East),
        w.connected(result, Direction::West),
    );
    let north_south_empty = !north && !south;
    let east_west_empty = !east && !west;
    for (direction, missing) in [
        (Direction::West, !west && north_south_empty),
        (Direction::East, !east && north_south_empty),
        (Direction::North, !north && east_west_empty),
        (Direction::South, !south && east_west_empty),
    ] {
        if missing {
            result = w.side_of(direction).with_index(result, w.side);
        }
    }
    result
}

/// `SpeleothemBlock.updateShape`.
fn speleothem(
    level: &mut impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    dir: Direction,
) -> BlockState {
    water_tick_if_waterlogged(level, state, pos);
    if !matches!(dir, Direction::Up | Direction::Down) {
        return state;
    }
    let Some(tip) = state.direction("vertical_direction") else {
        return state;
    };
    if tip == Direction::Down && level.has_block_tick(pos, state.block()) {
        return state;
    }
    if dir == tip.opposite() && !can_survive(level, state, pos) {
        level.schedule_block_tick(
            pos,
            state.block(),
            if tip == Direction::Down { 2 } else { 1 },
        );
        return state;
    }
    let merge = state.value("thickness") == Some("tip_merge");
    state.set(
        "thickness",
        speleothem_thickness(level, state, pos, tip, merge),
    )
}

/// `SpeleothemBlock.calculateSpeleothemThickness`.
fn speleothem_thickness(
    level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    tip: Direction,
    merge_opposing_tips: bool,
) -> &'static str {
    let base = tip.opposite();
    let in_front = level.block(pos.relative(tip));
    if is_speleothem_with_direction(in_front, base) && in_front.block() == state.block() {
        if !merge_opposing_tips && in_front.value("thickness") != Some("tip_merge") {
            "tip"
        } else {
            "tip_merge"
        }
    } else if !is_speleothem_with_direction(in_front, tip) {
        "tip"
    } else {
        let front_thickness = in_front.value("thickness");
        if front_thickness != Some("tip") && front_thickness != Some("tip_merge") {
            let behind = level.block(pos.relative(base));
            if is_speleothem_with_direction(behind, tip) {
                "middle"
            } else {
                "base"
            }
        } else {
            "frustum"
        }
    }
}
