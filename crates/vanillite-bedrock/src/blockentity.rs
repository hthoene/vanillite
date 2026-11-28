//! Java block entities as Bedrock block actor data: sign texts, bed and banner colours (one
//! Bedrock block for all colours, the colour lives in the block actor), banner patterns, the
//! halves of double chests, skull rotations and the plants of flower pots (Bedrock keeps them in
//! the block actor; Java pots are blocks without a block entity), shulker box facings, piston
//! arms, decorated pot sherds, campfire items, spawner mobs and the book of lecterns.

use vanillite_data::block::BlockState;
use vanillite_nbt::{Compound as JCompound, Tag};

use crate::nbt::Compound;

/// Java `DyeColor` names in id order (Bedrock beds use the same ids, banners `15 - id`).
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

/// `DyeColor.getTextColor` per dye id.
const TEXT_COLORS: [u32; 16] = [
    0xFF_FFFF, 0xFF_681F, 0xFF_00FF, 0x9A_C0CD, 0xFF_FF00, 0xBF_FF00, 0xFF_69B4, 0x80_8080,
    0xD3_D3D3, 0x00_FFFF, 0xA0_20F0, 0x00_00FF, 0x8B_4513, 0x00_FF00, 0xFF_0000, 0x00_0000,
];

/// Java banner pattern -> Bedrock pattern code.
const PATTERNS: &[(&str, &str)] = &[
    ("base", "b"),
    ("square_bottom_left", "bl"),
    ("square_bottom_right", "br"),
    ("square_top_left", "tl"),
    ("square_top_right", "tr"),
    ("stripe_bottom", "bs"),
    ("stripe_top", "ts"),
    ("stripe_left", "ls"),
    ("stripe_right", "rs"),
    ("stripe_center", "cs"),
    ("stripe_middle", "ms"),
    ("stripe_downright", "drs"),
    ("stripe_downleft", "dls"),
    ("small_stripes", "ss"),
    ("cross", "cr"),
    ("straight_cross", "sc"),
    ("triangle_bottom", "bt"),
    ("triangle_top", "tt"),
    ("triangles_bottom", "bts"),
    ("triangles_top", "tts"),
    ("diagonal_left", "ld"),
    ("diagonal_up_right", "rd"),
    ("diagonal_up_left", "lud"),
    ("diagonal_right", "rud"),
    ("circle", "mc"),
    ("rhombus", "mr"),
    ("half_vertical", "vh"),
    ("half_horizontal", "hh"),
    ("half_vertical_right", "vhr"),
    ("half_horizontal_bottom", "hhb"),
    ("border", "bo"),
    ("curly_border", "cbo"),
    ("gradient", "gra"),
    ("gradient_up", "gru"),
    ("bricks", "bri"),
    ("globe", "glb"),
    ("creeper", "cre"),
    ("skull", "sku"),
    ("flower", "flo"),
    ("mojang", "moj"),
    ("piglin", "pig"),
    ("flow", "flw"),
    ("guster", "gus"),
];

/// Bedrock `SkullType` by Java skull block name prefix (floor and wall variants).
const SKULLS: [(&str, u8); 7] = [
    ("skeleton_", 0),
    ("wither_skeleton_", 1),
    ("zombie_", 2),
    ("player_", 3),
    ("creeper_", 4),
    ("dragon_", 5),
    ("piglin_", 6),
];

/// The Bedrock skull type of a Java skull or head block (floor or wall).
fn skull_type(block: &str) -> Option<u8> {
    let name = block.strip_prefix("minecraft:")?;
    let base = name
        .strip_suffix("wall_skull")
        .or_else(|| name.strip_suffix("wall_head"))
        .or_else(|| name.strip_suffix("skull"))
        .or_else(|| name.strip_suffix("head"))?;
    // The longest prefix wins (wither_skeleton_ before skeleton_).
    SKULLS
        .iter()
        .filter(|(p, _)| *p == base)
        .map(|(_, t)| *t)
        .next()
}

/// Whether a Java block is a flower pot (empty or with a plant).
fn is_pot(block: &str) -> bool {
    block == "minecraft:flower_pot" || block.starts_with("minecraft:potted_")
}

/// The Bedrock pattern code (`cre`) of a Java banner pattern (`minecraft:creeper`).
pub(crate) fn bedrock_pattern(java: &str) -> Option<&'static str> {
    let name = java.strip_prefix("minecraft:").unwrap_or(java);
    PATTERNS.iter().find(|(j, _)| *j == name).map(|(_, b)| *b)
}

