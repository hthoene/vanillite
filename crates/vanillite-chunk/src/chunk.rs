//! A chunk column: sections, light and heightmaps.
//!
//! Sections are `Arc`s: a worker snapshots a section by cloning the `Arc`, a write copies only if
//! a snapshot still holds the old version. Block entities, ticks and entities live in the world.

use std::sync::Arc;

use crate::facts::{self, BiomeId, BlockStateId};
use crate::heightmap::{Heightmap, HeightmapKind, Heightmaps};
use crate::light::ChunkLight;
use crate::section::Section;

/// Highest number of sections a dimension can have (4064 blocks).
pub const MAX_SECTIONS: usize = 254;

/// One chunk column.
#[derive(Debug, Clone)]
pub struct Chunk {
    min_section: i32,
    sections: Box<[Arc<Section>]>,
    light: ChunkLight,
    heightmaps: Heightmaps,
    hot: HotSections,
}

impl PartialEq for Chunk {
    fn eq(&self, other: &Self) -> bool {
        // `hot` is a cache of the sections.
        self.min_section == other.min_section
            && self.sections == other.sections
            && self.light == other.light
            && self.heightmaps == other.heightmaps
    }
}

impl Eq for Chunk {}

/// What the tick asks about every section, kept in the chunk itself so a scan over the
/// sections does not visit each one: bit `i` is section `i` (the lowest 64 sections; above,
/// every section is "maybe ticking" and "not known to be solid"). A set `maybe_ticking` bit
/// may be stale-true and a clear `solid` bit stale-false (after [`Chunk::section_mut`]), never
/// the other way round.
#[derive(Debug, Clone, Copy, Default)]
struct HotSections {
    /// The section may tick randomly (`isRandomlyTicking`).
    maybe_ticking: u64,
    /// Every block of the section is a redstone conductor.
    solid: u64,
}

impl HotSections {
    fn of(sections: &[Arc<Section>]) -> HotSections {
        let mut hot = HotSections::default();
        for (index, section) in sections.iter().enumerate() {
            hot.refresh(index, section);
        }
        hot
    }

    #[inline]
    fn refresh(&mut self, index: usize, section: &Section) {
        if index >= 64 {
            return;
        }
        let bit = 1u64 << index;
        let summary = section.summary();
        self.maybe_ticking = (self.maybe_ticking & !bit)
            | if summary.is_randomly_ticking() {
                bit
            } else {
                0
            };
        self.solid = (self.solid & !bit) | if summary.all_conductors() { bit } else { 0 };
    }

    /// After a write the summary of `index` is not followed: assume the worst.
    #[inline]
    fn forget(&mut self, index: usize) {
        if index < 64 {
            let bit = 1u64 << index;
            self.maybe_ticking |= bit;
            self.solid &= !bit;
        }
    }
}

impl Chunk {
    /// Heap bytes of the chunk: sections, light and heightmaps (a section shared between
    /// chunks counts in each; `/vanillite memory`).
    pub fn heap_bytes(&self) -> usize {
        self.sections.len() * size_of::<Arc<Section>>()
            + self.sections.iter().map(|s| s.heap_bytes()).sum::<usize>()
            + self.light.heap_bytes()
            + size_of::<Heightmaps>()
            + self.heightmaps.heap_bytes()
    }

    /// An empty chunk (air, one biome) from `min_y` with `height` blocks; both multiples of 16.
    /// It keeps the heightmaps of a live chunk. All empty sections share one allocation until
    /// written.
    pub fn new(min_y: i32, height: u32, biome: BiomeId, air: BlockStateId) -> Self {
        assert!(
            min_y % 16 == 0 && height.is_multiple_of(16),
            "chunk bounds must be section aligned"
        );
        let count = (height / 16) as usize;
        assert!(count <= MAX_SECTIONS, "too many sections");
        let empty = Arc::new(Section::new(air, biome));
        let sections = vec![empty; count].into_boxed_slice();
        Self {
            min_section: min_y >> 4,
            hot: HotSections::of(&sections),
            sections,
            light: ChunkLight::new(count),
            heightmaps: Heightmaps::new(height, &HeightmapKind::LIVE),
        }
    }

    /// A chunk from parts; the heightmaps are primed from the blocks.
    pub fn from_sections(min_section: i32, sections: Vec<Arc<Section>>, light: ChunkLight) -> Self {
        Self::from_stored(min_section, sections, light, Default::default())
    }

