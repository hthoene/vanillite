//! The scoreboard on the server (Vanilla's `ServerScoreboard`): its changes as packets to all
//! players, the entire scoreboard for a joining player (`PlayerList.updateEntireScoreboard`),
//! the built-in criteria of players and their display names with the team's format.
//!
//! Changes are recorded by the [`Scoreboard`] on any world thread and flushed in order: at the
//! end of every world tick and right after team commands. An objective reaches the clients only
//! while it is in a display slot (`startTrackingObjective`/`stopTrackingObjective`); the
//! [`ScoreboardSync`] keeps the slots as the clients see them. Like Vanilla, where a player
//! joins the player list only after `updateEntireScoreboard`, changes go to the players that
//! got the entire scoreboard.

use std::collections::BTreeSet;
use std::sync::Arc;

use vanillite_game::entities::Entities;
use vanillite_game::player::PacketSink;
use vanillite_game::player::join::team_display_name;
use vanillite_game::scoreboard::{
    CriteriaKind, DISPLAY_SLOTS, Objective, Score, ScoreEvent, Scoreboard, Visibility,
};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{
    ObjectiveAction, ObjectiveParameters, ResetScore, SetDisplayObjective, SetObjective,
    SetPlayerTeam, SetScore, TeamAction,
};
use vanillite_protocol::text::Component;
use vanillite_world::entity::EntityId;

use crate::commands::teams::{add_packet, parameters};
use crate::global::Global;

/// What the clients know of the scoreboard: the tracked objectives and the display slots as
/// last sent (`ServerScoreboard.trackedObjectives`).
#[derive(Debug, Default)]
pub(crate) struct ScoreboardSync {
    tracked: BTreeSet<String>,
    display: [Option<String>; DISPLAY_SLOTS],
    /// The players that got the entire scoreboard.
    receivers: BTreeSet<u128>,
}

type Packets = Vec<Arc<SharedPacket>>;

fn objective_parameters(o: &Objective) -> Box<ObjectiveParameters> {
    Box::new(ObjectiveParameters {
        display_name: o.display_name.clone(),
        render_type: match o.render_type {
            vanillite_game::scoreboard::RenderType::Integer => 0,
            vanillite_game::scoreboard::RenderType::Hearts => 1,
        },
        number_format: o.number_format.clone(),
    })
}

fn score_packet(holder: &str, objective: &str, score: &Score) -> Arc<SharedPacket> {
    SharedPacket::of(&SetScore {
        owner: holder.to_owned(),
        objective: objective.to_owned(),
        score: score.value,
        display: score.display.clone(),
        number_format: score.number_format.clone(),
    })
}

fn display_packet(slot: usize, objective: Option<&str>) -> Arc<SharedPacket> {
    SharedPacket::of(&SetDisplayObjective {
        slot: i32::try_from(slot).unwrap_or(0),
        objective: objective.unwrap_or_default().to_owned(),
    })
}

impl ScoreboardSync {
    fn slots_of<'a>(&'a self, name: &'a str) -> impl Iterator<Item = usize> + 'a {
        self.display
            .iter()
            .enumerate()
            .filter(move |(_, o)| o.as_deref() == Some(name))
            .map(|(slot, _)| slot)
    }

    /// `getStartTrackingPackets`: the objective, its display slots and its scores.
    fn start_packets(&self, board: &Scoreboard, name: &str, out: &mut Packets) -> bool {
        let Some(objective) = board.objective(name) else {
            return false;
        };
        out.push(SharedPacket::of(&SetObjective {
            name: name.to_owned(),
            action: ObjectiveAction::Add(objective_parameters(objective)),
        }));
        for slot in self.slots_of(name) {
            out.push(display_packet(slot, Some(name)));
        }
        for (holder, score) in board.scores_in(name) {
            out.push(score_packet(holder, name, score));
        }
        true
    }

    /// `stopTrackingObjective` with `getStopTrackingPackets`.
    fn stop(&mut self, name: &str, out: &mut Packets) {
        if !self.tracked.remove(name) {
            return;
        }
        out.push(SharedPacket::of(&SetObjective {
            name: name.to_owned(),
            action: ObjectiveAction::Remove,
        }));
        for slot in self.slots_of(name) {
            out.push(display_packet(slot, Some(name)));
        }
    }

