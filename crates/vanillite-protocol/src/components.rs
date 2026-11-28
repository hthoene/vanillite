//! Data component values on the wire: validation of client-sent values against the stream
//! codec of their type (Vanilla 26.3 `DataComponents`), with limits (SEC-04/20).
//!
//! The server keeps component values as their wire bytes and sends them back to clients, so
//! every value a client sends is parsed completely here first: a malformed value would
//! otherwise reach (and crash) other clients.
//!
//! Every type of `minecraft:data_component_type` is covered. Types that Vanilla registers
//! without a stream codec (`custom_data`, `intangible_projectile`, `map_decorations`,
//! `debug_stick_state`, `recipes`, `lock`, `container_loot`) still go on the wire: Vanilla
//! falls back to `ByteBufCodecs.fromCodecWithRegistries`, one NBT tag that the receiver parses
//! with the type's persistent codec. Their tags are checked against the shape of that codec.

use crate::codec::{DecodeLimited, Error, Reader, Result};
use crate::text::Component;
use crate::types::MAX_STRING;
use vanillite_data::registry::{DATA_COMPONENT_TYPE, Registry};
use vanillite_nbt::{Budget, Compound, Tag};

/// Number of data component types.
pub const TYPE_COUNT: usize = DATA_COMPONENT_TYPE.entries.len();

/// Most levels of component values nested in component values (item stacks in containers and
/// bundles, exact component predicates). Vanilla has no limit; nested bundles legitimately go
/// a few levels deep.
pub const MAX_NESTING: u32 = 16;

/// Limit for lists whose Vanilla stream codec has none (`ByteBufCodecs.list()`).
pub const MAX_LIST: usize = 1024;

/// Most hidden effects chained behind one mob effect instance. Vanilla's decoder recurses
/// without a limit; a long chain would overflow the stack of the receiving client.
pub const MAX_HIDDEN_EFFECTS: usize = 32;

/// NBT quota of one tag (`NbtAccounter.defaultQuota`, 2 MiB) and Vanilla's depth limit.
const NBT_QUOTA: u64 = vanillite_nbt::read::NETWORK_QUOTA;
const NBT_DEPTH: u32 = vanillite_nbt::read::MAX_DEPTH;

/// NBT type id of a compound.
const TAG_COMPOUND: u8 = 10;

/// Entry counts of the registries that component values refer to by id.
pub trait Registries {
    /// Number of entries of `registry` (e.g. `minecraft:enchantment`), static or dynamic;
    /// `None` if unknown.
    fn size(&self, registry: &str) -> Option<usize>;

    /// Whether the tag `tag` (an identifier without `#`) of `registry` exists. A Vanilla client
    /// fails to decode a holder set that names an unknown tag, so implementations that know the
    /// tags should answer here; the default accepts every tag.
    fn has_tag(&self, registry: &str, tag: &str) -> bool {
        let _ = (registry, tag);
        true
    }
}

/// Registry sizes from the static tables only (dynamic registries are unknown).
#[derive(Debug, Clone, Copy, Default)]
pub struct StaticRegistries;

impl Registries for StaticRegistries {
    fn size(&self, registry: &str) -> Option<usize> {
        Registry::by_name(registry).map(|r| r.len())
    }
}

/// Component type ids the server reads.
pub mod ids {
    use vanillite_data::registry::DATA_COMPONENT_TYPE;

    // Runs at compile time only.
    #[allow(
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::arithmetic_side_effects
    )]
    const fn find(name: &str) -> u16 {
        let entries = DATA_COMPONENT_TYPE.entries;
        let mut i = 0;
        while i < entries.len() {
            let e = entries[i].as_bytes();
            let n = name.as_bytes();
            if e.len() == n.len() {
                let mut j = 0;
                while j < e.len() && e[j] == n[j] {
                    j += 1;
                }
                if j == e.len() {
                    return i as u16;
                }
            }
            i += 1;
        }
        panic!("unknown data component type");
    }

    pub const MAX_STACK_SIZE: u16 = find("minecraft:max_stack_size");
    pub const MAX_DAMAGE: u16 = find("minecraft:max_damage");
    pub const DAMAGE: u16 = find("minecraft:damage");
    pub const UNBREAKABLE: u16 = find("minecraft:unbreakable");
    pub const DAMAGE_RESISTANT: u16 = find("minecraft:damage_resistant");
    pub const ENCHANTMENTS: u16 = find("minecraft:enchantments");
    pub const TOOL: u16 = find("minecraft:tool");
    pub const CAN_BREAK: u16 = find("minecraft:can_break");
    pub const CAN_PLACE_ON: u16 = find("minecraft:can_place_on");
    pub const BLOCKS_ATTACKS: u16 = find("minecraft:blocks_attacks");
    pub const CONSUMABLE: u16 = find("minecraft:consumable");
    pub const DEBUG_STICK_STATE: u16 = find("minecraft:debug_stick_state");
    pub const BLOCK_STATE: u16 = find("minecraft:block_state");
    pub const BLOCK_ENTITY_DATA: u16 = find("minecraft:block_entity_data");
    pub const CUSTOM_NAME: u16 = find("minecraft:custom_name");
    pub const ATTRIBUTE_MODIFIERS: u16 = find("minecraft:attribute_modifiers");
    pub const ENTITY_DATA: u16 = find("minecraft:entity_data");
    pub const CONTAINER: u16 = find("minecraft:container");
    pub const REPAIR_COST: u16 = find("minecraft:repair_cost");
    pub const STORED_ENCHANTMENTS: u16 = find("minecraft:stored_enchantments");
    pub const DYED_COLOR: u16 = find("minecraft:dyed_color");
    pub const POTION_CONTENTS: u16 = find("minecraft:potion_contents");
    pub const SUSPICIOUS_STEW_EFFECTS: u16 = find("minecraft:suspicious_stew_effects");
    pub const POTION_DURATION_SCALE: u16 = find("minecraft:potion_duration_scale");
    pub const BANNER_PATTERNS: u16 = find("minecraft:banner_patterns");
    pub const BASE_COLOR: u16 = find("minecraft:base_color");
    pub const POT_DECORATIONS: u16 = find("minecraft:pot_decorations");
    pub const WRITTEN_BOOK_CONTENT: u16 = find("minecraft:written_book_content");
    pub const TRIM: u16 = find("minecraft:trim");
    pub const CONTAINER_LOOT: u16 = find("minecraft:container_loot");
    pub const LOCK: u16 = find("minecraft:lock");
    pub const PROFILE: u16 = find("minecraft:profile");
    pub const NOTE_BLOCK_SOUND: u16 = find("minecraft:note_block_sound");
    pub const MAP_ID: u16 = find("minecraft:map_id");
    pub const MAP_POST_PROCESSING: u16 = find("minecraft:map_post_processing");
    pub const MAP_DECORATIONS: u16 = find("minecraft:map_decorations");
    pub const FIREWORKS: u16 = find("minecraft:fireworks");
    pub const FIREWORK_EXPLOSION: u16 = find("minecraft:firework_explosion");
    pub const WRITABLE_BOOK_CONTENT: u16 = find("minecraft:writable_book_content");
    pub const PROVIDES_BANNER_PATTERNS: u16 = find("minecraft:provides_banner_patterns");
    pub const JUKEBOX_PLAYABLE: u16 = find("minecraft:jukebox_playable");
    pub const ITEM_NAME: u16 = find("minecraft:item_name");
    pub const RARITY: u16 = find("minecraft:rarity");
    pub const BUNDLE_CONTENTS: u16 = find("minecraft:bundle_contents");
    pub const DYE: u16 = find("minecraft:dye");
    pub const INSTRUMENT: u16 = find("minecraft:instrument");
    pub const OMINOUS_BOTTLE_AMPLIFIER: u16 = find("minecraft:ominous_bottle_amplifier");
    pub const USE_EFFECTS: u16 = find("minecraft:use_effects");
    pub const FOOD: u16 = find("minecraft:food");
    pub const USE_REMAINDER: u16 = find("minecraft:use_remainder");
    pub const USE_COOLDOWN: u16 = find("minecraft:use_cooldown");
    pub const RECIPES: u16 = find("minecraft:recipes");
    pub const LODESTONE_TRACKER: u16 = find("minecraft:lodestone_tracker");
    pub const EQUIPPABLE: u16 = find("minecraft:equippable");
    pub const BEES: u16 = find("minecraft:bees");
    pub const SIGN_TEXT_FRONT: u16 = find("minecraft:sign_text_front");
    pub const SIGN_TEXT_BACK: u16 = find("minecraft:sign_text_back");
    pub const WAXED: u16 = find("minecraft:waxed");
    pub const CHARGED_PROJECTILES: u16 = find("minecraft:charged_projectiles");
    pub const INTANGIBLE_PROJECTILE: u16 = find("minecraft:intangible_projectile");
    pub const ENCHANTMENT_GLINT_OVERRIDE: u16 = find("minecraft:enchantment_glint_override");
    pub const CUSTOM_DATA: u16 = find("minecraft:custom_data");
    pub const BUCKET_ENTITY_DATA: u16 = find("minecraft:bucket_entity_data");
    pub const WEAPON: u16 = find("minecraft:weapon");
    pub const DAMAGE_TYPE: u16 = find("minecraft:damage_type");
    pub const ATTACK_ANIMATION: u16 = find("minecraft:attack_animation");
    pub const ENCHANTABLE: u16 = find("minecraft:enchantable");
    pub const REPAIRABLE: u16 = find("minecraft:repairable");
    pub const GLIDER: u16 = find("minecraft:glider");
    pub const PROVIDES_TRIM_MATERIAL: u16 = find("minecraft:provides_trim_material");
    pub const BLOCK_TRANSFORMER: u16 = find("minecraft:block_transformer");
    pub const COMPOSTABLE: u16 = find("minecraft:compostable");
    pub const COOKING_FUEL: u16 = find("minecraft:cooking_fuel");
    pub const VILLAGER_FOOD: u16 = find("minecraft:villager_food");
}

