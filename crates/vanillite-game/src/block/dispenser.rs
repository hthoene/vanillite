//! Dispensers and droppers (`DispenserBlock`, `DropperBlock`) and the dispenser item
//! behaviours (`DispenseItemBehavior.bootStrap`).
//!
//! A rising signal at the block or the block above sets `triggered` and schedules a tick in 4
//! ticks; the tick dispenses from a random non-empty slot ([`dispense_slot`]). A dropper puts
//! one item into the container in front or drops it; a dispenser runs the behaviour of the
//! item ([`behaviour_of`], `DispenserBlock.getDispenseMethod`): every behaviour plays the
//! dispense sound (or the fail click of `OptionalDispenseItemBehavior`) and the smoke
//! animation, and its result replaces the slot's stack.
//!
//! Behaviours that spawn entity kinds the world does not simulate yet (projectiles, fireworks,
//! fireballs, boats, minecarts, armour stands, spawn eggs, the mobs of mob buckets) fall back
//! to `DefaultDispenseItemBehavior`: the item drops. Behaviours on mobs (shearing sheep,
//! brushing armadillos, chests on llamas, saddles and horse armour, sulfur cubes) find no mob
//! and act as in a world without that mob. Golems and the wither are not spawned by placed
//! pumpkins and skulls: a dispenser does not place them, it only equips players.

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockKind, BlockState, ids};
use vanillite_data::entity::ids as entity_ids;
use vanillite_data::item::{ItemId, ids as items};
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::{PosExt, StateExt};

use super::container::ContainerRef;
use super::{BlockBehaviour, block_tags, sound_id};
use crate::item::{self, EquipmentSlot, ItemStack};
use crate::level::game_event::{GameEventContext, events};
use crate::level::{Level, SOUND_BLOCKS, SOUND_NEUTRAL, update};

/// `LevelEvent.SOUND_DISPENSER_DISPENSE`.
const DISPENSE_SUCCESS: i32 = 1000;
/// `LevelEvent.SOUND_DISPENSER_FAIL`.
const DISPENSE_FAIL: i32 = 1001;
/// `LevelEvent.PARTICLES_SHOOT_SMOKE`.
const DISPENSE_ANIMATION: i32 = 2000;
/// `LevelEvent.PARTICLES_DESTROY_BLOCK`.
const DESTROY_BLOCK: i32 = 2001;
/// `LevelEvent.PARTICLES_AND_SOUND_PLANT_GROWTH`.
const PLANT_GROWTH: i32 = 1505;
/// `LevelEvent.PARTICLES_AND_SOUND_WAX_ON`.
const WAX_ON: i32 = 3003;
/// `DispenserBlock.TRIGGER_DURATION`.
const TRIGGER_DURATION: i32 = 4;
/// `SoundSource.PLAYERS`.
const SOUND_PLAYERS: u8 = 7;

/// Fills the dispenser and dropper behaviours of `block`.
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    if block == ids::DISPENSER || block == ids::DROPPER {
        b.neighbor_changed = Some(neighbor_changed);
        b.tick = Some(tick);
        b.affect_neighbors_after_removal = Some(container_removed);
        b.analog_output = Some(container_signal);
    }
}

/// `DispenserBlock.neighborChanged`.
fn neighbor_changed(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    _source: BlockId,
    _moved: bool,
) {
    let should_trigger = level.has_neighbor_signal(pos) || level.has_neighbor_signal(pos.above());
    let triggered = state.flag("triggered");
    if should_trigger && !triggered {
        level.schedule_block_tick(pos, state.block(), TRIGGER_DURATION);
        level.set_block(pos, state.set_flag("triggered", true), update::CLIENTS);
    } else if !should_trigger && triggered {
        level.set_block(pos, state.set_flag("triggered", false), update::CLIENTS);
    }
}

/// The dispenser being dispensed from (`BlockSource`) and the slot it dispenses.
#[derive(Clone, Copy, Debug)]
struct Source {
    pos: BlockPos,
    facing: Direction,
    slot: usize,
}

impl Source {
    /// The block in front.
    fn target(self) -> BlockPos {
        self.pos.relative(self.facing)
    }
}

/// `DispenserBlock.tick` / `dispenseFrom` and `DropperBlock.dispenseFrom`.
fn tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    let dropper = state.block() == ids::DROPPER;
    let Some(slot) = dispense_slot(level, pos) else {
        level.level_event(None, DISPENSE_FAIL, pos, 0);
        if !dropper {
            let state = level.block(pos);
            let context = GameEventContext::with_state(None, state);
            level.game_event_block(events().block_activate, pos, context);
        }
        return;
    };
    let container = ContainerRef::Block(pos);
    let stack = container.get(level, slot);
    if stack.is_empty() {
        return;
    }
    // Bukkit's `BlockDispenseEvent`: a cancelled dispense does nothing.
    if let Some(hooks) = level.hooks_for(vanillite_abi::event::BLOCK_DISPENSE) {
        let mut ev = crate::hooks::Event {
            pos,
            state: Some(state),
            item: crate::hooks::item_snbt(&stack),
            ..crate::hooks::Event::new(vanillite_abi::event::BLOCK_DISPENSE)
        };
        hooks.event(crate::hooks::HookWorld::of(level), &mut ev);
        if ev.cancelled {
            return;
        }
    }
    let facing = state.direction("facing").unwrap_or(Direction::North);
    let source = Source { pos, facing, slot };
    let remaining = if dropper {
        // `HopperBlockEntity.getContainerAt`: the block container in front, else a random
        // container entity (chest or hopper minecart, chest boat) at its centre.
        let front = pos.relative(facing);
        let centre = Vec3::new(
            f64::from(front.x) + 0.5,
            f64::from(front.y) + 0.5,
            f64::from(front.z) + 0.5,
        );
        match ContainerRef::at_point(level, centre) {
            // `DropperBlock.dispenseFrom`: one item into the container in front.
            Some(into) => {
                let rest = super::hopper::add_item(
                    level,
                    Some(container),
                    into,
                    stack.with_count(1),
                    Some(facing.opposite()),
                );
                let mut remaining = stack;
                if rest.is_empty() {
                    remaining.shrink(1);
                }
                remaining
            }
            None => dispense(level, source, stack, Behaviour::Default),
        }
    } else {
        let behaviour = behaviour_of(&stack);
        dispense(level, source, stack, behaviour)
    };
    set_item(level, source, remaining);
}

/// `RandomizableContainerBlockEntity.setItem`: the slot's stack, then `setChanged`.
fn set_item(level: &mut Level, source: Source, stack: ItemStack) {
    ContainerRef::Block(source.pos).set(level, source.slot, stack);
    super::container::set_changed(level, source.pos);
}

// --- Behaviour registry ----------------------------------------------------------------------

/// A dispenser item behaviour (`DispenseItemBehavior`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behaviour {
    /// `DefaultDispenseItemBehavior`: the item drops.
    Default,
    /// An entity kind that is not simulated yet: projectiles, fireworks, fire and wind
    /// charges, boats and armour stands. Falls back to [`Behaviour::Default`].
    Entity,
    /// `SpawnEggItemBehavior`.
    SpawnEgg,
    /// Water, lava and mob buckets (`DispensibleContainerItem`).
    FilledBucket,
    /// The powder snow bucket (`SolidBucketItem`).
    PowderSnowBucket,
    /// The empty bucket (`BucketPickup`).
    EmptyBucket,
    /// `ProjectileDispenseBehavior`: arrows, snowballs, eggs, experience bottles, firework
    /// rockets and wind charges fly out.
    Projectile,
    /// `FlintAndSteelDispenseItemBehavior`.
    FlintAndSteel,
    /// Bone meal.
    BoneMeal,
    /// TNT.
    Tnt,
    /// The wither skeleton skull.
    WitherSkull,
    /// The carved pumpkin.
    CarvedPumpkin,
    /// `ShulkerBoxDispenseBehavior`.
    ShulkerBox,
    /// The glass bottle.
    GlassBottle,
    /// Glowstone (respawn anchors).
    Glowstone,
    /// `ShearsDispenseItemBehavior`.
    Shears,
    /// The brush (armadillos).
    Brush,
    /// Honeycomb (waxing copper).
    Honeycomb,
    /// The potion (water onto dirt makes mud).
    Potion,
    /// The chest (chested horses).
    Chest,
    /// `EquipmentDispenseItemBehavior` (items with `equippable`).
    Equipment,
    /// `MinecartDispenseItemBehavior`.
    Minecart,
    /// `BoatDispenseItemBehavior`.
    Boat,
}

