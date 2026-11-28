//! What survival needs to know about blocks: tags, fluids, suffocation and the blocks an entity
//! touches (Vanilla's `EntityFluidInteraction`, `Entity.isInWall`, `checkInsideBlocks`).

use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::{BlockId, BlockState, flags};
use vanillite_data::fluid::FluidState;
use vanillite_worldgen::behaviour::StateExt;

/// Block state access of a world. `None` outside loaded chunks.
pub trait BlockAccess {
    /// The block state at `pos`.
    fn block_state(&self, pos: BlockPos) -> Option<BlockState>;
    /// The collision boxes (relative to `pos`) of the moving piston at `pos`, from its block
    /// entity; none without one or without access to block entities.
    fn moving_piston_boxes(&self, _pos: BlockPos, _out: &mut Vec<[f64; 6]>) {}
    /// [`BlockAccess::block_state`] of every position in the box from `min` to `max`
    /// (inclusive) in X, then Z, then Y order (Y varies fastest).
    fn block_states_in(&self, min: BlockPos, max: BlockPos, f: &mut dyn FnMut(Option<BlockState>)) {
        for x in min.x..=max.x {
            for z in min.z..=max.z {
                for y in min.y..=max.y {
                    f(self.block_state(BlockPos::new(x, y, z)));
                }
            }
        }
    }
}

/// Per-block facts from the data packs' block tags and special blocks.
#[derive(Clone, Debug, Default)]
pub struct BlockFacts {
    causes_suffocation: Vec<bool>,
    climbable: Vec<bool>,
    trapdoor: Vec<bool>,
    /// `minecraft:fluid` registry ids of water (still and flowing).
    water: [u16; 2],
    /// `minecraft:fluid` registry ids of lava (still and flowing).
    lava: [u16; 2],
}

/// Blocks with own behaviour.
mod special {
    use vanillite_data::block::{BlockId, ids};

    pub const FIRE: BlockId = ids::FIRE;
    pub const SOUL_FIRE: BlockId = ids::SOUL_FIRE;
    pub const MAGMA_BLOCK: BlockId = ids::MAGMA_BLOCK;
    pub const BUBBLE_COLUMN: BlockId = ids::BUBBLE_COLUMN;
    pub const HAY_BLOCK: BlockId = ids::HAY_BLOCK;
    pub const SLIME_BLOCK: BlockId = ids::SLIME_BLOCK;
    pub const HONEY_BLOCK: BlockId = ids::HONEY_BLOCK;
    pub const POINTED_DRIPSTONE: BlockId = ids::POINTED_DRIPSTONE;
    /// Blocks whose `isSuffocating` is `always`.
    pub const ALWAYS_SUFFOCATING: [BlockId; 4] =
        [ids::FARMLAND, ids::SOUL_SAND, ids::DIRT_PATH, ids::MUD];
}

/// Blocks whose `isSuffocating` is `never` though they are in `#causes_suffocation` with a
/// full cube: glass (plain, stained and tinted), leaves, mangrove roots, copper grates and the
/// moving piston.
pub fn never_suffocating(block: BlockId) -> bool {
    let name = block.info().name;
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    matches!(
        name,
        "glass" | "tinted_glass" | "mangrove_roots" | "moving_piston"
    ) || name.ends_with("_stained_glass")
        || name.ends_with("_leaves")
        || name.ends_with("copper_grate")
}

/// `Block.getBounceRestitution`: slime blocks, beds and shelf mushrooms bounce entities that
/// land on them.
pub fn bounce_restitution(block: BlockId) -> f32 {
    use vanillite_data::block::BlockKind;
    if block == special::SLIME_BLOCK {
        1.0
    } else if matches!(
        block.info().kind,
        BlockKind::BedBlock | BlockKind::ShelfMushroomBlock
    ) {
        0.75
    } else {
        0.0
    }
}