    /// `ServerScoreboard.setDisplayObjective`.
    fn set_display(
        &mut self,
        board: &Scoreboard,
        slot: u8,
        objective: Option<String>,
        out: &mut Packets,
    ) {
        let slot = usize::from(slot);
        let Some(entry) = self.display.get_mut(slot) else {
            return;
        };
        let old = std::mem::replace(entry, objective.clone());
        if let Some(old) = old
            && objective.as_ref() != Some(&old)
        {
            if self.slots_of(&old).next().is_some() {
                out.push(display_packet(slot, objective.as_deref()));
            } else {
                self.stop(&old, out);
            }
        }
        if let Some(name) = objective {
            if self.tracked.contains(&name) {
                out.push(display_packet(slot, Some(&name)));
            } else if self.start_packets(board, &name, out) {
                self.tracked.insert(name);
            }
        }
    }

    /// The packets of the recorded changes, in order (the `ServerScoreboard` callbacks).
    fn packets(&mut self, board: &Scoreboard, events: Vec<ScoreEvent>) -> Packets {
        let mut out = Vec::new();
        for event in events {
            match event {
                ScoreEvent::ObjectiveAdded(_) => {}
                ScoreEvent::ObjectiveChanged(o) => {
                    if self.tracked.contains(&o.name) {
                        out.push(SharedPacket::of(&SetObjective {
                            action: ObjectiveAction::Change(objective_parameters(&o)),
                            name: o.name.clone(),
                        }));
                    }
                }
                ScoreEvent::ObjectiveRemoved(o) => self.stop(&o.name, &mut out),
                ScoreEvent::ScoreChanged {
                    holder,
                    objective,
                    score,
                } => {
                    if self.tracked.contains(&objective) {
                        out.push(score_packet(&holder, &objective, &score));
                    }
                }
                ScoreEvent::ScoreRemoved { holder, objective } => {
                    if self.tracked.contains(&objective) {
                        out.push(SharedPacket::of(&ResetScore {
                            owner: holder,
                            objective: Some(objective),
                        }));
                    }
                }
                ScoreEvent::HolderRemoved(holder) => {
                    if !self.tracked.is_empty() {
                        out.push(SharedPacket::of(&ResetScore {
                            owner: holder,
                            objective: None,
                        }));
                    }
                }
                ScoreEvent::DisplaySlot { slot, objective } => {
                    self.set_display(board, slot, objective, &mut out);
                }
                ScoreEvent::TeamAdded(team) => out.push(SharedPacket::of(&add_packet(&team))),
                ScoreEvent::TeamChanged(team) => {
                    out.push(SharedPacket::of(&SetPlayerTeam {
                        action: TeamAction::Change(parameters(&team)),
                        name: team.name.clone(),
                    }));
                }
                ScoreEvent::TeamRemoved(name) => out.push(SharedPacket::of(&SetPlayerTeam {
                    name,
                    action: TeamAction::Remove,
                })),
                ScoreEvent::TeamJoined { team, holder } => {
                    out.push(SharedPacket::of(&SetPlayerTeam {
                        name: team,
                        action: TeamAction::Join(vec![holder]),
                    }));
                }
                ScoreEvent::TeamLeft { team, holder } => {
                    out.push(SharedPacket::of(&SetPlayerTeam {
                        name: team,
                        action: TeamAction::Leave(vec![holder]),
                    }));
                }
            }
        }
        out
    }
}

/// The scoreboard changes since the last flush to all players, each packet encoded once. The
/// sync lock keeps flushes of different worlds in order.
pub(crate) fn flush(global: &Global) {
    let mut sync = global.systems.scoreboard_sync();
    let packets = {
        let mut board = global.systems.scoreboard();
        let events = board.take_events();
        if events.is_empty() {
            return;
        }
        sync.packets(&board, events)
    };
    global
        .players
        .broadcast_all_to(&packets, |p| sync.receivers.contains(&p.0));
}

/// `PlayerList.updateEntireScoreboard`: the pending changes go out first, then the joining
/// player gets all teams and the tracked objectives with their slots and scores.
pub(crate) fn send_all(global: &Global, sink: &dyn PacketSink, uuid: u128) {
    let mut sync = global.systems.scoreboard_sync();
    let (pending, packets) = {
        let mut board = global.systems.scoreboard();
        let events = board.take_events();
        let pending = sync.packets(&board, events);
        let mut packets: Packets = board
            .teams()
            .map(|t| SharedPacket::of(&add_packet(t)))
            .collect();
        let mut sent = BTreeSet::new();
        for name in sync.display.iter().flatten() {
            if sync.tracked.contains(name) && sent.insert(name.as_str()) {
                sync.start_packets(&board, name, &mut packets);
            }
        }
        (pending, packets)
    };
    global
        .players
        .broadcast_all_to(&pending, |p| sync.receivers.contains(&p.0));
    for packet in &packets {
        sink.send_shared(packet);
    }
    sync.receivers.insert(uuid);
}

