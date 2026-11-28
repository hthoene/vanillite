//! Packed integer arrays in Vanilla's `SimpleBitStorage` layout.
//!
//! Entries never span two words; a word holds `64 / bits` entries, the first in the lowest bits.
//! Words are stored as big-endian bytes, which is the wire layout, so encoding is a plain copy.

/// One packed 64-bit word, big-endian.
pub type Word = [u8; 8];

#[derive(Clone, Copy)]
struct Divisor {
    per_word: u32,
    magic: u32,
}

/// Exact division by `per_word` for indices below 4096: `i / d == (i * ceil(2^22 / d)) >> 22`,
/// because the rounding error `i * (ceil(2^22 / d) * d - 2^22) < 4096 * 64 < 2^22`.
const MAGIC_SHIFT: u32 = 22;

const DIVISORS: [Divisor; 33] = {
    let mut table = [Divisor {
        per_word: 0,
        magic: 0,
    }; 33];
    let mut bits = 1;
    while bits <= 32 {
        let per_word = 64 / bits;
        table[bits as usize] = Divisor {
            per_word,
            magic: (1u32 << MAGIC_SHIFT).div_ceil(per_word),
        };
        bits += 1;
    }
    table
};

/// Largest index the fast division supports.
pub const MAX_ENTRIES: usize = 4096;

/// Entries per word for `bits` (1..=32).
#[inline]
pub const fn per_word(bits: u8) -> usize {
    64 / bits as usize
}

/// Number of words for `entries` entries of `bits` bits.
#[inline]
pub const fn words_for(entries: usize, bits: u8) -> usize {
    entries.div_ceil(per_word(bits))
}

/// Zeroed storage for `entries` entries of `bits` bits.
pub fn zeroed(entries: usize, bits: u8) -> Box<[Word]> {
    vec![[0u8; 8]; words_for(entries, bits)].into_boxed_slice()
}

#[inline(always)]
fn locate(index: usize, bits: u8) -> (usize, u32) {
    debug_assert!(index < MAX_ENTRIES && (1..=32).contains(&bits));
    let divisor = DIVISORS[usize::from(bits)];
    let index = index as u32;
    let word = ((u64::from(index) * u64::from(divisor.magic)) >> MAGIC_SHIFT) as u32;
    let shift = (index - word * divisor.per_word) * u32::from(bits);
    (word as usize, shift)
}

#[inline]
const fn mask(bits: u8) -> u64 {
    (1u64 << bits) - 1
}

/// Reads entry `index`.
#[inline(always)]
pub fn get(data: &[Word], bits: u8, index: usize) -> u32 {
    let (word, shift) = locate(index, bits);
    ((u64::from_be_bytes(data[word]) >> shift) & mask(bits)) as u32
}

/// Prefetch hint for the word of entry `index` (no read; any index is harmless).
#[inline(always)]
pub fn prefetch(data: &[Word], bits: u8, index: usize) {
    if bits == 0 || index >= MAX_ENTRIES || bits > 32 {
        return;
    }
    let (word, _) = locate(index, bits);
    vanillite_base::prefetch::read(data.as_ptr().wrapping_add(word));
}

/// Writes entry `index` and returns the previous value.
#[inline]
pub fn get_and_set(data: &mut [Word], bits: u8, index: usize, value: u32) -> u32 {
    let (word, shift) = locate(index, bits);
    let slot = &mut data[word];
    let current = u64::from_be_bytes(*slot);
    let old = (current >> shift) & mask(bits);
    let new = (current & !(mask(bits) << shift)) | ((u64::from(value) & mask(bits)) << shift);
    *slot = new.to_be_bytes();
    old as u32
}

/// Writes entry `index`.
#[inline]
pub fn set(data: &mut [Word], bits: u8, index: usize, value: u32) {
    get_and_set(data, bits, index, value);
}

