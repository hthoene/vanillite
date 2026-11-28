//! The player kind: what a player entity holds besides its core columns, its connection, the
//! play packet handlers, movement validation and chunk sending.

// SEC-07: packet handler modules of the game deny the lints of the input-handling crates.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]

pub mod chat;
pub mod chorus;
pub mod chunks;
pub mod connection;
pub mod cooldowns;
pub mod encode;
pub mod item_use;
pub mod items;
pub mod join;
pub mod movement;
pub mod play;
pub mod sink;
pub mod using;
pub mod world;

pub use chunks::{ChunkSource, PlayerChunks};
pub use connection::{
    AdminRequest, ChatContext, ChatRequest, Connection, EntityAction, Handled, PlayerChatState,
    PlayerState, SignedCommand, VehicleInput, effective_view_distance,
};
pub use movement::{Body, Kick, MoveContext, MoveRequest, Movement};
pub use sink::PacketSink;

use vanillite_base::ChunkPos;
use vanillite_config::GameMode;
pub use vanillite_protocol::packets::common::ChatVisibility;
use vanillite_world::entity::{Dimensions, Pose};

/// One property of a game profile (`textures` carries the skin).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileProperty {
    /// Name, e.g. `textures`.
    pub name: String,
    /// Base64 value.
    pub value: String,
    /// Mojang's signature, if any.
    pub signature: Option<String>,
}

/// A player's game profile. The UUID lives in the core columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameProfile {
    /// Player name.
    pub name: String,
    /// Properties (skin and cape).
    pub properties: Vec<ProfileProperty>,
}

/// Which hand is the main hand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MainHand {
    /// Left.
    Left,
    /// Right.
    #[default]
    Right,
}

/// What the client reported in `client_information`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInformation {
    /// Language, e.g. `en_us`.
    pub language: String,
    /// Requested view distance in chunks.
    pub view_distance: u8,
    /// Displayed skin parts (`DATA_PLAYER_MODE_CUSTOMISATION`).
    pub model_customisation: u8,
    /// Main hand (`DATA_PLAYER_MAIN_HAND`).
    pub main_hand: MainHand,
    /// Whether the player may be listed in the server list sample.
    pub allows_listing: bool,
    /// Which chat the player wants to see.
    pub chat_visibility: ChatVisibility,
}

impl Default for ClientInformation {
    /// Vanilla's `ClientInformation.createDefault()`.
    fn default() -> Self {
        ClientInformation {
            language: "en_us".to_owned(),
            view_distance: 2,
            model_customisation: 0,
            main_hand: MainHand::Right,
            allows_listing: false,
            chat_visibility: ChatVisibility::Full,
        }
    }
}

/// The chunks a player has loaded on its client (`ChunkTrackingView`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkView {
    /// Centre chunk.
    pub center: ChunkPos,
    /// Effective view distance in chunks (client's request clamped to the server's).
    pub view_distance: i32,
}

/// The kind data of a player entity (`Dense<PlayerData>`).
#[derive(Debug)]
pub struct PlayerData {
    /// Profile with name and skin.
    pub profile: GameProfile,
    /// Game mode.
    pub game_mode: GameMode,
    /// `previousPlayerGameType`: the game mode before the last change.
    pub previous_game_mode: Option<GameMode>,
    /// Client settings.
    pub client: ClientInformation,
    /// Loaded chunks on the client.
    pub chunk_view: ChunkView,
    /// The connection to the client.
    pub connection: Connection,
    /// Inventory and inventory menu.
    pub inventory: crate::inventory::PlayerInventory,
    /// The equipment other players last saw (`detectEquipmentUpdates`), in
    /// [`encode::EQUIPMENT_SLOTS`] order.
    pub last_equipment: [crate::item::ItemStack; 6],
    /// Health, food, air, fire, experience and the combat tracker.
    pub survival: crate::survival::player::Survival,
    /// `ServerPlayer.postEffects` (`/posteffect`), saved as `post_effects`.
    pub post_effects: Vec<String>,
    /// Statistics (`ServerStatsCounter`).
    pub stats: crate::stats::StatsCounter,
    /// Advancement progress (`PlayerAdvancements`).
    pub advancements: crate::advancements::PlayerAdvancements,
    /// The recipes the player knows (`ServerRecipeBook`).
    pub recipe_book: crate::recipe::book::PlayerRecipeBook,
}

impl PlayerData {
    /// `ServerPlayer.awardRecipes`: unlocks recipes (by id, e.g. `minecraft:torch`) and tells
    /// the client; the number of newly unlocked recipe displays. Unknown and special recipes
    /// are skipped.
    pub fn unlock_recipes(&mut self, ids: &[&str]) -> usize {
        let Some(data) = play::game_data() else {
            return 0;
        };
        let before: Vec<bool> = ids.iter().map(|id| self.recipe_book.contains(id)).collect();
        let (n, packet) = self.recipe_book.unlock(&data.recipes.book, ids);
        if let Some(packet) = packet {
            self.connection.sink.send(&packet);
        }
        // `CriteriaTriggers.RECIPE_UNLOCKED` of every newly known recipe.
        for (id, known) in ids.iter().zip(before) {
            if !known && self.recipe_book.contains(id) {
                self.advancements
                    .trigger(crate::advancements::trigger::Event::RecipeUnlocked(
                        (*id).to_owned(),
                    ));
            }
        }
        n
    }

    /// `ServerPlayer.resetRecipes`: locks recipes and tells the client; the number of removed
    /// recipe displays.
    pub fn lock_recipes(&mut self, ids: &[&str]) -> usize {
        let Some(data) = play::game_data() else {
            return 0;
        };
        let (n, packet) = self.recipe_book.lock(&data.recipes.book, ids);
        if let Some(packet) = packet {
            self.connection.sink.send(&packet);
        }
        n
    }
}

/// `Player.getDefaultDimensions(pose)`.
pub fn dimensions(pose: Pose) -> Dimensions {
    let (width, height) = match pose {
        Pose::Crouching => (0.6, 1.5),
        Pose::Swimming | Pose::FallFlying | Pose::SpinAttack => (0.6, 0.6),
        Pose::Sleeping | Pose::Dying => (0.2, 0.2),
        _ => (0.6, 1.8),
    };
    Dimensions { width, height }
}

/// `LivingEntity.getDimensions`: the pose's size scaled by `minecraft:scale` (sleeping keeps
/// its own).
pub fn scaled_dimensions(pose: Pose, scale: f64) -> Dimensions {
    let d = dimensions(pose);
    if scale == 1.0 || pose == Pose::Sleeping {
        return d;
    }
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let s = scale as f32;
    Dimensions {
        width: d.width * s,
        height: d.height * s,
    }
}

/// Fires a player event of `kind` (`vanillite_abi::event`; `entity` the player) when a plugin
/// listens: `fill` sets the kind's other fields. Returns the event with the handlers'
/// changes, `None` when nobody listens (nothing is built then).
pub fn player_event(
    hooks: &dyn crate::hooks::GameHooks,
    kind: u32,
    player: u128,
    fill: impl FnOnce(&mut crate::hooks::Event),
) -> Option<crate::hooks::Event> {
    if !hooks.wants(kind) {
        return None;
    }
    let mut event = crate::hooks::Event {
        entity: player,
        entity_type: "minecraft:player".to_owned(),
        ..crate::hooks::Event::new(kind)
    };
    fill(&mut event);
    hooks.event(crate::hooks::HookWorld::NONE, &mut event);
    Some(event)
}
