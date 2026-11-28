//! The scoreboard: objectives, scores, display slots and teams (Vanilla's `Scoreboard`,
//! `Objective`, `Score`, `PlayerTeam`). Pure data; every change is recorded as a
//! [`ScoreEvent`] the server turns into packets (`ServerScoreboard`) and a dirty flag for saving.

use std::collections::BTreeMap;

use vanillite_base::hash::FxHashMap;
use vanillite_protocol::text::Component;

mod save;

pub use save::{slot_by_name, slot_name};

/// How an objective is shown (`ObjectiveCriteria.RenderType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderType {
    /// `integer`.
    #[default]
    Integer,
    /// `hearts`.
    Hearts,
}

impl RenderType {
    /// The serialized name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Integer => "integer",
            Self::Hearts => "hearts",
        }
    }

    /// By serialized name.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "integer" => Some(Self::Integer),
            "hearts" => Some(Self::Hearts),
            _ => None,
        }
    }
}

/// How a score is shown (`NumberFormat`).
pub use vanillite_protocol::packets::play::clientbound::NumberFormat;

/// What feeds an objective (`ObjectiveCriteria`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CriteriaKind {
    /// `dummy`.
    Dummy,
    /// `trigger`.
    Trigger,
    /// `deathCount`.
    DeathCount,
    /// `playerKillCount`.
    PlayerKillCount,
    /// `totalKillCount`.
    TotalKillCount,
    /// `health`.
    Health,
    /// `food`.
    Food,
    /// `air`.
    Air,
    /// `armor`.
    Armor,
    /// `xp`.
    Experience,
    /// `level`.
    Level,
    /// `teamkill.<color>`: the `ChatFormatting` colour id.
    TeamKill(u8),
    /// `killedByTeam.<color>`.
    KilledByTeam(u8),
    /// `<stat type>:<value>` with both ids written `namespace.path`.
    Stat(String),
}

/// The colour names of `ChatFormatting` 0..=15.
pub const COLOR_NAMES: [&str; 16] = [
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

/// The colour id of a `ChatFormatting` colour name.
pub fn color_id(name: &str) -> Option<u8> {
    COLOR_NAMES
        .iter()
        .position(|n| *n == name)
        .and_then(|i| u8::try_from(i).ok())
}

impl CriteriaKind {
    /// By name (`ObjectiveCriteria.byName`), statistics as `<type>:<value>`.
    pub fn by_name(name: &str) -> Option<Self> {
        Some(match name {
            "dummy" => Self::Dummy,
            "trigger" => Self::Trigger,
            "deathCount" => Self::DeathCount,
            "playerKillCount" => Self::PlayerKillCount,
            "totalKillCount" => Self::TotalKillCount,
            "health" => Self::Health,
            "food" => Self::Food,
            "air" => Self::Air,
            "armor" => Self::Armor,
            "xp" => Self::Experience,
            "level" => Self::Level,
            _ => {
                if let Some(c) = name.strip_prefix("teamkill.") {
                    return color_id(c).map(Self::TeamKill);
                }
                if let Some(c) = name.strip_prefix("killedByTeam.") {
                    return color_id(c).map(Self::KilledByTeam);
                }
                // A statistic of a known type and value, under its canonical name.
                return crate::stats::Stat::by_criteria_name(name).map(|s| Self::Stat(s.name()));
            }
        })
    }

    /// The name.
    pub fn name(&self) -> String {
        match self {
            Self::Dummy => "dummy".into(),
            Self::Trigger => "trigger".into(),
            Self::DeathCount => "deathCount".into(),
            Self::PlayerKillCount => "playerKillCount".into(),
            Self::TotalKillCount => "totalKillCount".into(),
            Self::Health => "health".into(),
            Self::Food => "food".into(),
            Self::Air => "air".into(),
            Self::Armor => "armor".into(),
            Self::Experience => "xp".into(),
            Self::Level => "level".into(),
            Self::TeamKill(c) => format!("teamkill.{}", color_name(*c)),
            Self::KilledByTeam(c) => format!("killedByTeam.{}", color_name(*c)),
            Self::Stat(s) => s.clone(),
        }
    }

    /// Whether commands may not change its scores.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Health | Self::Food | Self::Air | Self::Armor | Self::Experience | Self::Level
        )
    }

    /// The default render type.
    pub fn default_render_type(&self) -> RenderType {
        if *self == Self::Health {
            RenderType::Hearts
        } else {
            RenderType::Integer
        }
    }
}

