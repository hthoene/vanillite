//! `ScoreboardSaveData`: the scoreboard as the saved data `minecraft:scoreboard`
//! (`ScoreboardSaveData.Packed.CODEC`). Fields at their default are left out, like Vanilla's
//! `optionalFieldOf` with a default.

use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::text::{Component, Style};

use super::{
    COLOR_NAMES, CollisionRule, CriteriaKind, DISPLAY_SLOTS, NumberFormat, Objective, RenderType,
    Score, Scoreboard, Visibility, color_id,
};

/// `DisplaySlot.getSerializedName` of a slot id.
pub fn slot_name(slot: u8) -> Option<String> {
    match slot {
        0 => Some("list".to_owned()),
        1 => Some("sidebar".to_owned()),
        2 => Some("below_name".to_owned()),
        _ => COLOR_NAMES
            .get(usize::from(slot.checked_sub(3)?))
            .map(|c| format!("sidebar.team.{c}")),
    }
}

/// The slot id of a serialized `DisplaySlot` name.
pub fn slot_by_name(name: &str) -> Option<u8> {
    match name {
        "list" => Some(0),
        "sidebar" => Some(1),
        "below_name" => Some(2),
        _ => color_id(name.strip_prefix("sidebar.team.")?)?.checked_add(3),
    }
}

/// `NumberFormatTypes.CODEC`: the type and its fields.
fn format_to_nbt(format: &NumberFormat) -> Tag {
    let (mut c, kind) = match format {
        NumberFormat::Blank => (Compound::new(), "minecraft:blank"),
        NumberFormat::Styled(style) => match style.to_nbt() {
            Tag::Compound(c) => (c, "minecraft:styled"),
            _ => (Compound::new(), "minecraft:styled"),
        },
        NumberFormat::Fixed(value) => (
            Compound::new().with("value", value.to_nbt()),
            "minecraft:fixed",
        ),
    };
    c.insert("type", kind);
    Tag::Compound(c.in_java_hash_order())
}

/// Reads a `NumberFormatTypes.CODEC` value.
fn format_from_nbt(tag: &Tag) -> Option<NumberFormat> {
    let c = tag.as_compound()?;
    let kind = c.get("type")?.as_str()?;
    Some(match kind.strip_prefix("minecraft:").unwrap_or(kind) {
        "blank" => NumberFormat::Blank,
        "styled" => NumberFormat::Styled(Box::new(Style::try_from_nbt(tag).ok()?)),
        "fixed" => NumberFormat::Fixed(Box::new(Component::try_from_nbt(c.get("value")?).ok()?)),
        _ => return None,
    })
}

fn flag(c: &Compound, key: &str, default: bool) -> bool {
    c.get(key).and_then(Tag::as_i64).map_or(default, |v| v != 0)
}

fn compounds<'a>(data: &'a Compound, key: &str) -> impl Iterator<Item = &'a Compound> {
    let list = match data.get(key) {
        Some(Tag::List(List(v))) => v.as_slice(),
        _ => &[],
    };
    list.iter().filter_map(Tag::as_compound)
}

fn component(c: &Compound, key: &str) -> Option<Component> {
    Component::try_from_nbt(c.get(key)?).ok()
}

/// `Objective.Packed.CODEC`.
fn pack_objective(o: &Objective) -> Compound {
    let mut c = Compound::new().with("Name", o.name.as_str());
    if o.criteria != CriteriaKind::Dummy {
        c.insert("CriteriaName", o.criteria.name());
    }
    c.insert("DisplayName", o.display_name.to_nbt());
    if o.render_type != RenderType::Integer {
        c.insert("RenderType", o.render_type.name());
    }
    if o.display_auto_update {
        c.insert("display_auto_update", true);
    }
    if let Some(format) = &o.number_format {
        c.insert("format", format_to_nbt(format));
    }
    c.in_java_hash_order()
}

fn unpack_objective(c: &Compound) -> Option<Objective> {
    let name = c.get("Name")?.as_str()?.to_owned();
    let criteria = match c.get("CriteriaName") {
        Some(tag) => CriteriaKind::by_name(tag.as_str()?)?,
        None => CriteriaKind::Dummy,
    };
    let render_type = match c.get("RenderType") {
        Some(tag) => RenderType::by_name(tag.as_str()?)?,
        None => RenderType::Integer,
    };
    Some(Objective {
        display_name: component(c, "DisplayName")?,
        name,
        criteria,
        render_type,
        display_auto_update: flag(c, "display_auto_update", false),
        number_format: c.get("format").and_then(format_from_nbt),
    })
}

