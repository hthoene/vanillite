//! Argument types: how each parses, what it suggests and how it goes on the wire
//! (Brigadier's and Vanilla's `ArgumentType`s with `ArgumentTypeInfos`).

use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::commands::{Parser, ParserProps, StringKind};
use vanillite_protocol::types::Identifier;

use crate::bounds::{self, DoubleRange, IntRange};
use crate::coords::{self, Coordinates};
use crate::error::{Arg, CommandError};
use crate::game_values::GameValue;
use crate::limits::Limits;
use crate::nbt_path::{self, NbtPath};
use crate::parse::Args;
use crate::reader::StringReader;
use crate::selector::{EntitySelector, SelectorParser};
use crate::snbt;
use crate::source::Source;
use crate::suggestion::{Suggestions, SuggestionsBuilder};
use crate::values::{Anchor, GameMode, parse_identifier, parse_uuid};

/// The type of an argument node.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgumentType {
    /// `brigadier:bool`.
    Bool,
    /// `brigadier:float` with optional bounds.
    Float { min: Option<f32>, max: Option<f32> },
    /// `brigadier:double` with optional bounds.
    Double { min: Option<f64>, max: Option<f64> },
    /// `brigadier:integer` with optional bounds.
    Integer { min: Option<i32>, max: Option<i32> },
    /// `brigadier:long` with optional bounds.
    Long { min: Option<i64>, max: Option<i64> },
    /// `brigadier:string`.
    String(StringKind),
    /// `minecraft:entity`.
    Entity { single: bool, players_only: bool },
    /// `minecraft:game_profile`.
    GameProfile,
    /// `minecraft:block_pos`.
    BlockPos,
    /// `minecraft:column_pos`.
    ColumnPos,
    /// `minecraft:vec3`; `center` adds 0.5 to whole x and z.
    Vec3 { center: bool },
    /// `minecraft:vec2`.
    Vec2 { center: bool },
    /// `minecraft:rotation`.
    Rotation,
    /// `minecraft:message` (signable).
    Message,
    /// `minecraft:nbt_compound_tag`.
    NbtCompound,
    /// `minecraft:nbt_tag`.
    NbtTag,
    /// `minecraft:nbt_path`.
    NbtPath,
    /// `minecraft:gamemode`.
    GameMode,
    /// `minecraft:time` with the minimum in ticks.
    Time { min: i32 },
    /// `minecraft:uuid`.
    Uuid,
    /// `minecraft:resource_location`.
    Identifier,
    /// `minecraft:resource` of a registry; the entry must exist.
    Resource { registry: &'static str },
    /// `minecraft:entity_anchor`.
    EntityAnchor,
    /// `minecraft:int_range`.
    IntRange,
    /// `minecraft:float_range`.
    FloatRange,
    /// `minecraft:block_state`.
    BlockState,
    /// `minecraft:block_predicate`.
    BlockPredicate,
    /// `minecraft:item_stack`.
    ItemStack,
    /// `minecraft:item_predicate`.
    ItemPredicate,
    /// `minecraft:team_color`.
    TeamColor,
    /// `minecraft:hex_color`.
    HexColor,
    /// `minecraft:component`.
    Component,
    /// `minecraft:style`.
    Style,
    /// `minecraft:objective`.
    Objective,
    /// `minecraft:objective_criteria`.
    ObjectiveCriteria,
    /// `minecraft:operation`.
    Operation,
    /// `minecraft:particle`.
    Particle,
    /// `minecraft:angle`.
    Angle,
    /// `minecraft:scoreboard_slot`.
    ScoreboardSlot,
    /// `minecraft:score_holder`.
    ScoreHolder { multiple: bool },
    /// `minecraft:swizzle`.
    Swizzle,
    /// `minecraft:team`.
    Team,
    /// `minecraft:item_slot`.
    ItemSlot,
    /// `minecraft:item_slots`.
    ItemSlots,
    /// `minecraft:function`.
    Function,
    /// `minecraft:dimension`.
    Dimension,
    /// `minecraft:resource_or_tag` of a registry.
    ResourceOrTag { registry: &'static str },
    /// `minecraft:resource_or_tag_key` of a registry.
    ResourceOrTagKey { registry: &'static str },
    /// `minecraft:resource_key` of a registry.
    ResourceKey { registry: &'static str },
    /// `minecraft:resource_selector` of a registry.
    ResourceSelector { registry: &'static str },
    /// `minecraft:template_mirror`.
    TemplateMirror,
    /// `minecraft:template_rotation`.
    TemplateRotation,
    /// `minecraft:heightmap`.
    Heightmap,
    /// `minecraft:loot_table`.
    LootTable,
    /// `minecraft:loot_predicate`.
    LootPredicate,
    /// `minecraft:loot_modifier`.
    LootModifier,
    /// `minecraft:context_float_provider`.
    ContextFloatProvider,
    /// `minecraft:context_int_provider`.
    ContextIntProvider,
    /// `minecraft:slot_source`.
    SlotSource,
    /// `minecraft:dialog`.
    Dialog,
    /// `minecraft:feature`.
    Feature,
    /// `minecraft:swing_animation`.
    SwingAnimation,
}

/// A part of a message that is an entity selector.
#[derive(Debug, Clone, PartialEq)]
pub struct MessagePart {
    /// Start in the message text (byte offset).
    pub start: usize,
    /// End in the message text (byte offset).
    pub end: usize,
    /// The selector.
    pub selector: EntitySelector,
}

/// A message argument: the raw text and its selector parts (only when the source may use
/// selectors).
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The text as typed.
    pub text: String,
    /// Selector parts, in order.
    pub parts: Vec<MessagePart>,
}

/// A game profile argument: a selector of players or a name.
#[derive(Debug, Clone, PartialEq)]
pub enum GameProfileArg {
    /// `@...` (players only).
    Selector(Box<EntitySelector>),
    /// A player name, looked up by the game.
    Name(String),
}

/// A parsed argument value.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgValue {
    Bool(bool),
    Float(f32),
    Double(f64),
    Int(i32),
    Long(i64),
    String(String),
    Entity(Box<EntitySelector>),
    GameProfile(GameProfileArg),
    Coordinates(Coordinates),
    Message(Box<Message>),
    Compound(Compound),
    Tag(Tag),
    NbtPath(NbtPath),
    GameMode(GameMode),
    /// Ticks.
    Time(i32),
    Uuid(u128),
    /// A full `namespace:path` identifier (also for `Resource`).
    Identifier(String),
    Anchor(Anchor),
    IntRange(IntRange),
    FloatRange(DoubleRange),
    /// The game-specific argument types.
    Game(GameValue),
}

/// What a parse needs besides the reader.
#[derive(Clone, Copy)]
pub struct ParseCtx<'s> {
    /// The command source.
    pub source: &'s dyn Source,
    /// Limits.
    pub limits: &'s Limits,
}

impl std::fmt::Debug for ParseCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParseCtx").finish_non_exhaustive()
    }
}