/// `DispenserBlock.getDispenseMethod`: the registered behaviour of the stack's item, else
/// `getDefaultDispenseMethod`.
pub fn behaviour_of(stack: &ItemStack) -> Behaviour {
    let id = item::item_id(stack);
    if let Some(b) = registered(id) {
        return b;
    }
    let name = id.name();
    if is_shulker_box(id) {
        Behaviour::ShulkerBox
    } else if item::equippable_slot(stack).is_some() {
        Behaviour::Equipment
    } else if name.ends_with("_spawn_egg") {
        // `SpawnEggItemBehavior` (spawn eggs carry `entity_data`).
        Behaviour::SpawnEgg
    } else {
        // `#minecraft:sulfur_cube_swallowable` items find no sulfur cube and drop.
        Behaviour::Default
    }
}

/// `DISPENSER_REGISTRY`: the behaviours `bootStrap` registers by item.
fn registered(id: ItemId) -> Option<Behaviour> {
    use Behaviour as B;
    let projectiles = [
        items::ARROW,
        items::TIPPED_ARROW,
        items::SPECTRAL_ARROW,
        items::EGG,
        items::BLUE_EGG,
        items::BROWN_EGG,
        items::SNOWBALL,
        items::EXPERIENCE_BOTTLE,
        items::SPLASH_POTION,
        items::LINGERING_POTION,
        items::FIREWORK_ROCKET,
        items::FIRE_CHARGE,
        items::WIND_CHARGE,
    ];
    let mob_buckets = [
        items::SALMON_BUCKET,
        items::COD_BUCKET,
        items::PUFFERFISH_BUCKET,
        items::TROPICAL_FISH_BUCKET,
        items::AXOLOTL_BUCKET,
        items::SULFUR_CUBE_BUCKET,
        items::TADPOLE_BUCKET,
    ];
    if crate::minecart::MinecartKind::of_item(id).is_some() {
        return Some(B::Minecart);
    }
    let name = id.name();
    let shot = [
        items::ARROW,
        items::TIPPED_ARROW,
        items::SPECTRAL_ARROW,
        items::EGG,
        items::BLUE_EGG,
        items::BROWN_EGG,
        items::SNOWBALL,
        items::EXPERIENCE_BOTTLE,
        items::FIREWORK_ROCKET,
        items::WIND_CHARGE,
        items::SPLASH_POTION,
        items::LINGERING_POTION,
        items::FIRE_CHARGE,
    ];
    if shot.contains(&id) {
        return Some(B::Projectile);
    }
    let spawns_entity = projectiles.contains(&id) || id == items::ARMOR_STAND;
    Some(if name.ends_with("_boat") || name.ends_with("_raft") {
        B::Boat
    } else if spawns_entity {
        B::Entity
    } else if id == items::WATER_BUCKET || id == items::LAVA_BUCKET || mob_buckets.contains(&id) {
        B::FilledBucket
    } else if id == items::POWDER_SNOW_BUCKET {
        B::PowderSnowBucket
    } else if id == items::BUCKET {
        B::EmptyBucket
    } else if id == items::FLINT_AND_STEEL {
        B::FlintAndSteel
    } else if id == items::BONE_MEAL {
        B::BoneMeal
    } else if id == items::TNT {
        B::Tnt
    } else if id == items::WITHER_SKELETON_SKULL {
        B::WitherSkull
    } else if id == items::CARVED_PUMPKIN {
        B::CarvedPumpkin
    } else if id == items::GLASS_BOTTLE {
        B::GlassBottle
    } else if id == items::GLOWSTONE {
        B::Glowstone
    } else if id == items::SHEARS {
        B::Shears
    } else if id == items::BRUSH {
        B::Brush
    } else if id == items::HONEYCOMB {
        B::Honeycomb
    } else if id == items::POTION {
        B::Potion
    } else if id == items::CHEST {
        B::Chest
    } else {
        return None;
    })
}

/// `Items.SHULKER_BOX` and `Items.DYED_SHULKER_BOX`.
fn is_shulker_box(id: ItemId) -> bool {
    id.block()
        .is_some_and(|b| b.info().kind == BlockKind::ShulkerBoxBlock)
}

// --- Dispensing ------------------------------------------------------------------------------

/// `DefaultDispenseItemBehavior.dispense`: runs the behaviour, plays the dispense sound (the
/// fail click if an optional behaviour failed) and the smoke animation; returns the stack for
/// the slot.
fn dispense(
    level: &mut Level,
    source: Source,
    stack: ItemStack,
    behaviour: Behaviour,
) -> ItemStack {
    let mut success = true;
    let launch = (behaviour == Behaviour::Projectile).then(|| projectile_config(&stack).4);
    let result = execute(level, source, stack, behaviour, &mut success);
    let sound = match launch {
        // `ProjectileDispenseBehavior.playSound`.
        Some(event) => event,
        None if success => DISPENSE_SUCCESS,
        None => DISPENSE_FAIL,
    };
    level.level_event(None, sound, source.pos, 0);
    level.level_event(
        None,
        DISPENSE_ANIMATION,
        source.pos,
        direction_3d(source.facing),
    );
    result
}

