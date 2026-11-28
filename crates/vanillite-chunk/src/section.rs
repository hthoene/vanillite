//! Chunk sections: 16³ block states, 4³ biomes and a summary.
//!
//! Vanilla: `net.minecraft.world.level.chunk.LevelChunkSection`.

use crate::container::{Biomes, BlockStates, ContainerData, PalettedContainer};
use crate::facts::{self, BiomeId, BlockStateId};
use crate::wire::{self, DecodeError};

/// Counts kept up to date on every block change, so systems can skip sections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SectionSummary {
    /// Non-air blocks (`nonEmptyBlockCount`).
    pub non_air: u16,
    /// Non-air blocks with a fluid (`fluidCount`).
    pub fluids: u16,
    /// Randomly ticking blocks (`tickingBlockCount`).
    pub ticking_blocks: u16,
    /// Randomly ticking fluids (`tickingFluidCount`).
    pub ticking_fluids: u16,
    /// Blocks with a special collision shape (not empty, not a full cube, or dynamic).
    pub special_collision: u16,
    /// Redstone conductors (`isRedstoneConductor`).
    pub conductors: u16,
}

impl SectionSummary {
    /// Summary of `count` blocks with the given flags.
    #[inline]
    fn add(&mut self, flags: u8, count: u16) {
        if flags & facts::AIR != 0 {
            return;
        }
        self.non_air = self.non_air.wrapping_add(count);
        if flags & facts::RANDOM_TICKING != 0 {
            self.ticking_blocks = self.ticking_blocks.wrapping_add(count);
        }
        if flags & facts::FLUID != 0 {
            self.fluids = self.fluids.wrapping_add(count);
            if flags & facts::FLUID_RANDOM_TICKING != 0 {
                self.ticking_fluids = self.ticking_fluids.wrapping_add(count);
            }
        }
        if flags & facts::SPECIAL_COLLISION != 0 {
            self.special_collision = self.special_collision.wrapping_add(count);
        }
        if flags & facts::CONDUCTOR != 0 {
            self.conductors = self.conductors.wrapping_add(count);
        }
    }

    #[inline]
    fn remove(&mut self, flags: u8) {
        // Subtracting one is adding `u16::MAX` in wrapping arithmetic.
        self.add(flags, u16::MAX);
    }

    /// Only air in the section (`hasOnlyAir`).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.non_air == 0
    }

    /// Every block of the section is a redstone conductor.
    #[inline]
    pub fn all_conductors(&self) -> bool {
        usize::from(self.conductors) == 4096
    }

    /// Any randomly ticking block or fluid (`isRandomlyTicking`).
    #[inline]
    pub fn is_randomly_ticking(&self) -> bool {
        self.ticking_blocks > 0 || self.ticking_fluids > 0
    }
}

/// One 16×16×16 section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    blocks: PalettedContainer<BlockStates>,
    biomes: PalettedContainer<Biomes>,
    summary: SectionSummary,
}

impl Section {
    /// Heap bytes of the section with its containers (`/vanillite memory`).
    pub fn heap_bytes(&self) -> usize {
        size_of::<Section>() + self.blocks.heap_bytes() + self.biomes.heap_bytes()
    }

    /// A section filled with one block state and one biome.
    pub fn new(block: BlockStateId, biome: BiomeId) -> Self {
        Self::from_containers(PalettedContainer::new(block), PalettedContainer::new(biome))
    }

    /// A section from stored parts: block palette and packed data as on the wire (validated
    /// like [`PalettedContainer::from_parts`]) and the biomes. The summary is counted in the
    /// same pass that checks the block indices.
    pub fn from_parts(
        bits: u8,
        palette: &[u16],
        data: &[crate::bits::Word],
        biomes: PalettedContainer<Biomes>,
    ) -> Result<Self, DecodeError> {
        let mut counts = [0u32; 256];
        let blocks = PalettedContainer::from_parts_counted(bits, palette, data, &mut counts)?;
        let mut section = Self {
            blocks,
            biomes,
            summary: SectionSummary::default(),
        };
        match section.blocks.data() {
            ContainerData::Indirect { palette, .. } => {
                for (&state, &count) in palette.iter().zip(&counts) {
                    if count > 0 {
                        section.summary.add(facts::block_flags(state), count as u16);
                    }
                }
            }
            _ => section.recalculate_summary(),
        }
        Ok(section)
    }

    /// A section from containers; the summary is computed.
    pub fn from_containers(
        blocks: PalettedContainer<BlockStates>,
        biomes: PalettedContainer<Biomes>,
    ) -> Self {
        let mut section = Self {
            blocks,
            biomes,
            summary: SectionSummary::default(),
        };
        section.recalculate_summary();
        section
    }