    /// A chunk from parts with stored heightmaps by type id (`ChunkAccess.setHeightmap`, as
    /// chunk loading does): each kept type takes its stored heightmap, the others are primed
    /// from the blocks. Stored heightmaps of other sizes are ignored.
    pub fn from_stored(
        min_section: i32,
        sections: Vec<Arc<Section>>,
        light: ChunkLight,
        mut stored: [Option<Heightmap>; 6],
    ) -> Self {
        assert!(sections.len() <= MAX_SECTIONS, "too many sections");
        assert_eq!(
            light.len(),
            sections.len() + 2,
            "light needs sections + 2 layers"
        );
        let height = (sections.len() * 16) as u32;
        let mut chunk = Self {
            min_section,
            hot: HotSections::of(&sections),
            sections: sections.into_boxed_slice(),
            light,
            heightmaps: Heightmaps::new(height, &HeightmapKind::LIVE),
        };
        let mut prime = [false; 6];
        for kind in HeightmapKind::LIVE {
            let slot = usize::from(kind.id());
            match (stored[slot].take(), chunk.heightmaps.get_mut(kind)) {
                (Some(map), Some(kept)) if map.bits() == kept.bits() => *kept = map,
                _ => prime[slot] = true,
            }
        }
        chunk.prime(|kind| prime[usize::from(kind.id())]);
        chunk
    }

    /// Lowest block Y.
    #[inline]
    pub fn min_y(&self) -> i32 {
        self.min_section * 16
    }

    /// Lowest section Y.
    #[inline]
    pub fn min_section(&self) -> i32 {
        self.min_section
    }

    /// Height in blocks.
    #[inline]
    pub fn height(&self) -> u32 {
        (self.sections.len() * 16) as u32
    }

    /// Sections from the bottom.
    #[inline]
    pub fn sections(&self) -> &[Arc<Section>] {
        &self.sections
    }

    /// Section `index` for writing; copies it if a snapshot shares it. Writing blocks through
    /// this skips the heightmaps; call [`prime_heightmaps`](Self::prime_heightmaps) afterwards.
    #[inline]
    pub fn section_mut(&mut self, index: usize) -> &mut Section {
        self.hot.forget(index);
        Arc::make_mut(&mut self.sections[index])
    }

    /// The first section at or above `from` that ticks randomly (`isRandomlyTicking`), found
    /// without visiting the sections that cannot.
    #[inline]
    pub fn next_randomly_ticking(&self, from: usize) -> Option<usize> {
        let mut index = from;
        while index < self.sections.len() {
            if index < 64 {
                let rest = self.hot.maybe_ticking >> index;
                if rest == 0 {
                    index = 64;
                    continue;
                }
                index += rest.trailing_zeros() as usize;
                if index >= self.sections.len() {
                    return None;
                }
            }
            if self.sections[index].summary().is_randomly_ticking() {
                return Some(index);
            }
            index += 1;
        }
        None
    }

    /// Prefetch hint: starts loading the list of sections (the part with the sections that may
    /// tick randomly).
    #[inline]
    pub fn prefetch_sections(&self) {
        let first = self.hot.maybe_ticking.trailing_zeros() as usize;
        let last = 63 - self.hot.maybe_ticking.leading_zeros().min(63) as usize;
        let sections = &self.sections[..];
        if let (Some(a), Some(b)) = (sections.get(first), sections.get(last)) {
            vanillite_base::prefetch::read(a);
            vanillite_base::prefetch::read(b);
        } else {
            vanillite_base::prefetch::read(sections.as_ptr());
        }
    }

    /// Prefetch hint: starts loading the sections that may tick randomly.
    #[inline]
    pub fn prefetch_ticking_sections(&self) {
        let mut bits = self.hot.maybe_ticking;
        while bits != 0 {
            let index = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if let Some(section) = self.sections.get(index) {
                vanillite_base::prefetch::read(Arc::as_ptr(section));
            }
        }
    }

    /// Prefetch hint ahead of natural spawning: the whole list of sections and a heightmap.
    #[inline]
    pub fn prefetch_sections_and(&self, heightmap: HeightmapKind) {
        for section in self.sections.iter().step_by(8).chain(self.sections.last()) {
            vanillite_base::prefetch::read(section);
        }
        if let Some(map) = self.heightmaps.get(heightmap) {
            map.prefetch();
        }
    }

    /// Prefetch hint: starts loading section `index` (`index` may be out of range).
    #[inline]
    pub fn prefetch_section(&self, index: usize) {
        if let Some(section) = self.sections.get(index) {
            vanillite_base::prefetch::read(Arc::as_ptr(section));
        }
    }