fn too_low(
    r: &mut StringReader<'_>,
    start: usize,
    key: &'static str,
    min: Arg,
    found: Arg,
) -> CommandError {
    r.set_cursor(start);
    CommandError::new(key, vec![min, found]).at(r)
}

fn parse_entity(
    r: &mut StringReader<'_>,
    single: bool,
    players_only: bool,
    allow: bool,
    limits: &Limits,
) -> Result<EntitySelector, CommandError> {
    let mut selector = SelectorParser::new(r, allow, limits).parse()?;
    selector.players_arg = players_only;
    if selector.max_results > 1 && single {
        r.set_cursor(0);
        let key = if players_only {
            "argument.player.toomany"
        } else {
            "argument.entity.toomany"
        };
        return Err(CommandError::new(key, vec![]).at(r));
    }
    if selector.includes_entities && players_only && !selector.current_entity {
        r.set_cursor(0);
        return Err(CommandError::new("argument.player.entities", vec![]).at(r));
    }
    Ok(selector)
}

/// `MessageArgument.Message.parseText`.
fn parse_message(
    r: &mut StringReader<'_>,
    allow: bool,
    limits: &Limits,
) -> Result<Message, CommandError> {
    let len = crate::reader::utf16_len(r.remaining());
    if len > limits.max_message_length {
        return Err(CommandError::new(
            "argument.message.too_long",
            vec![
                Arg::Int(i32::try_from(len).unwrap_or(i32::MAX)),
                Arg::Int(i32::try_from(limits.max_message_length).unwrap_or(i32::MAX)),
            ],
        ));
    }
    let text = r.remaining().to_owned();
    let offset = r.cursor();
    let mut parts = Vec::new();
    if !allow {
        r.set_cursor(r.string().len());
        return Ok(Message { text, parts });
    }
    while r.can_read() {
        if r.peek_is('@') {
            let start = r.cursor();
            match SelectorParser::new(r, true, limits).parse() {
                Ok(selector) => parts.push(MessagePart {
                    start: start.saturating_sub(offset),
                    end: r.cursor().saturating_sub(offset),
                    selector,
                }),
                Err(e)
                    if e.key == "argument.entity.selector.missing"
                        || e.key == "argument.entity.selector.unknown" =>
                {
                    r.set_cursor(start.saturating_add(1));
                }
                Err(e) => return Err(e),
            }
        } else {
            r.skip();
        }
    }
    Ok(Message { text, parts })
}

