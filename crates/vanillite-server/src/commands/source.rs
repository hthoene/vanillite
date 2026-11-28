//! The source of a command (Vanilla's `CommandSourceStack`): who runs it, where, with which
//! permission level, and where its feedback goes.

use std::sync::Arc;

use vanillite_base::Vec3;
use vanillite_game::player::PacketSink;
use vanillite_game::player::join::display_name;
use vanillite_game::survival::rules::Rule;
use vanillite_protocol::packets::play::clientbound::SystemChat;
use vanillite_protocol::text::{Color, Component, NamedColor};

use vanillite_command::values::Anchor;
use vanillite_data::entity::EntityTypeId;
use vanillite_world::entity::EntityId;

use super::engine::Callback;
use crate::global::Global;
use crate::player_list::Uuid;
use crate::world::WorldId;

/// Who gets the feedback of a command (Vanilla's `CommandSource`).
#[derive(Clone)]
pub(crate) enum SourceKind {
    /// A player.
    Player {
        uuid: u128,
        name: String,
        sink: Arc<dyn PacketSink>,
    },
    /// The server console.
    Console,
    /// A remote console (`RconConsoleSource`): the feedback is collected as the answer.
    Rcon(Arc<std::sync::Mutex<String>>),
    /// The server itself (functions of the game loop, scheduled functions): no feedback.
    Server,
    /// A command block or command block minecart: `CloseableCommandBlockSource` while it
    /// tracks its output, else `CommandSource.NULL`.
    CommandBlock(Arc<BlockOutput>),
}

/// What a command block source keeps of its run: its name and the last message.
pub(crate) struct BlockOutput {
    /// `trackOutput`.
    track: bool,
    /// The block's name (`getName`): the source's display name.
    name: Component,
    /// Its plain text (`getTextName`).
    text: String,
    last: std::sync::Mutex<Option<Component>>,
}

impl BlockOutput {
    pub(crate) fn new(track: bool, name: Component) -> Self {
        Self {
            track,
            text: name.plain(),
            name,
            last: std::sync::Mutex::new(None),
        }
    }

    /// The last message the source took.
    pub(crate) fn take(&self) -> Option<Component> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// The entity a command runs as (`CommandSourceStack.entity`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SourceEntity {
    /// UUID.
    pub(crate) uuid: u128,
    /// The world the entity is in.
    pub(crate) world: WorldId,
    /// The entity, when it is in the world the command runs on.
    pub(crate) id: Option<EntityId>,
    /// Its type.
    pub(crate) kind: EntityTypeId,
    /// `getScoreboardName`/`getName`: the player name, else the custom name or the type name.
    pub(crate) name: String,
    /// Its display name.
    pub(crate) display: Component,
    /// Eye height above the feet.
    pub(crate) eye_height: f64,
}

impl SourceEntity {
    /// Whether it is a player.
    pub(crate) fn is_player(&self) -> bool {
        self.kind == vanillite_data::entity::ids::PLAYER
    }
}

/// A command source (`CommandSourceStack`).
#[derive(Clone)]
pub(crate) struct CommandSource {
    /// The server, for suggestions.
    pub(crate) global: Arc<Global>,
    pub(crate) kind: SourceKind,
    /// Permission level 0..=4.
    pub(crate) level: u8,
    /// The world the command runs in.
    pub(crate) world: WorldId,
    /// Position.
    pub(crate) pos: Vec3,
    /// Rotation (`y_rot`, `x_rot`).
    pub(crate) rot: (f32, f32),
    /// The entity it runs as.
    pub(crate) entity: Option<SourceEntity>,
    /// `anchored`.
    pub(crate) anchor: Anchor,
    /// No feedback at all (`withSuppressedOutput`).
    pub(crate) silent: bool,
    /// Gets the result of the command (`execute store`, function results).
    pub(crate) callback: Callback,
    /// Console sources of parity scenarios: the messages as `msg <network JSON>` and (by
    /// [`Callback::Record`]) the results, collected instead of logged.
    pub(crate) feedback: Option<Arc<std::sync::Mutex<Vec<String>>>>,
}

impl vanillite_command::Source for CommandSource {
    fn permission_level(&self) -> u8 {
        self.level
    }