/// A player left the player list: no more scoreboard changes for it.
pub(crate) fn left(global: &Global, uuid: u128) {
    global.systems.scoreboard_sync().receivers.remove(&uuid);
}

/// `Player.getDisplayName` of an online player: its name in its team's format.
pub(crate) fn display_name(global: &Global, name: &str, uuid: u128) -> Component {
    team_display_name(global.systems.scoreboard().team_of(name), name, uuid)
}

/// `Entity.getTeam` of a score holder (a player's name, another entity's UUID string).
pub fn entity_team(global: &Global, holder: &str) -> Option<vanillite_game::scoreboard::TeamFacts> {
    let board = global.systems.scoreboard();
    if !board.has_team_members() {
        return None;
    }
    board
        .team_of(holder)
        .map(vanillite_game::scoreboard::Team::facts)
}

/// `ServerPlayer.doTick`'s `updateScoreForCriteria`: the changed built-in criteria (health,
/// food, air, armour, experience, level) of this world's players.
pub(crate) fn update_criteria(global: &Global, entities: &mut Entities) {
    let mut changed = Vec::new();
    for (id, data) in entities.players.iter_mut() {
        let criteria = data.survival.changed_criteria();
        if !criteria.is_empty() {
            changed.push((id, criteria));
        }
    }
    if changed.is_empty() {
        return;
    }
    let mut board = global.systems.scoreboard();
    for (id, criteria) in changed {
        let (Some(entity), Some(player)) = (entities.store.get(id), entities.player(id)) else {
            continue;
        };
        let name = player.profile.name.as_str();
        let display = team_display_name(board.team_of(name), name, entity.uuid());
        for (kind, value) in criteria {
            board.for_criteria(&kind, name, Some(&display), |_| value);
        }
    }
}

/// The score holder name of an entity: the player name or the UUID.
fn holder_name(entities: &Entities, id: EntityId) -> Option<(String, u128, bool)> {
    let e = entities.store.get(id)?;
    let uuid = e.uuid();
    match entities.player(id) {
        Some(p) => Some((p.profile.name.clone(), uuid, true)),
        None => Some((
            vanillite_protocol::types::Uuid(uuid).to_string(),
            uuid,
            false,
        )),
    }
}

/// A custom statistic by its name (`minecraft:mob_kills`).
fn custom_stat(name: &str) -> Option<vanillite_game::stats::Stat> {
    vanillite_game::stats::Stat::by_criteria_name(&format!(
        "minecraft.custom:{}",
        name.replace(':', ".")
    ))
}

