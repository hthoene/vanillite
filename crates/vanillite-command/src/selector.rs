//! Entity selectors: `@p @a @r @s @e @n` with options, player names and UUIDs (Vanilla's
//! `EntitySelectorParser` and `EntitySelectorOptions`). The parser builds a typed
//! [`EntitySelector`]; the game evaluates it against its entities.

use vanillite_data::registry::ENTITY_TYPE;
use vanillite_nbt::Compound;
use vanillite_protocol::text::Component;

use crate::bounds::{self, DoubleRange, FloatRange, IntRange};
use crate::error::{Arg, CommandError};
use crate::limits::Limits;
use crate::reader::StringReader;
use crate::snbt;
use crate::suggestion::{Suggestions, SuggestionsBuilder};
use crate::values::{GameMode, parse_identifier, parse_uuid};

/// How the matches are ordered before the limit applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// The order the world keeps them in.
    Arbitrary,
    /// Nearest first.
    Nearest,
    /// Furthest first.
    Furthest,
    /// Shuffled.
    Random,
}

/// The check of one advancement in `advancements={...}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvancementCheck {
    /// The advancement is (not) done.
    Done(bool),
    /// Criteria that must be (not) done.
    Criteria(Vec<(String, bool)>),
}

/// A filter of a selector; all filters must match.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// The entity is alive (`@e`, `@n`).
    Alive,
    /// `name=`.
    Name { name: String, inverted: bool },
    /// `gamemode=`; matches players only.
    GameMode { mode: GameMode, inverted: bool },
    /// `team=` (empty name: no team).
    Team { team: String, inverted: bool },
    /// `type=`.
    Type { id: String, inverted: bool },
    /// `type=#tag`.
    TypeTag { tag: String, inverted: bool },
    /// `tag=` (empty: no tags).
    Tag { tag: String, inverted: bool },
    /// `nbt=`.
    Nbt { nbt: Compound, inverted: bool },
    /// `scores={objective=range,...}`.
    Scores(Vec<(String, IntRange)>),
    /// `advancements={id=bool|{criterion=bool}}`; matches players only.
    Advancements(Vec<(String, AdvancementCheck)>),
    /// `predicate=`.
    Predicate { id: String, inverted: bool },
}

/// A parsed selector.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySelector {
    /// Maximum number of results.
    pub max_results: i32,
    /// May select entities other than players.
    pub includes_entities: bool,
    /// Only the source's world (position options were given).
    pub world_limited: bool,
    /// `@s`.
    pub current_entity: bool,
    /// Order before the limit.
    pub order: Order,
    /// `distance=`.
    pub distance: Option<DoubleRange>,
    /// `level=`; matches players only.
    pub level: Option<IntRange>,
    /// `x=`, `y=`, `z=`: replace the source position's coordinates.
    pub position: [Option<f64>; 3],
    /// `dx=`, `dy=`, `dz=`: a volume from the position.
    pub delta: [Option<f64>; 3],
    /// `x_rotation=`.
    pub x_rotation: Option<FloatRange>,
    /// `y_rotation=`.
    pub y_rotation: Option<FloatRange>,
    /// The entity type all matches have (`minecraft:player` for `@a @p @r`), for fast lookup.
    pub limit_to_type: Option<String>,
    /// Filters.
    pub filters: Vec<Filter>,
    /// A plain player name.
    pub player_name: Option<String>,
    /// A plain UUID.
    pub uuid: Option<u128>,
    /// Written with `@` (needs the selector permission when executed).
    pub uses_selector: bool,
    /// The argument takes players only (`EntityArgument.players()`): finding none is
    /// `argument.entity.notfound.player` (`getPlayers`), else `.entity` (`getEntities`).
    pub players_arg: bool,
}