/// Calls `body!(BITS)` with `BITS` a constant equal to `bits` for the common widths (1 to 8:
/// every paletted form), else `fallback`: loops over constant widths unroll per word.
macro_rules! with_const_bits {
    ($bits:expr, $body:ident, $fallback:expr) => {
        match $bits {
            1 => $body!(1),
            2 => $body!(2),
            3 => $body!(3),
            4 => $body!(4),
            5 => $body!(5),
            6 => $body!(6),
            7 => $body!(7),
            8 => $body!(8),
            _ => $fallback,
        }
    };
}

/// Packs `values` into fresh storage of `bits` bits.
pub fn pack(entries: usize, bits: u8, mut value_at: impl FnMut(usize) -> u32) -> Box<[Word]> {
    let mut data = zeroed(entries, bits);
    macro_rules! fixed {
        ($b:literal) => {
            pack_words::<$b>(&mut data, entries, &mut value_at)
        };
    }
    with_const_bits!(
        bits,
        fixed,
        pack_words_any(&mut data, bits, entries, &mut value_at)
    );
    data
}

#[inline(always)]
fn pack_words<const BITS: usize>(
    data: &mut [Word],
    entries: usize,
    value_at: &mut impl FnMut(usize) -> u32,
) {
    let per = 64 / BITS;
    let mask = (1u64 << BITS) - 1;
    let full = entries / per;
    for (word_index, word) in data[..full].iter_mut().enumerate() {
        let start = word_index * per;
        let mut acc = 0u64;
        for slot in 0..per {
            acc |= (u64::from(value_at(start + slot)) & mask) << (slot * BITS);
        }
        *word = acc.to_be_bytes();
    }
    if let Some(word) = data.get_mut(full) {
        let start = full * per;
        let mut acc = 0u64;
        for slot in 0..entries - start {
            acc |= (u64::from(value_at(start + slot)) & mask) << (slot * BITS);
        }
        *word = acc.to_be_bytes();
    }
}

fn pack_words_any(
    data: &mut [Word],
    bits: u8,
    entries: usize,
    value_at: &mut impl FnMut(usize) -> u32,
) {
    let per = per_word(bits);
    for (word_index, word) in data.iter_mut().enumerate() {
        let start = word_index * per;
        let end = (start + per).min(entries);
        let mut acc = 0u64;
        for (slot, index) in (start..end).enumerate() {
            acc |= (u64::from(value_at(index)) & mask(bits)) << (slot * usize::from(bits));
        }
        *word = acc.to_be_bytes();
    }
}

/// Calls `f` with every entry in index order.
#[inline]
pub fn for_each(data: &[Word], bits: u8, entries: usize, mut f: impl FnMut(usize, u32)) {
    macro_rules! fixed {
        ($b:literal) => {
            for_each_fixed::<$b>(data, entries, &mut f)
        };
    }
    with_const_bits!(bits, fixed, for_each_any(data, bits, entries, &mut f));
}

#[inline(always)]
fn for_each_fixed<const BITS: usize>(
    data: &[Word],
    entries: usize,
    f: &mut impl FnMut(usize, u32),
) {
    let per = 64 / BITS;
    let mask = (1u64 << BITS) - 1;
    let full = (entries / per).min(data.len());
    for (word_index, word) in data[..full].iter().enumerate() {
        let value = u64::from_be_bytes(*word);
        let start = word_index * per;
        for slot in 0..per {
            f(start + slot, ((value >> (slot * BITS)) & mask) as u32);
        }
    }
    let start = full * per;
    if start < entries
        && let Some(word) = data.get(full)
    {
        let value = u64::from_be_bytes(*word);
        for slot in 0..(entries - start).min(per) {
            f(start + slot, ((value >> (slot * BITS)) & mask) as u32);
        }
    }
}

