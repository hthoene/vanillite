//! Versioned Bedrock data (see `data/README.md`): per palette version the Java -> Bedrock block
//! state table, the item tables and the pre-encoded registries sent on join; shared by all
//! versions the biome ids, sounds and entity identifiers.

use std::sync::OnceLock;

use rustc_hash::FxHashMap;

use crate::items::Items;

static BIOMES: &str = include_str!("../data/biomes.txt");
static SOUNDS: &str = include_str!("../data/sounds.txt");
static PARTICLES: &str = include_str!("../data/particles.txt");
/// Body of `biome_definition_list`.
pub static BIOME_DEFINITIONS: &[u8] = include_bytes!("../data/biome_definitions.bin");
/// Body of `available_actor_identifiers`.
pub static ENTITY_IDENTIFIERS: &[u8] = include_bytes!("../data/entity_identifiers.bin");

/// The generated data of one Bedrock palette version (`data/<name>/`), shared by the protocols
/// that use it. Tables are built on first use.
#[derive(Debug)]
pub struct Palette {
    /// Palette version (directory name).
    pub name: &'static str,
    blocks_bin: &'static [u8],
    items_bin: &'static [u8],
    creative_java: &'static [u8],
    /// `block properties` of `start_game` (count + entries).
    pub block_properties: &'static [u8],
    /// Body of `item_registry`.
    pub item_registry: &'static [u8],
    /// Body of `creative_content`.
    pub creative: &'static [u8],
    /// Runtime ids of the frame blocks (`frames.bin`).
    frames_bin: &'static [u8],
    /// Bedrock block states of potted plants (`pots.bin`).
    pots_bin: &'static [u8],
    pots: OnceLock<FxHashMap<u16, &'static [u8]>>,
    /// Bedrock block states by runtime id (`states.bin`, compressed).
    states_bin: &'static [u8],
    states: OnceLock<States>,
    blocks: OnceLock<Blocks>,
    items: OnceLock<Items>,
}

macro_rules! palette {
    ($name:literal) => {
        Palette {
            name: $name,
            blocks_bin: include_bytes!(concat!("../data/", $name, "/blocks.bin")),
            items_bin: include_bytes!(concat!("../data/", $name, "/items.bin")),
            creative_java: include_bytes!(concat!("../data/", $name, "/creative_java.bin")),
            block_properties: include_bytes!(concat!("../data/", $name, "/block_properties.bin")),
            item_registry: include_bytes!(concat!("../data/", $name, "/item_registry.bin")),
            creative: include_bytes!(concat!("../data/", $name, "/creative.bin")),
            frames_bin: include_bytes!(concat!("../data/", $name, "/frames.bin")),
            pots_bin: include_bytes!(concat!("../data/", $name, "/pots.bin")),
            pots: OnceLock::new(),
            states_bin: include_bytes!(concat!("../data/", $name, "/states.bin")),
            states: OnceLock::new(),
            blocks: OnceLock::new(),
            items: OnceLock::new(),
        }
    };
}

/// Bedrock 1.26.40-1.26.45.
pub static V26_40: Palette = palette!("26_40");
/// Bedrock 1.26.50-1.26.52.
pub static V26_50: Palette = palette!("26_50");
/// Bedrock 1.26.60-1.26.66.
pub static V26_60: Palette = palette!("26_60");

impl Palette {
    /// The block state table.
    pub fn blocks(&self) -> &Blocks {
        self.blocks.get_or_init(|| Blocks::parse(self.blocks_bin))
    }

    /// The runtime id of the `frame` (`glow`: `glow_frame`) block facing `facing` (a Java 3D
    /// direction id, Bedrock's `facing_direction`), with the map bit for framed maps.
    pub fn frame_block(&self, glow: bool, map: bool, facing: u8) -> u32 {
        let i = usize::from(u8::from(glow).wrapping_mul(12))
            .wrapping_add(usize::from(u8::from(map).wrapping_mul(6)))
            .wrapping_add(usize::from(facing.min(5)));
        let at = i.wrapping_mul(2);
        match self.frames_bin.get(at..at.wrapping_add(2)) {
            Some(&[a, b]) => u32::from(u16::from_le_bytes([a, b])),
            _ => 0,
        }
    }

