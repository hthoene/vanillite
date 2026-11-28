//! Parsers and suggestions of the game-specific argument types (Vanilla's
//! `ScoreHolderArgument`, `ObjectiveCriteriaArgument`, `ResourceOrTagArgument`,
//! `ResourceOrIdArgument`, `SlotsArgument`, `ParticleArgument`, ...). Block and item arguments
//! live in [`crate::block_arg`] and [`crate::item_arg`].

use vanillite_nbt::Tag;
use vanillite_protocol::text::{Component, Style};

use crate::argument::{ArgValue, ArgumentType, ParseCtx};
use crate::error::{Arg, CommandError};
use crate::game_values::{
    AngleArg, DisplaySlot, FunctionRef, GameValue, HeightmapKind, InlineOrId, ParticleArg,
    ResourceOrTag, ScoreHolderArg, ScoreOperation, SlotSourceArg, Swizzle,
};
use crate::reader::StringReader;
use crate::selector::SelectorParser;
use crate::snbt;
use crate::suggestion::{Suggestions, SuggestionsBuilder};
use crate::values::parse_identifier;

/// The sixteen team colours in `TeamColor` id order.
pub const TEAM_COLORS: [&str; 16] = [
    "black",
    "dark_blue",
    "dark_green",
    "dark_aqua",
    "dark_red",
    "dark_purple",
    "gold",
    "gray",
    "dark_gray",
    "blue",
    "green",
    "aqua",
    "red",
    "light_purple",
    "yellow",
    "white",
];

const OPERATIONS: [&str; 9] = ["=", "+=", "-=", "*=", "/=", "%=", "<", ">", "><"];
const MIRRORS: [&str; 3] = ["none", "left_right", "front_back"];
const ROTATIONS: [&str; 4] = ["none", "clockwise_90", "180", "counterclockwise_90"];
const HEIGHTMAPS: [(&str, HeightmapKind); 4] = [
    ("world_surface", HeightmapKind::WorldSurface),
    ("ocean_floor", HeightmapKind::OceanFloor),
    ("motion_blocking", HeightmapKind::MotionBlocking),
    (
        "motion_blocking_no_leaves",
        HeightmapKind::MotionBlockingNoLeaves,
    ),
];
const SWING_ANIMATIONS: [&str; 3] = ["none", "whack", "stab"];

/// `ObjectiveCriteria`'s custom criteria (without the per-colour team ones).
const CUSTOM_CRITERIA: [&str; 11] = [
    "dummy",
    "trigger",
    "deathCount",
    "playerKillCount",
    "totalKillCount",
    "health",
    "food",
    "air",
    "armor",
    "xp",
    "level",
];

/// Stat types and the registries of their values (`StatType.getRegistry`).
const STAT_TYPES: [(&str, &str); 9] = [
    ("minecraft:mined", "minecraft:block"),
    ("minecraft:crafted", "minecraft:item"),
    ("minecraft:used", "minecraft:item"),
    ("minecraft:broken", "minecraft:item"),
    ("minecraft:picked_up", "minecraft:item"),
    ("minecraft:dropped", "minecraft:item"),
    ("minecraft:killed", "minecraft:entity_type"),
    ("minecraft:killed_by", "minecraft:entity_type"),
    ("minecraft:custom", "minecraft:custom_stat"),
];

/// The registry a `ResourceOrIdArgument` type refers to.
fn inline_registry(ty: &ArgumentType) -> Option<&'static str> {
    Some(match ty {
        ArgumentType::LootTable => "minecraft:loot_table",
        ArgumentType::LootPredicate => "minecraft:predicate",
        ArgumentType::LootModifier => "minecraft:item_modifier",
        ArgumentType::ContextFloatProvider => "minecraft:context_float_provider",
        ArgumentType::ContextIntProvider => "minecraft:context_int_provider",
        ArgumentType::Dialog => "minecraft:dialog",
        ArgumentType::Feature => "minecraft:worldgen/feature",
        _ => return None,
    })
}

fn game(v: GameValue) -> ArgValue {
    ArgValue::Game(v)
}

/// `ParserUtils.readWhile(c -> c != ' ')`.
fn read_word<'a>(r: &mut StringReader<'a>) -> &'a str {
    r.read_while(|c| c != ' ')
}