/// The Java banner pattern (`minecraft:creeper`) of a Bedrock pattern code (`cre`).
pub fn java_pattern(code: &str) -> Option<String> {
    PATTERNS
        .iter()
        .find(|(_, b)| *b == code)
        .map(|(j, _)| format!("minecraft:{j}"))
}

/// The dye id of a Java dye item (`minecraft:red_dye` -> 14).
pub fn dye_of_item(item: &str) -> Option<usize> {
    let name = item.strip_prefix("minecraft:")?.strip_suffix("_dye")?;
    dye(name)
}

/// Most banner layers sent.
const MAX_LAYERS: usize = 16;

pub(crate) fn dye(name: &str) -> Option<usize> {
    DYES.iter().position(|d| *d == name)
}

/// The dye of a coloured block name (`red_bed` -> red).
fn dye_prefix(block: &str) -> Option<usize> {
    let name = block.strip_prefix("minecraft:").unwrap_or(block);
    DYES.iter()
        .position(|d| name.starts_with(d) && name.as_bytes().get(d.len()) == Some(&b'_'))
}

/// A player head block (floor or wall).
pub(crate) fn is_player_head(block: &str) -> bool {
    block == "minecraft:player_head" || block == "minecraft:player_wall_head"
}

/// The `textures` property value of a skull block entity's `profile`.
pub(crate) fn head_textures(nbt: &JCompound) -> Option<String> {
    let profile = nbt.get("profile")?.as_compound()?;
    let Some(Tag::List(properties)) = profile.get("properties") else {
        return None;
    };
    properties
        .0
        .iter()
        .filter_map(Tag::as_compound)
        .find(|p| p.get("name").and_then(Tag::as_str) == Some("textures"))
        .and_then(|p| p.get("value")?.as_str())
        .map(str::to_owned)
}

/// The Bedrock facing (`Direction` 3D data value: down, up, north, south, west, east).
fn facing_id(name: &str) -> Option<u8> {
    ["down", "up", "north", "south", "west", "east"]
        .iter()
        .position(|d| *d == name)
        .and_then(|i| u8::try_from(i).ok())
}

/// The plain text of a text component in NBT form.
fn plain(tag: &Tag, out: &mut String, depth: u8) {
    let Some(depth) = depth.checked_sub(1) else {
        return;
    };
    match tag {
        Tag::String(s) => out.push_str(s),
        Tag::List(l) => l.0.iter().for_each(|t| plain(t, out, depth)),
        Tag::Compound(c) => {
            match (c.get("text"), c.get("translate")) {
                (Some(Tag::String(s)), _) => out.push_str(s),
                (_, Some(Tag::String(key))) => {
                    out.push_str(c.get("fallback").and_then(Tag::as_str).unwrap_or(key));
                }
                _ => {}
            }
            if let Some(extra) = c.get("extra") {
                plain(extra, out, depth);
            }
        }
        _ => {}
    }
}

/// A Bedrock sign side from a Java `front_text`/`back_text`.
fn sign_side(side: Option<&JCompound>) -> Compound {
    let mut text = String::new();
    let mut color = 15usize;
    let mut glowing = false;
    if let Some(side) = side {
        if let Some(Tag::List(lines)) = side.get("messages") {
            for (i, line) in lines.0.iter().take(4).enumerate() {
                if i > 0 {
                    text.push('\n');
                }
                plain(line, &mut text, 8);
            }
        }
        color = side
            .get("color")
            .and_then(Tag::as_str)
            .and_then(dye)
            .unwrap_or(15);
        glowing = side
            .get("has_glowing_text")
            .and_then(Tag::as_i64)
            .is_some_and(|v| v != 0);
    }
    let argb = TEXT_COLORS.get(color).copied().unwrap_or(0) | 0xFF00_0000;
    let mut c = Compound::default();
    c.string("Text", &text)
        .int("SignTextColor", i32::from_ne_bytes(argb.to_ne_bytes()))
        .byte("IgnoreLighting", u8::from(glowing))
        .byte("HideGlowOutline", 0)
        .byte("PersistFormatting", 1)
        .string("TextOwner", "");
    c
}