/// How a block reacts to an entity landing on it (`Block.fallOn`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Landing {
    /// `causeFallDamage(fallDistance, multiplier, fall)`.
    Fall(f32),
    /// Slime: no damage unless the entity suppresses the bounce (sneaks).
    Slime,
    /// Honey: damage times 0.2 and the slide sound.
    Honey,
    /// A pointed dripstone tip pointing up: `causeFallDamage(fallDistance + 2.5, 2.0, stalagmite)`.
    Stalagmite,
    /// Beds and shelf mushrooms (`fallDistanceReduction` 0.5): half the fall distance.
    Bed,
    /// Farmland: living entities may trample it (`FarmlandBlock.fallOn`), then a normal fall.
    Farmland,
    /// Powder snow: no damage, only the fall sound of falls from 4 blocks.
    PowderSnow,
}

/// [`BlockFacts::without_tags`], built once: water and lava for the entities that need no
/// block tags (experience orbs, arrows, primed TNT).
pub fn fluid_facts() -> &'static BlockFacts {
    static FACTS: std::sync::OnceLock<BlockFacts> = std::sync::OnceLock::new();
    FACTS.get_or_init(BlockFacts::without_tags)
}

impl BlockFacts {
    /// Facts from the ids of three block tags and the fluid registry ids of water and lava.
    pub fn new(
        causes_suffocation: &[u32],
        climbable: &[u32],
        trapdoors: &[u32],
        water: [u16; 2],
        lava: [u16; 2],
    ) -> BlockFacts {
        let set = |ids: &[u32]| {
            let mut v = vec![false; vanillite_data::block::BLOCK_COUNT];
            for &id in ids {
                if let Some(slot) = usize::try_from(id).ok().and_then(|i| v.get_mut(i)) {
                    *slot = true;
                }
            }
            v
        };
        BlockFacts {
            causes_suffocation: set(causes_suffocation),
            climbable: set(climbable),
            trapdoor: set(trapdoors),
            water,
            lava,
        }
    }

    /// Facts without tags, for servers without data packs: water and lava by Vanilla's ids.
    pub fn without_tags() -> BlockFacts {
        let fluid = |name: &str| vanillite_data::registry::FLUID.id(name).unwrap_or(u16::MAX);
        BlockFacts::new(
            &[],
            &[],
            &[],
            [fluid("minecraft:water"), fluid("minecraft:flowing_water")],
            [fluid("minecraft:lava"), fluid("minecraft:flowing_lava")],
        )
    }

    /// [`without_tags`](Self::without_tags), built once: enough for what only needs water and
    /// lava (`EntityFluidInteraction` of entities outside the mob tick).
    pub fn fluids_only() -> &'static BlockFacts {
        static FACTS: std::sync::OnceLock<BlockFacts> = std::sync::OnceLock::new();
        FACTS.get_or_init(BlockFacts::without_tags)
    }

    fn has(v: &[bool], block: BlockId) -> bool {
        v.get(usize::from(block.0)).copied().unwrap_or(false)
    }

    /// `#minecraft:climbable`.
    pub fn is_climbable(&self, block: BlockId) -> bool {
        Self::has(&self.climbable, block)
    }

    /// `#minecraft:trapdoors`.
    pub fn is_trapdoor(&self, block: BlockId) -> bool {
        Self::has(&self.trapdoor, block)
    }

    /// Whether a fluid state is water (`FluidTags.WATER`).
    pub fn is_water(&self, fluid: FluidState) -> bool {
        !fluid.is_empty() && self.water.contains(&fluid.info().fluid)
    }

    /// Whether a fluid state is lava (`FluidTags.LAVA`).
    pub fn is_lava(&self, fluid: FluidState) -> bool {
        !fluid.is_empty() && self.lava.contains(&fluid.info().fluid)
    }

    /// `BlockState.isSuffocating`: in `#causes_suffocation` with a full collision cube, or one
    /// of the always suffocating blocks; a piston only while retracted.
    pub fn is_suffocating(&self, state: BlockState) -> bool {
        let block = state.block();
        if special::ALWAYS_SUFFOCATING.contains(&block) {
            return true;
        }
        if block == vanillite_data::block::ids::PISTON
            || block == vanillite_data::block::ids::STICKY_PISTON
        {
            return state.value("extended") == Some("false");
        }
        Self::has(&self.causes_suffocation, block)
            && state.has(flags::COLLISION_FULL_BLOCK)
            && !never_suffocating(block)
    }

    /// `Block.fallOn` of a block.
    pub fn landing(&self, state: BlockState) -> Landing {
        let block = state.block();
        if block == special::HAY_BLOCK {
            Landing::Fall(0.2)
        } else if block == special::SLIME_BLOCK {
            Landing::Slime
        } else if block == special::HONEY_BLOCK {
            Landing::Honey
        } else if block == special::POINTED_DRIPSTONE
            && state.value("vertical_direction") == Some("up")
            && state.value("thickness") == Some("tip")
        {
            Landing::Stalagmite
        } else if block == vanillite_data::block::ids::FARMLAND {
            Landing::Farmland
        } else if block == vanillite_data::block::ids::POWDER_SNOW {
            Landing::PowderSnow
        } else if bounce_restitution(block) > 0.0 && block != special::SLIME_BLOCK {
            Landing::Bed
        } else {
            Landing::Fall(1.0)
        }
    }

    /// `MagmaBlock.stepOn` hurts.
    pub fn is_hot_floor(&self, state: BlockState) -> bool {
        state.block() == special::MAGMA_BLOCK
    }

    /// The fire damage of a fire block (`BaseFireBlock.fireDamage`).
    pub fn fire_damage(&self, state: BlockState) -> Option<f32> {
        match state.block() {
            special::FIRE => Some(1.0),
            special::SOUL_FIRE => Some(2.0),
            _ => None,
        }
    }

    /// `Blocks.BUBBLE_COLUMN`.
    pub fn is_bubble_column(&self, state: BlockState) -> bool {
        state.block() == special::BUBBLE_COLUMN
    }
}

