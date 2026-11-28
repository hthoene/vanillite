//! Heightmaps in Vanilla's packed layout.
//!
//! Vanilla: `net.minecraft.world.level.levelgen.Heightmap`. Each of the 256 columns (index
//! `x + z * 16`) stores the first free Y above the highest matching block, minus the chunk's
//! minimum Y, in `ceil_log2(height + 1)` bits.

use crate::bits::{self, Word};
use crate::facts;

/// Heightmap types with their protocol ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HeightmapKind {
    /// Non-air blocks, during generation.
    WorldSurfaceWg = 0,
    /// Non-air blocks.
    WorldSurface = 1,
    /// Motion-blocking blocks, during generation.
    OceanFloorWg = 2,
    /// Motion-blocking blocks.
    OceanFloor = 3,
    /// Motion-blocking blocks or fluids.
    MotionBlocking = 4,
    /// Motion-blocking blocks except leaves, or fluids.
    MotionBlockingNoLeaves = 5,
}

impl HeightmapKind {
    /// All types in id order.
    pub const ALL: [Self; 6] = [
        Self::WorldSurfaceWg,
        Self::WorldSurface,
        Self::OceanFloorWg,
        Self::OceanFloor,
        Self::MotionBlocking,
        Self::MotionBlockingNoLeaves,
    ];

    /// Types a live (full) chunk keeps, in the order `LevelChunk.setBlockState` updates them.
    pub const LIVE: [Self; 4] = [
        Self::MotionBlocking,
        Self::MotionBlockingNoLeaves,
        Self::OceanFloor,
        Self::WorldSurface,
    ];

    /// Protocol id.
    #[inline]
    pub const fn id(self) -> u8 {
        self as u8
    }

    /// Sent to the client (`Usage.CLIENT`).
    #[inline]
    pub const fn sent_to_client(self) -> bool {
        matches!(
            self,
            Self::WorldSurface | Self::MotionBlocking | Self::MotionBlockingNoLeaves
        )
    }

    /// Whether a block with these flags counts for this heightmap (`isOpaque` predicate).
    #[inline]
    pub const fn matches(self, flags: u8) -> bool {
        match self {
            Self::WorldSurfaceWg | Self::WorldSurface => flags & facts::AIR == 0,
            Self::OceanFloorWg | Self::OceanFloor => flags & facts::BLOCKS_MOTION != 0,
            Self::MotionBlocking => flags & (facts::BLOCKS_MOTION | facts::FLUID) != 0,
            Self::MotionBlockingNoLeaves => {
                flags & (facts::BLOCKS_MOTION_NO_LEAVES | facts::FLUID) != 0
            }
        }
    }
}

/// One heightmap: 256 heights relative to the chunk's minimum Y.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heightmap {
    bits: u8,
    data: Box<[Word]>,
}

impl Heightmap {
    /// A heightmap for a chunk of `height` blocks, all columns at the minimum Y.
    pub fn new(height: u32) -> Self {
        let bits = facts::ceil_log2(height + 1);
        Self {
            bits,
            data: bits::zeroed(256, bits),
        }
    }

    /// A heightmap from stored packed words (`Heightmap.setRawData`) for a chunk of `height`
    /// blocks; `None` if the word count does not match or a height lies above the chunk.
    pub fn from_raw(height: u32, words: &[Word]) -> Option<Self> {
        let mut map = Self::new(height);
        if words.len() != map.data.len() {
            return None;
        }
        map.data.copy_from_slice(words);
        (0..256)
            .all(|i| bits::get(&map.data, map.bits, i) <= height)
            .then_some(map)
    }

    /// Bits per entry.
    #[inline]
    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// Packed words, big-endian (`getRawData`).
    #[inline]
    pub fn raw(&self) -> &[Word] {
        &self.data
    }

    /// Height of a column relative to the minimum Y.
    #[inline]
    pub fn get(&self, x: u32, z: u32) -> u32 {
        bits::get(&self.data, self.bits, ((z & 15) * 16 + (x & 15)) as usize)
    }

    /// Prefetch hint for the whole heightmap.
    #[inline]
    pub fn prefetch(&self) {
        for word in self.data.iter().step_by(8).chain(self.data.last()) {
            vanillite_base::prefetch::read(word);
        }
    }

    /// Sets the height of a column relative to the minimum Y.
    #[inline]
    pub fn set(&mut self, x: u32, z: u32, relative: u32) {
        bits::set(
            &mut self.data,
            self.bits,
            ((z & 15) * 16 + (x & 15)) as usize,
            relative,
        );
    }
}

/// The heightmaps a chunk keeps, by type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Heightmaps {
    maps: [Option<Heightmap>; 6],
}

impl Heightmaps {
    /// Heap bytes of the heightmaps' data (`/vanillite memory`).
    pub fn heap_bytes(&self) -> usize {
        self.maps
            .iter()
            .flatten()
            .map(|m| std::mem::size_of_val::<[Word]>(&m.data))
            .sum()
    }

    /// Heightmaps of the given types for a chunk of `height` blocks.
    pub fn new(height: u32, kinds: &[HeightmapKind]) -> Self {
        let mut maps = Self::default();
        for &kind in kinds {
            maps.maps[usize::from(kind.id())] = Some(Heightmap::new(height));
        }
        maps
    }

    /// The heightmap of a type, if kept.
    #[inline]
    pub fn get(&self, kind: HeightmapKind) -> Option<&Heightmap> {
        self.maps[usize::from(kind.id())].as_ref()
    }

    /// The heightmap of a type, mutable, if kept.
    #[inline]
    pub fn get_mut(&mut self, kind: HeightmapKind) -> Option<&mut Heightmap> {
        self.maps[usize::from(kind.id())].as_mut()
    }

    /// Kept heightmaps in id order.
    pub fn iter(&self) -> impl Iterator<Item = (HeightmapKind, &Heightmap)> {
        HeightmapKind::ALL
            .into_iter()
            .zip(&self.maps)
            .filter_map(|(kind, map)| map.as_ref().map(|map| (kind, map)))
    }
}
