//! Vaults (`VaultBlockEntity`, `VaultConfig`, `VaultServerData`, `VaultSharedData`,
//! `VaultState`): the `vault_state` machine, the connected players, unlocking with the key
//! item (trial key, ominous trial key), the players already rewarded and the ejection of the
//! reward loot.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_nbt::{Compound, List, Tag};

use super::sound_id;
use super::trial_spawner::{Detected, detect_players, spawn_item_up, uuid_list, uuid_list_tag};
use crate::inventory::{stack_from_nbt, stack_to_nbt};
use crate::item::ItemStack;
use crate::level::block_entity::BlockEntity;
use crate::level::{Level, SOUND_BLOCKS, update};

/// The reward table of a vault without its own (`TRIAL_CHAMBERS_REWARD`).
const DEFAULT_LOOT_TABLE: &str = "minecraft:chests/trial_chambers/reward";
/// `MAX_REWARD_PLAYERS`.
const MAX_REWARD_PLAYERS: usize = 128;

/// `VaultState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultState {
    Inactive,
    Active,
    Unlocking,
    Ejecting,
}

impl VaultState {
    /// The property value.
    pub fn name(self) -> &'static str {
        match self {
            VaultState::Inactive => "inactive",
            VaultState::Active => "active",
            VaultState::Unlocking => "unlocking",
            VaultState::Ejecting => "ejecting",
        }
    }

    /// The state of a block state.
    pub fn of(state: BlockState) -> VaultState {
        match state.value("vault_state") {
            Some("active") => VaultState::Active,
            Some("unlocking") => VaultState::Unlocking,
            Some("ejecting") => VaultState::Ejecting,
            _ => VaultState::Inactive,
        }
    }
}

/// The block entity of a vault.
#[derive(Clone, Debug, PartialEq)]
pub struct Vault {
    // `VaultConfig`.
    pub loot_table: String,
    pub activation_range: f64,
    pub deactivation_range: f64,
    pub key_item: ItemStack,
    pub override_loot_table_to_display: Option<String>,
    // `VaultServerData`.
    /// `rewardedPlayers`, oldest first.
    pub rewarded_players: Vec<u128>,
    pub state_updating_resumes_at: i64,
    pub items_to_eject: Vec<ItemStack>,
    pub total_ejections_needed: i32,
    pub last_insert_fail: i64,
    server_dirty: bool,
    // `VaultSharedData`.
    pub display_item: ItemStack,
    pub connected_players: Vec<u128>,
    pub connected_particles_range: f64,
    shared_dirty: bool,
}

fn trial_key() -> ItemStack {
    vanillite_data::registry::ITEM
        .id("minecraft:trial_key")
        .map_or(ItemStack::EMPTY, |id| ItemStack::new(id, 1))
}

impl Default for Vault {
    fn default() -> Self {
        Vault {
            loot_table: DEFAULT_LOOT_TABLE.to_owned(),
            activation_range: 4.0,
            deactivation_range: 4.5,
            key_item: trial_key(),
            override_loot_table_to_display: None,
            rewarded_players: Vec::new(),
            state_updating_resumes_at: 0,
            items_to_eject: Vec::new(),
            total_ejections_needed: 0,
            last_insert_fail: 0,
            server_dirty: false,
            display_item: ItemStack::EMPTY,
            connected_players: Vec::new(),
            connected_particles_range: 4.5,
            shared_dirty: false,
        }
    }
}

fn double(c: &Compound, key: &str) -> Option<f64> {
    match c.get(key)? {
        Tag::Double(v) => Some(*v),
        Tag::Float(v) => Some(f64::from(*v)),
        t => t.as_i64().map(|v| v as f64),
    }
}