/// `Score.Packed.MAP_CODEC`.
fn pack_score(score: &Score, c: &mut Compound) {
    if score.value != 0 {
        c.insert("Score", score.value);
    }
    if score.locked {
        c.insert("Locked", true);
    }
    if let Some(display) = &score.display {
        c.insert("display", display.to_nbt());
    }
    if let Some(format) = &score.number_format {
        c.insert("format", format_to_nbt(format));
    }
}

fn unpack_score(c: &Compound) -> Score {
    Score {
        value: c
            .get("Score")
            .and_then(Tag::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0),
        locked: flag(c, "Locked", false),
        display: component(c, "display"),
        number_format: c.get("format").and_then(format_from_nbt),
    }
}

impl Scoreboard {
    /// `ServerScoreboard.store` written with `ScoreboardSaveData.Packed.CODEC`.
    pub fn to_nbt(&self) -> Compound {
        let mut out = Compound::new();
        let objectives: Vec<Tag> = self
            .objectives()
            .map(|o| Tag::Compound(pack_objective(o)))
            .collect();
        if !objectives.is_empty() {
            out.insert("Objectives", List(objectives));
        }
        let mut scores = Vec::new();
        for holder in self.holder_order.iter_ascending() {
            for (objective, score) in self.scores.get(holder).into_iter().flatten() {
                let mut c = Compound::new()
                    .with("Name", holder)
                    .with("Objective", objective.as_str());
                pack_score(score, &mut c);
                scores.push(Tag::Compound(c.in_java_hash_order()));
            }
        }
        if !scores.is_empty() {
            out.insert("PlayerScores", List(scores));
        }
        let mut slots = Compound::new();
        for (slot, objective) in (0u8..).zip(&self.display) {
            if let (Some(objective), Some(name)) = (objective, slot_name(slot)) {
                slots.insert(name, objective.as_str());
            }
        }
        if !slots.is_empty() {
            out.insert("DisplaySlots", slots.in_java_hash_order());
        }
        let teams: Vec<Tag> = self
            .teams()
            .map(|t| {
                let mut c = Compound::new()
                    .with("Name", t.name.as_str())
                    .with("DisplayName", t.display_name.to_nbt());
                if let Some(color) = t.color.and_then(|c| COLOR_NAMES.get(usize::from(c))) {
                    c.insert("TeamColor", *color);
                }
                if !t.allow_friendly_fire {
                    c.insert("AllowFriendlyFire", false);
                }
                if !t.see_friendly_invisibles {
                    c.insert("SeeFriendlyInvisibles", false);
                }
                if t.prefix != Component::empty() {
                    c.insert("MemberNamePrefix", t.prefix.to_nbt());
                }
                if t.suffix != Component::empty() {
                    c.insert("MemberNameSuffix", t.suffix.to_nbt());
                }
                if t.nametag_visibility != Visibility::Always {
                    c.insert("NameTagVisibility", t.nametag_visibility.name());
                }
                if t.death_message_visibility != Visibility::Always {
                    c.insert("DeathMessageVisibility", t.death_message_visibility.name());
                }
                if t.collision_rule != CollisionRule::Always {
                    c.insert("CollisionRule", t.collision_rule.name());
                }
                if !t.players.is_empty() {
                    let players = t.players.iter().map(|p| Tag::from(p.as_str())).collect();
                    c.insert("Players", List(players));
                }
                Tag::Compound(c.in_java_hash_order())
            })
            .collect();
        if !teams.is_empty() {
            out.insert("Teams", List(teams));
        }
        out.in_java_hash_order()
    }