/// Parses a game argument type at the reader's cursor.
pub(crate) fn parse(
    ty: &ArgumentType,
    r: &mut StringReader<'_>,
    ctx: ParseCtx<'_>,
) -> Result<ArgValue, CommandError> {
    let limits = ctx.limits;
    let source = ctx.source;
    Ok(match ty {
        ArgumentType::BlockState => crate::block_arg::parse_state(r, ctx)?,
        ArgumentType::BlockPredicate => crate::block_arg::parse_predicate(r, ctx)?,
        ArgumentType::ItemStack => crate::item_arg::parse_item(r, ctx)?,
        ArgumentType::ItemPredicate => crate::item_arg::parse_predicate(r, ctx)?,
        ArgumentType::ScoreHolder { multiple } => {
            if r.peek_is('@') {
                let selector = SelectorParser::new(r, source.allows_selectors(), limits).parse()?;
                if !multiple && selector.max_results > 1 {
                    return Err(CommandError::new("argument.entity.toomany", vec![]).at(r));
                }
                game(GameValue::ScoreHolder(ScoreHolderArg::Selector(Box::new(
                    selector,
                ))))
            } else {
                let text = read_word(r);
                if text == "*" {
                    game(GameValue::ScoreHolder(ScoreHolderArg::Wildcard))
                } else {
                    game(GameValue::ScoreHolder(ScoreHolderArg::Name(
                        text.to_owned(),
                    )))
                }
            }
        }
        ArgumentType::Objective | ArgumentType::Team => {
            ArgValue::String(r.read_unquoted_string().to_owned())
        }
        ArgumentType::ObjectiveCriteria => {
            let start = r.cursor();
            let id = read_word(r);
            if !criteria_exists(id) {
                r.set_cursor(start);
                return Err(
                    CommandError::new("argument.criteria.invalid", vec![Arg::from(id)]).at(r),
                );
            }
            ArgValue::String(id.to_owned())
        }
        ArgumentType::Operation => {
            if !r.can_read() {
                return Err(CommandError::new("arguments.operation.invalid", vec![]).at(r));
            }
            let op = match read_word(r) {
                "=" => ScoreOperation::Assign,
                "+=" => ScoreOperation::Add,
                "-=" => ScoreOperation::Sub,
                "*=" => ScoreOperation::Mul,
                "/=" => ScoreOperation::Div,
                "%=" => ScoreOperation::Mod,
                "<" => ScoreOperation::Min,
                ">" => ScoreOperation::Max,
                "><" => ScoreOperation::Swap,
                _ => return Err(CommandError::new("arguments.operation.invalid", vec![])),
            };
            game(GameValue::Operation(op))
        }
        ArgumentType::ScoreboardSlot => {
            let name = r.read_unquoted_string();
            match display_slot(name) {
                Some(slot) => game(GameValue::DisplaySlot(slot)),
                None => {
                    return Err(CommandError::new(
                        "argument.scoreboardDisplaySlot.invalid",
                        vec![Arg::from(name)],
                    )
                    .at(r));
                }
            }
        }
        ArgumentType::TeamColor => {
            let id = r.read_unquoted_string();
            match team_color(id) {
                Some(c) => game(GameValue::TeamColor(c)),
                None => {
                    return Err(
                        CommandError::new("argument.color.invalid", vec![Arg::from(id)]).at(r),
                    );
                }
            }
        }
        ArgumentType::HexColor => {
            let text = r.read_unquoted_string();
            match hex_color(text) {
                Some(argb) => game(GameValue::HexColor(argb)),
                None => {
                    return Err(CommandError::new(
                        "argument.hexcolor.invalid",
                        vec![Arg::from(text)],
                    )
                    .at(r));
                }
            }
        }
        ArgumentType::Angle => {
            if !r.can_read() {
                return Err(CommandError::new("argument.angle.incomplete", vec![]).at(r));
            }
            let relative = r.peek_is('~');
            if relative {
                r.skip();
            }
            let angle = if r.can_read() && !r.peek_is(' ') {
                r.read_float()?
            } else {
                0.0
            };
            if !angle.is_finite() {
                return Err(CommandError::new("argument.angle.invalid", vec![]).at(r));
            }
            game(GameValue::Angle(AngleArg { angle, relative }))
        }
        ArgumentType::Swizzle => {
            let mut swizzle = Swizzle::default();
            while r.can_read() && !r.peek_is(' ') {
                let flag = match r.read() {
                    Some('x') => &mut swizzle.x,
                    Some('y') => &mut swizzle.y,
                    Some('z') => &mut swizzle.z,
                    _ => return Err(CommandError::new("arguments.swizzle.invalid", vec![]).at(r)),
                };
                if *flag {
                    return Err(CommandError::new("arguments.swizzle.invalid", vec![]).at(r));
                }
                *flag = true;
            }
            game(GameValue::Swizzle(swizzle))
        }
        ArgumentType::ItemSlot => {
            let (name, slots) = read_slot_range(r)?;
            match slots.as_slice() {
                [slot] => game(GameValue::Slot(*slot)),
                _ => {
                    return Err(CommandError::new(
                        "slot.only_single_allowed",
                        vec![Arg::from(name)],
                    )
                    .at(r));
                }
            }
        }
        ArgumentType::ItemSlots => {
            let (name, slots) = read_slot_range(r)?;
            game(GameValue::Slots(name, slots))
        }
        ArgumentType::SlotSource => {
            let start = r.cursor();
            let name = read_word(r);
            match slot_range(name) {
                Some(slots) => game(GameValue::SlotSource(SlotSourceArg::Range(
                    name.to_owned(),
                    slots,
                ))),
                None => {
                    r.set_cursor(start);
                    let value = parse_inline_or_id(r, ctx, "minecraft:slot_source")?;
                    game(GameValue::SlotSource(SlotSourceArg::Source(value)))
                }
            }
        }
        ArgumentType::Function => {
            let tag = r.peek_is('#');
            if tag {
                r.skip();
            }
            let id = parse_identifier(r)?;
            game(GameValue::Function(FunctionRef { tag, id }))
        }
        ArgumentType::Dimension
        | ArgumentType::ResourceKey { .. }
        | ArgumentType::TemplateMirror
        | ArgumentType::TemplateRotation
        | ArgumentType::SwingAnimation
        | ArgumentType::Heightmap => parse_named(ty, r)?,
        ArgumentType::ResourceOrTag { registry } => game(GameValue::ResourceOrTag(
            parse_resource_or_tag(r, ctx, registry, true)?,
        )),
        ArgumentType::ResourceOrTagKey { registry } => game(GameValue::ResourceOrTag(
            parse_resource_or_tag(r, ctx, registry, false)?,
        )),
        ArgumentType::ResourceSelector { registry } => {
            let raw = r.read_while(|c| allowed_in_identifier(c) || c == '*' || c == '?');
            let pattern = if raw.contains(':') {
                raw.to_owned()
            } else {
                format!("minecraft:{raw}")
            };
            let any = source
                .registry_entries(registry)
                .iter()
                .any(|e| wildcard_match(e, &pattern));
            if !any {
                return Err(CommandError::new(
                    "argument.resource_selector.not_found",
                    vec![Arg::from(pattern), Arg::from(*registry)],
                )
                .at(r));
            }
            ArgValue::String(pattern)
        }
        ArgumentType::LootTable
        | ArgumentType::LootPredicate
        | ArgumentType::LootModifier
        | ArgumentType::ContextFloatProvider
        | ArgumentType::ContextIntProvider
        | ArgumentType::Dialog
        | ArgumentType::Feature => {
            let registry = inline_registry(ty).unwrap_or("minecraft:empty");
            game(GameValue::InlineOrId(parse_inline_or_id(r, ctx, registry)?))
        }
        ArgumentType::Particle => {
            let id = parse_identifier(r)?;
            if !source.resource_exists("minecraft:particle_type", &id) {
                return Err(CommandError::new("particle.notFound", vec![Arg::from(id)]).at(r));
            }
            let options = if r.peek_is('{') {
                match snbt::parse_tag(r, limits)? {
                    Tag::Compound(c) => Some(c),
                    other => {
                        return Err(CommandError::new(
                            "particle.invalidOptions",
                            vec![Arg::from(format!(
                                "Not a map: {}",
                                vanillite_nbt::snbt::to_string(&other)
                            ))],
                        ));
                    }
                }
            } else {
                None
            };
            game(GameValue::Particle(Box::new(ParticleArg { id, options })))
        }
        ArgumentType::Component => {
            let start = r.cursor();
            let tag = snbt::parse_tag(r, limits)?;
            match Component::try_from_nbt(&tag) {
                Ok(c) => game(GameValue::Component(Box::new(c))),
                Err(e) => {
                    r.set_cursor(start);
                    return Err(CommandError::new(
                        "argument.component.invalid",
                        vec![Arg::from(e.0.as_ref())],
                    )
                    .at(r));
                }
            }
        }
        ArgumentType::Style => {
            let start = r.cursor();
            let tag = snbt::parse_tag(r, limits)?;
            match Style::try_from_nbt(&tag) {
                Ok(s) => game(GameValue::Style(Box::new(s))),
                Err(e) => {
                    r.set_cursor(start);
                    return Err(CommandError::new(
                        "argument.style.invalid",
                        vec![Arg::from(e.0.as_ref())],
                    )
                    .at(r));
                }
            }
        }
        _ => return Err(CommandError::new("command.unknown.argument", vec![]).at(r)),
    })
}