    /// The Bedrock block state (network NBT compound payload) of the plant in a Java potted
    /// plant state.
    pub fn potted_plant(&self, java: u32) -> Option<&'static [u8]> {
        let pots = self.pots.get_or_init(|| {
            let bin = self.pots_bin;
            let mut map = FxHashMap::default();
            let n = bin
                .get(..4)
                .and_then(|b| b.try_into().ok())
                .map_or(0, u32::from_le_bytes);
            let mut at = 4usize;
            for _ in 0..n {
                let Some(&[a, b, c, d]) = bin.get(at..at.saturating_add(4)) else {
                    break;
                };
                let start = at.saturating_add(4);
                let end = start.saturating_add(usize::from(u16::from_le_bytes([c, d])));
                let Some(payload) = bin.get(start..end) else {
                    break;
                };
                map.insert(u16::from_le_bytes([a, b]), payload);
                at = end;
            }
            map
        });
        pots.get(&u16::try_from(java).ok()?).copied()
    }

    /// The Bedrock block states by runtime id (unpacked on first use: pistons moving blocks).
    pub fn states(&self) -> &States {
        self.states.get_or_init(|| States::parse(self.states_bin))
    }

    /// The item tables.
    pub fn items(&self) -> &Items {
        self.items.get_or_init(|| {
            Items::parse(
                self.items_bin,
                self.creative_java,
                self.item_registry,
                self.blocks(),
            )
        })
    }
}

/// The Bedrock block states (network NBT payloads of name, states and version) by runtime id.
#[derive(Debug, Default)]
pub struct States {
    bytes: Vec<u8>,
    /// Start and end in `bytes` per runtime id.
    ranges: Vec<(u32, u32)>,
    /// Runtime id of `minecraft:moving_block`.
    pub moving_block: Option<u32>,
}

impl States {
    fn parse(compressed: &[u8]) -> States {
        use std::io::Read;
        let mut bytes = Vec::new();
        if flate2::read::ZlibDecoder::new(compressed)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return States::default();
        }
        let count = bytes
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .map_or(0, u32::from_le_bytes);
        let mut ranges = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
        let mut at = 4usize;
        for _ in 0..count {
            let Some(&[a, b]) = bytes.get(at..at.saturating_add(2)) else {
                break;
            };
            let start = at.saturating_add(2);
            let end = start.saturating_add(usize::from(u16::from_le_bytes([a, b])));
            if end > bytes.len() {
                break;
            }
            ranges.push((
                u32::try_from(start).unwrap_or(0),
                u32::try_from(end).unwrap_or(0),
            ));
            at = end;
        }
        let mut states = States {
            bytes,
            ranges,
            moving_block: None,
        };
        // The payload starts with the `name` string tag: 8, 4, "name", then the value.
        let wanted = b"minecraft:moving_block";
        states.moving_block = (0..states.ranges.len()).find_map(|i| {
            let p = states.get(u32::try_from(i).ok()?)?;
            let value = p.get(6..)?;
            (value.first() == Some(&22) && value.get(1..23) == Some(wanted.as_slice()))
                .then(|| u32::try_from(i).ok())
                .flatten()
        });
        states
    }

    /// The network NBT payload (entries and end tag) of a runtime id's block state.
    pub fn get(&self, runtime: u32) -> Option<&[u8]> {
        let &(start, end) = self.ranges.get(usize::try_from(runtime).ok()?)?;
        self.bytes
            .get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
    }
}

/// Bit of a [`Blocks`] entry: the state holds water (second layer).
pub const WATERLOGGED: u16 = 0x8000;

/// The Java -> Bedrock block state table.
#[derive(Debug)]
pub struct Blocks {
    table: Vec<u16>,
    /// Runtime id of still water.
    pub water: u32,
    /// Runtime id of air.
    pub air: u32,
}

impl Blocks {
    fn parse(bin: &[u8]) -> Blocks {
        let word = |i: usize| -> u32 {
            bin.get(i..i.saturating_add(4))
                .and_then(|b| b.try_into().ok())
                .map_or(0, u32::from_le_bytes)
        };
        let count = usize::try_from(word(0)).unwrap_or(0);
        let water = word(4);
        let table: Vec<u16> = bin
            .get(8..)
            .unwrap_or_default()
            .as_chunks::<2>()
            .0
            .iter()
            .take(count)
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let air = u32::from(table.first().copied().unwrap_or(0) & !WATERLOGGED);
        Blocks { table, water, air }
    }