    /// Block states.
    #[inline]
    pub fn blocks(&self) -> &PalettedContainer<BlockStates> {
        &self.blocks
    }

    /// Biomes.
    #[inline]
    pub fn biomes(&self) -> &PalettedContainer<Biomes> {
        &self.biomes
    }

    /// Biomes, mutable (biomes do not affect the summary).
    #[inline]
    pub fn biomes_mut(&mut self) -> &mut PalettedContainer<Biomes> {
        &mut self.biomes
    }

    /// The summary.
    #[inline]
    pub fn summary(&self) -> SectionSummary {
        self.summary
    }

    /// Block state at local coordinates (0..16).
    #[inline(always)]
    pub fn block(&self, x: u32, y: u32, z: u32) -> BlockStateId {
        self.blocks.get_at(x, y, z)
    }

    /// Prefetch hint for the block at local coordinates (0..16).
    #[inline(always)]
    pub fn prefetch_block(&self, x: u32, y: u32, z: u32) {
        self.blocks.prefetch_at(x, y, z);
    }

    /// Sets a block state and returns the previous one; the summary follows.
    #[inline]
    pub fn set_block(&mut self, x: u32, y: u32, z: u32, state: BlockStateId) -> BlockStateId {
        let old = self.blocks.set_at(x, y, z, state);
        if old != state {
            let (old_flags, new_flags) = (facts::block_flags(old), facts::block_flags(state));
            if old_flags != new_flags {
                self.summary.remove(old_flags);
                self.summary.add(new_flags, 1);
            }
        }
        old
    }

    /// Fills the section with one block state.
    pub fn fill_blocks(&mut self, state: BlockStateId) {
        self.blocks.fill(state);
        self.recalculate_summary();
    }

    /// Biome at quart coordinates (0..4).
    #[inline]
    pub fn biome(&self, x: u32, y: u32, z: u32) -> BiomeId {
        self.biomes.get_at(x, y, z)
    }

    /// Recomputes the summary from the block states (`recalcBlockCounts`).
    pub fn recalculate_summary(&mut self) {
        let mut summary = SectionSummary::default();
        self.blocks.for_each_count(|state, count| {
            summary.add(facts::block_flags(state), count as u16);
        });
        self.summary = summary;
    }

    /// Bytes [`write`](Self::write) appends.
    #[inline]
    pub fn serialized_size(&self) -> usize {
        4 + self.blocks.serialized_size() + self.biomes.serialized_size()
    }

    /// Appends the wire form (`LevelChunkSection.write`).
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.summary.non_air.to_be_bytes());
        out.extend_from_slice(&self.summary.fluids.to_be_bytes());
        self.blocks.write(out);
        self.biomes.write(out);
    }

    /// Reads the wire form; the summary is recomputed, the sent counts are ignored.
    pub fn read(input: &mut &[u8]) -> Result<Self, DecodeError> {
        wire::read_array::<4>(input)?;
        let blocks = PalettedContainer::read(input)?;
        let biomes = PalettedContainer::read(input)?;
        Ok(Self::from_containers(blocks, biomes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, AIR, STONE, TICKING, WATER};

    #[test]
    fn summary_follows_changes() {
        test_support::install();
        let mut section = Section::new(AIR, 0);
        assert!(section.summary().is_empty());
        section.set_block(0, 0, 0, STONE);
        section.set_block(1, 0, 0, WATER);
        section.set_block(2, 0, 0, TICKING);
        section.set_block(2, 0, 0, TICKING);
        section.set_block(3, 0, 0, STONE);
        section.set_block(3, 0, 0, AIR);
        let summary = section.summary();
        assert_eq!(summary.non_air, 3);
        assert_eq!(summary.fluids, 1);
        assert_eq!(summary.ticking_fluids, 1);
        assert_eq!(summary.ticking_blocks, 1);
        assert!(summary.is_randomly_ticking());
        let mut copy = section.clone();
        copy.recalculate_summary();
        assert_eq!(copy.summary(), summary);
    }

    #[test]
    fn wire_form() {
        test_support::install();
        let mut out = vec![];
        Section::new(AIR, 5).write(&mut out);
        assert_eq!(out, [0, 0, 0, 0, 0, 0, 0, 5]);

        let mut section = Section::new(STONE, 1);
        section.set_block(4, 5, 6, WATER);
        out.clear();
        section.write(&mut out);
        assert_eq!(out.len(), section.serialized_size());
        assert_eq!(&out[..4], [0x10, 0x00, 0, 1]);
        let read = Section::read(&mut out.as_slice()).unwrap();
        assert_eq!(read, section);
    }
}