fn for_each_any(data: &[Word], bits: u8, entries: usize, f: &mut impl FnMut(usize, u32)) {
    let per = per_word(bits);
    let mut index = 0;
    for word in data {
        let mut value = u64::from_be_bytes(*word);
        for _ in 0..per {
            if index == entries {
                return;
            }
            f(index, (value & mask(bits)) as u32);
            value >>= bits;
            index += 1;
        }
    }
}

/// Writes every entry, mapped by `map`, into `out` (one per entry, in index order): the
/// unchecked bulk form of [`for_each`].
#[inline]
pub fn unpack(data: &[Word], bits: u8, out: &mut [u16], map: impl Fn(u32) -> u16) {
    macro_rules! fixed {
        ($b:literal) => {
            unpack_fixed::<$b>(data, out, &map)
        };
    }
    with_const_bits!(
        bits,
        fixed,
        for_each_any(data, bits, out.len(), &mut |i, v| out[i] = map(v))
    );
}

#[inline(always)]
fn unpack_fixed<const BITS: usize>(data: &[Word], out: &mut [u16], map: &impl Fn(u32) -> u16) {
    let per = 64 / BITS;
    let mask = (1u64 << BITS) - 1;
    let mut chunks = out.chunks_exact_mut(per);
    let mut words = data.iter();
    for (chunk, word) in (&mut chunks).zip(&mut words) {
        let value = u64::from_be_bytes(*word);
        for (slot, out) in chunk.iter_mut().enumerate() {
            *out = map(((value >> (slot * BITS)) & mask) as u32);
        }
    }
    let rest = chunks.into_remainder();
    if let Some(word) = words.next() {
        let value = u64::from_be_bytes(*word);
        for (slot, out) in rest.iter_mut().enumerate() {
            *out = map(((value >> (slot * BITS)) & mask) as u32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_division_is_exact() {
        for bits in 1..=16u8 {
            let per = per_word(bits);
            for index in 0..MAX_ENTRIES {
                let (word, shift) = locate(index, bits);
                assert_eq!(word, index / per, "bits {bits} index {index}");
                assert_eq!(shift as usize, (index % per) * usize::from(bits));
            }
        }
    }

    #[test]
    fn layout_matches_simple_bit_storage() {
        // Three 5-bit values: 12 per word, value 0 in the lowest bits of word 0.
        let mut data = zeroed(4096, 5);
        set(&mut data, 5, 0, 0b10101);
        set(&mut data, 5, 1, 0b00011);
        set(&mut data, 5, 12, 0b11111);
        assert_eq!(u64::from_be_bytes(data[0]), 0b00011_10101);
        assert_eq!(u64::from_be_bytes(data[1]), 0b11111);
        assert_eq!(data.len(), 342);
        let packed = pack(4096, 5, |i| get(&data, 5, i));
        assert_eq!(packed, data);
        let mut seen = 0;
        for_each(&data, 5, 4096, |i, v| {
            assert_eq!(v, get(&data, 5, i));
            seen += 1;
        });
        assert_eq!(seen, 4096);
    }

    #[test]
    fn constant_widths_match_generic_loops() {
        for entries in [4096, 64, 27] {
            for bits in 1..=16u8 {
                let value = |i: usize| ((i * 7919 + 13) % (1 << bits)) as u32;
                let packed = pack(entries, bits, value);
                let mut generic = zeroed(entries, bits);
                pack_words_any(&mut generic, bits, entries, &mut |i| value(i));
                assert_eq!(packed, generic, "bits {bits} entries {entries}");
                let mut seen = Vec::new();
                for_each(&packed, bits, entries, |i, v| seen.push((i, v)));
                let mut expected = Vec::new();
                for_each_any(&packed, bits, entries, &mut |i, v| expected.push((i, v)));
                assert_eq!(seen, expected);
                assert_eq!(seen.len(), entries);
                let mut out = vec![0u16; entries];
                unpack(&packed, bits, &mut out, |v| v as u16);
                assert!(
                    out.iter()
                        .enumerate()
                        .all(|(i, &v)| u32::from(v) == value(i))
                );
            }
        }
    }
}