/// Validates one component value: `data` must be exactly one value of the stream codec of
/// component type `type_id`, within limits. Registry references must be in range of
/// `registries`. Unknown type ids are rejected.
pub fn validate(type_id: u16, data: &[u8], registries: &dyn Registries) -> Result<()> {
    let mut v = Validator {
        r: Reader::new(data),
        registries,
        depth: 0,
        refs: None,
        len: data.len(),
    };
    v.value(type_id)?;
    if v.r.remaining() == 0 {
        Ok(())
    } else {
        Err(Error::Invalid("trailing bytes after component value"))
    }
}

/// Length of the one value of component type `type_id` at the start of `data`, for reading
/// the trusted (undelimited) codec that servers send. Registry ids are not range checked.
pub fn value_len(type_id: u16, data: &[u8]) -> Result<usize> {
    struct Unchecked;
    impl Registries for Unchecked {
        fn size(&self, _: &str) -> Option<usize> {
            Some(usize::MAX)
        }
    }
    let mut v = Validator {
        r: Reader::new(data),
        registries: &Unchecked,
        depth: 0,
        refs: None,
        len: data.len(),
    };
    v.value(type_id)?;
    Ok(data.len().saturating_sub(v.r.remaining()))
}

/// A registry reference in a component value, found by [`registry_refs`] (for rewriting ids
/// between versions). Offsets are byte positions in the value. Events are in reading order: a
/// patch or component comes before the references inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ref {
    /// A registry id VarInt at `start..end`; `plus_one` for a holder (id + 1, 0 = direct).
    Id {
        registry: &'static str,
        start: usize,
        end: usize,
        id: i32,
        plus_one: bool,
    },
    /// A data component patch: its added and removed count VarInts; `end_event` is the index of
    /// the first event after the patch.
    Patch {
        added: (usize, usize),
        removed: (usize, usize),
        end_event: usize,
    },
    /// An added component of a patch: its type VarInt at `start..type_end`, its value up to
    /// `end`.
    Component {
        type_id: u16,
        start: usize,
        type_end: usize,
        end: usize,
    },
    /// A removed component type of a patch at `start..end`.
    Removed {
        type_id: u16,
        start: usize,
        end: usize,
    },
}

/// Like [`value_len`], and collects every registry reference of the value into `refs`.
pub fn registry_refs(type_id: u16, data: &[u8], refs: &mut Vec<Ref>) -> Result<usize> {
    struct Unchecked;
    impl Registries for Unchecked {
        fn size(&self, _: &str) -> Option<usize> {
            Some(usize::MAX)
        }
    }
    let mut v = Validator {
        r: Reader::new(data),
        registries: &Unchecked,
        depth: 0,
        refs: Some(refs),
        len: data.len(),
    };
    v.value(type_id)?;
    Ok(data.len().saturating_sub(v.r.remaining()))
}

// Enum sizes (`ByIdMap` id ranges) of the 26.3 enums that component values refer to by id.
const RARITY: u32 = 4;
const ITEM_USE_ANIMATION: u32 = 12;
const EQUIPMENT_SLOT: u32 = 8;
const EQUIPMENT_SLOT_GROUP: u32 = 11;
const ATTRIBUTE_OPERATION: u32 = 3;
const SWING_ANIMATION_TYPE: u32 = 3;
const DYE_COLOR: u32 = 16;
const MAP_POST_PROCESSING: u32 = 2;
const FIREWORK_SHAPE: u32 = 5;
const FOX_VARIANT: u32 = 2;
const SALMON_VARIANT: u32 = 3;
const PARROT_VARIANT: u32 = 5;
const MOOSHROOM_VARIANT: u32 = 2;
const HORSE_VARIANT: u32 = 7;
const LLAMA_VARIANT: u32 = 4;
const AXOLOTL_VARIANT: u32 = 5;
/// `Rabbit.Variant` ids (sparse: the killer bunny is 99).
const RABBIT_VARIANTS: [i32; 7] = [0, 1, 2, 3, 4, 5, 99];
/// `TropicalFish.Pattern` packed ids: `base | index << 8` for 2 bases and 6 patterns each.
const TROPICAL_FISH_PATTERNS: [i32; 12] = [
    0, 0x100, 0x200, 0x300, 0x400, 0x500, 1, 0x101, 0x201, 0x301, 0x401, 0x501,
];
/// `ItemAttributeModifiers.Display.Type`: default, hidden, override (with a text).
const DISPLAY_TYPE: u32 = 3;
const DISPLAY_OVERRIDE: u32 = 2;

/// A cursor over one component value with the registry context and the nesting depth.
struct Validator<'a, 'r> {
    r: Reader<'a>,
    registries: &'r dyn Registries,
    depth: u32,
    /// Collects registry references ([`registry_refs`]).
    refs: Option<&'r mut Vec<Ref>>,
    /// Length of the whole value (positions of references).
    len: usize,
}