/// `execute(source, dispensed)` of each behaviour. `success` is `OptionalDispenseItemBehavior
/// .isSuccess` (always true for the others).
fn execute(
    level: &mut Level,
    source: Source,
    mut stack: ItemStack,
    behaviour: Behaviour,
    success: &mut bool,
) -> ItemStack {
    use Behaviour as B;
    let target = source.target();
    match behaviour {
        B::Entity if item::item_id(&stack) == items::ARMOR_STAND => {
            // `EntityType.create` snaps the stand to a random yaw from the level random before
            // the dispenser's configuration turns it (Vanilla's head keeps the constructor's
            // `Math.random` yaw, which cannot be reproduced; it follows the yaw here).
            let _ = level.random().next_float();
            // `EntityType.spawn(level, config, pos, DISPENSER, false, false)` facing the
            // dispenser's direction (`toYRot`).
            let pos = Vec3::new(
                f64::from(target.x) + 0.5,
                f64::from(target.y),
                f64::from(target.z) + 0.5,
            );
            #[allow(clippy::cast_precision_loss)]
            let yaw = ((crate::decoration::data_2d(source.facing) & 3) * 90) as f32;
            crate::decoration::stand::spawn_stand(
                level,
                pos,
                yaw,
                &stack,
                vanillite_abi::spawn_reason::DISPENSER,
            );
            stack.shrink(1);
            stack
        }
        B::Chest => {
            // The chested horses in front without a chest: the first tame one takes it.
            let mut found = Vec::new();
            level
                .entities
                .store
                .for_each_in(&Aabb::of_block(target), false, |id| found.push(id));
            for id in found {
                if crate::mob::fam::companion::horse::dispense_chest(&mut level.entities, id) {
                    stack.shrink(1);
                    return stack;
                }
            }
            default_execute(level, source, stack)
        }
        B::Default | B::Entity => default_execute(level, source, stack),
        B::Projectile => {
            shoot_projectile(level, source, &stack);
            stack.shrink(1);
            stack
        }
        B::Minecart => {
            // Onto the rail in front, else dropped like the default behaviour.
            let Some(kind) = crate::minecart::MinecartKind::of_item(item::item_id(&stack)) else {
                return default_execute(level, source, stack);
            };
            if crate::minecart::dispense(level, kind, source.pos, source.facing, &stack) {
                stack.shrink(1);
                stack
            } else {
                dispense(level, source, stack, B::Default)
            }
        }
        B::Boat => dispense_boat(level, source, stack),
        B::SpawnEgg => {
            let Some(kind) = crate::mob::animal::spawn_egg_kind(&stack) else {
                return stack;
            };
            let spawned = crate::mob::animal::spawn_from_item(
                level,
                kind,
                &stack,
                target,
                crate::mob::fam::SpawnReason::Dispenser,
                source.facing != Direction::Up,
                false,
            );
            if spawned.is_some() {
                stack.shrink(1);
                level.game_event_at(None, events().entity_place, source.pos);
            }
            stack
        }
        B::FilledBucket => {
            let id = item::item_id(&stack);
            let fluid = if id == items::LAVA_BUCKET {
                ids::LAVA
            } else {
                ids::WATER
            };
            let mob = crate::player::play::item_use::mob_bucket_kind(id);
            let emptied = match mob {
                // `MobBucketItem.emptyContents` of `Fluids.EMPTY`: only the empty sound.
                Some(kind @ crate::mob::Kind::SulfurCube) => {
                    let sound = crate::player::play::item_use::mob_bucket_sound(kind);
                    play_empty_sound(level, None, fluid, target, Some(sound));
                    true
                }
                Some(kind) => {
                    let sound = crate::player::play::item_use::mob_bucket_sound(kind);
                    empty_bucket_contents_by(level, None, fluid, target, None, false, Some(sound))
                }
                None => empty_bucket_contents(level, fluid, target),
            };
            if emptied {
                // `MobBucketItem.checkExtraContent`.
                if let Some(kind) = mob {
                    crate::player::play::item_use::release_mob(level, kind, &stack, target);
                    level.game_event_at(None, events().entity_place, target);
                }
                consume_with_remainder(level, source, stack, ItemStack::new(items::BUCKET.0, 1))
            } else {
                dispense(level, source, stack, B::Default)
            }
        }
        B::PowderSnowBucket => {
            if place_powder_snow(level, target) {
                consume_with_remainder(level, source, stack, ItemStack::new(items::BUCKET.0, 1))
            } else {
                dispense(level, source, stack, B::Default)
            }
        }
        B::EmptyBucket => match pickup_block(level, target, false) {
            Some(filled) => {
                level.game_event_at(None, events().fluid_pickup, target);
                consume_with_remainder(level, source, stack, ItemStack::new(filled.0, 1))
            }
            None => default_execute(level, source, stack),
        },
        B::FlintAndSteel => {
            *success = flint_and_steel(level, target, source.facing);
            if *success {
                hurt_and_break(level, &mut stack, 1);
            }
            stack
        }
        B::BoneMeal => {
            if super::bonemeal::fertilize(level, target, &mut stack, 0)
                || super::bonemeal::grow_water_plant(level, target, &mut stack)
            {
                level.level_event(None, PLANT_GROWTH, target, 15);
            } else {
                *success = false;
            }
            stack
        }
        B::Tnt => {
            if !level.settings.tnt_explodes {
                *success = false;
                return stack;
            }
            let at = Vec3::new(
                f64::from(target.x) + 0.5,
                f64::from(target.y),
                f64::from(target.z) + 0.5,
            );
            super::tnt::spawn_primed(level, at, None, None);
            let sound = sound_id("entity.tnt.primed");
            level.play_sound(None, at.x, at.y, at.z, sound, SOUND_BLOCKS, 1.0, 1.0);
            level.game_event_at(None, events().entity_place, target);
            stack.shrink(1);
            stack
        }
        // `WitherSkullBlock.canSpawnMob`: the skull goes on a finished wither base, turned like
        // the dispenser, and the wither spawns (`checkSpawn`); else it is equipped.
        B::WitherSkull => {
            let difficulty = level.spawner_env.difficulty;
            if level.block(target).is_air()
                && crate::mob::boss::wither::can_spawn_mob(level, target, difficulty)
            {
                // `RotationSegment.convertToSegment(direction)` of `Direction.toYRot`.
                let y_rot = match source.facing {
                    Direction::South => 0.0_f32,
                    Direction::West => 90.0,
                    Direction::North => 180.0,
                    _ => 270.0,
                };
                #[allow(clippy::cast_possible_truncation)]
                let segment = ((y_rot / 22.5 + 0.5).floor() as i32) & 15;
                let skull = vanillite_data::block::ids::WITHER_SKELETON_SKULL
                    .default_state()
                    .set_int("rotation", segment);
                level.set_block_and_update(target, skull);
                level.game_event_at(None, events().block_place, target);
                let seed = crate::mob::new_entity_seed();
                crate::mob::boss::wither::check_spawn(level, target, difficulty, seed);
                stack.shrink(1);
                *success = true;
            } else {
                *success = dispense_equipment(level, source, &mut stack);
            }
            stack
        }
        // `CarvedPumpkinBlock.canSpawnGolem`: the pumpkin goes on a finished golem body (its
        // `onPlace` builds the golem); else it is equipped.
        B::CarvedPumpkin => {
            if level.block(target).is_air()
                && crate::mob::golem_build::can_spawn_golem(level, target)
            {
                let pumpkin = vanillite_data::block::ids::CARVED_PUMPKIN.default_state();
                level.set_block_and_update(target, pumpkin);
                level.game_event_at(None, events().block_place, target);
                stack.shrink(1);
                *success = true;
            } else {
                *success = dispense_equipment(level, source, &mut stack);
            }
            stack
        }
        B::Equipment => {
            if dispense_equipment(level, source, &mut stack) {
                stack
            } else {
                default_execute(level, source, stack)
            }
        }
        B::ShulkerBox => {
            *success = place_shulker_box(level, source, &mut stack);
            stack
        }
        B::GlassBottle => glass_bottle(level, source, stack, success),
        B::Glowstone => {
            let state = level.block(target);
            if !state.is(ids::RESPAWN_ANCHOR) {
                return default_execute(level, source, stack);
            }
            let charge = state.int("charges").unwrap_or(0);
            if charge == 4 {
                *success = false;
                return stack;
            }
            crate::block::bed::charge_anchor(level, None, target, state);
            stack.shrink(1);
            stack
        }
        B::Shears => {
            *success = shear_beehive(level, target) || shear_entity(level, target);
            if *success {
                hurt_and_break(level, &mut stack, 1);
            }
            stack
        }
        B::Brush => {
            // The armadillos in front (no spectators): the first adult sheds a scute.
            let mut armadillos = Vec::new();
            level
                .entities
                .store
                .for_each_in(&Aabb::of_block(target), false, |id| armadillos.push(id));
            armadillos.retain(|&id| {
                level
                    .entities
                    .mob(id)
                    .is_some_and(|m| m.kind == crate::mob::Kind::Armadillo)
            });
            let brushed = !armadillos.is_empty()
                && crate::level::mob_env::with_mob_env(level, |level, ents, env, cx| {
                    armadillos.into_iter().any(|id| {
                        crate::mob::fam::brainy::armadillo::dispense_brush(level, ents, env, cx, id)
                    })
                });
            *success = brushed;
            if brushed {
                hurt_and_break(level, &mut stack, 16);
            }
            stack
        }
        B::Honeycomb => {
            let state = level.block(target);
            let Some(waxed) = waxed(state) else {
                return default_execute(level, source, stack);
            };
            level.set_block_and_update(target, waxed);
            level.level_event(None, WAX_ON, target, 0);
            level.play_block_sound(
                None,
                target,
                sound_id("item.honeycomb.wax_on"),
                SOUND_BLOCKS,
                1.0,
                1.0,
            );
            stack.shrink(1);
            stack
        }
        B::Potion => {
            let water = vanillite_data::registry::POTION.id("minecraft:water");
            if water.is_none() || crate::recipe::potion_of(&stack) != water {
                return dispense(level, source, stack, B::Default);
            }
            if !block_tags()
                .convertible_to_mud
                .contains(level.block(target))
            {
                return dispense(level, source, stack, B::Default);
            }
            // `sendParticles(SPLASH, ...)` five times on top of the dispenser.
            let pos = source.pos;
            for _ in 0..5 {
                let x = f64::from(pos.x) + level.random().next_double();
                let z = f64::from(pos.z) + level.random().next_double();
                let at = vanillite_base::Vec3::new(x, f64::from(pos.y + 1), z);
                let splash = crate::level::particles::simple("minecraft:splash");
                level.send_particles(splash, at, 1, vanillite_base::Vec3::ZERO, 1.0);
            }
            level.play_block_sound(
                None,
                pos,
                sound_id("item.bottle.empty"),
                SOUND_BLOCKS,
                1.0,
                1.0,
            );
            level.game_event_at(None, events().fluid_place, pos);
            level.set_block_and_update(target, ids::MUD.default_state());
            let bottle = ItemStack::new(items::GLASS_BOTTLE.0, 1);
            consume_with_remainder(level, source, stack, bottle)
        }
    }
}