/// `StringRepresentableArgument`s and plain identifiers (`dimension`, `resource_key`).
fn parse_named(ty: &ArgumentType, r: &mut StringReader<'_>) -> Result<ArgValue, CommandError> {
    let invalid = |r: &StringReader<'_>, key: &'static str, id: &str| {
        Err(CommandError::new(key, vec![Arg::from(id)]).at(r))
    };
    match ty {
        ArgumentType::Dimension | ArgumentType::ResourceKey { .. } => {
            Ok(ArgValue::Identifier(parse_identifier(r)?))
        }
        ArgumentType::TemplateMirror => {
            let id = r.read_unquoted_string();
            if MIRRORS.contains(&id) {
                Ok(ArgValue::String(id.to_owned()))
            } else {
                invalid(r, "argument.enum.invalid", id)
            }
        }
        ArgumentType::TemplateRotation => {
            let id = r.read_unquoted_string();
            if ROTATIONS.contains(&id) {
                Ok(ArgValue::String(id.to_owned()))
            } else {
                invalid(r, "argument.enum.invalid", id)
            }
        }
        ArgumentType::Heightmap => {
            let id = r.read_unquoted_string();
            match HEIGHTMAPS.iter().find(|(n, _)| *n == id) {
                Some((_, kind)) => Ok(game(GameValue::Heightmap(*kind))),
                None => invalid(r, "argument.enum.invalid", id),
            }
        }
        _ => {
            let id = r.read_unquoted_string();
            if SWING_ANIMATIONS.contains(&id) {
                Ok(ArgValue::String(id.to_owned()))
            } else {
                invalid(r, "argument.swing_animation.invalid", id)
            }
        }
    }
}

/// `ResourceOrTagArgument` (`check`: the entry or tag must exist) and `ResourceOrTagKeyArgument`.
fn parse_resource_or_tag(
    r: &mut StringReader<'_>,
    ctx: ParseCtx<'_>,
    registry: &str,
    check: bool,
) -> Result<ResourceOrTag, CommandError> {
    if r.peek_is('#') {
        let start = r.cursor();
        r.skip();
        let id = match parse_identifier(r) {
            Ok(id) => id,
            Err(e) => {
                r.set_cursor(start);
                return Err(e);
            }
        };
        if check && !ctx.source.tag_exists(registry, &id) {
            let e = CommandError::new(
                "argument.resource_tag.not_found",
                vec![Arg::from(id.as_str()), Arg::from(registry)],
            )
            .at(r);
            r.set_cursor(start);
            return Err(e);
        }
        Ok(ResourceOrTag::Tag(id))
    } else {
        let id = parse_identifier(r)?;
        if check && !ctx.source.resource_exists(registry, &id) {
            return Err(CommandError::new(
                "argument.resource.not_found",
                vec![Arg::from(id), Arg::from(registry)],
            )
            .at(r));
        }
        Ok(ResourceOrTag::Resource(id))
    }
}