/// `FluidState.getHeight`: full when the same fluid is above, else its own height.
pub(super) fn fluid_height(
    blocks: &dyn BlockAccess,
    facts: &BlockFacts,
    pos: BlockPos,
    fluid: FluidState,
) -> f32 {
    let above = blocks
        .block_state(BlockPos::new(pos.x, pos.y.saturating_add(1), pos.z))
        .map_or(FluidState::EMPTY, BlockState::fluid_state);
    let same = (facts.is_water(fluid) && facts.is_water(above))
        || (facts.is_lava(fluid) && facts.is_lava(above));
    if same { 1.0 } else { fluid.info().own_height }
}

/// `FluidState.getHeightForCamera`: a source under a sturdy bottom face counts as full.
fn fluid_height_for_camera(
    blocks: &dyn BlockAccess,
    facts: &BlockFacts,
    pos: BlockPos,
    fluid: FluidState,
) -> f32 {
    if fluid.info().source {
        let above = blocks.block_state(BlockPos::new(pos.x, pos.y.saturating_add(1), pos.z));
        // Bit 0 of `sturdy_full` is the down face.
        if above.is_some_and(|s| s.misc().sturdy_full & 1 != 0) {
            return 1.0;
        }
    }
    fluid_height(blocks, facts, pos, fluid)
}

/// The result of `EntityFluidInteraction.update` for water and lava.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FluidContact {
    /// Height of the water above the box bottom (`getFluidHeight(WATER)`).
    pub water_height: f64,
    pub lava_height: f64,
    pub eye_in_water: bool,
    pub eye_in_lava: bool,
}

impl FluidContact {
    /// `isInFluid(WATER)`.
    pub fn in_water(&self) -> bool {
        self.water_height > 0.0
    }

    /// `isInFluid(LAVA)`.
    pub fn in_lava(&self) -> bool {
        self.lava_height > 0.0
    }
}

fn floor(v: f64) -> i32 {
    vanillite_base::math::floor(v)
}

/// `Mth.ceil`.
fn ceil(v: f64) -> i32 {
    let f = floor(v);
    if f64::from(f) < v {
        f.saturating_add(1)
    } else {
        f
    }
}