    /// Prefetch hint for the block at local x/z (0..16) and world y, its section already
    /// loaded (see [`Self::prefetch_section`]).
    #[inline]
    pub fn prefetch_block(&self, x: u32, y: i32, z: u32) {
        if let Some(index) = self.section_index(y) {
            self.sections[index].prefetch_block(x, (y & 15) as u32, z);
        }
    }

    /// Whether every block of section `index` is known to be a redstone conductor without
    /// visiting it (`false` = not known; read the block).
    #[inline]
    pub fn section_all_conductors(&self, index: usize) -> bool {
        index < 64 && self.hot.solid & (1u64 << index) != 0
    }

    /// Light.
    #[inline]
    pub fn light(&self) -> &ChunkLight {
        &self.light
    }

    /// Light, mutable.
    #[inline]
    pub fn light_mut(&mut self) -> &mut ChunkLight {
        &mut self.light
    }

    /// Heightmaps.
    #[inline]
    pub fn heightmaps(&self) -> &Heightmaps {
        &self.heightmaps
    }

    /// Heightmaps, mutable.
    #[inline]
    pub fn heightmaps_mut(&mut self) -> &mut Heightmaps {
        &mut self.heightmaps
    }

    #[inline]
    fn section_index(&self, y: i32) -> Option<usize> {
        usize::try_from((y >> 4) - self.min_section)
            .ok()
            .filter(|&index| index < self.sections.len())
    }

    /// Block state at local x/z (0..16) and world y; `None` outside the chunk's height.
    #[inline]
    pub fn block(&self, x: u32, y: i32, z: u32) -> Option<BlockStateId> {
        let index = self.section_index(y)?;
        Some(self.sections[index].block(x, (y & 15) as u32, z))
    }

    /// Sets a block state at local x/z and world y like `LevelChunk.setBlockState`: summary and
    /// heightmaps follow. Returns the previous state, or `None` if nothing changed or y is outside.
    pub fn set_block(
        &mut self,
        x: u32,
        y: i32,
        z: u32,
        state: BlockStateId,
    ) -> Option<BlockStateId> {
        let index = self.section_index(y)?;
        let local_y = (y & 15) as u32;
        let section = &self.sections[index];
        let state_flags = facts::block_flags(state);
        if (section.summary().is_empty() && state_flags & facts::AIR != 0)
            || section.block(x, local_y, z) == state
        {
            return None;
        }
        let section = Arc::make_mut(&mut self.sections[index]);
        let old = section.set_block(x, local_y, z, state);
        self.hot.refresh(index, section);
        for kind in HeightmapKind::LIVE {
            self.update_heightmap(kind, x, y, z, state_flags);
        }
        Some(old)
    }

    /// `Heightmap.update` for one column after a change at `y`.
    fn update_heightmap(&mut self, kind: HeightmapKind, x: u32, y: i32, z: u32, flags: u8) {
        let min_y = self.min_y();
        let Some(map) = self.heightmaps.get(kind) else {
            return;
        };
        let first_available = map.get(x, z) as i32 + min_y;
        if y <= first_available - 2 {
            return;
        }
        let new_height = if kind.matches(flags) {
            if y < first_available {
                return;
            }
            y + 1
        } else if first_available - 1 == y {
            self.highest_matching(kind, x, z, y - 1)
                .map_or(min_y, |found| found + 1)
        } else {
            return;
        };
        if let Some(map) = self.heightmaps.get_mut(kind) {
            map.set(x, z, (new_height - min_y) as u32);
        }
    }

    /// Highest Y at or below `from` in column x/z whose block matches `kind`; skips empty sections.
    fn highest_matching(&self, kind: HeightmapKind, x: u32, z: u32, from: i32) -> Option<i32> {
        let top = self.section_index(from.min(self.min_y() + self.height() as i32 - 1))?;
        for index in (0..=top).rev() {
            let section = &self.sections[index];
            if section.summary().is_empty() {
                continue;
            }
            let base = (self.min_section + index as i32) * 16;
            let start = if index == top {
                (from - base) as u32
            } else {
                15
            };
            for local_y in (0..=start).rev() {
                if kind.matches(facts::block_flags(section.block(x, local_y, z))) {
                    return Some(base + local_y as i32);
                }
            }
        }
        None
    }

    /// Recomputes all kept heightmaps from the blocks (`Heightmap.primeHeightmaps`).
    pub fn prime_heightmaps(&mut self) {
        self.prime(|_| true);
    }