fn parse_time(r: &mut StringReader<'_>, min: i32) -> Result<i32, CommandError> {
    let value = r.read_float()?;
    let unit = r.read_unquoted_string();
    let factor: f32 = match unit {
        "d" => 24000.0,
        "s" => 20.0,
        "t" | "" => 1.0,
        _ => return Err(CommandError::new("argument.time.invalid_unit", vec![]).at(r)),
    };
    let ticks = java_round(value * factor);
    if ticks < min {
        return Err(CommandError::new(
            "argument.time.tick_count_too_low",
            vec![Arg::Int(min), Arg::Int(ticks)],
        )
        .at(r));
    }
    Ok(ticks)
}

/// Java's `Math.round(float)`: `floor(x + 0.5)` saturated to the int range, NaN is 0.
fn java_round(v: f32) -> i32 {
    let r = (v + 0.5).floor();
    if r.is_nan() {
        0
    } else if r >= 2_147_483_647.0 {
        i32::MAX
    } else if r <= -2_147_483_648.0 {
        i32::MIN
    } else {
        // In range and integral after the checks above.
        format!("{r:.0}").parse().unwrap_or(0)
    }
}

impl ArgumentType {
    /// Whether the argument is signed in signed commands (`SignedArgument`).
    pub fn is_signable(&self) -> bool {
        matches!(self, Self::Message)
    }