/// `BoatDispenseItemBehavior.execute`: a boat just outside the dispenser on or above the water
/// in front (yawed like the dispenser), else the nested default dispense.
fn dispense_boat(level: &mut Level, source: Source, mut stack: ItemStack) -> ItemStack {
    let entity = vanillite_data::registry::ENTITY_TYPE
        .id(item::item_id(&stack).name())
        .map(vanillite_data::entity::EntityTypeId)
        .and_then(crate::vehicle::BoatKind::of);
    let Some(kind) = entity else {
        return dispense(level, source, stack, Behaviour::Default);
    };
    let [sx, sy, sz] = source.facing.normal();
    let just_outside = 0.5625 + f64::from(kind.kind.info().width) / 2.0;
    let x = f64::from(source.pos.x) + 0.5 + f64::from(sx) * just_outside;
    let y = f64::from(source.pos.y) + 0.5 + f64::from(sy) * 1.125;
    let z = f64::from(source.pos.z) + 0.5 + f64::from(sz) * just_outside;
    let front = source.target();
    let water = crate::mob::is_water;
    let y_offset = if water(level.block(front)) {
        1.0
    } else if !level.block(front).is_air() || !water(level.block(front.below())) {
        return dispense(level, source, stack, Behaviour::Default);
    } else {
        0.0
    };
    let mut data = crate::vehicle::BoatData::new(kind, crate::mob::new_entity_seed());
    data.custom_name = stack
        .component(vanillite_protocol::components::ids::CUSTOM_NAME)
        .map(Into::into);
    #[allow(clippy::cast_precision_loss)]
    let yaw = ((crate::decoration::data_2d(source.facing) & 3) * 90) as f32;
    let uuid = crate::mob::new_uuid();
    let (ents, network_ids, _) = level.entity_parts();
    let id = crate::vehicle::spawn_boat(
        ents,
        network_ids,
        uuid,
        Vec3::new(x, y + y_offset, z),
        yaw,
        data,
    );
    crate::vehicle::apply_stack_entity_data(ents, id, &stack, false);
    if ents.spawned(id, vanillite_abi::spawn_reason::DISPENSER, 0) {
        stack.shrink(1);
    }
    stack
}

/// `DefaultDispenseItemBehavior.execute`: one item drops from the dispenser's face.
fn default_execute(level: &mut Level, source: Source, mut stack: ItemStack) -> ItemStack {
    let one = stack.split(1);
    spawn_dispensed(level, source.pos, source.facing, one);
    stack
}

/// `DefaultDispenseItemBehavior.consumeWithRemainder`: one item is used up and `remainder`
/// goes into the dispenser (or drops if it does not fit).
fn consume_with_remainder(
    level: &mut Level,
    source: Source,
    mut dispensed: ItemStack,
    remainder: ItemStack,
) -> ItemStack {
    dispensed.shrink(1);
    if dispensed.is_empty() {
        return remainder;
    }
    // Vanilla changes the slot's own stack: the slot holds what is left while the remainder
    // goes in (and may join it).
    set_item(level, source, dispensed);
    let rest = insert_item(level, source.pos, remainder);
    if !rest.is_empty() {
        spawn_dispensed(level, source.pos, source.facing, rest);
        level.level_event(None, DISPENSE_SUCCESS, source.pos, 0);
        level.level_event(
            None,
            DISPENSE_ANIMATION,
            source.pos,
            direction_3d(source.facing),
        );
    }
    ContainerRef::Block(source.pos).get(level, source.slot)
}

/// `DispenserBlockEntity.insertItem`: fills matching and empty slots in order; returns what
/// did not fit.
fn insert_item(level: &mut Level, pos: BlockPos, mut stack: ItemStack) -> ItemStack {
    let max = item::max_stack_size(&stack).min(crate::menu::CONTAINER_MAX);
    let size = super::container::items(level, pos).map_or(0, <[ItemStack]>::len);
    for i in 0..size {
        let target = ContainerRef::Block(pos).get(level, i);
        if !target.is_empty() && !stack.same_item_same_components(&target) {
            continue;
        }
        let count = stack.count.min(max - target.count);
        if count > 0 {
            if target.is_empty() {
                let part = stack.split(count);
                ContainerRef::Block(pos).set(level, i, part);
                super::container::set_changed(level, pos);
            } else {
                stack.shrink(count);
                if let Some(slot) = ContainerRef::Block(pos).get_mut(level, i) {
                    slot.grow(count);
                }
            }
        }
        if stack.is_empty() {
            break;
        }
    }
    stack
}

/// `Direction.get3DDataValue`.
pub(super) fn direction_3d(d: Direction) -> i32 {
    match d {
        Direction::Down => 0,
        Direction::Up => 1,
        Direction::North => 2,
        Direction::South => 3,
        Direction::West => 4,
        Direction::East => 5,
    }
}

/// `DefaultDispenseItemBehavior.spawnItem(level, stack, 6, facing, dispensePosition)`.
pub(super) fn spawn_dispensed(
    level: &mut Level,
    pos: BlockPos,
    facing: Direction,
    stack: ItemStack,
) {
    let [dx, dy, dz] = facing.normal();
    let x = f64::from(pos.x) + 0.5 + 0.7 * f64::from(dx);
    let mut y = f64::from(pos.y) + 0.5 + 0.7 * f64::from(dy);
    let z = f64::from(pos.z) + 0.5 + 0.7 * f64::from(dz);
    y -= if dy != 0 { 0.125 } else { 0.156_25 };
    // `new ItemEntity(level, x, y, z, stack)` draws its first velocity from the entity's own
    // random, not the level's.
    let pow = level.random().next_double() * 0.1 + 0.2;
    let spread = 0.017_227_5 * 6.0;
    let mut triangle = |mode: f64| {
        let a = level.random().next_double();
        let b = level.random().next_double();
        mode + spread * (a - b)
    };
    let vx = triangle(f64::from(dx) * pow);
    let vy = triangle(0.2);
    let vz = triangle(f64::from(dz) * pow);
    super::drops::spawn_item(
        level,
        Vec3::new(x, y, z),
        stack,
        Vec3::new(vx, vy, vz),
        0,
        None,
    );
}

/// `ItemStack.hurtAndBreak(amount, level, null, ...)`: durability loss through the
/// `item_damage` effects (Unbreaking) from the level random; the stack breaks at its maximum.
fn hurt_and_break(level: &mut Level, stack: &mut ItemStack, amount: i32) {
    crate::durability::hurt_and_break(stack, amount, level.random());
}

// --- Buckets ---------------------------------------------------------------------------------

/// `BucketItem.emptyContents(null, level, pos, null)` of a water or lava bucket (`fluid` is
/// the liquid block): whether the bucket emptied.
fn empty_bucket_contents(level: &mut Level, fluid: BlockId, pos: BlockPos) -> bool {
    empty_bucket_contents_by(level, None, fluid, pos, None, false, None)
}

/// `clicked instanceof LiquidBlockContainer`: blocks that can hold water.
pub(crate) fn is_liquid_container(state: BlockState) -> bool {
    state.value("waterlogged").is_some() || is_closed_container(state.block())
}

/// `BucketItem.emptyContents(user, level, pos, hitResult)`: `hit` is the clicked block and
/// face of a player's use, `sneaking` whether the user sneaks; `mob_sound` is a mob bucket's
/// `emptySound` (`MobBucketItem.playEmptySound`).
pub(crate) fn empty_bucket_contents_by(
    level: &mut Level,
    user: Option<EntityId>,
    fluid: BlockId,
    pos: BlockPos,
    hit: Option<(BlockPos, Direction)>,
    sneaking: bool,
    mob_sound: Option<&str>,
) -> bool {
    let state = level.block(pos);
    let kind = state.block().info().kind;
    // `canBeReplaced(state, fluid)`.
    let may_replace = !matches!(kind, BlockKind::EndGatewayBlock | BlockKind::EndPortalBlock)
        && (super::can_be_replaced(state) || !super::is_solid(state));
    let water = fluid == ids::WATER;
    // `BarrierBlock.canPlaceLiquid`: only creative players waterlog a barrier.
    let waterloggable = state.value("waterlogged").is_some()
        && (!state.is(ids::BARRIER)
            || user.is_some_and(|u| {
                level
                    .entities
                    .player(u)
                    .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Creative)
            }));
    // `LiquidBlockContainer.canPlaceLiquid`: any water for waterloggable blocks, also when they
    // hold water already (the bucket then empties without effect); not double slabs.
    let place_liquid =
        may_replace || (water && waterloggable && !super::fluid::is_double_slab(state));
    if !(state.is_air() || (place_liquid && (!sneaking || hit.is_none()))) {
        return hit.is_some_and(|(clicked, face)| {
            empty_bucket_contents_by(
                level,
                user,
                fluid,
                clicked.relative(face),
                None,
                sneaking,
                mob_sound,
            )
        });
    }
    if level.settings.water_evaporates && water {
        let pitch = 2.6 + (level.random().next_float() - level.random().next_float()) * 0.8;
        level.play_block_sound(
            user,
            pos,
            sound_id("block.fire.extinguish"),
            SOUND_BLOCKS,
            0.5,
            pitch,
        );
        // `sendParticles(LARGE_SMOKE, x, y, z, 8, 1, 1, 1, 0, ALTERNATIVE)`.
        let mut smoke = crate::level::particles::packet(
            crate::level::particles::simple("minecraft:large_smoke"),
            Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z)),
            8,
            Vec3::new(1.0, 1.0, 1.0),
            0.0,
        );
        smoke.randomization = 1;
        level.send_particles_packet(smoke);
        return true;
    }
    if water && (waterloggable || is_closed_container(state.block())) {
        // `LiquidBlockContainer.placeLiquid` (kelp and seagrass take none).
        super::fluid::place_liquid(level, pos, state, ids::WATER.default_state().fluid_state());
        play_empty_sound(level, user, fluid, pos, mob_sound);
        return true;
    }
    if may_replace && !super::is_liquid(state) {
        level.destroy_block(pos, true, None);
    }
    if !level.set_block(pos, fluid.default_state(), update::ALL_IMMEDIATE)
        && !state.fluid_state().info().source
    {
        return false;
    }
    play_empty_sound(level, user, fluid, pos, mob_sound);
    true
}

