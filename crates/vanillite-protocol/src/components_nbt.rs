//! Data component values between their wire form and their persistent NBT form (Vanilla 26.3
//! `DataComponentType.codec()` written with `NbtOps`), for item stacks in saved data
//! (`ItemStack.CODEC`: `{id, count, components: {"minecraft:<type>": …, "!minecraft:<type>": {}}}`).
//!
//! The server keeps component values as their wire bytes (see [`crate::item`]); saved data must
//! be what Vanilla writes so that worlds move between Vanilla and Vanillite without losing item
//! data. Every component type is described once as a [`S`] schema that is walked in both
//! directions, so the two conversions cannot drift apart:
//!
//! - records list their fields in stream codec order with the persistent field name and how
//!   the persistent codec treats a missing field (required, `optionalFieldOf` without a default
//!   = a wire `optional`, with a default = omitted when equal, `optionalAlwaysPresentFieldOf`);
//! - registry references become names, holders a name or an inline value, holder sets `#tag`,
//!   one name or a list of names, enums their serialized names;
//! - lists of bytes, ints and longs become array tags as `NbtOps` writes them.
//!
//! Types without a stream codec already travel as their persistent NBT tag and map directly.
//! Transient types (`creative_slot_lock`, `additional_trade_cost`, `map_post_processing`) have
//! no persistent form: Vanilla drops them on save.
//!
//! Values that do not fit the persistent codec (e.g. an inline painting variant, which Vanilla
//! can send but not save) fail to convert; callers keep such values in their own form. Values
//! read from NBT are checked with [`crate::components::validate`] before they are used.

use vanillite_data::registry::{DATA_COMPONENT_TYPE, Registry};
use vanillite_nbt::{Budget, Compound, List, Tag};

use crate::codec::{Error, Reader, Result, write_len, write_str, write_varint};
use crate::components::{MAX_NESTING, Registries};
use crate::types::MAX_STRING;

/// `CustomData.COMPOUND_TAG_CODEC`: a compound, or an SNBT string of one
/// (`TagParser.FLATTENED_CODEC`).
fn snbt_or_compound(tag: &Tag) -> Result<Compound> {
    match tag {
        Tag::Compound(c) => Ok(c.clone()),
        Tag::String(s) => {
            vanillite_nbt::snbt::parse_compound(s).map_err(|_| Error::Invalid("bad SNBT compound"))
        }
        _ => Err(Error::Invalid("expected compound tag")),
    }
}

/// Names of registry entries by id and back, for registries that are not static (the dynamic
/// registries of the data packs, e.g. `minecraft:enchantment`). Static registries are looked up
/// in [`vanillite_data::registry`] first.
pub trait RegistryNames: Registries {
    /// The name of entry `id` of `registry`.
    fn entry_name(&self, registry: &str, id: usize) -> Option<&str>;
    /// The id of entry `name` (a full identifier) of `registry`.
    fn entry_id(&self, registry: &str, name: &str) -> Option<usize>;
}

/// Most elements of one list (also bounds the tags of holder sets).
const MAX_ELEMENTS: usize = 1 << 16;
/// NBT limits of one embedded tag.
const NBT_QUOTA: u64 = vanillite_nbt::read::NETWORK_QUOTA;
const NBT_DEPTH: u32 = vanillite_nbt::read::MAX_DEPTH;

/// Whether a component type has no persistent codec (`DataComponentType.isTransient`).
pub fn is_transient(type_id: u16) -> bool {
    matches!(
        DATA_COMPONENT_TYPE.name(type_id),
        Some(
            "minecraft:creative_slot_lock"
                | "minecraft:additional_trade_cost"
                | "minecraft:map_post_processing"
        )
    )
}

/// The persistent NBT form of one component value in its wire form. `None` for transient
/// types and values without a persistent form.
pub fn to_nbt(type_id: u16, wire: &[u8], names: &dyn RegistryNames) -> Result<Tag> {
    let mut c = Conv { names, depth: 0 };
    let mut r = Reader::new(wire);
    let tag = c.value_to_nbt(type_id, &mut r)?;
    if r.remaining() == 0 {
        Ok(tag)
    } else {
        Err(Error::Invalid("trailing bytes after component value"))
    }
}

/// The wire form of one component value from its persistent NBT form, checked like a value
/// from a client.
pub fn from_nbt(type_id: u16, tag: &Tag, names: &dyn RegistryNames) -> Result<Box<[u8]>> {
    let mut c = Conv { names, depth: 0 };
    let mut out = Vec::new();
    c.value_to_wire(type_id, tag, &mut out)?;
    crate::components::validate(type_id, &out, names)?;
    Ok(out.into_boxed_slice())
}

/// The persistent key of a component type (`minecraft:<type>`).
pub fn type_name(type_id: u16) -> Option<&'static str> {
    DATA_COMPONENT_TYPE.name(type_id)
}

/// The component type of a persistent key (namespace optional).
pub fn type_by_name(name: &str) -> Option<u16> {
    DATA_COMPONENT_TYPE.id(&normalize(name))
}

// ---- schema ---------------------------------------------------------------------------------

