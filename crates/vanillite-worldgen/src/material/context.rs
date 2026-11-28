//! `MaterialRuleContext`: the state material rules read while the surface is built, computed
//! lazily per column and per block, and the per-chunk "compiled" state of the rule tree.

use vanillite_chunk::BiomeId;

use super::rule::{FoldedRules, StoneDepth};
use super::system::MaterialSystem;
use crate::anchor::{VerticalAnchor, WorldGenerationContext};
use crate::biome::BiomeRegistry;
use crate::density::{Program, SamplerContext};
use crate::mth::floor_f;
use crate::volume::DensityVolume;
use vanillite_base::math;

/// `HOW_FAR_BELOW_PRELIMINARY_SURFACE_LEVEL_TO_BUILD_SURFACE`.
const BELOW_PRELIMINARY_SURFACE: i32 = 8;

/// One evaluation context of the rule tree (Vanilla creates one per `buildSurface` and per
/// `topMaterial` call). The update counters count from 1 with the first update, so cache stamps
/// of 0 mean "never computed".
pub(super) struct MaterialRuleContext<'a> {
    pub(super) system: &'a MaterialSystem,
    program: &'a Program,
    samplers: &'a mut SamplerContext,
    pub(super) biomes: &'a BiomeRegistry,
    biome_at: &'a dyn Fn(i32, i32, i32) -> BiomeId,
    generation: WorldGenerationContext,
    /// The volume ore vein densities are prefilled on.
    expected_volume: DensityVolume,
    /// The `y = 0` layer of `expected_volume` the preliminary surface level is sampled on.
    preliminary_surface_volume: DensityVolume,
    /// The sampled layer; empty until the first `min_surface_level` call.
    preliminary_surface: Vec<f32>,
    /// Per `BiomeCondition`: the constant it compiled to for this chunk, if any.
    pub(super) biome_folds: Vec<Option<bool>>,
    /// The rule tree folded with `biome_folds`.
    pub(super) rules: &'a FoldedRules,
    /// Per ore vein: the prefilled density and richness buffers.
    ore_densities: Vec<OreBuffers>,
    /// See [`Self::ore_veins_present`].
    ore_present: u32,
    /// Per noise slot: update stamp and value.
    pub(super) noise_values: Vec<(u64, f64)>,
    pub(super) last_update_xz: u64,
    pub(super) last_update_y: u64,
    pub(super) block_x: i32,
    pub(super) block_z: i32,
    pub(super) surface_gradient_x: i32,
    pub(super) surface_gradient_z: i32,
    pub(super) surface_depth: i32,
    surface_secondary_update: u64,
    surface_secondary: f64,
    min_surface_level_update: u64,
    min_surface_level: i32,
    biome: Option<BiomeId>,
    pub(super) block_y: i32,
    /// Index of the current block in `expected_volume`, if it is inside.
    block_index: Option<usize>,
    /// Column of the current block in `expected_volume` (`x + z * size_x`), if it is inside.
    column: Option<i32>,
    pub(super) water_height: i32,
    pub(super) stone_depth_below: i32,
    pub(super) stone_depth_above: i32,
}

/// The prefilled densities of one ore vein: the density (`values[0]`) and richness
/// (`values[1]`) over `volume`, the part of the expected volume where the density can be other
/// than its constant outside the vein's Y range (none when that part is empty).
struct OreBuffers {
    volume: Option<DensityVolume>,
    values: Option<[Vec<f32>; 2]>,
    /// The density is proven not to be positive anywhere in `volume` (then it is not
    /// sampled, and reads as its constant outside the vein's Y range, which is not either).
    absent: bool,
}

/// What a vein's density reads as where it is proven not to be positive (any such value has
/// the same effect: the vein places nothing).
const ABSENT_DENSITY: f32 = -1.0;

