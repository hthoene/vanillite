//! Trial spawners (`TrialSpawnerBlockEntity`, `TrialSpawner`, `TrialSpawnerStateData`,
//! `TrialSpawnerState`): the state machine in the `trial_spawner_state` property, the players
//! a spawner detected, the mobs it tracks, turning ominous through the trial omen, the reward
//! ejection and the cooldown. Configurations come from the `minecraft:trial_spawner` registry
//! (by id) or inline.
//!
//! Ominous spawners drop items on players and mobs through `OminousItemSpawner` entities
//! ([`OminousItemSpawner`], ticked by [`tick_item_spawner`]).

use std::collections::HashMap;

use serde_json::Value;
use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_config::GameMode;
use vanillite_data::block::BlockState;
use vanillite_nbt::{Compound, List, Tag};
use vanillite_world::entity::EntityId;

use super::sound_id;
use super::spawner::{
    SpawnData, SpawnPotentials, center, create_mob, custom_rules_valid, entity_place_event,
    fixed_pos, mob_spawn_checks, no_collision_at, random_spawn_pos, spawner_spawn_rules,
};
use crate::effect::{EffectInstance, MobEffect};
use crate::item::ItemStack;
use crate::level::block_entity::BlockEntity;
use crate::level::{Level, SOUND_BLOCKS as SOUND, update};
use crate::mob::{Kind, physics, spawn};
use crate::survival::rules::Difficulty;

/// `MAX_MOB_TRACKING_DISTANCE`.
const MAX_MOB_TRACKING_DISTANCE: i32 = 47;
/// `TIME_BETWEEN_EACH_EJECTION`.
const TIME_BETWEEN_EACH_EJECTION: i64 = 30;

/// `TrialSpawnerState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrialState {
    Inactive,
    WaitingForPlayers,
    Active,
    WaitingForRewardEjection,
    EjectingReward,
    Cooldown,
}

impl TrialState {
    const ALL: [TrialState; 6] = [
        TrialState::Inactive,
        TrialState::WaitingForPlayers,
        TrialState::Active,
        TrialState::WaitingForRewardEjection,
        TrialState::EjectingReward,
        TrialState::Cooldown,
    ];

    /// The property value.
    pub fn name(self) -> &'static str {
        match self {
            TrialState::Inactive => "inactive",
            TrialState::WaitingForPlayers => "waiting_for_players",
            TrialState::Active => "active",
            TrialState::WaitingForRewardEjection => "waiting_for_reward_ejection",
            TrialState::EjectingReward => "ejecting_reward",
            TrialState::Cooldown => "cooldown",
        }
    }

    /// The state of a block state (`INACTIVE` without the property).
    pub fn of(state: BlockState) -> TrialState {
        state
            .value("trial_spawner_state")
            .and_then(|v| TrialState::ALL.into_iter().find(|s| s.name() == v))
            .unwrap_or(TrialState::Inactive)
    }
}

/// `TrialSpawnerConfig`.
#[derive(Clone, Debug, PartialEq)]
pub struct TrialConfig {
    pub spawn_range: i32,
    pub total_mobs: f32,
    pub simultaneous_mobs: f32,
    pub total_mobs_added_per_player: f32,
    pub simultaneous_mobs_added_per_player: f32,
    pub ticks_between_spawn: i32,
    pub spawn_potentials: SpawnPotentials,
    /// Weighted loot table ids.
    pub loot_tables_to_eject: Vec<(String, i32)>,
    pub items_to_drop_when_ominous: String,
}

impl Default for TrialConfig {
    /// `TrialSpawnerConfig.DEFAULT`.
    fn default() -> Self {
        TrialConfig {
            spawn_range: 4,
            total_mobs: 6.0,
            simultaneous_mobs: 2.0,
            total_mobs_added_per_player: 2.0,
            simultaneous_mobs_added_per_player: 1.0,
            ticks_between_spawn: 40,
            spawn_potentials: SpawnPotentials::default(),
            loot_tables_to_eject: vec![
                ("minecraft:spawners/trial_chamber/consumables".to_owned(), 1),
                ("minecraft:spawners/trial_chamber/key".to_owned(), 1),
            ],
            items_to_drop_when_ominous:
                "minecraft:spawners/trial_chamber/items_to_drop_when_ominous".to_owned(),
        }
    }
}

fn num(tag: Option<&Tag>) -> Option<f64> {
    match tag? {
        Tag::Byte(v) => Some(f64::from(*v)),
        Tag::Short(v) => Some(f64::from(*v)),
        Tag::Int(v) => Some(f64::from(*v)),
        #[allow(clippy::cast_precision_loss)]
        Tag::Long(v) => Some(*v as f64),
        Tag::Float(v) => Some(f64::from(*v)),
        Tag::Double(v) => Some(*v),
        _ => None,
    }
}

#[allow(clippy::cast_possible_truncation)]
fn num_i32(tag: Option<&Tag>, default: i32) -> i32 {
    num(tag).map_or(default, |v| v as i32)
}

#[allow(clippy::cast_possible_truncation)]
fn num_f32(tag: Option<&Tag>, default: f32) -> f32 {
    num(tag).map_or(default, |v| v as f32)
}

impl TrialConfig {
    /// `TrialSpawnerConfig.DIRECT_CODEC` from NBT.
    pub fn from_nbt(c: &Compound) -> TrialConfig {
        let d = TrialConfig::default();
        let loot = match c.get("loot_tables_to_eject") {
            Some(Tag::List(l)) => {
                l.0.iter()
                    .filter_map(|t| {
                        let e = t.as_compound()?;
                        let id = e.get("data")?.as_str()?.to_owned();
                        Some((qualify(&id), num_i32(e.get("weight"), 1)))
                    })
                    .filter(|(_, w)| *w > 0)
                    .collect()
            }
            _ => d.loot_tables_to_eject,
        };
        TrialConfig {
            spawn_range: num_i32(c.get("spawn_range"), d.spawn_range),
            total_mobs: num_f32(c.get("total_mobs"), d.total_mobs),
            simultaneous_mobs: num_f32(c.get("simultaneous_mobs"), d.simultaneous_mobs),
            total_mobs_added_per_player: num_f32(
                c.get("total_mobs_added_per_player"),
                d.total_mobs_added_per_player,
            ),
            simultaneous_mobs_added_per_player: num_f32(
                c.get("simultaneous_mobs_added_per_player"),
                d.simultaneous_mobs_added_per_player,
            ),
            ticks_between_spawn: num_i32(c.get("ticks_between_spawn"), d.ticks_between_spawn),
            spawn_potentials: SpawnPotentials::from_nbt(c.get("spawn_potentials"))
                .unwrap_or_default(),
            loot_tables_to_eject: loot,
            items_to_drop_when_ominous: c
                .get("items_to_drop_when_ominous")
                .and_then(Tag::as_str)
                .map_or(d.items_to_drop_when_ominous, qualify),
        }
    }

