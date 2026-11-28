//! `end_spike`: the obsidian pillars around the main End island.
//!
//! The End crystal on top of each pillar is stored with the chunk as entity NBT, with the
//! bedrock and fire under and in it.

use std::sync::LazyLock;

use serde_json::Value;
use vanillite_base::BlockPos;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_data::block::{BlockState, ids};

use crate::Error;
use crate::blocks::BlockSet;
use crate::feature::kinds::vegetation::util::{parse_block_pos, shuffle, with_bool};
use crate::feature::region::WorldGenRegion;
use crate::feature::{FeatureContext, FeatureParser, FeatureType, bool_or, field, int_or};

/// `EndSpikeFeature.NUMBER_OF_SPIKES`.
const NUMBER_OF_SPIKES: usize = 10;
/// `EndSpikeFeature.SPIKE_DISTANCE`.
const SPIKE_DISTANCE: f64 = 42.0;

/// `EndSpikeFeature.EndSpike`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndSpike {
    pub center_x: i32,
    pub center_z: i32,
    pub radius: i32,
    pub height: i32,
    pub guarded: bool,
}

impl EndSpike {
    fn parse(json: &Value) -> Self {
        Self {
            center_x: int_or(json, "centerX", 0),
            center_z: int_or(json, "centerZ", 0),
            radius: int_or(json, "radius", 0),
            height: int_or(json, "height", 0),
            guarded: bool_or(json, "guarded", false),
        }
    }

    /// `isCenterWithinChunk(chunkOrigin)`.
    fn is_center_within_chunk(&self, origin: BlockPos) -> bool {
        origin.x >> 4 == self.center_x >> 4 && origin.z >> 4 == self.center_z >> 4
    }
}

/// `EndSpikeFeature.getSpikesForLevel`: the ten spikes the world seed gives (Vanilla caches
/// them per seed; computing them is cheaper than a lookup here).
pub fn spikes_for_seed(seed: i64) -> [EndSpike; NUMBER_OF_SPIKES] {
    let key = LegacyRandom::new(seed).next_long() & 0xFFFF;
    // `SpikeCacheLoader.load(key)`.
    let mut sizes: [i32; NUMBER_OF_SPIKES] = std::array::from_fn(|i| i as i32);
    shuffle(&mut sizes, &mut LegacyRandom::new(key));
    std::array::from_fn(|i| {
        let angle = 2.0 * (-std::f64::consts::PI + (std::f64::consts::PI / 10.0) * i as f64);
        let size = sizes[i];
        EndSpike {
            center_x: (SPIKE_DISTANCE * crate::mth::math_cos(angle)).floor() as i32,
            center_z: (SPIKE_DISTANCE * crate::mth::math_sin(angle)).floor() as i32,
            radius: 2 + size / 3,
            height: 76 + size * 3,
            guarded: size == 1 || size == 2,
        }
    })
}

/// The iron bars cage states by `(north, south, west, east)` mask.
static IRON_BARS: LazyLock<[BlockState; 16]> = LazyLock::new(|| {
    std::array::from_fn(|mask| {
        let bars = ids::IRON_BARS.default_state();
        let bars = with_bool(bars, "north", mask & 1 != 0);
        let bars = with_bool(bars, "south", mask & 2 != 0);
        let bars = with_bool(bars, "west", mask & 4 != 0);
        with_bool(bars, "east", mask & 8 != 0)
    })
});

/// `end_spike`.
#[derive(Debug)]
pub struct EndSpikeFeature {
    /// Explicit spikes; empty means the seed's spikes.
    spikes: Vec<EndSpike>,
    /// `#minecraft:soul_fire_base_blocks` for `BaseFireBlock.getState`.
    soul_fire_base: BlockSet,
    /// `crystal_invulnerable`.
    crystal_invulnerable: bool,
    /// `crystal_beam_target`.
    crystal_beam_target: Option<BlockPos>,
}

impl FeatureType for EndSpikeFeature {
    fn parse(json: &Value, parser: &mut FeatureParser<'_>) -> Result<Self, Error> {
        let spikes = field(json, "spikes")?
            .as_array()
            .ok_or_else(|| Error::new("spikes is not a list"))?
            .iter()
            .map(EndSpike::parse)
            .collect();
        let crystal_beam_target = json
            .get("crystal_beam_target")
            .map(parse_block_pos)
            .transpose()?;
        Ok(Self {
            spikes,
            crystal_invulnerable: bool_or(json, "crystal_invulnerable", false),
            crystal_beam_target,
            soul_fire_base: parser.block_tag("minecraft:soul_fire_base_blocks"),
        })
    }