fn color_name(id: u8) -> &'static str {
    COLOR_NAMES.get(usize::from(id)).copied().unwrap_or("white")
}

/// An objective.
#[derive(Debug, Clone, PartialEq)]
pub struct Objective {
    /// Name.
    pub name: String,
    /// Criteria.
    pub criteria: CriteriaKind,
    /// Display name.
    pub display_name: Component,
    /// Render type.
    pub render_type: RenderType,
    /// Score display names follow the holders' display names.
    pub display_auto_update: bool,
    /// Number format.
    pub number_format: Option<NumberFormat>,
}

impl Objective {
    /// `getFormattedDisplayName`: the display name with a hover of the name, in brackets.
    pub fn formatted_display_name(&self) -> Component {
        use vanillite_protocol::text::HoverEvent;
        Component::text("[")
            .append(
                self.display_name
                    .clone()
                    .hover(HoverEvent::ShowText(Component::text(self.name.as_str()))),
            )
            .append(Component::text("]"))
    }
}

/// A score.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    /// Value.
    pub value: i32,
    /// For `trigger` objectives: locked until `scoreboard players enable`.
    pub locked: bool,
    /// Display name override.
    pub display: Option<Component>,
    /// Number format override.
    pub number_format: Option<NumberFormat>,
}

impl Default for Score {
    fn default() -> Self {
        Score {
            value: 0,
            locked: true,
            display: None,
            number_format: None,
        }
    }
}

/// `Team.Visibility`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    /// `always`.
    #[default]
    Always,
    /// `never`.
    Never,
    /// `hideForOtherTeams`.
    HideForOtherTeams,
    /// `hideForOwnTeam`.
    HideForOwnTeam,
}

impl Visibility {
    /// All, in id order.
    pub const ALL: [Visibility; 4] = [
        Self::Always,
        Self::Never,
        Self::HideForOtherTeams,
        Self::HideForOwnTeam,
    ];

    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Never => "never",
            Self::HideForOtherTeams => "hideForOtherTeams",
            Self::HideForOwnTeam => "hideForOwnTeam",
        }
    }

    /// The network id.
    pub fn id(self) -> i32 {
        match self {
            Self::Always => 0,
            Self::Never => 1,
            Self::HideForOtherTeams => 2,
            Self::HideForOwnTeam => 3,
        }
    }

    /// By name.
    pub fn by_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.name() == name)
    }
}

/// `Team.CollisionRule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CollisionRule {
    /// `always`.
    #[default]
    Always,
    /// `never`.
    Never,
    /// `pushOtherTeams`.
    PushOtherTeams,
    /// `pushOwnTeam`.
    PushOwnTeam,
}

impl CollisionRule {
    /// All, in id order.
    pub const ALL: [CollisionRule; 4] = [
        Self::Always,
        Self::Never,
        Self::PushOtherTeams,
        Self::PushOwnTeam,
    ];

    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Never => "never",
            Self::PushOtherTeams => "pushOtherTeams",
            Self::PushOwnTeam => "pushOwnTeam",
        }
    }

    /// The network id.
    pub fn id(self) -> i32 {
        match self {
            Self::Always => 0,
            Self::Never => 1,
            Self::PushOtherTeams => 2,
            Self::PushOwnTeam => 3,
        }
    }

    /// By name.
    pub fn by_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.name() == name)
    }
}

/// A team (`PlayerTeam`).
#[derive(Debug, Clone, PartialEq)]
pub struct Team {
    /// Name.
    pub name: String,
    /// Display name.
    pub display_name: Component,
    /// Prefix of member names.
    pub prefix: Component,
    /// Suffix of member names.
    pub suffix: Component,
    /// `ChatFormatting` colour id, `None` = reset.
    pub color: Option<u8>,
    /// `friendlyFire`.
    pub allow_friendly_fire: bool,
    /// `seeFriendlyInvisibles`.
    pub see_friendly_invisibles: bool,
    /// `nametagVisibility`.
    pub nametag_visibility: Visibility,
    /// `deathMessageVisibility`.
    pub death_message_visibility: Visibility,
    /// `collisionRule`.
    pub collision_rule: CollisionRule,
    /// Members in join order.
    pub players: Vec<String>,
}