    /// Parses the argument at the reader's cursor.
    pub fn parse(
        &self,
        r: &mut StringReader<'_>,
        ctx: ParseCtx<'_>,
    ) -> Result<ArgValue, CommandError> {
        let allow = ctx.source.allows_selectors();
        let limits = ctx.limits;
        Ok(match self {
            Self::Bool => ArgValue::Bool(r.read_boolean()?),
            Self::Float { min, max } => {
                let start = r.cursor();
                let v = r.read_float()?;
                if let Some(m) = *min
                    && v < m
                {
                    return Err(too_low(r, start, "argument.float.low", m.into(), v.into()));
                }
                if let Some(m) = *max
                    && v > m
                {
                    return Err(too_low(r, start, "argument.float.big", m.into(), v.into()));
                }
                ArgValue::Float(v)
            }
            Self::Double { min, max } => {
                let start = r.cursor();
                let v = r.read_double()?;
                if let Some(m) = *min
                    && v < m
                {
                    return Err(too_low(r, start, "argument.double.low", m.into(), v.into()));
                }
                if let Some(m) = *max
                    && v > m
                {
                    return Err(too_low(r, start, "argument.double.big", m.into(), v.into()));
                }
                ArgValue::Double(v)
            }
            Self::Integer { min, max } => {
                let start = r.cursor();
                let v = r.read_int()?;
                if let Some(m) = *min
                    && v < m
                {
                    return Err(too_low(
                        r,
                        start,
                        "argument.integer.low",
                        m.into(),
                        v.into(),
                    ));
                }
                if let Some(m) = *max
                    && v > m
                {
                    return Err(too_low(
                        r,
                        start,
                        "argument.integer.big",
                        m.into(),
                        v.into(),
                    ));
                }
                ArgValue::Int(v)
            }
            Self::Long { min, max } => {
                let start = r.cursor();
                let v = r.read_long()?;
                if let Some(m) = *min
                    && v < m
                {
                    return Err(too_low(r, start, "argument.long.low", m.into(), v.into()));
                }
                if let Some(m) = *max
                    && v > m
                {
                    return Err(too_low(r, start, "argument.long.big", m.into(), v.into()));
                }
                ArgValue::Long(v)
            }
            Self::String(StringKind::SingleWord) => {
                ArgValue::String(r.read_unquoted_string().to_owned())
            }
            Self::String(StringKind::QuotablePhrase) => ArgValue::String(r.read_string()?),
            Self::String(StringKind::GreedyPhrase) => {
                let text = r.remaining().to_owned();
                r.set_cursor(r.string().len());
                ArgValue::String(text)
            }
            Self::Entity {
                single,
                players_only,
            } => ArgValue::Entity(Box::new(parse_entity(
                r,
                *single,
                *players_only,
                allow,
                limits,
            )?)),
            Self::GameProfile => {
                if r.peek_is('@') {
                    let selector = SelectorParser::new(r, allow, limits).parse()?;
                    if selector.includes_entities {
                        return Err(CommandError::new("argument.player.entities", vec![]).at(r));
                    }
                    ArgValue::GameProfile(GameProfileArg::Selector(Box::new(selector)))
                } else {
                    let name = r.read_while(|c| c != ' ').to_owned();
                    ArgValue::GameProfile(GameProfileArg::Name(name))
                }
            }
            Self::BlockPos => ArgValue::Coordinates(coords::parse_block_pos(r)?),
            Self::ColumnPos => ArgValue::Coordinates(coords::parse_column_pos(r)?),
            Self::Vec3 { center } => ArgValue::Coordinates(coords::parse_vec3(r, *center)?),
            Self::Vec2 { center } => ArgValue::Coordinates(coords::parse_vec2(r, *center)?),
            Self::Rotation => ArgValue::Coordinates(coords::parse_rotation(r)?),
            Self::Message => ArgValue::Message(Box::new(parse_message(r, allow, limits)?)),
            Self::NbtCompound => ArgValue::Compound(snbt::parse_compound(r, limits)?),
            Self::NbtTag => ArgValue::Tag(snbt::parse_tag(r, limits)?),
            Self::NbtPath => ArgValue::NbtPath(nbt_path::parse(r, limits)?),
            Self::GameMode => {
                let name = r.read_unquoted_string();
                match GameMode::by_name(name) {
                    Some(m) => ArgValue::GameMode(m),
                    None => {
                        return Err(CommandError::new(
                            "argument.gamemode.invalid",
                            vec![Arg::from(name)],
                        )
                        .at(r));
                    }
                }
            }
            Self::Time { min } => ArgValue::Time(parse_time(r, *min)?),
            Self::Uuid => {
                let candidate = r.remaining();
                let run = candidate
                    .find(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
                    .map_or(candidate, |i| candidate.get(..i).unwrap_or(""));
                match (!run.is_empty()).then(|| parse_uuid(run)).flatten() {
                    Some(u) => {
                        r.set_cursor(r.cursor().saturating_add(run.len()));
                        ArgValue::Uuid(u)
                    }
                    None => return Err(CommandError::new("argument.uuid.invalid", vec![]).at(r)),
                }
            }
            Self::Identifier => ArgValue::Identifier(parse_identifier(r)?),
            Self::Resource { registry } => {
                let id = parse_identifier(r)?;
                if !ctx.source.resource_exists(registry, &id) {
                    return Err(CommandError::new(
                        "argument.resource.not_found",
                        vec![Arg::from(id), Arg::from(*registry)],
                    )
                    .at(r));
                }
                ArgValue::Identifier(id)
            }
            Self::EntityAnchor => {
                let start = r.cursor();
                let name = r.read_unquoted_string();
                match Anchor::by_name(name) {
                    Some(a) => ArgValue::Anchor(a),
                    None => {
                        let e = CommandError::new("argument.anchor.invalid", vec![Arg::from(name)]);
                        r.set_cursor(start);
                        return Err(e.at(r));
                    }
                }
            }
            Self::IntRange => ArgValue::IntRange(bounds::read_int_range(r)?),
            Self::FloatRange => ArgValue::FloatRange(bounds::read_double_range(r)?),
            _ => crate::game_args::parse(self, r, ctx)?,
        })
    }

    /// Whether parsing `text` as a whole succeeds (`Commands.createValidator`).
    fn validates(&self, text: &str, ctx: ParseCtx<'_>) -> bool {
        self.parse(&mut StringReader::new(text), ctx).is_ok()
    }

    /// `listSuggestions` of the type.
    pub fn suggestions(
        &self,
        builder: SuggestionsBuilder<'_>,
        ctx: ParseCtx<'_>,
        _args: &Args,
    ) -> Suggestions {
        let mut b = builder;
        match self {
            Self::Bool => {
                let lower = b.remaining_lower().to_owned();
                if "true".starts_with(lower.as_str()) {
                    b.suggest("true");
                }
                if "false".starts_with(lower.as_str()) {
                    b.suggest("false");
                }
            }
            Self::Entity { players_only, .. } => {
                return selector_suggestions(&b, ctx, *players_only);
            }
            Self::GameProfile => return selector_suggestions(&b, ctx, true),
            Self::Vec3 { .. } | Self::BlockPos => {
                let local = b.remaining().starts_with('^');
                let c = if local { "^" } else { "~" };
                let remaining = b.remaining().to_owned();
                let mut result = Vec::new();
                if remaining.is_empty() {
                    let full = format!("{c} {c} {c}");
                    if self.validates(&full, ctx) {
                        result.push(c.to_owned());
                        result.push(format!("{c} {c}"));
                        result.push(full);
                    }
                } else {
                    let fields: Vec<&str> = java_split(&remaining);
                    match fields.as_slice() {
                        [x] => {
                            let full = format!("{x} {c} {c}");
                            if self.validates(&full, ctx) {
                                result.push(format!("{x} {c}"));
                                result.push(full);
                            }
                        }
                        [x, y] => {
                            let full = format!("{x} {y} {c}");
                            if self.validates(&full, ctx) {
                                result.push(full);
                            }
                        }
                        _ => {}
                    }
                }
                b.suggest_matching(result.iter().map(String::as_str));
            }
            Self::Vec2 { .. } | Self::ColumnPos => {
                let remaining = b.remaining().to_owned();
                let mut result = Vec::new();
                if remaining.is_empty() {
                    if self.validates("~ ~", ctx) {
                        result.push("~".to_owned());
                        result.push("~ ~".to_owned());
                    }
                } else if let [x] = java_split(&remaining).as_slice() {
                    let full = format!("{x} ~");
                    if self.validates(&full, ctx) {
                        result.push(full);
                    }
                }
                b.suggest_matching(result.iter().map(String::as_str));
            }
            Self::GameMode => {
                b.suggest_matching(GameMode::ALL.iter().map(|m| m.name()));
            }
            Self::Time { .. } => {
                let remaining = b.remaining();
                let mut r = StringReader::new(remaining);
                if r.read_float().is_err() {
                    return b.build();
                }
                let mut sub = b.offset(b.start().saturating_add(r.cursor()));
                sub.suggest_matching(["d", "s", "t", ""]);
                return sub.build();
            }
            Self::EntityAnchor => {
                b.suggest_matching(["feet", "eyes"]);
            }
            Self::Resource { registry } => {
                let entries = ctx.source.registry_entries(registry);
                b.suggest_resource_ids(entries.iter().map(String::as_str));
            }
            _ => return crate::game_args::suggestions(self, b, ctx),
        }
        b.build()
    }

    /// The wire form for the commands packet.
    pub fn wire(&self) -> Parser {
        let p = |name: &'static str, props: ParserProps| Parser { name, props };
        match self {
            Self::Bool => Parser::plain("brigadier:bool"),
            Self::Float { min, max } => p(
                "brigadier:float",
                ParserProps::Float {
                    min: *min,
                    max: *max,
                },
            ),
            Self::Double { min, max } => p(
                "brigadier:double",
                ParserProps::Double {
                    min: *min,
                    max: *max,
                },
            ),
            Self::Integer { min, max } => p(
                "brigadier:integer",
                ParserProps::Integer {
                    min: *min,
                    max: *max,
                },
            ),
            Self::Long { min, max } => p(
                "brigadier:long",
                ParserProps::Long {
                    min: *min,
                    max: *max,
                },
            ),
            Self::String(kind) => p("brigadier:string", ParserProps::String(*kind)),
            Self::Entity {
                single,
                players_only,
            } => p(
                "minecraft:entity",
                ParserProps::Entity {
                    single: *single,
                    players_only: *players_only,
                },
            ),
            Self::GameProfile => Parser::plain("minecraft:game_profile"),
            Self::BlockPos => Parser::plain("minecraft:block_pos"),
            Self::ColumnPos => Parser::plain("minecraft:column_pos"),
            Self::Vec3 { .. } => Parser::plain("minecraft:vec3"),
            Self::Vec2 { .. } => Parser::plain("minecraft:vec2"),
            Self::Rotation => Parser::plain("minecraft:rotation"),
            Self::Message => Parser::plain("minecraft:message"),
            Self::NbtCompound => Parser::plain("minecraft:nbt_compound_tag"),
            Self::NbtTag => Parser::plain("minecraft:nbt_tag"),
            Self::NbtPath => Parser::plain("minecraft:nbt_path"),
            Self::GameMode => Parser::plain("minecraft:gamemode"),
            Self::Time { min } => p("minecraft:time", ParserProps::Time { min: *min }),
            Self::Uuid => Parser::plain("minecraft:uuid"),
            Self::Identifier => Parser::plain("minecraft:resource_location"),
            Self::Resource { registry } => p("minecraft:resource", registry_props(registry)),
            Self::EntityAnchor => Parser::plain("minecraft:entity_anchor"),
            Self::IntRange => Parser::plain("minecraft:int_range"),
            Self::FloatRange => Parser::plain("minecraft:float_range"),
            Self::BlockState => Parser::plain("minecraft:block_state"),
            Self::BlockPredicate => Parser::plain("minecraft:block_predicate"),
            Self::ItemStack => Parser::plain("minecraft:item_stack"),
            Self::ItemPredicate => Parser::plain("minecraft:item_predicate"),
            Self::TeamColor => Parser::plain("minecraft:team_color"),
            Self::HexColor => Parser::plain("minecraft:hex_color"),
            Self::Component => Parser::plain("minecraft:component"),
            Self::Style => Parser::plain("minecraft:style"),
            Self::Objective => Parser::plain("minecraft:objective"),
            Self::ObjectiveCriteria => Parser::plain("minecraft:objective_criteria"),
            Self::Operation => Parser::plain("minecraft:operation"),
            Self::Particle => Parser::plain("minecraft:particle"),
            Self::Angle => Parser::plain("minecraft:angle"),
            Self::ScoreboardSlot => Parser::plain("minecraft:scoreboard_slot"),
            Self::ScoreHolder { multiple } => p(
                "minecraft:score_holder",
                ParserProps::ScoreHolder {
                    multiple: *multiple,
                },
            ),
            Self::Swizzle => Parser::plain("minecraft:swizzle"),
            Self::Team => Parser::plain("minecraft:team"),
            Self::ItemSlot => Parser::plain("minecraft:item_slot"),
            Self::ItemSlots => Parser::plain("minecraft:item_slots"),
            Self::Function => Parser::plain("minecraft:function"),
            Self::Dimension => Parser::plain("minecraft:dimension"),
            Self::ResourceOrTag { registry } => {
                p("minecraft:resource_or_tag", registry_props(registry))
            }
            Self::ResourceOrTagKey { registry } => {
                p("minecraft:resource_or_tag_key", registry_props(registry))
            }
            Self::ResourceKey { registry } => p("minecraft:resource_key", registry_props(registry)),
            Self::ResourceSelector { registry } => {
                p("minecraft:resource_selector", registry_props(registry))
            }
            Self::TemplateMirror => Parser::plain("minecraft:template_mirror"),
            Self::TemplateRotation => Parser::plain("minecraft:template_rotation"),
            Self::Heightmap => Parser::plain("minecraft:heightmap"),
            Self::LootTable => Parser::plain("minecraft:loot_table"),
            Self::LootPredicate => Parser::plain("minecraft:loot_predicate"),
            Self::LootModifier => Parser::plain("minecraft:loot_modifier"),
            Self::ContextFloatProvider => Parser::plain("minecraft:context_float_provider"),
            Self::ContextIntProvider => Parser::plain("minecraft:context_int_provider"),
            Self::SlotSource => Parser::plain("minecraft:slot_source"),
            Self::Dialog => Parser::plain("minecraft:dialog"),
            Self::Feature => Parser::plain("minecraft:feature"),
            Self::SwingAnimation => Parser::plain("minecraft:swing_animation"),
        }
    }
}

fn registry_props(registry: &str) -> ParserProps {
    ParserProps::Registry(
        Identifier::new(registry).unwrap_or_else(|_| Identifier::vanilla("empty")),
    )
}

/// Java's `String.split(" ")`: trailing empty fields are dropped.
fn java_split(s: &str) -> Vec<&str> {
    let mut fields: Vec<&str> = s.split(' ').collect();
    while fields.last().is_some_and(|f| f.is_empty()) {
        fields.pop();
    }
    fields
}

/// Suggestions of entity and game profile arguments (`EntityArgument.listSuggestions`).
pub(crate) fn selector_suggestions(
    b: &SuggestionsBuilder<'_>,
    ctx: ParseCtx<'_>,
    _players_only: bool,
) -> Suggestions {
    let input = b.input();
    let mut r = StringReader::new(input);
    r.set_cursor(b.start());
    let allow = ctx.source.allows_selectors();
    let mut parser = SelectorParser::new(&mut r, allow, ctx.limits);
    let _ = parser.parse();
    // The server has no selected entities; both kinds offer the online player names.
    let names = ctx.source.online_player_names();
    let tags = ctx.source.registry_tags("minecraft:entity_type");
    parser.suggestions(b, &names, &tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_like_java() {
        assert_eq!(java_round(0.5), 1);
        assert_eq!(java_round(-0.5), 0);
        assert_eq!(java_round(1.0e20), i32::MAX);
        assert_eq!(java_round(f32::NAN), 0);
    }
}