    /// `TrialSpawnerConfig.CODEC` to NBT (every field written).
    pub fn to_nbt(&self) -> Compound {
        let mut c = Compound::new();
        c.insert("spawn_range", self.spawn_range);
        c.insert("total_mobs", self.total_mobs);
        c.insert("simultaneous_mobs", self.simultaneous_mobs);
        c.insert(
            "total_mobs_added_per_player",
            self.total_mobs_added_per_player,
        );
        c.insert(
            "simultaneous_mobs_added_per_player",
            self.simultaneous_mobs_added_per_player,
        );
        c.insert("ticks_between_spawn", self.ticks_between_spawn);
        c.insert("spawn_potentials", self.spawn_potentials.to_nbt());
        let loot: Vec<Tag> = self
            .loot_tables_to_eject
            .iter()
            .map(|(id, weight)| {
                let mut e = Compound::new();
                e.insert("data", Tag::String(id.clone()));
                e.insert("weight", *weight);
                Tag::Compound(e)
            })
            .collect();
        c.insert("loot_tables_to_eject", Tag::List(vanillite_nbt::List(loot)));
        c.insert(
            "items_to_drop_when_ominous",
            Tag::String(self.items_to_drop_when_ominous.clone()),
        );
        c
    }

    /// `withSpawning(type)`: only `type` spawns (`SpawnData` with just the entity id).
    #[must_use]
    pub fn with_spawning(mut self, entity: &str) -> TrialConfig {
        let mut e = Compound::new();
        e.insert("id", Tag::String(entity.to_owned()));
        let mut data = Compound::new();
        data.insert("entity", e);
        self.spawn_potentials = SpawnPotentials(vec![(SpawnData(data), 1)]);
        self
    }

    /// A registry entry (`data/<ns>/trial_spawner/**.json`).
    pub fn from_json(json: &Value) -> TrialConfig {
        match json_to_nbt(json) {
            Tag::Compound(c) => TrialConfig::from_nbt(&c),
            _ => TrialConfig::default(),
        }
    }

    /// `calculateTargetTotalMobs`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn target_total_mobs(&self, additional_players: i32) -> i32 {
        (self.total_mobs + self.total_mobs_added_per_player * additional_players as f32).floor()
            as i32
    }

    /// `calculateTargetSimultaneousMobs`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn target_simultaneous_mobs(&self, additional_players: i32) -> i32 {
        (self.simultaneous_mobs
            + self.simultaneous_mobs_added_per_player * additional_players as f32)
            .floor() as i32
    }

    /// `ticksBetweenItemSpawners`.
    pub fn ticks_between_item_spawners(&self) -> i64 {
        160
    }
}