impl Team {
    /// A team with Vanilla's defaults.
    pub fn new(name: &str) -> Team {
        Team {
            name: name.to_owned(),
            display_name: Component::text(name),
            prefix: Component::empty(),
            suffix: Component::empty(),
            color: None,
            allow_friendly_fire: true,
            see_friendly_invisibles: true,
            nametag_visibility: Visibility::Always,
            death_message_visibility: Visibility::Always,
            collision_rule: CollisionRule::Always,
            players: Vec::new(),
        }
    }

    /// The options byte of the team packet.
    pub fn pack_options(&self) -> u8 {
        u8::from(self.allow_friendly_fire) | (u8::from(self.see_friendly_invisibles) << 1)
    }

    /// `PlayerTeam.applyColor`.
    fn apply_color(&self, c: Component) -> Component {
        match self.color.and_then(named_color) {
            Some(color) => c.color(vanillite_protocol::text::Color::Named(color)),
            None => c,
        }
    }

    /// `getFormattedDisplayName`: the display name with the name as hover and insertion, in
    /// `chat.square_brackets`, in the team colour.
    pub fn formatted_display_name(&self) -> Component {
        use vanillite_protocol::text::HoverEvent;
        let inner = self
            .display_name
            .clone()
            .hover(HoverEvent::ShowText(Component::text(self.name.as_str())))
            .insertion(self.name.clone());
        self.apply_color(Component::translatable("chat.square_brackets", vec![inner]))
    }

    /// `PlayerTeam.getFormattedName`: prefix, name, suffix, in the team colour.
    pub fn format_name(&self, name: Component) -> Component {
        self.apply_color(
            Component::empty()
                .append(self.prefix.clone())
                .append(name)
                .append(self.suffix.clone()),
        )
    }
}

/// What the game needs of a holder's team (`Entity.getTeam`): its name (teams are the same
/// when their names are, `Team.isAlliedTo`) and `isAllowFriendlyFire`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamFacts {
    pub name: Box<str>,
    pub friendly_fire: bool,
}

impl Team {
    /// The facts the game reads of this team.
    pub fn facts(&self) -> TeamFacts {
        TeamFacts {
            name: self.name.as_str().into(),
            friendly_fire: self.allow_friendly_fire,
        }
    }
}

/// The named text colour of a `ChatFormatting` colour id.
pub fn named_color(id: u8) -> Option<vanillite_protocol::text::NamedColor> {
    vanillite_protocol::text::NamedColor::from_name(color_name(id))
}

/// A change of the scoreboard the server sends to players.
#[derive(Debug, Clone, PartialEq)]
pub enum ScoreEvent {
    /// `onObjectiveAdded`.
    ObjectiveAdded(String),
    /// `onObjectiveChanged`.
    ObjectiveChanged(Box<Objective>),
    /// `onObjectiveRemoved` (with its objective as it was).
    ObjectiveRemoved(Box<Objective>),
    /// `onScoreChanged`, with the score as it was then (the packet carries that).
    ScoreChanged {
        holder: String,
        objective: String,
        score: Box<Score>,
    },
    /// `onPlayerScoreRemoved`.
    ScoreRemoved { holder: String, objective: String },
    /// `onPlayerRemoved`: all scores of the holder.
    HolderRemoved(String),
    /// `setDisplayObjective`: the slot id and its new objective.
    DisplaySlot { slot: u8, objective: Option<String> },
    /// `onTeamAdded`.
    TeamAdded(Box<Team>),
    /// `onTeamChanged`.
    TeamChanged(Box<Team>),
    /// `onTeamRemoved`.
    TeamRemoved(String),
    /// `addPlayerToTeam`.
    TeamJoined { team: String, holder: String },
    /// `removePlayerFromTeam`.
    TeamLeft { team: String, holder: String },
}

/// Display slots: list, sidebar, below name and the 16 team sidebars.
pub const DISPLAY_SLOTS: usize = 19;

/// The keys of a fastutil `Object2ObjectOpenHashMap<String, _>(16, 0.5f)` as it lays them
/// out (linear probing over `HashCommon.mix(hashCode)`, growing and shrinking like fastutil),
/// so iteration (from the last slot down) gives Vanilla's order of `playerScores`.
#[derive(Debug, Clone)]
struct HolderTable {
    slots: Vec<Option<String>>,
    size: usize,
}

