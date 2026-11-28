//! `ChunkGenerator.findNearestMapStructure` on a world's terrain generator (`/locate
//! structure`, eyes of ender).

use vanillite_base::{BlockPos, ChunkPos};
use vanillite_worldgen::generator::TerrainGenerator;
use vanillite_worldgen::structure::placement::StructurePlacement;
use vanillite_worldgen::structure::state::StructureState;
use vanillite_worldgen::structure::{LevelHeight, StructureId, StructureSetId};

/// `LocateCommand.MAX_STRUCTURE_SEARCH_RADIUS` (the radius eyes of ender search too).
pub const MAX_STRUCTURE_SEARCH_RADIUS: i32 = 100;

/// `BlockPos.distSqr`.
fn dist_sqr(a: BlockPos, b: BlockPos) -> f64 {
    let d = |p: i32, q: i32| f64::from(p) - f64::from(q);
    let (x, y, z) = (d(a.x, b.x), d(a.y, b.y), d(a.z, b.z));
    x * x + y * y + z * z
}

/// `StructurePlacement.getLocatePos`.
fn locate_pos(placement: &StructurePlacement, chunk: ChunkPos) -> BlockPos {
    let offset = placement.spreading().map_or([0, 0, 0], |s| s.locate_offset);
    BlockPos::new(
        (chunk.x << 4).saturating_add(offset[0]),
        offset[1],
        (chunk.z << 4).saturating_add(offset[2]),
    )
}

/// `getStructureGeneratingAt`: the set's start in `chunk`, if wanted and `claim` takes it
/// (`createReference`: `checkStructurePresence` skips referenced starts, `tryAddReference`).
fn generating_at(
    terrain: &TerrainGenerator,
    state: &StructureState<'_>,
    set: StructureSetId,
    wanted: &[StructureId],
    chunk: ChunkPos,
    level: LevelHeight,
    claim: &mut dyn FnMut(ChunkPos, StructureId) -> bool,
) -> Option<(BlockPos, StructureId)> {
    let id = terrain.structure_start_in(state, set, chunk, level)?;
    (wanted.contains(&id) && claim(chunk, id)).then(|| {
        (
            locate_pos(&terrain.structures().set(set).placement, chunk),
            id,
        )
    })
}

/// `ServerLevel.findNearestMapStructure` for the structures `names` (the content of a
/// structure tag) around `origin`: `None` in a world that generates no structures.
pub fn nearest_named(
    level: &crate::level::Level,
    names: &[&str],
    origin: BlockPos,
    max_radius: i32,
) -> Option<BlockPos> {
    let g = level.generation.as_ref().filter(|g| g.structures)?;
    let terrain = g.terrain();
    let wanted: Vec<_> = names
        .iter()
        .filter_map(|id| terrain.structures().structure_id(id))
        .collect();
    let height = LevelHeight {
        min_y: level.min_y(),
        height: level
            .max_y()
            .saturating_sub(level.min_y())
            .saturating_add(1),
    };
    nearest_structure(&terrain, &wanted, origin, height, max_radius).map(|(p, _)| p)
}

/// `ChunkGenerator.findNearestMapStructure(level, structures, pos, radius, false)`.
pub fn nearest_structure(
    terrain: &TerrainGenerator,
    wanted: &[StructureId],
    origin: BlockPos,
    level: LevelHeight,
    max_radius: i32,
) -> Option<(BlockPos, StructureId)> {
    nearest_structure_claiming(terrain, wanted, origin, level, max_radius, &mut |_, _| true)
}

/// `ChunkGenerator.findNearestMapStructure(level, structures, pos, radius, createReference)`:
/// a start counts only when `claim(start chunk, structure)` takes it (with `createReference`:
/// unreferenced, and then referenced). Like Vanilla, every closer ring start found on the way
/// is claimed.
pub fn nearest_structure_claiming(
    terrain: &TerrainGenerator,
    wanted: &[StructureId],
    origin: BlockPos,
    level: LevelHeight,
    max_radius: i32,
    claim: &mut dyn FnMut(ChunkPos, StructureId) -> bool,
) -> Option<(BlockPos, StructureId)> {
    let state = StructureState::new(terrain);
    let registry = terrain.structures();
    // `getPlacementsForStructure` of the wanted structures, by set.
    let sets: Vec<StructureSetId> = state
        .possible_structure_sets()
        .iter()
        .copied()
        .filter(|&id| {
            registry
                .set(id)
                .structures
                .iter()
                .any(|(s, _)| wanted.contains(s))
        })
        .collect();
    let mut nearest: Option<(BlockPos, StructureId)> = None;
    let mut best = f64::MAX;
    let mut spreads = Vec::new();
    for &set in &sets {
        match &registry.set(set).placement {
            StructurePlacement::ConcentricRings { rings, .. } => {
                let Some(positions) = state.ring_positions_for(rings) else {
                    continue;
                };
                let mut closest: Option<((BlockPos, StructureId), f64)> = None;
                for &chunk in positions {
                    let center = BlockPos::new((chunk.x << 4) + 8, 32, (chunk.z << 4) + 8);
                    let dist = dist_sqr(center, origin);
                    if closest.as_ref().is_none_or(|(_, d)| dist < *d)
                        && let Some(found) =
                            generating_at(terrain, &state, set, wanted, chunk, level, claim)
                    {
                        closest = Some((found, dist));
                    }
                }
                if let Some((found, _)) = closest {
                    let dist = dist_sqr(origin, found.0);
                    if dist < best {
                        best = dist;
                        nearest = Some(found);
                    }
                }
            }
            StructurePlacement::RandomSpread { spacing, .. } => spreads.push((set, *spacing)),
            StructurePlacement::DimensionOrigin => {}
        }
    }
    if spreads.is_empty() {
        return nearest;
    }
    let (cx, cz) = (origin.x >> 4, origin.z >> 4);
    let seed = state.level_seed();
    for radius in 0..=max_radius {
        let mut found_something = false;
        for &(set, spacing) in &spreads {
            let placement = &registry.set(set).placement;
            let mut found = None;
            'ring: for x in -radius..=radius {
                let x_edge = x == -radius || x == radius;
                for z in -radius..=radius {
                    if !x_edge && z != -radius && z != radius {
                        continue;
                    }
                    let sector_x = cx.saturating_add(spacing.saturating_mul(x));
                    let sector_z = cz.saturating_add(spacing.saturating_mul(z));
                    let Some(chunk) = placement.potential_structure_chunk(seed, sector_x, sector_z)
                    else {
                        continue;
                    };
                    found = generating_at(terrain, &state, set, wanted, chunk, level, claim);
                    if found.is_some() {
                        break 'ring;
                    }
                }
            }
            if let Some(found) = found {
                found_something = true;
                let dist = dist_sqr(origin, found.0);
                if dist < best {
                    best = dist;
                    nearest = Some(found);
                }
            }
        }
        if found_something {
            return nearest;
        }
    }
    nearest
}
