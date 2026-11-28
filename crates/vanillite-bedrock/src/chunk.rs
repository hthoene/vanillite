//! Java chunk sections -> Bedrock sub-chunks (format 9) and biome storages.
//!
//! Each Java section (its exact wire bytes plus the dimension, biome table and Bedrock palette) is
//! translated once per palette and cached for all Bedrock viewers; a chunk packet is assembled
//! from cached sections.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use rustc_hash::{FxHashMap, FxHasher};

use crate::data::{Blocks, WATERLOGGED};
use crate::io::{self, Error, Reader, Result};
use crate::version::Version;

/// Blocks per section.
const ENTRIES: usize = 4096;

/// Vertical layout of a dimension on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Layout {
    /// Lowest Java section y (block y / 16).
    pub java_min_section: i32,
    /// Java sections per chunk.
    pub java_sections: u32,
    /// Lowest Bedrock sub-chunk y.
    pub bedrock_min_section: i32,
    /// Bedrock sub-chunks per chunk.
    pub bedrock_sections: u32,
    /// Bedrock dimension id.
    pub dimension: i32,
}

impl Layout {
    /// The layout of a Java dimension by name and height.
    pub fn for_dimension(name: &str, java_min_y: i32, java_height: u32) -> Self {
        let (dimension, min, count) = match name {
            "minecraft:the_nether" => (1, 0, 8),
            "minecraft:the_end" => (2, 0, 16),
            _ => (0, -4, 24),
        };
        Self {
            java_min_section: java_min_y.div_euclid(16),
            java_sections: java_height / 16,
            bedrock_min_section: min,
            bedrock_sections: count,
            dimension,
        }
    }
}

/// Java biome id -> Bedrock biome id, plus the bits of Java's global biome palette.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BiomeTable {
    pub ids: Vec<u32>,
    pub global_bits: u32,
}

impl BiomeTable {
    /// The table for the Java biome registry in network id order.
    pub fn new(java_names: &[String]) -> Self {
        let count = u32::try_from(java_names.len()).unwrap_or(u32::MAX).max(1);
        Self {
            ids: java_names
                .iter()
                .map(|n| crate::data::biome_id(n))
                .collect(),
            global_bits: u32::BITS.saturating_sub(count.saturating_sub(1).leading_zeros()),
        }
    }

    fn get(&self, java: u32) -> u32 {
        usize::try_from(java)
            .ok()
            .and_then(|i| self.ids.get(i))
            .copied()
            .unwrap_or(1)
    }
}

/// A Java paletted container read from the wire.
struct Container {
    /// Palette (empty for the global palette).
    palette: Vec<u32>,
    bits: u32,
    per_word: usize,
    words: Vec<u64>,
}