impl Default for HolderTable {
    fn default() -> Self {
        HolderTable {
            slots: vec![None; Self::MIN_SLOTS],
            size: 0,
        }
    }
}

impl HolderTable {
    /// `arraySize(16, 0.5f)`.
    const MIN_SLOTS: usize = 32;

    /// `HashCommon.mix(String.hashCode())`.
    fn mix(key: &str) -> u32 {
        let h = key
            .encode_utf16()
            .fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(u32::from(c)));
        let h = h.wrapping_mul(0x9E37_79B9);
        h ^ (h >> 16)
    }

    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    /// `maxFill(n, 0.5f)`.
    fn max_fill(&self) -> usize {
        (self.slots.len() / 2).min(self.slots.len() - 1)
    }

    fn find(&self, key: &str) -> Result<usize, usize> {
        let mask = self.mask();
        let mut pos = Self::mix(key) as usize & mask;
        loop {
            match &self.slots[pos] {
                None => return Err(pos),
                Some(k) if k == key => return Ok(pos),
                Some(_) => pos = (pos + 1) & mask,
            }
        }
    }

    fn insert(&mut self, key: &str) {
        let Err(pos) = self.find(key) else {
            return;
        };
        self.slots[pos] = Some(key.to_owned());
        let was = self.size;
        self.size += 1;
        if was >= self.max_fill() {
            // `arraySize(size + 1, 0.5f)`.
            self.rehash(((self.size + 1) * 2).next_power_of_two());
        }
    }

    fn remove(&mut self, key: &str) {
        let Ok(mut pos) = self.find(key) else {
            return;
        };
        self.size -= 1;
        // `shiftKeys`.
        let mask = self.mask();
        loop {
            let last = pos;
            pos = (last + 1) & mask;
            loop {
                let Some(curr) = &self.slots[pos] else {
                    self.slots[last] = None;
                    if self.slots.len() > Self::MIN_SLOTS && self.size < self.max_fill() / 4 {
                        self.rehash(self.slots.len() / 2);
                    }
                    return;
                };
                let slot = Self::mix(curr) as usize & mask;
                let stop = if last <= pos {
                    last >= slot || slot > pos
                } else {
                    last >= slot && slot > pos
                };
                if stop {
                    break;
                }
                pos = (pos + 1) & mask;
            }
            self.slots[last] = self.slots[pos].take();
        }
    }

    /// `rehash(newN)`: the old slots from the last down into the new table.
    fn rehash(&mut self, n: usize) {
        let old = std::mem::replace(&mut self.slots, vec![None; n]);
        let mask = n - 1;
        for key in old.into_iter().rev().flatten() {
            let mut pos = Self::mix(&key) as usize & mask;
            while self.slots[pos].is_some() {
                pos = (pos + 1) & mask;
            }
            self.slots[pos] = Some(key);
        }
    }

    /// The keys in iterator and `forEach` order: from the last slot down.
    fn iter(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().rev().flatten().map(String::as_str)
    }

    /// The keys in stream (spliterator) order: from the first slot up.
    fn iter_ascending(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().flatten().map(String::as_str)
    }
}

/// The scoreboard of the server.
#[derive(Debug, Default)]
pub struct Scoreboard {
    objectives: BTreeMap<String, Objective>,
    /// Objective names in creation order (`objectivesByName` iteration and the save order).
    objective_order: Vec<String>,
    scores: FxHashMap<String, BTreeMap<String, Score>>,
    /// The holders as `playerScores` (a fastutil open hash map) orders them.
    holder_order: HolderTable,
    display: [Option<String>; DISPLAY_SLOTS],
    teams: BTreeMap<String, Team>,
    team_order: Vec<String>,
    team_of: FxHashMap<String, String>,
    events: Vec<ScoreEvent>,
    dirty: bool,
}

impl Scoreboard {
    /// An empty scoreboard.
    pub fn new() -> Scoreboard {
        Scoreboard::default()
    }

    fn changed(&mut self, event: ScoreEvent) {
        self.events.push(event);
        self.dirty = true;
    }