impl<'a> Validator<'a, '_> {
    /// Position in the value.
    fn pos(&self) -> usize {
        self.len.saturating_sub(self.r.remaining())
    }

    /// Records a registry id read from `start` to here.
    fn record(&mut self, registry: &'static str, start: usize, id: i32, plus_one: bool) {
        let end = self.pos();
        if let Some(refs) = self.refs.as_mut() {
            refs.push(Ref::Id {
                registry,
                start,
                end,
                id,
                plus_one,
            });
        }
    }

    // ---- primitives -------------------------------------------------------------------------

    fn varint(&mut self) -> Result<i32> {
        self.r.varint()
    }

    /// `ByteBufCodecs.BOOL`. Vanilla writes 0 or 1; anything else is not from a Vanilla client.
    fn bool(&mut self) -> Result<bool> {
        match self.r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Invalid("boolean")),
        }
    }

    fn int(&mut self) -> Result<()> {
        self.r.i32().map(drop)
    }

    fn long(&mut self) -> Result<()> {
        self.r.i64().map(drop)
    }

    /// `ByteBufCodecs.FLOAT`; NaN and infinities are rejected.
    fn float(&mut self) -> Result<()> {
        if self.r.f32()?.is_finite() {
            Ok(())
        } else {
            Err(Error::Invalid("non-finite float"))
        }
    }

    /// `ByteBufCodecs.DOUBLE`; NaN and infinities are rejected.
    fn double(&mut self) -> Result<()> {
        if self.r.f64()?.is_finite() {
            Ok(())
        } else {
            Err(Error::Invalid("non-finite double"))
        }
    }

    fn uuid(&mut self) -> Result<()> {
        self.r.u128().map(drop)
    }

    /// `ByteBufCodecs.stringUtf8(max)`.
    fn string(&mut self, max: usize) -> Result<&'a str> {
        self.r.string(max)
    }

    /// `Identifier.STREAM_CODEC` (also `ResourceKey.streamCodec` and `ClientAsset`).
    fn identifier(&mut self) -> Result<&'a str> {
        let s = self.string(MAX_STRING)?;
        if valid_identifier(s) {
            Ok(s)
        } else {
            Err(Error::Invalid("identifier"))
        }
    }

    /// `ByteBufCodecs.idMapper` over an enum with ids `0..count`.
    fn enum_id(&mut self, count: u32, what: &'static str) -> Result<u32> {
        self.r.enum_id(count, what)
    }

    /// An enum with sparse ids.
    fn sparse_enum(&mut self, ids: &[i32], what: &'static str) -> Result<()> {
        if ids.contains(&self.varint()?) {
            Ok(())
        } else {
            Err(Error::Invalid(what))
        }
    }

    fn dye_color(&mut self) -> Result<()> {
        self.enum_id(DYE_COLOR, "dye color").map(drop)
    }

    // ---- combinators ------------------------------------------------------------------------

    /// `ByteBufCodecs.optional`: a boolean, then the value if true.
    fn optional(&mut self, f: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        if self.bool()? { f(self) } else { Ok(()) }
    }

    /// `ByteBufCodecs.list(max)` / `collection`: a VarInt count, then the elements, each at
    /// least `min_size` bytes.
    fn list(
        &mut self,
        what: &'static str,
        max: usize,
        min_size: usize,
        mut f: impl FnMut(&mut Self) -> Result<()>,
    ) -> Result<()> {
        let n = self.r.len_prefix(what, max, min_size)?;
        for _ in 0..n {
            f(self)?;
        }
        Ok(())
    }

    /// `ByteBufCodecs.fixedSizeList(n)`: exactly `n` elements without a count.
    fn fixed_list(&mut self, n: usize, mut f: impl FnMut(&mut Self) -> Result<()>) -> Result<()> {
        for _ in 0..n {
            f(self)?;
        }
        Ok(())
    }

    /// `Filterable.streamCodec`: the raw value and an optional filtered one.
    fn filterable(&mut self, mut f: impl FnMut(&mut Self) -> Result<()>) -> Result<()> {
        f(self)?;
        self.optional(f)
    }

    // ---- registries -------------------------------------------------------------------------

    /// Entry count of a registry: the static tables first, then the given registries.
    fn registry_size(&self, registry: &str) -> Result<usize> {
        Registry::by_name(registry)
            .map(Registry::len)
            .or_else(|| self.registries.size(registry))
            .ok_or(Error::Invalid("unknown registry"))
    }

    /// `ByteBufCodecs.registry` / `holderRegistry`: a VarInt id in the registry.
    fn registry_id(&mut self, registry: &'static str) -> Result<usize> {
        let size = self.registry_size(registry)?;
        let start = self.pos();
        let id = self.varint()?;
        self.record(registry, start, id, false);
        usize::try_from(id)
            .ok()
            .filter(|&id| id < size)
            .ok_or(Error::Invalid("registry id"))
    }

    /// `ByteBufCodecs.holder`: VarInt 0 and a direct value, or a registry id plus one.
    fn holder(
        &mut self,
        registry: &'static str,
        direct: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        let start = self.pos();
        let id = self.varint()?;
        if id == 0 {
            return direct(self);
        }
        self.record(registry, start, id, true);
        let size = self.registry_size(registry)?;
        usize::try_from(id)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .filter(|&id| id < size)
            .map(drop)
            .ok_or(Error::Invalid("registry id"))
    }

    /// `ByteBufCodecs.holderSet`: VarInt 0 and a tag, or the number of ids plus one and the ids.
    fn holder_set(&mut self, registry: &'static str) -> Result<()> {
        let size = self.registry_size(registry)?;
        let n = self.varint()?;
        if n == 0 {
            let tag = self.identifier()?;
            return if self.registries.has_tag(registry, tag) {
                Ok(())
            } else {
                Err(Error::Invalid("unknown tag"))
            };
        }
        let count = usize::try_from(n)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .ok_or(Error::NegativeLength)?;
        let max = size.max(MAX_LIST);
        if count > max {
            return Err(Error::TooLong {
                what: "holder set",
                len: count,
                max,
            });
        }
        if count > self.r.remaining() {
            return Err(Error::UnexpectedEnd);
        }
        for _ in 0..count {
            let start = self.pos();
            let id = self.varint()?;
            self.record(registry, start, id, false);
            usize::try_from(id)
                .ok()
                .filter(|&id| id < size)
                .ok_or(Error::Invalid("registry id"))?;
        }
        Ok(())
    }

    /// `SoundEvent.STREAM_CODEC`: a holder of `minecraft:sound_event` with a direct form of an
    /// identifier and an optional fixed range.
    fn sound(&mut self) -> Result<()> {
        self.holder("minecraft:sound_event", |v| {
            v.identifier()?;
            v.optional(Self::float)
        })
    }

    fn optional_sound(&mut self) -> Result<()> {
        self.optional(Self::sound)
    }

    // ---- NBT and text -----------------------------------------------------------------------

    /// `ByteBufCodecs.COMPOUND_TAG`: one network NBT compound, checked without building it.
    fn compound(&mut self) -> Result<()> {
        let rest = self.r.rest();
        if rest.first() != Some(&TAG_COMPOUND) {
            return Err(Error::Invalid("expected compound tag"));
        }
        let mut budget = Budget::new(NBT_QUOTA, NBT_DEPTH);
        let len = vanillite_nbt::read::skip_network(rest, &mut budget)?;
        self.r.skip(len)
    }

    /// `ByteBufCodecs.TAG`: one network NBT tag (not `TAG_End`).
    fn tag(&mut self) -> Result<Tag> {
        let mut input = self.r.rest();
        let before = input.len();
        let mut budget = Budget::new(NBT_QUOTA, NBT_DEPTH);
        let tag = vanillite_nbt::read::read_network(&mut input, &mut budget)?;
        self.r.skip(before.saturating_sub(input.len()))?;
        tag.ok_or(Error::Invalid("expected non-empty tag"))
    }

    /// `ComponentSerialization.STREAM_CODEC`: a text component as NBT.
    fn text(&mut self) -> Result<()> {
        let quota = usize::try_from(NBT_QUOTA).unwrap_or(usize::MAX);
        Component::decode_limited(&mut self.r, quota).map(drop)
    }

    fn optional_text(&mut self) -> Result<()> {
        self.optional(Self::text)
    }

    // ---- nested values ----------------------------------------------------------------------

    /// One level deeper; fails past [`MAX_NESTING`].
    fn nested(&mut self, f: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        let depth = self.depth.saturating_add(1);
        if depth > MAX_NESTING {
            return Err(Error::Invalid("component values nested too deeply"));
        }
        self.depth = depth;
        let result = f(self);
        self.depth = depth.saturating_sub(1);
        result
    }

    /// `ItemStackTemplate.STREAM_CODEC`: item, count and a component patch in the trusted
    /// (undelimited) form.
    fn item_template(&mut self) -> Result<()> {
        let item = self.registry_id("minecraft:item")?;
        if item == 0 {
            return Err(Error::Invalid("air in item stack template"));
        }
        let count = self.varint()?;
        if !(1..=99).contains(&count) {
            return Err(Error::Invalid("item stack template count"));
        }
        self.nested(Self::patch)
    }

    /// `DataComponentPatch.STREAM_CODEC`: added values without delimiters, then removed types.
    fn patch(&mut self) -> Result<()> {
        let p0 = self.pos();
        let added = self.r.len_prefix("added components", TYPE_COUNT, 1)?;
        let p1 = self.pos();
        let removed = self.r.len_prefix("removed components", TYPE_COUNT, 1)?;
        let p2 = self.pos();
        let patch_event = self.refs.as_mut().map(|refs| {
            refs.push(Ref::Patch {
                added: (p0, p1),
                removed: (p1, p2),
                end_event: 0,
            });
            refs.len().saturating_sub(1)
        });
        for _ in 0..added {
            let start = self.pos();
            let t = self.raw_component_type()?;
            let type_end = self.pos();
            let event = self.refs.as_mut().map(|refs| {
                refs.push(Ref::Component {
                    type_id: t,
                    start,
                    type_end,
                    end: type_end,
                });
                refs.len().saturating_sub(1)
            });
            self.value(t)?;
            let end = self.pos();
            if let (Some(refs), Some(i)) = (self.refs.as_mut(), event)
                && let Some(Ref::Component { end: e, .. }) = refs.get_mut(i)
            {
                *e = end;
            }
        }
        for _ in 0..removed {
            let start = self.pos();
            let t = self.raw_component_type()?;
            let end = self.pos();
            if let Some(refs) = self.refs.as_mut() {
                refs.push(Ref::Removed {
                    type_id: t,
                    start,
                    end,
                });
            }
        }
        if let (Some(refs), Some(i)) = (self.refs.as_mut(), patch_event) {
            let n = refs.len();
            if let Some(Ref::Patch { end_event, .. }) = refs.get_mut(i) {
                *end_event = n;
            }
        }
        Ok(())
    }

    /// `DataComponentType.STREAM_CODEC`.
    fn component_type(&mut self) -> Result<u16> {
        let start = self.pos();
        let t = self.raw_component_type()?;
        self.record("minecraft:data_component_type", start, i32::from(t), false);
        Ok(t)
    }

    fn raw_component_type(&mut self) -> Result<u16> {
        u16::try_from(self.varint()?)
            .ok()
            .filter(|&t| usize::from(t) < TYPE_COUNT)
            .ok_or(Error::Invalid("component type"))
    }

    // ---- shared structures ------------------------------------------------------------------

    /// `ItemEnchantments.STREAM_CODEC`: a map of enchantment ids to levels (0 to 255).
    fn enchantments(&mut self) -> Result<()> {
        let size = self.registry_size("minecraft:enchantment")?;
        self.list("enchantments", size, 2, |v| {
            v.registry_id("minecraft:enchantment")?;
            let level = v.varint()?;
            if (0..=255).contains(&level) {
                Ok(())
            } else {
                Err(Error::Invalid("enchantment level"))
            }
        })
    }

    /// `MobEffectInstance.STREAM_CODEC`: an effect and its details with their chain of hidden
    /// effects (decoded recursively by Vanilla, iteratively here).
    fn mob_effect_instance(&mut self) -> Result<()> {
        self.registry_id("minecraft:mob_effect")?;
        for _ in 0..=MAX_HIDDEN_EFFECTS {
            self.varint()?; // amplifier
            self.varint()?; // duration
            self.bool()?; // ambient
            self.bool()?; // show particles
            self.bool()?; // show icon
            if !self.bool()? {
                return Ok(());
            }
        }
        Err(Error::Invalid("hidden mob effects nested too deeply"))
    }

    /// `ConsumeEffect.STREAM_CODEC`: a `minecraft:consume_effect_type` and its fields.
    fn consume_effect(&mut self) -> Result<()> {
        let types = &vanillite_data::registry::CONSUME_EFFECT_TYPE;
        let id = self.registry_id(types.name)?;
        let name = u16::try_from(id).ok().and_then(|id| types.name(id));
        match name {
            Some("minecraft:apply_effects") => {
                self.list("effects", MAX_LIST, 1, Self::mob_effect_instance)?;
                self.float()
            }
            Some("minecraft:remove_effects") => self.holder_set("minecraft:mob_effect"),
            Some("minecraft:clear_all_effects") => Ok(()),
            Some("minecraft:teleport_randomly") => {
                self.float()?;
                self.bool().map(drop)
            }
            Some("minecraft:play_sound") => self.sound(),
            _ => Err(Error::Invalid("consume effect type")),
        }
    }

    fn consume_effects(&mut self) -> Result<()> {
        self.list("consume effects", MAX_LIST, 1, Self::consume_effect)
    }

    /// `AdventureModePredicate.STREAM_CODEC`: a list of `BlockPredicate`s.
    fn adventure_predicate(&mut self) -> Result<()> {
        self.list("block predicates", MAX_LIST, 4, Self::block_predicate)
    }

    /// `advancements.predicates.BlockPredicate.STREAM_CODEC`.
    fn block_predicate(&mut self) -> Result<()> {
        self.optional(|v| v.holder_set("minecraft:block"))?;
        self.optional(|v| {
            // StatePropertiesPredicate: (name, exact value | range of optional bounds).
            v.list("state properties", MAX_LIST, 2, |v| {
                v.string(MAX_STRING)?;
                if v.bool()? {
                    v.string(MAX_STRING).map(drop)
                } else {
                    v.optional(|v| v.string(MAX_STRING).map(drop))?;
                    v.optional(|v| v.string(MAX_STRING).map(drop))
                }
            })
        })?;
        self.optional(Self::compound)?;
        // DataComponentMatchers: exact components, then partial predicates.
        self.list("exact components", MAX_LIST, 1, |v| {
            let t = v.component_type()?;
            v.nested(|v| v.value(t))
        })?;
        self.list("component predicates", 64, 2, |v| {
            if v.bool()? {
                // A predicate type with a value in its persistent codec, as NBT. The codecs
                // differ per type (compounds and lists); only the tag itself is checked.
                v.registry_id("minecraft:data_component_predicate_type")?;
                v.tag().map(drop)
            } else {
                // "Any value" of a component type: a unit map codec, an (empty) compound.
                v.component_type()?;
                v.compound()
            }
        })
    }

    /// `ItemAttributeModifiers.Entry.STREAM_CODEC`.
    fn attribute_modifier(&mut self) -> Result<()> {
        self.registry_id("minecraft:attribute")?;
        // AttributeModifier: id, amount, operation.
        self.identifier()?;
        self.double()?;
        self.enum_id(ATTRIBUTE_OPERATION, "attribute operation")?;
        self.enum_id(EQUIPMENT_SLOT_GROUP, "equipment slot group")?;
        if self.enum_id(DISPLAY_TYPE, "attribute display type")? == DISPLAY_OVERRIDE {
            self.text()?;
        }
        Ok(())
    }

    /// `FireworkExplosion.STREAM_CODEC`.
    fn firework_explosion(&mut self) -> Result<()> {
        self.enum_id(FIREWORK_SHAPE, "firework shape")?;
        self.list("colors", MAX_LIST, 4, Self::int)?;
        self.list("fade colors", MAX_LIST, 4, Self::int)?;
        self.bool()?;
        self.bool().map(drop)
    }

    /// `ByteBufCodecs.GAME_PROFILE_PROPERTIES`.
    fn profile_properties(&mut self) -> Result<()> {
        self.list("profile properties", 16, 3, |v| {
            v.string(64)?;
            v.string(MAX_STRING)?;
            v.optional(|v| v.string(1024).map(drop))
        })
    }

    /// `ResolvableProfile.STREAM_CODEC`.
    fn resolvable_profile(&mut self) -> Result<()> {
        if self.bool()? {
            // Complete GameProfile: id, name, properties.
            self.uuid()?;
            self.string(16)?;
        } else {
            // Partial: optional name and id.
            self.optional(|v| v.string(16).map(drop))?;
            self.optional(Self::uuid)?;
        }
        self.profile_properties()?;
        // PlayerSkin.Patch: body, cape and elytra textures, model type.
        for _ in 0..3 {
            self.optional(|v| v.identifier().map(drop))?;
        }
        self.optional(|v| v.bool().map(drop))
    }

    /// `TypedEntityData.streamCodec`: a type id, then the data as a compound.
    fn typed_entity_data(&mut self, registry: &'static str) -> Result<()> {
        self.registry_id(registry)?;
        self.compound()
    }

    /// `ResolvableInt.STREAM_CODEC`: a constant or a `context_int_provider` key.
    fn resolvable_int(&mut self) -> Result<()> {
        if self.bool()? {
            self.int()
        } else {
            self.identifier().map(drop)
        }
    }

    /// `ResolvableFloat.STREAM_CODEC`: a constant or a `context_float_provider` key.
    fn resolvable_float(&mut self) -> Result<()> {
        if self.bool()? {
            self.float()
        } else {
            self.identifier().map(drop)
        }
    }

    /// `SignText.STREAM_CODEC`.
    fn sign_text(&mut self) -> Result<()> {
        self.fixed_list(4, Self::text)?;
        self.optional(|v| v.fixed_list(4, Self::text))?;
        self.dye_color()?;
        self.bool().map(drop)
    }

    // ---- component types --------------------------------------------------------------------

    /// One value of component type `type_id` (its 26.3 stream codec).
    fn value(&mut self, type_id: u16) -> Result<()> {
        let name = DATA_COMPONENT_TYPE
            .name(type_id)
            .and_then(|n| n.strip_prefix("minecraft:"))
            .ok_or(Error::Invalid("component type"))?;
        match name {
            // Unit.STREAM_CODEC: no bytes.
            "unbreakable" | "creative_slot_lock" | "glider" | "waxed" => Ok(()),

            "max_stack_size"
            | "max_damage"
            | "damage"
            | "repair_cost"
            | "additional_trade_cost"
            | "villager_food"
            | "ominous_bottle_amplifier"
            | "map_id" => self.varint().map(drop),
            "enchantable" => {
                if self.varint()? > 0 {
                    Ok(())
                } else {
                    Err(Error::Invalid("enchantable value"))
                }
            }
            "enchantment_glint_override" => self.bool().map(drop),
            "minimum_attack_charge" | "potion_duration_scale" => self.float(),
            "dyed_color" => self.int(),

            "custom_name" | "item_name" => self.text(),
            "item_model" | "tooltip_style" | "note_block_sound" => self.identifier().map(drop),
            "lore" => self.list("lore", 256, 1, Self::text),
            "rarity" => self.enum_id(RARITY, "rarity").map(drop),
            "enchantments" | "stored_enchantments" => self.enchantments(),
            "can_place_on" | "can_break" => self.adventure_predicate(),
            "attribute_modifiers" => {
                self.list("attribute modifiers", MAX_LIST, 4, Self::attribute_modifier)
            }
            "custom_model_data" => {
                self.list("floats", MAX_LIST, 4, Self::float)?;
                self.list("flags", MAX_LIST, 1, |v| v.bool().map(drop))?;
                self.list("strings", MAX_LIST, 1, |v| v.string(MAX_STRING).map(drop))?;
                self.list("colors", MAX_LIST, 4, Self::int)
            }
            "tooltip_display" => {
                self.bool()?;
                self.list("hidden components", MAX_LIST, 1, |v| {
                    v.component_type().map(drop)
                })
            }
            "use_effects" => {
                self.bool()?;
                self.bool()?;
                self.float()
            }
            "damage_type" => self.registry_id("minecraft:damage_type").map(drop),
            "food" => {
                self.varint()?;
                self.float()?;
                self.bool().map(drop)
            }
            "consumable" => {
                self.float()?;
                self.enum_id(ITEM_USE_ANIMATION, "item use animation")?;
                self.sound()?;
                self.bool()?;
                self.consume_effects()
            }
            "use_remainder" | "sulfur_cube_content" => self.item_template(),
            "use_cooldown" => {
                self.float()?;
                self.optional(|v| v.identifier().map(drop))
            }
            "damage_resistant" => self.holder_set("minecraft:damage_type"),
            "tool" => {
                self.list("tool rules", MAX_LIST, 3, |v| {
                    v.holder_set("minecraft:block")?;
                    v.optional(Self::float)?;
                    v.optional(|v| v.bool().map(drop))
                })?;
                self.float()?;
                self.varint()?;
                self.bool().map(drop)
            }
            "weapon" => {
                self.varint()?;
                self.float()
            }
            "attack_range" => {
                for _ in 0..6 {
                    self.float()?;
                }
                Ok(())
            }
            "equippable" => {
                self.enum_id(EQUIPMENT_SLOT, "equipment slot")?;
                self.sound()?;
                self.optional(|v| v.identifier().map(drop))?; // asset id
                self.optional(|v| v.identifier().map(drop))?; // camera overlay
                self.optional(|v| v.holder_set("minecraft:entity_type"))?;
                for _ in 0..5 {
                    self.bool()?;
                }
                self.sound()
            }
            "repairable" => self.holder_set("minecraft:item"),
            "death_protection" => self.consume_effects(),
            "blocks_attacks" => {
                self.float()?;
                self.float()?;
                self.list("damage reductions", MAX_LIST, 13, |v| {
                    v.float()?;
                    v.optional(|v| v.holder_set("minecraft:damage_type"))?;
                    v.float()?;
                    v.float()
                })?;
                for _ in 0..3 {
                    self.float()?;
                }
                self.optional(|v| v.holder_set("minecraft:damage_type"))?;
                self.optional_sound()?;
                self.optional_sound()
            }
            "piercing_weapon" => {
                self.bool()?;
                self.bool()?;
                self.optional_sound()?;
                self.optional_sound()
            }
            "kinetic_weapon" => {
                self.varint()?;
                self.varint()?;
                for _ in 0..3 {
                    self.optional(|v| {
                        v.varint()?;
                        v.float()?;
                        v.float()
                    })?;
                }
                self.float()?;
                self.float()?;
                self.optional_sound()?;
                self.optional_sound()
            }
            "attack_animation" | "interact_animation" => {
                self.enum_id(SWING_ANIMATION_TYPE, "swing animation")?;
                self.varint().map(drop)
            }
            "block_transformer" => self.registry_id("minecraft:block_transformer").map(drop),
            "dye"
            | "base_color"
            | "wolf/collar"
            | "tropical_fish/base_color"
            | "tropical_fish/pattern_color"
            | "cat/collar"
            | "sheep/color"
            | "shulker/color"
            | "cushion/color" => self.dye_color(),
            "map_post_processing" => self
                .enum_id(MAP_POST_PROCESSING, "map post processing")
                .map(drop),
            "charged_projectiles" => self.list("charged projectiles", 1024, 3, Self::item_template),
            "bundle_contents" => self.list("bundle contents", MAX_LIST, 3, Self::item_template),
            "potion_contents" => {
                self.optional(|v| v.registry_id("minecraft:potion").map(drop))?;
                self.optional(Self::int)?;
                self.list("custom effects", MAX_LIST, 7, Self::mob_effect_instance)?;
                self.optional(|v| v.string(MAX_STRING).map(drop))
            }
            "suspicious_stew_effects" => self.list("stew effects", MAX_LIST, 2, |v| {
                v.registry_id("minecraft:mob_effect")?;
                v.varint().map(drop)
            }),
            "writable_book_content" => self.list("pages", 100, 2, |v| {
                v.filterable(|v| v.string(1024).map(drop))
            }),
            "written_book_content" => {
                self.filterable(|v| v.string(32).map(drop))?;
                self.string(MAX_STRING)?;
                let generation = self.varint()?;
                if !(0..=3).contains(&generation) {
                    return Err(Error::Invalid("book generation"));
                }
                self.list("pages", MAX_LIST, 2, |v| v.filterable(Self::text))?;
                self.bool().map(drop)
            }
            "trim" => {
                self.holder("minecraft:trim_material", |v| {
                    v.identifier()?;
                    v.text()
                })?;
                self.holder("minecraft:trim_pattern", |v| {
                    v.identifier()?;
                    v.text()?;
                    v.bool().map(drop)
                })
            }
            "provides_trim_material" => self.holder("minecraft:trim_material", |v| {
                v.identifier()?;
                v.text()
            }),
            "entity_data" => self.typed_entity_data("minecraft:entity_type"),
            "block_entity_data" => self.typed_entity_data("minecraft:block_entity_type"),
            "bucket_entity_data" => self.compound(),
            "instrument" => self.holder("minecraft:instrument", |v| {
                v.sound()?;
                v.float()?; // use duration
                v.float()?; // range
                v.varint()?; // durability damage
                v.text()
            }),
            "jukebox_playable" => self.holder("minecraft:jukebox_song", |v| {
                v.sound()?;
                v.text()?;
                v.float()?; // length in seconds
                v.varint().map(drop) // comparator output
            }),
            "provides_banner_patterns" => self.holder_set("minecraft:banner_pattern"),
            "lodestone_tracker" => {
                self.optional(|v| {
                    v.identifier()?; // dimension
                    v.long() // block position
                })?;
                self.bool().map(drop)
            }
            "firework_explosion" => self.firework_explosion(),
            "fireworks" => {
                self.varint()?;
                self.list("explosions", 256, 5, Self::firework_explosion)
            }
            "profile" => self.resolvable_profile(),
            "banner_patterns" => self.list("banner layers", MAX_LIST, 2, |v| {
                v.holder("minecraft:banner_pattern", |v| {
                    v.identifier()?;
                    v.string(MAX_STRING).map(drop)
                })?;
                v.dye_color()
            }),
            "pot_decorations" => {
                for _ in 0..4 {
                    self.optional(Self::item_template)?;
                }
                Ok(())
            }
            "container" => self.list("container slots", 256, 1, |v| {
                v.optional(Self::item_template)
            }),
            "block_state" => self.list("block state properties", MAX_LIST, 2, |v| {
                v.string(MAX_STRING)?;
                v.string(MAX_STRING).map(drop)
            }),
            "bees" => self.list("bees", MAX_LIST, 4, |v| {
                v.typed_entity_data("minecraft:entity_type")?;
                v.varint()?; // ticks in hive
                v.varint().map(drop) // minimum ticks in hive
            }),
            "break_sound" => self.sound(),
            "compostable" => self.resolvable_int(),
            "cooking_fuel" | "brewing_fuel" => {
                self.resolvable_int()?;
                self.resolvable_float()
            }
            "mob_visibility" => {
                self.holder_set("minecraft:entity_type")?;
                self.float()
            }
            "villager/variant" => self.registry_id("minecraft:villager_type").map(drop),
            "wolf/variant" => self.registry_id("minecraft:wolf_variant").map(drop),
            "wolf/sound_variant" => self.registry_id("minecraft:wolf_sound_variant").map(drop),
            "pig/variant" => self.registry_id("minecraft:pig_variant").map(drop),
            "pig/sound_variant" => self.registry_id("minecraft:pig_sound_variant").map(drop),
            "cow/variant" => self.registry_id("minecraft:cow_variant").map(drop),
            "cow/sound_variant" => self.registry_id("minecraft:cow_sound_variant").map(drop),
            "chicken/variant" => self.registry_id("minecraft:chicken_variant").map(drop),
            "chicken/sound_variant" => self
                .registry_id("minecraft:chicken_sound_variant")
                .map(drop),
            "zombie_nautilus/variant" => self
                .registry_id("minecraft:zombie_nautilus_variant")
                .map(drop),
            "frog/variant" => self.registry_id("minecraft:frog_variant").map(drop),
            "cat/variant" => self.registry_id("minecraft:cat_variant").map(drop),
            "cat/sound_variant" => self.registry_id("minecraft:cat_sound_variant").map(drop),
            "provides_pottery_pattern" => self
                .registry_id("minecraft:decorated_pot_pattern")
                .map(drop),
            "fox/variant" => self.enum_id(FOX_VARIANT, "fox variant").map(drop),
            "salmon/size" => self.enum_id(SALMON_VARIANT, "salmon size").map(drop),
            "parrot/variant" => self.enum_id(PARROT_VARIANT, "parrot variant").map(drop),
            "tropical_fish/pattern" => {
                self.sparse_enum(&TROPICAL_FISH_PATTERNS, "tropical fish pattern")
            }
            "mooshroom/variant" => self
                .enum_id(MOOSHROOM_VARIANT, "mooshroom variant")
                .map(drop),
            "rabbit/variant" => self.sparse_enum(&RABBIT_VARIANTS, "rabbit variant"),
            "horse/variant" => self.enum_id(HORSE_VARIANT, "horse variant").map(drop),
            "llama/variant" => self.enum_id(LLAMA_VARIANT, "llama variant").map(drop),
            "axolotl/variant" => self.enum_id(AXOLOTL_VARIANT, "axolotl variant").map(drop),
            "painting/variant" => self.holder("minecraft:painting_variant", |v| {
                v.varint()?; // width
                v.varint()?; // height
                v.identifier()?;
                v.optional_text()?; // title
                v.optional_text() // author
            }),
            "sign_text_front" | "sign_text_back" => self.sign_text(),

            // Types without a stream codec: one NBT tag in the persistent codec's form.
            "custom_data" | "intangible_projectile" | "lock" => {
                // CustomData: a compound; Unit: a unit map codec, which needs a compound;
                // LockCode: an ItemPredicate record, a compound (its fields are not checked).
                self.compound()
            }
            "map_decorations" => {
                let tag = self.tag()?;
                check_map_decorations(&tag)
            }
            "debug_stick_state" => {
                let tag = self.tag()?;
                check_debug_stick_state(&tag)
            }
            "recipes" => {
                let tag = self.tag()?;
                check_recipes(&tag)
            }
            "container_loot" => {
                let tag = self.tag()?;
                check_container_loot(&tag)
            }

            _ => Err(Error::Invalid("component type without codec")),
        }
    }
}

