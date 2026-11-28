//! Paletted containers in the client's wire layout.
//!
//! Vanilla: `net.minecraft.world.level.chunk.PalettedContainer`, `Strategy`, `Configuration`.
//! Growth follows Vanilla: a full palette is replaced by the configuration for one more bit, the
//! new palette holds the old values in order of their first appearance in the data (unused entries
//! are dropped), then the new value. Containers never shrink on writes; [`PalettedContainer::from_parts`]
//! and [`PalettedContainer::read`] build the representation Vanilla would read.

use std::fmt;
use std::marker::PhantomData;

use crate::bits::{self, Word};
use crate::facts;
use crate::wire::{self, DecodeError};

/// What a container holds: entry count and the palette configurations per bit count.
pub trait Strategy: 'static {
    /// Bits per axis of the coordinate index (`(y << a | z) << a | x`).
    const AXIS_BITS: u32;
    /// Number of entries.
    const ENTRIES: usize = 1 << (3 * Self::AXIS_BITS);

    /// Bits of the indirect configuration chosen for a request of `bits` (>= 1), or `None` when
    /// the request needs the global palette.
    fn indirect_bits(bits: u8) -> Option<u8>;
    /// Bits per entry of the global palette.
    fn global_bits() -> u8;
    /// Size of the global palette.
    fn global_count() -> u32;

    /// Index of local coordinates.
    #[inline]
    fn index(x: u32, y: u32, z: u32) -> usize {
        let m = (1 << Self::AXIS_BITS) - 1;
        ((((y & m) << Self::AXIS_BITS | (z & m)) << Self::AXIS_BITS) | (x & m)) as usize
    }
}

/// Block states of a section: 16³ entries; linear palette with 4 bits for 1..=4, hash palette
/// for 5..=8, global palette above.
#[derive(Debug)]
pub enum BlockStates {}

impl Strategy for BlockStates {
    const AXIS_BITS: u32 = 4;

    #[inline]
    fn indirect_bits(bits: u8) -> Option<u8> {
        match bits {
            0..=4 => Some(4),
            5..=8 => Some(bits),
            _ => None,
        }
    }

    #[inline]
    fn global_bits() -> u8 {
        facts::block_global_bits()
    }

    #[inline]
    fn global_count() -> u32 {
        facts::block_state_count()
    }
}

/// Biomes of a section: 4³ entries; linear palette for 1..=3 bits, global palette above.
#[derive(Debug)]
pub enum Biomes {}

impl Strategy for Biomes {
    const AXIS_BITS: u32 = 2;

    #[inline]
    fn indirect_bits(bits: u8) -> Option<u8> {
        match bits {
            0..=3 => Some(bits.max(1)),
            _ => None,
        }
    }

    #[inline]
    fn global_bits() -> u8 {
        facts::biome_global_bits()
    }

    #[inline]
    fn global_count() -> u32 {
        facts::biome_count()
    }
}

/// The stored representation, exactly what goes on the wire.
#[derive(Clone, PartialEq, Eq)]
pub enum ContainerData {
    /// One value everywhere; no data array (wire bits 0).
    Single(u16),
    /// Local palette and packed palette indices.
    Indirect {
        /// Bits per entry.
        bits: u8,
        /// Palette, in Vanilla's id order.
        palette: Vec<u16>,
        /// Packed indices, big-endian words.
        data: Box<[Word]>,
    },
    /// Packed global ids.
    Direct {
        /// Bits per entry (bits of the global palette).
        bits: u8,
        /// Packed global ids, big-endian words.
        data: Box<[Word]>,
    },
}

impl fmt::Debug for ContainerData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Single(value) => f.debug_tuple("Single").field(value).finish(),
            Self::Indirect { bits, palette, .. } => f
                .debug_struct("Indirect")
                .field("bits", bits)
                .field("palette", palette)
                .finish_non_exhaustive(),
            Self::Direct { bits, .. } => f
                .debug_struct("Direct")
                .field("bits", bits)
                .finish_non_exhaustive(),
        }
    }
}