/// `ResourceOrIdArgument`: an identifier that must exist in the registry, or an inline SNBT
/// value.
fn parse_inline_or_id(
    r: &mut StringReader<'_>,
    ctx: ParseCtx<'_>,
    registry: &str,
) -> Result<InlineOrId, CommandError> {
    r.skip_whitespace();
    let start = r.cursor();
    let raw = r.read_while(allowed_in_identifier);
    if !raw.is_empty()
        && let Some(id) = crate::values::normalize_identifier(raw)
    {
        if !ctx.source.resource_exists(registry, &id) {
            return Err(CommandError::new(
                "argument.resource_or_id.no_such_element",
                vec![Arg::from(id), Arg::from(registry)],
            )
            .at(r));
        }
        return Ok(InlineOrId::Id(id));
    }
    r.set_cursor(start);
    Ok(InlineOrId::Inline(snbt::parse_tag(r, ctx.limits)?))
}

/// `Identifier.isAllowedInIdentifier`.
pub(crate) fn allowed_in_identifier(c: char) -> bool {
    c.is_ascii_digit() || c.is_ascii_lowercase() || matches!(c, '_' | ':' | '/' | '.' | '-')
}

/// `FilenameUtils.wildcardMatch` (case-sensitive): `*` any run, `?` one character.
fn wildcard_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi = pi.saturating_add(1);
            }
            Some(&c) if c == '?' || t.get(ti) == Some(&c) => {
                pi = pi.saturating_add(1);
                ti = ti.saturating_add(1);
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp.saturating_add(1);
                    ti = st.saturating_add(1);
                    star = Some((sp, ti));
                }
                None => return false,
            },
        }
    }
    p.get(pi..)
        .is_some_and(|rest| rest.iter().all(|c| *c == '*'))
}

/// `ObjectiveCriteria.byName`.
pub fn criteria_exists(name: &str) -> bool {
    if CUSTOM_CRITERIA.contains(&name) {
        return true;
    }
    if let Some(color) = name
        .strip_prefix("teamkill.")
        .or_else(|| name.strip_prefix("killedByTeam."))
    {
        return TEAM_COLORS.contains(&color);
    }
    let Some((kind, value)) = name.split_once(':') else {
        return false;
    };
    let (Some(kind), Some(value)) = (by_separator(kind), by_separator(value)) else {
        return false;
    };
    STAT_TYPES
        .iter()
        .find(|(k, _)| *k == kind)
        .and_then(|(_, registry)| vanillite_data::registry::Registry::by_name(registry))
        .is_some_and(|r| r.id(&value).is_some())
}

/// `Identifier.bySeparator(s, '.')`.
fn by_separator(s: &str) -> Option<String> {
    let raw = match s.split_once('.') {
        Some((ns, path)) => format!("{ns}:{path}"),
        None => format!("minecraft:{s}"),
    };
    crate::values::normalize_identifier(&raw)
}

/// `Stat.buildName` of every stat and the custom criteria, for suggestions.
fn all_criteria() -> Vec<String> {
    let mut out: Vec<String> = CUSTOM_CRITERIA.iter().map(|s| (*s).to_owned()).collect();
    for color in TEAM_COLORS {
        out.push(format!("teamkill.{color}"));
        out.push(format!("killedByTeam.{color}"));
    }
    for (kind, registry) in STAT_TYPES {
        let Some(reg) = vanillite_data::registry::Registry::by_name(registry) else {
            continue;
        };
        let kind = kind.replace(':', ".");
        for entry in reg.entries {
            out.push(format!("{kind}:{}", entry.replace(':', ".")));
        }
    }
    out
}

/// `DisplaySlot` by name.
fn display_slot(name: &str) -> Option<DisplaySlot> {
    Some(match name {
        "list" => DisplaySlot::List,
        "sidebar" => DisplaySlot::Sidebar,
        "below_name" => DisplaySlot::BelowName,
        _ => DisplaySlot::SidebarTeam(team_color(name.strip_prefix("sidebar.team.")?)?),
    })
}

fn display_slot_names() -> Vec<String> {
    let mut out = vec![
        "list".to_owned(),
        "sidebar".to_owned(),
        "below_name".to_owned(),
    ];
    out.extend(TEAM_COLORS.iter().map(|c| format!("sidebar.team.{c}")));
    out
}

/// A `TeamColor` id by name.
fn team_color(name: &str) -> Option<u8> {
    TEAM_COLORS
        .iter()
        .position(|c| *c == name)
        .and_then(|i| u8::try_from(i).ok())
}

/// `HexColorArgument`: `rgb` or `rrggbb` as ARGB with full alpha.
fn hex_color(text: &str) -> Option<i32> {
    if !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let rgb = match text.len() {
        3 => {
            let v = u32::from_str_radix(text, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xF, (v >> 4) & 0xF, v & 0xF);
            (r.wrapping_mul(17) << 16) | (g.wrapping_mul(17) << 8) | b.wrapping_mul(17)
        }
        6 => u32::from_str_radix(text, 16).ok()?,
        _ => return None,
    };
    Some(i32::from_ne_bytes((0xFF00_0000 | rgb).to_ne_bytes()))
}