/// Whether `s` parses as Vanilla's `Identifier` (`namespace:path`, namespace optional).
fn valid_identifier(s: &str) -> bool {
    let (namespace, path) = s.split_once(':').unwrap_or(("minecraft", s));
    namespace
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
        && path
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'/'))
}

// ---- persistent codecs over NBT (`NbtOps`) ----------------------------------------------------

fn compound_of(tag: &Tag) -> Result<&Compound> {
    tag.as_compound()
        .ok_or(Error::Invalid("expected compound tag"))
}

/// `Codec.STRING` of an identifier.
fn identifier_of(tag: &Tag) -> Result<&str> {
    tag.as_str()
        .filter(|s| valid_identifier(s))
        .ok_or(Error::Invalid("identifier"))
}

/// `Codec.FLOAT` / `Codec.DOUBLE` / `Codec.LONG`: any numeric tag; floats must be finite.
fn number_of(tag: Option<&Tag>) -> Result<()> {
    match tag {
        Some(Tag::Byte(_) | Tag::Short(_) | Tag::Int(_) | Tag::Long(_)) => Ok(()),
        Some(Tag::Float(v)) if v.is_finite() => Ok(()),
        Some(Tag::Double(v)) if v.is_finite() => Ok(()),
        _ => Err(Error::Invalid("expected finite number")),
    }
}