impl Container {
    fn read(r: &mut Reader<'_>, entries: usize, blocks: bool, global_bits: u32) -> Result<Self> {
        let wire = u32::from(r.u8()?);
        let (bits, palette) = if wire == 0 {
            (0, vec![java_varint(r)?])
        } else {
            let indirect = if blocks {
                (1..=8).contains(&wire).then(|| wire.max(4))
            } else {
                (1..=3).contains(&wire).then_some(wire)
            };
            match indirect {
                Some(bits) => {
                    let len = usize::try_from(java_varint(r)?).unwrap_or(usize::MAX);
                    if len == 0 || len > 1 << bits {
                        return Err(Error::Invalid("palette length"));
                    }
                    let mut palette = Vec::with_capacity(len);
                    for _ in 0..len {
                        palette.push(java_varint(r)?);
                    }
                    (bits, palette)
                }
                None => (global_bits, Vec::new()),
            }
        };
        let per_word = usize::try_from(64u32.checked_div(bits).unwrap_or(64))
            .unwrap_or(1)
            .max(1);
        let words = if bits == 0 {
            Vec::new()
        } else {
            let n = entries.div_ceil(per_word);
            let raw = r.take(n.saturating_mul(8))?;
            raw.as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_be_bytes(*c))
                .collect()
        };
        Ok(Self {
            palette,
            bits,
            per_word,
            words,
        })
    }

    /// Value at index `i` (a global id).
    #[inline]
    fn get(&self, i: usize) -> u32 {
        if self.bits == 0 {
            return self.palette.first().copied().unwrap_or(0);
        }
        let word = self
            .words
            .get(i.checked_div(self.per_word).unwrap_or(0))
            .copied()
            .unwrap_or(0);
        let shift = u32::try_from(i.checked_rem(self.per_word).unwrap_or(0))
            .unwrap_or(0)
            .saturating_mul(self.bits);
        let mask = 1u64.wrapping_shl(self.bits).wrapping_sub(1);
        let raw = u32::try_from(word.wrapping_shr(shift) & mask).unwrap_or(0);
        if self.palette.is_empty() {
            raw
        } else {
            usize::try_from(raw)
                .ok()
                .and_then(|p| self.palette.get(p))
                .copied()
                .unwrap_or(0)
        }
    }

    fn single(&self) -> Option<u32> {
        (self.bits == 0).then(|| self.palette.first().copied().unwrap_or(0))
    }
}

fn java_varint(r: &mut Reader<'_>) -> Result<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let b = r.u8()?;
        value |= u32::from(b & 0x7f).checked_shl(shift).unwrap_or(0);
        if b & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::VarInt)
}

/// Writes one Bedrock palette storage for 4096 values given by `value(bedrock_index)`.
fn write_storage(out: &mut Vec<u8>, value: impl Fn(usize) -> u32) {
    let mut palette: Vec<u32> = Vec::new();
    let mut indices = [0u16; ENTRIES];
    let mut last = (u32::MAX, 0u16);
    for (i, slot) in indices.iter_mut().enumerate() {
        let v = value(i);
        if v != last.0 {
            let p = match palette.iter().position(|&e| e == v) {
                Some(p) => p,
                None => {
                    palette.push(v);
                    palette.len().saturating_sub(1)
                }
            };
            last = (v, u16::try_from(p).unwrap_or(0));
        }
        *slot = last.1;
    }
    let bits: u32 = match palette.len() {
        0 | 1 => 0,
        2 => 1,
        3..=4 => 2,
        5..=8 => 3,
        9..=16 => 4,
        17..=32 => 5,
        33..=64 => 6,
        65..=256 => 8,
        _ => 16,
    };
    out.push(u8::try_from(bits.wrapping_shl(1) | 1).unwrap_or(1));
    if bits == 0 {
        io::write_vari32(
            out,
            i32::try_from(palette.first().copied().unwrap_or(0)).unwrap_or(0),
        );
        return;
    }
    let per_word = usize::try_from(32u32.checked_div(bits).unwrap_or(1)).unwrap_or(1);
    for chunk in indices.chunks(per_word) {
        let mut word = 0u32;
        for (j, &idx) in chunk.iter().enumerate() {
            let shift = u32::try_from(j).unwrap_or(0).saturating_mul(bits);
            word |= u32::from(idx).wrapping_shl(shift);
        }
        out.extend_from_slice(&word.to_le_bytes());
    }
    io::write_vari32(out, i32::try_from(palette.len()).unwrap_or(0));
    for v in palette {
        io::write_vari32(out, i32::try_from(v).unwrap_or(0));
    }
}

/// Bedrock index (x, z, y order) -> Java index (y, z, x order) at the same block.
#[inline]
fn java_index(i: usize) -> usize {
    let x = i.wrapping_shr(8) & 15;
    let z = i.wrapping_shr(4) & 15;
    let y = i & 15;
    y.wrapping_shl(8) | z.wrapping_shl(4) | x
}