/// `SlotRanges`: the slot ranges in Vanilla's order with their slot indices.
pub fn slot_ranges() -> Vec<(String, Vec<i32>)> {
    let mut out: Vec<(String, Vec<i32>)> = Vec::new();
    let range = |out: &mut Vec<(String, Vec<i32>)>, prefix: &str, offset: i32, size: i32| {
        let mut all = Vec::new();
        for i in 0..size {
            let slot = offset.saturating_add(i);
            out.push((format!("{prefix}{i}"), vec![slot]));
            all.push(slot);
        }
        out.push((format!("{prefix}*"), all));
    };
    out.push(("contents".to_owned(), vec![0]));
    range(&mut out, "container.", 0, 54);
    range(&mut out, "hotbar.", 0, 9);
    range(&mut out, "inventory.", 9, 27);
    range(&mut out, "enderchest.", 200, 27);
    range(&mut out, "mob.inventory.", 300, 8);
    range(&mut out, "horse.", 500, 15);
    let single = |out: &mut Vec<(String, Vec<i32>)>, name: &str, slots: &[i32]| {
        out.push((name.to_owned(), slots.to_vec()));
    };
    single(&mut out, "weapon", &[98]);
    single(&mut out, "weapon.mainhand", &[98]);
    single(&mut out, "weapon.offhand", &[99]);
    single(&mut out, "weapon.*", &[98, 99]);
    single(&mut out, "armor.head", &[103]);
    single(&mut out, "armor.chest", &[102]);
    single(&mut out, "armor.legs", &[101]);
    single(&mut out, "armor.feet", &[100]);
    single(&mut out, "armor.body", &[105]);
    single(&mut out, "armor.*", &[103, 102, 101, 100, 105]);
    single(&mut out, "saddle", &[106]);
    single(&mut out, "horse.chest", &[499]);
    single(&mut out, "player.cursor", &[499]);
    range(&mut out, "player.crafting.", 500, 4);
    out
}

/// `SlotRanges.nameToIds`.
pub fn slot_range(name: &str) -> Option<Vec<i32>> {
    slot_ranges()
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, s)| s)
}

/// `SlotRanges.read`.
fn read_slot_range(r: &mut StringReader<'_>) -> Result<(String, Vec<i32>), CommandError> {
    let name = read_word(r);
    match slot_range(name) {
        Some(slots) => Ok((name.to_owned(), slots)),
        None => Err(CommandError::new("slot.unknown", vec![Arg::from(name)]).at(r)),
    }
}

/// `SharedSuggestionProvider.suggestRegistryElements`: tags with `#` and/or the entries.
fn suggest_registry(
    b: &mut SuggestionsBuilder<'_>,
    ctx: ParseCtx<'_>,
    registry: &str,
    tags: bool,
    elements: bool,
) {
    if tags {
        let tags = ctx.source.registry_tags(registry);
        let ids: Vec<String> = tags.iter().map(|t| full_id(t)).collect();
        b.suggest_resources(ids.iter().map(String::as_str), "#");
    }
    if elements {
        let entries = ctx.source.registry_entries(registry);
        b.suggest_resource_ids(entries.iter().map(String::as_str));
    }
}