/// The namespaced form of an identifier for registry lookups.
fn with_namespace(id: &str) -> std::borrow::Cow<'_, str> {
    if id.contains(':') {
        std::borrow::Cow::Borrowed(id)
    } else {
        std::borrow::Cow::Owned(format!("minecraft:{id}"))
    }
}

/// `MapDecorations.CODEC`: a map of names to `{type, x, z, rotation}` with a
/// `minecraft:map_decoration_type` name.
fn check_map_decorations(tag: &Tag) -> Result<()> {
    for (_, entry) in compound_of(tag)?.iter() {
        let entry = compound_of(entry)?;
        let ty = identifier_of(entry.get("type").ok_or(Error::Invalid("map decoration"))?)?;
        vanillite_data::registry::MAP_DECORATION_TYPE
            .id(&with_namespace(ty))
            .ok_or(Error::Invalid("map decoration type"))?;
        number_of(entry.get("x"))?;
        number_of(entry.get("z"))?;
        number_of(entry.get("rotation"))?;
    }
    Ok(())
}

/// `DebugStickState.CODEC`: a map of block names to the name of one of their properties.
fn check_debug_stick_state(tag: &Tag) -> Result<()> {
    use vanillite_data::block::{BlockId, PROPERTIES};
    for (block, property) in compound_of(tag)?.iter() {
        if !valid_identifier(block) {
            return Err(Error::Invalid("identifier"));
        }
        let block = BlockId::by_name(&with_namespace(block)).ok_or(Error::Invalid("block"))?;
        let property = property.as_str().ok_or(Error::Invalid("block property"))?;
        let known = block
            .info()
            .properties
            .iter()
            .filter_map(|&p| PROPERTIES.get(usize::from(p)))
            .any(|p| p.name == property);
        if !known {
            return Err(Error::Invalid("block property"));
        }
    }
    Ok(())
}