/// The partner of a double chest half: its offset (x, z) and whether this half leads.
fn chest_pair(state: BlockState) -> Option<([i32; 2], bool)> {
    let kind = state.value("type")?;
    let facing = state.value("facing")?;
    // `ChestBlock.getConnectedDirection`: clockwise of the facing for the left half.
    let clockwise = match facing {
        "north" => [1, 0],
        "east" => [0, 1],
        "south" => [-1, 0],
        _ => [0, -1],
    };
    match kind {
        "left" => Some((clockwise, true)),
        "right" => Some((clockwise.map(|v: i32| v.saturating_neg()), false)),
        _ => None,
    }
}

/// The Bedrock block actor data (a root compound) for a Java block entity at `pos` with block
/// state `state` and its client NBT (`None` when only the block changed).
pub fn translate(
    pos: [i32; 3],
    state: u32,
    nbt: Option<&JCompound>,
    palette: &crate::data::Palette,
) -> Option<Vec<u8>> {
    let state = BlockState(u16::try_from(state).ok()?);
    if !state.is_valid() {
        return None;
    }
    let name = state.block().name();
    let [x, y, z] = pos;
    let mut c = Compound::default();
    if name.ends_with("_sign") {
        let id = if name.ends_with("hanging_sign") {
            "HangingSign"
        } else {
            "Sign"
        };
        let nbt = nbt?;
        let side = |k: &str| nbt.get(k).and_then(Tag::as_compound);
        c.string("id", id)
            .compound("FrontText", &sign_side(side("front_text")))
            .compound("BackText", &sign_side(side("back_text")))
            .byte(
                "IsWaxed",
                u8::from(
                    nbt.get("is_waxed")
                        .and_then(Tag::as_i64)
                        .is_some_and(|v| v != 0),
                ),
            );
    } else if name.ends_with("_bed") {
        let color = dye_prefix(name)?;
        c.string("id", "Bed")
            .byte("color", u8::try_from(color).unwrap_or(14));
    } else if name.ends_with("_banner") {
        let base = dye_prefix(name)?;
        let mut layers = Vec::new();
        if let Some(Tag::List(list)) = nbt.and_then(|n| n.get("patterns")) {
            for layer in list.0.iter().filter_map(Tag::as_compound).take(MAX_LAYERS) {
                let pattern = layer
                    .get("pattern")
                    .and_then(Tag::as_str)
                    .map(|p| p.strip_prefix("minecraft:").unwrap_or(p))
                    .and_then(|p| PATTERNS.iter().find(|(j, _)| *j == p))
                    .map(|(_, b)| *b);
                let color = layer.get("color").and_then(Tag::as_str).and_then(dye);
                if let (Some(pattern), Some(color)) = (pattern, color) {
                    let mut l = Compound::default();
                    l.string("Pattern", pattern).int(
                        "Color",
                        15i32.saturating_sub(i32::try_from(color).unwrap_or(0)),
                    );
                    layers.push(l);
                }
            }
        }
        c.string("id", "Banner")
            .int(
                "Base",
                15i32.saturating_sub(i32::try_from(base).unwrap_or(0)),
            )
            .int("Type", 0)
            .compounds("Patterns", &layers);
    } else if name == "minecraft:beacon" {
        let effect = |k: &str| {
            nbt.and_then(|n| n.get(k))
                .and_then(Tag::as_str)
                .and_then(crate::entity::effect_id)
                .unwrap_or(0)
        };
        c.string("id", "Beacon")
            .int("primary", effect("primary_effect"))
            .int("secondary", effect("secondary_effect"));
    } else if name == "minecraft:chest" || name == "minecraft:trapped_chest" {
        let ([dx, dz], lead) = chest_pair(state)?;
        c.string("id", "Chest")
            .int("pairx", x.saturating_add(dx))
            .int("pairz", z.saturating_add(dz))
            .byte("pairlead", u8::from(lead));
    } else if let Some(kind) = skull_type(name) {
        // Floor skulls turn in 16 steps; wall skulls face their wall (the block state).
        let rotation = state
            .value("rotation")
            .and_then(|r| r.parse::<u8>().ok())
            .map_or(0.0, |r| f32::from(r) * 22.5);
        c.string("id", "Skull")
            .float("Rotation", rotation)
            .byte("SkullType", kind)
            .byte("MouthMoving", 0)
            .int("MouthTickCount", 0)
            .byte("DoingAnimation", 0);
    } else if is_pot(name) {
        c.string("id", "FlowerPot");
        if let Some(plant) = palette.potted_plant(u32::from(state.0)) {
            c.raw_compound("PlantBlock", plant);
        }
    } else if name.ends_with("shulker_box") {
        let facing = state.value("facing").and_then(facing_id).unwrap_or(1);
        c.string("id", "ShulkerBox").byte("facing", facing);
    } else if name == "minecraft:piston" || name == "minecraft:sticky_piston" {
        // A resting piston: its arm out or in (Bedrock draws the arm from the block actor).
        let out = state.value("extended") == Some("true");
        let (progress, s) = if out { (1.0, 2) } else { (0.0, 0) };
        c.string("id", "PistonArm")
            .float("Progress", progress)
            .float("LastProgress", progress)
            .byte("State", s)
            .byte("NewState", s)
            .byte("Sticky", u8::from(name == "minecraft:sticky_piston"));
    } else if name == "minecraft:lectern" {
        // Java sends the book only to the reader; any book shows on the stand.
        let has_book = state.value("has_book") == Some("true");
        c.string("id", "Lectern")
            .byte("hasBook", u8::from(has_book));
        if has_book {
            let mut book = Compound::default();
            book.string("Name", "minecraft:written_book")
                .byte("Count", 1)
                .short("Damage", 0);
            c.compound("book", &book)
                .int("page", 0)
                .int("totalPages", 1);
        }
    } else if name == "minecraft:decorated_pot" {
        let sherds: Vec<String> = match nbt.and_then(|n| n.get("sherds")) {
            Some(Tag::List(list)) => list
                .0
                .iter()
                .take(4)
                .map(|t| t.as_str().unwrap_or("minecraft:brick").to_owned())
                .collect(),
            _ => Vec::new(),
        };
        let mut all: Vec<&str> = sherds.iter().map(String::as_str).collect();
        all.resize(4, "minecraft:brick");
        c.string("id", "DecoratedPot").strings("sherds", &all);
    } else if name.ends_with("campfire") {
        c.string("id", "Campfire");
        if let Some(Tag::List(items)) = nbt.and_then(|n| n.get("Items")) {
            for item in items.0.iter().filter_map(Tag::as_compound).take(4) {
                let slot = item.get("Slot").and_then(Tag::as_i64).unwrap_or(0);
                let java = item
                    .get("id")
                    .and_then(Tag::as_str)
                    .and_then(vanillite_data::item::ItemId::by_name);
                let Some((bedrock, meta)) = java.and_then(|i| palette.items().name(i.0)) else {
                    continue;
                };
                let mut stack = Compound::default();
                stack
                    .string("Name", bedrock)
                    .byte("Count", 1)
                    .short("Damage", i16::try_from(meta).unwrap_or(0));
                let n = slot.clamp(0, 3).saturating_add(1);
                c.compound(&format!("Item{n}"), &stack)
                    .int(&format!("ItemTime{n}"), 0);
            }
        }
    } else if name == "minecraft:spawner" {
        let entity = nbt
            .and_then(|n| n.get("SpawnData"))
            .and_then(Tag::as_compound)
            .and_then(|d| d.get("entity"))
            .and_then(Tag::as_compound)
            .and_then(|e| e.get("id"))
            .and_then(Tag::as_str)
            .and_then(crate::entity::bedrock_name);
        c.string("id", "MobSpawner");
        if let Some(entity) = entity {
            c.string("EntityIdentifier", &entity)
                .float("DisplayEntityScale", 1.0)
                .float("DisplayEntityWidth", 1.0)
                .float("DisplayEntityHeight", 1.0);
        }
    } else {
        return None;
    }
    c.int("x", x).int("y", y).int("z", z).byte("isMovable", 1);
    Some(c.root())
}