/// A paletted container of `S::ENTRIES` values.
pub struct PalettedContainer<S: Strategy> {
    data: ContainerData,
    strategy: PhantomData<fn() -> S>,
}

impl<S: Strategy> Clone for PalettedContainer<S> {
    fn clone(&self) -> Self {
        Self::wrap(self.data.clone())
    }
}

impl<S: Strategy> PartialEq for PalettedContainer<S> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<S: Strategy> Eq for PalettedContainer<S> {}

impl<S: Strategy> fmt::Debug for PalettedContainer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.data.fmt(f)
    }
}

impl<S: Strategy> PalettedContainer<S> {
    /// Heap bytes of the palette and the data array (`/vanillite memory`).
    pub fn heap_bytes(&self) -> usize {
        match &self.data {
            ContainerData::Single(_) => 0,
            ContainerData::Indirect { palette, data, .. } => {
                palette.capacity() * size_of::<u16>() + std::mem::size_of_val::<[Word]>(data)
            }
            ContainerData::Direct { data, .. } => std::mem::size_of_val::<[Word]>(data),
        }
    }

    #[inline]
    fn wrap(data: ContainerData) -> Self {
        Self {
            data,
            strategy: PhantomData,
        }
    }

    /// A container holding `value` everywhere.
    #[inline]
    pub fn new(value: u16) -> Self {
        Self::wrap(ContainerData::Single(value))
    }

    /// Builds a container from its wire parts, validated as untrusted input. `bits` is the bit
    /// count of the stored data; 0 means `Single(palette[0])`; `palette` is ignored when `bits` is
    /// the global bit count above the indirect range.
    pub fn from_parts(bits: u8, palette: &[u16], data: &[Word]) -> Result<Self, DecodeError> {
        Self::from_parts_counted(bits, palette, data, &mut [0; 256])
    }

    /// [`from_parts`](Self::from_parts) that also counts how often each palette index occurs
    /// in indirect data (`counts` starts zeroed; it stays untouched for other forms).
    pub(crate) fn from_parts_counted(
        bits: u8,
        palette: &[u16],
        data: &[Word],
        counts: &mut [u32; 256],
    ) -> Result<Self, DecodeError> {
        let global = S::global_count();
        let check_value = |value: u16| {
            if u32::from(value) < global {
                Ok(value)
            } else {
                Err(DecodeError::InvalidValue(u32::from(value)))
            }
        };
        if bits == 0 {
            let [value] = palette else {
                return Err(DecodeError::InvalidPaletteLength(palette.len() as u32));
            };
            if !data.is_empty() {
                return Err(DecodeError::InvalidDataLength(data.len()));
            }
            return Ok(Self::new(check_value(*value)?));
        }
        let expected_words = |bits: u8| bits::words_for(S::ENTRIES, bits);
        match S::indirect_bits(bits) {
            Some(config) if config == bits => {
                if palette.is_empty() || palette.len() > 1 << bits {
                    return Err(DecodeError::InvalidPaletteLength(palette.len() as u32));
                }
                if data.len() != expected_words(bits) {
                    return Err(DecodeError::InvalidDataLength(data.len()));
                }
                for &value in palette {
                    check_value(value)?;
                }
                // Indirect data has at most 8 bits: every index has a counter.
                bits::for_each(data, bits, S::ENTRIES, |_, index| {
                    counts[index as usize & 255] += 1;
                });
                if let Some(index) = counts[palette.len()..].iter().position(|&n| n > 0) {
                    return Err(DecodeError::InvalidIndex((palette.len() + index) as u32));
                }
                Ok(Self::wrap(ContainerData::Indirect {
                    bits,
                    palette: palette.to_vec(),
                    data: data.into(),
                }))
            }
            None if bits == S::global_bits() => {
                if data.len() != expected_words(bits) {
                    return Err(DecodeError::InvalidDataLength(data.len()));
                }
                let mut bad = None;
                bits::for_each(data, bits, S::ENTRIES, |_, value| {
                    if value >= global {
                        bad = Some(value);
                    }
                });
                if let Some(value) = bad {
                    return Err(DecodeError::InvalidValue(value));
                }
                Ok(Self::wrap(ContainerData::Direct {
                    bits,
                    data: data.into(),
                }))
            }
            _ => Err(DecodeError::InvalidBits(bits)),
        }
    }