/// Kelp and seagrass: `LiquidBlockContainer`s that hold water but take none.
fn is_closed_container(block: BlockId) -> bool {
    [
        ids::KELP,
        ids::KELP_PLANT,
        ids::SEAGRASS,
        ids::TALL_SEAGRASS,
    ]
    .contains(&block)
}

/// `BucketItem.playEmptySound` (`MobBucketItem.playEmptySound` with `mob_sound`).
fn play_empty_sound(
    level: &mut Level,
    user: Option<EntityId>,
    fluid: BlockId,
    pos: BlockPos,
    mob_sound: Option<&str>,
) {
    if let Some(sound) = mob_sound {
        level.play_block_sound(user, pos, sound_id(sound), SOUND_NEUTRAL, 1.0, 1.0);
        return;
    }
    let sound = if fluid == ids::LAVA {
        "item.bucket.empty_lava"
    } else {
        "item.bucket.empty"
    };
    level.play_block_sound(user, pos, sound_id(sound), SOUND_BLOCKS, 1.0, 1.0);
    level.game_event_at(user, events().fluid_place, pos);
}

/// `SolidBucketItem.emptyContents` of the powder snow bucket.
fn place_powder_snow(level: &mut Level, pos: BlockPos) -> bool {
    if !level.in_height(pos.y) || !level.block(pos).is_air() {
        return false;
    }
    level.set_block_and_update(pos, ids::POWDER_SNOW.default_state());
    level.game_event_at(None, events().fluid_place, pos);
    level.play_block_sound(
        None,
        pos,
        sound_id("item.bucket.empty_powder_snow"),
        SOUND_BLOCKS,
        1.0,
        1.0,
    );
    true
}

/// `BucketPickup.pickupBlock(null, level, pos, state)`: the filled bucket, `None` if the block
/// gives nothing (also for blocks that are no `BucketPickup`).
pub(crate) fn pickup_block(level: &mut Level, pos: BlockPos, creative: bool) -> Option<ItemId> {
    let state = level.block(pos);
    match state.block().info().kind {
        BlockKind::LiquidBlock => {
            if state.int("level") != Some(0) {
                return None;
            }
            level.set_block(pos, BlockState::AIR, update::ALL_IMMEDIATE);
            Some(if state.is(ids::LAVA) {
                items::LAVA_BUCKET
            } else {
                items::WATER_BUCKET
            })
        }
        BlockKind::PowderSnowBlock => {
            level.set_block(pos, BlockState::AIR, update::ALL_IMMEDIATE);
            level.level_event(None, DESTROY_BLOCK, pos, i32::from(state.0));
            Some(items::POWDER_SNOW_BUCKET)
        }
        BlockKind::BubbleColumnBlock => {
            level.set_block(pos, BlockState::AIR, update::ALL_IMMEDIATE);
            Some(items::WATER_BUCKET)
        }
        // `BarrierBlock.pickupBlock`: only for creative players.
        _ if state.is(ids::BARRIER) && !creative => None,
        _ if state.flag("waterlogged") => {
            // `SimpleWaterloggedBlock.pickupBlock`.
            level.set_block(pos, state.set_flag("waterlogged", false), update::ALL);
            if !level.can_survive(state, pos) {
                level.destroy_block(pos, true, None);
            }
            Some(items::WATER_BUCKET)
        }
        _ => None,
    }
}

// --- Flint and steel, shears, honeycomb, bottles ---------------------------------------------

/// `FlintAndSteelDispenseItemBehavior.execute` without the durability: whether it succeeded.
fn flint_and_steel(level: &mut Level, target: BlockPos, facing: Direction) -> bool {
    // `tryIgniteExplosiveEntities`: sulfur cubes have no explosion data here (their swallowed
    // block archetypes are not simulated), so none can explode.
    let state = level.block(target);
    let fire = super::place::base_fire_state(level, target);
    // `BaseFireBlock.canBePlacedAt`.
    if state.is_air()
        && (level.can_survive(fire, target)
            || super::portal::lights_portal_any(level, target, facing))
    {
        level.set_block_and_update(target, fire);
        level.game_event_at(None, events().block_place, target);
        return true;
    }
    if can_light(state) {
        level.set_block_and_update(target, state.set_flag("lit", true));
        level.game_event_at(None, events().block_change, target);
        return true;
    }
    if state.block().info().kind == BlockKind::TntBlock {
        // `TntBlock.prime` fails while TNT does not explode.
        if !level.settings.tnt_explodes {
            return false;
        }
        super::tnt::prime(level, target, None);
        level.remove_block(target, false);
    }
    true
}

/// `CampfireBlock.canLight`, `CandleBlock.canLight` and `CandleCakeBlock.canLight`.
fn can_light(state: BlockState) -> bool {
    let tags = block_tags();
    let lit = state.flag("lit");
    let waterlogged = state.flag("waterlogged");
    let has = |p: &str| state.value(p).is_some();
    ((tags.campfires.contains(state) || tags.candles.contains(state))
        && has("lit")
        && has("waterlogged")
        && !lit
        && !waterlogged)
        || (tags.candle_cakes.contains(state) && has("lit") && !lit)
}

/// The honey level of a full beehive or bee nest at `pos`, if it is one.
fn full_beehive(state: BlockState) -> bool {
    block_tags().beehives.contains(state)
        && state.block().info().kind == BlockKind::BeehiveBlock
        && state.int("honey_level").is_some_and(|h| h >= 5)
}

/// `BeehiveBlock.releaseBeesAndResetHoneyLevel(level, state, pos, null, BEE_RELEASED)`.
fn reset_honey_level(level: &mut Level, pos: BlockPos, state: BlockState) {
    super::beehive::release_bees_and_reset_honey_level(
        level,
        state,
        pos,
        None,
        super::beehive::ReleaseStatus::BeeReleased,
    );
}

/// `ShearsDispenseItemBehavior.tryShearBeehive`.
fn shear_beehive(level: &mut Level, pos: BlockPos) -> bool {
    let state = level.block(pos);
    if !full_beehive(state) {
        return false;
    }
    level.play_block_sound(
        None,
        pos,
        sound_id("block.beehive.shear"),
        SOUND_BLOCKS,
        1.0,
        1.0,
    );
    super::beehive::drop_honeycomb(level, pos);
    reset_honey_level(level, pos, state);
    level.game_event_at(None, events().shear, pos);
    true
}