/// Java biome index (4x4x4 cells) of a Bedrock block index.
#[inline]
fn biome_index(i: usize) -> usize {
    let x = i.wrapping_shr(10) & 3;
    let z = i.wrapping_shr(6) & 3;
    let y = i.wrapping_shr(2) & 3;
    y.wrapping_shl(4) | z.wrapping_shl(2) | x
}

/// One translated section: the sub-chunk and the biome storage.
#[derive(Debug)]
struct Translated {
    sub_chunk: Vec<u8>,
    biomes: Vec<u8>,
    empty: bool,
}

fn translate_section(
    blocks: &Container,
    biomes: &Container,
    y: i32,
    table: &BiomeTable,
    map: &Blocks,
) -> Translated {
    let mut sub_chunk = Vec::new();
    sub_chunk.push(9);
    let single = blocks.single().map(|s| map.entry(s));
    let wet = match single {
        Some(e) => e & WATERLOGGED != 0,
        None => {
            blocks
                .palette
                .iter()
                .any(|&s| map.entry(s) & WATERLOGGED != 0)
                || (blocks.palette.is_empty()
                    && (0..ENTRIES).any(|i| map.entry(blocks.get(i)) & WATERLOGGED != 0))
        }
    };
    sub_chunk.push(if wet { 2 } else { 1 });
    sub_chunk.push(i8::try_from(y).map_or(0, |v| v.to_le_bytes()[0]));
    let empty = single.is_some_and(|e| u32::from(e & !WATERLOGGED) == map.air) && !wet;
    match single {
        Some(e) => write_storage(&mut sub_chunk, |_| u32::from(e & !WATERLOGGED)),
        None => write_storage(&mut sub_chunk, |i| {
            u32::from(map.entry(blocks.get(java_index(i))) & !WATERLOGGED)
        }),
    }
    if wet {
        write_storage(&mut sub_chunk, |i| {
            if map.entry(blocks.get(java_index(i))) & WATERLOGGED != 0 {
                map.water
            } else {
                map.air
            }
        });
    }
    let mut biome_out = Vec::new();
    match biomes.single() {
        Some(b) => write_storage(&mut biome_out, |_| table.get(b)),
        None => write_storage(&mut biome_out, |i| table.get(biomes.get(biome_index(i)))),
    }
    Translated {
        sub_chunk,
        biomes: biome_out,
        empty,
    }
}

/// Shared cache of translated sections (FIFO eviction).
#[derive(Debug)]
pub struct ChunkCache {
    inner: Mutex<CacheInner>,
    capacity: usize,
}

#[derive(Debug, Default)]
struct CacheInner {
    map: FxHashMap<u64, Arc<Translated>>,
    order: VecDeque<u64>,
}

impl ChunkCache {
    /// A cache of at most `capacity` sections.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(CacheInner::default()),
            capacity,
        }
    }

    fn get_or_insert(&self, key: u64, make: impl FnOnce() -> Translated) -> Arc<Translated> {
        if let Ok(g) = self.inner.lock()
            && let Some(t) = g.map.get(&key)
        {
            return Arc::clone(t);
        }
        let t = Arc::new(make());
        if self.capacity > 0
            && let Ok(mut g) = self.inner.lock()
        {
            if g.map.insert(key, Arc::clone(&t)).is_none() {
                g.order.push_back(key);
            }
            while g.order.len() > self.capacity {
                if let Some(old) = g.order.pop_front() {
                    g.map.remove(&old);
                }
            }
        }
        t
    }
}

/// Most block entities of one chunk read.
const MAX_BLOCK_ENTITIES: u32 = 1 << 16;