    /// `ServerScoreboard.load`: objectives, scores, display slots, then teams. Invalid entries
    /// and scores of unknown objectives are skipped with an error, as in Vanilla. The changes
    /// stay recorded so the display slots start tracking; the board is not dirty.
    pub fn from_nbt(data: &Compound) -> Scoreboard {
        let mut board = Scoreboard::new();
        for c in compounds(data, "Objectives") {
            match unpack_objective(c) {
                Some(o) => {
                    board.add_objective(o);
                }
                None => log::error!("Failed to parse objective {c:?}"),
            }
        }
        for c in compounds(data, "PlayerScores") {
            let (Some(holder), Some(objective)) = (
                c.get("Name").and_then(Tag::as_str),
                c.get("Objective").and_then(Tag::as_str),
            ) else {
                continue;
            };
            if board.objective(objective).is_none() {
                log::error!("Unknown objective {objective} for name {holder}, ignoring");
                continue;
            }
            *board.score_entry(holder, objective).0 = unpack_score(c);
        }
        if let Some(Tag::Compound(slots)) = data.get("DisplaySlots") {
            for (name, objective) in slots.iter() {
                let (Some(slot), Some(objective)) = (slot_by_name(name), objective.as_str()) else {
                    continue;
                };
                if usize::from(slot) < DISPLAY_SLOTS && board.objective(objective).is_some() {
                    board.set_display(slot, Some(objective));
                }
            }
        }
        for c in compounds(data, "Teams") {
            let Some(name) = c.get("Name").and_then(Tag::as_str) else {
                continue;
            };
            board.add_team(name);
            let visibility = |key: &str| {
                c.get(key)
                    .and_then(Tag::as_str)
                    .and_then(Visibility::by_name)
                    .unwrap_or_default()
            };
            board.modify_team(name, |t| {
                if let Some(display) = component(c, "DisplayName") {
                    t.display_name = display;
                }
                t.color = c.get("TeamColor").and_then(Tag::as_str).and_then(color_id);
                t.allow_friendly_fire = flag(c, "AllowFriendlyFire", true);
                t.see_friendly_invisibles = flag(c, "SeeFriendlyInvisibles", true);
                t.prefix = component(c, "MemberNamePrefix").unwrap_or_else(Component::empty);
                t.suffix = component(c, "MemberNameSuffix").unwrap_or_else(Component::empty);
                t.nametag_visibility = visibility("NameTagVisibility");
                t.death_message_visibility = visibility("DeathMessageVisibility");
                t.collision_rule = c
                    .get("CollisionRule")
                    .and_then(Tag::as_str)
                    .and_then(CollisionRule::by_name)
                    .unwrap_or_default();
            });
            let players = match c.get("Players") {
                Some(Tag::List(List(v))) => v.as_slice(),
                _ => &[],
            };
            for player in players.iter().filter_map(Tag::as_str) {
                board.join_team(player, name);
            }
        }
        board.dirty = false;
        board
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::scoreboard::Team;

    #[test]
    fn scoreboard_round_trips_through_nbt() {
        let mut board = Scoreboard::new();
        board.add_objective(Objective {
            name: "kills".into(),
            criteria: CriteriaKind::TeamKill(12),
            display_name: Component::text("Kills"),
            render_type: RenderType::Hearts,
            display_auto_update: true,
            number_format: Some(NumberFormat::Fixed(Box::new(Component::text("x")))),
        });
        board.add_objective(Objective {
            name: "plain".into(),
            criteria: CriteriaKind::Dummy,
            display_name: Component::text("plain"),
            render_type: RenderType::Integer,
            display_auto_update: false,
            number_format: None,
        });
        board.set_score("Alice", "kills", 7, None);
        board.set_score_display("Alice", "kills", Some(Component::text("A")));
        board.set_score_format("Bob", "plain", Some(NumberFormat::Blank));
        board.set_locked("Bob", "plain", false);
        board.set_display(1, Some("kills"));
        board.set_display(17, Some("plain"));
        board.add_team("red");
        board.modify_team("red", |t: &mut Team| {
            t.color = Some(12);
            t.prefix = Component::text("[R] ");
            t.allow_friendly_fire = false;
            t.death_message_visibility = Visibility::HideForOtherTeams;
            t.collision_rule = CollisionRule::PushOwnTeam;
        });
        board.join_team("Alice", "red");
        let nbt = board.to_nbt();
        assert_eq!(
            nbt.get("DisplaySlots")
                .and_then(Tag::as_compound)
                .and_then(|c| c.get("sidebar.team.yellow"))
                .and_then(Tag::as_str),
            Some("plain")
        );
        let loaded = Scoreboard::from_nbt(&nbt);
        assert_eq!(loaded.to_nbt(), nbt);
        assert_eq!(loaded.objective("kills"), board.objective("kills"));
        assert_eq!(
            loaded.score("Alice", "kills"),
            board.score("Alice", "kills")
        );
        assert_eq!(loaded.score("Bob", "plain"), board.score("Bob", "plain"));
        assert_eq!(loaded.team("red"), board.team("red"));
        assert_eq!(
            loaded.team_of("Alice").map(|t| t.name.as_str()),
            Some("red")
        );
        assert_eq!(loaded.display(1), Some("kills"));
        assert_eq!(slot_by_name("sidebar.team.white"), Some(18));
        assert_eq!(slot_name(18).as_deref(), Some("sidebar.team.white"));
    }
}