    /// The stored representation.
    #[inline]
    pub fn data(&self) -> &ContainerData {
        &self.data
    }

    /// The value if the container holds one value everywhere (only the `Single` form).
    #[inline]
    pub fn single(&self) -> Option<u16> {
        match self.data {
            ContainerData::Single(value) => Some(value),
            _ => None,
        }
    }

    /// Bits per entry as sent on the wire.
    #[inline]
    pub fn bits(&self) -> u8 {
        match self.data {
            ContainerData::Single(_) => 0,
            ContainerData::Indirect { bits, .. } | ContainerData::Direct { bits, .. } => bits,
        }
    }

    /// Value at `index`.
    #[inline(always)]
    pub fn get(&self, index: usize) -> u16 {
        match &self.data {
            ContainerData::Single(value) => *value,
            ContainerData::Indirect {
                bits,
                palette,
                data,
            } => palette[bits::get(data, *bits, index) as usize],
            ContainerData::Direct { bits, data } => bits::get(data, *bits, index) as u16,
        }
    }

    /// Copies every value into `out` (`S::ENTRIES` values, in index order): much cheaper than
    /// reading them one by one.
    pub fn copy_to(&self, out: &mut [u16]) {
        assert_eq!(out.len(), S::ENTRIES, "one value per entry");
        match &self.data {
            ContainerData::Single(value) => out.fill(*value),
            ContainerData::Indirect {
                bits,
                palette,
                data,
            } => {
                // Paletted data has at most 8 bits and only indices into the palette (checked
                // on decode, kept by every write).
                let mut lookup = [0u16; 256];
                lookup[..palette.len()].copy_from_slice(palette);
                bits::unpack(data, *bits, out, |id| lookup[id as usize & 255]);
            }
            ContainerData::Direct { bits, data } => {
                bits::unpack(data, *bits, out, |value| value as u16);
            }
        }
    }

    /// Prefetch hint for the value at local coordinates: its packed word and the palette.
    #[inline(always)]
    pub fn prefetch_at(&self, x: u32, y: u32, z: u32) {
        match &self.data {
            ContainerData::Single(_) => {}
            ContainerData::Indirect {
                bits,
                palette,
                data,
            } => {
                bits::prefetch(data, *bits, S::index(x, y, z));
                vanillite_base::prefetch::read(palette.as_ptr());
            }
            ContainerData::Direct { bits, data } => bits::prefetch(data, *bits, S::index(x, y, z)),
        }
    }

    /// Value at local coordinates.
    #[inline(always)]
    pub fn get_at(&self, x: u32, y: u32, z: u32) -> u16 {
        self.get(S::index(x, y, z))
    }

    /// Sets the value at `index` and returns the previous one.
    #[inline]
    pub fn set(&mut self, index: usize, value: u16) -> u16 {
        // The common case inline: a value already in the palette.
        if let ContainerData::Indirect {
            bits,
            palette,
            data,
        } = &mut self.data
            && let Some(id) = find(palette, value)
        {
            let old = bits::get_and_set(data, *bits, index, id as u32);
            return palette[old as usize];
        }
        self.set_other(index, value)
    }