/// `EntityFluidInteraction.update` for an entity with bounding box `aabb` and eyes at
/// `eye`. Unloaded chunks around the box mean no contact.
pub fn fluid_contact(
    blocks: &dyn BlockAccess,
    facts: &BlockFacts,
    aabb: Aabb,
    eye: Vec3,
) -> FluidContact {
    let mut out = FluidContact::default();
    let b = Aabb {
        min_x: aabb.min_x + 0.001,
        min_y: aabb.min_y + 0.001,
        min_z: aabb.min_z + 0.001,
        max_x: aabb.max_x - 0.001,
        max_y: aabb.max_y - 0.001,
        max_z: aabb.max_z - 0.001,
    };
    let (x0, y0, z0) = (floor(b.min_x), floor(b.min_y), floor(b.min_z));
    let (x1, y1, z1) = (
        ceil(b.max_x).saturating_sub(1),
        ceil(b.max_y).saturating_sub(1),
        ceil(b.max_z).saturating_sub(1),
    );
    let (eye_x, eye_z) = (floor(eye.x), floor(eye.z));
    for x in x0..=x1 {
        for y in y0..=y1 {
            for z in z0..=z1 {
                let pos = BlockPos::new(x, y, z);
                let Some(state) = blocks.block_state(pos) else {
                    return FluidContact::default();
                };
                let fluid = state.fluid_state();
                if fluid.is_empty() {
                    continue;
                }
                let water = facts.is_water(fluid);
                let lava = facts.is_lava(fluid);
                if !water && !lava {
                    continue;
                }
                let bottom = f64::from(y);
                let top = bottom + f64::from(fluid_height(blocks, facts, pos, fluid));
                if top < b.min_y {
                    continue;
                }
                let eyes = x == eye_x
                    && z == eye_z
                    && eye.y >= bottom
                    && eye.y
                        <= bottom + f64::from(fluid_height_for_camera(blocks, facts, pos, fluid));
                let height = top - aabb.min_y;
                if water {
                    out.water_height = out.water_height.max(height);
                    out.eye_in_water |= eyes;
                } else {
                    out.lava_height = out.lava_height.max(height);
                    out.eye_in_lava |= eyes;
                }
            }
        }
    }
    out
}

/// The block cells a box touches (`BlockPos.betweenClosed` of its floored corners).
fn cells(aabb: Aabb) -> impl Iterator<Item = BlockPos> {
    let (x0, y0, z0) = (floor(aabb.min_x), floor(aabb.min_y), floor(aabb.min_z));
    let (x1, y1, z1) = (floor(aabb.max_x), floor(aabb.max_y), floor(aabb.max_z));
    (x0..=x1).flat_map(move |x| {
        (y0..=y1).flat_map(move |y| (z0..=z1).map(move |z| BlockPos::new(x, y, z)))
    })
}

/// Whether one of the collision boxes of `state` at `pos` intersects `aabb`.
fn collides(state: BlockState, pos: BlockPos, aabb: &Aabb) -> bool {
    let (px, py, pz) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    state.collision_shape().iter().any(|b| {
        let shape = Aabb {
            min_x: px + b[0],
            min_y: py + b[1],
            min_z: pz + b[2],
            max_x: px + b[3],
            max_y: py + b[4],
            max_z: pz + b[5],
        };
        shape.intersects(aabb)
    })
}

/// `Entity.isInWall` for an entity of `width` with eyes at `eye`.
pub fn is_in_wall(blocks: &dyn BlockAccess, facts: &BlockFacts, eye: Vec3, width: f32) -> bool {
    let half = f64::from(width * 0.8) / 2.0;
    let eye_box = Aabb {
        min_x: eye.x - half,
        min_y: eye.y - 5.0e-7,
        min_z: eye.z - half,
        max_x: eye.x + half,
        max_y: eye.y + 5.0e-7,
        max_z: eye.z + half,
    };
    cells(eye_box).any(|pos| {
        blocks.block_state(pos).is_some_and(|state| {
            !state.is_air() && facts.is_suffocating(state) && collides(state, pos, &eye_box)
        })
    })
}