/// `LivingEntity.die` of a player or mob: the player's death message by the team's
/// `deathMessageVisibility` and death count, and the kill credit's `awardKillScore`
/// (`ServerPlayer.awardKillScore`: kill counts, kill statistics, `handleTeamKill`).
pub(crate) fn died(
    global: &Global,
    entities: &mut Entities,
    victim: EntityId,
    killer: Option<EntityId>,
    killer_kind: Option<vanillite_data::entity::EntityTypeId>,
    message: Option<&Component>,
) {
    let Some((name, uuid, victim_player)) = holder_name(entities, victim) else {
        return;
    };
    let victim_kind = entities.store.get(victim).map(|e| e.kind());
    let team = global.systems.scoreboard().team_of(&name).cloned();
    if victim_player && let Some(message) = message {
        // `broadcastSystemMessage` logs the message (`MinecraftServer.sendSystemMessage`).
        let broadcast = || {
            log::info!("System chat: {}", global.english(message));
            global
                .players
                .broadcast_system(message.clone(), false, None);
        };
        match &team {
            None => broadcast(),
            Some(t) => match t.death_message_visibility {
                Visibility::Always => broadcast(),
                Visibility::Never => {}
                // `broadcastSystemToTeam`: the other members.
                Visibility::HideForOtherTeams => {
                    global.players.broadcast_system_to(message.clone(), |p| {
                        p != name && t.players.iter().any(|m| m == p)
                    })
                }
                // `broadcastSystemToAllExceptTeam`.
                Visibility::HideForOwnTeam => global
                    .players
                    .broadcast_system_to(message.clone(), |p| !t.players.iter().any(|m| m == p)),
            },
        }
    }
    let increment = |v: i32| v.wrapping_add(1);
    let display = team_display_name(team.as_ref(), &name, uuid);
    if victim_player {
        global.systems.scoreboard().for_criteria(
            &CriteriaKind::DeathCount,
            &name,
            Some(&display),
            increment,
        );
    }
    let killer = killer.filter(|k| *k != victim);
    // `Player.die`: `ENTITY_KILLED_BY` of the killer's type.
    if victim_player
        && let Some(kind) =
            killer.and_then(|k| killer_kind.or_else(|| entities.store.get(k).map(|e| e.kind())))
        && let Some(p) = entities.player_mut(victim)
    {
        p.stats.award(
            vanillite_game::stats::Stat::of(vanillite_game::stats::StatType::KilledBy, kind.0),
            1,
        );
    }
    let Some((killer, killer_uuid, true)) = killer.and_then(|k| holder_name(entities, k)) else {
        return;
    };
    let killer_id = entities.players.iter().map(|(id, _)| id).find(|&id| {
        entities
            .store
            .get(id)
            .is_some_and(|e| e.uuid() == killer_uuid)
    });
    if let Some(p) = killer_id.and_then(|id| entities.player_mut(id)) {
        if let Some(kind) = victim_kind {
            p.stats.award(
                vanillite_game::stats::Stat::of(vanillite_game::stats::StatType::Killed, kind.0),
                1,
            );
        }
        let counter = if victim_player {
            "player_kills"
        } else {
            "mob_kills"
        };
        if let Some(stat) = custom_stat(counter) {
            p.stats.award(stat, 1);
        }
    }
    let mut board = global.systems.scoreboard();
    let killer_team = board.team_of(&killer).cloned();
    let killer_display = team_display_name(killer_team.as_ref(), &killer, killer_uuid);
    board.for_criteria(
        &CriteriaKind::TotalKillCount,
        &killer,
        Some(&killer_display),
        increment,
    );
    if victim_player {
        board.for_criteria(
            &CriteriaKind::PlayerKillCount,
            &killer,
            Some(&killer_display),
            increment,
        );
    }
    if let Some(color) = team.as_ref().and_then(|t| t.color) {
        let kind = CriteriaKind::TeamKill(color);
        board.for_criteria(&kind, &killer, Some(&killer_display), increment);
    }
    if let Some(color) = killer_team.and_then(|t| t.color) {
        let kind = CriteriaKind::KilledByTeam(color);
        board.for_criteria(&kind, &name, Some(&display), increment);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use vanillite_game::scoreboard::RenderType;

    fn objective(name: &str) -> Objective {
        Objective {
            name: name.into(),
            criteria: CriteriaKind::Dummy,
            display_name: Component::text(name),
            render_type: RenderType::Integer,
            display_auto_update: false,
            number_format: None,
        }
    }

    fn ids(packets: &Packets) -> Vec<i32> {
        packets
            .iter()
            .map(|p| {
                vanillite_protocol::codec::Reader::new(p.body())
                    .varint()
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn objectives_are_sent_only_while_displayed() {
        use vanillite_protocol::Packet;
        let (add, score, display, reset) = (
            SetObjective::ID,
            SetScore::ID,
            SetDisplayObjective::ID,
            ResetScore::ID,
        );
        let mut board = Scoreboard::new();
        let mut sync = ScoreboardSync::default();
        board.add_objective(objective("a"));
        board.set_score("x", "a", 1, None);
        let events = board.take_events();
        assert!(sync.packets(&board, events).is_empty());
        board.set_display(1, Some("a"));
        let events = board.take_events();
        let p = sync.packets(&board, events);
        assert_eq!(ids(&p), [add, display, score]);
        board.set_display(0, Some("a"));
        board.set_score("x", "a", 2, None);
        board.reset_all("x");
        let events = board.take_events();
        let p = sync.packets(&board, events);
        // Each change goes out as it was (`onScoreChanged` sends at once): the score, then
        // its reset.
        assert_eq!(ids(&p), [display, score, reset]);
        board.remove_objective("a");
        let events = board.take_events();
        let p = sync.packets(&board, events);
        // The first slot is cleared while the objective is still shown in the other.
        assert_eq!(ids(&p), [display, add]);
        assert!(sync.tracked.is_empty());
    }
}