/// `Recipe.KEY_CODEC.listOf()`: a list of recipe identifiers.
fn check_recipes(tag: &Tag) -> Result<()> {
    match tag {
        Tag::List(list) => list.0.iter().try_for_each(|t| identifier_of(t).map(drop)),
        _ => Err(Error::Invalid("expected list tag")),
    }
}

/// `SeededContainerLoot.CODEC`: `{loot_table: identifier, seed?: long}`.
fn check_container_loot(tag: &Tag) -> Result<()> {
    let c = compound_of(tag)?;
    identifier_of(c.get("loot_table").ok_or(Error::Invalid("loot table"))?)?;
    match c.get("seed") {
        None => Ok(()),
        seed => number_of(seed),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::codec::{Encode, write_varint};
    use crate::types::Nbt;

    /// Static registries plus 8 entries in every dynamic one.
    struct TestRegistries;

    impl Registries for TestRegistries {
        fn size(&self, registry: &str) -> Option<usize> {
            StaticRegistries.size(registry).or(Some(8))
        }

        fn has_tag(&self, _registry: &str, tag: &str) -> bool {
            tag != "minecraft:missing"
        }
    }

    fn id(name: &str) -> u16 {
        DATA_COMPONENT_TYPE.id(name).unwrap()
    }

    fn check(name: &str, data: &[u8]) -> Result<()> {
        validate(id(name), data, &TestRegistries)
    }

    fn varints(values: &[i32]) -> Vec<u8> {
        let mut out = Vec::new();
        for &v in values {
            write_varint(&mut out, v);
        }
        out
    }

    fn nbt(tag: Tag) -> Vec<u8> {
        Nbt(Some(tag)).to_bytes()
    }

    fn text(s: &str) -> Vec<u8> {
        nbt(Component::text(s).to_nbt())
    }

    fn item(name: &str) -> i32 {
        i32::from(vanillite_data::registry::ITEM.id(name).unwrap())
    }

    /// An item stack template with the given patch bytes.
    fn template(item_id: i32, count: i32, patch: &[u8]) -> Vec<u8> {
        let mut out = varints(&[item_id, count]);
        out.extend_from_slice(patch);
        out
    }

    #[test]
    fn every_type_has_a_codec() {
        for t in 0..TYPE_COUNT {
            let t = u16::try_from(t).unwrap();
            let r = validate(t, &[], &TestRegistries);
            assert_ne!(
                r,
                Err(Error::Invalid("component type without codec")),
                "{:?}",
                DATA_COMPONENT_TYPE.name(t)
            );
        }
    }

    #[test]
    fn simple_values() {
        assert_eq!(check("minecraft:max_stack_size", &[16]), Ok(()));
        assert_eq!(check("minecraft:damage", &varints(&[300])), Ok(()));
        assert_eq!(check("minecraft:unbreakable", &[]), Ok(()));
        assert_eq!(check("minecraft:enchantment_glint_override", &[1]), Ok(()));
        assert!(check("minecraft:enchantment_glint_override", &[2]).is_err());
        assert_eq!(check("minecraft:rarity", &[3]), Ok(()));
        assert!(check("minecraft:rarity", &[4]).is_err());
        assert_eq!(check("minecraft:rabbit/variant", &[99]), Ok(()));
        assert!(check("minecraft:rabbit/variant", &[6]).is_err());
        assert_eq!(
            check("minecraft:tropical_fish/pattern", &varints(&[0x501])),
            Ok(())
        );
        assert!(check("minecraft:tropical_fish/pattern", &varints(&[0x601])).is_err());
        assert_eq!(
            check("minecraft:item_model", &[5, b'a', b':', b'b', b'/', b'c']),
            Ok(())
        );
        assert!(check("minecraft:item_model", &[3, b'A', b':', b'b']).is_err());
        assert!(check("minecraft:enchantable", &[0]).is_err());
    }

    #[test]
    fn floats_must_be_finite() {
        let mut data = vec![1, 0];
        data.extend_from_slice(&1.5f32.to_be_bytes());
        assert_eq!(check("minecraft:use_effects", &data), Ok(()));
        let mut data = vec![1, 0];
        data.extend_from_slice(&f32::NAN.to_be_bytes());
        assert!(check("minecraft:use_effects", &data).is_err());
    }

    #[test]
    fn enchantments() {
        // Two entries: enchantment 3 at level 5, enchantment 7 at level 255.
        assert_eq!(
            check("minecraft:enchantments", &varints(&[2, 3, 5, 7, 255])),
            Ok(())
        );
        assert_eq!(check("minecraft:stored_enchantments", &[0]), Ok(()));
        // Level out of range, id out of range.
        assert!(check("minecraft:enchantments", &varints(&[1, 3, 256])).is_err());
        assert!(check("minecraft:enchantments", &varints(&[1, 8, 1])).is_err());
        // The dynamic registry is unknown to the static tables.
        assert!(validate(ids::ENCHANTMENTS, &varints(&[1, 0, 1]), &StaticRegistries).is_err());
    }

    #[test]
    fn text_components() {
        assert_eq!(check("minecraft:custom_name", &text("Sword")), Ok(()));
        let mut lore = vec![2];
        lore.extend(text("a"));
        lore.extend(text("b"));
        assert_eq!(check("minecraft:lore", &lore), Ok(()));
        // An empty tag is not a text component.
        assert!(check("minecraft:custom_name", &[0]).is_err());
    }

    #[test]
    fn nbt_values() {
        let compound = nbt(Tag::Compound(Compound::new().with("a", 1i32)));
        assert_eq!(check("minecraft:custom_data", &compound), Ok(()));
        assert_eq!(check("minecraft:bucket_entity_data", &compound), Ok(()));
        let mut entity = varints(&[1]);
        entity.extend_from_slice(&compound);
        assert_eq!(check("minecraft:entity_data", &entity), Ok(()));
        assert!(check("minecraft:custom_data", &nbt(Tag::String("x".into()))).is_err());

        let loot = Compound::new()
            .with("loot_table", "minecraft:chests/simple_dungeon")
            .with("seed", 5i64);
        assert_eq!(
            check("minecraft:container_loot", &nbt(Tag::Compound(loot))),
            Ok(())
        );
        let bad = Compound::new().with("loot_table", "Bad Name");
        assert!(check("minecraft:container_loot", &nbt(Tag::Compound(bad))).is_err());

        let stick = Compound::new().with("minecraft:oak_log", "axis");
        assert_eq!(
            check("minecraft:debug_stick_state", &nbt(Tag::Compound(stick))),
            Ok(())
        );
        let stick = Compound::new().with("minecraft:oak_log", "facing");
        assert!(check("minecraft:debug_stick_state", &nbt(Tag::Compound(stick))).is_err());

        let entry = Compound::new()
            .with("type", "minecraft:red_x")
            .with("x", 1.0f64)
            .with("z", 2.0f64)
            .with("rotation", 180.0f32);
        let decorations = Compound::new().with("+", entry);
        assert_eq!(
            check(
                "minecraft:map_decorations",
                &nbt(Tag::Compound(decorations))
            ),
            Ok(())
        );
    }

    #[test]
    fn potion_contents() {
        // Potion 5, no color, one effect (id 2, amplifier 1, 200 ticks, flags, no hidden
        // effect), no custom name.
        let data = [1, 5, 0, 1, 2, 1, 0xC8, 0x01, 0, 1, 1, 0, 0];
        assert_eq!(check("minecraft:potion_contents", &data), Ok(()));
        // A hidden effect chain.
        let data = [0, 0, 1, 2, 1, 1, 0, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0];
        assert_eq!(check("minecraft:potion_contents", &data), Ok(()));
        // Unknown potion id.
        let potions = i32::try_from(vanillite_data::registry::POTION.len()).unwrap();
        let mut data = vec![1];
        data.extend(varints(&[potions, 0, 0, 0]));
        assert!(check("minecraft:potion_contents", &data).is_err());
    }

    #[test]
    fn holder_sets() {
        // A tag, a direct set and an unknown tag.
        let mut tag = vec![0, 19];
        tag.extend_from_slice(b"minecraft:planks_ok");
        assert_eq!(check("minecraft:repairable", &tag), Ok(()));
        assert_eq!(check("minecraft:repairable", &varints(&[3, 1, 2])), Ok(()));
        let mut missing = vec![0, 17];
        missing.extend_from_slice(b"minecraft:missing");
        assert!(check("minecraft:repairable", &missing).is_err());
        let items = i32::try_from(vanillite_data::registry::ITEM.len()).unwrap();
        assert!(check("minecraft:repairable", &varints(&[2, items])).is_err());
    }

    #[test]
    fn container_with_nested_stacks() {
        // A shulker box with a damaged sword and an empty slot.
        let damage = id("minecraft:damage");
        let patch = varints(&[1, 0, i32::from(damage), 12]);
        let mut data = vec![2, 1];
        data.extend(template(item("minecraft:diamond_sword"), 1, &patch));
        data.push(0);
        assert_eq!(check("minecraft:container", &data), Ok(()));

        // A bundle holding a written book inside a container.
        // Title "T" without a filtered form, author "", generation 0, one page without a
        // filtered form, not resolved.
        let mut book = vec![1, b'T', 0, 0, 0, 1];
        book.extend(text("page"));
        book.extend_from_slice(&[0, 0]);
        let mut book_patch = varints(&[1, 0, i32::from(id("minecraft:written_book_content"))]);
        book_patch.extend(book);
        let mut bundle = vec![1];
        bundle.extend(template(item("minecraft:written_book"), 1, &book_patch));
        let mut bundle_patch = varints(&[1, 0, i32::from(id("minecraft:bundle_contents"))]);
        bundle_patch.extend(bundle);
        let mut data = vec![1, 1];
        data.extend(template(item("minecraft:bundle"), 1, &bundle_patch));
        assert_eq!(check("minecraft:container", &data), Ok(()));

        // Air and count 0 are not templates.
        let mut data = vec![1, 1];
        data.extend(template(0, 1, &[0, 0]));
        assert!(check("minecraft:container", &data).is_err());
        let mut data = vec![1, 1];
        data.extend(template(item("minecraft:stone"), 0, &[0, 0]));
        assert!(check("minecraft:container", &data).is_err());
    }

    #[test]
    fn nesting_is_limited() {
        let container = i32::from(id("minecraft:container"));
        let shulker = item("minecraft:shulker_box");
        let mut value = vec![0]; // an empty container
        for _ in 0..=MAX_NESTING {
            let mut patch = varints(&[1, 0, container]);
            patch.extend(&value);
            let mut next = vec![1, 1];
            next.extend(template(shulker, 1, &patch));
            value = next;
        }
        assert_eq!(
            check("minecraft:container", &value),
            Err(Error::Invalid("component values nested too deeply"))
        );
    }

    #[test]
    fn malformed_values() {
        // Truncated.
        assert_eq!(
            check("minecraft:max_stack_size", &[]),
            Err(Error::UnexpectedEnd)
        );
        assert!(check("minecraft:use_effects", &[1, 0, 0]).is_err());
        assert!(check("minecraft:custom_name", &text("abc")[..4]).is_err());
        // Trailing bytes.
        assert_eq!(
            check("minecraft:max_stack_size", &[16, 0]),
            Err(Error::Invalid("trailing bytes after component value"))
        );
        assert!(check("minecraft:unbreakable", &[0]).is_err());
        // Out-of-range registry id; dynamic registries the server does not know.
        assert!(check("minecraft:damage_type", &[8]).is_err());
        assert!(validate(id("minecraft:damage_type"), &[0], &StaticRegistries).is_err());
        // Unknown type.
        let unknown = u16::try_from(TYPE_COUNT).unwrap();
        assert!(validate(unknown, &[0], &TestRegistries).is_err());
        // Oversized lists.
        let mut lore = varints(&[257]);
        for _ in 0..257 {
            lore.extend(text(""));
        }
        assert!(matches!(
            check("minecraft:lore", &lore),
            Err(Error::TooLong { .. })
        ));
        let mut pages = vec![101];
        pages.extend(std::iter::repeat_n(0u8, 202));
        assert!(check("minecraft:writable_book_content", &pages).is_err());
        // A count larger than the input.
        assert_eq!(
            check("minecraft:custom_model_data", &varints(&[1000])),
            Err(Error::UnexpectedEnd)
        );
    }

    #[test]
    fn fireworks_and_banners() {
        // Flight 2, one star explosion with one color, no fade colors, trail.
        let mut data = vec![2, 1, 2, 1];
        data.extend_from_slice(&0x00FF_0000i32.to_be_bytes());
        data.extend_from_slice(&[0, 1, 0]);
        assert_eq!(check("minecraft:fireworks", &data), Ok(()));
        // Two layers: registered pattern 3 (id + 1) in red, a direct pattern in blue.
        let mut data = vec![2, 4, 14, 0, 9];
        data.extend_from_slice(b"minecraft");
        data.extend_from_slice(&[1, b'k', 11]);
        assert_eq!(check("minecraft:banner_patterns", &data), Ok(()));
    }

    #[test]
    fn profiles() {
        // A partial profile with a name, no id, no properties, default skin.
        let data = [0, 1, 3, b'B', b'o', b'b', 0, 0, 0, 0, 0, 0];
        assert_eq!(check("minecraft:profile", &data), Ok(()));
        // Name too long for a player name.
        let mut data = vec![0, 1, 17];
        data.extend_from_slice(&[b'a'; 17]);
        data.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        assert!(check("minecraft:profile", &data).is_err());
    }
}