    /// Recomputes the kept heightmaps of the types `wanted` selects: one top-down scan per
    /// column finds the highest matching block of every type at once.
    fn prime(&mut self, wanted: impl Fn(HeightmapKind) -> bool) {
        let mut kinds = [HeightmapKind::WorldSurface; 6];
        let mut count = 0;
        for kind in HeightmapKind::ALL {
            if self.heightmaps.get(kind).is_some() && wanted(kind) {
                kinds[count] = kind;
                count += 1;
            }
        }
        let kinds = &kinds[..count];
        if kinds.is_empty() {
            return;
        }
        // Relative heights per type and column (`x + z * 16`); 0 is "no matching block".
        let mut heights = [[0u32; 256]; 6];
        // Sections above the highest one with a block cannot match (none in a new chunk).
        let filled = self
            .sections
            .iter()
            .rposition(|section| !section.summary().is_empty())
            .map_or(0, |top| top + 1);
        for z in 0..16 {
            for x in 0..16 {
                let column = (x + z * 16) as usize;
                let mut found = [false; 6];
                let mut remaining = kinds.len();
                'column: for (index, section) in self.sections[..filled].iter().enumerate().rev() {
                    if section.summary().is_empty() {
                        continue;
                    }
                    for local_y in (0..16).rev() {
                        let flags = facts::block_flags(section.block(x, local_y, z));
                        for (i, kind) in kinds.iter().enumerate() {
                            if !found[i] && kind.matches(flags) {
                                found[i] = true;
                                heights[i][column] = (index * 16) as u32 + local_y + 1;
                                remaining -= 1;
                            }
                        }
                        if remaining == 0 {
                            break 'column;
                        }
                    }
                }
            }
        }
        for (i, &kind) in kinds.iter().enumerate() {
            if let Some(map) = self.heightmaps.get_mut(kind) {
                for z in 0..16 {
                    for x in 0..16 {
                        map.set(x, z, heights[i][(x + z * 16) as usize]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, AIR, FLOWER, LEAVES, STONE, WATER};

    fn height(chunk: &Chunk, kind: HeightmapKind, x: u32, z: u32) -> i32 {
        chunk.heightmaps().get(kind).unwrap().get(x, z) as i32 + chunk.min_y()
    }

    #[test]
    fn heightmaps_follow_changes_and_match_priming() {
        test_support::install();
        let mut chunk = Chunk::new(-64, 384, 0, AIR);
        let shared = Arc::clone(&chunk.sections()[0]);
        assert_eq!(chunk.set_block(1, -64, 1, AIR), None);
        assert_eq!(chunk.set_block(1, 10, 1, STONE), Some(AIR));
        assert_eq!(chunk.set_block(1, 10, 1, STONE), None);
        chunk.set_block(1, 11, 1, WATER);
        chunk.set_block(1, 12, 1, LEAVES);
        chunk.set_block(1, 13, 1, FLOWER);
        assert_eq!(height(&chunk, HeightmapKind::WorldSurface, 1, 1), 14);
        assert_eq!(height(&chunk, HeightmapKind::MotionBlocking, 1, 1), 13);
        assert_eq!(
            height(&chunk, HeightmapKind::MotionBlockingNoLeaves, 1, 1),
            12
        );
        assert_eq!(height(&chunk, HeightmapKind::OceanFloor, 1, 1), 13);
        assert_eq!(height(&chunk, HeightmapKind::WorldSurface, 0, 0), -64);

        chunk.set_block(1, 13, 1, AIR);
        chunk.set_block(1, 12, 1, AIR);
        chunk.set_block(1, 11, 1, AIR);
        assert_eq!(height(&chunk, HeightmapKind::WorldSurface, 1, 1), 11);
        chunk.set_block(1, 10, 1, AIR);
        assert_eq!(height(&chunk, HeightmapKind::WorldSurface, 1, 1), -64);

        chunk.set_block(3, 200, 4, STONE);
        chunk.set_block(3, -30, 4, WATER);
        chunk.set_block(3, 200, 4, AIR);
        let primed = {
            let mut copy = chunk.clone();
            copy.prime_heightmaps();
            copy
        };
        assert_eq!(primed.heightmaps(), chunk.heightmaps());
        assert_eq!(height(&chunk, HeightmapKind::MotionBlocking, 3, 4), -29);
        assert_eq!(height(&chunk, HeightmapKind::OceanFloor, 3, 4), -64);
        // Writes copied the shared empty section instead of changing it.
        assert!(shared.summary().is_empty());
        assert_eq!(chunk.block(1, 400, 1), None);
    }
}