/// `ShearsDispenseItemBehavior.tryShearEntity`: the first non-spectator entity in the block
/// that loses a lead (`shearOffAllLeashConnections`) or is an alive `Shearable` ready for
/// shearing gets sheared (sounds in `BLOCKS`), then `SHEAR` at the block.
fn shear_entity(level: &mut Level, pos: BlockPos) -> bool {
    use crate::mob::Kind;
    use crate::mob::fam::{mount, undead, wildlife};
    let aabb = Aabb::new(
        f64::from(pos.x),
        f64::from(pos.y),
        f64::from(pos.z),
        f64::from(pos.x) + 1.0,
        f64::from(pos.y) + 1.0,
        f64::from(pos.z) + 1.0,
    );
    let mut candidates: Vec<EntityId> = Vec::new();
    level
        .entities
        .store
        .for_each_in(&aabb, false, |id| candidates.push(id));
    candidates.retain(|&id| {
        level
            .entities
            .player(id)
            .is_none_or(|p| p.game_mode != vanillite_config::GameMode::Spectator)
    });
    if candidates.is_empty() {
        return false;
    }
    let sheared = crate::level::mob_env::with_mob_env(level, |level, ents, env, cx| {
        let blocks = vanillite_protocol::packets::play::clientbound::SoundSource::Blocks;
        for id in candidates {
            if crate::mob::tame::shear_off_all_leash_connections(ents, env, id, None) {
                return Some(false);
            }
            let Some(m) = ents.mob(id) else {
                continue;
            };
            if !m.is_alive() {
                continue;
            }
            let ready = match m.kind {
                Kind::Sheep => !m.sheared && !m.baby,
                Kind::SnowGolem => wildlife::st(m).pumpkin,
                Kind::Mooshroom => !m.baby,
                Kind::Bogged => undead::skeletons::ready_for_shearing(m),
                Kind::CopperGolem => mount::copper_golem::ready_for_shearing(m),
                Kind::SulfurCube => undead::cubes::has_body_item(m),
                _ => false,
            };
            if !ready {
                continue;
            }
            match m.kind {
                Kind::Sheep => crate::mob::animal::shear_sheep(level, ents, env, cx, id, blocks),
                Kind::SnowGolem => wildlife::golem::snow_shear(ents, env, id, blocks),
                Kind::Mooshroom => wildlife::mooshroom::shear(level, ents, env, cx, id, blocks),
                Kind::Bogged => undead::skeletons::shear(level, ents, env, id, blocks),
                Kind::CopperGolem => mount::copper_golem::shear(ents, env, id, blocks),
                _ => undead::cubes::shear(ents, env, id),
            }
            return Some(true);
        }
        None
    });
    if sheared == Some(true) {
        level.game_event_at(None, events().shear, pos);
    }
    sheared.is_some()
}

/// `HoneycombItem.getWaxed`: the waxed block of a copper block with the same properties.
pub(crate) fn waxed(state: BlockState) -> Option<BlockState> {
    let name = state.block().name();
    let path = name.strip_prefix("minecraft:")?;
    let waxed = BlockId::by_name(&format!("minecraft:waxed_{path}"))?;
    let mut out = waxed.default_state();
    for (key, value) in state.properties() {
        out = out.with(key, value).unwrap_or(out);
    }
    Some(out)
}

/// The glass bottle behaviour: honey from a full beehive, water from water.
fn glass_bottle(
    level: &mut Level,
    source: Source,
    stack: ItemStack,
    success: &mut bool,
) -> ItemStack {
    *success = false;
    let target = source.target();
    let state = level.block(target);
    let filled = if full_beehive(state) {
        reset_honey_level(level, target, state);
        ItemStack::new(items::HONEY_BOTTLE.0, 1)
    } else if super::fluid::is_water(level.fluid(target)) {
        water_potion()
    } else {
        return default_execute(level, source, stack);
    };
    *success = true;
    level.game_event_at(None, events().fluid_pickup, source.pos);
    consume_with_remainder(level, source, stack, filled)
}

/// `PotionContents.createItemStack(Items.POTION, Potions.WATER)`.
pub(crate) fn water_potion() -> ItemStack {
    let mut stack = ItemStack::new(items::POTION.0, 1);
    if let Some(water) = vanillite_data::registry::POTION.id("minecraft:water") {
        stack.set_component(
            vanillite_protocol::components::ids::POTION_CONTENTS,
            crate::recipe::potion_contents(water),
        );
    }
    stack
}

// --- Shulker boxes ---------------------------------------------------------------------------

/// `ShulkerBoxDispenseBehavior`: `BlockItem.place` with a `DirectionalPlaceContext` in front,
/// clicked on the dispenser's facing (up when the block below the target is not empty).
fn place_shulker_box(level: &mut Level, source: Source, stack: &mut ItemStack) -> bool {
    use super::place;
    let id = item::item_id(stack);
    let target = source.target();
    if !level.in_height(target.y) || !level.is_loaded(target) {
        return false;
    }
    let face = if level.block(target.below()).is_air() {
        source.facing
    } else {
        Direction::Up
    };
    let hit = Vec3::new(
        f64::from(target.x) + 0.5,
        f64::from(target.y),
        f64::from(target.z) + 0.5,
    );
    let ctx = place::PlaceContext::new(target, face, hit, false, 0.0, 0.0, false, id, level);
    if !ctx.replace_clicked {
        return false;
    }
    let Some(state) = place::state_for_placement(&ctx, level) else {
        return false;
    };
    if !level.can_survive(state, target) || !is_unobstructed(level, state, target) {
        return false;
    }
    if !level.set_block(target, state, update::ALL_IMMEDIATE) {
        return false;
    }
    let mut placed = level.block(target);
    if placed.block() == state.block() {
        if let Some(updated) = crate::player::play::apply_block_state_component(stack, placed) {
            level.set_block(target, updated, update::CLIENTS);
            placed = updated;
        }
        // `BlockItem.place` without a player: `updateCustomBlockEntityTag` (no operator-only
        // kinds) and `applyComponentsFromItemStack` (name, lock, contents, loot).
        crate::level::block_entity::apply_block_entity_data(level, target, stack, false);
        crate::level::block_entity::apply_components(level, target, stack);
    }
    let sound = placed.block().sound();
    level.play_block_sound(
        None,
        target,
        sound.place_sound,
        SOUND_BLOCKS,
        (sound.volume + 1.0) / 2.0,
        sound.pitch * 0.8,
    );
    let context = GameEventContext::with_state(None, placed);
    level.game_event_block(events().block_place, target, context);
    stack.shrink(1);
    true
}

/// `Entity.blocksBuilding` of a live entity that is no spectator: living entities (not marker
/// armor stands), minecarts, boats, end crystals, falling blocks and primed TNT.
pub fn blocks_building(level: &Level, id: vanillite_world::entity::EntityId) -> bool {
    entity_blocks_building(&level.entities, id)
}

/// [`blocks_building`] on the entities themselves (while they are taken out of the level).
pub fn entity_blocks_building(
    ents: &crate::entities::Entities,
    id: vanillite_world::entity::EntityId,
) -> bool {
    let Some(kind) = ents.store.get(id).map(|e| e.kind()) else {
        return false;
    };
    if kind == entity_ids::PLAYER {
        return ents
            .player(id)
            .is_some_and(|p| p.game_mode != vanillite_config::GameMode::Spectator);
    }
    if kind == entity_ids::ARMOR_STAND {
        return !ents
            .mob(id)
            .is_some_and(crate::decoration::stand::is_marker);
    }
    super::plate::is_living(kind)
        || super::rail::is_minecart(kind)
        || ents.boat(id).is_some()
        || kind == entity_ids::END_CRYSTAL
        || kind == entity_ids::FALLING_BLOCK
        || kind == entity_ids::TNT
}

/// `Level.isUnobstructed(state, pos, CollisionContext.empty())`: no entity that blocks
/// building ([`blocks_building`]) in the block's collision shape.
fn is_unobstructed(level: &Level, state: BlockState, pos: BlockPos) -> bool {
    let (dx, dy, dz) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    for b in state.collision_shape() {
        let shape = Aabb::new(
            b[0] + dx,
            b[1] + dy,
            b[2] + dz,
            b[3] + dx,
            b[4] + dy,
            b[5] + dz,
        );
        let mut blocked = false;
        level.entities.store.for_each_in(&shape, false, |id| {
            blocked |= blocks_building(level, id);
        });
        if blocked {
            return false;
        }
    }
    true
}

// --- Equipment -------------------------------------------------------------------------------

/// The equipment slot a player wears the item in, if players can wear it
/// (`canUseSlot(equippable.slot())`; body and saddle slots are for mobs).
fn player_slot(stack: &ItemStack) -> Option<EquipmentSlot> {
    item::equippable(stack)
        .filter(|e| e.can_be_equipped_by(vanillite_data::entity::ids::PLAYER.0))
        .map(|e| e.slot)
        .filter(|s| !matches!(s, EquipmentSlot::Body | EquipmentSlot::Saddle))
}