impl EntitySelector {
    fn empty() -> Self {
        Self {
            max_results: 0,
            includes_entities: false,
            world_limited: false,
            current_entity: false,
            order: Order::Arbitrary,
            distance: None,
            level: None,
            position: [None; 3],
            delta: [None; 3],
            x_rotation: None,
            y_rotation: None,
            limit_to_type: None,
            filters: Vec::new(),
            player_name: None,
            uuid: None,
            uses_selector: false,
            players_arg: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Limitation {
    #[default]
    None,
    Single,
    Multiple,
}

/// `InvertableSetOptionState`.
#[derive(Debug, Clone, Default)]
struct InvertableState {
    state: Limitation,
    tags: Vec<String>,
}

impl InvertableState {
    fn can_parse_positive(&self) -> bool {
        self.state == Limitation::None
    }
    fn can_parse_negative(&self) -> bool {
        self.state != Limitation::Single
    }
    fn can_parse_any_tag(&self) -> bool {
        self.can_parse_negative()
    }
    fn can_parse_tag(&self, tag: &str) -> bool {
        self.can_parse_any_tag() && !self.tags.iter().any(|t| t == tag)
    }
    fn can_parse_any(&self) -> bool {
        self.state != Limitation::Single
    }
    fn can_parse(&self, inverted: bool) -> bool {
        if inverted {
            self.can_parse_negative()
        } else {
            self.can_parse_positive()
        }
    }
    fn mark(&mut self, inverted: bool) {
        self.state = if inverted {
            Limitation::Multiple
        } else {
            Limitation::Single
        };
    }
    fn mark_tag(&mut self, tag: String) {
        self.state = Limitation::Multiple;
        self.tags.push(tag);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suggest {
    Nothing,
    NameOrSelector,
    Name,
    Selector,
    OpenOptions,
    OptionsKeyOrClose,
    OptionsKey,
    OptionsNextOrClose,
    Sort,
    GameMode,
    Type,
}

/// The options in Vanilla's registration order.
const OPTIONS: [&str; 21] = [
    "name",
    "distance",
    "level",
    "x",
    "y",
    "z",
    "dx",
    "dy",
    "dz",
    "x_rotation",
    "y_rotation",
    "limit",
    "sort",
    "gamemode",
    "team",
    "type",
    "tag",
    "nbt",
    "scores",
    "advancements",
    "predicate",
];

fn description(option: &str) -> &'static str {
    match option {
        "name" => "argument.entity.options.name.description",
        "distance" => "argument.entity.options.distance.description",
        "level" => "argument.entity.options.level.description",
        "x" => "argument.entity.options.x.description",
        "y" => "argument.entity.options.y.description",
        "z" => "argument.entity.options.z.description",
        "dx" => "argument.entity.options.dx.description",
        "dy" => "argument.entity.options.dy.description",
        "dz" => "argument.entity.options.dz.description",
        "x_rotation" => "argument.entity.options.x_rotation.description",
        "y_rotation" => "argument.entity.options.y_rotation.description",
        "limit" => "argument.entity.options.limit.description",
        "sort" => "argument.entity.options.sort.description",
        "gamemode" => "argument.entity.options.gamemode.description",
        "team" => "argument.entity.options.team.description",
        "type" => "argument.entity.options.type.description",
        "tag" => "argument.entity.options.tag.description",
        "nbt" => "argument.entity.options.nbt.description",
        "scores" => "argument.entity.options.scores.description",
        "advancements" => "argument.entity.options.advancements.description",
        _ => "argument.entity.options.predicate.description",
    }
}

/// Selector suggestions with Vanilla's tooltips, in Vanilla's order.
const SELECTORS: [(&str, &str); 6] = [
    ("@p", "argument.entity.selector.nearestPlayer"),
    ("@a", "argument.entity.selector.allPlayers"),
    ("@r", "argument.entity.selector.randomPlayer"),
    ("@s", "argument.entity.selector.self"),
    ("@e", "argument.entity.selector.allEntities"),
    ("@n", "argument.entity.selector.nearestEntity"),
];

/// The parser of one selector (`EntitySelectorParser`).
#[derive(Debug)]
pub struct SelectorParser<'r, 'a> {
    reader: &'r mut StringReader<'a>,
    allow_selectors: bool,
    limits: &'r Limits,
    sel: EntitySelector,
    start_position: usize,
    suggest: Suggest,
    name_option: InvertableState,
    limited: bool,
    sorted: bool,
    gamemode_option: InvertableState,
    team_option: InvertableState,
    type_option: InvertableState,
    scores: bool,
    advancements: bool,
}

fn rollback(
    r: &mut StringReader<'_>,
    start: usize,
    key: &'static str,
    args: Vec<Arg>,
) -> CommandError {
    r.set_cursor(start);
    CommandError::new(key, args).at(r)
}

impl<'r, 'a> SelectorParser<'r, 'a> {
    /// A parser at the reader's cursor.
    pub fn new(
        reader: &'r mut StringReader<'a>,
        allow_selectors: bool,
        limits: &'r Limits,
    ) -> Self {
        Self {
            reader,
            allow_selectors,
            limits,
            sel: EntitySelector::empty(),
            start_position: 0,
            suggest: Suggest::Nothing,
            name_option: InvertableState::default(),
            limited: false,
            sorted: false,
            gamemode_option: InvertableState::default(),
            team_option: InvertableState::default(),
            type_option: InvertableState::default(),
            scores: false,
            advancements: false,
        }
    }

    /// `parse`.
    pub fn parse(&mut self) -> Result<EntitySelector, CommandError> {
        self.start_position = self.reader.cursor();
        self.suggest = Suggest::NameOrSelector;
        if self.reader.peek_is('@') {
            if !self.allow_selectors {
                return Err(
                    CommandError::new("argument.entity.selector.not_allowed", vec![])
                        .at(self.reader),
                );
            }
            self.reader.skip();
            self.parse_selector()?;
        } else {
            self.parse_name_or_uuid()?;
        }
        Ok(self.sel.clone())
    }

    fn parse_selector(&mut self) -> Result<(), CommandError> {
        self.sel.uses_selector = true;
        self.suggest = Suggest::Selector;
        let start = self.reader.cursor();
        let Some(kind) = self.reader.read() else {
            return Err(
                CommandError::new("argument.entity.selector.missing", vec![]).at(self.reader),
            );
        };
        let player = || Some("minecraft:player".to_owned());
        let alive = match kind {
            'a' => {
                self.sel.max_results = i32::MAX;
                self.sel.limit_to_type = player();
                false
            }
            'e' => {
                self.sel.max_results = i32::MAX;
                self.sel.includes_entities = true;
                true
            }
            'n' => {
                self.sel.max_results = 1;
                self.sel.includes_entities = true;
                self.sel.order = Order::Nearest;
                true
            }
            'p' => {
                self.sel.max_results = 1;
                self.sel.order = Order::Nearest;
                self.sel.limit_to_type = player();
                false
            }
            'r' => {
                self.sel.max_results = 1;
                self.sel.order = Order::Random;
                self.sel.limit_to_type = player();
                false
            }
            's' => {
                self.sel.max_results = 1;
                self.sel.includes_entities = true;
                self.sel.current_entity = true;
                false
            }
            other => {
                self.reader.set_cursor(start);
                return Err(CommandError::new(
                    "argument.entity.selector.unknown",
                    vec![Arg::from(format!("@{other}"))],
                )
                .at(self.reader));
            }
        };
        if alive {
            self.sel.filters.push(Filter::Alive);
        }
        self.suggest = Suggest::OpenOptions;
        if self.reader.peek_is('[') {
            self.reader.skip();
            self.suggest = Suggest::OptionsKeyOrClose;
            self.parse_options()?;
        }
        Ok(())
    }

    fn parse_name_or_uuid(&mut self) -> Result<(), CommandError> {
        if self.reader.can_read() {
            self.suggest = Suggest::Name;
        }
        let start = self.reader.cursor();
        let name = self.reader.read_string()?;
        if let Some(uuid) = parse_uuid(&name) {
            self.sel.uuid = Some(uuid);
            self.sel.includes_entities = true;
        } else {
            if name.is_empty() || name.chars().map(char::len_utf16).sum::<usize>() > 16 {
                self.reader.set_cursor(start);
                return Err(CommandError::new("argument.entity.invalid", vec![]).at(self.reader));
            }
            self.sel.includes_entities = false;
            self.sel.player_name = Some(name);
        }
        self.sel.max_results = 1;
        Ok(())
    }

    fn parse_options(&mut self) -> Result<(), CommandError> {
        self.suggest = Suggest::OptionsKey;
        self.reader.skip_whitespace();
        let mut count = 0usize;
        while self.reader.can_read() && !self.reader.peek_is(']') {
            self.reader.skip_whitespace();
            let start = self.reader.cursor();
            let key = self.reader.read_string()?;
            count = count.saturating_add(1);
            if count > self.limits.max_selector_options {
                return Err(rollback(
                    self.reader,
                    start,
                    "argument.entity.options.inapplicable",
                    vec![Arg::from(key)],
                ));
            }
            self.check_option(&key, start)?;
            self.reader.skip_whitespace();
            if !self.reader.peek_is('=') {
                return Err(rollback(
                    self.reader,
                    start,
                    "argument.entity.options.valueless",
                    vec![Arg::from(key)],
                ));
            }
            self.reader.skip();
            self.reader.skip_whitespace();
            self.suggest = Suggest::Nothing;
            self.apply_option(&key)?;
            self.reader.skip_whitespace();
            self.suggest = Suggest::OptionsNextOrClose;
            if self.reader.can_read() {
                if !self.reader.peek_is(',') {
                    if !self.reader.peek_is(']') {
                        return Err(CommandError::new(
                            "argument.entity.options.unterminated",
                            vec![],
                        )
                        .at(self.reader));
                    }
                    break;
                }
                self.reader.skip();
                self.suggest = Suggest::OptionsKey;
            }
        }
        if self.reader.can_read() {
            self.reader.skip();
            self.suggest = Suggest::Nothing;
            Ok(())
        } else {
            Err(CommandError::new("argument.entity.options.unterminated", vec![]).at(self.reader))
        }
    }

    fn can_use(&self, option: &str) -> bool {
        match option {
            "name" => self.name_option.can_parse_any(),
            "distance" => self.sel.distance.is_none(),
            "level" => self.sel.level.is_none(),
            "x" => self.sel.position[0].is_none(),
            "y" => self.sel.position[1].is_none(),
            "z" => self.sel.position[2].is_none(),
            "dx" => self.sel.delta[0].is_none(),
            "dy" => self.sel.delta[1].is_none(),
            "dz" => self.sel.delta[2].is_none(),
            "x_rotation" => self.sel.x_rotation.is_none(),
            "y_rotation" => self.sel.y_rotation.is_none(),
            "limit" => !self.sel.current_entity && !self.limited,
            "sort" => !self.sel.current_entity && !self.sorted,
            "gamemode" => self.gamemode_option.can_parse_any(),
            "team" => self.team_option.can_parse_any(),
            "type" => self.type_option.can_parse_any(),
            "scores" => !self.scores,
            "advancements" => !self.advancements,
            _ => true,
        }
    }

    fn check_option(&mut self, key: &str, start: usize) -> Result<(), CommandError> {
        if !OPTIONS.contains(&key) {
            return Err(rollback(
                self.reader,
                start,
                "argument.entity.options.unknown",
                vec![Arg::from(key)],
            ));
        }
        if !self.can_use(key) {
            return Err(rollback(
                self.reader,
                start,
                "argument.entity.options.inapplicable",
                vec![Arg::from(key)],
            ));
        }
        Ok(())
    }

    fn invert(&mut self) -> bool {
        self.reader.skip_whitespace();
        if self.reader.peek_is('!') {
            self.reader.skip();
            self.reader.skip_whitespace();
            true
        } else {
            false
        }
    }

    fn is_tag(&mut self) -> bool {
        self.reader.skip_whitespace();
        if self.reader.peek_is('#') {
            self.reader.skip();
            self.reader.skip_whitespace();
            true
        } else {
            false
        }
    }

    fn set_coord(&mut self, i: usize, delta: bool) -> Result<(), CommandError> {
        self.sel.world_limited = true;
        let v = self.reader.read_double()?;
        let slot = if delta {
            self.sel.delta.get_mut(i)
        } else {
            self.sel.position.get_mut(i)
        };
        if let Some(slot) = slot {
            *slot = Some(v);
        }
        Ok(())
    }

    fn apply_option(&mut self, key: &str) -> Result<(), CommandError> {
        match key {
            "name" => {
                let start = self.reader.cursor();
                let inverted = self.invert();
                let name = self.reader.read_string()?;
                if !self.name_option.can_parse(inverted) {
                    return Err(rollback(
                        self.reader,
                        start,
                        "argument.entity.options.inapplicable",
                        vec![Arg::from("name")],
                    ));
                }
                self.name_option.mark(inverted);
                self.sel.filters.push(Filter::Name { name, inverted });
            }
            "distance" => {
                let start = self.reader.cursor();
                let v = bounds::read_double_range(self.reader)?;
                if v.min.is_some_and(|m| m < 0.0) || v.max.is_some_and(|m| m < 0.0) {
                    return Err(rollback(
                        self.reader,
                        start,
                        "argument.entity.options.distance.negative",
                        vec![],
                    ));
                }
                self.sel.distance = Some(v);
                self.sel.world_limited = true;
            }
            "level" => {
                let start = self.reader.cursor();
                let v = bounds::read_int_range(self.reader)?;
                if v.min.is_some_and(|m| m < 0) || v.max.is_some_and(|m| m < 0) {
                    return Err(rollback(
                        self.reader,
                        start,
                        "argument.entity.options.level.negative",
                        vec![],
                    ));
                }
                self.sel.level = Some(v);
                self.sel.includes_entities = false;
            }
            "x" => self.set_coord(0, false)?,
            "y" => self.set_coord(1, false)?,
            "z" => self.set_coord(2, false)?,
            "dx" => self.set_coord(0, true)?,
            "dy" => self.set_coord(1, true)?,
            "dz" => self.set_coord(2, true)?,
            "x_rotation" => self.sel.x_rotation = Some(bounds::read_float_range(self.reader)?),
            "y_rotation" => self.sel.y_rotation = Some(bounds::read_float_range(self.reader)?),
            "limit" => {
                let start = self.reader.cursor();
                let count = self.reader.read_int()?;
                if count < 1 {
                    return Err(rollback(
                        self.reader,
                        start,
                        "argument.entity.options.limit.toosmall",
                        vec![],
                    ));
                }
                self.sel.max_results = count;
                self.limited = true;
            }
            "sort" => {
                let start = self.reader.cursor();
                let name = self.reader.read_unquoted_string();
                self.suggest = Suggest::Sort;
                self.sel.order = match name {
                    "nearest" => Order::Nearest,
                    "furthest" => Order::Furthest,
                    "random" => Order::Random,
                    "arbitrary" => Order::Arbitrary,
                    _ => {
                        return Err(rollback(
                            self.reader,
                            start,
                            "argument.entity.options.sort.irreversible",
                            vec![Arg::from(name)],
                        ));
                    }
                };
                self.sorted = true;
            }
            "gamemode" => {
                self.suggest = Suggest::GameMode;
                let start = self.reader.cursor();
                let inverted = self.invert();
                if !self.gamemode_option.can_parse(inverted) {
                    return Err(rollback(
                        self.reader,
                        start,
                        "argument.entity.options.inapplicable",
                        vec![Arg::from("gamemode")],
                    ));
                }
                let name = self.reader.read_unquoted_string();
                let Some(mode) = GameMode::by_name(name) else {
                    return Err(rollback(
                        self.reader,
                        start,
                        "argument.entity.options.mode.invalid",
                        vec![Arg::from(name)],
                    ));
                };
                self.sel.includes_entities = false;
                self.sel.filters.push(Filter::GameMode { mode, inverted });
                self.gamemode_option.mark(inverted);
            }
            "team" => {
                let start = self.reader.cursor();
                let inverted = self.invert();
                let team = self.reader.read_unquoted_string().to_owned();
                if !self.team_option.can_parse(inverted) {
                    return Err(rollback(
                        self.reader,
                        start,
                        "argument.entity.options.inapplicable",
                        vec![Arg::from("team")],
                    ));
                }
                self.sel.filters.push(Filter::Team { team, inverted });
                self.team_option.mark(inverted);
            }
            "type" => self.parse_type()?,
            "tag" => {
                let inverted = self.invert();
                let tag = self.reader.read_unquoted_string().to_owned();
                self.sel.filters.push(Filter::Tag { tag, inverted });
            }
            "nbt" => {
                let inverted = self.invert();
                let nbt = snbt::parse_compound(self.reader, self.limits)?;
                self.sel.filters.push(Filter::Nbt { nbt, inverted });
            }
            "scores" => self.parse_scores()?,
            "advancements" => self.parse_advancements()?,
            _ => {
                let inverted = self.invert();
                let id = parse_identifier(self.reader)?;
                self.sel.filters.push(Filter::Predicate { id, inverted });
            }
        }
        Ok(())
    }

    fn parse_type(&mut self) -> Result<(), CommandError> {
        self.suggest = Suggest::Type;
        let start = self.reader.cursor();
        let inverted = self.invert();
        let inapplicable = |r: &mut StringReader<'_>| {
            rollback(
                r,
                start,
                "argument.entity.options.inapplicable",
                vec![Arg::from("type")],
            )
        };
        if self.is_tag() {
            if !self.type_option.can_parse_any_tag() {
                return Err(inapplicable(self.reader));
            }
            let tag = parse_identifier(self.reader)?;
            if !self.type_option.can_parse_tag(&tag) {
                return Err(inapplicable(self.reader));
            }
            self.sel.filters.push(Filter::TypeTag {
                tag: tag.clone(),
                inverted,
            });
            self.type_option.mark_tag(tag);
        } else {
            if !self.type_option.can_parse(inverted) {
                return Err(inapplicable(self.reader));
            }
            let id = parse_identifier(self.reader)?;
            if ENTITY_TYPE.id(&id).is_none() {
                return Err(rollback(
                    self.reader,
                    start,
                    "argument.entity.options.type.invalid",
                    vec![Arg::from(id)],
                ));
            }
            if id == "minecraft:player" && !inverted {
                self.sel.includes_entities = false;
            }
            if !inverted {
                self.sel.limit_to_type = Some(id.clone());
            }
            self.sel.filters.push(Filter::Type { id, inverted });
            self.type_option.mark(inverted);
        }
        Ok(())
    }

    fn parse_scores(&mut self) -> Result<(), CommandError> {
        let r = &mut *self.reader;
        let mut expected: Vec<(String, IntRange)> = Vec::new();
        r.expect('{')?;
        r.skip_whitespace();
        while r.can_read() && !r.peek_is('}') {
            r.skip_whitespace();
            let name = r.read_unquoted_string().to_owned();
            r.skip_whitespace();
            r.expect('=')?;
            r.skip_whitespace();
            let value = bounds::read_int_range(r)?;
            // Vanilla keeps a map: a repeated objective replaces the earlier one.
            expected.retain(|(n, _)| *n != name);
            expected.push((name, value));
            r.skip_whitespace();
            if r.peek_is(',') {
                r.skip();
            }
        }
        r.expect('}')?;
        if !expected.is_empty() {
            self.sel.filters.push(Filter::Scores(expected));
        }
        self.scores = true;
        Ok(())
    }

    fn parse_advancements(&mut self) -> Result<(), CommandError> {
        let r = &mut *self.reader;
        let mut expected: Vec<(String, AdvancementCheck)> = Vec::new();
        r.expect('{')?;
        r.skip_whitespace();
        while r.can_read() && !r.peek_is('}') {
            r.skip_whitespace();
            let name = parse_identifier(r)?;
            r.skip_whitespace();
            r.expect('=')?;
            r.skip_whitespace();
            let check = if r.peek_is('{') {
                let mut criteria: Vec<(String, bool)> = Vec::new();
                r.skip_whitespace();
                r.expect('{')?;
                r.skip_whitespace();
                while r.can_read() && !r.peek_is('}') {
                    r.skip_whitespace();
                    let criterion = r.read_unquoted_string().to_owned();
                    r.skip_whitespace();
                    r.expect('=')?;
                    r.skip_whitespace();
                    let value = r.read_boolean()?;
                    criteria.retain(|(c, _)| *c != criterion);
                    criteria.push((criterion, value));
                    r.skip_whitespace();
                    if r.peek_is(',') {
                        r.skip();
                    }
                }
                r.skip_whitespace();
                r.expect('}')?;
                r.skip_whitespace();
                AdvancementCheck::Criteria(criteria)
            } else {
                AdvancementCheck::Done(r.read_boolean()?)
            };
            expected.retain(|(n, _)| *n != name);
            expected.push((name, check));
            r.skip_whitespace();
            if r.peek_is(',') {
                r.skip();
            }
        }
        r.expect('}')?;
        if !expected.is_empty() {
            self.sel.filters.push(Filter::Advancements(expected));
            self.sel.includes_entities = false;
        }
        self.advancements = true;
        Ok(())
    }

    /// `fillSuggestions`: what may follow where parsing stopped. `names` are the player (and
    /// entity) names to offer, `type_tags` the entity type tags without `#`.
    pub fn suggestions(
        &self,
        builder: &SuggestionsBuilder<'_>,
        names: &[String],
        type_tags: &[String],
    ) -> Suggestions {
        let mut b = builder.offset(self.reader.cursor());
        let suggest_names = |b: &mut SuggestionsBuilder<'_>| {
            b.suggest_matching(names.iter().map(String::as_str));
        };
        match self.suggest {
            Suggest::Nothing => {}
            Suggest::NameOrSelector => {
                suggest_names(&mut b);
                if self.allow_selectors {
                    fill_selectors(&mut b);
                }
            }
            Suggest::Name => {
                let mut sub = b.offset(self.start_position);
                suggest_names(&mut sub);
                b.add(sub);
            }
            Suggest::Selector => {
                let mut sub = b.offset(b.start().saturating_sub(1));
                fill_selectors(&mut sub);
                b.add(sub);
            }
            Suggest::OpenOptions => {
                b.suggest("[");
            }
            Suggest::OptionsKeyOrClose => {
                b.suggest("]");
                self.suggest_option_names(&mut b);
            }
            Suggest::OptionsKey => self.suggest_option_names(&mut b),
            Suggest::OptionsNextOrClose => {
                b.suggest(",");
                b.suggest("]");
            }
            Suggest::Sort => {
                b.suggest_matching(["nearest", "furthest", "random", "arbitrary"]);
            }
            Suggest::GameMode => {
                let mut prefix = b.remaining_lower().to_owned();
                let mut normal = self.gamemode_option.can_parse_positive();
                let mut inverted = self.gamemode_option.can_parse_negative();
                if let Some(rest) = prefix.strip_prefix('!') {
                    normal = false;
                    prefix = rest.to_owned();
                } else if !prefix.is_empty() {
                    inverted = false;
                }
                for mode in GameMode::ALL {
                    if mode.name().starts_with(prefix.as_str()) {
                        if inverted {
                            b.suggest(format!("!{}", mode.name()));
                        }
                        if normal {
                            b.suggest(mode.name());
                        }
                    }
                }
            }
            Suggest::Type => {
                let types = ENTITY_TYPE.entries.iter().copied();
                if self.type_option.can_parse_negative() {
                    b.suggest_resources(types.clone(), "!");
                }
                if self.type_option.can_parse_positive() {
                    b.suggest_resource_ids(types);
                }
                if self.type_option.can_parse_any_tag() {
                    let tags: Vec<&str> = type_tags
                        .iter()
                        .map(String::as_str)
                        .filter(|t| self.type_option.can_parse_tag(t))
                        .collect();
                    if !tags.is_empty() {
                        b.suggest_resources(tags.iter().copied(), "#");
                        b.suggest_resources(tags.iter().copied(), "!#");
                    }
                }
            }
        }
        b.build()
    }

    fn suggest_option_names(&self, b: &mut SuggestionsBuilder<'_>) {
        let prefix = b.remaining_lower().to_owned();
        for option in OPTIONS {
            if self.can_use(option) && option.starts_with(prefix.as_str()) {
                b.suggest_with(
                    format!("{option}="),
                    Some(Component::translatable(description(option), vec![])),
                );
            }
        }
    }
}

fn fill_selectors(b: &mut SuggestionsBuilder<'_>) {
    for (text, key) in SELECTORS {
        b.suggest_with(text, Some(Component::translatable(key, vec![])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<EntitySelector, CommandError> {
        let limits = Limits::default();
        let mut r = StringReader::new(s);
        SelectorParser::new(&mut r, true, &limits).parse()
    }

    #[test]
    fn selectors_and_options() {
        let s = parse("@e[type=minecraft:cow,limit=2,sort=nearest,distance=..5,tag=!x]").ok();
        let s = s.as_ref();
        assert_eq!(s.map(|s| s.max_results), Some(2));
        assert_eq!(s.map(|s| s.order), Some(Order::Nearest));
        assert_eq!(
            s.and_then(|s| s.limit_to_type.clone()).as_deref(),
            Some("minecraft:cow")
        );
        assert_eq!(s.map(|s| s.world_limited), Some(true));
        let a = parse("@a").ok();
        assert_eq!(
            a.map(|a| (a.includes_entities, a.max_results)),
            Some((false, i32::MAX))
        );
        let n = parse("Notch").ok();
        assert_eq!(n.and_then(|n| n.player_name).as_deref(), Some("Notch"));
        let u = parse("10920508-d5d8-3eed-93d2-92f193afe7d7").ok();
        assert!(u.is_some_and(|u| u.uuid.is_some()));
    }

    #[test]
    fn option_errors() {
        let key = |s: &str| parse(s).err().map(|e| e.key);
        assert_eq!(key("@x"), Some("argument.entity.selector.unknown"));
        assert_eq!(key("@"), Some("argument.entity.selector.missing"));
        assert_eq!(key("@e[foo=1]"), Some("argument.entity.options.unknown"));
        assert_eq!(
            key("@e[limit=0]"),
            Some("argument.entity.options.limit.toosmall")
        );
        assert_eq!(
            key("@s[limit=1]"),
            Some("argument.entity.options.inapplicable")
        );
        assert_eq!(
            key("@e[type=cow,type=pig]"),
            Some("argument.entity.options.inapplicable")
        );
        assert_eq!(
            key("@e[distance=-1]"),
            Some("argument.entity.options.distance.negative")
        );
        assert_eq!(
            key("@e[limit=1"),
            Some("argument.entity.options.unterminated")
        );
        assert_eq!(
            key("@e[type=minecraft:nope]"),
            Some("argument.entity.options.type.invalid")
        );
        assert_eq!(
            key("this_name_is_too_long"),
            Some("argument.entity.invalid")
        );
    }

    #[test]
    fn selectors_need_permission() {
        let limits = Limits::default();
        let mut r = StringReader::new("@a");
        let e = SelectorParser::new(&mut r, false, &limits).parse().err();
        assert_eq!(
            e.map(|e| e.key),
            Some("argument.entity.selector.not_allowed")
        );
    }

    #[test]
    fn deep_nbt_fails_cleanly() {
        let deep = format!("@e[nbt={}]", "{a:".repeat(100_000));
        assert!(parse(&deep).is_err());
        let mut many = String::from("@e[");
        for _ in 0..1000 {
            many.push_str("tag=a,");
        }
        many.push(']');
        assert_eq!(
            parse(&many).err().map(|e| e.key),
            Some("argument.entity.options.inapplicable")
        );
    }

    #[test]
    fn option_suggestions() {
        let limits = Limits::default();
        let input = "@e[";
        let mut r = StringReader::new(input);
        let mut p = SelectorParser::new(&mut r, true, &limits);
        let _ = p.parse();
        let b = SuggestionsBuilder::new(input, 0);
        let s = p.suggestions(&b, &[], &[]);
        assert_eq!(s.list.len(), 21);
        assert_eq!(s.texts().first().copied(), Some("advancements="));
    }
}