/// Translates the body of a Java `level_chunk_with_light` (after x and z) into the body of a
/// Bedrock `level_chunk` of version `v` and the `block_actor_data` bodies of its block
/// entities ([`crate::blockentity`]).
#[allow(clippy::too_many_arguments)]
pub fn translate(
    v: &Version,
    x: i32,
    z: i32,
    java: &[u8],
    layout: Layout,
    biomes: &BiomeTable,
    biome_key: u64,
    cache: &ChunkCache,
) -> Result<ChunkOut> {
    let map = v.palette.blocks();
    let mut r = Reader::new(java);
    // Heightmaps.
    let maps = java_varint(&mut r)?;
    if maps > 16 {
        return Err(Error::TooLong("heightmaps"));
    }
    for _ in 0..maps {
        java_varint(&mut r)?;
        let longs = usize::try_from(java_varint(&mut r)?).unwrap_or(usize::MAX);
        r.take(longs.checked_mul(8).ok_or(Error::TooLong("heightmap"))?)?;
    }
    let size = usize::try_from(java_varint(&mut r)?).unwrap_or(usize::MAX);
    let mut sections = Reader::new(r.take(size)?);

    let mut translated: Vec<Option<Arc<Translated>>> =
        vec![None; usize::try_from(layout.bedrock_sections).unwrap_or(0)];
    let mut java_blocks = Vec::with_capacity(usize::try_from(layout.java_sections).unwrap_or(0));
    for i in 0..layout.java_sections {
        let start = sections.clone();
        sections.take(4)?;
        let blocks = Container::read(&mut sections, ENTRIES, true, 16)?;
        let biome_container = Container::read(&mut sections, 64, false, biomes.global_bits)?;
        let consumed = start.remaining().saturating_sub(sections.remaining());
        let y = layout
            .java_min_section
            .saturating_add(i32::try_from(i).unwrap_or(0));
        let Some(slot) = usize::try_from(y.saturating_sub(layout.bedrock_min_section))
            .ok()
            .and_then(|s| translated.get_mut(s))
        else {
            continue;
        };
        let bytes = start.clone().take(consumed)?;
        let mut h = FxHasher::default();
        bytes.hash(&mut h);
        y.hash(&mut h);
        biome_key.hash(&mut h);
        v.palette.name.hash(&mut h);
        let key = h.finish();
        *slot = Some(cache.get_or_insert(key, || {
            translate_section(&blocks, &biome_container, y, biomes, map)
        }));
        java_blocks.push(blocks);
    }
    let mut heads = Vec::new();
    let mut actors = block_entities(
        &mut r,
        x,
        z,
        layout.java_min_section,
        &java_blocks,
        v,
        &mut heads,
    );
    flower_pots(&mut actors, x, z, layout.java_min_section, &java_blocks, v);
    let near_pistons = near_pistons(x, z, layout.java_min_section, &java_blocks);

    // Sub-chunks up to the highest non-empty one; biomes for the whole dimension.
    let count = translated
        .iter()
        .rposition(|t| t.as_ref().is_some_and(|t| !t.empty))
        .map_or(0, |p| p.saturating_add(1));
    let mut payload = Vec::with_capacity(16 * 1024);
    for (i, t) in translated.iter().take(count).enumerate() {
        match t {
            Some(t) => payload.extend_from_slice(&t.sub_chunk),
            None => {
                let y = layout
                    .bedrock_min_section
                    .saturating_add(i32::try_from(i).unwrap_or(0));
                payload.extend_from_slice(&[
                    9,
                    1,
                    i8::try_from(y).map_or(0, |v| v.to_le_bytes()[0]),
                ]);
                write_storage(&mut payload, |_| map.air);
            }
        }
    }
    let mut previous: Option<&[u8]> = None;
    for t in &translated {
        let storage = t.as_ref().map(|t| t.biomes.as_slice());
        match (storage, previous) {
            (Some(s), Some(p)) if s == p => payload.push(0xff),
            (Some(s), _) => {
                payload.extend_from_slice(s);
                previous = Some(s);
            }
            (None, Some(_)) => payload.push(0xff),
            (None, None) => {
                write_storage(&mut payload, |_| 1);
            }
        }
    }
    payload.push(0); // No border blocks.

    let mut out = Vec::with_capacity(payload.len().saturating_add(24));
    io::write_vari32(&mut out, x);
    io::write_vari32(&mut out, z);
    io::write_vari32(&mut out, layout.dimension);
    io::write_len(&mut out, count);
    out.push(0); // No sub-chunk request limit.
    out.push(0); // Blob cache off.
    out.push(0); // No blob hashes.
    io::write_bytes(&mut out, &payload);
    if !v.before_26_60() {
        out.push(0); // No client biome override.
    }
    Ok((out, actors, heads, near_pistons))
}