/// The shape of a value in both forms.
#[derive(Clone, Copy, Debug)]
enum S {
    /// `Unit`: no bytes; `{}`.
    Unit,
    /// VarInt; int.
    VarInt,
    /// Fixed int; int.
    Int,
    /// Fixed int; int, or `[r, g, b]` floats read as an opaque colour
    /// (`ExtraCodecs.RGB_COLOR_CODEC`).
    Rgb,
    /// Float; float.
    Float,
    /// Double; double.
    Double,
    /// Boolean; byte 0/1.
    Bool,
    /// VarInt 0–255; byte (`ExtraCodecs.UNSIGNED_BYTE`).
    UByte,
    /// String; string.
    Str,
    /// Identifier; string.
    Id,
    /// One NBT tag on the wire (text components); the same tag.
    Tag,
    /// One NBT compound on the wire; the same compound.
    Compound,
    /// [`S::Compound`] whose persistent form may also be an SNBT string
    /// (`CustomData.COMPOUND_TAG_CODEC`).
    CustomData,
    /// 16 bytes; int array of 4 (`UUIDUtil.CODEC`).
    Uuid,
    /// Packed long; int array `[x, y, z]` (`BlockPos.CODEC`).
    BlockPos,
    /// Enum id `0..n`; its name.
    Enum(&'static [&'static str]),
    /// Enum with sparse ids; its name.
    Sparse(&'static [(i32, &'static str)]),
    /// Registry id; entry name.
    Reg(&'static str),
    /// Holder: VarInt 0 and an inline value, or id + 1; inline value or name.
    Holder(&'static str, Option<&'static S>),
    /// Holder set: VarInt 0 and a tag, or count + 1 and ids; `#tag`, a name or a list of names.
    HolderSet(&'static str),
    /// VarInt count and elements; a list.
    List(&'static S),
    /// VarInt count and elements; one element alone or a list (`compactListCodec`).
    Compact(&'static S),
    /// `n` elements; a list.
    Fixed(usize, &'static S),
    /// VarInt count and key–value pairs; a compound by key (keys are `Str` or `Reg`).
    Map(&'static S, &'static S),
    /// Fields in wire order; a compound.
    Record(&'static [F]),
    /// Boolean (true = left) and a value; the value (left tried first).
    Either(&'static S, &'static S),
    /// A record with a fallback: a bare (non-compound) value is the given field alone.
    OrBare(&'static str, &'static [F]),
    /// Type (registry id or enum id `0..n` if no registry) and its fields; a compound with
    /// `type`.
    Dispatch(
        Option<&'static str>,
        &'static [(&'static str, &'static [F])],
    ),
    /// `Filterable`: a value and an optional filtered one; `{raw, filtered}` or a bare value.
    Filterable(&'static S),
    /// `ItemStackTemplate`.
    Template,
    /// `TypedEntityData` of a registry: type id and compound; compound with `id`.
    Typed(&'static str),
    /// `ResolvableProfile`.
    Profile,
    /// `MobEffectInstance` with its hidden effects.
    Effect,
    /// `DataComponentExactPredicate`: typed component values; a compound by type.
    Exact,
    /// `DataComponentPredicate` map: predicate type or component type with an NBT value.
    Predicates,
    /// `ItemContainerContents`: optional templates by slot; `[{slot, item}]`.
    Container,
    /// `NbtPredicate`: a compound; SNBT text.
    NbtPredicate,
}

/// How the persistent codec treats a missing field.
#[derive(Clone, Copy, Debug)]
enum M {
    /// `fieldOf`: always there.
    Req,
    /// `optionalFieldOf` without a default: a wire `optional`.
    Opt,
    /// `optionalFieldOf(name, default)`: omitted when equal to the default.
    Def(D),
    /// `optionalAlwaysPresentFieldOf`: always written, the default when missing.
    Always(D),
    /// A map codec merged into the enclosing record (the field name is unused).
    Inline,
}

/// A default value in its NBT form.
#[derive(Clone, Copy, Debug)]
enum D {
    Bool(bool),
    Int(i32),
    Byte(i8),
    Float(f32),
    Str(&'static str),
    EmptyList,
    EmptyCompound,
    Fn(fn() -> Tag),
}

impl D {
    fn tag(self) -> Tag {
        match self {
            D::Bool(v) => Tag::Byte(i8::from(v)),
            D::Int(v) => Tag::Int(v),
            D::Byte(v) => Tag::Byte(v),
            D::Float(v) => Tag::Float(v),
            D::Str(v) => Tag::String(v.to_owned()),
            D::EmptyList => Tag::List(List::new()),
            D::EmptyCompound => Tag::Compound(Compound::new()),
            D::Fn(f) => f(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct F {
    name: &'static str,
    s: S,
    m: M,
}

const fn req(name: &'static str, s: S) -> F {
    F { name, s, m: M::Req }
}

const fn opt(name: &'static str, s: S) -> F {
    F { name, s, m: M::Opt }
}

const fn def(name: &'static str, s: S, d: D) -> F {
    F {
        name,
        s,
        m: M::Def(d),
    }
}

const fn always(name: &'static str, s: S, d: D) -> F {
    F {
        name,
        s,
        m: M::Always(d),
    }
}

const fn inline(fields: &'static [F]) -> F {
    F {
        name: "",
        s: S::Record(fields),
        m: M::Inline,
    }
}

// ---- enums ----------------------------------------------------------------------------------

const DYES: [&str; 16] = [
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];
const RARITY: [&str; 4] = ["common", "uncommon", "rare", "epic"];
const ANIMATIONS: [&str; 12] = [
    "none",
    "eat",
    "drink",
    "block",
    "bow",
    "trident",
    "crossbow",
    "spyglass",
    "toot_horn",
    "brush",
    "bundle",
    "spear",
];
const SWINGS: [&str; 3] = ["none", "whack", "stab"];
const SHAPES: [&str; 5] = ["small_ball", "large_ball", "star", "creeper", "burst"];
const OPERATIONS: [&str; 3] = ["add_value", "add_multiplied_base", "add_multiplied_total"];
/// `EquipmentSlotGroup` by id.
const SLOT_GROUPS: [&str; 11] = [
    "any", "mainhand", "offhand", "hand", "feet", "legs", "chest", "head", "armor", "body",
    "saddle",
];
/// `EquipmentSlot` by its network id.
const SLOTS: [&str; 8] = [
    "mainhand", "feet", "legs", "chest", "head", "offhand", "body", "saddle",
];
const FOX: [&str; 2] = ["red", "snow"];
const SALMON: [&str; 3] = ["small", "medium", "large"];
const PARROT: [&str; 5] = ["red_blue", "blue", "green", "yellow_blue", "gray"];
const MOOSHROOM: [&str; 2] = ["red", "brown"];
const HORSE: [&str; 7] = [
    "white",
    "creamy",
    "chestnut",
    "brown",
    "black",
    "gray",
    "dark_brown",
];
const LLAMA: [&str; 4] = ["creamy", "white", "brown", "gray"];
const AXOLOTL: [&str; 5] = ["lucy", "wild", "gold", "cyan", "blue"];
const RABBIT: [(i32, &str); 7] = [
    (0, "brown"),
    (1, "white"),
    (2, "black"),
    (3, "white_splotched"),
    (4, "gold"),
    (5, "salt"),
    (99, "evil"),
];
const TROPICAL_FISH: [(i32, &str); 12] = [
    (0, "kob"),
    (0x100, "sunstreak"),
    (0x200, "snooper"),
    (0x300, "dasher"),
    (0x400, "brinely"),
    (0x500, "spotty"),
    (1, "flopper"),
    (0x101, "stripey"),
    (0x201, "glitter"),
    (0x301, "blockfish"),
    (0x401, "betty"),
    (0x501, "clayfish"),
];

// ---- shared shapes --------------------------------------------------------------------------

const DYE: S = S::Enum(&DYES);
const SOUND: S = S::Holder(
    "minecraft:sound_event",
    Some(&S::Record(&[
        req("sound_id", S::Id),
        opt("range", S::Float),
    ])),
);
const CONSUME_EFFECT: S = S::Dispatch(
    Some("minecraft:consume_effect_type"),
    &[
        (
            "minecraft:apply_effects",
            &[
                req("effects", S::List(&S::Effect)),
                def("probability", S::Float, D::Float(1.0)),
            ],
        ),
        (
            "minecraft:remove_effects",
            &[req("effects", S::HolderSet("minecraft:mob_effect"))],
        ),
        ("minecraft:clear_all_effects", &[]),
        (
            "minecraft:teleport_randomly",
            &[
                def("diameter", S::Float, D::Float(16.0)),
                def("directional_particles", S::Bool, D::Bool(true)),
            ],
        ),
        ("minecraft:play_sound", &[req("sound", SOUND)]),
    ],
);
const CONSUME_EFFECTS: S = S::List(&CONSUME_EFFECT);
const BLOCK_PREDICATE: S = S::Record(&[
    opt("blocks", S::HolderSet("minecraft:block")),
    opt(
        "state",
        S::Map(
            &S::Str,
            &S::Either(
                &S::Str,
                &S::Record(&[opt("min", S::Str), opt("max", S::Str)]),
            ),
        ),
    ),
    opt("nbt", S::NbtPredicate),
    def("components", S::Exact, D::EmptyCompound),
    def("predicates", S::Predicates, D::EmptyCompound),
]);
const ADVENTURE: S = S::Compact(&BLOCK_PREDICATE);
const ATTRIBUTE_MODIFIER: S = S::Record(&[
    req("type", S::Reg("minecraft:attribute")),
    inline(&[
        req("id", S::Id),
        req("amount", S::Double),
        req("operation", S::Enum(&OPERATIONS)),
    ]),
    def("slot", S::Enum(&SLOT_GROUPS), D::Str("any")),
    def(
        "display",
        S::Dispatch(
            None,
            &[
                ("default", &[]),
                ("hidden", &[]),
                ("override", &[req("value", S::Tag)]),
            ],
        ),
        D::Fn(display_default),
    ),
]);
const DAMAGE_REDUCTION: S = S::Record(&[
    def("horizontal_blocking_angle", S::Float, D::Float(90.0)),
    opt("type", S::HolderSet("minecraft:damage_type")),
    req("base", S::Float),
    req("factor", S::Float),
]);
const KINETIC_CONDITION: S = S::Record(&[
    req("max_duration_ticks", S::VarInt),
    def("min_speed", S::Float, D::Float(0.0)),
    def("min_relative_speed", S::Float, D::Float(0.0)),
]);
const FIREWORK_EXPLOSION: S = S::Record(&[
    req("shape", S::Enum(&SHAPES)),
    def("colors", S::List(&S::Int), D::EmptyList),
    def("fade_colors", S::List(&S::Int), D::EmptyList),
    def("has_trail", S::Bool, D::Bool(false)),
    def("has_twinkle", S::Bool, D::Bool(false)),
]);
const TRIM_MATERIAL: S = S::Holder(
    "minecraft:trim_material",
    Some(&S::Record(&[
        req("palette_id", S::Id),
        req("description", S::Tag),
    ])),
);
const RESOLVABLE_INT: S = S::Either(&S::Int, &S::Id);
const RESOLVABLE_FLOAT: S = S::Either(&S::Float, &S::Id);
const SIGN_TEXT: S = S::Record(&[
    req("messages", S::Fixed(4, &S::Tag)),
    opt("filtered_messages", S::Fixed(4, &S::Tag)),
    always("color", DYE, D::Str("black")),
    always("has_glowing_text", S::Bool, D::Bool(false)),
]);

const TOOL_RULE: S = S::Record(&[
    req("blocks", S::HolderSet("minecraft:block")),
    opt("speed", S::Float),
    opt("correct_for_drops", S::Bool),
]);
const ITEM_DAMAGE: S = S::Record(&[
    req("threshold", S::Float),
    req("base", S::Float),
    req("factor", S::Float),
]);
const TRIM_PATTERN: S = S::Holder(
    "minecraft:trim_pattern",
    Some(&S::Record(&[
        req("asset_id", S::Id),
        req("description", S::Tag),
        always("decal", S::Bool, D::Bool(false)),
    ])),
);
const GLOBAL_POS: S = S::Record(&[req("dimension", S::Id), req("pos", S::BlockPos)]);
const BANNER_PATTERN: S = S::Holder(
    "minecraft:banner_pattern",
    Some(&S::Record(&[
        req("asset_id", S::Id),
        req("translation_key", S::Str),
    ])),
);

fn display_default() -> Tag {
    let mut c = Compound::new();
    c.insert("type", "default");
    Tag::Compound(c)
}

fn damage_reductions_default() -> Tag {
    let mut c = Compound::new();
    c.insert("base", 0.0f32);
    c.insert("factor", 1.0f32);
    Tag::List(List(vec![Tag::Compound(c)]))
}

fn item_damage_default() -> Tag {
    let mut c = Compound::new();
    c.insert("threshold", 1.0f32);
    c.insert("base", 0.0f32);
    c.insert("factor", 1.0f32);
    Tag::Compound(c)
}

/// The shape of a component type (name without `minecraft:`); `None` for transient types.
fn schema(name: &str) -> Option<S> {
    Some(match name {
        "unbreakable" | "glider" | "waxed" => const { S::Unit },
        "max_stack_size"
        | "max_damage"
        | "damage"
        | "repair_cost"
        | "map_id"
        | "ominous_bottle_amplifier" => const { S::VarInt },
        "villager_food" => const { S::Record(&[req("nutrition", S::VarInt)]) },
        "enchantable" => const { S::Record(&[req("value", S::VarInt)]) },
        "enchantment_glint_override" => const { S::Bool },
        "minimum_attack_charge" | "potion_duration_scale" => const { S::Float },
        "dyed_color" => const { S::Rgb },
        "custom_name" | "item_name" => const { S::Tag },
        "item_model" | "tooltip_style" | "note_block_sound" => const { S::Id },
        "lore" => const { S::List(&S::Tag) },
        "rarity" => const { S::Enum(&RARITY) },
        "enchantments" | "stored_enchantments" => {
            const { S::Map(&S::Reg("minecraft:enchantment"), &S::VarInt) }
        }
        "can_place_on" | "can_break" => const { ADVENTURE },
        "attribute_modifiers" => const { S::List(&ATTRIBUTE_MODIFIER) },
        "custom_model_data" => {
            const {
                S::Record(&[
                    def("floats", S::List(&S::Float), D::EmptyList),
                    def("flags", S::List(&S::Bool), D::EmptyList),
                    def("strings", S::List(&S::Str), D::EmptyList),
                    def("colors", S::List(&S::Rgb), D::EmptyList),
                ])
            }
        }
        "tooltip_display" => {
            const {
                S::Record(&[
                    def("hide_tooltip", S::Bool, D::Bool(false)),
                    def(
                        "hidden_components",
                        S::List(&S::Reg("minecraft:data_component_type")),
                        D::EmptyList,
                    ),
                ])
            }
        }
        "use_effects" => {
            const {
                S::Record(&[
                    def("can_sprint", S::Bool, D::Bool(false)),
                    def("interact_vibrations", S::Bool, D::Bool(true)),
                    def("speed_multiplier", S::Float, D::Float(0.2)),
                ])
            }
        }
        "damage_type" => const { S::Reg("minecraft:damage_type") },
        "food" => {
            const {
                S::Record(&[
                    req("nutrition", S::VarInt),
                    req("saturation", S::Float),
                    def("can_always_eat", S::Bool, D::Bool(false)),
                ])
            }
        }
        "consumable" => {
            const {
                S::Record(&[
                    def("consume_seconds", S::Float, D::Float(1.6)),
                    def("animation", S::Enum(&ANIMATIONS), D::Str("eat")),
                    def("sound", SOUND, D::Str("minecraft:entity.generic.eat")),
                    def("has_consume_particles", S::Bool, D::Bool(true)),
                    def("on_consume_effects", CONSUME_EFFECTS, D::EmptyList),
                ])
            }
        }
        "use_remainder" | "sulfur_cube_content" => const { S::Template },
        "use_cooldown" => {
            const { S::Record(&[req("seconds", S::Float), opt("cooldown_group", S::Id)]) }
        }
        "damage_resistant" => {
            const { S::Record(&[req("types", S::HolderSet("minecraft:damage_type"))]) }
        }
        "tool" => {
            const {
                S::Record(&[
                    req("rules", S::List(&TOOL_RULE)),
                    def("default_mining_speed", S::Float, D::Float(1.0)),
                    def("damage_per_block", S::VarInt, D::Int(1)),
                    def("can_destroy_blocks_in_creative", S::Bool, D::Bool(true)),
                ])
            }
        }
        "weapon" => {
            const {
                S::Record(&[
                    def("item_damage_per_attack", S::VarInt, D::Int(1)),
                    def("disable_blocking_for_seconds", S::Float, D::Float(0.0)),
                ])
            }
        }
        "attack_range" => {
            const {
                S::Record(&[
                    def("min_reach", S::Float, D::Float(0.0)),
                    def("max_reach", S::Float, D::Float(3.0)),
                    def("min_creative_reach", S::Float, D::Float(0.0)),
                    def("max_creative_reach", S::Float, D::Float(5.0)),
                    def("hitbox_margin", S::Float, D::Float(0.3)),
                    def("mob_factor", S::Float, D::Float(1.0)),
                ])
            }
        }
        "equippable" => {
            const {
                S::Record(&[
                    req("slot", S::Enum(&SLOTS)),
                    def(
                        "equip_sound",
                        SOUND,
                        D::Str("minecraft:item.armor.equip_generic"),
                    ),
                    opt("asset_id", S::Id),
                    opt("camera_overlay", S::Id),
                    opt("allowed_entities", S::HolderSet("minecraft:entity_type")),
                    def("dispensable", S::Bool, D::Bool(true)),
                    def("swappable", S::Bool, D::Bool(true)),
                    def("damage_on_hurt", S::Bool, D::Bool(true)),
                    def("equip_on_interact", S::Bool, D::Bool(false)),
                    def("can_be_sheared", S::Bool, D::Bool(false)),
                    def(
                        "shearing_sound",
                        SOUND,
                        D::Str("minecraft:item.shears.snip"),
                    ),
                ])
            }
        }
        "repairable" => const { S::Record(&[req("items", S::HolderSet("minecraft:item"))]) },
        "death_protection" => {
            const { S::Record(&[def("death_effects", CONSUME_EFFECTS, D::EmptyList)]) }
        }
        "blocks_attacks" => {
            const {
                S::Record(&[
                    def("block_delay_seconds", S::Float, D::Float(0.0)),
                    def("disable_cooldown_scale", S::Float, D::Float(1.0)),
                    def(
                        "damage_reductions",
                        S::List(&DAMAGE_REDUCTION),
                        D::Fn(damage_reductions_default),
                    ),
                    def("item_damage", ITEM_DAMAGE, D::Fn(item_damage_default)),
                    opt("bypassed_by", S::HolderSet("minecraft:damage_type")),
                    opt("block_sound", SOUND),
                    opt("disabled_sound", SOUND),
                ])
            }
        }
        "piercing_weapon" => {
            const {
                S::Record(&[
                    def("deals_knockback", S::Bool, D::Bool(true)),
                    def("dismounts", S::Bool, D::Bool(false)),
                    opt("sound", SOUND),
                    opt("hit_sound", SOUND),
                ])
            }
        }
        "kinetic_weapon" => {
            const {
                S::Record(&[
                    def("contact_cooldown_ticks", S::VarInt, D::Int(10)),
                    def("delay_ticks", S::VarInt, D::Int(0)),
                    opt("dismount_conditions", KINETIC_CONDITION),
                    opt("knockback_conditions", KINETIC_CONDITION),
                    opt("damage_conditions", KINETIC_CONDITION),
                    def("forward_movement", S::Float, D::Float(0.0)),
                    def("damage_multiplier", S::Float, D::Float(1.0)),
                    opt("sound", SOUND),
                    opt("hit_sound", SOUND),
                ])
            }
        }
        "attack_animation" | "interact_animation" => {
            const {
                S::Record(&[
                    def("type", S::Enum(&SWINGS), D::Str("whack")),
                    def("duration", S::VarInt, D::Int(6)),
                ])
            }
        }
        "block_transformer" => const { S::Reg("minecraft:block_transformer") },
        "dye"
        | "base_color"
        | "wolf/collar"
        | "tropical_fish/base_color"
        | "tropical_fish/pattern_color"
        | "cat/collar"
        | "sheep/color"
        | "shulker/color"
        | "cushion/color" => const { DYE },
        "charged_projectiles" | "bundle_contents" => const { S::List(&S::Template) },
        "potion_contents" => {
            const {
                S::OrBare(
                    "potion",
                    &[
                        opt("potion", S::Reg("minecraft:potion")),
                        opt("custom_color", S::Int),
                        def("custom_effects", S::List(&S::Effect), D::EmptyList),
                        opt("custom_name", S::Str),
                    ],
                )
            }
        }
        "suspicious_stew_effects" => {
            const {
                S::List(&S::Record(&[
                    req("id", S::Reg("minecraft:mob_effect")),
                    def("duration", S::VarInt, D::Int(160)),
                ]))
            }
        }
        "writable_book_content" => {
            const { S::Record(&[def("pages", S::List(&S::Filterable(&S::Str)), D::EmptyList)]) }
        }
        "written_book_content" => {
            const {
                S::Record(&[
                    req("title", S::Filterable(&S::Str)),
                    req("author", S::Str),
                    def("generation", S::VarInt, D::Int(0)),
                    def("pages", S::List(&S::Filterable(&S::Tag)), D::EmptyList),
                    def("resolved", S::Bool, D::Bool(false)),
                ])
            }
        }
        "trim" => {
            const { S::Record(&[req("material", TRIM_MATERIAL), req("pattern", TRIM_PATTERN)]) }
        }
        "provides_trim_material" => const { TRIM_MATERIAL },
        "entity_data" => const { S::Typed("minecraft:entity_type") },
        "block_entity_data" => const { S::Typed("minecraft:block_entity_type") },
        "instrument" => {
            const {
                S::Holder(
                    "minecraft:instrument",
                    Some(&S::Record(&[
                        req("sound_event", SOUND),
                        req("use_duration", S::Float),
                        req("range", S::Float),
                        def("durability_damage", S::VarInt, D::Int(0)),
                        req("description", S::Tag),
                    ])),
                )
            }
        }
        "jukebox_playable" => const { S::Holder("minecraft:jukebox_song", None) },
        "provides_banner_patterns" => const { S::HolderSet("minecraft:banner_pattern") },
        "lodestone_tracker" => {
            const {
                S::Record(&[
                    opt("target", GLOBAL_POS),
                    def("tracked", S::Bool, D::Bool(true)),
                ])
            }
        }
        "firework_explosion" => const { FIREWORK_EXPLOSION },
        "fireworks" => {
            const {
                S::Record(&[
                    def("flight_duration", S::UByte, D::Byte(0)),
                    def("explosions", S::List(&FIREWORK_EXPLOSION), D::EmptyList),
                ])
            }
        }
        "profile" => const { S::Profile },
        "banner_patterns" => {
            const {
                S::List(&S::Record(&[
                    req("pattern", BANNER_PATTERN),
                    req("color", DYE),
                ]))
            }
        }
        "pot_decorations" => {
            const {
                S::Record(&[
                    opt("back", S::Template),
                    opt("left", S::Template),
                    opt("right", S::Template),
                    opt("front", S::Template),
                ])
            }
        }
        "container" => const { S::Container },
        "block_state" => const { S::Map(&S::Str, &S::Str) },
        "bees" => {
            const {
                S::List(&S::Record(&[
                    req("entity_data", S::Typed("minecraft:entity_type")),
                    req("ticks_in_hive", S::VarInt),
                    req("min_ticks_in_hive", S::VarInt),
                ]))
            }
        }
        "break_sound" => const { SOUND },
        "compostable" => const { S::Record(&[req("layers", RESOLVABLE_INT)]) },
        "cooking_fuel" => {
            const {
                S::Record(&[
                    req("burn_time", RESOLVABLE_INT),
                    req("speed_multiplier", RESOLVABLE_FLOAT),
                ])
            }
        }
        "brewing_fuel" => {
            const {
                S::Record(&[
                    req("uses", RESOLVABLE_INT),
                    req("speed_multiplier", RESOLVABLE_FLOAT),
                ])
            }
        }
        "mob_visibility" => {
            const {
                S::Record(&[
                    req(
                        "targeting_entity_types",
                        S::HolderSet("minecraft:entity_type"),
                    ),
                    req("visibility", S::Float),
                ])
            }
        }
        "villager/variant" => const { S::Reg("minecraft:villager_type") },
        "wolf/variant" => const { S::Reg("minecraft:wolf_variant") },
        "wolf/sound_variant" => const { S::Reg("minecraft:wolf_sound_variant") },
        "pig/variant" => const { S::Reg("minecraft:pig_variant") },
        "pig/sound_variant" => const { S::Reg("minecraft:pig_sound_variant") },
        "cow/variant" => const { S::Reg("minecraft:cow_variant") },
        "cow/sound_variant" => const { S::Reg("minecraft:cow_sound_variant") },
        "chicken/variant" => const { S::Reg("minecraft:chicken_variant") },
        "chicken/sound_variant" => const { S::Reg("minecraft:chicken_sound_variant") },
        "zombie_nautilus/variant" => const { S::Reg("minecraft:zombie_nautilus_variant") },
        "frog/variant" => const { S::Reg("minecraft:frog_variant") },
        "cat/variant" => const { S::Reg("minecraft:cat_variant") },
        "cat/sound_variant" => const { S::Reg("minecraft:cat_sound_variant") },
        "provides_pottery_pattern" => const { S::Reg("minecraft:decorated_pot_pattern") },
        "painting/variant" => const { S::Holder("minecraft:painting_variant", None) },
        "fox/variant" => const { S::Enum(&FOX) },
        "salmon/size" => const { S::Enum(&SALMON) },
        "parrot/variant" => const { S::Enum(&PARROT) },
        "tropical_fish/pattern" => const { S::Sparse(&TROPICAL_FISH) },
        "mooshroom/variant" => const { S::Enum(&MOOSHROOM) },
        "rabbit/variant" => const { S::Sparse(&RABBIT) },
        "horse/variant" => const { S::Enum(&HORSE) },
        "llama/variant" => const { S::Enum(&LLAMA) },
        "axolotl/variant" => const { S::Enum(&AXOLOTL) },
        "sign_text_front" | "sign_text_back" => const { SIGN_TEXT },
        // No stream codec: the persistent tag itself goes on the wire.
        "custom_data" | "bucket_entity_data" => const { S::CustomData },
        "intangible_projectile" | "lock" => const { S::Compound },
        "map_decorations" | "debug_stick_state" | "recipes" | "container_loot" => const { S::Tag },
        _ => return None,
    })
}

// ---- helpers --------------------------------------------------------------------------------

/// `Identifier` parsing of a name: the namespace defaults to `minecraft`.
fn normalize(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    }
}

fn string(tag: &Tag) -> Result<&str> {
    tag.as_str().ok_or(Error::Invalid("expected string"))
}

fn int(tag: &Tag) -> Result<i32> {
    tag.as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or(Error::Invalid("expected int"))
}

/// `ExtraCodecs.RGB_COLOR_CODEC`: an int, or `[r, g, b]` channels from 0 to 1 made an opaque
/// colour (`ARGB.colorFromFloat(1, r, g, b)`).
#[allow(clippy::as_conversions)]
fn rgb(tag: &Tag) -> Result<i32> {
    if let Ok(v) = int(tag) {
        return Ok(v);
    }
    let channels = elements(tag)?;
    let [r, g, b] = channels.as_slice() else {
        return Err(Error::Invalid("expected int or 3 floats"));
    };
    // `ARGB.as8BitChannel`: `Mth.floor(v * 255)`, then the low 8 bits.
    let channel = |t: &Tag| float(t).map(|v| (v * 255.0).floor() as i32 as u8);
    Ok(i32::from_be_bytes([
        0xFF,
        channel(r)?,
        channel(g)?,
        channel(b)?,
    ]))
}

fn long(tag: &Tag) -> Result<i64> {
    tag.as_i64().ok_or(Error::Invalid("expected long"))
}

/// A numeric tag as a double (`Number.doubleValue()`).
#[allow(clippy::as_conversions)]
fn double(tag: &Tag) -> Result<f64> {
    // Widening integers to f64 is exact up to 2^53 and rounds like Java's conversion beyond.
    match *tag {
        Tag::Float(v) => Ok(f64::from(v)),
        Tag::Double(v) => Ok(v),
        Tag::Byte(v) => Ok(f64::from(v)),
        Tag::Short(v) => Ok(f64::from(v)),
        Tag::Int(v) => Ok(f64::from(v)),
        Tag::Long(v) => Ok(v as f64),
        _ => Err(Error::Invalid("expected number")),
    }
}

/// A numeric tag as a float (`Number.floatValue()`).
#[allow(clippy::as_conversions)]
fn float(tag: &Tag) -> Result<f32> {
    match *tag {
        Tag::Float(v) => Ok(v),
        // Narrowing rounds to nearest like Java's (float) cast.
        _ => double(tag).map(|v| v as f32),
    }
}

fn compound(tag: &Tag) -> Result<&Compound> {
    tag.as_compound().ok_or(Error::Invalid("expected compound"))
}

/// The elements of a list-like tag (`NbtOps.getStream`).
fn elements(tag: &Tag) -> Result<Vec<Tag>> {
    Ok(match tag {
        Tag::List(l) => l.0.clone(),
        Tag::ByteArray(a) => a.iter().map(|v| Tag::Byte(*v)).collect(),
        Tag::IntArray(a) => a.iter().map(|v| Tag::Int(*v)).collect(),
        Tag::LongArray(a) => a.iter().map(|v| Tag::Long(*v)).collect(),
        _ => return Err(Error::Invalid("expected list")),
    })
}

/// A list as `NbtOps.createList` builds it: arrays for bytes, ints and longs.
fn make_list(items: Vec<Tag>) -> Tag {
    if !items.is_empty() {
        if items.iter().all(|t| matches!(t, Tag::Byte(_))) {
            return Tag::ByteArray(
                items
                    .iter()
                    .filter_map(|t| match t {
                        Tag::Byte(v) => Some(*v),
                        _ => None,
                    })
                    .collect(),
            );
        }
        if items.iter().all(|t| matches!(t, Tag::Int(_))) {
            return Tag::IntArray(
                items
                    .iter()
                    .filter_map(|t| match t {
                        Tag::Int(v) => Some(*v),
                        _ => None,
                    })
                    .collect(),
            );
        }
        if items.iter().all(|t| matches!(t, Tag::Long(_))) {
            return Tag::LongArray(
                items
                    .iter()
                    .filter_map(|t| match t {
                        Tag::Long(v) => Some(*v),
                        _ => None,
                    })
                    .collect(),
            );
        }
    }
    Tag::List(List(items))
}

/// Equality as `Objects.equals` on the decoded values: floats by their bits.
fn same(a: &Tag, b: &Tag) -> bool {
    match (a, b) {
        (Tag::Float(x), Tag::Float(y)) => x.to_bits() == y.to_bits(),
        (Tag::Double(x), Tag::Double(y)) => x.to_bits() == y.to_bits(),
        (Tag::List(x), Tag::List(y)) => {
            x.0.len() == y.0.len() && x.0.iter().zip(&y.0).all(|(a, b)| same(a, b))
        }
        (Tag::Compound(x), Tag::Compound(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

fn write_bool(out: &mut Vec<u8>, v: bool) {
    out.push(u8::from(v));
}

fn write_tag(tag: &Tag, out: &mut Vec<u8>) -> Result<()> {
    vanillite_nbt::write::write_network(tag, out).map_err(Error::from)
}

fn read_tag(r: &mut Reader<'_>) -> Result<Tag> {
    let mut input = r.rest();
    let before = input.len();
    let mut budget = Budget::new(NBT_QUOTA, NBT_DEPTH);
    let tag = vanillite_nbt::read::read_network(&mut input, &mut budget)?;
    r.skip(before.saturating_sub(input.len()))?;
    tag.ok_or(Error::Invalid("expected non-empty tag"))
}

fn list_len(r: &mut Reader<'_>) -> Result<usize> {
    r.len_prefix("list", MAX_ELEMENTS, 1)
}

/// Packs a block position like `BlockPos.asLong`.
fn pack_pos(x: i32, y: i32, z: i32) -> i64 {
    ((i64::from(x) & 0x3FF_FFFF).wrapping_shl(38))
        | ((i64::from(z) & 0x3FF_FFFF).wrapping_shl(12))
        | (i64::from(y) & 0xFFF)
}

/// Unpacks a block position like `BlockPos.of(long)`.
fn unpack_pos(v: i64) -> [i32; 3] {
    let part = |shl: u32, shr: u32| -> i32 {
        i32::try_from(v.wrapping_shl(shl).wrapping_shr(shr)).unwrap_or_default()
    };
    [part(0, 38), part(52, 52), part(26, 38)]
}

fn uuid_from_tag(tag: &Tag) -> Result<u128> {
    match tag {
        Tag::IntArray(a) => {
            let [a, b, c, d] = <[i32; 4]>::try_from(a.as_slice())
                .map_err(|_| Error::Invalid("uuid int array length"))?;
            let bytes = [a, b, c, d].map(i32::to_be_bytes).concat();
            let bytes = <[u8; 16]>::try_from(bytes.as_slice())
                .map_err(|_| Error::Invalid("uuid int array length"))?;
            Ok(u128::from_be_bytes(bytes))
        }
        Tag::String(s) => {
            let hex: String = s.chars().filter(|c| *c != '-').collect();
            if hex.len() != 32 {
                return Err(Error::Invalid("uuid string"));
            }
            u128::from_str_radix(&hex, 16).map_err(|_| Error::Invalid("uuid string"))
        }
        _ => Err(Error::Invalid("expected uuid")),
    }
}

// ---- conversion -----------------------------------------------------------------------------

struct Conv<'n> {
    names: &'n dyn RegistryNames,
    depth: u32,
}

impl Conv<'_> {
    fn deeper(&mut self) -> Result<()> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_NESTING {
            Err(Error::Invalid("component values nested too deeply"))
        } else {
            Ok(())
        }
    }

    fn shallower(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn entry_name(&self, registry: &str, id: usize) -> Result<String> {
        let name = match Registry::by_name(registry) {
            Some(r) => u16::try_from(id).ok().and_then(|id| r.name(id)),
            None => self.names.entry_name(registry, id),
        };
        name.map(str::to_owned)
            .ok_or(Error::Invalid("unknown registry entry id"))
    }

    fn entry_id(&self, registry: &str, name: &str) -> Result<usize> {
        let name = normalize(name);
        let id = match Registry::by_name(registry) {
            Some(r) => r.id(&name).map(usize::from),
            None => self.names.entry_id(registry, &name),
        };
        id.ok_or(Error::Invalid("unknown registry entry name"))
    }

    fn write_entry_id(&self, registry: &str, name: &str, out: &mut Vec<u8>) -> Result<()> {
        let id = self.entry_id(registry, name)?;
        write_varint(
            out,
            i32::try_from(id).map_err(|_| Error::Invalid("registry id"))?,
        );
        Ok(())
    }

    fn read_entry_id(r: &mut Reader<'_>) -> Result<usize> {
        usize::try_from(r.varint()?).map_err(|_| Error::Invalid("registry id"))
    }

    // ---- wire → NBT ---------------------------------------------------------------------

    fn value_to_nbt(&mut self, type_id: u16, r: &mut Reader<'_>) -> Result<Tag> {
        let name = type_name(type_id)
            .and_then(|n| n.strip_prefix("minecraft:"))
            .ok_or(Error::Invalid("component type"))?;
        let s = schema(name).ok_or(Error::Invalid("transient component type"))?;
        self.nbt(s, r)
    }

    fn nbt(&mut self, s: S, r: &mut Reader<'_>) -> Result<Tag> {
        Ok(match s {
            S::Unit => Tag::Compound(Compound::new()),
            S::VarInt => Tag::Int(r.varint()?),
            S::Int | S::Rgb => Tag::Int(r.i32()?),
            S::Float => Tag::Float(r.f32()?),
            S::Double => Tag::Double(r.f64()?),
            S::Bool => Tag::Byte(i8::from(r.u8()? != 0)),
            S::UByte => {
                let v = u8::try_from(r.varint()?).map_err(|_| Error::Invalid("unsigned byte"))?;
                Tag::Byte(i8::from_ne_bytes([v]))
            }
            S::Str | S::Id => Tag::String(r.string(MAX_STRING)?.to_owned()),
            S::Tag => read_tag(r)?,
            S::Compound | S::CustomData => match read_tag(r)? {
                tag @ Tag::Compound(_) => tag,
                _ => return Err(Error::Invalid("expected compound tag")),
            },
            S::Uuid => Tag::IntArray(vanillite_nbt::snbt::uuid_to_ints(r.u128()?).to_vec()),
            S::BlockPos => Tag::IntArray(unpack_pos(r.i64()?).to_vec()),
            S::Enum(names) => {
                let id = usize::try_from(r.varint()?).map_err(|_| Error::Invalid("enum id"))?;
                Tag::String((*names.get(id).ok_or(Error::Invalid("enum id"))?).to_owned())
            }
            S::Sparse(names) => {
                let id = r.varint()?;
                let name = names
                    .iter()
                    .find(|(i, _)| *i == id)
                    .map(|(_, n)| *n)
                    .ok_or(Error::Invalid("enum id"))?;
                Tag::String(name.to_owned())
            }
            S::Reg(registry) => Tag::String(self.entry_name(registry, Self::read_entry_id(r)?)?),
            S::Holder(registry, direct) => {
                let id = Self::read_entry_id(r)?;
                match id.checked_sub(1) {
                    Some(id) => Tag::String(self.entry_name(registry, id)?),
                    None => {
                        let direct = direct.ok_or(Error::Invalid("inline holder not storable"))?;
                        self.nbt(*direct, r)?
                    }
                }
            }
            S::HolderSet(registry) => {
                let n = Self::read_entry_id(r)?;
                match n.checked_sub(1) {
                    None => Tag::String(format!("#{}", r.string(MAX_STRING)?)),
                    Some(count) => {
                        if count > MAX_ELEMENTS || count > r.remaining() {
                            return Err(Error::Invalid("holder set size"));
                        }
                        let mut items = Vec::with_capacity(count);
                        for _ in 0..count {
                            let id = Self::read_entry_id(r)?;
                            items.push(Tag::String(self.entry_name(registry, id)?));
                        }
                        match <[Tag; 1]>::try_from(items) {
                            Ok([one]) => one,
                            Err(items) => Tag::List(List(items)),
                        }
                    }
                }
            }
            S::List(e) => {
                let n = list_len(r)?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.nbt(*e, r)?);
                }
                make_list(items)
            }
            S::Compact(e) => {
                let n = list_len(r)?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.nbt(*e, r)?);
                }
                match <[Tag; 1]>::try_from(items) {
                    Ok([one]) => one,
                    Err(items) => make_list(items),
                }
            }
            S::Fixed(n, e) => {
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.nbt(*e, r)?);
                }
                make_list(items)
            }
            S::Map(k, v) => {
                let n = list_len(r)?;
                let mut c = Compound::with_capacity(n);
                for _ in 0..n {
                    let key = match self.nbt(*k, r)? {
                        Tag::String(key) => key,
                        _ => return Err(Error::Invalid("map key")),
                    };
                    let value = self.nbt(*v, r)?;
                    c.insert(key, value);
                }
                Tag::Compound(c)
            }
            S::Record(fields) | S::OrBare(_, fields) => {
                let mut c = Compound::new();
                self.fields_to_nbt(fields, r, &mut c)?;
                Tag::Compound(c)
            }
            S::Either(left, right) => {
                if r.u8()? != 0 {
                    self.nbt(*left, r)?
                } else {
                    self.nbt(*right, r)?
                }
            }
            S::Dispatch(registry, cases) => {
                let id = Self::read_entry_id(r)?;
                let (name, fields) = match registry {
                    Some(registry) => {
                        let name = self.entry_name(registry, id)?;
                        let fields = cases
                            .iter()
                            .find(|(n, _)| *n == name)
                            .map(|(_, f)| *f)
                            .ok_or(Error::Invalid("dispatch type"))?;
                        (name, fields)
                    }
                    None => {
                        let (name, fields) =
                            cases.get(id).ok_or(Error::Invalid("dispatch type"))?;
                        ((*name).to_owned(), *fields)
                    }
                };
                let mut c = Compound::new();
                c.insert("type", name);
                self.fields_to_nbt(fields, r, &mut c)?;
                Tag::Compound(c)
            }
            S::Filterable(e) => {
                let mut c = Compound::new();
                c.insert("raw", self.nbt(*e, r)?);
                if r.u8()? != 0 {
                    c.insert("filtered", self.nbt(*e, r)?);
                }
                Tag::Compound(c)
            }
            S::Template => {
                self.deeper()?;
                let result = self.template_to_nbt(r);
                self.shallower();
                result?
            }
            S::Typed(registry) => {
                let name = self.entry_name(registry, Self::read_entry_id(r)?)?;
                let Tag::Compound(mut c) = read_tag(r)? else {
                    return Err(Error::Invalid("expected compound tag"));
                };
                c.remove("id");
                c.insert("id", name);
                Tag::Compound(c)
            }
            S::Profile => self.profile_to_nbt(r)?,
            S::Effect => self.effect_to_nbt(r)?,
            S::Exact => {
                let n = list_len(r)?;
                let mut c = Compound::with_capacity(n);
                self.deeper()?;
                let result = (|| -> Result<()> {
                    for _ in 0..n {
                        let t = component_type(r)?;
                        let value = self.value_to_nbt(t, r)?;
                        if !is_transient(t) {
                            let name = type_name(t).ok_or(Error::Invalid("component type"))?;
                            c.insert(name, value);
                        }
                    }
                    Ok(())
                })();
                self.shallower();
                result?;
                Tag::Compound(c)
            }
            S::Predicates => {
                let n = list_len(r)?;
                let mut c = Compound::with_capacity(n);
                for _ in 0..n {
                    let predicate = r.u8()? != 0;
                    let id = Self::read_entry_id(r)?;
                    let name = if predicate {
                        self.entry_name("minecraft:data_component_predicate_type", id)?
                    } else {
                        let t = u16::try_from(id).map_err(|_| Error::Invalid("component type"))?;
                        type_name(t)
                            .ok_or(Error::Invalid("component type"))?
                            .to_owned()
                    };
                    c.insert(name, read_tag(r)?);
                }
                Tag::Compound(c)
            }
            S::Container => {
                let n = r.len_prefix("container slots", 256, 1)?;
                let mut items = Vec::new();
                for slot in 0..n {
                    if r.u8()? != 0 {
                        let mut c = Compound::new();
                        c.insert(
                            "slot",
                            i32::try_from(slot).map_err(|_| Error::Invalid("slot"))?,
                        );
                        c.insert("item", self.nbt(S::Template, r)?);
                        items.push(Tag::Compound(c));
                    }
                }
                Tag::List(List(items))
            }
            S::NbtPredicate => match read_tag(r)? {
                tag @ Tag::Compound(_) => Tag::String(vanillite_nbt::snbt::to_string(&tag)),
                _ => return Err(Error::Invalid("expected compound tag")),
            },
        })
    }

    fn fields_to_nbt(&mut self, fields: &[F], r: &mut Reader<'_>, c: &mut Compound) -> Result<()> {
        for f in fields {
            match f.m {
                M::Req | M::Always(_) => {
                    let value = self.nbt(f.s, r)?;
                    c.insert(f.name, value);
                }
                M::Opt => {
                    if r.u8()? != 0 {
                        let value = self.nbt(f.s, r)?;
                        c.insert(f.name, value);
                    }
                }
                M::Def(d) => {
                    let value = self.nbt(f.s, r)?;
                    if !same(&value, &d.tag()) {
                        c.insert(f.name, value);
                    }
                }
                M::Inline => {
                    if let S::Record(inner) = f.s {
                        self.fields_to_nbt(inner, r, c)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn template_to_nbt(&mut self, r: &mut Reader<'_>) -> Result<Tag> {
        let item = self.entry_name("minecraft:item", Self::read_entry_id(r)?)?;
        let count = r.varint()?;
        let mut c = Compound::new();
        c.insert("id", item);
        if count != 1 {
            c.insert("count", count);
        }
        let components = self.patch_to_nbt(r)?;
        if !components.is_empty() {
            c.insert("components", components);
        }
        Ok(Tag::Compound(c))
    }

    /// `DataComponentPatch` in its trusted wire form.
    fn patch_to_nbt(&mut self, r: &mut Reader<'_>) -> Result<Compound> {
        let added = r.len_prefix("added components", crate::components::TYPE_COUNT, 1)?;
        let removed = r.len_prefix("removed components", crate::components::TYPE_COUNT, 1)?;
        let mut c = Compound::new();
        for _ in 0..added {
            let t = component_type(r)?;
            let value = self.value_to_nbt(t, r)?;
            if !is_transient(t) {
                c.insert(type_name(t).ok_or(Error::Invalid("component type"))?, value);
            }
        }
        for _ in 0..removed {
            let t = component_type(r)?;
            if !is_transient(t) {
                let name = type_name(t).ok_or(Error::Invalid("component type"))?;
                c.insert(format!("!{name}"), Compound::new());
            }
        }
        Ok(c)
    }

    fn profile_to_nbt(&mut self, r: &mut Reader<'_>) -> Result<Tag> {
        let mut c = Compound::new();
        if r.u8()? != 0 {
            c.insert("id", self.nbt(S::Uuid, r)?);
            c.insert("name", r.string(MAX_STRING)?);
        } else {
            if r.u8()? != 0 {
                c.insert("name", r.string(MAX_STRING)?);
            }
            if r.u8()? != 0 {
                c.insert("id", self.nbt(S::Uuid, r)?);
            }
        }
        let properties = self.nbt(S::List(&PROPERTY), r)?;
        if !same(&properties, &D::EmptyList.tag()) {
            c.insert("properties", properties);
        }
        self.fields_to_nbt(SKIN_PATCH, r, &mut c)?;
        Ok(Tag::Compound(c))
    }

    fn effect_to_nbt(&mut self, r: &mut Reader<'_>) -> Result<Tag> {
        let id = self.entry_name("minecraft:mob_effect", Self::read_entry_id(r)?)?;
        let mut chain = Vec::new();
        loop {
            if chain.len() > crate::components::MAX_HIDDEN_EFFECTS {
                return Err(Error::Invalid("hidden mob effects nested too deeply"));
            }
            let amplifier = r.varint()?;
            let duration = r.varint()?;
            let ambient = r.u8()? != 0;
            let particles = r.u8()? != 0;
            let icon = r.u8()? != 0;
            let mut c = Compound::new();
            if amplifier != 0 {
                let v = u8::try_from(amplifier).map_err(|_| Error::Invalid("amplifier"))?;
                c.insert("amplifier", i8::from_ne_bytes([v]));
            }
            if duration != 0 {
                c.insert("duration", duration);
            }
            if ambient {
                c.insert("ambient", true);
            }
            if !particles {
                c.insert("show_particles", false);
            }
            c.insert("show_icon", icon);
            chain.push(c);
            if r.u8()? == 0 {
                break;
            }
        }
        let mut hidden: Option<Compound> = None;
        while let Some(mut c) = chain.pop() {
            if let Some(h) = hidden.take() {
                c.insert("hidden_effect", h);
            }
            hidden = Some(c);
        }
        let mut out = Compound::new();
        out.insert("id", id);
        for (k, v) in hidden.unwrap_or_default().iter() {
            out.insert(k, v.clone());
        }
        Ok(Tag::Compound(out))
    }

    // ---- NBT → wire ---------------------------------------------------------------------

    fn value_to_wire(&mut self, type_id: u16, tag: &Tag, out: &mut Vec<u8>) -> Result<()> {
        let name = type_name(type_id)
            .and_then(|n| n.strip_prefix("minecraft:"))
            .ok_or(Error::Invalid("component type"))?;
        let s = schema(name).ok_or(Error::Invalid("transient component type"))?;
        self.wire(s, tag, out)
    }

    fn wire(&mut self, s: S, tag: &Tag, out: &mut Vec<u8>) -> Result<()> {
        match s {
            S::Unit => {
                compound(tag)?;
            }
            S::VarInt => write_varint(out, int(tag)?),
            S::Int => out.extend_from_slice(&int(tag)?.to_be_bytes()),
            S::Rgb => out.extend_from_slice(&rgb(tag)?.to_be_bytes()),
            S::Float => out.extend_from_slice(&float(tag)?.to_be_bytes()),
            S::Double => out.extend_from_slice(&double(tag)?.to_be_bytes()),
            S::Bool => write_bool(out, long(tag)? != 0),
            S::UByte => write_varint(out, i32::from(long(tag)?.to_le_bytes()[0])),
            S::Str => write_str(out, string(tag)?),
            S::Id => write_str(out, &normalize(string(tag)?)),
            S::Tag => write_tag(tag, out)?,
            S::Compound => {
                compound(tag)?;
                write_tag(tag, out)?;
            }
            S::CustomData => write_tag(&Tag::Compound(snbt_or_compound(tag)?), out)?,
            S::Uuid => out.extend_from_slice(&uuid_from_tag(tag)?.to_be_bytes()),
            S::BlockPos => {
                let Tag::IntArray(a) = tag else {
                    return Err(Error::Invalid("expected block position"));
                };
                let [x, y, z] = <[i32; 3]>::try_from(a.as_slice())
                    .map_err(|_| Error::Invalid("block position length"))?;
                out.extend_from_slice(&pack_pos(x, y, z).to_be_bytes());
            }
            S::Enum(names) => {
                let name = string(tag)?;
                let id = names
                    .iter()
                    .position(|n| *n == name)
                    .ok_or(Error::Invalid("enum name"))?;
                write_varint(out, i32::try_from(id).map_err(|_| Error::Invalid("enum"))?);
            }
            S::Sparse(names) => {
                let name = string(tag)?;
                let id = names
                    .iter()
                    .find(|(_, n)| *n == name)
                    .map(|(i, _)| *i)
                    .ok_or(Error::Invalid("enum name"))?;
                write_varint(out, id);
            }
            S::Reg(registry) => self.write_entry_id(registry, string(tag)?, out)?,
            S::Holder(registry, direct) => match tag {
                Tag::String(name) => {
                    let id = self.entry_id(registry, name)?;
                    let id = i32::try_from(id)
                        .ok()
                        .and_then(|id| id.checked_add(1))
                        .ok_or(Error::Invalid("registry id"))?;
                    write_varint(out, id);
                }
                _ => {
                    let direct = direct.ok_or(Error::Invalid("inline holder not allowed"))?;
                    write_varint(out, 0);
                    self.wire(*direct, tag, out)?;
                }
            },
            S::HolderSet(registry) => {
                if let Tag::String(s) = tag
                    && let Some(tag_id) = s.strip_prefix('#')
                {
                    write_varint(out, 0);
                    write_str(out, &normalize(tag_id));
                    return Ok(());
                }
                let items = match tag {
                    Tag::String(_) => vec![tag.clone()],
                    _ => elements(tag)?,
                };
                let n = items
                    .len()
                    .checked_add(1)
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or(Error::Invalid("holder set size"))?;
                write_varint(out, n);
                for item in &items {
                    self.write_entry_id(registry, string(item)?, out)?;
                }
            }
            S::List(e) | S::Fixed(_, e) => {
                let items = elements(tag)?;
                if let S::Fixed(n, _) = s {
                    if items.len() != n {
                        return Err(Error::Invalid("fixed list length"));
                    }
                } else {
                    write_len(out, items.len());
                }
                for item in &items {
                    self.wire(*e, item, out)?;
                }
            }
            S::Compact(e) => {
                let items = match tag {
                    Tag::List(_) | Tag::ByteArray(_) | Tag::IntArray(_) | Tag::LongArray(_) => {
                        elements(tag)?
                    }
                    _ => vec![tag.clone()],
                };
                write_len(out, items.len());
                for item in &items {
                    self.wire(*e, item, out)?;
                }
            }
            S::Map(k, v) => {
                let c = compound(tag)?;
                write_len(out, c.len());
                for (key, value) in c.iter() {
                    self.wire(*k, &Tag::String(key.to_owned()), out)?;
                    self.wire(*v, value, out)?;
                }
            }
            S::Record(fields) => self.fields_to_wire(fields, compound(tag)?, out)?,
            S::OrBare(field, fields) => match tag {
                Tag::Compound(c) => self.fields_to_wire(fields, c, out)?,
                bare => {
                    let mut c = Compound::new();
                    c.insert(field, bare.clone());
                    self.fields_to_wire(fields, &c, out)?;
                }
            },
            S::Either(left, right) => {
                let mut buf = Vec::new();
                if self.wire(*left, tag, &mut buf).is_ok() {
                    write_bool(out, true);
                    out.extend_from_slice(&buf);
                } else {
                    write_bool(out, false);
                    self.wire(*right, tag, out)?;
                }
            }
            S::Dispatch(registry, cases) => {
                let c = compound(tag)?;
                let ty = string(c.get("type").ok_or(Error::Invalid("missing type"))?)?;
                let ty = match registry {
                    Some(_) => normalize(ty),
                    None => ty.to_owned(),
                };
                let index = cases
                    .iter()
                    .position(|(n, _)| *n == ty)
                    .ok_or(Error::Invalid("dispatch type"))?;
                let (_, fields) = cases.get(index).ok_or(Error::Invalid("dispatch type"))?;
                match registry {
                    Some(registry) => self.write_entry_id(registry, &ty, out)?,
                    None => write_len(out, index),
                }
                self.fields_to_wire(fields, c, out)?;
            }
            S::Filterable(e) => match tag {
                Tag::Compound(c) if c.get("raw").is_some() => {
                    self.wire(*e, c.get("raw").ok_or(Error::Invalid("raw"))?, out)?;
                    match c.get("filtered") {
                        Some(filtered) => {
                            write_bool(out, true);
                            self.wire(*e, filtered, out)?;
                        }
                        None => write_bool(out, false),
                    }
                }
                bare => {
                    self.wire(*e, bare, out)?;
                    write_bool(out, false);
                }
            },
            S::Template => {
                self.deeper()?;
                let result = self.template_to_wire(tag, out);
                self.shallower();
                result?;
            }
            S::Typed(registry) => {
                let c = &snbt_or_compound(tag)?;
                let ty = string(c.get("id").ok_or(Error::Invalid("missing id"))?)?;
                self.write_entry_id(registry, ty, out)?;
                let mut data = c.clone();
                data.remove("id");
                write_tag(&Tag::Compound(data), out)?;
            }
            S::Profile => self.profile_to_wire(tag, out)?,
            S::Effect => self.effect_to_wire(tag, out)?,
            S::Exact => {
                let c = compound(tag)?;
                write_len(out, c.len());
                self.deeper()?;
                let result = (|| -> Result<()> {
                    for (key, value) in c.iter() {
                        let t = type_by_name(key).ok_or(Error::Invalid("component type"))?;
                        write_varint(out, i32::from(t));
                        self.value_to_wire(t, value, out)?;
                    }
                    Ok(())
                })();
                self.shallower();
                result?;
            }
            S::Predicates => {
                let c = compound(tag)?;
                write_len(out, c.len());
                for (key, value) in c.iter() {
                    let key = normalize(key);
                    if let Ok(id) = self.entry_id("minecraft:data_component_predicate_type", &key) {
                        write_bool(out, true);
                        write_len(out, id);
                        write_tag(value, out)?;
                    } else {
                        let t = type_by_name(&key).ok_or(Error::Invalid("component type"))?;
                        write_bool(out, false);
                        write_varint(out, i32::from(t));
                        compound(value)?;
                        write_tag(value, out)?;
                    }
                }
            }
            S::Container => {
                let mut slots: Vec<Option<&Tag>> = Vec::new();
                let entries = elements(tag)?;
                for entry in &entries {
                    let c = compound(entry)?;
                    let slot =
                        usize::try_from(int(c.get("slot").ok_or(Error::Invalid("slot"))?)?)
                            .ok()
                            .filter(|s| *s < 256)
                            .ok_or(Error::Invalid("slot"))?;
                    let item = c.get("item").ok_or(Error::Invalid("missing item"))?;
                    if slots.len() <= slot {
                        slots.resize(slot.saturating_add(1), None);
                    }
                    if let Some(target) = slots.get_mut(slot) {
                        *target = Some(item);
                    }
                }
                write_len(out, slots.len());
                for slot in slots {
                    match slot {
                        Some(item) => {
                            write_bool(out, true);
                            self.wire(S::Template, item, out)?;
                        }
                        None => write_bool(out, false),
                    }
                }
            }
            S::NbtPredicate => match tag {
                Tag::String(s) => {
                    let c = vanillite_nbt::snbt::parse_compound(s)
                        .map_err(|_| Error::Invalid("nbt predicate"))?;
                    write_tag(&Tag::Compound(c), out)?;
                }
                Tag::Compound(_) => write_tag(tag, out)?,
                _ => return Err(Error::Invalid("nbt predicate")),
            },
        }
        Ok(())
    }

    fn fields_to_wire(&mut self, fields: &[F], c: &Compound, out: &mut Vec<u8>) -> Result<()> {
        for f in fields {
            match f.m {
                M::Req => {
                    let value = c.get(f.name).ok_or(Error::Invalid("missing field"))?;
                    self.wire(f.s, value, out)?;
                }
                M::Opt => match c.get(f.name) {
                    Some(value) => {
                        write_bool(out, true);
                        self.wire(f.s, value, out)?;
                    }
                    None => write_bool(out, false),
                },
                M::Def(d) | M::Always(d) => match c.get(f.name) {
                    Some(value) => self.wire(f.s, value, out)?,
                    None => self.wire(f.s, &d.tag(), out)?,
                },
                M::Inline => {
                    if let S::Record(inner) = f.s {
                        self.fields_to_wire(inner, c, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn template_to_wire(&mut self, tag: &Tag, out: &mut Vec<u8>) -> Result<()> {
        let (item, count, components) = match tag {
            Tag::String(item) => (item.as_str(), 1, None),
            Tag::Compound(c) => (
                string(c.get("id").ok_or(Error::Invalid("missing item id"))?)?,
                c.get("count").map(int).transpose()?.unwrap_or(1),
                c.get("components").map(compound).transpose()?,
            ),
            _ => return Err(Error::Invalid("item stack template")),
        };
        self.write_entry_id("minecraft:item", item, out)?;
        write_varint(out, count);
        self.patch_to_wire(components, out)
    }

    /// A persistent component map as a trusted wire patch (sorted by type id).
    fn patch_to_wire(&mut self, components: Option<&Compound>, out: &mut Vec<u8>) -> Result<()> {
        let mut added: Vec<(u16, Vec<u8>)> = Vec::new();
        let mut removed: Vec<u16> = Vec::new();
        for (key, value) in components.into_iter().flat_map(Compound::iter) {
            if let Some(name) = key.strip_prefix('!') {
                let t = type_by_name(name).ok_or(Error::Invalid("component type"))?;
                removed.push(t);
            } else {
                let t = type_by_name(key).ok_or(Error::Invalid("component type"))?;
                let mut value_bytes = Vec::new();
                self.value_to_wire(t, value, &mut value_bytes)?;
                added.push((t, value_bytes));
            }
        }
        added.sort_by_key(|(t, _)| *t);
        removed.sort_unstable();
        write_len(out, added.len());
        write_len(out, removed.len());
        for (t, value) in added {
            write_varint(out, i32::from(t));
            out.extend_from_slice(&value);
        }
        for t in removed {
            write_varint(out, i32::from(t));
        }
        Ok(())
    }

    fn profile_to_wire(&mut self, tag: &Tag, out: &mut Vec<u8>) -> Result<()> {
        let owned;
        let c = match tag {
            Tag::String(name) => {
                let mut c = Compound::new();
                c.insert("name", name.clone());
                owned = c;
                &owned
            }
            _ => compound(tag)?,
        };
        let name = c.get("name").map(string).transpose()?;
        let id = c.get("id").map(uuid_from_tag).transpose()?;
        match (id, name) {
            (Some(id), Some(name)) => {
                write_bool(out, true);
                out.extend_from_slice(&id.to_be_bytes());
                write_str(out, name);
            }
            (id, name) => {
                write_bool(out, false);
                match name {
                    Some(name) => {
                        write_bool(out, true);
                        write_str(out, name);
                    }
                    None => write_bool(out, false),
                }
                match id {
                    Some(id) => {
                        write_bool(out, true);
                        out.extend_from_slice(&id.to_be_bytes());
                    }
                    None => write_bool(out, false),
                }
            }
        }
        match c.get("properties") {
            // The map form: name → values without signatures.
            Some(Tag::Compound(map)) => {
                let mut list = Vec::new();
                for (name, values) in map.iter() {
                    for value in elements(values)? {
                        let mut p = Compound::new();
                        p.insert("name", name);
                        p.insert("value", value);
                        list.push(Tag::Compound(p));
                    }
                }
                self.wire(S::List(&PROPERTY), &Tag::List(List(list)), out)?;
            }
            Some(list) => self.wire(S::List(&PROPERTY), list, out)?,
            None => write_len(out, 0),
        }
        self.fields_to_wire(SKIN_PATCH, c, out)
    }

    fn effect_to_wire(&mut self, tag: &Tag, out: &mut Vec<u8>) -> Result<()> {
        let c = compound(tag)?;
        let id = string(c.get("id").ok_or(Error::Invalid("missing effect id"))?)?;
        self.write_entry_id("minecraft:mob_effect", id, out)?;
        let mut details = c;
        for _ in 0..=crate::components::MAX_HIDDEN_EFFECTS {
            let amplifier = details.get("amplifier").map(long).transpose()?.unwrap_or(0);
            write_varint(out, i32::from(amplifier.to_le_bytes()[0]));
            write_varint(
                out,
                details.get("duration").map(int).transpose()?.unwrap_or(0),
            );
            let flag = |name: &str, default: bool| -> Result<bool> {
                Ok(details
                    .get(name)
                    .map(long)
                    .transpose()?
                    .map_or(default, |v| v != 0))
            };
            let particles = flag("show_particles", true)?;
            write_bool(out, flag("ambient", false)?);
            write_bool(out, particles);
            write_bool(out, flag("show_icon", particles)?);
            match details.get("hidden_effect") {
                Some(hidden) => {
                    write_bool(out, true);
                    details = compound(hidden)?;
                }
                None => {
                    write_bool(out, false);
                    return Ok(());
                }
            }
        }
        Err(Error::Invalid("hidden mob effects nested too deeply"))
    }
}

/// A game profile property.
const PROPERTY: S = S::Record(&[
    req("name", S::Str),
    req("value", S::Str),
    opt("signature", S::Str),
]);

/// `PlayerSkin.Patch`.
const SKIN_PATCH: &[F] = &[
    opt("texture", S::Id),
    opt("cape", S::Id),
    opt("elytra", S::Id),
    // `PlayerModelType`: a boolean (slim) on the wire.
    opt("model", S::Enum(&["wide", "slim"])),
];

/// A data component type id on the wire.
fn component_type(r: &mut Reader<'_>) -> Result<u16> {
    u16::try_from(r.varint()?)
        .ok()
        .filter(|&t| usize::from(t) < crate::components::TYPE_COUNT)
        .ok_or(Error::Invalid("component type"))
}

#[cfg(test)]
mod tests;