/// `EquipmentDispenseItemBehavior.dispenseEquipment`: the first living entity in the block in
/// front that can wear the item (`canEquipWithDispenser`) gets one; a mob drops it for sure
/// and no longer despawns.
fn dispense_equipment(level: &mut Level, source: Source, stack: &mut ItemStack) -> bool {
    let Some(slot) = item::equippable_slot(stack) else {
        return false;
    };
    let target = source.target();
    let aabb = Aabb::new(
        f64::from(target.x),
        f64::from(target.y),
        f64::from(target.z),
        f64::from(target.x) + 1.0,
        f64::from(target.y) + 1.0,
        f64::from(target.z) + 1.0,
    );
    let mut candidates: Vec<EntityId> = Vec::new();
    level
        .entities
        .store
        .for_each_in(&aabb, false, |id| candidates.push(id));
    let Some(id) = candidates.into_iter().find(|&id| {
        level.entities.player(id).is_some_and(|p| {
            player_slot(stack).is_some()
                && !p.survival.is_dead_or_dying()
                && p.game_mode != vanillite_config::GameMode::Spectator
                && p.inventory.equipment(slot).is_empty()
        }) || level.entities.mob(id).is_some_and(|m| {
            crate::decoration::stand::can_dispenser_equip(m, stack).is_some()
                || mob_can_equip(m, stack, slot)
        })
    }) else {
        return false;
    };
    if let Some(m) = level.entities.mob(id)
        && m.kind != crate::mob::Kind::ArmorStand
    {
        equip_mob(level, id, slot, stack.split(1));
        return true;
    }
    if level.entities.mob(id).is_some() {
        let slot = player_slot(stack).unwrap_or(slot);
        // An armor stand: `setItemSlot`, the equip sound and the equipment packet.
        let equip = stack.split(1);
        let sound = sound_id(&equip_sound(&equip));
        let silent = level.entities.mob(id).is_some_and(|m| m.silent);
        if let Some(m) = level.entities.mob_mut(id) {
            m.equipment.set(slot, equip);
        }
        let Some(e) = level.entities.store.get(id) else {
            return true;
        };
        let (at, network_id) = (e.pos(), e.network_id().0);
        let slots = crate::mob::equipment::SLOTS
            .iter()
            .map(|&s| {
                let item = level.entities.mob(id).map(|m| m.equipment.get(s).clone());
                (s.ordinal(), item.unwrap_or_default())
            })
            .collect();
        let packet = vanillite_protocol::SharedPacket::of(
            &vanillite_protocol::packets::play::clientbound::SetEquipment {
                entity_id: network_id,
                slots,
            },
        );
        level.output().nearby_packets.push((at, 160.0, packet));
        if !silent {
            level.play_sound(
                None,
                at.x,
                at.y,
                at.z,
                sound,
                crate::level::SOUND_NEUTRAL,
                1.0,
                1.0,
            );
        }
        level.game_event(events().equip, at, GameEventContext::of(Some(id)));
        return true;
    }
    let equip = stack.split(1);
    let sound = sound_id(&equip_sound(&equip));
    if let Some(player) = level.entities.player_mut(id) {
        let inv = &mut player.inventory;
        match slot {
            EquipmentSlot::Feet => inv.set(36, equip),
            EquipmentSlot::Legs => inv.set(37, equip),
            EquipmentSlot::Chest => inv.set(38, equip),
            EquipmentSlot::Head => inv.set(39, equip),
            EquipmentSlot::OffHand => inv.offhand = equip,
            EquipmentSlot::MainHand => {
                let selected = inv.selected;
                inv.set(selected, equip);
            }
            EquipmentSlot::Body | EquipmentSlot::Saddle => {}
        }
    }
    // `LivingEntity.onEquipItem`: the equip sound and game event at the player.
    if let Some(at) = level.entities.store.get(id).map(|e| e.pos()) {
        level.play_sound(None, at.x, at.y, at.z, sound, SOUND_PLAYERS, 1.0, 1.0);
        level.game_event(events().equip, at, GameEventContext::of(Some(id)));
    }
    true
}

/// `LivingEntity.canEquipWithDispenser` of a mob (not an armor stand): alive, the slot of the
/// item's `equippable` usable and empty, the item allows the mob's type and
/// `canDispenserEquipIntoSlot`.
fn mob_can_equip(m: &crate::mob::MobData, stack: &ItemStack, slot: EquipmentSlot) -> bool {
    use crate::mob::Kind as K;
    if m.kind == K::ArmorStand || !m.is_alive() || !crate::mob::animal::can_use_slot(m, slot) {
        return false;
    }
    // `Equippable.allowedEntities`.
    if !item::equippable(stack).is_some_and(|e| e.can_be_equipped_by(m.kind.type_id().0)) {
        return false;
    }
    let empty = if m.kind == K::Pig && slot == EquipmentSlot::Saddle {
        !m.saddled
    } else {
        m.equipment.get(slot).is_empty()
    };
    if !empty {
        return false;
    }
    // `canDispenserEquipIntoSlot`.
    let loot = m.can_pick_up_loot;
    let mount_slot = matches!(slot, EquipmentSlot::Body | EquipmentSlot::Saddle);
    match m.kind {
        K::Allay => false,
        K::Panda | K::Fox | K::Dolphin => slot == EquipmentSlot::MainHand && loot,
        K::HappyGhast | K::SulfurCube => slot == EquipmentSlot::Body,
        K::Pig | K::Strider => slot == EquipmentSlot::Saddle || loot,
        K::Nautilus | K::ZombieNautilus => mount_slot || loot,
        K::Horse
        | K::Donkey
        | K::Mule
        | K::SkeletonHorse
        | K::ZombieHorse
        | K::Llama
        | K::TraderLlama => mount_slot && m.tame || loot,
        K::Camel | K::CamelHusk => mount_slot || loot,
        _ => loot,
    }
}

/// `setItemSlot` with `onEquipItem` (the equip sound from the mob's random, `EQUIP`), then
/// `setGuaranteedDrop` and `setPersistenceRequired`.
fn equip_mob(level: &mut Level, id: EntityId, slot: EquipmentSlot, equip: ItemStack) {
    use crate::mob::Kind as K;
    let Some(pos) = level.entities.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = level.entities.mob_mut(id) else {
        return;
    };
    let saddle = slot == EquipmentSlot::Saddle;
    let sound = match m.kind {
        K::Pig if saddle => "minecraft:entity.pig.saddle",
        K::Strider if saddle => "minecraft:entity.strider.saddle",
        K::Camel if saddle => "minecraft:entity.camel.saddle",
        K::CamelHusk if saddle => "minecraft:entity.camel_husk.saddle",
        K::Nautilus | K::ZombieNautilus if saddle && m.fluids.eye_in_water => {
            "minecraft:item.nautilus_saddle_underwater_equip"
        }
        K::Nautilus | K::ZombieNautilus if saddle => "minecraft:item.nautilus_saddle_equip",
        _ => "",
    };
    let sound = if sound.is_empty() {
        equip_sound(&equip)
    } else {
        std::borrow::Cow::Borrowed(sound)
    };
    if m.kind == K::Pig && saddle {
        // The pig keeps its saddle as a flag.
        m.saddled = true;
    } else {
        m.equipment.set(slot, equip);
        m.equipment.set_guaranteed_drop(slot);
    }
    m.persistent = true;
    let source = m.kind.sound_source();
    let seed = (!m.silent).then(|| m.random.next_long());
    if let Some(seed) = seed {
        let sound = sound_id(sound.strip_prefix("minecraft:").unwrap_or(&sound));
        level.play_seeded_sound(
            None,
            pos.x,
            pos.y,
            pos.z,
            sound,
            source as u8,
            1.0,
            1.0,
            seed,
        );
    }
    level.game_event(events().equip, pos, GameEventContext::of(Some(id)));
    level.entities.equipment_updates.push(id);
}

/// `equippable.equipSound()` without the `minecraft:` namespace.
pub(crate) fn equip_sound(stack: &ItemStack) -> std::borrow::Cow<'static, str> {
    use std::borrow::Cow;
    match item::equippable(stack).map(|e| e.equip_sound) {
        Some(Cow::Borrowed(s)) => Cow::Borrowed(s.strip_prefix("minecraft:").unwrap_or(s)),
        Some(Cow::Owned(s)) => match s.strip_prefix("minecraft:") {
            Some(path) => Cow::Owned(path.to_owned()),
            None => Cow::Owned(s),
        },
        None => Cow::Borrowed("item.armor.equip_generic"),
    }
}

// --- Containers ------------------------------------------------------------------------------

/// `DispenserBlockEntity.getRandomSlot(level.getRandom())`: a random non-empty slot (one
/// `nextInt` per non-empty slot), `None` if the dispenser is empty.
pub fn dispense_slot(level: &mut Level, pos: BlockPos) -> Option<usize> {
    super::chest_loot::unpack(level, pos, None);
    let filled: Vec<usize> = super::container::items(level, pos)?
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, _)| i)
        .collect();
    let mut chosen = None;
    for (odds, slot) in (1..).zip(filled) {
        if level.random().next_int_bounded(odds) == 0 {
            chosen = Some(slot);
        }
    }
    chosen
}

/// `Containers.updateNeighboursAfterDestroy`: comparators re-read the removed container.
pub fn container_removed(level: &mut Level, state: BlockState, pos: BlockPos, _moved: bool) {
    level.update_neighbour_for_output_signal(pos, state.block());
}