/// Whether the Bedrock block actor data of a Java block state needs the block entity's NBT
/// (signs, banner patterns).
pub fn needs_nbt(state: u32) -> bool {
    u16::try_from(state).is_ok_and(|s| {
        let s = BlockState(s);
        s.is_valid() && {
            let name = s.block().name();
            name.ends_with("_sign")
                || name.ends_with("_banner")
                || name.ends_with("campfire")
                || is_player_head(name)
                || matches!(
                    name,
                    "minecraft:beacon" | "minecraft:decorated_pot" | "minecraft:spawner"
                )
        }
    })
}

/// The facing (a 3D direction id) of a Java piston base state.
pub fn piston_facing(state: u32) -> Option<u8> {
    let s = BlockState(u16::try_from(state).ok()?);
    if !s.is_valid()
        || !matches!(
            s.block().name(),
            "minecraft:piston" | "minecraft:sticky_piston"
        )
    {
        return None;
    }
    s.value("facing").and_then(facing_id)
}

/// The unit vector of a 3D direction id (down, up, north, south, west, east).
pub fn direction_step(direction: u8) -> [i32; 3] {
    match direction {
        0 => [0, -1, 0],
        1 => [0, 1, 0],
        2 => [0, 0, -1],
        3 => [0, 0, 1],
        4 => [-1, 0, 0],
        _ => [1, 0, 0],
    }
}