    fn score_changed(&mut self, holder: &str, objective: &str) {
        if let Some(score) = self.score(holder, objective) {
            let score = Box::new(score.clone());
            self.changed(ScoreEvent::ScoreChanged {
                holder: holder.to_owned(),
                objective: objective.to_owned(),
                score,
            });
        }
    }

    /// Takes the changes since the last call.
    pub fn take_events(&mut self) -> Vec<ScoreEvent> {
        std::mem::take(&mut self.events)
    }

    /// Whether it changed since the last save; clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// An objective.
    pub fn objective(&self, name: &str) -> Option<&Objective> {
        self.objectives.get(name)
    }

    /// The objectives in creation order.
    pub fn objectives(&self) -> impl Iterator<Item = &Objective> {
        self.objective_order
            .iter()
            .filter_map(|n| self.objectives.get(n))
    }

    /// `addObjective`; `false` if the name is taken.
    pub fn add_objective(&mut self, objective: Objective) -> bool {
        if self.objectives.contains_key(&objective.name) {
            return false;
        }
        let name = objective.name.clone();
        self.objective_order.push(name.clone());
        self.objectives.insert(name.clone(), objective);
        self.changed(ScoreEvent::ObjectiveAdded(name));
        true
    }

    /// Changes an objective (`onObjectiveChanged`).
    pub fn modify_objective(&mut self, name: &str, f: impl FnOnce(&mut Objective)) -> bool {
        let Some(o) = self.objectives.get_mut(name) else {
            return false;
        };
        f(o);
        let o = Box::new(o.clone());
        self.changed(ScoreEvent::ObjectiveChanged(o));
        true
    }

    /// `removeObjective`: clears its display slots and scores.
    pub fn remove_objective(&mut self, name: &str) -> bool {
        let Some(objective) = self.objectives.remove(name) else {
            return false;
        };
        self.objective_order.retain(|n| n != name);
        for slot in 0..DISPLAY_SLOTS {
            if self.display.get(slot).and_then(Option::as_deref) == Some(name)
                && let Ok(slot) = u8::try_from(slot)
            {
                self.set_display(slot, None);
            }
        }
        for scores in self.scores.values_mut() {
            scores.remove(name);
        }
        self.changed(ScoreEvent::ObjectiveRemoved(Box::new(objective)));
        true
    }

    /// The objective in a display slot.
    pub fn display(&self, slot: u8) -> Option<&str> {
        self.display
            .get(usize::from(slot))
            .and_then(Option::as_deref)
    }

    /// `setDisplayObjective`.
    pub fn set_display(&mut self, slot: u8, objective: Option<&str>) {
        let Some(entry) = self.display.get_mut(usize::from(slot)) else {
            return;
        };
        let objective = objective.map(str::to_owned);
        entry.clone_from(&objective);
        self.changed(ScoreEvent::DisplaySlot { slot, objective });
    }

    /// The display slots an objective is in.
    pub fn slots_of(&self, objective: &str) -> Vec<u8> {
        (0..DISPLAY_SLOTS)
            .filter_map(|i| {
                (self.display.get(i).and_then(Option::as_deref) == Some(objective))
                    .then(|| u8::try_from(i).ok())
                    .flatten()
            })
            .collect()
    }

    /// A score, if it exists (`getPlayerScoreInfo`).
    pub fn score(&self, holder: &str, objective: &str) -> Option<&Score> {
        self.scores.get(holder)?.get(objective)
    }

    /// The scores of a holder by objective name.
    pub fn scores_of(&self, holder: &str) -> Option<&BTreeMap<String, Score>> {
        self.scores.get(holder)
    }

    /// Holders with any score (`getTrackedPlayers`: a stream of `playerScores.keySet()`).
    pub fn holders(&self) -> impl Iterator<Item = &str> {
        self.holder_order
            .iter_ascending()
            .filter(|h| self.scores.get(*h).is_some_and(|s| !s.is_empty()))
    }

    /// The scores of an objective as (holder, score) (`listPlayerScores`: `playerScores.forEach`).
    pub fn scores_in(&self, objective: &str) -> Vec<(&str, &Score)> {
        self.holder_order
            .iter()
            .filter_map(|h| Some((h, self.scores.get(h)?.get(objective)?)))
            .collect()
    }