/// A translated chunk: the `level_chunk` body, its block actors and its player heads.
/// The Bedrock chunk, its block actors, its player heads and the Java blocks in front of its
/// pistons (the blocks they may move).
pub type ChunkOut = (Vec<u8>, Vec<Vec<u8>>, Vec<Head>, Vec<([i32; 3], u32)>);

/// A player head with a skin in a chunk: position, Java state and the `textures` value of its
/// profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub pos: [i32; 3],
    pub state: u32,
    pub textures: String,
}

/// The block entities after the sections that Bedrock needs block actor data for.
fn block_entities(
    r: &mut Reader<'_>,
    cx: i32,
    cz: i32,
    min_section: i32,
    sections: &[Container],
    v: &Version,
    heads: &mut Vec<Head>,
) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Ok(n) = java_varint(r) else {
        return out;
    };
    for _ in 0..n.min(MAX_BLOCK_ENTITIES) {
        let (Ok(xz), Ok(y), Ok(_kind)) = (r.u8(), r.array::<2>(), java_varint(r)) else {
            break;
        };
        let y = i32::from(i16::from_be_bytes(y));
        let (lx, lz) = (i32::from(xz >> 4), i32::from(xz & 15));
        let state = y
            .checked_shr(4)
            .and_then(|s| usize::try_from(s.saturating_sub(min_section)).ok())
            .and_then(|s| sections.get(s))
            .map_or(0, |c| {
                let i = (y & 15).wrapping_shl(8) | lz.wrapping_shl(4) | lx;
                c.get(usize::try_from(i).unwrap_or(0))
            });
        let rest = r.rest();
        let mut budget = vanillite_nbt::Budget::network();
        let (nbt, used) = if crate::blockentity::needs_nbt(state) {
            let mut input = rest;
            match vanillite_nbt::read::read_network(&mut input, &mut budget) {
                Ok(tag) => (tag, rest.len().saturating_sub(input.len())),
                Err(_) => break,
            }
        } else {
            match vanillite_nbt::read::skip_network(rest, &mut budget) {
                Ok(used) => (None, used),
                Err(_) => break,
            }
        };
        *r = Reader::new(rest.get(used..).unwrap_or_default());
        let pos = [
            cx.wrapping_mul(16).wrapping_add(lx),
            y,
            cz.wrapping_mul(16).wrapping_add(lz),
        ];
        let compound = match &nbt {
            Some(vanillite_nbt::Tag::Compound(c)) => Some(c),
            _ => None,
        };
        if let Some(textures) = compound.and_then(crate::blockentity::head_textures) {
            heads.push(Head {
                pos,
                state,
                textures,
            });
        }
        if let Some(nbt) = crate::blockentity::translate(pos, state, compound, v.palette) {
            out.push(crate::packets::block_actor_data(pos, &nbt));
        }
    }
    out
}