/// `getAnalogOutputSignal` of containers (`AbstractContainerMenu
/// .getRedstoneSignalFromBlockEntity`): the fill level; a double chest counts both halves, a
/// blocked chest reads 0.
pub fn container_signal(
    level: &Level,
    state: BlockState,
    pos: BlockPos,
    _direction: Direction,
) -> i32 {
    use super::container::{Kind, kind_of};
    if matches!(
        kind_of(state.block()),
        Some(Kind::Chest | Kind::TrappedChest)
    ) {
        let Some(request) = super::container::menu_provider(level, state, pos) else {
            return 0;
        };
        if let crate::menu::container::Storage::Double(a, b) = request.storage {
            let mut all: Vec<ItemStack> = Vec::with_capacity(54);
            all.extend(
                super::container::items(level, a)
                    .unwrap_or_default()
                    .iter()
                    .cloned(),
            );
            all.extend(
                super::container::items(level, b)
                    .unwrap_or_default()
                    .iter()
                    .cloned(),
            );
            return crate::menu::redstone_signal(&all, crate::menu::CONTAINER_MAX);
        }
    }
    super::container::items(level, pos).map_or(0, |items| {
        crate::menu::redstone_signal(items, crate::menu::CONTAINER_MAX)
    })
}

#[cfg(test)]
mod tests;

// --- Projectiles -----------------------------------------------------------------------------

/// `ProjectileItem.createDispenseConfig` of a dispensed projectile: the offset of the start
/// from the dispenser's centre along its facing and up, the uncertainty, the power and the
/// launch level event.
fn projectile_config(stack: &ItemStack) -> (f64, f64, f32, f32, i32) {
    let id = item::item_id(stack);
    if id == items::FIREWORK_ROCKET {
        // `getEntityJustOutsideOfBlockPos`.
        (0.500_009_999_999_747_4, 0.0, 1.0, 0.5, 1004)
    } else if id == items::WIND_CHARGE {
        (1.0, 0.0, 6.666_666_5, 1.0, 1051)
    } else if id == items::FIRE_CHARGE {
        (1.0, 0.0, 6.666_666_5, 1.0, 1018)
    } else if id == items::EXPERIENCE_BOTTLE
        || id == items::SPLASH_POTION
        || id == items::LINGERING_POTION
    {
        (0.7, 0.1, 6.0 * 0.5, 1.1 * 1.25, 1002)
    } else {
        (0.7, 0.1, 6.0, 1.1, 1002)
    }
}

/// `ProjectileDispenseBehavior.execute`: `asProjectile` at the dispense position, then
/// `spawnProjectileUsingShoot` along the facing.
fn shoot_projectile(level: &mut Level, source: Source, stack: &ItemStack) {
    let (scale, up, uncertainty, power, _) = projectile_config(stack);
    let [sx, sy, sz] = source.facing.normal();
    let (sx, sy, sz) = (f64::from(sx), f64::from(sy), f64::from(sz));
    let at = Vec3::new(
        f64::from(source.pos.x) + 0.5 + scale * sx,
        f64::from(source.pos.y) + 0.5 + scale * sy + up,
        f64::from(source.pos.z) + 0.5 + scale * sz,
    );
    let _ = spawn_projectile_item(level, at, source.facing, stack, power, uncertainty);
}

/// The dispense configuration of a projectile item (`ProjectileItem.createDispenseConfig`):
/// uncertainty, power and the level event that replaces the dispense sound (if any).
pub fn projectile_item_config(stack: &ItemStack) -> Option<(f32, f32, Option<i32>)> {
    let id = item::item_id(stack);
    let projectile = [
        items::ARROW,
        items::TIPPED_ARROW,
        items::SPECTRAL_ARROW,
        items::EGG,
        items::BLUE_EGG,
        items::BROWN_EGG,
        items::SNOWBALL,
        items::EXPERIENCE_BOTTLE,
        items::SPLASH_POTION,
        items::LINGERING_POTION,
        items::FIREWORK_ROCKET,
        items::FIRE_CHARGE,
        items::WIND_CHARGE,
    ]
    .contains(&id);
    if !projectile {
        return None;
    }
    let (_, _, uncertainty, power, event) = projectile_config(stack);
    let event = (id == items::WIND_CHARGE || id == items::FIRE_CHARGE).then_some(event);
    Some((uncertainty, power, event))
}

/// `ProjectileItem.asProjectile(level, at, stack, direction)` and
/// `Projectile.spawnProjectileUsingShoot(projectile, level, stack, step, power, uncertainty)`:
/// the projectile of `stack` at `at`, shot along `direction`. Returns the new entity.
pub fn spawn_projectile_item(
    level: &mut Level,
    at: Vec3,
    direction: Direction,
    stack: &ItemStack,
    power: f32,
    uncertainty: f32,
) -> Option<vanillite_world::entity::EntityId> {
    use crate::mob::missile::{self, MissileData, MissileKind};
    use vanillite_base::random::{LegacyRandom, RandomSource};
    let [sx, sy, sz] = direction.normal();
    let (sx, sy, sz) = (f64::from(sx), f64::from(sy), f64::from(sz));
    let dir = Vec3::new(sx, sy, sz);
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    let id = item::item_id(stack);
    if [items::ARROW, items::TIPPED_ARROW, items::SPECTRAL_ARROW].contains(&id) {
        // `new Arrow(level, x, y, z, stack, null)` with `pickup = ALLOWED`.
        let (entities, _, _) = level.entity_parts();
        return Some(crate::mob::projectile::shoot_arrow(
            entities,
            &network_ids,
            at,
            dir,
            power,
            uncertainty,
            None,
            None,
            &stack.with_count(1),
            false,
            1,
        ));
    }
    if id == items::FIREWORK_ROCKET {
        // `new FireworkRocketEntity(level, stack, x, y, z, true)`, then `shoot`.
        let rocket = crate::firework::spawn_rocket(
            &mut level.entities,
            &network_ids,
            None,
            at,
            &stack.with_count(1),
            None,
            Some(Vec3::ZERO),
        );
        shoot_missile(level, rocket, dir, power, uncertainty);
        return Some(rocket);
    }
    if id == items::SPLASH_POTION || id == items::LINGERING_POTION {
        // `ThrowablePotionItem.createPotion` at the position, then `shoot`.
        return crate::effect::thrown::spawn_shot(
            &mut level.entities,
            &network_ids,
            at,
            stack,
            dir,
            power,
            uncertainty,
        );
    }
    let data = if id == items::WIND_CHARGE || id == items::FIRE_CHARGE {
        // `WindChargeItem` / `FireChargeItem.asProjectile`: a direction around the facing from
        // the level random (the following `shoot` replaces the movement).
        let r = level.random();
        let _ = (
            r.triangle(sx, 0.114_850_000_000_000_01),
            r.triangle(sy, 0.114_850_000_000_000_01),
            r.triangle(sz, 0.114_850_000_000_000_01),
        );
        if id == items::WIND_CHARGE {
            crate::mob::fam::mount::wind::new_charge(MissileKind::WindCharge)
        } else {
            let mut d = MissileData::new(MissileKind::SmallFireball);
            d.item = stack.with_count(1);
            d
        }
    } else {
        let kind = crate::mob::fam::wildlife::snowball::thrown_kind(stack)?;
        let mut d = MissileData::new(kind);
        d.item = stack.with_count(1);
        d
    };
    let mut own = LegacyRandom::new(crate::mob::new_entity_seed());
    let (velocity, rot) = crate::mob::projectile::shoot(&mut own, dir, power, uncertainty);
    let mut data = data;
    data.random_state = own.state();
    Some(missile::spawn(
        &mut level.entities,
        &network_ids,
        crate::mob::new_uuid(),
        at,
        velocity,
        rot,
        data,
    ))
}

/// `Projectile.shoot` of a spawned missile with its own random.
fn shoot_missile(
    level: &mut Level,
    id: vanillite_world::entity::EntityId,
    dir: Vec3,
    power: f32,
    uncertainty: f32,
) {
    use vanillite_base::random::LegacyRandom;
    let Some(state) = level.entities.missile(id).map(|d| d.random_state) else {
        return;
    };
    let mut own = LegacyRandom::from_state(state);
    let (velocity, rot) = crate::mob::projectile::shoot(&mut own, dir, power, uncertainty);
    if let Some(d) = level.entities.missile_mut(id) {
        d.random_state = own.state();
    }
    if let Some(mut e) = level.entities.store.get_mut(id) {
        e.set_velocity(velocity);
        e.set_rot(rot);
    }
}