    /// `getOrCreatePlayerScore(...).set(value)`. `display` is the holder's display name for
    /// objectives with `displayAutoUpdate`. Returns `false` when the objective is unknown.
    pub fn set_score(
        &mut self,
        holder: &str,
        objective: &str,
        value: i32,
        display: Option<&Component>,
    ) -> bool {
        let Some(auto) = self
            .objectives
            .get(objective)
            .map(|o| o.display_auto_update)
        else {
            return false;
        };
        let (score, new) = self.score_entry(holder, objective);
        let mut changed = new;
        if auto
            && let Some(d) = display
            && score.display.as_ref() != Some(d)
        {
            score.display = Some(d.clone());
            changed = true;
        }
        if score.value != value {
            score.value = value;
            changed = true;
        }
        if changed {
            self.score_changed(holder, objective);
        }
        true
    }

    /// Creates the score if missing (unsynced like Vanilla's `getOrCreate`) and returns it.
    fn score_entry(&mut self, holder: &str, objective: &str) -> (&mut Score, bool) {
        if !self.scores.contains_key(holder) {
            self.holder_order.insert(holder);
        }
        let scores = self.scores.entry(holder.to_owned()).or_default();
        let new = !scores.contains_key(objective);
        (scores.entry(objective.to_owned()).or_default(), new)
    }

    /// Ensures the score exists (`getOrCreatePlayerScore` without a change).
    pub fn touch_score(&mut self, holder: &str, objective: &str) {
        if self.objectives.contains_key(objective) {
            self.score_entry(holder, objective);
        }
    }

    /// Sets or clears the lock of a score (`lock`/`unlock`), creating it.
    pub fn set_locked(&mut self, holder: &str, objective: &str, locked: bool) {
        if !self.objectives.contains_key(objective) {
            return;
        }
        let (score, new) = self.score_entry(holder, objective);
        score.locked = locked;
        if new {
            self.score_changed(holder, objective);
        } else {
            self.dirty = true;
        }
    }

    /// Changes the display name override of a score (`display`).
    pub fn set_score_display(&mut self, holder: &str, objective: &str, display: Option<Component>) {
        if !self.objectives.contains_key(objective) {
            return;
        }
        let (score, new) = self.score_entry(holder, objective);
        if new || score.display != display {
            score.display = display;
            self.score_changed(holder, objective);
        }
    }

    /// Changes the number format override of a score.
    pub fn set_score_format(
        &mut self,
        holder: &str,
        objective: &str,
        format: Option<NumberFormat>,
    ) {
        if !self.objectives.contains_key(objective) {
            return;
        }
        let (score, _) = self.score_entry(holder, objective);
        score.number_format = format;
        self.score_changed(holder, objective);
    }

    /// `resetSinglePlayerScore`.
    pub fn reset_score(&mut self, holder: &str, objective: &str) -> bool {
        let Some(scores) = self.scores.get_mut(holder) else {
            return false;
        };
        if scores.remove(objective).is_none() {
            return false;
        }
        if scores.is_empty() {
            self.scores.remove(holder);
            self.holder_order.remove(holder);
            self.changed(ScoreEvent::HolderRemoved(holder.to_owned()));
        } else {
            self.changed(ScoreEvent::ScoreRemoved {
                holder: holder.to_owned(),
                objective: objective.to_owned(),
            });
        }
        true
    }

    /// `resetAllPlayerScores`.
    pub fn reset_all(&mut self, holder: &str) -> bool {
        if self.scores.remove(holder).is_none() {
            return false;
        }
        self.holder_order.remove(holder);
        self.changed(ScoreEvent::HolderRemoved(holder.to_owned()));
        true
    }

    /// The objectives of a criteria.
    pub fn objectives_of(&self, criteria: &CriteriaKind) -> Vec<String> {
        self.objectives()
            .filter(|o| o.criteria == *criteria)
            .map(|o| o.name.clone())
            .collect()
    }

    /// `forAllObjectives(criteria, holder, op)`: applies `f` to the score of every objective of
    /// the criteria (writable regardless of read-only).
    pub fn for_criteria(
        &mut self,
        criteria: &CriteriaKind,
        holder: &str,
        display: Option<&Component>,
        mut f: impl FnMut(i32) -> i32,
    ) {
        for objective in self.objectives_of(criteria) {
            let current = self.score(holder, &objective).map_or(0, |s| s.value);
            self.set_score(holder, &objective, f(current), display);
        }
    }