fn qualify(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// A stack as the client reads it from the shared data (`ItemStack.CODEC`, without
/// components).
fn display_stack_nbt(stack: &ItemStack) -> Compound {
    let mut c = Compound::new();
    c.insert(
        "id",
        vanillite_data::registry::ITEM
            .name(stack.item)
            .unwrap_or("minecraft:air"),
    );
    if stack.count != 1 {
        c.insert("count", stack.count);
    }
    c
}

impl Vault {
    /// `VaultBlockEntity.loadAdditional`.
    pub fn load(nbt: &Compound) -> Vault {
        let mut v = Vault::default();
        if let Some(Tag::Compound(config)) = nbt.get("config") {
            if let Some(t) = config.get("loot_table").and_then(Tag::as_str) {
                v.loot_table = qualify(t);
            }
            v.activation_range = double(config, "activation_range").unwrap_or(4.0);
            v.deactivation_range = double(config, "deactivation_range").unwrap_or(4.5);
            // `lenientOptionalFieldOf("key_item")`: no key in a given config is no key at all
            // (the trial key is the default of a missing config only).
            v.key_item = match config.get("key_item") {
                Some(Tag::Compound(key)) => stack_from_nbt(key),
                _ => ItemStack::EMPTY,
            };
            v.override_loot_table_to_display = config
                .get("override_loot_table_to_display")
                .and_then(Tag::as_str)
                .map(qualify);
        }
        if let Some(Tag::Compound(server)) = nbt.get("server_data") {
            v.rewarded_players = uuid_list(server.get("rewarded_players"));
            v.state_updating_resumes_at = server
                .get("state_updating_resumes_at")
                .and_then(Tag::as_i64)
                .unwrap_or(0);
            if let Some(Tag::List(items)) = server.get("items_to_eject") {
                v.items_to_eject = items
                    .0
                    .iter()
                    .filter_map(Tag::as_compound)
                    .map(stack_from_nbt)
                    .collect();
            }
            v.total_ejections_needed = server
                .get("total_ejections_needed")
                .and_then(Tag::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0);
        }
        if let Some(Tag::Compound(shared)) = nbt.get("shared_data") {
            if let Some(Tag::Compound(item)) = shared.get("display_item") {
                v.display_item = stack_from_nbt(item);
            }
            v.connected_players = uuid_list(shared.get("connected_players"));
            v.connected_particles_range =
                double(shared, "connected_particles_range").unwrap_or(4.5);
        }
        v
    }

    /// `VaultBlockEntity.saveAdditional`.
    pub fn save(&self, out: &mut Compound) {
        let mut config = Compound::new();
        if self.loot_table != DEFAULT_LOOT_TABLE {
            config.insert("loot_table", Tag::String(self.loot_table.clone()));
        }
        if self.activation_range != 4.0 {
            config.insert("activation_range", self.activation_range);
        }
        if self.deactivation_range != 4.5 {
            config.insert("deactivation_range", self.deactivation_range);
        }
        if !self.key_item.is_empty() {
            config.insert("key_item", stack_to_nbt(&self.key_item));
        }
        if let Some(t) = &self.override_loot_table_to_display {
            config.insert("override_loot_table_to_display", Tag::String(t.clone()));
        }
        out.insert("config", config);
        out.insert("shared_data", self.shared_data(false));
        let mut server = Compound::new();
        if !self.rewarded_players.is_empty() {
            server.insert(
                "rewarded_players",
                uuid_list_tag(self.rewarded_players.iter().copied()),
            );
        }
        if self.state_updating_resumes_at != 0 {
            server.insert("state_updating_resumes_at", self.state_updating_resumes_at);
        }
        if !self.items_to_eject.is_empty() {
            server.insert(
                "items_to_eject",
                List(
                    self.items_to_eject
                        .iter()
                        .map(|s| Tag::Compound(stack_to_nbt(s)))
                        .collect(),
                ),
            );
        }
        if self.total_ejections_needed != 0 {
            server.insert("total_ejections_needed", self.total_ejections_needed);
        }
        out.insert("server_data", server);
    }

    /// `VaultSharedData.CODEC`; `client` writes the display item as the client reads it.
    fn shared_data(&self, client: bool) -> Compound {
        let mut c = Compound::new();
        if !self.display_item.is_empty() {
            let item = if client {
                display_stack_nbt(&self.display_item)
            } else {
                stack_to_nbt(&self.display_item)
            };
            c.insert("display_item", item);
        }
        if !self.connected_players.is_empty() {
            c.insert(
                "connected_players",
                uuid_list_tag(self.connected_players.iter().copied()),
            );
        }
        if self.connected_particles_range != 4.5 {
            c.insert("connected_particles_range", self.connected_particles_range);
        }
        c
    }

    /// `getUpdateTag`: the shared data.
    pub fn update_tag(&self) -> Compound {
        let mut c = Compound::new();
        c.insert("shared_data", self.shared_data(true));
        c
    }

    /// `setDisplayItem`.
    fn set_display_item(&mut self, stack: ItemStack) {
        if self.display_item != stack {
            self.display_item = stack;
            self.shared_dirty = true;
        }
    }

    /// `hasRewardedPlayer`.
    pub fn has_rewarded_player(&self, uuid: u128) -> bool {
        self.rewarded_players.contains(&uuid)
    }

    /// `addToRewardedPlayers`: at most 128, the oldest goes first.
    pub fn add_rewarded_player(&mut self, uuid: u128) {
        if !self.rewarded_players.contains(&uuid) {
            self.rewarded_players.push(uuid);
        }
        if self.rewarded_players.len() > MAX_REWARD_PLAYERS {
            self.rewarded_players.remove(0);
        }
        self.server_dirty = true;
    }

    /// `pauseStateUpdatingUntil`.
    fn pause_until(&mut self, time: i64) {
        self.state_updating_resumes_at = time;
        self.server_dirty = true;
    }

    /// `getNextItemToEject`.
    fn next_item_to_eject(&self) -> ItemStack {
        self.items_to_eject
            .last()
            .cloned()
            .unwrap_or(ItemStack::EMPTY)
    }

    /// `ejectionProgress`.
    #[allow(clippy::cast_precision_loss)]
    fn ejection_progress(&self) -> f32 {
        if self.total_ejections_needed == 1 {
            return 1.0;
        }
        // `1 - Mth.inverseLerp(size, 1, total)`.
        let size = self.items_to_eject.len() as f32;
        let total = self.total_ejections_needed as f32;
        1.0 - (size - 1.0) / (total - 1.0)
    }

    /// `isValidToInsert`.
    pub fn is_valid_key(&self, stack: &ItemStack) -> bool {
        stack.same_item_same_components(&self.key_item) && stack.count >= self.key_item.count
    }
}

fn vault_mut(level: &mut Level, pos: BlockPos) -> Option<&mut Vault> {
    match level.block_entity_mut(pos) {
        Some(BlockEntity::Vault(v)) => Some(v),
        _ => None,
    }
}

fn play(level: &mut Level, pos: BlockPos, sound: &str, volume: f32, pitch: f32) {
    level.play_block_sound(None, pos, sound_id(sound), SOUND_BLOCKS, volume, pitch);
}

/// `updateConnectedPlayersWithinRange`.
fn update_connected_players(level: &mut Level, pos: BlockPos, range: f64) {
    let mut found: Vec<Detected> = Vec::new();
    detect_players(level, pos, range, true, false, &mut found);
    let Some(v) = vault_mut(level, pos) else {
        return;
    };
    let mut current: Vec<u128> = found
        .into_iter()
        .map(|(_, u)| u)
        .filter(|u| !v.rewarded_players.contains(u))
        .collect();
    current.sort_unstable();
    let mut old = v.connected_players.clone();
    old.sort_unstable();
    if old != current {
        v.connected_players = current;
        v.shared_dirty = true;
    }
}

/// `updateStateForConnectedPlayers`.
fn state_for_connected_players(level: &mut Level, pos: BlockPos, range: f64) -> VaultState {
    update_connected_players(level, pos, range);
    let time = level.game_time() + 20;
    let Some(v) = vault_mut(level, pos) else {
        return VaultState::Inactive;
    };
    v.pause_until(time);
    if v.connected_players.is_empty() {
        VaultState::Inactive
    } else {
        VaultState::Active
    }
}

/// A random item of the display loot table (`getRandomDisplayItemFromLootTable`).
fn random_display_item(level: &mut Level, pos: BlockPos, table: &str) -> ItemStack {
    let Some(data) = crate::player::play::game_data() else {
        return ItemStack::EMPTY;
    };
    let mut items = Vec::new();
    // `LootContextParamSets.VAULT`: the origin is the vault's centre.
    let origin = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    data.random_items(table, Some(origin), level.random(), &mut items);
    if items.is_empty() {
        return ItemStack::EMPTY;
    }
    let len = i32::try_from(items.len()).unwrap_or(i32::MAX);
    let i = usize::try_from(level.random().next_int_bounded(len)).unwrap_or(0);
    items.swap_remove(i.min(items.len() - 1))
}

/// `cycleDisplayItemFromLootTable`.
fn cycle_display_item(level: &mut Level, pos: BlockPos, state: VaultState) {
    let Some(v) = vault_mut(level, pos) else {
        return;
    };
    if v.key_item.is_empty() || state == VaultState::Inactive {
        v.set_display_item(ItemStack::EMPTY);
        return;
    }
    let table = v
        .override_loot_table_to_display
        .clone()
        .unwrap_or_else(|| v.loot_table.clone());
    let item = random_display_item(level, pos, &table);
    if let Some(v) = vault_mut(level, pos) {
        v.set_display_item(item);
    }
}

/// `VaultState.onTransition`: `onExit` of the old state, `onEnter` of the new one.
fn on_transition(
    level: &mut Level,
    pos: BlockPos,
    from: VaultState,
    to: VaultState,
    ominous: bool,
) {
    if from == VaultState::Ejecting {
        play(level, pos, "block.vault.close_shutter", 1.0, 1.0);
    }
    match to {
        VaultState::Inactive => {
            if let Some(v) = vault_mut(level, pos) {
                v.set_display_item(ItemStack::EMPTY);
            }
            level.level_event(None, 3016, pos, i32::from(ominous));
        }
        VaultState::Active => {
            if vault_mut(level, pos).is_some_and(|v| v.display_item.is_empty()) {
                cycle_display_item(level, pos, to);
            }
            level.level_event(None, 3015, pos, i32::from(ominous));
        }
        VaultState::Unlocking => play(level, pos, "block.vault.insert_item", 1.0, 1.0),
        VaultState::Ejecting => play(level, pos, "block.vault.open_shutter", 1.0, 1.0),
    }
}

/// `setVaultState`.
fn set_vault_state(level: &mut Level, pos: BlockPos, block: BlockState, to: VaultState) {
    let from = VaultState::of(block);
    let ominous = block.value("ominous") == Some("true");
    if let Some(next) = block.with("vault_state", to.name()) {
        level.set_block(pos, next, update::ALL);
    }
    on_transition(level, pos, from, to, ominous);
}

/// `VaultState.tickAndGetNext`.
fn tick_and_get_next(level: &mut Level, pos: BlockPos, state: VaultState) -> VaultState {
    let time = level.game_time();
    let Some(v) = vault_mut(level, pos) else {
        return state;
    };
    let (activation, deactivation) = (v.activation_range, v.deactivation_range);
    match state {
        VaultState::Inactive => state_for_connected_players(level, pos, activation),
        VaultState::Active => state_for_connected_players(level, pos, deactivation),
        VaultState::Unlocking => {
            v.pause_until(time + 20);
            VaultState::Ejecting
        }
        VaultState::Ejecting => {
            if v.items_to_eject.is_empty() {
                v.total_ejections_needed = 0;
                v.server_dirty = true;
                return state_for_connected_players(level, pos, deactivation);
            }
            let progress = v.ejection_progress();
            let item = v.items_to_eject.pop().unwrap_or(ItemStack::EMPTY);
            v.server_dirty = true;
            let next = v.next_item_to_eject();
            // `ejectResultItem`.
            let at = Vec3::new(
                f64::from(pos.x) + 0.5,
                f64::from(pos.y) + 1.2,
                f64::from(pos.z) + 0.5,
            );
            spawn_item_up(level, item, 2, at);
            level.level_event(None, 3017, pos, 0);
            play(
                level,
                pos,
                "block.vault.eject_item",
                1.0,
                0.8 + 0.4 * progress,
            );
            if let Some(v) = vault_mut(level, pos) {
                v.set_display_item(next);
                v.pause_until(time + 20);
            }
            VaultState::Ejecting
        }
    }
}

/// `VaultBlockEntity.Server.tick`.
pub fn tick(level: &mut Level, pos: BlockPos, block: BlockState) {
    let time = level.game_time();
    let state = VaultState::of(block);
    if time % 20 == 0 && state == VaultState::Active {
        cycle_display_item(level, pos, state);
    }
    if vault_mut(level, pos).is_some_and(|v| time >= v.state_updating_resumes_at) {
        let next = tick_and_get_next(level, pos, state);
        if next != state {
            set_vault_state(level, pos, block, next);
        }
    }
    let Some(v) = vault_mut(level, pos) else {
        return;
    };
    let shared = v.shared_dirty;
    v.server_dirty = false;
    v.shared_dirty = false;
    if shared {
        level.send_block_entity_updated(pos);
    }
}

/// `VaultBlock.useItemOn` with a non-empty stack: in the active state the vault takes the key
/// (`tryInsertKey`) and the use is consumed (`Some`, with whether the key was taken); else
/// `None` (`TRY_WITH_EMPTY_HAND`).
pub fn use_item_on(
    level: &mut Level,
    pos: BlockPos,
    player: u128,
    stack: &ItemStack,
) -> Option<bool> {
    let block = level.block(pos);
    if stack.is_empty() || VaultState::of(block) != VaultState::Active {
        return None;
    }
    let time = level.game_time();
    let v = vault_mut(level, pos)?;
    if v.key_item.is_empty() {
        return Some(false);
    }
    let fail = |level: &mut Level, sound: &str| {
        let Some(v) = vault_mut(level, pos) else {
            return;
        };
        if time >= v.last_insert_fail + 15 {
            v.last_insert_fail = time;
            play(level, pos, sound, 1.0, 1.0);
        }
    };
    if !v.is_valid_key(stack) {
        fail(level, "block.vault.insert_item_fail");
        return Some(false);
    }
    if v.has_rewarded_player(player) {
        fail(level, "block.vault.reject_rewarded_player");
        return Some(false);
    }
    let table = v.loot_table.clone();
    let deactivation = v.deactivation_range;
    let Some(data) = crate::player::play::game_data() else {
        return Some(false);
    };
    let mut items = Vec::new();
    let origin = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    // `resolveItemsToEject`: `getRandomItems(params)` draws from the table's random sequence.
    match data.loot.random_sequence(&table) {
        Some(key) => level.with_random_sequence(key, |r| {
            data.random_items(&table, Some(origin), r, &mut items);
        }),
        None => data.random_items(&table, Some(origin), level.random(), &mut items),
    }
    if items.is_empty() {
        return Some(false);
    }
    let Some(v) = vault_mut(level, pos) else {
        return Some(false);
    };
    // `unlock`.
    v.total_ejections_needed = i32::try_from(items.len()).unwrap_or(i32::MAX);
    v.items_to_eject = items;
    let next = v.next_item_to_eject();
    v.set_display_item(next);
    v.pause_until(time + 14);
    v.server_dirty = true;
    set_vault_state(level, pos, block, VaultState::Unlocking);
    if let Some(v) = vault_mut(level, pos) {
        v.add_rewarded_player(player);
    }
    update_connected_players(level, pos, deactivation);
    Some(true)
}

/// The number of key items a successful insert consumes.
pub fn key_count(level: &Level, pos: BlockPos) -> i32 {
    match level.block_entity(pos) {
        Some(BlockEntity::Vault(v)) => v.key_item.count,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewarded_players_are_capped_oldest_first() {
        let mut v = Vault::default();
        for i in 0..130u128 {
            v.add_rewarded_player(i);
        }
        assert_eq!(v.rewarded_players.len(), 128);
        assert!(!v.has_rewarded_player(0));
        assert!(!v.has_rewarded_player(1));
        assert!(v.has_rewarded_player(2));
        assert!(v.has_rewarded_player(129));
    }

    #[test]
    fn only_the_key_item_unlocks() {
        let v = Vault::default();
        let key = trial_key();
        assert!(v.is_valid_key(&key));
        let other = vanillite_data::registry::ITEM
            .id("minecraft:ominous_trial_key")
            .map(|id| ItemStack::new(id, 1))
            .expect("item");
        assert!(!v.is_valid_key(&other));
    }

    #[test]
    fn nbt_round_trip() {
        let mut v = Vault::default();
        v.add_rewarded_player(42);
        v.state_updating_resumes_at = 7;
        v.activation_range = 3.0;
        let mut c = Compound::new();
        v.save(&mut c);
        let back = Vault::load(&c);
        assert_eq!(back.rewarded_players, vec![42]);
        assert_eq!(back.state_updating_resumes_at, 7);
        assert_eq!(back.activation_range, 3.0);
        assert_eq!(back.key_item, v.key_item);
    }

    #[test]
    fn ejection_progress_runs_to_one() {
        let mut v = Vault {
            total_ejections_needed: 3,
            items_to_eject: vec![trial_key(), trial_key(), trial_key()],
            ..Vault::default()
        };
        assert_eq!(v.ejection_progress(), 0.0);
        v.items_to_eject.pop();
        assert_eq!(v.ejection_progress(), 0.5);
        v.items_to_eject.pop();
        assert_eq!(v.ejection_progress(), 1.0);
    }
}