/// An identifier with its namespace.
pub(crate) fn full_id(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// Suggestions of the game argument types (`listSuggestions`).
pub(crate) fn suggestions(
    ty: &ArgumentType,
    builder: SuggestionsBuilder<'_>,
    ctx: ParseCtx<'_>,
) -> Suggestions {
    let mut b = builder;
    match ty {
        ArgumentType::BlockState => return crate::block_arg::suggestions(&b, ctx, false),
        ArgumentType::BlockPredicate => return crate::block_arg::suggestions(&b, ctx, true),
        ArgumentType::ItemStack => return crate::item_arg::item_suggestions(&b, ctx),
        ArgumentType::ItemPredicate => return crate::item_arg::predicate_suggestions(&b, ctx),
        ArgumentType::ScoreHolder { .. } => {
            return crate::argument::selector_suggestions(&b, ctx, false);
        }
        ArgumentType::Objective => {
            let names = ctx.source.objective_names();
            b.suggest_matching(names.iter().map(String::as_str));
        }
        ArgumentType::Team => {
            let names = ctx.source.team_names();
            b.suggest_matching(names.iter().map(String::as_str));
        }
        ArgumentType::ObjectiveCriteria => {
            let all = all_criteria();
            b.suggest_matching(all.iter().map(String::as_str));
        }
        ArgumentType::Operation => {
            b.suggest_matching(OPERATIONS);
        }
        ArgumentType::ScoreboardSlot => {
            let names = display_slot_names();
            b.suggest_matching(names.iter().map(String::as_str));
        }
        ArgumentType::TeamColor => {
            b.suggest_matching(TEAM_COLORS);
        }
        ArgumentType::ItemSlot => {
            let names: Vec<String> = slot_ranges()
                .into_iter()
                .filter(|(_, s)| s.len() == 1)
                .map(|(n, _)| n)
                .collect();
            b.suggest_matching(names.iter().map(String::as_str));
        }
        ArgumentType::ItemSlots => {
            let names: Vec<String> = slot_ranges().into_iter().map(|(n, _)| n).collect();
            b.suggest_matching(names.iter().map(String::as_str));
        }
        ArgumentType::SlotSource => {
            let names: Vec<String> = slot_ranges().into_iter().map(|(n, _)| n).collect();
            b.suggest_matching(names.iter().map(String::as_str));
            suggest_registry(&mut b, ctx, "minecraft:slot_source", false, true);
        }
        ArgumentType::Dimension => {
            let names = ctx.source.dimension_names();
            b.suggest_resource_ids(names.iter().map(String::as_str));
        }
        ArgumentType::ResourceOrTag { registry } | ArgumentType::ResourceOrTagKey { registry } => {
            suggest_registry(&mut b, ctx, registry, true, true);
        }
        ArgumentType::ResourceKey { registry } | ArgumentType::ResourceSelector { registry } => {
            suggest_registry(&mut b, ctx, registry, false, true);
        }
        ArgumentType::TemplateMirror => {
            b.suggest_matching(MIRRORS);
        }
        ArgumentType::TemplateRotation => {
            b.suggest_matching(ROTATIONS);
        }
        ArgumentType::Heightmap => {
            b.suggest_matching(HEIGHTMAPS.iter().map(|(n, _)| *n));
        }
        ArgumentType::SwingAnimation => {
            b.suggest_matching(SWING_ANIMATIONS);
        }
        ArgumentType::Particle => {
            suggest_registry(&mut b, ctx, "minecraft:particle_type", false, true);
        }
        other => {
            if let Some(registry) = inline_registry(other) {
                suggest_registry(&mut b, ctx, registry, false, true);
            }
        }
    }
    b.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    use crate::source::Source;

    struct S;

    impl Source for S {
        fn permission_level(&self) -> u8 {
            4
        }

        fn online_player_names(&self) -> Vec<String> {
            vec!["Alice".into()]
        }

        fn registry_tags(&self, registry: &str) -> Vec<String> {
            match registry {
                "minecraft:worldgen/biome" => vec!["minecraft:is_forest".into()],
                _ => Vec::new(),
            }
        }

        fn registry_entries(&self, registry: &str) -> Vec<String> {
            match registry {
                "minecraft:test_instance" => {
                    vec!["minecraft:a/one".into(), "minecraft:a/two".into()]
                }
                "minecraft:worldgen/biome" => vec!["minecraft:plains".into()],
                _ => vanillite_data::registry::Registry::by_name(registry)
                    .map(|r| r.entries.iter().map(|e| (*e).to_owned()).collect())
                    .unwrap_or_default(),
            }
        }

        fn resource_exists(&self, registry: &str, id: &str) -> bool {
            match registry {
                "minecraft:loot_table" => id == "minecraft:chests/a",
                "minecraft:worldgen/biome" => id == "minecraft:plains",
                _ => vanillite_data::registry::Registry::by_name(registry)
                    .is_none_or(|r| r.id(id).is_some()),
            }
        }
    }

    fn run(ty: ArgumentType, input: &str) -> (Result<ArgValue, CommandError>, usize) {
        let limits = Limits::default();
        let ctx = ParseCtx {
            source: &S,
            limits: &limits,
        };
        let mut r = StringReader::new(input);
        let v = ty.parse(&mut r, ctx);
        (v, r.cursor())
    }

    fn ok(ty: ArgumentType, input: &str) -> (ArgValue, usize) {
        let (v, c) = run(ty, input);
        (v.unwrap_or_else(|e| panic!("{input}: {e:?}")), c)
    }

    fn err(ty: ArgumentType, input: &str) -> (&'static str, Option<usize>) {
        let (v, _) = run(ty, input);
        let e = v.err().unwrap_or_else(|| panic!("{input} parsed"));
        (e.key, e.cursor)
    }

    fn g(v: ArgValue) -> GameValue {
        match v {
            ArgValue::Game(g) => g,
            other => panic!("{other:?}"),
        }
    }

    fn suggest(ty: ArgumentType, input: &str) -> Vec<String> {
        let limits = Limits::default();
        let ctx = ParseCtx {
            source: &S,
            limits: &limits,
        };
        ty.suggestions(
            SuggestionsBuilder::new(input, 0),
            ctx,
            &crate::Args::default(),
        )
        .texts()
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn score_holders() {
        let (v, c) = ok(ArgumentType::ScoreHolder { multiple: false }, "#fake rest");
        assert_eq!(
            g(v),
            GameValue::ScoreHolder(ScoreHolderArg::Name("#fake".into()))
        );
        assert_eq!(c, 5);
        let (v, _) = ok(ArgumentType::ScoreHolder { multiple: true }, "*");
        assert_eq!(g(v), GameValue::ScoreHolder(ScoreHolderArg::Wildcard));
        let (v, _) = ok(ArgumentType::ScoreHolder { multiple: true }, "@a");
        assert!(matches!(
            g(v),
            GameValue::ScoreHolder(ScoreHolderArg::Selector(_))
        ));
        assert_eq!(
            err(ArgumentType::ScoreHolder { multiple: false }, "@e"),
            ("argument.entity.toomany", Some(2))
        );
        assert!(
            suggest(ArgumentType::ScoreHolder { multiple: false }, "").contains(&"Alice".into())
        );
    }

    #[test]
    fn criteria() {
        let (v, c) = ok(ArgumentType::ObjectiveCriteria, "dummy x");
        assert_eq!((v, c), (ArgValue::String("dummy".into()), 5));
        ok(
            ArgumentType::ObjectiveCriteria,
            "minecraft.mined:minecraft.stone",
        );
        ok(ArgumentType::ObjectiveCriteria, "mined:stone");
        ok(
            ArgumentType::ObjectiveCriteria,
            "minecraft.custom:minecraft.jump",
        );
        ok(ArgumentType::ObjectiveCriteria, "teamkill.red");
        assert_eq!(
            err(
                ArgumentType::ObjectiveCriteria,
                "minecraft.mined:minecraft.nope"
            ),
            ("argument.criteria.invalid", Some(0))
        );
        assert_eq!(
            err(ArgumentType::ObjectiveCriteria, "teamkill.reset"),
            ("argument.criteria.invalid", Some(0))
        );
        let s = suggest(
            ArgumentType::ObjectiveCriteria,
            "minecraft.custom:minecraft.ju",
        );
        assert!(s.contains(&"minecraft.custom:minecraft.jump".into()));
    }

    #[test]
    fn operations() {
        let (v, c) = ok(ArgumentType::Operation, ">< x");
        assert_eq!((g(v), c), (GameValue::Operation(ScoreOperation::Swap), 2));
        assert_eq!(
            err(ArgumentType::Operation, "=="),
            ("arguments.operation.invalid", None)
        );
        assert_eq!(
            err(ArgumentType::Operation, ""),
            ("arguments.operation.invalid", Some(0))
        );
        assert_eq!(suggest(ArgumentType::Operation, ">"), ["><"]);
    }

    #[test]
    fn slots_and_colors() {
        let (v, _) = ok(ArgumentType::ScoreboardSlot, "sidebar.team.red");
        assert_eq!(g(v), GameValue::DisplaySlot(DisplaySlot::SidebarTeam(12)));
        assert_eq!(DisplaySlot::SidebarTeam(12).id(), 15);
        assert_eq!(
            err(ArgumentType::ScoreboardSlot, "belowName"),
            ("argument.scoreboardDisplaySlot.invalid", Some(9))
        );
        let (v, _) = ok(ArgumentType::TeamColor, "light_purple");
        assert_eq!(g(v), GameValue::TeamColor(13));
        assert_eq!(
            err(ArgumentType::TeamColor, "reset"),
            ("argument.color.invalid", Some(5))
        );
        let (v, _) = ok(ArgumentType::HexColor, "f0a");
        assert_eq!(
            g(v),
            GameValue::HexColor(i32::from_ne_bytes(0xFFFF_00AA_u32.to_ne_bytes()))
        );
        let (v, _) = ok(ArgumentType::HexColor, "123456");
        assert_eq!(
            g(v),
            GameValue::HexColor(i32::from_ne_bytes(0xFF12_3456_u32.to_ne_bytes()))
        );
        assert_eq!(
            err(ArgumentType::HexColor, "12345"),
            ("argument.hexcolor.invalid", Some(5))
        );
    }

    #[test]
    fn angles_and_swizzles() {
        let (v, c) = ok(ArgumentType::Angle, "~10.5 x");
        assert_eq!(
            (g(v), c),
            (
                GameValue::Angle(AngleArg {
                    angle: 10.5,
                    relative: true
                }),
                5
            )
        );
        let (v, _) = ok(ArgumentType::Angle, "~");
        assert_eq!(
            g(v),
            GameValue::Angle(AngleArg {
                angle: 0.0,
                relative: true
            })
        );
        assert_eq!(
            err(ArgumentType::Angle, ""),
            ("argument.angle.incomplete", Some(0))
        );
        let (v, c) = ok(ArgumentType::Swizzle, "zx y");
        assert_eq!(
            (g(v), c),
            (
                GameValue::Swizzle(Swizzle {
                    x: true,
                    y: false,
                    z: true
                }),
                2
            )
        );
        assert_eq!(
            err(ArgumentType::Swizzle, "xx"),
            ("arguments.swizzle.invalid", Some(2))
        );
        assert_eq!(
            err(ArgumentType::Swizzle, "xw"),
            ("arguments.swizzle.invalid", Some(2))
        );
    }

    #[test]
    fn item_slots() {
        let (v, _) = ok(ArgumentType::ItemSlot, "armor.head");
        assert_eq!(g(v), GameValue::Slot(103));
        let (v, _) = ok(ArgumentType::ItemSlot, "hotbar.8");
        assert_eq!(g(v), GameValue::Slot(8));
        assert_eq!(
            err(ArgumentType::ItemSlot, "armor.*"),
            ("slot.only_single_allowed", Some(7))
        );
        assert_eq!(
            err(ArgumentType::ItemSlots, "nope"),
            ("slot.unknown", Some(4))
        );
        let (v, _) = ok(ArgumentType::ItemSlots, "inventory.*");
        let GameValue::Slots(_, slots) = g(v) else {
            panic!()
        };
        assert_eq!((slots.first(), slots.len()), (Some(&9), 27));
        let (v, _) = ok(ArgumentType::SlotSource, "weapon.*");
        assert!(matches!(
            g(v),
            GameValue::SlotSource(SlotSourceArg::Range(_, _))
        ));
        let (v, _) = ok(ArgumentType::SlotSource, "{type:'x'}");
        assert!(matches!(
            g(v),
            GameValue::SlotSource(SlotSourceArg::Source(InlineOrId::Inline(_)))
        ));
        assert!(suggest(ArgumentType::ItemSlot, "armor.").contains(&"armor.feet".into()));
        assert!(!suggest(ArgumentType::ItemSlot, "armor.").contains(&"armor.*".into()));
    }

    #[test]
    fn functions_and_resources() {
        let (v, _) = ok(ArgumentType::Function, "#foo:bar");
        assert_eq!(
            g(v),
            GameValue::Function(FunctionRef {
                tag: true,
                id: "foo:bar".into()
            })
        );
        let (v, _) = ok(ArgumentType::Function, "tick");
        assert_eq!(
            g(v),
            GameValue::Function(FunctionRef {
                tag: false,
                id: "minecraft:tick".into()
            })
        );
        let biome = ArgumentType::ResourceOrTag {
            registry: "minecraft:worldgen/biome",
        };
        let (v, _) = ok(biome.clone(), "#is_forest");
        assert_eq!(
            g(v),
            GameValue::ResourceOrTag(ResourceOrTag::Tag("minecraft:is_forest".into()))
        );
        assert_eq!(
            err(biome.clone(), "#nope"),
            ("argument.resource_tag.not_found", Some(5))
        );
        assert_eq!(
            err(biome.clone(), "desert"),
            ("argument.resource.not_found", Some(6))
        );
        let s = suggest(biome, "");
        assert_eq!(s, ["#minecraft:is_forest", "minecraft:plains"]);
        let key = ArgumentType::ResourceOrTagKey {
            registry: "minecraft:worldgen/biome",
        };
        ok(key, "#anything");
        let sel = ArgumentType::ResourceSelector {
            registry: "minecraft:test_instance",
        };
        let (v, _) = ok(sel.clone(), "a/*");
        assert_eq!(v, ArgValue::String("minecraft:a/*".into()));
        ok(sel.clone(), "*:a/t?o");
        assert_eq!(
            err(sel, "b/*"),
            ("argument.resource_selector.not_found", Some(3))
        );
        let (v, _) = ok(ArgumentType::Dimension, "the_nether");
        assert_eq!(v, ArgValue::Identifier("minecraft:the_nether".into()));
    }

    #[test]
    fn named_enums() {
        let (v, _) = ok(ArgumentType::TemplateRotation, "180");
        assert_eq!(v, ArgValue::String("180".into()));
        assert_eq!(
            err(ArgumentType::TemplateMirror, "up"),
            ("argument.enum.invalid", Some(2))
        );
        let (v, _) = ok(ArgumentType::Heightmap, "motion_blocking_no_leaves");
        assert_eq!(
            g(v),
            GameValue::Heightmap(HeightmapKind::MotionBlockingNoLeaves)
        );
        assert_eq!(
            err(ArgumentType::Heightmap, "world_surface_wg"),
            ("argument.enum.invalid", Some(16))
        );
        assert_eq!(
            err(ArgumentType::SwingAnimation, "spin"),
            ("argument.swing_animation.invalid", Some(4))
        );
    }

    #[test]
    fn inline_or_ids() {
        let (v, c) = ok(ArgumentType::LootTable, "chests/a rest");
        assert_eq!(
            (g(v), c),
            (
                GameValue::InlineOrId(InlineOrId::Id("minecraft:chests/a".into())),
                8
            )
        );
        assert_eq!(
            err(ArgumentType::LootTable, "chests/b"),
            ("argument.resource_or_id.no_such_element", Some(8))
        );
        let (v, _) = ok(
            ArgumentType::LootPredicate,
            "{condition:'minecraft:survives_explosion'}",
        );
        assert!(matches!(g(v), GameValue::InlineOrId(InlineOrId::Inline(_))));
    }

    #[test]
    fn particles() {
        let (v, c) = ok(ArgumentType::Particle, "flame rest");
        assert_eq!(c, 5);
        assert_eq!(
            g(v),
            GameValue::Particle(Box::new(ParticleArg {
                id: "minecraft:flame".into(),
                options: None
            }))
        );
        let (v, _) = ok(ArgumentType::Particle, "dust{color:[1f,0f,0f],scale:1f}");
        let GameValue::Particle(p) = g(v) else {
            panic!()
        };
        assert!(p.options.is_some());
        assert_eq!(
            err(ArgumentType::Particle, "nope"),
            ("particle.notFound", Some(4))
        );
        assert!(suggest(ArgumentType::Particle, "fla").contains(&"minecraft:flame".into()));
    }

    #[test]
    fn components_and_styles() {
        let (v, c) = ok(ArgumentType::Component, "{text:'hi',color:'red'} x");
        assert_eq!(c, 23);
        assert!(matches!(g(v), GameValue::Component(_)));
        let (v, _) = ok(ArgumentType::Component, "'plain'");
        assert_eq!(
            g(v),
            GameValue::Component(Box::new(Component::text("plain")))
        );
        assert_eq!(
            err(ArgumentType::Component, "{color:'red'}"),
            ("argument.component.invalid", Some(0))
        );
        let (v, _) = ok(ArgumentType::Style, "{bold:1b}");
        let GameValue::Style(s) = g(v) else { panic!() };
        assert_eq!(s.bold, Some(true));
        assert_eq!(
            err(ArgumentType::Style, "'x'"),
            ("argument.style.invalid", Some(0))
        );
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("minecraft:a/one", "minecraft:a/*"));
        assert!(wildcard_match("minecraft:a/one", "*o?e"));
        assert!(!wildcard_match("minecraft:a/one", "minecraft:b*"));
        assert!(wildcard_match("", "*"));
    }
}