/// The effects one step of the way collected (`InsideBlockEffectApplier.StepBasedCollector`).
/// They apply in this order: `FREEZE`, `CLEAR_FREEZE`, `FIRE_IGNITE` and the fire damage,
/// `LAVA_IGNITE` and `lavaHurt`, what runs before `EXTINGUISH` (melting powder snow, the
/// cauldron), `EXTINGUISH`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StepEffects {
    /// `FREEZE` (powder snow).
    pub freeze: bool,
    /// `CLEAR_FREEZE` (fire, lava cauldrons).
    pub clear_freeze: bool,
    /// `FIRE_IGNITE` with the highest fire damage of the fire blocks touched
    /// (`BaseFireBlock.entityInside`).
    pub fire: Option<f32>,
    /// Number of fire blocks touched.
    pub fire_blocks: u8,
    /// `LAVA_IGNITE`: number of lava fluid blocks and lava cauldrons touched.
    pub lava_blocks: u8,
    /// The first powder snow block touched: a burning entity melts it before the fire goes
    /// out (`runBefore(EXTINGUISH)`).
    pub melt_snow: Option<BlockPos>,
    /// The first water or powder snow cauldron touched: a burning entity takes a level of
    /// it before the fire goes out (`LayeredCauldronBlock.entityInside`).
    pub cauldron: Option<BlockPos>,
    /// `EXTINGUISH` (water, powder snow, filled cauldrons).
    pub extinguish: bool,
}

/// What an entity touches on its way (the effects of `checkInsideBlocks`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Inside {
    /// `CactusBlock.entityInside`.
    pub cactus: bool,
    /// `SweetBerryBushBlock.entityInside`: a bush touched, and whether one of them is grown
    /// (`age != 0`, it hurts entities that move).
    pub berry_bush: Option<bool>,
    /// `CampfireBlock.entityInside`: the damage of the first lit campfire (soul campfires
    /// hurt twice as much).
    pub campfire: Option<f32>,
    /// `WitherRoseBlock.entityInside`.
    pub wither_rose: bool,
    /// `EyeblossomBlock.entityInside`: an eyeblossom that attracts bees (`Bee.attractsBees`).
    pub eyeblossom: bool,
    /// `PowderSnowBlock.entityInside`: the first powder snow block touched.
    pub powder_snow: Option<BlockPos>,
    /// `WebBlock.entityInside`.
    pub cobweb: bool,
    steps: [StepEffects; Inside::STEPS],
    step_count: u8,
}

impl Inside {
    /// Steps with effects kept apart; the effects of later ones join the last.
    const STEPS: usize = 4;

    /// The collected effects, step by step in the order they apply.
    pub fn steps(&self) -> &[StepEffects] {
        &self.steps[..usize::from(self.step_count)]
    }

    /// Fire or lava touched on the way.
    pub fn burns(&self) -> bool {
        self.steps()
            .iter()
            .any(|s| s.fire.is_some() || s.lava_blocks > 0)
    }

    /// `flushStep`: the effects of a finished step.
    fn flush(&mut self, step: StepEffects) {
        if step == StepEffects::default() {
            return;
        }
        let count = usize::from(self.step_count);
        if count < Inside::STEPS {
            self.steps[count] = step;
            self.step_count += 1;
            return;
        }
        let last = &mut self.steps[Inside::STEPS - 1];
        last.freeze |= step.freeze;
        last.clear_freeze |= step.clear_freeze;
        last.fire = match (last.fire, step.fire) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        last.fire_blocks = last.fire_blocks.saturating_add(step.fire_blocks);
        last.lava_blocks = last.lava_blocks.saturating_add(step.lava_blocks);
        last.melt_snow = last.melt_snow.or(step.melt_snow);
        last.cauldron = last.cauldron.or(step.cauldron);
        last.extinguish |= step.extinguish;
    }
}