    fn online_player_names(&self) -> Vec<String> {
        self.global
            .players
            .snapshot()
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    fn has_permission_node(&self, node: &str) -> bool {
        match &self.kind {
            SourceKind::Player { uuid, .. } => {
                let operator = self.global.access.op_level(*uuid).is_some();
                self.global.plugins.has_permission(*uuid, node, operator)
            }
            SourceKind::Console
            | SourceKind::Rcon(_)
            | SourceKind::Server
            | SourceKind::CommandBlock(_) => true,
        }
    }

    fn permission_node_answer(&self, node: &str) -> Option<bool> {
        match &self.kind {
            SourceKind::Player { uuid, .. } => {
                let operator = self.global.access.op_level(*uuid).is_some();
                self.global.plugins.permission_answer(*uuid, node, operator)
            }
            SourceKind::Console
            | SourceKind::Rcon(_)
            | SourceKind::Server
            | SourceKind::CommandBlock(_) => None,
        }
    }

    fn suggest_custom(
        &self,
        provider: &str,
        args: &vanillite_command::Args,
        builder: &mut vanillite_command::SuggestionsBuilder<'_>,
    ) {
        use vanillite_game::commands::suggest;
        if provider.starts_with(vanillite_plugin_host::SUGGEST_PREFIX) {
            let input = builder.input().to_owned();
            let args = super::dispatch::plugin_args(None, self, args, &input).unwrap_or_default();
            let list = self.global.plugins.suggest(
                provider,
                &self.plugin_sender(),
                &args,
                builder.remaining(),
            );
            builder.suggest_matching(list.iter().map(String::as_str));
            return;
        }
        let access = &self.global.access;
        let online = self.global.players.snapshot();
        let names: Vec<String> = match provider {
            suggest::OP_CANDIDATES => online
                .into_iter()
                .filter(|p| access.op_level(p.uuid.0).is_none())
                .map(|p| p.name)
                .collect(),
            suggest::OPS => access.ops().into_iter().map(|o| o.name).collect(),
            suggest::WHITELIST_CANDIDATES => {
                let listed = access.whitelisted_names();
                online
                    .into_iter()
                    .filter(|p| !listed.iter().any(|n| n.eq_ignore_ascii_case(&p.name)))
                    .map(|p| p.name)
                    .collect()
            }
            suggest::WHITELISTED => access.whitelisted_names(),
            suggest::PLUGINS => super::plugins_cmd::names(&self.global),
            suggest::SCORE_HOLDERS => {
                let mut names: Vec<String> = online.into_iter().map(|p| p.name).collect();
                names.extend(
                    self.global
                        .systems
                        .scoreboard()
                        .holders()
                        .filter(|h| !names.iter().any(|n| n == h))
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                );
                names
            }
            suggest::TRIGGER_OBJECTIVES => self
                .global
                .systems
                .scoreboard()
                .objectives()
                .filter(|o| o.criteria == vanillite_game::scoreboard::CriteriaKind::Trigger)
                .map(|o| o.name.clone())
                .collect(),
            suggest::FUNCTIONS => {
                let library = self.global.systems.functions();
                let tags: Vec<String> = library.tag_names().map(|t| format!("#{t}")).collect();
                let names: Vec<&str> = library.names().collect();
                builder.suggest_resource_ids(names.iter().copied());
                builder.suggest_matching(tags.iter().map(String::as_str));
                return;
            }
            suggest::STORAGE => {
                let ids: Vec<String> = self.global.systems.storage().keys().cloned().collect();
                builder.suggest_resource_ids(ids.iter().map(String::as_str));
                return;
            }
            suggest::TRIGGERS => {
                let Some(e) = &self.entity else {
                    return;
                };
                let holder = if e.is_player() {
                    e.name.clone()
                } else {
                    crate::player_list::Uuid(e.uuid).to_string()
                };
                let board = self.global.systems.scoreboard();
                board
                    .objectives()
                    .filter(|o| o.criteria == vanillite_game::scoreboard::CriteriaKind::Trigger)
                    .filter(|o| board.score(&holder, &o.name).is_some_and(|s| !s.locked))
                    .map(|o| o.name.clone())
                    .collect()
            }
            suggest::RANDOM_SEQUENCES => self
                .global
                .systems
                .random_sequences()
                .ids()
                .map(str::to_owned)
                .collect(),
            suggest::BANNED_PLAYERS => access.bans().into_iter().map(|b| b.name).collect(),
            suggest::BANNED_IPS => access.ip_bans().into_iter().map(|b| b.ip).collect(),
            suggest::SCHEDULED | suggest::STOPWATCHES => {
                let ids: Vec<String> = if provider == suggest::SCHEDULED {
                    let queue = self.global.systems.scheduled();
                    queue
                        .ids()
                        .filter(|id| !id.starts_with('#'))
                        .map(str::to_owned)
                        .collect()
                } else {
                    let stopwatches = self.global.systems.stopwatches();
                    stopwatches.ids().map(str::to_owned).collect()
                };
                builder.suggest_resource_ids(ids.iter().map(String::as_str));
                return;
            }
            suggest::TEST_INSTANCES => {
                builder.suggest_resource_ids(
                    self.registry_entries("minecraft:test_instance")
                        .iter()
                        .map(String::as_str),
                );
                return;
            }
            suggest::SELECTED_PACKS | suggest::UNSELECTED_PACKS => {
                let packs = &self.global.packs;
                let repo = packs.repository();
                if provider == suggest::SELECTED_PACKS {
                    repo.selected()
                        .iter()
                        .map(|id| super::pack_cmd::escape_if_required(id))
                        .collect()
                } else {
                    repo.available()
                        .filter(|p| {
                            vanillite_datapack::repository::features_allowed(
                                &p.features,
                                &packs.features,
                            ) && !repo.is_selected(&p.id)
                        })
                        .map(|p| super::pack_cmd::escape_if_required(&p.id))
                        .collect()
                }
            }
            suggest::ADVANCEMENT_CRITERIA => {
                let set = self.global.systems.advancements();
                args.identifier("advancement")
                    .and_then(|id| set.get(set.index(id)?))
                    .map(|a| a.criteria.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default()
            }
            suggest::TICK_RATE => vec!["20".to_owned()],
            suggest::TICK_STEP => vec!["1t".to_owned(), "1s".to_owned()],
            suggest::TICK_SPRINT => vec!["60s".to_owned(), "1d".to_owned(), "3d".to_owned()],
            suggest::TIMELINES => {
                builder.suggest_resource_ids(
                    self.registry_entries("minecraft:timeline")
                        .iter()
                        .map(String::as_str),
                );
                return;
            }
            _ => Vec::new(),
        };
        builder.suggest_matching(names.iter().map(String::as_str));
    }

    fn resource_exists(&self, registry: &str, id: &str) -> bool {
        // The reloadable registries of the data packs.
        match registry {
            "minecraft:loot_table" => return self.global.game().loot.contains(id),
            "minecraft:predicate" => return self.global.game().loot.has_predicate(id),
            "minecraft:item_modifier" => return self.global.game().loot.has_modifier(id),
            // Data packs define none in Vanilla, and named slot sources are not loaded.
            "minecraft:slot_source" => return false,
            _ => {}
        }
        match self.global.registries.get(registry) {
            Some(entries) => entries.iter().any(|e| e == id),
            None => vanillite_data::registry::Registry::by_name(registry)
                .is_none_or(|r| r.id(id).is_some()),
        }
    }

    fn registry_tags(&self, registry: &str) -> Vec<String> {
        if registry == "minecraft:function" {
            return self
                .global
                .systems
                .functions()
                .tag_names()
                .map(str::to_owned)
                .collect();
        }
        self.global
            .systems
            .tags()
            .get(registry)
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn tag_entries(&self, registry: &str, tag: &str) -> Vec<String> {
        self.global
            .systems
            .tags()
            .get(registry)
            .and_then(|t| t.get(tag))
            .cloned()
            .unwrap_or_default()
    }

    fn objective_names(&self) -> Vec<String> {
        self.global
            .systems
            .scoreboard()
            .objectives()
            .map(|o| o.name.clone())
            .collect()
    }

    fn team_names(&self) -> Vec<String> {
        self.global
            .systems
            .scoreboard()
            .teams()
            .map(|t| t.name.clone())
            .collect()
    }

    fn registry_entries(&self, registry: &str) -> Vec<String> {
        if registry == "minecraft:advancement" {
            let set = self.global.systems.advancements();
            return set.iter().map(|(_, a)| a.id.clone()).collect();
        }
        match self.global.registries.get(registry) {
            Some(entries) => entries.clone(),
            None => vanillite_data::registry::Registry::by_name(registry)
                .map(|r| r.entries.iter().map(|e| (*e).to_owned()).collect())
                .unwrap_or_default(),
        }
    }
}

impl CommandSource {
    /// The source as a plugin command sender.
    pub(crate) fn plugin_sender(&self) -> vanillite_plugin_host::Sender {
        let kind = match (&self.kind, &self.entity) {
            (SourceKind::CommandBlock(_), _) => vanillite_abi::sender::COMMAND_BLOCK,
            (_, Some(e)) if !e.is_player() => vanillite_abi::sender::ENTITY,
            _ => vanillite_abi::sender::CONSOLE,
        };
        vanillite_plugin_host::Sender {
            kind,
            player: self.uuid().or_else(|| {
                (kind == vanillite_abi::sender::ENTITY)
                    .then(|| self.entity.as_ref().map(|e| e.uuid))
                    .flatten()
            }),
            world: Some(u32::from(self.world.0)),
            pos: [self.pos.x, self.pos.y, self.pos.z],
        }
    }

    /// A source with the defaults of `CommandSourceStack`'s constructor.
    pub(crate) fn new(
        global: Arc<Global>,
        kind: SourceKind,
        level: u8,
        world: WorldId,
        pos: Vec3,
        rot: (f32, f32),
        entity: Option<SourceEntity>,
    ) -> CommandSource {
        CommandSource {
            global,
            kind,
            level,
            world,
            pos,
            rot,
            entity,
            anchor: Anchor::Feet,
            silent: false,
            callback: Callback::Empty,
            feedback: None,
        }
    }

    /// `EntityAnchorArgument.Anchor.apply(source)`: the position, or the entity's eyes.
    pub(crate) fn anchor_pos(&self) -> Vec3 {
        match (self.anchor, &self.entity) {
            (Anchor::Eyes, Some(e)) => self.pos + Vec3::new(0.0, e.eye_height, 0.0),
            _ => self.pos,
        }
    }

    /// The player's UUID, if the source runs as a player (`getPlayer`).
    pub(crate) fn uuid(&self) -> Option<u128> {
        self.entity
            .as_ref()
            .filter(|e| e.is_player())
            .map(|e| e.uuid)
    }

    /// The UUID of the player that gets the feedback.
    pub(crate) fn receiver_uuid(&self) -> Option<u128> {
        match &self.kind {
            SourceKind::Player { uuid, .. } => Some(*uuid),
            SourceKind::Console
            | SourceKind::Rcon(_)
            | SourceKind::Server
            | SourceKind::CommandBlock(_) => None,
        }
    }

    /// `getDisplayName`: the entity's display name, the player's, or `Server`.
    pub(crate) fn display_name(&self) -> Component {
        if let Some(e) = &self.entity {
            return e.display.clone();
        }
        match &self.kind {
            SourceKind::Player { uuid, name, .. } => display_name(name, *uuid),
            SourceKind::Console | SourceKind::Server => Component::text("Server"),
            SourceKind::Rcon(_) => Component::text("Rcon"),
            SourceKind::CommandBlock(out) => out.name.clone(),
        }
    }

    /// `getTextName`.
    pub(crate) fn text_name(&self) -> &str {
        if let Some(e) = &self.entity {
            return &e.name;
        }
        match &self.kind {
            SourceKind::Player { name, .. } => name,
            SourceKind::Console | SourceKind::Server => "Server",
            SourceKind::Rcon(_) => "Rcon",
            SourceKind::CommandBlock(out) => &out.text,
        }
    }

    /// `withCallback`.
    pub(crate) fn with_callback(&self, callback: Callback) -> CommandSource {
        CommandSource {
            callback,
            ..self.clone()
        }
    }

    /// `sendSystemMessage`: to the player (if it accepts system messages) or the log.
    pub(crate) fn send_system(&self, global: &Global, message: Component) {
        if self.silent {
            return;
        }
        match &self.kind {
            SourceKind::Player { uuid, sink, .. } => {
                let accepts = global
                    .players
                    .connection(Uuid(*uuid))
                    .is_none_or(|(_, inbox)| {
                        inbox
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .accepts_system(false)
                    });
                if accepts {
                    sink.send(&SystemChat {
                        content: message,
                        overlay: false,
                    });
                }
            }
            // `MinecraftServer.sendSystemMessage`.
            SourceKind::Console => match &self.feedback {
                Some(feedback) => feedback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(format!("msg {}", message.to_json_string())),
                None => log::info!("System chat: {}", global.console_text(&message)),
            },
            // `RconConsoleSource.sendSystemMessage`: appended without a separator.
            SourceKind::Rcon(buffer) => buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_str(&global.english(&message)),
            SourceKind::Server => {}
            // `CloseableCommandBlockSource.sendSystemMessage`: the block's last output.
            SourceKind::CommandBlock(out) => {
                if out.track {
                    *out.last
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message);
                }
            }
        }
    }

    /// `CommandSource.acceptsSuccess`: players by `send_command_feedback`, command blocks
    /// that track their output by it too.
    fn accepts_success(&self, global: &Global) -> bool {
        let feedback = || global.level.rules().bool(Rule::SEND_COMMAND_FEEDBACK);
        match &self.kind {
            SourceKind::Player { .. } => feedback(),
            SourceKind::Console | SourceKind::Rcon(_) => true,
            SourceKind::Server => false,
            SourceKind::CommandBlock(out) => out.track && feedback(),
        }
    }

    /// `CommandSource.shouldInformAdmins`.
    fn informs_admins(&self, global: &Global) -> bool {
        match &self.kind {
            SourceKind::Console => global.broadcast_console_to_ops,
            SourceKind::Rcon(_) => global.broadcast_rcon_to_ops,
            SourceKind::Player { .. } => true,
            SourceKind::Server => false,
            SourceKind::CommandBlock(out) => {
                out.track && global.level.rules().bool(Rule::COMMAND_BLOCK_OUTPUT)
            }
        }
    }

    /// `sendSuccess`: feedback to the source if it accepts it and, when `inform_admins` and
    /// the source informs admins, `broadcastToAdmins`: a gray italic note to the other
    /// operators (by `send_command_feedback`) and the log (by `log_admin_commands`, not for
    /// the console itself).
    pub(crate) fn send_success(&self, global: &Global, message: Component, inform_admins: bool) {
        if self.silent {
            return;
        }
        let broadcast = inform_admins && self.informs_admins(global);
        if self.accepts_success(global) {
            self.send_system(global, message.clone());
        }
        if !broadcast {
            return;
        }
        let broadcast =
            Component::translatable("chat.type.admin", vec![self.display_name(), message])
                .color(Color::Named(NamedColor::Gray))
                .italic(true);
        let (feedback, log_admin) = {
            let rules = global.level.rules();
            (
                rules.bool(Rule::SEND_COMMAND_FEEDBACK),
                rules.bool(Rule::LOG_ADMIN_COMMANDS),
            )
        };
        let own = self.receiver_uuid();
        if feedback {
            for entry in global.players.snapshot() {
                if Some(entry.uuid.0) != own && global.access.op_level(entry.uuid.0).is_some() {
                    let accepts = entry
                        .inbox
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .accepts_system(false);
                    if accepts {
                        entry.sink.send(&SystemChat {
                            content: broadcast.clone(),
                            overlay: false,
                        });
                    }
                }
            }
        }
        if log_admin && !matches!(self.kind, SourceKind::Console) {
            log::info!("System chat: {}", global.console_text(&broadcast));
        }
    }

    /// `sendFailure`: the message in red, if the source accepts failures (command blocks only
    /// while they track their output).
    pub(crate) fn send_failure(&self, global: &Global, message: Component) {
        if let SourceKind::CommandBlock(out) = &self.kind
            && !out.track
        {
            return;
        }
        self.send_system(
            global,
            Component::empty()
                .append(message)
                .color(Color::Named(NamedColor::Red)),
        );
    }
}