    /// [`Self::set`] for the other cases: one value, a new palette entry, the direct form.
    #[inline(never)]
    fn set_other(&mut self, index: usize, value: u16) -> u16 {
        match &mut self.data {
            ContainerData::Single(current) => {
                if *current == value {
                    return value;
                }
                self.grow(1, value);
            }
            ContainerData::Indirect {
                bits,
                palette,
                data,
            } => {
                let id = match find(palette, value) {
                    Some(id) => id,
                    None if palette.len() < 1 << *bits => {
                        palette.push(value);
                        palette.len() - 1
                    }
                    None => {
                        let bits = *bits + 1;
                        self.grow(bits, value);
                        return self.set(index, value);
                    }
                };
                let old = bits::get_and_set(data, *bits, index, id as u32);
                return palette[old as usize];
            }
            ContainerData::Direct { bits, data } => {
                return bits::get_and_set(data, *bits, index, u32::from(value)) as u16;
            }
        }
        self.set(index, value)
    }

    /// Sets the value at local coordinates and returns the previous one.
    #[inline]
    pub fn set_at(&mut self, x: u32, y: u32, z: u32, value: u16) -> u16 {
        self.set(S::index(x, y, z), value)
    }

    /// A container holding `values` (`S::ENTRIES` of them, in index order), with the palette in
    /// order of first occurrence and the fewest bits that hold it.
    pub fn from_values(values: &[u16]) -> Self {
        assert_eq!(values.len(), S::ENTRIES, "one value per entry");
        let first = values[0];
        if values.iter().all(|&v| v == first) {
            return Self::new(first);
        }
        let mut palette = vec![first];
        let mut indices = vec![0u8; S::ENTRIES];
        let (mut last, mut last_id) = (first, 0u8);
        for (slot, &value) in indices.iter_mut().zip(values) {
            if value != last {
                last = value;
                last_id = match find(&palette, value) {
                    Some(id) => id as u8,
                    None if palette.len() < 256 => {
                        palette.push(value);
                        (palette.len() - 1) as u8
                    }
                    None => return Self::direct(values),
                };
            }
            *slot = last_id;
        }
        let needed = (usize::BITS - (palette.len() - 1).leading_zeros()) as u8;
        match S::indirect_bits(needed.max(1)) {
            Some(bits) => Self::wrap(ContainerData::Indirect {
                bits,
                data: bits::pack(S::ENTRIES, bits, |i| u32::from(indices[i])),
                palette,
            }),
            None => Self::direct(values),
        }
    }

    /// The global-palette form of `values`.
    fn direct(values: &[u16]) -> Self {
        let bits = S::global_bits();
        Self::wrap(ContainerData::Direct {
            bits,
            data: bits::pack(S::ENTRIES, bits, |i| u32::from(values[i])),
        })
    }

    /// Sets every entry to `value` (drops the data array).
    #[inline]
    pub fn fill(&mut self, value: u16) {
        self.data = ContainerData::Single(value);
    }

    /// Vanilla's `onResize`: switch to the configuration for `requested` bits, copy the values,
    /// and add `value` to the new palette.
    fn grow(&mut self, requested: u8, value: u16) {
        let old = &self.data;
        let new = match S::indirect_bits(requested) {
            Some(bits) => {
                let mut palette = Vec::with_capacity(1 << bits);
                let data = match old {
                    ContainerData::Single(current) => {
                        palette.push(*current);
                        bits::zeroed(S::ENTRIES, bits)
                    }
                    ContainerData::Indirect {
                        bits: old_bits,
                        palette: old_palette,
                        data: old_data,
                    } => {
                        let mut remap = [u16::MAX; 256];
                        let mut indices = vec![0u16; S::ENTRIES];
                        bits::for_each(old_data, *old_bits, S::ENTRIES, |i, old_id| {
                            let slot = &mut remap[old_id as usize];
                            if *slot == u16::MAX {
                                *slot = palette.len() as u16;
                                palette.push(old_palette[old_id as usize]);
                            }
                            indices[i] = *slot;
                        });
                        bits::pack(S::ENTRIES, bits, |i| u32::from(indices[i]))
                    }
                    ContainerData::Direct { .. } => unreachable!("the global palette never grows"),
                };
                palette.push(value);
                ContainerData::Indirect {
                    bits,
                    palette,
                    data,
                }
            }
            None => {
                let bits = S::global_bits();
                ContainerData::Direct {
                    bits,
                    data: bits::pack(S::ENTRIES, bits, |i| u32::from(self.get(i))),
                }
            }
        };
        self.data = new;
    }