    fn place(&self, ctx: &mut FeatureContext<'_, '_>, origin: BlockPos) -> bool {
        if self.spikes.is_empty() {
            for spike in spikes_for_seed(ctx.level.seed()) {
                if spike.is_center_within_chunk(origin) {
                    self.place_spike(ctx, &spike);
                }
            }
        } else {
            for spike in &self.spikes {
                if spike.is_center_within_chunk(origin) {
                    self.place_spike(ctx, spike);
                }
            }
        }
        true
    }
}

impl EndSpikeFeature {
    /// `placeSpike`.
    fn place_spike(&self, ctx: &mut FeatureContext<'_, '_>, spike: &EndSpike) {
        let level = &mut *ctx.level;
        let radius = spike.radius;
        let (cx, cz) = (spike.center_x, spike.center_z);
        let limit = f64::from(radius * radius + 1);
        let obsidian = ids::OBSIDIAN.default_state();
        // `BlockPos.betweenClosed`: x fastest, then y, then z.
        for z in cz - radius..=cz + radius {
            for y in level.min_y()..=spike.height + 10 {
                for x in cx - radius..=cx + radius {
                    let pos = BlockPos::new(x, y, z);
                    let dx = f64::from(x) - f64::from(cx);
                    let dz = f64::from(z) - f64::from(cz);
                    if dx * dx + dz * dz <= limit && y < spike.height {
                        level.set_block_and_update(pos, obsidian);
                    } else if y > 65 {
                        level.set_block_and_update(pos, BlockState::AIR);
                    }
                }
            }
        }
        if spike.guarded {
            for dx in -2_i32..=2 {
                for dz in -2_i32..=2 {
                    for dy in 0..=3 {
                        let x_side = dx.abs() == 2;
                        let z_side = dz.abs() == 2;
                        let top = dy == 3;
                        if x_side || z_side || top {
                            let x_edge = dx == -2 || dx == 2 || top;
                            let z_edge = dz == -2 || dz == 2 || top;
                            let mask = usize::from(x_edge && dz != -2)
                                | usize::from(x_edge && dz != 2) << 1
                                | usize::from(z_edge && dx != -2) << 2
                                | usize::from(z_edge && dx != 2) << 3;
                            let pos = BlockPos::new(cx + dx, spike.height + dy, cz + dz);
                            level.set_block_and_update(pos, IRON_BARS[mask]);
                        }
                    }
                }
            }
        }
        // The crystal (`EndCrystal` snapped to the pillar's top with a random yaw).
        let yaw = ctx.random.next_float() * 360.0;
        let mut entity = crate::feature::region::entity_tag(
            "minecraft:end_crystal",
            f64::from(cx) + 0.5,
            f64::from(spike.height + 1),
            f64::from(cz) + 0.5,
        );
        entity.insert(
            "Rotation",
            vanillite_nbt::List(vec![
                vanillite_nbt::Tag::Float(yaw),
                vanillite_nbt::Tag::Float(0.0),
            ]),
        );
        entity.insert(
            "Invulnerable",
            vanillite_nbt::Tag::Byte(i8::from(self.crystal_invulnerable)),
        );
        entity.insert("ShowBottom", vanillite_nbt::Tag::Byte(1));
        if let Some(t) = self.crystal_beam_target {
            entity.insert(
                "beam_target",
                vanillite_nbt::Tag::IntArray(vec![t.x, t.y, t.z]),
            );
        }
        level.add_fresh_entity(entity);
        let crystal = BlockPos::new(cx, spike.height + 1, cz);
        level.set_block_and_update(crystal.offset(0, -1, 0), ids::BEDROCK.default_state());
        let fire = self.fire_state(level, crystal);
        level.set_block_and_update(crystal, fire);
    }

    /// `BaseFireBlock.getState(level, pos)` on the bedrock just placed below: soul fire on soul
    /// fire bases, else the plain fire state (`FireBlock.getStateForPlacement` only looks at its
    /// neighbours when the block below is neither flammable nor sturdy, and bedrock is sturdy).
    fn fire_state(&self, level: &WorldGenRegion<'_>, pos: BlockPos) -> BlockState {
        if self
            .soul_fire_base
            .contains(level.block(pos.offset(0, -1, 0)))
        {
            ids::SOUL_FIRE.default_state()
        } else {
            ids::FIRE.default_state()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spike_centres_match_vanilla() {
        let centres: Vec<(i32, i32)> = spikes_for_seed(0)
            .iter()
            .map(|s| (s.center_x, s.center_z))
            .collect();
        assert_eq!(
            centres,
            [
                (42, 0),
                (33, 24),
                (12, 39),
                (-13, 39),
                (-34, 24),
                (-42, -1),
                (-34, -25),
                (-13, -40),
                (12, -40),
                (33, -25),
            ]
        );
        let mut heights: Vec<i32> = spikes_for_seed(0).iter().map(|s| s.height).collect();
        heights.sort_unstable();
        assert_eq!(heights, (0..10).map(|s| 76 + s * 3).collect::<Vec<_>>());
    }
}