/// The blocks an entity of the size `dims` is inside on the movements of a tick
/// ([`super::inside`]), each block once; `each` sees every block whose `entityInside` runs.
pub fn inside_blocks(
    blocks: &dyn BlockAccess,
    facts: &BlockFacts,
    dims: vanillite_world::entity::Dimensions,
    fast_fall: bool,
    movements: &[super::inside::Movement],
    mut each: impl FnMut(&super::inside::Cell),
) -> Inside {
    let walker = super::inside::Walker {
        blocks,
        facts: Some(facts),
        dims,
        fast_fall,
    };
    let mut out = Inside::default();
    let mut step = StepEffects::default();
    // `advanceStep`: effects of one step apply together.
    let mut last_step = -1;
    walker.walk(movements, |cell| {
        if cell.step != last_step {
            last_step = cell.step;
            out.flush(std::mem::take(&mut step));
        }
        let (state, pos) = (cell.state, cell.pos);
        if cell.in_block {
            let block = state.block();
            if block == vanillite_data::block::ids::LAVA_CAULDRON {
                // `LavaCauldronBlock.entityInside`: `LAVA_IGNITE` and `lavaHurt`.
                step.clear_freeze = true;
                step.lava_blocks = step.lava_blocks.saturating_add(1);
            } else if block == vanillite_data::block::ids::WATER_CAULDRON
                || block == vanillite_data::block::ids::POWDER_SNOW_CAULDRON
            {
                step.extinguish = true;
                step.cauldron.get_or_insert(pos);
            }
            match block.info().kind {
                vanillite_data::block::BlockKind::CactusBlock => out.cactus = true,
                vanillite_data::block::BlockKind::SweetBerryBushBlock => {
                    let grown = state.int("age").unwrap_or(0) != 0;
                    out.berry_bush = Some(out.berry_bush.unwrap_or(false) || grown);
                }
                vanillite_data::block::BlockKind::CampfireBlock if state.flag("lit") => {
                    let damage = if block == vanillite_data::block::ids::SOUL_CAMPFIRE {
                        2.0
                    } else {
                        1.0
                    };
                    out.campfire.get_or_insert(damage);
                }
                vanillite_data::block::BlockKind::WitherRoseBlock => out.wither_rose = true,
                vanillite_data::block::BlockKind::EyeblossomBlock => {
                    out.eyeblossom |= crate::mob::fam::wildlife::bee::attracts_bees(state);
                }
                vanillite_data::block::BlockKind::PowderSnowBlock => {
                    out.powder_snow.get_or_insert(pos);
                    step.melt_snow.get_or_insert(pos);
                    step.freeze = true;
                    step.extinguish = true;
                }
                vanillite_data::block::BlockKind::WebBlock => out.cobweb = true,
                _ => {}
            }
            if let Some(damage) = facts.fire_damage(state) {
                step.clear_freeze = true;
                step.fire = Some(step.fire.map_or(damage, |d| d.max(damage)));
                step.fire_blocks = step.fire_blocks.saturating_add(1);
            }
            each(cell);
        }
        if cell.in_fluid {
            let fluid = state.fluid_state();
            if facts.is_lava(fluid) {
                // `LavaFluid.entityInside`.
                step.clear_freeze = true;
                step.lava_blocks = step.lava_blocks.saturating_add(1);
            } else if facts.is_water(fluid) {
                step.extinguish = true;
            }
        }
        true
    });
    out.flush(step);
    out
}

/// The first portal block of each kind an entity's box touches on its way (`entityInside` of
/// the portal blocks): nether portals and End gateways fill their block, End portals only the
/// slab between 6 and 12 pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InsidePortals {
    pub nether: Option<BlockPos>,
    pub end: Option<BlockPos>,
    pub gateway: Option<BlockPos>,
}

/// The portal blocks inside `swept`, the box an entity covered on its way this tick.
pub fn inside_portals(blocks: &dyn BlockAccess, swept: Aabb) -> InsidePortals {
    use vanillite_data::block::ids;
    let b = Aabb {
        min_x: swept.min_x + 1.0e-5,
        min_y: swept.min_y + 1.0e-5,
        min_z: swept.min_z + 1.0e-5,
        max_x: swept.max_x - 1.0e-5,
        max_y: swept.max_y - 1.0e-5,
        max_z: swept.max_z - 1.0e-5,
    };
    let mut out = InsidePortals::default();
    for pos in cells(b) {
        let Some(state) = blocks.block_state(pos) else {
            continue;
        };
        let block = state.block();
        if block == ids::NETHER_PORTAL {
            out.nether.get_or_insert(pos);
        } else if block == ids::END_GATEWAY {
            out.gateway.get_or_insert(pos);
        } else if block == ids::END_PORTAL {
            let y = f64::from(pos.y);
            if b.min_y < y + 0.75 && b.max_y > y + 0.375 {
                out.end.get_or_insert(pos);
            }
        }
    }
    out
}