impl<'a> MaterialRuleContext<'a> {
    /// The context plus `ruleSource.compile(context)`: `rules` is the rule tree folded with
    /// `biome_folds` (`MaterialRules::biome_folds`), and ore vein densities are prefilled over
    /// `expected_volume`, in the rule tree's compile order.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn compile(
        system: &'a MaterialSystem,
        program: &'a Program,
        samplers: &'a mut SamplerContext,
        biomes: &'a BiomeRegistry,
        biome_at: &'a dyn Fn(i32, i32, i32) -> BiomeId,
        generation: WorldGenerationContext,
        expected_volume: DensityVolume,
        rules: &'a FoldedRules,
        biome_folds: Vec<Option<bool>>,
    ) -> Self {
        let ore_densities: Vec<OreBuffers> = system
            .ore_vein_fns
            .iter()
            .map(|fns| {
                let volume = fns.prefill_volume(&expected_volume, system.ore_y_period);
                // A vein whose density is proven not to be positive anywhere in the chunk
                // places nothing: its density is never above zero and its richness never
                // read, so neither is sampled (nothing else reads their cache cells).
                let absent = system.ore_cells_private
                    && volume.is_some_and(|volume| {
                        let mut scratch =
                            system.bounds_scratch.take_or(|| SamplerContext::new(true));
                        let absent = program.not_positive_in(&mut scratch, fns.density, &volume);
                        system.bounds_scratch.give(scratch);
                        absent
                    });
                let values = volume.filter(|_| !absent).map(|volume| {
                    let mut density = vec![0.0; volume.len()];
                    program.sample_volume(samplers, fns.density, &mut density, &volume);
                    // The richness is only read where the density is above zero (or NaN, which
                    // passes that test), and nothing else reads its cache cells.
                    let mask: Vec<bool> = density.iter().map(|&d| d > 0.0 || d.is_nan()).collect();
                    let mut richness = vec![0.0; volume.len()];
                    if !system.ore_cells_private {
                        program.sample_volume(samplers, fns.richness, &mut richness, &volume);
                    } else if mask.contains(&true) {
                        program.sample_volume_masked(
                            samplers,
                            fns.richness,
                            &mut richness,
                            &volume,
                            &mask,
                        );
                    }
                    [density, richness]
                });
                OreBuffers {
                    volume,
                    values,
                    absent,
                }
            })
            .collect();
        let mut ore_present = 0;
        for (vein, (buffers, fns)) in ore_densities.iter().zip(&system.ore_vein_fns).enumerate() {
            let outside = fns.y_range.map_or(0.0, |r| r.2);
            let inside = buffers.values.as_ref().is_some_and(|values| {
                values[0]
                    .iter()
                    .fold(false, |any, &d| any | (d > 0.0) | d.is_nan())
            });
            if (inside || outside.is_nan() || outside > 0.0) && vein < 32 {
                ore_present |= 1 << vein;
            }
        }
        Self {
            system,
            program,
            samplers,
            biomes,
            biome_at,
            generation,
            expected_volume,
            preliminary_surface_volume: DensityVolume::new(
                expected_volume.size_x,
                1,
                expected_volume.size_z,
                expected_volume.min_x,
                0,
                expected_volume.min_z,
            ),
            preliminary_surface: Vec::new(),
            biome_folds,
            rules,
            ore_densities,
            ore_present,
            noise_values: vec![(0, 0.0); system.rules.noises.len()],
            last_update_xz: 0,
            last_update_y: 0,
            block_x: 0,
            block_z: 0,
            surface_gradient_x: 0,
            surface_gradient_z: 0,
            surface_depth: 0,
            surface_secondary_update: 0,
            surface_secondary: 0.0,
            min_surface_level_update: 0,
            min_surface_level: 0,
            biome: None,
            block_y: 0,
            block_index: None,
            column: None,
            water_height: 0,
            stone_depth_below: 0,
            stone_depth_above: 0,
        }
    }

    /// `updateXZ`: moves to a new column.
    pub(super) fn update_xz(
        &mut self,
        block_x: i32,
        block_z: i32,
        surface_gradient_x: i32,
        surface_gradient_z: i32,
    ) {
        self.last_update_xz += 1;
        self.last_update_y += 1;
        self.block_x = block_x;
        self.block_z = block_z;
        self.surface_gradient_x = surface_gradient_x;
        self.surface_gradient_z = surface_gradient_z;
        self.surface_depth = self.system.surface_depth(block_x, block_z);
        let volume = &self.expected_volume;
        let (column_x, column_z) = (block_x - volume.min_x, block_z - volume.min_z);
        self.column = ((0..volume.size_x).contains(&column_x)
            && (0..volume.size_z).contains(&column_z))
        .then_some(column_x + column_z * volume.size_x);
    }

    /// `updateY`: moves to a new block of the column.
    pub(super) fn update_y(
        &mut self,
        stone_depth_above: i32,
        stone_depth_below: i32,
        water_height: i32,
        block_y: i32,
    ) {
        self.last_update_y += 1;
        self.biome = None;
        self.block_y = block_y;
        let volume = &self.expected_volume;
        let row = block_y.wrapping_sub(volume.min_y);
        self.block_index = match self.column {
            Some(column) if (0..volume.size_y).contains(&row) => {
                Some((row + column * volume.size_y) as usize)
            }
            _ => None,
        };
        self.water_height = water_height;
        self.stone_depth_below = stone_depth_below;
        self.stone_depth_above = stone_depth_above;
    }

    /// `getSurfaceSecondary()`, cached per column.
    pub(super) fn surface_secondary(&mut self) -> f64 {
        if self.surface_secondary_update != self.last_update_xz {
            self.surface_secondary_update = self.last_update_xz;
            self.surface_secondary = self.system.surface_secondary(self.block_x, self.block_z);
        }
        self.surface_secondary
    }

    /// `getBiome()`, cached per block.
    pub(super) fn biome(&mut self) -> BiomeId {
        match self.biome {
            Some(biome) => biome,
            None => {
                let biome = (self.biome_at)(self.block_x, self.block_y, self.block_z);
                self.biome = Some(biome);
                biome
            }
        }
    }

    /// `getMinSurfaceLevel()`: the preliminary surface level (`chunk_surface_level`) minus a
    /// margin, cached per column; the first call samples the whole layer of the chunk.
    #[inline]
    pub(super) fn min_surface_level(&mut self) -> i32 {
        if self.min_surface_level_update != self.last_update_xz {
            self.update_min_surface_level();
        }
        self.min_surface_level
    }

    /// Computes [`Self::min_surface_level`] for the current column.
    #[inline(never)]
    fn update_min_surface_level(&mut self) {
        {
            self.min_surface_level_update = self.last_update_xz;
            let function = self.system.preliminary_surface;
            let level =
                match self
                    .preliminary_surface_volume
                    .index_of_block(self.block_x, 0, self.block_z)
                {
                    Some(index) => {
                        if self.preliminary_surface.is_empty() {
                            let volume = self.preliminary_surface_volume;
                            self.preliminary_surface = vec![0.0; volume.len()];
                            self.program.sample_volume(
                                self.samplers,
                                function,
                                &mut self.preliminary_surface,
                                &volume,
                            );
                        }
                        self.preliminary_surface[index]
                    }
                    None => self.program.sample_value(
                        self.samplers,
                        function,
                        self.block_x,
                        0,
                        self.block_z,
                    ),
                };
            self.min_surface_level = floor_f(level)
                .wrapping_add(self.surface_depth)
                .wrapping_sub(BELOW_PRELIMINARY_SURFACE);
        }
    }

    /// The stone depths from which on every condition of `conditions` is false in the current
    /// column: above for floors and below for ceilings (each is true up to its own bound, as
    /// `test` computes it).
    pub(super) fn stone_depth_reach(&mut self, conditions: &[StoneDepth]) -> (i32, i32) {
        let (mut floor, mut ceiling) = (i32::MIN, i32::MIN);
        for condition in conditions {
            let surface_depth = if condition.add_surface_depth {
                self.surface_depth
            } else {
                0
            };
            let secondary_surface_depth = if condition.secondary_depth_range == 0 {
                0
            } else {
                math::map(
                    self.surface_secondary(),
                    -1.0,
                    1.0,
                    0.0,
                    f64::from(condition.secondary_depth_range),
                ) as i32
            };
            let bound = 1_i32
                .wrapping_add(condition.offset)
                .wrapping_add(surface_depth)
                .wrapping_add(secondary_surface_depth);
            let reach = if condition.ceiling {
                &mut ceiling
            } else {
                &mut floor
            };
            *reach = (*reach).max(bound);
        }
        (floor, ceiling)
    }

    /// Whether [`Self::min_surface_level`] of the current column reads the sampled layer, so
    /// calling it leaves the sampling context untouched.
    #[inline]
    pub(super) fn surface_level_is_free(&self) -> bool {
        !self.preliminary_surface.is_empty() && self.column.is_some()
    }

    /// Bit `v` set: ore vein `v` has a density above zero somewhere in the expected volume (or
    /// is not prefilled), so it may place a block in this chunk.
    pub(super) fn ore_veins_present(&self) -> u32 {
        self.ore_present
    }

    /// Whether one of the ore veins of `veins` (bit `v`: vein `v`) may place a block at `y` of
    /// the current column: its density there is not known to be `<= 0`.
    pub(super) fn ore_may_place(&self, veins: u32, y: i32) -> bool {
        let expected = &self.expected_volume;
        let Some(column) = self.column else {
            return true;
        };
        if !(0..expected.size_y).contains(&y.wrapping_sub(expected.min_y)) {
            // Sampled block by block there.
            return true;
        }
        let mut veins = veins;
        while veins != 0 {
            let vein = veins.trailing_zeros() as usize;
            veins &= veins - 1;
            let buffers = &self.ore_densities[vein];
            if buffers.absent {
                continue;
            }
            let density = match (&buffers.volume, &buffers.values) {
                (Some(volume), Some(values))
                    if (0..volume.size_y).contains(&(y - volume.min_y)) =>
                {
                    values[0][(y - volume.min_y + column * volume.size_y) as usize]
                }
                _ => self.system.ore_vein_fns[vein].y_range.map_or(0.0, |r| r.2),
            };
            // As `ore_vein` tests it: a NaN density passes.
            if density.is_nan() || density > 0.0 {
                return true;
            }
        }
        false
    }

    /// Whether `y` is a row of the expected volume (where [`Self::ore_may_place`] reads the
    /// prefilled densities).
    pub(super) fn in_expected_rows(&self, y: i32) -> bool {
        (0..self.expected_volume.size_y).contains(&y.wrapping_sub(self.expected_volume.min_y))
    }

    /// Whether no ore vein may place a block in the expected rows of the current column:
    /// [`Self::ore_may_place`] is false there for every vein.
    pub(super) fn column_ore_free(&self) -> bool {
        let Some(column) = self.column else {
            return false;
        };
        let mut veins = self.ore_present;
        while veins != 0 {
            let vein = veins.trailing_zeros() as usize;
            veins &= veins - 1;
            let buffers = &self.ore_densities[vein];
            if buffers.absent {
                continue;
            }
            let outside = self.system.ore_vein_fns[vein].y_range.map_or(0.0, |r| r.2);
            #[allow(clippy::neg_cmp_op_on_partial_ord)]
            let may_place = |density: f32| !(density <= 0.0);
            if may_place(outside) {
                return false;
            }
            if let (Some(volume), Some(values)) = (&buffers.volume, &buffers.values) {
                let start = (column * volume.size_y) as usize;
                let densities = &values[0][start..start + volume.size_y as usize];
                if densities.iter().fold(false, |any, &d| any | may_place(d)) {
                    return false;
                }
            }
        }
        true
    }

    /// `resolveAnchorY(anchor)`.
    pub(super) fn resolve_anchor_y(&self, anchor: VerticalAnchor) -> i32 {
        anchor.resolve_y(&self.generation)
    }

    /// Ore vein density (`which = 0`) or richness (`1`) at the current block
    /// (`getDensitiesInChunk(function, true)`): prefilled inside the expected volume, sampled
    /// outside it.
    pub(super) fn prefilled_density(&mut self, vein: usize, which: usize) -> f32 {
        let (x, y, z) = (self.block_x, self.block_y, self.block_z);
        match (self.block_index, self.column) {
            (Some(_), Some(column)) => {
                let buffers = &self.ore_densities[vein];
                if buffers.absent {
                    return ABSENT_DENSITY;
                }
                if let (Some(volume), Some(values)) = (&buffers.volume, &buffers.values) {
                    let row = y - volume.min_y;
                    if (0..volume.size_y).contains(&row) {
                        return values[which][(row + column * volume.size_y) as usize];
                    }
                }
                // Inside the expected volume but outside the vein's Y range: only the density
                // is read there, and it is the constant outside the range.
                self.system.ore_vein_fns[vein].y_range.map_or(0.0, |r| r.2)
            }
            _ => {
                let fns = &self.system.ore_vein_fns[vein];
                let function = if which == 0 {
                    fns.density
                } else {
                    fns.richness
                };
                self.program.sample_value(self.samplers, function, x, y, z)
            }
        }
    }

    /// Ore vein filler gap at the current block (`getDensitiesInChunk(function, false)`).
    pub(super) fn filler_gap(&mut self, vein: usize) -> f32 {
        let function = self.system.ore_vein_fns[vein].filler_gap;
        self.program.sample_value(
            self.samplers,
            function,
            self.block_x,
            self.block_y,
            self.block_z,
        )
    }
}