    /// Entry of a Java state: runtime id and [`WATERLOGGED`]; unknown states become air.
    #[inline]
    pub fn entry(&self, java: u32) -> u16 {
        usize::try_from(java)
            .ok()
            .and_then(|i| self.table.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// Number of Java states.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.table.len()
    }
}

/// Bedrock biome id of a Java biome name (plains for unknown biomes).
pub fn biome_id(java: &str) -> u32 {
    static MAP: OnceLock<FxHashMap<&'static str, u32>> = OnceLock::new();
    MAP.get_or_init(|| {
        BIOMES
            .lines()
            .filter_map(|l| {
                let (name, id) = l.split_once(' ')?;
                Some((name, id.parse().ok()?))
            })
            .collect()
    })
    .get(java)
    .copied()
    .unwrap_or(1)
}

/// Bedrock sound name of a Java sound event.
pub fn sound_name(java: &str) -> Option<&'static str> {
    static MAP: OnceLock<FxHashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| SOUNDS.lines().filter_map(|l| l.split_once(' ')).collect())
        .get(java)
        .copied()
}

/// The Bedrock particle effect of a Java particle type, if Bedrock has a vanilla one.
pub fn particle_name(java: &str) -> Option<&'static str> {
    static MAP: OnceLock<FxHashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        PARTICLES
            .lines()
            .filter_map(|l| l.split_once(' '))
            .collect()
    })
    .get(java)
    .copied()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_java_26_3() {
        for p in [&V26_40, &V26_50, &V26_60] {
            assert_eq!(p.blocks().len(), 35_723, "{}", p.name);
            assert_eq!(
                p.items().len(),
                vanillite_data::item::ITEMS.len(),
                "{}",
                p.name
            );
        }
        let b = V26_60.blocks();
        // Java water (state 89) maps to Bedrock water, oak stairs waterlogged carry the flag.
        assert_eq!(u32::from(b.entry(89) & !WATERLOGGED), b.water);
        assert_ne!(b.entry(5463) & WATERLOGGED, 0);
        assert_eq!(b.entry(5464) & WATERLOGGED, 0);
        assert_eq!(biome_id("minecraft:plains"), 1);
        assert!(sound_name("minecraft:block.stone.break").is_some());
        assert_eq!(
            biome_id("minecraft:the_void"),
            biome_id("minecraft:the_void")
        );
    }

    /// `creative_content` parses to its end; the shield's user data carries its blocking tick
    /// (the client fails to decode the packet otherwise, #182).
    #[test]
    fn creative_content_parses() {
        use crate::io::Reader;
        let shield_java = vanillite_data::registry::ITEM
            .id("minecraft:shield")
            .unwrap();
        for p in [&V26_40, &V26_50, &V26_60] {
            let shield = p.items().bedrock(shield_java).0;
            let mut r = Reader::new(p.creative);
            let mut shields = 0;
            let mut item = |r: &mut Reader<'_>| {
                let id = r.vari32().unwrap();
                if id == 0 {
                    return;
                }
                r.take(2).unwrap();
                r.varu32().unwrap();
                r.vari32().unwrap();
                let user = r.bytes("user data", 1 << 16).unwrap();
                let expected = if id == i32::from(shield) { 18 } else { 10 };
                shields += usize::from(id == i32::from(shield));
                if user.starts_with(&[0xff, 0xff, 1]) {
                    // NBT (enchanted books, fireworks), then no place/destroy lists.
                    assert!(user.ends_with(&[0; 8]), "{} item {id}", p.name);
                    return;
                }
                assert_eq!(user.len(), expected, "{} item {id}", p.name);
            };
            for _ in 0..r.varu32().unwrap() {
                r.u8().unwrap();
                r.string("group", 1 << 16).unwrap();
                item(&mut r);
            }
            for _ in 0..r.varu32().unwrap() {
                r.varu32().unwrap();
                item(&mut r);
                r.varu32().unwrap();
            }
            assert_eq!(r.remaining(), 0, "{}", p.name);
            assert!(shields > 0, "{}", p.name);
        }
    }
}