/// The Java blocks in front of the chunk's pistons (up to 13, within the chunk): the blocks a
/// piston may move. Only sections whose palette holds a piston are scanned.
fn near_pistons(
    cx: i32,
    cz: i32,
    min_section: i32,
    sections: &[Container],
) -> Vec<([i32; 3], u32)> {
    let mut out = Vec::new();
    let is_piston = |s: u32| crate::blockentity::piston_facing(s).is_some();
    let base_y = min_section.saturating_mul(16);
    let height = i32::try_from(sections.len())
        .unwrap_or(0)
        .saturating_mul(16);
    let state_at = |lx: i32, y: i32, lz: i32| -> Option<u32> {
        if !(0..16).contains(&lx) || !(0..16).contains(&lz) || !(0..height).contains(&y) {
            return None;
        }
        let c = sections.get(usize::try_from(y >> 4).ok()?)?;
        Some(c.get(usize::try_from((y & 15) << 8 | lz << 4 | lx).ok()?))
    };
    for (i, c) in sections.iter().enumerate() {
        if !c.palette.is_empty() && !c.palette.iter().any(|&s| is_piston(s)) {
            continue;
        }
        let sy = i32::try_from(i).unwrap_or(0).saturating_mul(16);
        for index in 0..ENTRIES {
            let Some(facing) = crate::blockentity::piston_facing(c.get(index)) else {
                continue;
            };
            let idx = i32::try_from(index).unwrap_or(0);
            let (lx, ly, lz) = (idx & 15, sy.wrapping_add(idx >> 8), (idx >> 4) & 15);
            let [dx, dy, dz] = crate::blockentity::direction_step(facing);
            for k in 1..=13 {
                let (x, y, z) = (
                    lx.wrapping_add(dx.wrapping_mul(k)),
                    ly.wrapping_add(dy.wrapping_mul(k)),
                    lz.wrapping_add(dz.wrapping_mul(k)),
                );
                let Some(state) = state_at(x, y, z) else {
                    break;
                };
                out.push((
                    [
                        cx.wrapping_mul(16).wrapping_add(x),
                        base_y.wrapping_add(y),
                        cz.wrapping_mul(16).wrapping_add(z),
                    ],
                    state,
                ));
            }
        }
    }
    out
}