    /// Whether a stored value may match `test`: exact for one value, the palette for the
    /// paletted form (it may still list values no longer stored), always `true` for the direct
    /// form. A cheap filter before scanning the entries.
    pub fn may_contain(&self, test: impl Fn(u16) -> bool) -> bool {
        match &self.data {
            ContainerData::Single(value) => test(*value),
            ContainerData::Indirect { palette, .. } => palette.iter().any(|&v| test(v)),
            ContainerData::Direct { .. } => true,
        }
    }

    /// Calls `f(value, count)` for the stored values; values may repeat in the direct form.
    pub fn for_each_count(&self, mut f: impl FnMut(u16, u32)) {
        match &self.data {
            ContainerData::Single(value) => f(*value, S::ENTRIES as u32),
            ContainerData::Indirect {
                bits,
                palette,
                data,
            } => {
                let mut counts = [0u32; 256];
                bits::for_each(data, *bits, S::ENTRIES, |_, id| counts[id as usize] += 1);
                for (value, &count) in palette.iter().zip(&counts) {
                    if count > 0 {
                        f(*value, count);
                    }
                }
            }
            ContainerData::Direct { bits, data } => {
                bits::for_each(data, *bits, S::ENTRIES, |_, value| f(value as u16, 1));
            }
        }
    }

    /// Bytes [`write`](Self::write) appends.
    pub fn serialized_size(&self) -> usize {
        match &self.data {
            ContainerData::Single(value) => 1 + wire::varint_len(u32::from(*value)),
            ContainerData::Indirect { palette, data, .. } => {
                1 + wire::varint_len(palette.len() as u32)
                    + palette
                        .iter()
                        .map(|&v| wire::varint_len(u32::from(v)))
                        .sum::<usize>()
                    + data.len() * 8
            }
            ContainerData::Direct { data, .. } => 1 + data.len() * 8,
        }
    }

    /// Appends the wire form (`PalettedContainer.write`): bits, palette, packed longs without length.
    pub fn write(&self, out: &mut Vec<u8>) {
        match &self.data {
            ContainerData::Single(value) => {
                out.push(0);
                wire::write_varint(out, u32::from(*value));
            }
            ContainerData::Indirect {
                bits,
                palette,
                data,
            } => {
                out.push(*bits);
                wire::write_varint(out, palette.len() as u32);
                for &value in palette {
                    wire::write_varint(out, u32::from(value));
                }
                out.extend_from_slice(data.as_flattened());
            }
            ContainerData::Direct { bits, data } => {
                out.push(*bits);
                out.extend_from_slice(data.as_flattened());
            }
        }
    }

    /// Reads the wire form like the client does (`PalettedContainer.read`), validated.
    pub fn read(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let wire_bits = wire::read_u8(input)?;
        let (bits, palette) = if wire_bits == 0 {
            (0, vec![read_u16_varint(input)?])
        } else {
            match S::indirect_bits(wire_bits) {
                Some(bits) => {
                    let len = wire::read_varint(input)?;
                    if len == 0 || len > 1 << bits {
                        return Err(DecodeError::InvalidPaletteLength(len));
                    }
                    let palette = (0..len)
                        .map(|_| read_u16_varint(input))
                        .collect::<Result<Vec<_>, _>>()?;
                    (bits, palette)
                }
                None => (S::global_bits(), Vec::new()),
            }
        };
        let words = if bits == 0 {
            0
        } else {
            bits::words_for(S::ENTRIES, bits)
        };
        let bytes = words * 8;
        if input.len() < bytes {
            return Err(DecodeError::UnexpectedEnd);
        }
        let (head, rest) = input.split_at(bytes);
        *input = rest;
        let (data, _) = head.as_chunks::<8>();
        Self::from_parts(bits, &palette, data)
    }
}