fn qualify(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// JSON of a registry entry as NBT (numbers become ints or doubles).
fn json_to_nbt(json: &Value) -> Tag {
    match json {
        Value::Null => Tag::Byte(0),
        Value::Bool(b) => Tag::Byte(i8::from(*b)),
        Value::Number(n) => match n.as_i64().and_then(|v| i32::try_from(v).ok()) {
            Some(v) => Tag::Int(v),
            None => Tag::Double(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => Tag::String(s.clone()),
        Value::Array(a) => Tag::List(List(a.iter().map(json_to_nbt).collect())),
        Value::Object(o) => {
            let mut c = Compound::new();
            for (k, v) in o {
                c.insert(k.as_str(), json_to_nbt(v));
            }
            Tag::Compound(c)
        }
    }
}

/// The parsed `minecraft:trial_spawner` registry, by id.
pub type TrialConfigs = HashMap<String, TrialConfig>;

/// A `Holder<TrialSpawnerConfig>`: a registry id or an inline configuration.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigRef {
    Named(String),
    Inline(Box<TrialConfig>, Compound),
}

impl Default for ConfigRef {
    fn default() -> Self {
        ConfigRef::Inline(Box::default(), Compound::new())
    }
}

impl ConfigRef {
    fn load(tag: Option<&Tag>) -> ConfigRef {
        match tag {
            Some(Tag::String(id)) => ConfigRef::Named(qualify(id)),
            Some(Tag::Compound(c)) => {
                ConfigRef::Inline(Box::new(TrialConfig::from_nbt(c)), c.clone())
            }
            _ => ConfigRef::default(),
        }
    }

    fn save(&self) -> Tag {
        match self {
            ConfigRef::Named(id) => Tag::String(id.clone()),
            ConfigRef::Inline(_, c) => Tag::Compound(c.clone()),
        }
    }

    /// The configuration; unknown ids fall back to the default.
    pub fn get(&self) -> TrialConfig {
        match self {
            ConfigRef::Inline(c, _) => (**c).clone(),
            ConfigRef::Named(id) => crate::player::play::game_data()
                .and_then(|d| d.trial_spawner_configs.get(id).cloned())
                .unwrap_or_default(),
        }
    }
}

/// The block entity of a trial spawner: `TrialSpawner.FullConfig` and `TrialSpawnerStateData`.
#[derive(Clone, Debug, PartialEq)]
pub struct TrialSpawner {
    pub normal_config: ConfigRef,
    pub ominous_config: ConfigRef,
    pub target_cooldown_length: i32,
    pub required_player_range: i32,
    /// `detectedPlayers` (UUIDs), in detection order.
    pub detected_players: Vec<u128>,
    /// `currentMobs`: UUIDs with the entity once it was found.
    pub current_mobs: Vec<(u128, Option<EntityId>)>,
    pub cooldown_ends_at: i64,
    pub next_mob_spawns_at: i64,
    pub total_mobs_spawned: i32,
    pub next_spawn_data: Option<SpawnData>,
    pub ejecting_loot_table: Option<String>,
    /// `displayEntity != null`.
    pub(crate) display: bool,
    /// `isOminous`, from the block state.
    pub(crate) ominous: bool,
    /// `dispensing`: the items to drop when ominous with their weights, rolled once (not
    /// saved).
    pub(crate) dispensing: Option<Vec<(ItemStack, i32)>>,
}

impl Default for TrialSpawner {
    fn default() -> Self {
        TrialSpawner {
            normal_config: ConfigRef::default(),
            ominous_config: ConfigRef::default(),
            target_cooldown_length: 36000,
            required_player_range: 14,
            detected_players: Vec::new(),
            current_mobs: Vec::new(),
            cooldown_ends_at: 0,
            next_mob_spawns_at: 0,
            total_mobs_spawned: 0,
            next_spawn_data: None,
            ejecting_loot_table: None,
            display: false,
            ominous: false,
            dispensing: None,
        }
    }
}

pub(crate) fn uuid_tag(uuid: u128) -> Tag {
    #[allow(clippy::cast_possible_truncation)]
    let parts = [
        (uuid >> 96) as u32 as i32,
        (uuid >> 64) as u32 as i32,
        (uuid >> 32) as u32 as i32,
        uuid as u32 as i32,
    ];
    Tag::IntArray(parts.to_vec())
}

pub(crate) fn uuid_of(tag: &Tag) -> Option<u128> {
    match tag {
        Tag::IntArray(a) if a.len() == 4 => Some(
            a.iter()
                .fold(0u128, |acc, &p| (acc << 32) | u128::from(p.cast_unsigned())),
        ),
        _ => None,
    }
}

/// A UUID set (`UUIDUtil.CODEC_SET`) from NBT.
pub fn uuid_list(tag: Option<&Tag>) -> Vec<u128> {
    let mut out = Vec::new();
    if let Some(Tag::List(l)) = tag {
        for u in l.0.iter().filter_map(uuid_of) {
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

/// A UUID set as NBT.
pub fn uuid_list_tag(uuids: impl Iterator<Item = u128>) -> Tag {
    Tag::List(List(uuids.map(uuid_tag).collect()))
}

impl TrialSpawner {
    /// `TrialSpawner.load` (state data and configuration).
    pub fn load(c: &Compound) -> TrialSpawner {
        let long = |k: &str| c.get(k).and_then(Tag::as_i64).unwrap_or(0);
        TrialSpawner {
            normal_config: ConfigRef::load(c.get("normal_config")),
            ominous_config: ConfigRef::load(c.get("ominous_config")),
            target_cooldown_length: num_i32(c.get("target_cooldown_length"), 36000).max(0),
            required_player_range: num_i32(c.get("required_player_range"), 14).clamp(1, 128),
            detected_players: uuid_list(c.get("registered_players")),
            current_mobs: uuid_list(c.get("current_mobs"))
                .into_iter()
                .map(|u| (u, None))
                .collect(),
            cooldown_ends_at: long("cooldown_ends_at"),
            next_mob_spawns_at: long("next_mob_spawns_at"),
            total_mobs_spawned: num_i32(c.get("total_mobs_spawned"), 0).max(0),
            next_spawn_data: c
                .get("spawn_data")
                .and_then(Tag::as_compound)
                .map(|d| SpawnData::from_nbt(d.clone())),
            ejecting_loot_table: c
                .get("ejecting_loot_table")
                .and_then(Tag::as_str)
                .map(qualify),
            display: false,
            ominous: false,
            dispensing: None,
        }
    }

    /// `TrialSpawner.store`: the fields that differ from their defaults.
    pub fn save(&self, out: &mut Compound) {
        if !self.detected_players.is_empty() {
            out.insert(
                "registered_players",
                uuid_list_tag(self.detected_players.iter().copied()),
            );
        }
        if !self.current_mobs.is_empty() {
            out.insert(
                "current_mobs",
                uuid_list_tag(self.current_mobs.iter().map(|(u, _)| *u)),
            );
        }
        if self.cooldown_ends_at != 0 {
            out.insert("cooldown_ends_at", self.cooldown_ends_at);
        }
        if self.next_mob_spawns_at != 0 {
            out.insert("next_mob_spawns_at", self.next_mob_spawns_at);
        }
        if self.total_mobs_spawned != 0 {
            out.insert("total_mobs_spawned", self.total_mobs_spawned);
        }
        if let Some(d) = &self.next_spawn_data {
            out.insert("spawn_data", d.0.clone());
        }
        if let Some(t) = &self.ejecting_loot_table {
            out.insert("ejecting_loot_table", Tag::String(t.clone()));
        }
        if self.normal_config != ConfigRef::default() {
            out.insert("normal_config", self.normal_config.save());
        }
        if self.ominous_config != ConfigRef::default() {
            out.insert("ominous_config", self.ominous_config.save());
        }
        if self.target_cooldown_length != 36000 {
            out.insert("target_cooldown_length", self.target_cooldown_length);
        }
        if self.required_player_range != 14 {
            out.insert("required_player_range", self.required_player_range);
        }
    }

    /// `TrialSpawnerStateData.getUpdateTag(state)`.
    pub fn update_tag(&self, state: TrialState) -> Compound {
        let mut c = Compound::new();
        if state == TrialState::Active {
            c.insert("next_mob_spawns_at", self.next_mob_spawns_at);
        }
        if let Some(d) = &self.next_spawn_data {
            c.insert("spawn_data", d.0.clone());
        }
        c
    }

    /// `activeConfig`.
    pub fn active_config(&self) -> TrialConfig {
        if self.ominous {
            self.ominous_config.get()
        } else {
            self.normal_config.get()
        }
    }

    /// `resetStatistics`.
    pub fn reset_statistics(&mut self) {
        self.detected_players.clear();
        self.total_mobs_spawned = 0;
        self.next_mob_spawns_at = 0;
        self.cooldown_ends_at = 0;
    }

    /// `TrialSpawner.overrideEntityToSpawn(type)` without the state change: both
    /// configurations spawn only `entity`, and the data is reset.
    pub fn override_entity(&mut self, entity: &str) {
        self.reset();
        for config in [&mut self.normal_config, &mut self.ominous_config] {
            let c = config.get().with_spawning(entity);
            let nbt = c.to_nbt();
            *config = ConfigRef::Inline(Box::new(c), nbt);
        }
    }

    /// `reset`.
    pub fn reset(&mut self) {
        self.current_mobs.clear();
        self.next_spawn_data = None;
        self.reset_statistics();
    }

    /// `countAdditionalPlayers`.
    pub fn additional_players(&self) -> i32 {
        i32::try_from(self.detected_players.len().saturating_sub(1)).unwrap_or(i32::MAX)
    }

    /// `hasFinishedSpawningAllMobs`.
    pub fn finished_spawning(&self, config: &TrialConfig, additional: i32) -> bool {
        self.total_mobs_spawned >= config.target_total_mobs(additional)
    }

    /// `isReadyToSpawnNextMob`.
    pub fn ready_to_spawn(&self, game_time: i64, config: &TrialConfig, additional: i32) -> bool {
        game_time >= self.next_mob_spawns_at
            && i32::try_from(self.current_mobs.len()).unwrap_or(i32::MAX)
                < config.target_simultaneous_mobs(additional)
    }

    /// `isReadyToOpenShutter`.
    #[allow(clippy::cast_precision_loss)]
    pub fn ready_to_open_shutter(&self, game_time: i64) -> bool {
        let started = self.cooldown_ends_at - i64::from(self.target_cooldown_length);
        game_time as f32 >= started as f32 + 40.0
    }

    /// `isReadyToEjectItems`.
    #[allow(clippy::cast_precision_loss)]
    pub fn ready_to_eject(&self, game_time: i64) -> bool {
        let started = self.cooldown_ends_at - i64::from(self.target_cooldown_length);
        (game_time - started) as f32 % TIME_BETWEEN_EACH_EJECTION as f32 == 0.0
    }
}

fn spawner_mut(level: &mut Level, pos: BlockPos) -> Option<&mut TrialSpawner> {
    match level.block_entity_mut(pos) {
        Some(BlockEntity::TrialSpawner(s)) => Some(s),
        _ => None,
    }
}

/// `markUpdated`: the clients get the block entity's update tag.
fn mark_updated(level: &mut Level, pos: BlockPos) {
    level.send_block_entity_updated(pos);
}

/// `getOrCreateNextSpawnData`.
fn next_spawn_data(level: &mut Level, pos: BlockPos) -> Option<SpawnData> {
    let s = spawner_mut(level, pos)?;
    if let Some(d) = &s.next_spawn_data {
        return Some(d.clone());
    }
    let potentials = s.active_config().spawn_potentials;
    let selected = if potentials.is_empty() {
        None
    } else {
        potentials.get_random(level.random()).cloned()
    };
    let data = selected.unwrap_or_default();
    if let Some(s) = spawner_mut(level, pos) {
        s.next_spawn_data = Some(data.clone());
    }
    mark_updated(level, pos);
    Some(data)
}

/// `hasMobToSpawn`.
fn has_mob_to_spawn(level: &mut Level, pos: BlockPos) -> bool {
    let has_id = next_spawn_data(level, pos).is_some_and(|d| d.entity_name().is_some());
    has_id
        || spawner_mut(level, pos).is_some_and(|s| !s.active_config().spawn_potentials.is_empty())
}

/// `canSpawnInLevel`.
fn can_spawn_in_level(level: &Level) -> bool {
    let env = &level.spawner_env;
    env.spawner_blocks_work && env.difficulty != Difficulty::Peaceful && env.spawn_mobs
}

/// `BlockPos.asLong`.
fn pos_long(pos: BlockPos) -> i64 {
    ((i64::from(pos.x) & 0x3FF_FFFF) << 38)
        | ((i64::from(pos.z) & 0x3FF_FFFF) << 12)
        | (i64::from(pos.y) & 0xFFF)
}

/// The eye height of a player of the given box height (`Player` poses).
fn player_eye(height: f32) -> f64 {
    let eye: f32 = if height >= 1.8 {
        1.62
    } else if height >= 1.5 {
        1.27
    } else if height >= 0.6 {
        0.4
    } else {
        0.2
    };
    f64::from(eye)
}

/// `inLineOfSight(level, origin, dest)`: the ray from `dest` hits nothing before the origin's
/// block.
pub fn in_line_of_sight(level: &Level, origin: Vec3, dest: Vec3) -> bool {
    let origin_block = BlockPos::containing(origin.x, origin.y, origin.z);
    physics::clip(level, dest, origin).is_none_or(|(hit, _)| hit == origin_block)
}

/// A detected player: entity, UUID.
pub type Detected = (EntityId, u128);

/// `PlayerDetector.NO_CREATIVE_PLAYERS` / `INCLUDING_CREATIVE_PLAYERS`: players whose block is
/// closer than `range` to `pos`, not spectators (and not creative unless `creative`), with a
/// line of sight from the block's centre to their eyes when `sight`.
pub fn detect_players(
    level: &Level,
    pos: BlockPos,
    range: f64,
    creative: bool,
    sight: bool,
    out: &mut Vec<Detected>,
) {
    out.clear();
    for (id, p) in level.entities.players.iter() {
        if p.game_mode == GameMode::Spectator || !creative && p.game_mode == GameMode::Creative {
            continue;
        }
        let Some(e) = level.entities.store.get(id) else {
            continue;
        };
        let at = e.pos();
        let block = BlockPos::containing(at.x, at.y, at.z);
        if block.dist_sqr(pos) >= range * range {
            continue;
        }
        let eye = Vec3::new(at.x, at.y + player_eye(e.dims().height), at.z);
        if sight && !in_line_of_sight(level, center(pos), eye) {
            continue;
        }
        out.push((id, e.uuid()));
    }
}

/// `findPlayerWithOminousEffect`: a player with the trial omen, else the last one with a bad
/// omen.
fn player_with_ominous_effect(level: &Level, players: &[Detected]) -> Option<(EntityId, bool)> {
    let mut bad_omen = None;
    for &(id, _) in players {
        let Some(p) = level.entities.player(id) else {
            continue;
        };
        if p.survival.effects.has(MobEffect::TRIAL_OMEN) {
            return Some((id, true));
        }
        if p.survival.effects.has(MobEffect::BAD_OMEN) {
            bad_omen = Some((id, false));
        }
    }
    bad_omen
}

/// `transformBadOmenIntoTrialOmen`.
fn bad_omen_to_trial_omen(level: &mut Level, player: EntityId) {
    let Some(amplifier) = level
        .entities
        .player(player)
        .and_then(|p| p.survival.effects.amplifier(MobEffect::BAD_OMEN))
    else {
        return;
    };
    let amplifier = amplifier + 1;
    let duration = 18000 * amplifier;
    if let Some(p) = level.entities.player_mut(player) {
        p.survival.effects.remove(MobEffect::BAD_OMEN);
    }
    level.entities.add_effect(
        player,
        EffectInstance::new(MobEffect::TRIAL_OMEN, duration, 0),
        true,
    );
}

/// `tryDetectPlayers`.
fn try_detect_players(level: &mut Level, pos: BlockPos, state: TrialState) {
    let game_time = level.game_time();
    if pos_long(pos).wrapping_add(game_time) % 20 != 0 {
        return;
    }
    let Some(s) = spawner_mut(level, pos) else {
        return;
    };
    let ominous = s.ominous;
    if state == TrialState::Cooldown && ominous {
        return;
    }
    let range = f64::from(s.required_player_range);
    let mut in_sight = Vec::new();
    detect_players(level, pos, range, false, true, &mut in_sight);
    let mut became_ominous = false;
    if !ominous
        && !in_sight.is_empty()
        && let Some((player, trial_omen)) = player_with_ominous_effect(level, &in_sight)
    {
        if !trial_omen {
            bad_omen_to_trial_omen(level, player);
        }
        if let Some(e) = level.entities.store.get(player) {
            let at = e.pos();
            let eye = BlockPos::containing(at.x, at.y + player_eye(e.dims().height), at.z);
            level.level_event(None, 3020, eye, 0);
        }
        apply_ominous(level, pos);
        became_ominous = true;
    }
    if state != TrialState::Cooldown || became_ominous {
        let Some(s) = spawner_mut(level, pos) else {
            return;
        };
        let found = if s.detected_players.is_empty() {
            in_sight
        } else {
            let mut all = Vec::new();
            detect_players(level, pos, range, false, false, &mut all);
            all
        };
        let Some(s) = spawner_mut(level, pos) else {
            return;
        };
        let mut added = false;
        for (_, uuid) in found {
            if !s.detected_players.contains(&uuid) {
                s.detected_players.push(uuid);
                added = true;
            }
        }
        if added {
            s.next_mob_spawns_at = (game_time + 40).max(s.next_mob_spawns_at);
            let count = i32::try_from(s.detected_players.len()).unwrap_or(i32::MAX);
            let ominous = s.ominous;
            if !became_ominous {
                level.level_event(None, if ominous { 3019 } else { 3013 }, pos, count);
            }
        }
    }
}

/// `applyOminous` with `resetAfterBecomingOminous`.
fn apply_ominous(level: &mut Level, pos: BlockPos) {
    let state = level.block(pos);
    if let Some(ominous) = state.with("ominous", "true") {
        level.set_block(pos, ominous, update::ALL);
    }
    level.level_event(None, 3020, pos, 1);
    let game_time = level.game_time();
    let Some(s) = spawner_mut(level, pos) else {
        return;
    };
    s.ominous = true;
    let mobs: Vec<(u128, Option<EntityId>)> = std::mem::take(&mut s.current_mobs);
    let ominous_config = s.ominous_config.get();
    if !ominous_config.spawn_potentials.is_empty() {
        s.next_spawn_data = None;
    }
    s.total_mobs_spawned = 0;
    s.next_mob_spawns_at = game_time + i64::from(ominous_config.ticks_between_spawn);
    s.cooldown_ends_at = game_time + ominous_config.ticks_between_item_spawners();
    for (uuid, id) in mobs {
        let Some(id) = id.or_else(|| find_mob(level, pos, uuid)) else {
            continue;
        };
        if let Some(e) = level.entities.store.get(id) {
            let at = e.pos();
            level.level_event(None, 3012, BlockPos::containing(at.x, at.y, at.z), 0);
            // `Mob.dropPreservedEquipment`: what the mob picked up drops.
            let mut preserved = Vec::new();
            if let Some(m) = level.entities.mob_mut(id) {
                for slot in crate::mob::equipment::SLOTS {
                    if m.equipment.drop_chance(slot) > 1.0 && !m.equipment.get(slot).is_empty() {
                        preserved.push(std::mem::take(
                            &mut m.equipment.items[usize::from(slot.ordinal())],
                        ));
                    }
                }
            }
            for stack in preserved {
                super::drops::spawn_item_moving(level, at, stack, None, 10, None);
            }
            level.entities.despawn(id);
        }
    }
    mark_updated(level, pos);
}

/// `removeOminous`.
fn remove_ominous(level: &mut Level, pos: BlockPos) {
    let state = level.block(pos);
    if let Some(normal) = state.with("ominous", "false") {
        level.set_block(pos, normal, update::ALL);
    }
    if let Some(s) = spawner_mut(level, pos) {
        s.ominous = false;
    }
}

/// The loaded entity with `uuid` within the tracking distance of the spawner.
fn find_mob(level: &Level, pos: BlockPos, uuid: u128) -> Option<EntityId> {
    let r = f64::from(MAX_MOB_TRACKING_DISTANCE + 1);
    let c = center(pos);
    let area = Aabb::new(c.x - r, c.y - r, c.z - r, c.x + r, c.y + r, c.z + r);
    let mut found = None;
    level.entities.store.for_each_in(&area, false, |id| {
        if found.is_none()
            && level
                .entities
                .store
                .get(id)
                .is_some_and(|e| e.uuid() == uuid)
        {
            found = Some(id);
        }
    });
    found
}

/// `shouldMobBeUntracked`: gone, dead or farther than 47 blocks.
fn mob_untracked(level: &Level, pos: BlockPos, uuid: u128, id: Option<EntityId>) -> bool {
    let Some(id) = id else {
        return true;
    };
    let Some(e) = level.entities.store.get(id) else {
        return true;
    };
    if e.uuid() != uuid {
        return true;
    }
    if level.entities.mob(id).is_some_and(|m| !m.is_alive()) {
        return true;
    }
    let at = e.pos();
    let max = f64::from(MAX_MOB_TRACKING_DISTANCE);
    BlockPos::containing(at.x, at.y, at.z).dist_sqr(pos) > max * max
}

/// `TrialSpawner.spawnMob`: the UUID and entity of the spawned mob.
fn spawn_mob(level: &mut Level, pos: BlockPos) -> Option<(u128, EntityId)> {
    let data = next_spawn_data(level, pos)?;
    let s = spawner_mut(level, pos)?;
    let config = s.active_config();
    let ominous = s.ominous;
    let type_id = data.entity_type()?;
    let spawn_pos = fixed_pos(&data)
        .unwrap_or_else(|| random_spawn_pos(level.random(), pos, config.spawn_range));
    if !no_collision_at(level, type_id, spawn_pos) {
        return None;
    }
    if !in_line_of_sight(level, center(pos), spawn_pos) {
        return None;
    }
    let block = BlockPos::containing(spawn_pos.x, spawn_pos.y, spawn_pos.z);
    if let Some(kind) = Kind::of(type_id)
        && !spawner_spawn_rules(level, kind, block, true)
    {
        return None;
    }
    if let Some(rules) = data.custom_spawn_rules()
        && !custom_rules_valid(level, rules, block)
    {
        return None;
    }
    let (mut mob, at) = create_mob(level, &data, spawn_pos)?;
    let y_rot = level.random().next_float() * 360.0;
    if !mob_spawn_checks(level, &mob, at, false) {
        return None;
    }
    let extra = if data.has_no_configuration() {
        super::spawner::finalize_spawner_mob(
            level,
            &mut mob,
            at,
            crate::mob::fam::SpawnReason::TrialSpawner,
        )
    } else {
        Vec::new()
    };
    mob.persistent = true;
    // `nextSpawnData.getEquipment().ifPresent(mob::equip)`.
    if let Some((table, chances)) = data.equipment() {
        let table = table.to_owned();
        super::spawner::equip_from_table(level, &mut mob, at, &table, &chances);
    }
    let id = spawn::add_mob_with(
        level,
        mob,
        at,
        y_rot,
        extra,
        crate::mob::fam::SpawnReason::TrialSpawner,
    );
    // `Entity.load` turned head and body to the loaded yaw (0 without `Rotation`); the random
    // yaw of `snapTo` turns the entity only.
    if let Some(m) = level.entities.mob_mut(id) {
        m.y_body_rot = 0.0;
    }
    if let Some(mut e) = level.entities.store.get_mut(id) {
        let rot = e.as_ref().rot();
        e.set_rot(vanillite_world::entity::Rotation {
            y_head_rot: 0.0,
            ..rot
        });
    }
    let uuid = level.entities.store.get(id)?.uuid();
    let flame = i32::from(ominous);
    level.level_event(None, 3011, pos, flame);
    level.level_event(None, 3012, block, flame);
    entity_place_event(level, id, block);
    Some((uuid, id))
}

/// `DefaultDispenseItemBehavior.spawnItem(level, stack, accuracy, UP, position)`.
pub fn spawn_item_up(level: &mut Level, stack: ItemStack, accuracy: i32, at: Vec3) {
    if stack.is_empty() {
        return;
    }
    let at = Vec3::new(at.x, at.y - 0.125, at.z);
    // `new ItemEntity(level, x, y, z, stack)` draws its motion and yaw from its own random.
    let Some(id) = super::drops::spawn_item_moving(level, at, stack, None, 0, None) else {
        return;
    };
    let r = level.random();
    let pow = r.next_double() * 0.1 + 0.2;
    let spread = 0.017_227_5 * f64::from(accuracy);
    let step = Direction::Up.normal();
    let vx = r.triangle(f64::from(step[0]) * pow, spread);
    let vy = r.triangle(0.2, spread);
    let vz = r.triangle(f64::from(step[2]) * pow, spread);
    if let Some(mut e) = level.entities.store.get_mut(id) {
        e.set_velocity(Vec3::new(vx, vy, vz));
    }
}

/// `ejectReward`: the items of a loot table out of the top, then level event 3014.
fn eject_reward(level: &mut Level, pos: BlockPos, table: &str) {
    let Some(data) = crate::player::play::game_data() else {
        return;
    };
    let mut items = Vec::new();
    // `LootContext.Builder.create`: the table's random sequence, else the level random.
    match data.loot.random_sequence(table) {
        Some(key) => {
            level.with_random_sequence(key, |r| data.random_items(table, None, r, &mut items));
        }
        None => data.random_items(table, None, level.random(), &mut items),
    }
    if items.is_empty() {
        return;
    }
    let at = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 1.2,
        f64::from(pos.z) + 0.5,
    );
    for item in items {
        spawn_item_up(level, item, 2, at);
    }
    level.level_event(None, 3014, pos, 0);
}

/// `TrialSpawnerBlockEntity.setEntityId` (a spawn egg used on it): the override, then
/// `setState(INACTIVE)`.
pub fn set_entity_id(level: &mut Level, pos: BlockPos, entity: &str) {
    let Some(s) = spawner_mut(level, pos) else {
        return;
    };
    s.override_entity(entity);
    set_state(level, pos, TrialState::Inactive);
}

/// `setState`.
fn set_state(level: &mut Level, pos: BlockPos, state: TrialState) {
    let block = level.block(pos);
    if let Some(next) = block.with("trial_spawner_state", state.name()) {
        level.set_block(pos, next, update::ALL);
    }
}

/// `TrialSpawnerState.tickAndGetNext`.
fn tick_and_get_next(level: &mut Level, pos: BlockPos, state: TrialState) -> TrialState {
    let game_time = level.game_time();
    let Some(s) = spawner_mut(level, pos) else {
        return state;
    };
    let config = s.active_config();
    match state {
        TrialState::Inactive => {
            // `getOrCreateDisplayEntity(..., WAITING_FOR_PLAYERS)`.
            if !s.display {
                let data = next_spawn_data(level, pos);
                let has = data.is_some_and(|d| d.entity_type().is_some());
                if has {
                    // `EntityType.loadEntityRecursive` of the display entity: it takes the
                    // next entity id, and its random the next seed of the stream.
                    let _ = level.network_ids().next();
                    let _ = crate::mob::new_entity_seed();
                }
                if let Some(s) = spawner_mut(level, pos) {
                    s.display = has;
                }
            }
            if spawner_mut(level, pos).is_some_and(|s| s.display) {
                TrialState::WaitingForPlayers
            } else {
                state
            }
        }
        TrialState::WaitingForPlayers => {
            if !can_spawn_in_level(level) {
                if let Some(s) = spawner_mut(level, pos) {
                    s.reset_statistics();
                }
                state
            } else if !has_mob_to_spawn(level, pos) {
                TrialState::Inactive
            } else {
                try_detect_players(level, pos, state);
                if spawner_mut(level, pos).is_some_and(|s| s.detected_players.is_empty()) {
                    state
                } else {
                    TrialState::Active
                }
            }
        }
        TrialState::Active => {
            if !can_spawn_in_level(level) {
                if let Some(s) = spawner_mut(level, pos) {
                    s.reset_statistics();
                }
                return TrialState::WaitingForPlayers;
            }
            if !has_mob_to_spawn(level, pos) {
                return TrialState::Inactive;
            }
            let additional = spawner_mut(level, pos).map_or(0, |s| s.additional_players());
            try_detect_players(level, pos, state);
            if spawner_mut(level, pos).is_some_and(|s| s.ominous) {
                spawn_ominous_item_spawner(level, pos);
            }
            let Some(s) = spawner_mut(level, pos) else {
                return state;
            };
            if s.finished_spawning(&config, additional) {
                if s.current_mobs.is_empty() {
                    s.cooldown_ends_at = game_time + i64::from(s.target_cooldown_length);
                    s.total_mobs_spawned = 0;
                    s.next_mob_spawns_at = 0;
                    return TrialState::WaitingForRewardEjection;
                }
            } else if s.ready_to_spawn(game_time, &config, additional)
                && let Some((uuid, id)) = spawn_mob(level, pos)
            {
                let next = config.spawn_potentials.get_random(level.random()).cloned();
                if let Some(s) = spawner_mut(level, pos) {
                    s.current_mobs.push((uuid, Some(id)));
                    s.total_mobs_spawned += 1;
                    s.next_mob_spawns_at = game_time + i64::from(config.ticks_between_spawn);
                    if let Some(next) = next {
                        s.next_spawn_data = Some(next);
                    }
                }
                mark_updated(level, pos);
            }
            state
        }
        TrialState::WaitingForRewardEjection => {
            if s.ready_to_open_shutter(game_time) {
                level.play_block_sound(
                    None,
                    pos,
                    sound_id("block.trial_spawner.open_shutter"),
                    SOUND,
                    1.0,
                    1.0,
                );
                TrialState::EjectingReward
            } else {
                state
            }
        }
        TrialState::EjectingReward => {
            if !s.ready_to_eject(game_time) {
                return state;
            }
            if s.detected_players.is_empty() {
                s.ejecting_loot_table = None;
                level.play_block_sound(
                    None,
                    pos,
                    sound_id("block.trial_spawner.close_shutter"),
                    SOUND,
                    1.0,
                    1.0,
                );
                return TrialState::Cooldown;
            }
            let table = match s.ejecting_loot_table.clone() {
                Some(t) => Some(t),
                None => {
                    let total: i32 = config.loot_tables_to_eject.iter().map(|(_, w)| *w).sum();
                    let picked = if total > 0 {
                        let mut roll = level.random().next_int_bounded(total);
                        config
                            .loot_tables_to_eject
                            .iter()
                            .find(|(_, w)| {
                                let hit = roll < *w;
                                roll -= w;
                                hit
                            })
                            .map(|(t, _)| t.clone())
                    } else {
                        None
                    };
                    if let Some(s) = spawner_mut(level, pos) {
                        s.ejecting_loot_table.clone_from(&picked);
                    }
                    picked
                }
            };
            if let Some(table) = table {
                eject_reward(level, pos, &table);
            }
            if let Some(s) = spawner_mut(level, pos)
                && !s.detected_players.is_empty()
            {
                s.detected_players.remove(0);
            }
            state
        }
        TrialState::Cooldown => {
            try_detect_players(level, pos, state);
            let Some(s) = spawner_mut(level, pos) else {
                return state;
            };
            if !s.detected_players.is_empty() {
                s.total_mobs_spawned = 0;
                s.next_mob_spawns_at = 0;
                TrialState::Active
            } else if game_time >= s.cooldown_ends_at {
                remove_ominous(level, pos);
                if let Some(s) = spawner_mut(level, pos) {
                    s.reset();
                }
                TrialState::WaitingForPlayers
            } else {
                state
            }
        }
    }
}

/// `TrialSpawnerBlock`'s server ticker (`TrialSpawner.tickServer`).
pub fn tick(level: &mut Level, pos: BlockPos, block: BlockState) {
    let ominous = block.value("ominous") == Some("true");
    let game_time = level.game_time();
    let Some(s) = spawner_mut(level, pos) else {
        return;
    };
    s.ominous = ominous;
    let ticks_between = i64::from(s.active_config().ticks_between_spawn);
    let mobs = std::mem::take(&mut s.current_mobs);
    let mut kept = Vec::with_capacity(mobs.len());
    let mut removed = false;
    for (uuid, id) in mobs {
        let id = id.or_else(|| find_mob(level, pos, uuid));
        if mob_untracked(level, pos, uuid, id) {
            removed = true;
        } else {
            kept.push((uuid, id));
        }
    }
    let Some(s) = spawner_mut(level, pos) else {
        return;
    };
    s.current_mobs = kept;
    if removed {
        s.next_mob_spawns_at = game_time + ticks_between;
    }
    let state = TrialState::of(block);
    let next = tick_and_get_next(level, pos, state);
    if next != state {
        set_state(level, pos, next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mob_targets_grow_with_players() {
        let c = TrialConfig::default();
        assert_eq!(c.target_total_mobs(0), 6);
        assert_eq!(c.target_total_mobs(2), 10);
        assert_eq!(c.target_simultaneous_mobs(0), 2);
        assert_eq!(c.target_simultaneous_mobs(3), 5);
    }

    #[test]
    fn config_from_registry_json() {
        let json: Value = serde_json::from_str(
            r#"{"simultaneous_mobs": 1.0, "simultaneous_mobs_added_per_player": 0.5,
                "spawn_potentials": [{"data": {"entity": {"id": "minecraft:breeze"}}, "weight": 1}],
                "ticks_between_spawn": 20, "total_mobs": 2.0, "total_mobs_added_per_player": 1.0}"#,
        )
        .expect("json");
        let c = TrialConfig::from_json(&json);
        assert_eq!(c.ticks_between_spawn, 20);
        assert_eq!(c.target_simultaneous_mobs(1), 1);
        assert_eq!(c.target_total_mobs(1), 3);
        assert_eq!(c.spawn_potentials.0.len(), 1);
        assert_eq!(
            c.spawn_potentials.0[0].0.entity_name(),
            Some("minecraft:breeze")
        );
        assert_eq!(c.loot_tables_to_eject.len(), 2);
    }

    #[test]
    fn nbt_round_trip() {
        let mut s = TrialSpawner {
            normal_config: ConfigRef::Named("minecraft:trial_chamber/melee/zombie".into()),
            ..TrialSpawner::default()
        };
        s.detected_players
            .push(0x1234_5678_9abc_def0_1122_3344_5566_7788);
        s.total_mobs_spawned = 3;
        s.cooldown_ends_at = 99;
        let mut c = Compound::new();
        s.save(&mut c);
        let back = TrialSpawner::load(&c);
        assert_eq!(back.detected_players, s.detected_players);
        assert_eq!(back.total_mobs_spawned, 3);
        assert_eq!(back.cooldown_ends_at, 99);
        assert_eq!(back.normal_config, s.normal_config);
    }

    #[test]
    fn ejection_timing() {
        let s = TrialSpawner {
            cooldown_ends_at: 36_000 + 100,
            ..TrialSpawner::default()
        };
        // The cooldown started at 100: the shutter opens 40 ticks later, items every 30.
        assert!(!s.ready_to_open_shutter(139));
        assert!(s.ready_to_open_shutter(140));
        assert!(s.ready_to_eject(130));
        assert!(!s.ready_to_eject(131));
    }
}

// --- Ominous item spawners ---------------------------------------------------------------------

/// `OminousItemSpawner`: drops its item (or shoots it down when it is a projectile) once its
/// tick count reaches `spawn_item_after_ticks`, then dies.
#[derive(Clone, Debug, PartialEq)]
pub struct OminousItemSpawner {
    pub item: ItemStack,
    pub spawn_item_after_ticks: i64,
    /// `tickCount`.
    pub tick_count: i32,
}

/// `TrialSpawnerStateData.getDispensingItems`: the `items_to_drop_when_ominous` table rolled
/// once with the seed of the spawner's low resolution position, each stack weighted by its
/// count.
fn dispensing_items(level: &mut Level, pos: BlockPos) -> Vec<(ItemStack, i32)> {
    if let Some(d) = spawner_mut(level, pos).and_then(|s| s.dispensing.clone()) {
        return d;
    }
    let Some(table) = spawner_mut(level, pos).map(|s| s.active_config().items_to_drop_when_ominous)
    else {
        return Vec::new();
    };
    let Some(data) = crate::player::play::game_data() else {
        return Vec::new();
    };
    // `lowResolutionPosition`: the world seed plus the packed position divided by 30, 20, 30.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let low = BlockPos::new(
        (pos.x as f32 / 30.0).floor() as i32,
        (pos.y as f32 / 20.0).floor() as i32,
        (pos.z as f32 / 30.0).floor() as i32,
    );
    let seed = level.seed().wrapping_add(low.as_long());
    let mut rng = vanillite_base::random::LegacyRandom::new(seed);
    let mut drops = Vec::new();
    data.random_items(&table, None, &mut rng, &mut drops);
    if drops.is_empty() {
        return Vec::new();
    }
    let list: Vec<(ItemStack, i32)> = drops
        .into_iter()
        .map(|d| {
            let weight = d.count;
            (d.with_count(1), weight)
        })
        .collect();
    if let Some(s) = spawner_mut(level, pos) {
        s.dispensing = Some(list.clone());
    }
    list
}

/// `TrialSpawnerState.spawnOminousOminousItemSpawner`: every tick an item is picked from the
/// dispensing list (a level random draw); after the cooldown an item spawner appears above a
/// random tracked mob or detected player.
fn spawn_ominous_item_spawner(level: &mut Level, pos: BlockPos) {
    let items = dispensing_items(level, pos);
    let total: i32 = items.iter().map(|(_, w)| *w).sum();
    if total <= 0 {
        return;
    }
    let mut pick = level.random().next_int_bounded(total);
    let mut item = ItemStack::EMPTY;
    for (stack, w) in items {
        pick -= w;
        if pick < 0 {
            item = stack;
            break;
        }
    }
    if item.is_empty() {
        return;
    }
    let game_time = level.game_time();
    if spawner_mut(level, pos).is_none_or(|s| game_time < s.cooldown_ends_at) {
        return;
    }
    let Some(at) = position_to_spawn_item_spawner(level, pos) else {
        return;
    };
    // `new OminousItemSpawner`: the entity's random takes the next seed of the stream.
    let _ = crate::mob::new_entity_seed();
    let after = i64::from(level.random().next_int_between_inclusive(60, 120));
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    let new = vanillite_world::entity::NewEntity::new(
        vanillite_data::entity::ids::OMINOUS_ITEM_SPAWNER,
        network_ids.next(),
        crate::mob::new_uuid(),
        at,
    );
    let id = level.entities.spawn_special(
        new,
        crate::mob::boss::special::SpecialData::OminousItemSpawner(Box::new(OminousItemSpawner {
            item,
            spawn_item_after_ticks: after,
            tick_count: 0,
        })),
    );
    if !level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0)
    {
        return;
    }
    let r = level.random();
    let pitch = (r.next_float() - r.next_float()) * 0.2 + 1.0;
    level.play_block_sound(
        None,
        BlockPos::containing(at.x, at.y, at.z),
        sound_id("block.trial_spawner.spawn_item_begin"),
        SOUND,
        1.0,
        pitch,
    );
    let ticks =
        spawner_mut(level, pos).map_or(0, |s| s.ominous_config.get().ticks_between_item_spawners());
    if let Some(s) = spawner_mut(level, pos) {
        s.cooldown_ends_at = game_time + ticks;
    }
}

/// `calculatePositionToSpawnSpawner`: above a detected player (not creative or spectator,
/// alive, within the required range) or a tracked mob in range, chosen by `nextBoolean`.
fn position_to_spawn_item_spawner(level: &mut Level, pos: BlockPos) -> Option<Vec3> {
    let s = spawner_mut(level, pos)?;
    let range = f64::from(s.required_player_range);
    let (detected, mobs) = (s.detected_players.clone(), s.current_mobs.clone());
    let c = center(pos);
    let ents = &level.entities;
    let near = |id: EntityId| {
        ents.store
            .get(id)
            .is_some_and(|e| e.pos().distance_to_sqr(c) <= range * range)
    };
    let players: Vec<EntityId> = detected
        .iter()
        .filter_map(|u| ents.player_by_uuid(*u))
        .filter(|&id| {
            ents.player(id).is_some_and(|p| {
                !matches!(p.game_mode, GameMode::Creative | GameMode::Spectator)
                    && !p.survival.is_dead_or_dying()
            }) && near(id)
        })
        .collect();
    if players.is_empty() {
        return None;
    }
    let mobs: Vec<EntityId> = mobs
        .iter()
        .filter_map(|(u, id)| id.or_else(|| find_mob(level, pos, *u)))
        .filter(|&id| level.entities.mob(id).is_some_and(|m| m.is_alive()) && near(id))
        .collect();
    let r = level.random();
    let eligible = if r.next_boolean() { mobs } else { players };
    let target = match eligible.len() {
        0 => return None,
        1 => eligible[0],
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        n => eligible[usize::try_from(r.next_int_bounded(n as i32)).unwrap_or(0)],
    };
    // `calculatePositionAbove`: up to the height plus 2 to 5 blocks, stopped by blocks (the
    // visual shapes are taken as the collision shapes); the centre of the block below.
    let e = level.entities.store.get(target)?;
    let (from, height) = (e.pos(), e.dims().height);
    #[allow(clippy::cast_precision_loss)]
    let up = height + 2.0 + level.random().next_int_bounded(4) as f32;
    let to = Vec3::new(from.x, from.y + f64::from(up), from.z);
    let hit = physics::clip(level, from, to)
        .map_or_else(|| BlockPos::containing(to.x, to.y, to.z), |(b, _)| b);
    let down = Vec3::new(
        f64::from(hit.x) + 0.5,
        f64::from(hit.y) + 0.5 - 1.0,
        f64::from(hit.z) + 0.5,
    );
    let below = BlockPos::containing(down.x, down.y, down.z);
    level
        .block(below)
        .collision_shape()
        .is_empty()
        .then_some(down)
}

/// `OminousItemSpawner.tick` on the server (`tickNonPassenger` counts the tick first): the
/// warning sound 36 ticks before, then the item drops or flies down, level event 3021, the
/// `entity_place` game event of the new entity, and the spawner dies (`entity_die`).
pub fn tick_item_spawner(level: &mut Level, id: EntityId) {
    use crate::mob::boss::special::SpecialData;
    let Some(SpecialData::OminousItemSpawner(s)) = level.entities.special_mut(id) else {
        return;
    };
    s.tick_count += 1;
    let (tick, after) = (i64::from(s.tick_count), s.spawn_item_after_ticks);
    let Some(pos) = level.entities.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let block = BlockPos::containing(pos.x, pos.y, pos.z);
    if tick == after - 36 {
        level.play_block_sound(
            None,
            block,
            sound_id("block.trial_spawner.about_to_spawn_item"),
            crate::level::SOUND_NEUTRAL,
            1.0,
            1.0,
        );
    }
    if tick < after {
        return;
    }
    let item = match level.entities.special_mut(id) {
        Some(SpecialData::OminousItemSpawner(s)) => std::mem::take(&mut s.item),
        _ => ItemStack::EMPTY,
    };
    if !item.is_empty() {
        let spawned = match super::dispenser::projectile_item_config(&item) {
            Some((uncertainty, power, event)) => {
                if let Some(event) = event {
                    level.level_event(None, event, block, 0);
                }
                let shot = super::dispenser::spawn_projectile_item(
                    level,
                    pos,
                    Direction::Down,
                    &item,
                    power,
                    uncertainty,
                );
                // `projectile.setOwner(this)`: an arrow of the spawner cannot be picked up
                // (`AbstractArrow.setOwner`: `Pickup.DISALLOWED`).
                if let Some(a) = shot.and_then(|s| level.entities.arrow_mut(s)) {
                    a.owner = Some(id);
                    a.pickup = 0;
                }
                shot
            }
            // `new ItemEntity(level, x, y, z, item)`: its motion from its own random.
            None => super::drops::spawn_item_moving(level, pos, item, None, 0, None),
        };
        level.level_event(None, 3021, block, 1);
        let events = crate::level::game_event::events();
        level.game_event(
            events.entity_place,
            pos,
            crate::level::game_event::GameEventContext::of(spawned),
        );
    }
    // `kill`: removed, then `entity_die`.
    let events = crate::level::game_event::events();
    level.game_event(
        events.entity_die,
        pos,
        crate::level::game_event::GameEventContext::of(Some(id)),
    );
    level.entities.despawn(id);
}

/// The synched data of an ominous item spawner: `DATA_ITEM` (8).
pub fn item_spawner_entity_data(out: &mut Vec<u8>, s: &OminousItemSpawner, all: bool) {
    use vanillite_protocol::codec::Encode;
    if all {
        // `EntityDataSerializers.ITEM_STACK`.
        crate::mob::boss::data::item(out, 8, 7, |o| s.item.encode(o));
    }
}

/// `addAdditionalSaveData`.
pub fn save_item_spawner(s: &OminousItemSpawner, out: &mut Compound) {
    if !s.item.is_empty() {
        out.insert(
            "item",
            Tag::Compound(crate::item_nbt::stack_to_nbt(&s.item)),
        );
    }
    out.insert(
        "spawn_item_after_ticks",
        Tag::Long(s.spawn_item_after_ticks),
    );
}

/// `readAdditionalSaveData`.
pub fn load_item_spawner(tag: &Compound) -> OminousItemSpawner {
    OminousItemSpawner {
        item: tag
            .get("item")
            .and_then(Tag::as_compound)
            .map_or(ItemStack::EMPTY, crate::item_nbt::stack_from_nbt),
        spawn_item_after_ticks: match tag.get("spawn_item_after_ticks") {
            Some(Tag::Long(v)) => *v,
            _ => 0,
        },
        tick_count: 0,
    }
}