/// Block actor data of the potted plants (Java pots have no block entity; Bedrock keeps the
/// plant in one). Only sections whose palette holds a potted plant are scanned.
fn flower_pots(
    out: &mut Vec<Vec<u8>>,
    cx: i32,
    cz: i32,
    min_section: i32,
    sections: &[Container],
    v: &Version,
) {
    // Pots and extended pistons: block actors without a Java block entity.
    let pot =
        |s: u32| v.palette.potted_plant(s).is_some() || crate::blockentity::extended_piston(s);
    for (i, c) in sections.iter().enumerate() {
        if !c.palette.is_empty() && !c.palette.iter().any(|&s| pot(s)) {
            continue;
        }
        let base_y = min_section
            .saturating_add(i32::try_from(i).unwrap_or(0))
            .saturating_mul(16);
        for index in 0..ENTRIES {
            let state = c.get(index);
            if !pot(state) {
                continue;
            }
            let i = i32::try_from(index).unwrap_or(0);
            let pos = [
                cx.wrapping_mul(16).wrapping_add(i & 15),
                base_y.wrapping_add(i >> 8),
                cz.wrapping_mul(16).wrapping_add((i >> 4) & 15),
            ];
            if let Some(nbt) = crate::blockentity::translate(pos, state, None, v.palette) {
                out.push(crate::packets::block_actor_data(pos, &nbt));
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    /// A Java section: `non_air`, fluids, blocks container, biomes container (single values).
    fn java_single(block: u32, biome: u32) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0, 0];
        vanillite_protocol::codec::write_varint(&mut v, i32::try_from(block).unwrap());
        v.push(0);
        vanillite_protocol::codec::write_varint(&mut v, i32::try_from(biome).unwrap());
        v
    }

    /// A Java section with a 4-bit palette: stone at y = 0, air above.
    fn java_layered() -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0, 4, 2, 0, 1];
        // 16 entries of 4 bits per long; the first 256 entries (y = 0) are 1.
        for word in 0..256 {
            let w: u64 = if word < 16 { 0x1111_1111_1111_1111 } else { 0 };
            v.extend_from_slice(&w.to_be_bytes());
        }
        v.extend_from_slice(&[0, 0]);
        v
    }

    fn chunk(sections: &[Vec<u8>]) -> Vec<u8> {
        let mut v = vec![0];
        let data: Vec<u8> = sections.concat();
        vanillite_protocol::codec::write_varint(&mut v, i32::try_from(data.len()).unwrap());
        v.extend_from_slice(&data);
        v.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
        v
    }

    fn read_storage(r: &mut Reader<'_>) -> Vec<u32> {
        let header = r.u8().unwrap();
        let bits = u32::from(header >> 1);
        if bits == 0 {
            return vec![u32::try_from(r.vari32().unwrap()).unwrap(); ENTRIES];
        }
        let per_word = usize::try_from(32 / bits).unwrap();
        let words: Vec<u32> = (0..ENTRIES.div_ceil(per_word))
            .map(|_| r.u32().unwrap())
            .collect();
        let len = usize::try_from(r.vari32().unwrap()).unwrap();
        let palette: Vec<u32> = (0..len)
            .map(|_| u32::try_from(r.vari32().unwrap()).unwrap())
            .collect();
        (0..ENTRIES)
            .map(|i| {
                let w = words[i / per_word];
                let idx = (w >> (u32::try_from(i % per_word).unwrap() * bits)) & ((1 << bits) - 1);
                palette[usize::try_from(idx).unwrap()]
            })
            .collect()
    }

    #[test]
    fn translates_blocks_in_bedrock_order() {
        let layout = Layout::for_dimension("minecraft:overworld", -64, 384);
        let mut sections = vec![java_layered()];
        sections.extend((1..24).map(|_| java_single(0, 0)));
        let java = chunk(&sections);
        let table = BiomeTable::new(&["minecraft:plains".to_owned()]);
        let cache = ChunkCache::new(16);
        let (out, _, _, _) =
            translate(Version::latest(), 3, -2, &java, layout, &table, 0, &cache).unwrap();
        let mut r = Reader::new(&out);
        assert_eq!(
            (
                r.vari32().unwrap(),
                r.vari32().unwrap(),
                r.vari32().unwrap()
            ),
            (3, -2, 0)
        );
        assert_eq!(
            r.varu32().unwrap(),
            1,
            "only the lowest sub-chunk has blocks"
        );
        r.take(3).unwrap();
        let payload = r.bytes("payload", 1 << 20).unwrap();
        let mut p = Reader::new(payload);
        assert_eq!(p.array::<3>().unwrap(), [9, 1, 0xfc]);
        let blocks = read_storage(&mut p);
        let map = Version::latest().palette.blocks();
        let stone = u32::from(map.entry(1));
        let air = map.air;
        // Bedrock index x << 8 | z << 4 | y: y = 0 is stone for every column.
        assert!((0..ENTRIES).all(|i| blocks[i] == if i & 15 == 0 { stone } else { air }));
        // 24 biome storages: one plains storage, then "same as before".
        assert_eq!(read_storage(&mut p)[0], 1);
        assert_eq!(p.rest(), [[0xffu8; 23].as_slice(), &[0]].concat());
        // A second translation hits the cache.
        assert_eq!(
            translate(Version::latest(), 3, -2, &java, layout, &table, 0, &cache)
                .unwrap()
                .0,
            out
        );
    }

    #[test]
    fn rejects_truncated_chunks() {
        let layout = Layout::for_dimension("minecraft:overworld", -64, 384);
        let table = BiomeTable::new(&["minecraft:plains".to_owned()]);
        let java = chunk(&[java_layered()]);
        assert!(
            translate(
                Version::latest(),
                0,
                0,
                &java[..java.len() - 600],
                layout,
                &table,
                0,
                &ChunkCache::new(0)
            )
            .is_err()
        );
    }
}
