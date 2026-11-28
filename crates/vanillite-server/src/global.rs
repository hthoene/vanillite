//! Server-wide state that worlds can read and change through narrow methods (architecture 4.7).

use vanillite_game::player::chat::ChatSettings;
use vanillite_world::entity::NetworkIds;

use std::sync::{Arc, OnceLock, Weak};

use crate::access::Access;
use crate::clock::Clock;
use crate::player_list::PlayerList;
use crate::server::ServerHandle;
use crate::world::{ServerWorld, WorldId};

/// Server-wide state shared with all worlds.
#[derive(Debug)]
pub struct Global {
    /// The monotonic server clock.
    pub clock: Clock,
    /// Online players.
    pub players: PlayerList,
    /// Network entity ids of all worlds.
    pub network_ids: Arc<NetworkIds>,
    /// Secure chat and spam settings.
    pub chat: ChatSettings,
    /// Network ids of the chat types.
    pub chat_types: ChatTypes,
    /// Operators and whitelist.
    pub access: Access,
    /// A handle to the server for work on other worlds and `/stop`; set once the server runs.
    pub handle: OnceLock<ServerHandle>,
    /// The command tree; replaced (append-only) when plugins add commands after the start.
    pub(crate) commands: std::sync::RwLock<Arc<crate::commands::CommandDispatcher>>,
    /// `online-mode`.
    pub(crate) online_mode: bool,
    /// The dimensions; a world's index here is its [`WorldId`].
    pub dimensions: crate::dimensions::Dimensions,
    /// Game time, clocks, game rules, weather and the survival registries.
    pub(crate) level: crate::level::LevelState,
    /// The world seed (`/seed`).
    pub(crate) seed: i64,
    /// Entries of data-driven registries commands refer to (timelines, world clocks,
    /// enchantments).
    pub(crate) registries: std::collections::BTreeMap<String, Vec<String>>,
    /// Tool rules, loot tables and item data of the block and item handlers: this server's
    /// current game data (replaced by reloads), [`Global::game`].
    pub(crate) game_data: std::sync::RwLock<Arc<vanillite_game::player::play::GameData>>,
    /// `ClientboundUpdateRecipesPacket` of the loaded recipes, encoded once.
    pub(crate) recipes_packet: std::sync::RwLock<Arc<vanillite_protocol::SharedPacket>>,
    /// `spawn-protection` radius in blocks.
    pub(crate) spawn_protection: i32,
    /// Scoreboard, command storage, functions and tags.
    pub(crate) systems: crate::systems::GameSystems,
    /// The data pack repository.
    pub(crate) packs: crate::packs::DataPacks,
    /// `StructureTemplateManager`: the structure templates of `generated/` and the data packs.
    pub(crate) templates: Arc<vanillite_worldgen::template::StructureTemplateManager>,
    /// `/tick`'s state; the server loop reads it once per tick.
    pub(crate) tick_rate: std::sync::Mutex<crate::tick_rate::TickRateManager>,
    /// The durations of the last ticks (`/tick query`, `/vanillite tps`).
    pub(crate) tick_times: std::sync::Mutex<crate::tick_rate::TickTimes>,
    /// Uptime, CPU samples and the profiler of `/vanillite`.
    pub(crate) diagnostics: crate::diagnostics::Diagnostics,
    /// Vanilla's English translations, for texts of the console and RCON.
    pub(crate) language: Arc<std::collections::HashMap<String, String>>,
    /// `broadcast-console-to-ops`.
    pub(crate) broadcast_console_to_ops: bool,
    /// `broadcast-rcon-to-ops`.
    pub(crate) broadcast_rcon_to_ops: bool,
    /// `force-gamemode`.
    pub(crate) force_game_mode: bool,
    /// The plugins.
    pub(crate) plugins: vanillite_plugin_host::PluginHost,
    /// The server's version as `vanillite --version` shows it (`/version`, Query).
    pub(crate) version: String,
    /// A message of the day a plugin set, for the server list to take over.
    pub(crate) motd: std::sync::Mutex<Option<vanillite_protocol::text::Component>>,
    /// `server_data` with the server list's MOTD and icon, sent to joining players
    /// (`ServerPlayer.sendServerStatus`).
    pub(crate) server_data: std::sync::RwLock<Arc<vanillite_protocol::SharedPacket>>,
    /// The server icon (a 64x64 PNG) of `server_data`.
    pub(crate) server_icon: Option<Vec<u8>>,
    /// This value itself, for work handed to other worlds.
    pub(crate) this: Weak<Global>,
}

impl Global {
    /// The current command tree.
    pub(crate) fn commands(&self) -> Arc<crate::commands::CommandDispatcher> {
        Arc::clone(
            &self
                .commands
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// The current game data (loot tables, recipes, tags), replaced by `/reload`.
    pub(crate) fn game(&self) -> Arc<vanillite_game::player::play::GameData> {
        Arc::clone(
            &self
                .game_data
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Replaces the game data after a reload (and installs it for the game crate).
    pub(crate) fn set_game(&self, game: Arc<vanillite_game::player::play::GameData>) {
        vanillite_game::player::play::install_game_data(Arc::clone(&game));
        *self
            .game_data
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = game;
    }

    /// A shared reference to this value (for tasks on other worlds).
    pub fn arc(&self) -> Option<Arc<Global>> {
        self.this.upgrade()
    }

    /// Runs `task` on another world between its ticks (cross-world command effects). Dropped
    /// before the server runs.
    pub fn run_in_world(
        &self,
        world: WorldId,
        task: impl FnOnce(&mut ServerWorld) + Send + 'static,
    ) {
        if let Some(handle) = self.handle.get() {
            handle.run_in_world(world, task);
        }
    }

    /// The tick rate state, locked for one read or change.
    pub(crate) fn tick_rate(&self) -> std::sync::MutexGuard<'_, crate::tick_rate::TickRateManager> {
        self.tick_rate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Component.getString` in English, as the console and RCON show texts.
    pub(crate) fn english(&self, text: &vanillite_protocol::text::Component) -> String {
        text.to_string_in(&|key| self.language.get(key).cloned())
    }

    /// [`Self::english`] with colours as `§` codes, for the server console (the logger shows
    /// them as terminal colours or strips them).
    pub(crate) fn console_text(&self, text: &vanillite_protocol::text::Component) -> String {
        text.to_legacy_in(&|key| self.language.get(key).cloned())
    }

    /// Asks the server to stop after the current tick (`/stop`).
    pub fn stop(&self) {
        if let Some(handle) = self.handle.get() {
            handle.stop();
        }
    }
}

/// Network ids of the chat types in the `minecraft:chat_type` registry the clients received.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChatTypes {
    /// `minecraft:chat`.
    pub chat: i32,
    /// `minecraft:say_command`.
    pub say_command: i32,
    /// `minecraft:msg_command_incoming`.
    pub msg_incoming: i32,
    /// `minecraft:msg_command_outgoing`.
    pub msg_outgoing: i32,
    /// `minecraft:emote_command`.
    pub emote_command: i32,
    /// `minecraft:team_msg_command_incoming`.
    pub team_msg_incoming: i32,
    /// `minecraft:team_msg_command_outgoing`.
    pub team_msg_outgoing: i32,
}