/// Position of `value` in `palette`; compares 16 entries per step so the loop vectorises.
#[inline]
fn find(palette: &[u16], value: u16) -> Option<usize> {
    if palette.len() <= 16 {
        return palette.iter().position(|&entry| entry == value);
    }
    let mut base = 0;
    for chunk in palette.chunks(16) {
        let hits = chunk.iter().enumerate().fold(0u32, |hits, (i, &entry)| {
            hits | (u32::from(entry == value) << i)
        });
        if hits != 0 {
            return Some(base + hits.trailing_zeros() as usize);
        }
        base += 16;
    }
    None
}

fn read_u16_varint(input: &mut &[u8]) -> Result<u16, DecodeError> {
    let value = wire::read_varint(input)?;
    u16::try_from(value).map_err(|_| DecodeError::InvalidValue(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    type Blocks = PalettedContainer<BlockStates>;

    #[test]
    fn grows_like_vanilla() {
        test_support::install();
        let mut c = Blocks::new(0);
        assert_eq!(c.set(5, 0), 0);
        assert_eq!(c.bits(), 0);
        let mut expected_bits = vec![];
        for value in 1..=300u16 {
            c.set(usize::from(value), value);
            expected_bits.push(c.bits());
        }
        // Linear 4 bits up to 16 values, then 5..=8 bits, then the global palette.
        assert_eq!(expected_bits[0], 4);
        assert_eq!(expected_bits[15], 5);
        assert_eq!(expected_bits[31], 6);
        assert_eq!(expected_bits[255], 16);
        for value in 1..=300u16 {
            assert_eq!(c.get(usize::from(value)), value);
        }
        assert_eq!(c.get(0), 0);
        assert_eq!(c.get(4000), 0);
    }

    #[test]
    fn resize_drops_unused_entries_in_first_appearance_order() {
        test_support::install();
        let mut c = Blocks::new(1);
        for value in 2..=16u16 {
            c.set(usize::from(value), value);
        }
        // Overwrite value 3; palette is full (16 entries) but 3 is unused now.
        c.set(3, 9);
        c.set(4000, 100);
        let ContainerData::Indirect { bits, palette, .. } = c.data() else {
            panic!("expected indirect")
        };
        assert_eq!(*bits, 5);
        // Index 0, 1 hold 1; 2 holds 2; 3 holds 9; 4.. hold 4..; then the new value.
        let mut expected = vec![1, 2, 9, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15, 16, 100];
        expected.dedup();
        assert_eq!(palette, &expected);
    }

    #[test]
    fn wire_round_trip_and_validation() {
        test_support::install();
        let mut c = Blocks::new(7);
        let mut out = vec![];
        c.write(&mut out);
        assert_eq!(out, [0, 7]);
        for i in 0..4096 {
            c.set(i, (i % 40) as u16);
        }
        out.clear();
        c.write(&mut out);
        assert_eq!(out.len(), c.serialized_size());
        let read = Blocks::read(&mut out.as_slice()).unwrap();
        assert_eq!(read, c);

        let mut biomes = PalettedContainer::<Biomes>::new(3);
        biomes.set(10, 4);
        out.clear();
        biomes.write(&mut out);
        assert_eq!(&out[..4], [1, 2, 3, 4]);
        assert_eq!(out.len(), 4 + 8);
        assert_eq!(
            PalettedContainer::<Biomes>::read(&mut out.as_slice()).unwrap(),
            biomes
        );

        // An index past the palette is rejected.
        let data = bits::pack(4096, 4, |i| if i == 9 { 2 } else { 0 });
        assert_eq!(
            Blocks::from_parts(4, &[1, 2], &data),
            Err(DecodeError::InvalidIndex(2))
        );
        assert!(Blocks::from_parts(3, &[1, 2], &data).is_err());
        assert!(Blocks::from_parts(0, &[60000], &[]).is_err());
    }
}