    /// A team.
    pub fn team(&self, name: &str) -> Option<&Team> {
        self.teams.get(name)
    }

    /// The teams in creation order.
    pub fn teams(&self) -> impl Iterator<Item = &Team> {
        self.team_order.iter().filter_map(|n| self.teams.get(n))
    }

    /// `addPlayerTeam`; `false` if the name is taken.
    pub fn add_team(&mut self, name: &str) -> bool {
        if self.teams.contains_key(name) {
            return false;
        }
        let team = Team::new(name);
        let added = Box::new(team.clone());
        self.teams.insert(name.to_owned(), team);
        self.team_order.push(name.to_owned());
        self.changed(ScoreEvent::TeamAdded(added));
        true
    }

    /// Changes a team (`onTeamChanged`).
    pub fn modify_team(&mut self, name: &str, f: impl FnOnce(&mut Team)) -> bool {
        let Some(t) = self.teams.get_mut(name) else {
            return false;
        };
        f(t);
        let t = Box::new(t.clone());
        self.changed(ScoreEvent::TeamChanged(t));
        true
    }

    /// `removePlayerTeam`.
    pub fn remove_team(&mut self, name: &str) -> bool {
        let Some(team) = self.teams.remove(name) else {
            return false;
        };
        self.team_order.retain(|n| n != name);
        for p in &team.players {
            self.team_of.remove(p);
        }
        self.changed(ScoreEvent::TeamRemoved(name.to_owned()));
        true
    }

    /// Whether any holder is in a team.
    pub fn has_team_members(&self) -> bool {
        !self.team_of.is_empty()
    }

    /// The team of a holder (`getPlayersTeam`).
    pub fn team_of(&self, holder: &str) -> Option<&Team> {
        self.teams.get(self.team_of.get(holder)?)
    }

    /// `addPlayerToTeam`: leaves the current team first (also when it is this team, as in
    /// Vanilla). `false` if the team is unknown.
    pub fn join_team(&mut self, holder: &str, team: &str) -> bool {
        if !self.teams.contains_key(team) {
            return false;
        }
        if self.team_of.contains_key(holder) {
            self.leave_team(holder);
        }
        self.team_of.insert(holder.to_owned(), team.to_owned());
        if let Some(t) = self.teams.get_mut(team) {
            t.players.push(holder.to_owned());
        }
        self.changed(ScoreEvent::TeamJoined {
            team: team.to_owned(),
            holder: holder.to_owned(),
        });
        true
    }

    /// `removePlayerFromTeam`; `false` if the holder is in no team.
    pub fn leave_team(&mut self, holder: &str) -> bool {
        let Some(team) = self.team_of.remove(holder) else {
            return false;
        };
        if let Some(t) = self.teams.get_mut(&team) {
            t.players.retain(|p| p != holder);
        }
        self.changed(ScoreEvent::TeamLeft {
            team,
            holder: holder.to_owned(),
        });
        true
    }
}

#[cfg(test)]
mod holder_order_tests {
    use super::HolderTable;

    #[test]
    fn iterates_like_a_fastutil_open_hash_map() {
        // The orders `Object2ObjectOpenHashMap<String, _>(16, 0.5f).keySet()` gives.
        let mut t = HolderTable::default();
        let keys = [
            "a", "b", "steve", "c", "z", "x", "y", "Alpha", "#fake", "p1", "p2", "p3", "p4", "p5",
            "p6", "p7", "p8", "p9", "q1", "q2", "q3",
        ];
        keys.iter().for_each(|k| t.insert(k));
        let order: Vec<&str> = t.iter().collect();
        assert_eq!(
            order.join(","),
            "c,p9,steve,x,p1,q3,b,q2,a,p3,z,#fake,q1,p8,p2,y,Alpha,p4,p5,p7,p6"
        );
        for k in [
            "b", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9", "q1", "q2",
        ] {
            t.remove(k);
        }
        let order: Vec<&str> = t.iter().collect();
        assert_eq!(order.join(","), "c,steve,x,q3,a,z,#fake,y,Alpha");
        let mut t = HolderTable::default();
        ["steve", "a", "b", "z", "c"]
            .iter()
            .for_each(|k| t.insert(k));
        let order: Vec<&str> = t.iter_ascending().collect();
        assert_eq!(order.join(","), "b,z,a,steve,c");
    }
}