/// Whether a Java block state is an extended piston (Bedrock draws the arm from a block actor
/// that Java has no block entity for).
pub fn extended_piston(state: u32) -> bool {
    u16::try_from(state).is_ok_and(|s| {
        let s = BlockState(s);
        s.is_valid()
            && matches!(
                s.block().name(),
                "minecraft:piston" | "minecraft:sticky_piston"
            )
            && s.value("extended") == Some("true")
    })
}

/// Whether a Java block state has Bedrock block actor data from its state alone (beds,
/// banners, double chests, skulls, flower pots): a block update must send it.
pub fn from_state(state: u32) -> bool {
    let Ok(s) = u16::try_from(state) else {
        return false;
    };
    let s = BlockState(s);
    if !s.is_valid() {
        return false;
    }
    let name = s.block().name();
    name.ends_with("_bed")
        || name.ends_with("_banner")
        || name.ends_with("shulker_box")
        || matches!(
            name,
            "minecraft:piston" | "minecraft:sticky_piston" | "minecraft:lectern"
        )
        || is_pot(name)
        || skull_type(name).is_some()
        || ((name == "minecraft:chest" || name == "minecraft:trapped_chest")
            && chest_pair(s).is_some())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    #[test]
    fn colours_come_from_block_names() {
        assert_eq!(dye_prefix("minecraft:light_blue_bed"), Some(3));
        assert_eq!(dye_prefix("minecraft:blue_bed"), Some(11));
        assert_eq!(dye_prefix("minecraft:light_gray_wall_banner"), Some(8));
    }

    #[test]
    fn skulls_and_pots() {
        assert_eq!(skull_type("minecraft:wither_skeleton_wall_skull"), Some(1));
        assert_eq!(skull_type("minecraft:skeleton_skull"), Some(0));
        assert_eq!(skull_type("minecraft:piglin_head"), Some(6));
        assert_eq!(skull_type("minecraft:piston_head"), None);
        let pot = vanillite_data::block::BlockId::by_name("minecraft:potted_poppy")
            .map(|b| u32::from(b.default_state().0))
            .unwrap_or(0);
        let plant = crate::data::V26_60.potted_plant(pot).unwrap_or_default();
        assert!(plant.windows(5).any(|w| w == b"poppy"));
        let actor = translate([0, 64, 0], pot, None, &crate::data::V26_60).unwrap_or_default();
        assert!(actor.windows(10).any(|w| w == b"PlantBlock"));
    }

    #[test]
    fn shulker_piston_and_lectern_from_state() {
        let state = |name: &str, props: &[(&str, &str)]| {
            let block = vanillite_data::block::BlockId::by_name(name).unwrap();
            let mut s = block.default_state();
            for (k, v) in props {
                s = s.with(k, v).unwrap();
            }
            u32::from(s.0)
        };
        let shulker = state("minecraft:red_shulker_box", &[("facing", "west")]);
        assert!(from_state(shulker));
        let actor = translate([0; 3], shulker, None, &crate::data::V26_60).unwrap();
        let at = actor.windows(6).position(|w| w == b"facing").unwrap();
        assert_eq!(actor[at + 6], 4);
        let piston = state("minecraft:sticky_piston", &[("extended", "true")]);
        let actor = translate([0; 3], piston, None, &crate::data::V26_60).unwrap();
        assert!(actor.windows(9).any(|w| w == b"PistonArm"));
        let lectern = state("minecraft:lectern", &[("has_book", "true")]);
        let actor = translate([0; 3], lectern, None, &crate::data::V26_60).unwrap();
        assert!(actor.windows(12).any(|w| w == b"written_book"));
    }

    #[test]
    fn sign_text_is_plain() {
        let line = Tag::Compound(JCompound::default().with("text", "Hello ").with(
            "extra",
            Tag::List(vanillite_nbt::List(vec![Tag::from("world")])),
        ));
        let mut out = String::new();
        plain(&line, &mut out, 8);
        assert_eq!(out, "Hello world");
    }
}
