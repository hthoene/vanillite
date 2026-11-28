//! Mobs in a world: the entity tick of mobs, arrows and experience orbs in Vanilla's tick
//! order, the attacks of players, natural spawning and the animals of new chunks.

use std::sync::Arc;

use vanillite_base::random::LegacyRandom;
use vanillite_base::{Aabb, BlockPos, ChunkPos, Vec3};
use vanillite_config::{GameMode, VanillaOr, WorldRules};
use vanillite_data::entity::ids;
use vanillite_game::advancements::trigger::{Event, TriggerKind};
use vanillite_game::advancements::view;
use vanillite_game::entities::{Entities, is_mob_kind};
use vanillite_game::item::ItemStack;
use vanillite_game::level::Level;
use vanillite_game::level::explosion::BlockInteraction;
use vanillite_game::lightning::{self, StrikeInput};
use vanillite_game::minecart::{self, MinecartKind};
use vanillite_game::mob::activation::{ActivationSettings, Decision, WakeUp};
use vanillite_game::mob::boss;
use vanillite_game::mob::boss::dragon::{self, fight::DragonFight};
use vanillite_game::mob::boss::special::{self, is_special_kind};
use vanillite_game::mob::spawn::{self, SpawnInput, SpawnLimits, SpawnTable, Spawner};
use vanillite_game::mob::{
    self, Environment, MobContext, MobSettings, WorldAction, animal, combat, projectile, tame,
};
use vanillite_game::player::item_use;
use vanillite_game::player::{EntityAction, VehicleInput};
use vanillite_game::portal;
use vanillite_game::riding;
use vanillite_game::survival::blocks::BlockAccess;
use vanillite_game::survival::damage::DamageSource;
use vanillite_game::survival::rules::{Difficulty, Rule};
use vanillite_game::vehicle::{self, BoatKind};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::EntityId;

use crate::global::Global;

/// The End's dimension key.
const END_DIMENSION: &str = "minecraft:the_end";
use crate::survival::{WorldBlocks, WorldSurvival, raining_at};

/// Block access without blocks, for effects applied after a tick.
struct NoBlocks;

impl BlockAccess for NoBlocks {
    fn block_state(&self, _: BlockPos) -> Option<vanillite_data::block::BlockState> {
        None
    }
}

/// The dragon fight's saved data: the last state the fight wrote, taken by the next save.
#[derive(Debug, Default)]
struct FightData(std::sync::Mutex<Option<vanillite_nbt::Compound>>);

impl vanillite_storage::saved_data::SavedData for FightData {
    fn id(&self) -> &str {
        DragonFight::ID
    }

    fn save(&self) -> Option<vanillite_nbt::Compound> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// The raids' saved data (`minecraft:raids`): the last state the raids wrote, taken by the
/// next save.
#[derive(Debug, Default)]
struct RaidsData(std::sync::Mutex<Option<vanillite_nbt::Compound>>);

impl vanillite_storage::saved_data::SavedData for RaidsData {
    fn id(&self) -> &str {
        "minecraft:raids"
    }

    fn save(&self) -> Option<vanillite_nbt::Compound> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// The wandering trader spawner's saved data: the last state it wrote, taken by the next save.
#[derive(Debug, Default)]
struct TraderSavedData(std::sync::Mutex<Option<vanillite_nbt::Compound>>);

impl vanillite_storage::saved_data::SavedData for TraderSavedData {
    fn id(&self) -> &str {
        "minecraft:wandering_trader"
    }

    fn save(&self) -> Option<vanillite_nbt::Compound> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// The mob state of one world.
#[derive(Debug)]
pub(crate) struct WorldMobs {
    /// The End's dragon fight.
    fight: Option<DragonFight>,
    fight_data: Option<Arc<FightData>>,
    cx: MobContext,
    spawner: Spawner,
    table: Option<Arc<SpawnTable>>,
    seed: i64,
    settings: MobSettings,
    limits: SpawnLimits,
    /// Ticks between natural spawn rounds (`performance.spawning.spawn-interval`).
    spawn_interval: i64,
    /// Generated chunks waiting to become live, with the ticks they waited.
    generated: Vec<(ChunkPos, u32)>,
    scratch: Vec<ChunkPos>,
    ticks: u64,
    /// The activation range state (`performance.activation-range`).
    activation: vanillite_game::mob::activation::Activation,
    /// The players of the tick that activate mobs, reused.
    activation_players: Vec<(EntityId, Vec3)>,
    /// The entities the ticking lightning bolt strikes.
    bolt_hits: Vec<EntityId>,
    /// The custom spawners of the overworld (`ServerLevel.customSpawners`).
    phantoms: vanillite_game::mob::fam::undead::phantom::PhantomSpawner,
    patrols: vanillite_game::mob::fam::illager::patrol::PatrolSpawner,
    cats: spawn::CatSpawner,
    siege: vanillite_game::raid::siege::VillageSiege,
    raids_data: Option<Arc<RaidsData>>,
    /// `WanderingTraderSpawner` (overworld) and its saved data.
    traders: Option<vanillite_game::mob::fam::village::trader::spawner::TraderSpawner>,
    trader_data: Option<Arc<TraderSavedData>>,
    /// Reused buffers of the item entities' tick.
    item_scratch: crate::blocks::ItemScratch,
    /// World changes entity ticks queued and that were applied after their tick, since the
    /// world started.
    deferred_actions: u64,
    /// A `/vanillite profile` runs this tick (set by the world).
    pub(crate) profiling: bool,
    /// The entity times of the running or last profile.
    entity_profile: Option<crate::profiler::EntityProfile>,
}

/// What the item entities' tick needs from the world.
pub(crate) struct ItemTick {
    /// Despawn and merge rules.
    pub(crate) settings: crate::blocks::ItemSettings,
}

impl WorldMobs {
    pub(crate) fn new(table: Option<Arc<SpawnTable>>, seed: i64, rules: &WorldRules) -> WorldMobs {
        WorldMobs {
            fight: None,
            fight_data: None,
            item_scratch: crate::blocks::ItemScratch::default(),
            deferred_actions: 0,
            cx: MobContext::default(),
            spawner: Spawner::default(),
            table,
            seed,
            settings: settings(rules),
            limits: spawn_limits(rules),
            spawn_interval: i64::from(rules.spawning.spawn_interval.max(1)),
            generated: Vec::new(),
            scratch: Vec::new(),
            ticks: 0,
            activation: Default::default(),
            activation_players: Vec::new(),
            bolt_hits: Vec::new(),
            phantoms: Default::default(),
            patrols: Default::default(),
            cats: spawn::CatSpawner::default(),
            siege: Default::default(),
            raids_data: None,
            traders: None,
            trader_data: None,
            profiling: false,
            entity_profile: None,
        }
    }

    /// Takes the entity times of the last profile.
    pub(crate) fn take_entity_profile(&mut self) -> Option<crate::profiler::EntityProfile> {
        self.entity_profile.take()
    }

    /// Loads the dimension's raids from their saved data (`Raids.TYPE`).
    pub(crate) fn init_raids(
        &mut self,
        data: &vanillite_storage::saved_data::SavedDataStore,
        level: &mut Level,
    ) {
        match data.read("minecraft:raids") {
            Ok(Some(saved)) => level.entities.raids = vanillite_game::raid::Raids::load(&saved),
            Ok(None) => {}
            Err(e) => log::error!("Could not read the raids: {e}"),
        }
        let store = Arc::new(RaidsData::default());
        data.register(Arc::clone(&store) as _);
        self.raids_data = Some(store);
    }

    /// `Raids.tick` (after the scheduled ticks, before the chunks), then the raids' changed
    /// state goes to their saved data.
    pub(crate) fn tick_raids(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        level.flush_poi_changes();
        if level.entities.raids.raids.is_empty() && !level.entities.raids.dirty {
            return;
        }
        let state = &global.level;
        let rules = state.rules().clone();
        let env = environment(
            global,
            dimension,
            survival,
            self.seed,
            level.climate().map(|c| c.sea_level),
        );
        let mut cx = vanillite_game::raid::RaidTick {
            difficulty: state.difficulty(),
            raids_rule: rules.bool(Rule::RAIDS),
            table: self.table.as_deref(),
            env,
            out: &mut survival.effects,
        };
        vanillite_game::raid::Raids::tick(level, &mut cx);
        survival.deliver(level, global);
        if let Some(data) = &self.raids_data
            && std::mem::take(&mut level.entities.raids.dirty)
        {
            *data
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(level.entities.raids.save());
        }
    }

    /// Creates the wandering trader spawner from its saved data (`WanderingTraderData.TYPE`).
    pub(crate) fn init_trader_spawner(
        &mut self,
        data: &vanillite_storage::saved_data::SavedDataStore,
    ) {
        use vanillite_game::mob::fam::village::trader::spawner::{TraderData, TraderSpawner};
        let saved = match data.read("minecraft:wandering_trader") {
            Ok(saved) => saved,
            Err(e) => {
                log::error!("Could not read the wandering trader data: {e}");
                None
            }
        };
        let trader = saved.as_ref().map(TraderData::load).unwrap_or_default();
        let store = Arc::new(TraderSavedData::default());
        data.register(Arc::clone(&store) as _);
        // `RandomSource.create()`: seeded from the clock.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64);
        self.traders = Some(TraderSpawner::new(trader, seed));
        self.trader_data = Some(store);
    }

    /// Creates the End's dragon fight from its saved data (`EnderDragonFight.TYPE`).
    pub(crate) fn init_dragon_fight(
        &mut self,
        data: &vanillite_storage::saved_data::SavedDataStore,
        level: &mut Level,
    ) {
        let saved = match data.read(DragonFight::ID) {
            Ok(saved) => saved,
            Err(e) => {
                log::error!("Could not read the dragon fight: {e}");
                None
            }
        };
        let mut bar_random = vanillite_base::random::LegacyRandom::new(
            vanillite_base::random::RandomSource::next_long(level.random()),
        );
        let fight = DragonFight::new(saved.as_ref(), self.seed, &mut bar_random);
        let store = Arc::new(FightData::default());
        data.register(Arc::clone(&store) as _);
        self.fight = Some(fight);
        self.fight_data = Some(store);
    }

    /// What commands told the fight since the last tick, `EnderDragonFight.tick` (before the
    /// entities), and the crystal placed since the last tick (`tryRespawn`).
    pub(crate) fn tick_dragon_fight(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        let pending = std::mem::take(&mut level.output().dragon_events);
        if !pending.is_empty() {
            self.cx.dragon_events.extend(pending);
            self.apply_actions(level, survival, global, dimension);
        }
        let Some(fight) = &mut self.fight else {
            self.cx.dragon_fight = None;
            return;
        };
        let state = &global.level;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let rules = state.rules().clone();
        let no_blocks = NoBlocks;
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
        let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
        // The fight has the whole level and draws `level.getRandom()` from it; its `Env`
        // random is the unseeded `RandomSource.create()` of the spikes and gateways.
        let mut random = LegacyRandom::new(vanillite_game::mob::new_unique_seed());
        let mut env = survival.env(
            &mut random,
            state,
            &rules,
            &no_blocks,
            &raining,
            dimension,
            &mut no_gateway,
            &hooks,
            &names,
            &teams,
        );
        if std::mem::take(&mut level.output().try_dragon_respawn) {
            fight.try_respawn(level);
        }
        fight.tick(level, &mut env, &mut self.cx);
        self.cx.dragon_fight = Some(fight.view());
        self.store_fight();
        self.apply_explosion_hurts(level, survival, global, dimension);
        self.apply_actions(level, survival, global, dimension);
        survival.deliver(level, global);
    }

    /// Hands the fight's changed state to its saved data.
    fn store_fight(&mut self) {
        if let (Some(fight), Some(data)) = (&mut self.fight, &self.fight_data)
            && fight.take_dirty()
        {
            *data
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fight.save());
        }
    }

    /// What the dragon and the crystals reported, handed to the fight.
    fn dragon_events(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        let events = std::mem::take(&mut self.cx.dragon_events);
        let Some(fight) = &mut self.fight else {
            return;
        };
        let state = &global.level;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let rules = state.rules().clone();
        let no_blocks = NoBlocks;
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
        let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
        // As in `tick_dragon_fight`: the fight draws the level random from the level.
        let mut random = LegacyRandom::new(vanillite_game::mob::new_unique_seed());
        let mut env = survival.env(
            &mut random,
            state,
            &rules,
            &no_blocks,
            &raining,
            dimension,
            &mut no_gateway,
            &hooks,
            &names,
            &teams,
        );
        for event in events {
            fight.handle(level, &mut env, &mut self.cx, event);
        }
        self.cx.dragon_fight = Some(fight.view());
        self.store_fight();
    }

    /// World changes entity ticks queued and that were applied after their tick, since the
    /// world started (see `docs/architecture/world-access.md`).
    pub(crate) fn deferred_actions(&self) -> u64 {
        self.deferred_actions
    }

    /// What the entities' tick asked of the world (tickets, gateway exits, pearl owners).
    pub(crate) fn take_travel(&mut self) -> Vec<vanillite_game::portal::TravelRequest> {
        std::mem::take(&mut self.cx.travel)
    }

    /// Takes over changed rules (config reload).
    pub(crate) fn set_rules(&mut self, rules: &WorldRules) {
        self.settings = settings(rules);
        self.limits = spawn_limits(rules);
        self.spawn_interval = i64::from(rules.spawning.spawn_interval.max(1));
    }

    /// The attacks, interactions, entity item uses and vehicle input of the players since the
    /// last tick (`handleAttack`, `handleInteract`, `handleUseItem`, `handleMoveVehicle`,
    /// `handlePaddleBoat`, sneaking off a vehicle).
    pub(crate) fn player_actions(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        let mut pending: Vec<(EntityId, EntityAction)> = Vec::new();
        let mut inputs: Vec<(EntityId, VehicleInput)> = Vec::new();
        let riding = !level.entities.riding.is_empty();
        for (id, p) in level.entities.players.iter_mut() {
            for action in p.connection.entity_actions.drain(..) {
                pending.push((id, action));
            }
            let v = &mut p.connection.vehicle;
            if riding || v.moved.is_some() {
                inputs.push((id, *v));
                v.moved = None;
                v.jump = None;
            }
        }
        if pending.is_empty() && inputs.is_empty() {
            return;
        }
        let state = &global.level;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let rules = state.rules().clone();
        let network_ids = std::sync::Arc::clone(level.network_ids_arc());
        let mut ents = std::mem::take(&mut level.entities);
        let mut random = level.lend_random();
        {
            let blocks = WorldBlocks(&level.chunks);
            let (weather, climate) = (survival.weather, survival.climate().cloned());
            let chunks = &level.chunks;
            let raining = move |pos| raining_at(chunks, climate.as_deref(), &weather, pos);
            let mut no_gateway = |_| false;
            let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
            let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
            let mut env = survival.env(
                &mut random,
                state,
                &rules,
                &blocks,
                &raining,
                dimension,
                &mut no_gateway,
                &hooks,
                &names,
                &teams,
            );
            let level: &Level = level;
            for (player, action) in pending {
                player_entity_action(
                    level,
                    &mut ents,
                    &mut env,
                    &mut self.cx,
                    &network_ids,
                    player,
                    action,
                );
            }
            for (player, input) in inputs {
                player_vehicle_input(level, &mut ents, &mut env, &mut self.cx, player, input);
            }
        }
        level.restore_random(random);
        level.entities = ents;
        self.apply_actions(level, survival, global, dimension);
        vanillite_game::menu::container::open_requests(level, std::mem::take(&mut self.cx.menus));
        survival.deliver(level, global);
    }

    /// `updateSkyBrightness`: this tick's environment (sky darkness from the day time and the
    /// weather), which scheduled and random block ticks read (light checks, turtle eggs).
    pub(crate) fn update_environment(
        &self,
        level: &mut Level,
        survival: &WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        let env = environment(
            global,
            dimension,
            survival,
            self.seed,
            level.climate().map(|c| c.sea_level),
        );
        level.spawner_env.env = env;
        level.entities.day_time = env.day_time;
    }

    /// Natural spawning (`NaturalSpawner` via `ServerChunkCache.tickChunks`) and the animals of
    /// chunks generated since the last tick, in the environment of
    /// [`Self::update_environment`].
    pub(crate) fn spawn(
        &mut self,
        level: &mut Level,
        global: &Global,
        dimension: &str,
        spawn_point: Option<BlockPos>,
        tiles: Option<&vanillite_game::level::tiles::TileRun<'_>>,
        phases: &vanillite_game::level::tiles::Phases<'_>,
    ) {
        let env = level.spawner_env.env;
        let Some(table) = self.table.clone() else {
            return;
        };
        let roll_siege = day_time(global) == Some(18000);
        let state = &global.level;
        let rules = state.rules().clone();
        let difficulty = state.difficulty();
        // New chunks: their animals once they are live.
        level.chunks.take_generated(&mut self.scratch);
        self.generated
            .extend(self.scratch.drain(..).map(|c| (c, 0)));
        if !self.generated.is_empty() {
            let mut waiting = Vec::new();
            for (chunk, waited) in std::mem::take(&mut self.generated) {
                if level.chunks.chunk(chunk).is_none() {
                    // Generated chunks become live within a few ticks unless they unload.
                    if waited < 200 {
                        waiting.push((chunk, waited + 1));
                    }
                    continue;
                }
                if rules.bool(Rule::SPAWN_MOBS) {
                    spawn::spawn_for_chunk_generation(
                        level, &table, &env, chunk, self.seed, difficulty,
                    );
                }
            }
            self.generated = waiting;
        }
        let persistent = level.game_time() % 400 == 0;
        // `performance.spawning.spawn-interval`: natural spawn rounds only every n-th tick.
        let natural =
            self.spawn_interval == 1 || persistent || level.game_time() % self.spawn_interval == 0;
        let players: Vec<(EntityId, Vec3)> = level
            .entities
            .players
            .iter()
            // A dead player is out of `level.players()` once `tickDeath` removed it.
            .filter(|(_, p)| p.game_mode != GameMode::Spectator && !p.survival.removed)
            .filter_map(|(id, _)| level.entities.store.get(id).map(|e| (id, e.pos())))
            .collect();
        let input = SpawnInput {
            players: &players,
            spawn_point,
            spawn_enemies: difficulty != Difficulty::Peaceful && rules.bool(Rule::SPAWN_MONSTERS),
            difficulty,
            env,
            limits: self.limits,
            thunder: StrikeInput {
                spawn_mobs: rules.bool(Rule::SPAWN_MOBS),
                clock_time: overworld_clock_time(global),
            },
            natural,
        };
        match tiles {
            Some(run) => {
                spawn::tick_tiled(
                    level,
                    &table,
                    &mut self.spawner,
                    &input,
                    persistent,
                    run,
                    phases,
                );
            }
            None => spawn::tick(level, &table, &mut self.spawner, &input, persistent, phases),
        }
        // `tickChunks`: the custom spawners only run with `doMobSpawning`.
        if dimension == "minecraft:overworld" && rules.bool(Rule::SPAWN_MOBS) {
            self.custom_spawners(
                level,
                &table,
                &rules,
                input.spawn_enemies,
                difficulty,
                env,
                &players,
                roll_siege,
                input.thunder.clock_time,
            );
        }
    }

    /// `tickCustomSpawners`: phantoms, patrols, cats and zombie sieges.
    #[allow(clippy::too_many_arguments)]
    fn custom_spawners(
        &mut self,
        level: &mut Level,
        table: &SpawnTable,
        rules: &vanillite_game::survival::rules::GameRules,
        spawn_enemies: bool,
        difficulty: Difficulty,
        env: Environment,
        players: &[(EntityId, Vec3)],
        roll_siege: bool,
        clock_time: i64,
    ) {
        level.flush_poi_changes();
        use vanillite_game::mob::fam::illager::patrol::{self, PatrolInput};
        use vanillite_game::mob::fam::undead::phantom;
        let rest = |id: EntityId| {
            level.entities.player(id).map_or(0, |p| {
                p.stats.get(vanillite_game::stats::custom::TIME_SINCE_REST)
            })
        };
        let rests: Vec<(EntityId, i32)> = players.iter().map(|&(id, _)| (id, rest(id))).collect();
        let time_since_rest = |id: EntityId| rests.iter().find(|r| r.0 == id).map_or(0, |r| r.1);
        phantom::phantom_spawner_tick(
            &mut self.phantoms,
            level,
            &env,
            difficulty,
            rules.bool(Rule::SPAWN_PHANTOMS),
            spawn_enemies,
            &time_since_rest,
        );
        let patrol_players: Vec<(Vec3, bool)> = level
            .entities
            .players
            .iter()
            .filter_map(|(id, p)| {
                let e = level.entities.store.get(id)?;
                Some((e.pos(), p.game_mode == GameMode::Spectator))
            })
            .collect();
        // `CAN_PILLAGER_PATROL_SPAWN`: the biome's value ANDed with the `early_game` timeline
        // of the overworld clock (false for the first 120000 ticks).
        let early_game = clock_time < 120_000;
        let can_patrol = |level: &Level, pos: BlockPos| {
            !early_game && table.biome(level.biome(pos)).is_none_or(|b| b.patrol_spawn)
        };
        let input = PatrolInput {
            enabled: spawn_enemies && rules.bool(Rule::SPAWN_PATROLS),
            difficulty,
            env,
            players: &patrol_players,
            table: Some(table),
            can_patrol_spawn: &can_patrol,
        };
        patrol::patrol_spawner_tick(level, &mut self.patrols, &input);
        let ids: Vec<EntityId> = players.iter().map(|p| p.0).collect();
        spawn::cat_spawner_tick(
            level,
            Some(table),
            &mut self.cats,
            &mut self.spawner.structures,
            &env,
            difficulty,
            &ids,
        );
        vanillite_game::raid::siege::tick(
            level,
            &mut self.siege,
            &vanillite_game::raid::siege::SiegeInput {
                spawn_enemies,
                difficulty,
                env,
                at_roll_marker: roll_siege,
                table: Some(table),
            },
        );
        if let Some(traders) = &mut self.traders {
            use vanillite_game::mob::fam::village::trader::spawner::{self, TraderInput};
            // `getRandomPlayer`: the alive players.
            let alive: Vec<Vec3> = level
                .entities
                .players
                .iter()
                .filter(|(_, p)| !p.survival.is_dead_or_dying())
                .filter_map(|(id, _)| level.entities.store.get(id).map(|e| e.pos()))
                .collect();
            let input = TraderInput {
                enabled: rules.bool(Rule::SPAWN_WANDERING_TRADERS),
                difficulty,
                env,
                table: Some(table),
                players: &alive,
            };
            spawner::tick(level, traders, &input);
            if std::mem::take(&mut traders.dirty)
                && let Some(store) = &self.trader_data
            {
                *store
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(traders.data.save());
            }
        }
    }

    /// `entityTickList.forEach(tickNonPassenger)`: falling blocks, mobs, arrows and orbs in
    /// tick order. Mobs, arrows and orbs only tick in chunks that tick.
    pub(crate) fn tick_entities(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
        items: ItemTick,
    ) {
        self.ticks += 1;
        level.flush_poi_changes();
        // `detectEquipmentUpdates` of the mobs whose equipment a menu changed.
        for id in std::mem::take(&mut level.entities.equipment_updates) {
            if let Some(e) = vanillite_game::mob::equipment::tracking_update(&level.entities, id) {
                survival.effects.push(e);
            }
        }
        let state = &global.level;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let rules = state.rules().clone();
        // `WitherSkullBlock.setPlacedBy` of the skulls players placed since the last tick.
        for pos in std::mem::take(&mut level.output().summon_checks) {
            let seed = vanillite_game::mob::new_entity_seed();
            boss::wither::check_spawn(level, pos, state.difficulty(), seed);
        }
        // `getNearestPlayer` skips spectators.
        let has_players = level
            .entities
            .players
            .iter()
            .any(|(_, p)| p.game_mode != GameMode::Spectator);
        let env = environment(
            global,
            dimension,
            survival,
            self.seed,
            level.climate().map(|c| c.sea_level),
        );
        self.cx.begin_tick(env, self.settings, has_players);
        // `PiglinAi.angerNearbyPiglins` of the block uses and breaks of the players' packets.
        let anger = std::mem::take(&mut level.output().piglin_anger);
        if !anger.is_empty() {
            let mut ents = std::mem::take(&mut level.entities);
            let mut random = level.lend_random();
            {
                let blocks = WorldBlocks(&level.chunks);
                let raining = |_| false;
                let mut no_gateway = |_| false;
                let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
                let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
                let mut env = survival.env(
                    &mut random,
                    state,
                    &rules,
                    &blocks,
                    &raining,
                    dimension,
                    &mut no_gateway,
                    &hooks,
                    &names,
                    &teams,
                );
                for (player, only_if_seen) in anger {
                    vanillite_game::mob::fam::brainy::piglin::anger_nearby_piglins(
                        level,
                        &mut ents,
                        &mut env,
                        &mut self.cx,
                        player,
                        only_if_seen,
                    );
                }
            }
            level.restore_random(random);
            level.entities = ents;
        }
        // What spawner blocks read when the block entities tick after the entities.
        level.spawner_env = vanillite_game::block::spawner::SpawnerEnv {
            env,
            difficulty: state.difficulty(),
            spawner_blocks_work: rules.bool(Rule::SPAWNER_BLOCKS_WORK),
            spawn_mobs: rules.bool(Rule::SPAWN_MOBS),
            spawn_monsters: rules.bool(Rule::SPAWN_MONSTERS),
        };
        self.activation_players.clear();
        if self.settings.activation.enabled() {
            let ents = &level.entities;
            self.activation_players.extend(
                ents.players
                    .iter()
                    .filter(|(_, p)| p.game_mode != GameMode::Spectator && !p.survival.removed)
                    .filter_map(|(id, _)| ents.store.get(id).map(|e| (id, e.pos()))),
            );
        }
        self.activation.begin_tick(
            self.settings.activation,
            self.ticks,
            &self.activation_players,
        );
        let activation = self.activation.enabled();
        let in_end = dimension == END_DIMENSION;
        let mut cursor = level.entities.store.tick_cursor();
        let mut tnt_left = self.settings.max_tnt_per_tick;
        let mut ents = std::mem::take(&mut level.entities);
        // `/vanillite profile`: the time of each entity by type and chunk.
        let mut profile = if self.profiling {
            Some(self.entity_profile.take().unwrap_or_default())
        } else {
            None
        };
        while let Some(id) = ents.store.next_to_tick(&mut cursor) {
            let Some(mut entity) = ents.store.get_mut(id) else {
                continue;
            };
            entity.set_old_pos();
            let kind = entity.as_ref().kind();
            let pos = entity.as_ref().pos();
            if let Some(profile) = &mut profile {
                profile.next(kind, BlockPos::containing(pos.x, pos.y, pos.z).chunk());
            }
            if kind == ids::LIGHTNING_BOLT {
                let chunk = BlockPos::containing(pos.x, pos.y, pos.z).chunk();
                if !level.chunks.in_entity_ticking_range(chunk) {
                    continue;
                }
                level.entities = ents;
                self.tick_bolt(level, survival, global, dimension, id);
                ents = std::mem::take(&mut level.entities);
                continue;
            }
            if kind == ids::OMINOUS_ITEM_SPAWNER {
                let chunk = BlockPos::containing(pos.x, pos.y, pos.z).chunk();
                if !level.chunks.in_entity_ticking_range(chunk) {
                    continue;
                }
                level.entities = ents;
                vanillite_game::block::trial_spawner::tick_item_spawner(level, id);
                ents = std::mem::take(&mut level.entities);
                continue;
            }
            if kind == ids::ITEM {
                // `ItemEntity.tick` in the same list as every other entity: `baseTick` handles
                // the portal before the item moves.
                let chunk = BlockPos::containing(pos.x, pos.y, pos.z).chunk();
                if !level.chunks.in_entity_ticking_range(chunk) {
                    continue;
                }
                if portal::tick_portal(level, &mut ents, id, in_end, &mut self.cx.travel) {
                    continue;
                }
                let burn = {
                    let chunks = &level.chunks;
                    let (weather, climate) = (&survival.weather, survival.climate());
                    let raining = |pos| raining_at(chunks, climate.map(|c| &**c), weather, pos);
                    crate::blocks::tick_item(
                        &mut ents,
                        level,
                        id,
                        level.min_y(),
                        items.settings,
                        &mut self.item_scratch,
                        &vanillite_game::item_entity::FireWorld {
                            blocks: &WorldBlocks(chunks),
                            facts: &state.block_facts,
                            types: &state.damage_types,
                            raining_at: &raining,
                            reacts: &|s| level.has_entity_inside(s),
                        },
                    )
                };
                if !burn.is_none() {
                    // Rare: the item took fire or lava damage.
                    level.entities = ents;
                    vanillite_game::item_entity::burn_effects(level, id, burn);
                    ents = std::mem::take(&mut level.entities);
                }
                portal::enter_portals(level, &mut ents, id, &mut self.cx.gateways);
                continue;
            }
            if kind == ids::FALLING_BLOCK || kind == ids::TNT {
                let chunk = BlockPos::containing(pos.x, pos.y, pos.z).chunk();
                if !level.chunks.in_entity_ticking_range(chunk) {
                    continue;
                }
                // `max-tnt-per-tick`: the rest waits for a later tick.
                if kind == ids::TNT
                    && let Some(left) = &mut tnt_left
                {
                    if *left == 0 {
                        continue;
                    }
                    *left -= 1;
                }
                if portal::tick_portal(level, &mut ents, id, in_end, &mut self.cx.travel) {
                    continue;
                }
                level.entities = ents;
                if kind == ids::FALLING_BLOCK {
                    // The entities a landing block hurts take the damage before it lands.
                    if let Some(resume) = vanillite_game::block::falling::tick_until_hurt(level, id)
                    {
                        self.apply_explosion_hurts(level, survival, global, dimension);
                        vanillite_game::block::falling::resume_tick(level, id, resume);
                    }
                } else {
                    vanillite_game::entities::tick_entity(level, id, kind);
                }
                self.apply_explosion_hurts(level, survival, global, dimension);
                self.apply_actions(level, survival, global, dimension);
                ents = std::mem::take(&mut level.entities);
                portal::enter_portals(level, &mut ents, id, &mut self.cx.gateways);
                continue;
            }
            let is_mob = is_mob_kind(kind);
            let is_special = is_special_kind(kind);
            let is_boat = BoatKind::of(kind).is_some();
            let is_minecart = MinecartKind::of(kind).is_some();
            let is_missile = vanillite_game::mob::missile::is_missile_kind(kind);
            let is_decor = vanillite_game::decoration::ticks(kind);
            if !is_mob
                && !is_special
                && !is_boat
                && !is_minecart
                && !is_missile
                && !is_decor
                && !projectile::is_arrow_kind(kind)
                && kind != ids::EXPERIENCE_ORB
                && kind != ids::AREA_EFFECT_CLOUD
            {
                continue;
            }
            let chunk = BlockPos::containing(pos.x, pos.y, pos.z).chunk();
            if !level.is_block_ticking(chunk) {
                continue;
            }
            if kind == ids::EXPERIENCE_ORB {
                if portal::tick_portal(level, &mut ents, id, in_end, &mut self.cx.travel) {
                    continue;
                }
                level.entities = ents;
                vanillite_game::entities::tick_entity(level, id, kind);
                ents = std::mem::take(&mut level.entities);
                portal::enter_portals(level, &mut ents, id, &mut self.cx.gateways);
                continue;
            }
            // `performance.activation-range`: inactive mobs only age or think.
            let mut think = false;
            if is_mob && activation {
                match self.activation.check(&mut ents, id) {
                    Decision::Active => {}
                    Decision::Inactive => {
                        if let Some(m) = ents.mob_mut(id) {
                            vanillite_game::mob::activation::age(m);
                        }
                        continue;
                    }
                    Decision::Think => think = true,
                }
            }
            // Throwables handle portals inside their tick (`ThrowableProjectile.tick`).
            let throwable = is_missile && ents.missile(id).is_some_and(|d| d.kind.is_throwable());
            if !throwable
                && !think
                && portal::tick_portal(level, &mut ents, id, in_end, &mut self.cx.travel)
            {
                continue;
            }
            let mut random = level.lend_random();
            {
                let blocks = WorldBlocks(&level.chunks);
                let (weather, climate) = (survival.weather, survival.climate().cloned());
                let chunks = &level.chunks;
                let raining = move |pos| raining_at(chunks, climate.as_deref(), &weather, pos);
                let mut no_gateway = |_| false;
                let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
                let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
                let mut env = survival.env(
                    &mut random,
                    state,
                    &rules,
                    &blocks,
                    &raining,
                    dimension,
                    &mut no_gateway,
                    &hooks,
                    &names,
                    &teams,
                );
                let level_ref: &Level = level;
                if think {
                    mob::think(level_ref, &mut ents, &mut env, &mut self.cx, id);
                } else if is_mob {
                    mob::tick(level_ref, &mut ents, &mut env, &mut self.cx, id);
                } else if is_special {
                    special::tick(level_ref, &mut ents, &mut env, &mut self.cx, id);
                } else if is_boat {
                    vehicle::tick_boat(level_ref, &mut ents, &mut env, &mut self.cx, id);
                } else if is_minecart {
                    minecart::tick(level_ref, &mut ents, &mut env, &mut self.cx, id);
                } else if projectile::is_arrow_kind(kind) {
                    projectile::tick_arrow(level_ref, &mut ents, &mut env, &mut self.cx, id);
                } else if is_missile {
                    vanillite_game::mob::missile::tick(
                        level_ref,
                        &mut ents,
                        &mut env,
                        &mut self.cx,
                        id,
                    );
                } else if kind == ids::AREA_EFFECT_CLOUD {
                    vanillite_game::effect::cloud::tick(
                        level_ref,
                        &mut ents,
                        &mut env,
                        &mut self.cx,
                        id,
                    );
                } else if is_decor {
                    vanillite_game::decoration::tick(
                        level_ref,
                        &mut ents,
                        &mut env,
                        &mut self.cx,
                        id,
                    );
                }
            }
            level.restore_random(random);
            if !self.cx.actions.is_empty() || !self.cx.dragon_events.is_empty() {
                level.entities = ents;
                self.apply_actions(level, survival, global, dimension);
                ents = std::mem::take(&mut level.entities);
            }
            if !throwable && !think {
                portal::enter_portals(level, &mut ents, id, &mut self.cx.gateways);
            }
            if ents.mob(id).is_some_and(|m| m.removed) {
                ents.despawn(id);
            }
            for d in std::mem::take(&mut self.cx.discarded) {
                ents.despawn(d);
            }
        }
        if let Some(profile) = &mut profile {
            profile.finish();
        }
        self.entity_profile = profile;
        boss::wither::sweep_boss_bars(&mut self.cx, &ents, &mut survival.effects);
        let mut random = level.lend_random();
        {
            let blocks = WorldBlocks(&level.chunks);
            let raining = |_| false;
            let mut no_gateway = |_| false;
            let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
            let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
            let mut env = survival.env(
                &mut random,
                state,
                &rules,
                &blocks,
                &raining,
                dimension,
                &mut no_gateway,
                &hooks,
                &names,
                &teams,
            );
            // `rideTick`: sneaking players get off, passengers follow their vehicles; leads of
            // knots and loaded mobs.
            riding::stop_riding_sneaking_players(&mut ents, &mut env);
            riding::ride_tick(&mut ents);
            let mut discarded = Vec::new();
            tame::tick_leashes(level, &mut ents, &mut env, &mut discarded);
            for d in discarded {
                ents.despawn(d);
            }
        }
        level.restore_random(random);
        level.entities = ents;
        if !self.cx.actions.is_empty() {
            self.apply_actions(level, survival, global, dimension);
        }
        // `TheEndGatewayBlockEntity.triggerCooldown` of the gateways that took an entity.
        for pos in std::mem::take(&mut self.cx.gateways) {
            let state = level.block(pos);
            vanillite_game::block::portal::trigger_gateway_cooldown(level, pos, state);
        }
        vanillite_game::player::using::tick_brushes(level);
        survival.deliver(level, global);
    }

    /// The damage of an explosion made outside the entity tick (a plugin's `explode`) and
    /// what it queued, applied at once as after an exploding entity's tick.
    pub(crate) fn after_explosion(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        self.apply_explosion_hurts(level, survival, global, dimension);
        self.apply_actions(level, survival, global, dimension);
        survival.deliver(level, global);
    }

    /// The mob context of the players' ticks (their `pushEntities` and `touch`); afterwards
    /// [`Self::after_player_ticks`].
    pub(crate) fn cx_mut(&mut self) -> &mut MobContext {
        &mut self.cx
    }

    /// Applies what the players' ticks queued in the mob context (a cube they touched).
    pub(crate) fn after_player_ticks(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        if self.cx.actions.is_empty()
            && self.cx.dragon_events.is_empty()
            && self.cx.discarded.is_empty()
        {
            return;
        }
        self.apply_actions(level, survival, global, dimension);
        survival.deliver(level, global);
    }

    /// Applies what the last mob tick queued: explosions and block changes.
    fn apply_actions(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        let state = &global.level;
        let rules = state.rules().clone();
        while !self.cx.actions.is_empty() || !self.cx.dragon_events.is_empty() {
            self.apply_queued(level, survival, global, dimension, &rules);
            self.finish_removals(level);
            if !self.cx.dragon_events.is_empty() {
                self.dragon_events(level, survival, global, dimension);
                self.finish_removals(level);
            }
        }
        self.finish_removals(level);
    }

    /// One round of the queued world changes.
    fn apply_queued(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
        rules: &vanillite_game::survival::rules::GameRules,
    ) {
        let actions = std::mem::take(&mut self.cx.actions);
        self.deferred_actions += actions.len() as u64;
        for action in actions {
            match action {
                WorldAction::ExplodeWith {
                    source,
                    indirect,
                    pos,
                    radius,
                    blocks,
                } => {
                    // `ExplosionInteraction.BLOCK` decays with `blockExplosionDropDecay`.
                    let interaction = if !blocks {
                        BlockInteraction::Keep
                    } else if rules.bool(Rule::BLOCK_EXPLOSION_DROP_DECAY) {
                        BlockInteraction::DestroyWithDecay
                    } else {
                        BlockInteraction::Destroy
                    };
                    level.explode(source, indirect, pos, radius, interaction);
                    self.apply_explosion_hurts(level, survival, global, dimension);
                }
                WorldAction::Explode(e) => {
                    e.run(level, rules);
                    self.apply_explosion_hurts(level, survival, global, dimension);
                }
                WorldAction::Particles(p) => level.send_particles_packet(*p),
                WorldAction::SetBlock(pos, block, flags) => {
                    level.set_block(pos, block, flags);
                }
                WorldAction::DestroyBlock(pos, drop) => {
                    level.destroy_block(pos, drop, None);
                }
                WorldAction::LevelEvent(event, pos, data) => {
                    level.level_event(None, event, pos, data);
                }
                WorldAction::SetDoorOpen(pos, open, source) => {
                    vanillite_game::block::redstone::set_door_open(level, pos, open, source);
                }
                WorldAction::WindBurst(b) => {
                    // `ExplosionInteraction.TRIGGER` without damage (`damagesEntities` off).
                    level.explode_with(
                        b.source,
                        None,
                        b.pos,
                        b.radius,
                        BlockInteraction::TriggerBlock,
                        vanillite_game::level::explosion::ExplosionStyle {
                            damages_entities: false,
                            knockback: b.knockback,
                            wind_sound: Some(b.sound),
                            resistance:
                                vanillite_game::level::explosion::BlockResistance::WindCharge,
                            fire: false,
                            bad_respawn_point: false,
                        },
                    );
                }
                WorldAction::Level(action) => action.apply(level),
                other => {
                    other.apply_to_level(level);
                }
            }
        }
        self.finish_removals(level);
    }

    /// `entity.hurtServer(explosion damage)` of the living entities the last explosions hit.
    fn apply_explosion_hurts(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        if level.output().explosion_hurts.is_empty() && level.output().falling_hurts.is_empty() {
            level.finish_explosion();
            return;
        }
        let state = &global.level;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let rules = state.rules().clone();
        let hurts = std::mem::take(&mut level.output().explosion_hurts);
        let falling = std::mem::take(&mut level.output().falling_hurts);
        let no_blocks = NoBlocks;
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
        let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
        let mut random = level.lend_random();
        let mut env = survival.env(
            &mut random,
            state,
            &rules,
            &no_blocks,
            &raining,
            dimension,
            &mut no_gateway,
            &hooks,
            &names,
            &teams,
        );
        let mut ents = std::mem::take(&mut level.entities);
        if !falling.is_empty() {
            vanillite_game::block::falling::apply_falling_hurts(
                level,
                &mut ents,
                &mut env,
                &mut self.cx,
                falling,
            );
        }
        for hurt in hurts {
            hurt.aim(&mut ents);
            // `damageSources().explosion(source, indirect)`.
            let mut source = match (hurt.source, hurt.indirect) {
                // `badRespawnPointExplosion(center)`.
                _ if hurt.bad_respawn_point => DamageSource::of(env.types.bad_respawn_point),
                (Some(direct), Some(cause)) => {
                    combat::entity_source(&ents, env.types.player_explosion, cause, direct)
                }
                (direct, _) => {
                    let mut source = DamageSource::of(env.types.explosion);
                    source.direct = direct;
                    source.position = Some(
                        direct
                            .and_then(|d| ents.store.get(d))
                            .map_or(hurt.center, |e| e.pos()),
                    );
                    source
                }
            };
            source.position.get_or_insert(hurt.center);
            if ents.boat(hurt.target).is_some() || ents.minecart(hurt.target).is_some() {
                // `VehicleEntity.ignoreExplosion`: mob explosions without mob griefing.
                let mob_source = hurt.indirect.is_some_and(|c| ents.mob(c).is_some());
                if !(mob_source && !rules.bool(Rule::MOB_GRIEFING)) {
                    item_use::hurt_vehicle(
                        &mut ents,
                        &mut env,
                        &mut self.cx,
                        hurt.target,
                        source,
                        hurt.amount,
                    );
                }
                continue;
            }
            // `ignoreExplosion` of the block-like entities: block-attached entities and armor
            // stands (invisible ones always).
            let blocklike = hurt.affects_blocklike(rules.bool(Rule::MOB_GRIEFING));
            let ignores = if ents.decor(hurt.target).is_some() {
                !blocklike
            } else {
                ents.mob(hurt.target)
                    .filter(|m| m.kind == vanillite_game::mob::Kind::ArmorStand)
                    .is_some_and(|_| {
                        !blocklike
                            || ents.store.get(hurt.target).is_some_and(|e| {
                                e.flags()
                                    .contains(vanillite_world::entity::EntityFlags::INVISIBLE)
                            })
                    })
            };
            if ignores {
                continue;
            }
            combat::hurt(
                level,
                &mut ents,
                &mut env,
                &mut self.cx,
                hurt.target,
                source,
                hurt.amount,
            );
        }
        level.entities = ents;
        level.restore_random(random);
        // What the victims dropped (`dropAllDeathLoot`: items and experience orbs) comes
        // before the blocks' drops of `interactWithBlocks`.
        survival.deliver(level, global);
        // `interactWithBlocks` after `hurtEntities`.
        level.finish_explosion();
        self.finish_removals(level);
    }

    /// The magic damage block entities dealt this tick (conduits hunting mobs).
    /// What the items players use do to other entities, after the players' ticks: the
    /// kinetic weapons they hold out (`onUseTick` of `updatingUsingItem`) and their spin
    /// attacks (`checkAutoSpinAttack`).
    pub(crate) fn tick_player_items(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        let busy = level
            .entities
            .players
            .iter()
            .any(|(_, p)| p.survival.using_item.is_some() || p.survival.spin_attack.is_some());
        if !busy {
            return;
        }
        let state = &global.level;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let rules = state.rules().clone();
        let mut ents = std::mem::take(&mut level.entities);
        let mut random = level.lend_random();
        {
            let blocks = WorldBlocks(&level.chunks);
            let raining = |_| false;
            let mut no_gateway = |_| false;
            let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
            let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
            let mut env = survival.env(
                &mut random,
                state,
                &rules,
                &blocks,
                &raining,
                dimension,
                &mut no_gateway,
                &hooks,
                &names,
                &teams,
            );
            vanillite_game::spear::tick_players(level, &mut ents, &mut env, &mut self.cx);
            vanillite_game::trident::tick_spin_attacks(level, &mut ents, &mut env, &mut self.cx);
        }
        level.restore_random(random);
        level.entities = ents;
        if !self.cx.actions.is_empty() {
            self.apply_actions(level, survival, global, dimension);
        }
        self.finish_removals(level);
    }

    pub(crate) fn apply_magic_hurts(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
    ) {
        if level.output().magic_hurts.is_empty() {
            return;
        }
        let state = &global.level;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let rules = state.rules().clone();
        let hurts = std::mem::take(&mut level.output().magic_hurts);
        let no_blocks = NoBlocks;
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
        let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
        let mut random = level.lend_random();
        let mut env = survival.env(
            &mut random,
            state,
            &rules,
            &no_blocks,
            &raining,
            dimension,
            &mut no_gateway,
            &hooks,
            &names,
            &teams,
        );
        let mut ents = std::mem::take(&mut level.entities);
        for (target, amount) in hurts {
            let source = DamageSource::of(env.types.magic);
            combat::hurt(
                level,
                &mut ents,
                &mut env,
                &mut self.cx,
                target,
                source,
                amount,
            );
        }
        level.entities = ents;
        level.restore_random(random);
        self.finish_removals(level);
    }

    /// `LightningBolt.tick` and the `thunderHit` of the entities it strikes.
    fn tick_bolt(
        &mut self,
        level: &mut Level,
        survival: &mut WorldSurvival,
        global: &Global,
        dimension: &str,
        id: EntityId,
    ) {
        let mut hits = std::mem::take(&mut self.bolt_hits);
        lightning::tick(level, id, &mut hits);
        if !hits.is_empty() {
            let state = &global.level;
            let rules = state.rules().clone();
            let mut ents = std::mem::take(&mut level.entities);
            let mut random = level.lend_random();
            {
                let blocks = WorldBlocks(&level.chunks);
                let (weather, climate) = (survival.weather, survival.climate().cloned());
                let chunks = &level.chunks;
                let raining = move |pos| raining_at(chunks, climate.as_deref(), &weather, pos);
                let hooks = crate::plugins::Hooks::of(global, dimension);
                let mut no_gateway = |_| false;
                let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
                let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
                let mut env = survival.env(
                    &mut random,
                    state,
                    &rules,
                    &blocks,
                    &raining,
                    dimension,
                    &mut no_gateway,
                    &hooks,
                    &names,
                    &teams,
                );
                for &target in &hits {
                    lightning::thunder_hit(level, &mut ents, &mut env, &mut self.cx, target, id);
                }
            }
            level.restore_random(random);
            level.entities = ents;
            self.finish_removals(level);
            survival.deliver(level, global);
        }
        self.bolt_hits = hits;
    }

    /// Despawns the entities removed outside their own tick.
    fn finish_removals(&mut self, level: &mut Level) {
        for d in std::mem::take(&mut self.cx.discarded) {
            level.entities.despawn(d);
        }
    }
}

fn settings(rules: &WorldRules) -> MobSettings {
    let vanilla = MobSettings::default();
    let spawning = &rules.spawning;
    MobSettings {
        path_nodes_per_tick: rules.mobs.path_nodes_per_tick,
        activation: activation_settings(rules),
        max_entity_collisions: match rules.collisions.max_entity_collisions {
            VanillaOr::Vanilla => None,
            VanillaOr::Value(n) => Some(n),
        },
        arrow_despawn_ticks: i32::from(rules.items.arrow_despawn_ticks),
        hard_despawn_range: spawning.hard_despawn_range.map(f64::from),
        soft_despawn_range: spawning
            .soft_despawn_range
            .map_or(vanilla.soft_despawn_range, f64::from),
        max_tnt_per_tick: rules.explosions.max_tnt_per_tick.get(),
    }
}

/// The activation range of `performance.activation-range`.
fn activation_settings(rules: &WorldRules) -> ActivationSettings {
    let a = &rules.activation;
    let w = &rules.wake_up;
    let wake = |max_per_tick, every, ticks: u16| WakeUp {
        max_per_tick,
        every,
        ticks: u32::from(ticks),
    };
    let none = WakeUp::default();
    ActivationSettings {
        // In `Group` order.
        ranges: [
            a.animals,
            a.monsters,
            a.raiders,
            a.misc,
            a.water,
            a.villagers,
            a.flying,
        ],
        wake_up: [
            wake(w.animals_max_per_tick, w.animals_every, w.animals_for),
            wake(w.monsters_max_per_tick, w.monsters_every, w.monsters_for),
            none,
            none,
            none,
            wake(w.villagers_max_per_tick, w.villagers_every, w.villagers_for),
            wake(w.flying_max_per_tick, w.flying_every, w.flying_for),
        ],
        villager_work_after: u32::from(a.villagers_work_immunity_after),
        villager_work_for: u32::from(a.villagers_work_immunity_for),
        villager_panic: a.villagers_active_for_panic,
        goal_throttle: a.inactive_mob_goal_selector_throttle.get(),
    }
}

/// The mob caps and despawn distance of `performance.spawning`.
fn spawn_limits(rules: &WorldRules) -> SpawnLimits {
    let s = &rules.spawning;
    let vanilla = SpawnLimits::default();
    let caps = [
        s.monster_cap,
        s.creature_cap,
        s.ambient_cap,
        s.axolotls_cap,
        s.underground_water_creature_cap,
        s.water_creature_cap,
        s.water_ambient_cap,
    ];
    SpawnLimits {
        caps: std::array::from_fn(|i| caps[i].map_or(vanilla.caps[i], i32::from)),
        despawn_distance: s.hard_despawn_range.map(i32::from),
    }
}

fn distance_sqr_to_box(b: &Aabb, p: Vec3) -> f64 {
    let dx = (b.min_x - p.x).max(0.0).max(p.x - b.max_x);
    let dy = (b.min_y - p.y).max(0.0).max(p.y - b.max_y);
    let dz = (b.min_z - p.z).max(0.0).max(p.z - b.max_z);
    dx * dx + dy * dy + dz * dz
}

/// The overworld day time (`minecraft:day` timeline) within the day.
fn day_time(global: &Global) -> Option<i64> {
    let clocks = global.level.clocks();
    clocks.timeline("minecraft:day").and_then(|t| {
        clocks
            .clocks
            .get(t.clock)
            .map(|c| t.current_ticks(c.total_ticks).rem_euclid(24000))
    })
}

/// `getOverworldClockTime`: the ticks of the overworld clock.
pub(crate) fn overworld_clock_time(global: &Global) -> i64 {
    global
        .level
        .clocks()
        .clocks
        .iter()
        .find(|c| c.id == "minecraft:overworld")
        .map_or(0, |c| c.total_ticks)
}

/// `ServerLevel.getMoonBrightness`: the overworld follows the `minecraft:moon` timeline, the
/// other dimensions keep the attribute's default (`MoonPhase.FULL_MOON`).
pub(crate) fn moon_brightness(global: &Global, dimension: &str) -> f32 {
    /// `DimensionType.MOON_BRIGHTNESS_PER_PHASE`.
    const MOON_BRIGHTNESS: [f32; 8] = [1.0, 0.75, 0.5, 0.25, 0.0, 0.25, 0.5, 0.75];
    if dimension != "minecraft:overworld" {
        return 1.0;
    }
    let moon = {
        let clocks = global.level.clocks();
        clocks.timeline("minecraft:moon").and_then(|t| {
            clocks
                .clocks
                .get(t.clock)
                .map(|c| t.current_ticks(c.total_ticks))
        })
    }
    .unwrap_or(0);
    let phase = usize::try_from((moon / 24000).rem_euclid(8)).unwrap_or(0);
    MOON_BRIGHTNESS.get(phase).copied().unwrap_or(1.0)
}

/// This tick's environment: sky darkness from the day time and the weather, whether monsters
/// burn, and the dimension's monster light rules.
fn environment(
    global: &Global,
    dimension: &str,
    survival: &WorldSurvival,
    seed: i64,
    sea_level: Option<i32>,
) -> Environment {
    let overworld = dimension == "minecraft:overworld";
    let nether = dimension == "minecraft:the_nether";
    let day = {
        let clocks = global.level.clocks();
        clocks.timeline("minecraft:day").and_then(|t| {
            clocks
                .clocks
                .get(t.clock)
                .map(|c| t.current_ticks(c.total_ticks))
        })
    }
    .unwrap_or(6000);
    // `Timelines.OVERWORLD_DAY` period count (villagers' restock days).
    let day_count = {
        let clocks = global.level.clocks();
        clocks.timeline("minecraft:day").and_then(|t| {
            clocks
                .clocks
                .get(t.clock)
                .map(|c| t.period_count(c.total_ticks))
        })
    }
    .unwrap_or(0);
    if !overworld {
        return Environment {
            day_count,
            surface_slime_chance: 0.0,
            world_seed: seed,
            sea_level: sea_level.unwrap_or(if nether { 32 } else { 0 }),
            sky_darken: if nether { 0 } else { 11 },
            monsters_burn: false,
            thundering: false,
            raining: false,
            has_sky_light: false,
            monster_block_light_limit: if nether { 15 } else { 0 },
            monster_light_test: if nether { (7, 7) } else { (0, 7) },
            ambient_light: if nether { 0.1 } else { 0.0 },
            has_ceiling: nether,
            day_time: day,
            bees_stay_in_hive: false,
            creaking_active: false,
            turtle_egg_hatch_chance: 0.002,
            cat_waking_up_gift_chance: 0.0,
            sun_angle: 0.0,
            eyeblossom_open: None,
            nether_portal_spawns_piglins: false,
        };
    }
    let w = survival.weather;
    // The surface slime chance is half the moon's brightness.
    Environment {
        surface_slime_chance: moon_brightness(global, dimension) * 0.5,
        world_seed: seed,
        day_time: day,
        day_count,
        sea_level: sea_level.unwrap_or(63),
        ..Environment::overworld(day, w.rain(), w.thunder())
    }
}

/// Bukkit's `PlayerInteractEntityEvent` (with the hand and the packet's offset from the
/// target; the event gets the point clicked in the world) or, without, the attack of `target`
/// (`PLAYER_ATTACK_ENTITY`); `true` when a plugin cancelled it.
fn player_entity_event(
    hooks: &dyn vanillite_game::hooks::GameHooks,
    ents: &Entities,
    player: EntityId,
    target: EntityId,
    interact: Option<(Hand, Vec3)>,
) -> bool {
    use vanillite_abi::event;
    let kind = match interact {
        Some(_) => event::PLAYER_INTERACT_ENTITY,
        None => event::PLAYER_ATTACK_ENTITY,
    };
    if !hooks.wants(kind) {
        return false;
    }
    let (Some(pe), Some(te)) = (ents.store.get(player), ents.store.get(target)) else {
        return false;
    };
    let hand = interact.map_or(Hand::MainHand, |(hand, _)| hand);
    let stack = vanillite_game::mob::animal::held(ents, player, hand);
    let target_pos = te.pos();
    let mut ev = vanillite_game::hooks::Event {
        entity: pe.uuid(),
        entity_type: "minecraft:player".to_owned(),
        other: te.uuid(),
        other_type: te.kind().name().to_owned(),
        key: vanillite_game::item::item_id(&stack).name().to_owned(),
        ..vanillite_game::hooks::Event::new(kind)
    };
    if let Some((hand, at)) = interact {
        ev.cause = match hand {
            Hand::MainHand => vanillite_abi::hand::MAIN,
            Hand::OffHand => vanillite_abi::hand::OFF,
        };
        ev.to = vanillite_game::hooks::Place::at(Vec3::new(
            target_pos.x + at.x,
            target_pos.y + at.y,
            target_pos.z + at.z,
        ));
    }
    hooks.event(vanillite_game::hooks::HookWorld::NONE, &mut ev);
    ev.cancelled
}

/// One attack, interaction, entity item use, spectate or pick of a player
/// (`handleAttack`, `handleInteract`, `handleUseItem`, ...), as [`WorldMobs::player_actions`]
/// runs it. The scenario harness drives its packet-level players through it too.
#[allow(clippy::too_many_arguments)]
pub fn player_entity_action(
    level: &Level,
    ents: &mut Entities,
    env: &mut vanillite_game::survival::player::Env<'_>,
    cx: &mut MobContext,
    network_ids: &Arc<vanillite_world::entity::NetworkIds>,
    player: EntityId,
    action: EntityAction,
) {
    let usable = ents
        .player(player)
        .is_some_and(|p| p.game_mode != GameMode::Spectator && !p.survival.is_dead_or_dying());
    match action {
        EntityAction::Attack(network_id) => {
            if !usable {
                return;
            }
            let Some(target) = attack_target(level, ents, player, network_id) else {
                return;
            };
            if player_entity_event(env.hooks, ents, player, target, None) {
                return;
            }
            // The part the player hit takes the damage (`EnderDragon.hurt(part)`).
            if let Some(i) = dragon::part_of(ents, target, network_id)
                && let Some(s) = ents.mob_mut(target).and_then(|m| m.boss.dragon_mut())
            {
                s.hit_part = Some(i);
            }
            item_use::attack_entity(level, ents, env, cx, player, target);
        }
        EntityAction::Interact {
            entity,
            hand,
            sneaking,
            location,
        } => {
            let Some(target) = in_reach_bordered(level, ents, player, entity) else {
                return;
            };
            if player_entity_event(env.hooks, ents, player, target, Some((hand, location))) {
                return;
            }
            // `handleInteract`: the stack before the interaction for
            // `player_interacted_with_entity`.
            let interacted = TriggerKind::PLAYER_INTERACTED_WITH_ENTITY;
            let used =
                view::listens(ents, player, interacted).then(|| animal::held(ents, player, hand));
            // The target as it was, for a target the interaction removes (a bucketed fish
            // still matches: Vanilla passes the removed entity object).
            let before = used
                .is_some()
                .then(|| view::snapshot(ents, target))
                .flatten();
            let r = if ents
                .store
                .get(target)
                .is_some_and(|e| e.kind() == ids::LEASH_KNOT)
            {
                tame::knot_interact(ents, env, &mut cx.discarded, player, target, hand, sneaking)
            } else if ents.decor(target).is_some() {
                let spectator = ents
                    .player(player)
                    .is_none_or(|p| p.game_mode == GameMode::Spectator);
                if spectator {
                    animal::Interaction::Pass
                } else {
                    vanillite_game::decoration::interact(
                        level, ents, env, cx, player, target, hand, sneaking,
                    )
                }
            } else if ents
                .mob(target)
                .is_some_and(|m| m.kind == vanillite_game::mob::Kind::ArmorStand && m.is_alive())
                && let r @ (animal::Interaction::Success | animal::Interaction::Consume) =
                    vanillite_game::decoration::stand::interact(
                        ents, env, player, target, hand, location,
                    )
            {
                r
            } else {
                animal::interact_on(level, ents, env, cx, player, target, hand, sneaking)
            };
            if r.consumes()
                && let Some(used) = used
            {
                view::fire(ents, player, interacted, |ents| {
                    Event::EntityItem(interacted, view::snapshot(ents, target).or(before), used)
                });
            }
            if r == animal::Interaction::Success
                && let Some(p) = ents.player_mut(player)
            {
                // `swingAndResetAttackStrength`.
                p.survival.attack_ticker = 0;
            }
        }
        EntityAction::Punch => {
            // `handlePunch`: `resetAttackStrengthTicker` (also for spectators).
            if let Some(p) = ents.player_mut(player) {
                p.survival.attack_ticker = 0;
            }
        }
        EntityAction::UseItem {
            hand,
            known_movement,
        } => {
            if usable {
                // The item sees the known movement of its packet's time (thrown projectiles).
                let known = ents.player_mut(player).map(|p| {
                    std::mem::replace(&mut p.connection.movement.known_movement, known_movement)
                });
                item_use::use_item(level, ents, env, cx, network_ids, player, hand);
                if let (Some(k), Some(p)) = (known, ents.player_mut(player)) {
                    p.connection.movement.known_movement = k;
                }
            }
        }
        EntityAction::UseItemOn {
            hand,
            pos,
            face,
            location,
            op,
        } => {
            if usable {
                item_use::use_item_on(
                    level,
                    ents,
                    env,
                    network_ids,
                    player,
                    hand,
                    pos,
                    face,
                    location,
                    op,
                );
            }
        }
        EntityAction::StartUsing(hand) => {
            if usable {
                vanillite_game::player::using::start(ents, env, network_ids, player, hand);
            }
        }
        EntityAction::BindToFence(pos) => {
            if usable {
                vanillite_game::mob::tame::bind_player_mobs(ents, env, network_ids, player, pos);
            }
        }
        EntityAction::StartFallFlying => {
            if usable {
                vanillite_game::elytra::start_fall_flying(ents, player);
            }
        }
        EntityAction::Stab => {
            if usable {
                vanillite_game::spear::player_stab(level, ents, env, cx, player);
            }
        }
        EntityAction::StopUsing { main_hand_only } => {
            let using = ents.player(player).and_then(|p| p.survival.using_item);
            if let Some(u) = using
                && (!main_hand_only
                    || u.hand == vanillite_protocol::packets::play::serverbound::Hand::MainHand)
            {
                let stack = vanillite_game::item::ItemStack::new(u.item, 1);
                vanillite_game::player::using::stop(ents, env, player, &stack);
            }
        }
        EntityAction::ReleaseUseItem => {
            vanillite_game::player::using::release(ents, env, network_ids, player);
        }
        EntityAction::Spectate(network_id) => {
            if let Some(target) = in_reach_bordered(level, ents, player, network_id) {
                crate::spectator::spectate(ents, &level.border, env.hooks, player, target);
            }
        }
        EntityAction::PickItem { entity, .. } => {
            pick_item_from_entity(ents, player, entity);
        }
        EntityAction::OpenVehicleInventory => {
            // `openCustomInventoryScreen` of a chest boat or mount the player
            // rides.
            if let Some(v) = ents.vehicle_of(player) {
                if ents.boat(v).is_some_and(|b| b.kind.chest) {
                    minecart::open_container(ents, env, cx, player, v);
                } else if let Some(request) = vanillite_game::menu::mount::request(ents, v, player)
                {
                    cx.menus.push(request);
                }
            }
        }
    }
}

/// The vehicle input of a player since the last tick (`handleMoveVehicle`,
/// `handlePaddleBoat`, riding jumps, sneaking off a vehicle).
pub fn player_vehicle_input(
    level: &Level,
    ents: &mut Entities,
    env: &mut vanillite_game::survival::player::Env<'_>,
    cx: &mut MobContext,
    player: EntityId,
    input: VehicleInput,
) {
    if let Some((pos, y_rot, x_rot, on_ground)) = input.moved {
        riding::move_vehicle(level, ents, cx, player, pos, y_rot, x_rot, on_ground);
    }
    riding::paddle_boat(ents, player, input.paddles.0, input.paddles.1);
    if let Some(strength) = input.jump {
        riding::riding_jump(ents, env, player, strength);
    }
    // Sneaking gets a player off its vehicle in its `rideTick` after the vehicles ticked
    // (`riding::stop_riding_sneaking_players`).
}

/// The entity (or dragon part) with `network_id` other than `player` and its box: a dragon
/// part (`getEntityOrPart`) is reached by its own box; the hit goes to the dragon.
fn entity_or_part(ents: &Entities, player: EntityId, network_id: i32) -> Option<(EntityId, Aabb)> {
    let target = ents.by_network_id(network_id)?;
    if target == player {
        return None;
    }
    let te = ents.store.get(target)?;
    let target_box = match dragon::part_of(ents, target, network_id) {
        Some(i) => ents
            .mob(target)
            .and_then(|m| m.boss.dragon())
            .map_or(te.aabb(), |s| s.part_box(i)),
        None => te.aabb(),
    };
    Some((target, target_box))
}

/// `getEyePosition` of a player.
fn eye_of(ents: &Entities, player: EntityId) -> Option<Vec3> {
    let pe = ents.store.get(player)?;
    Some(Vec3::new(
        pe.pos().x,
        pe.pos().y + f64::from(pe.kind().info().eye_height) * f64::from(pe.dims().height / 1.8),
        pe.pos().z,
    ))
}

/// The player's `entityInteractionRange()`.
fn entity_range(ents: &Entities, player: EntityId) -> f64 {
    ents.player(player).map_or(3.0, |p| {
        vanillite_game::player::play::interaction_ranges(&p.inventory, p.game_mode).1
    })
}

/// `level.getWorldBorder().isWithinBounds(target.blockPosition())`.
fn in_border(level: &Level, ents: &Entities, target: EntityId) -> bool {
    ents.store.get(target).is_some_and(|e| {
        let p = BlockPos::containing(e.pos().x, e.pos().y, e.pos().z);
        level
            .border
            .is_within_bounds(f64::from(p.x), f64::from(p.z))
    })
}

/// The entity with `network_id` if `player` can reach it (`isWithinEntityInteractionRange`
/// with a buffer of 3: the entity interaction range plus 3 from the eyes to its box).
fn in_reach(ents: &Entities, player: EntityId, network_id: i32) -> Option<EntityId> {
    let (target, target_box) = entity_or_part(ents, player, network_id)?;
    let max = entity_range(ents, player) + 3.0;
    (distance_sqr_to_box(&target_box, eye_of(ents, player)?) < max * max).then_some(target)
}

/// [`in_reach`] inside the world border (`handleInteract`, `handleSpectatorAction`).
fn in_reach_bordered(
    level: &Level,
    ents: &Entities,
    player: EntityId,
    network_id: i32,
) -> Option<EntityId> {
    in_reach(ents, player, network_id).filter(|&t| in_border(level, ents, t))
}

/// `handleAttack`'s checks: the target is inside the world border and within the main hand's
/// `attack_range` (`isWithinAttackRange(mainHand, box, 3)`), the main hand holds no
/// `piercing_weapon` and its `minimum_attack_charge` is reached (`cannotAttackWithItem(5)`);
/// items, experience orbs and arrows that cannot be attacked are refused.
fn attack_target(
    level: &Level,
    ents: &Entities,
    player: EntityId,
    network_id: i32,
) -> Option<EntityId> {
    let (target, target_box) = entity_or_part(ents, player, network_id)?;
    if !in_border(level, ents, target) {
        return None;
    }
    let p = ents.player(player)?;
    let main_hand = p.inventory.selected_item();
    let range = vanillite_game::spear::attack_range(
        main_hand,
        // `AttackRange.defaultFor`: the entity interaction range.
        #[allow(clippy::cast_possible_truncation)]
        {
            entity_range(ents, player) as f32
        },
    );
    let creative = p.game_mode == GameMode::Creative;
    let (min, max) = if creative {
        (range.min_creative_reach, range.max_creative_reach)
    } else {
        (range.min_reach, range.max_reach)
    };
    let distance = distance_sqr_to_box(&target_box, eye_of(ents, player)?).sqrt();
    let margin = f64::from(range.hitbox_margin);
    if distance < f64::from(min) - margin - 3.0 || distance > f64::from(max) + margin + 3.0 {
        return None;
    }
    if vanillite_game::spear::piercing_weapon(main_hand).is_some() {
        return None;
    }
    let kind = ents.store.get(target)?.kind();
    if [
        ids::ITEM,
        ids::EXPERIENCE_ORB,
        ids::ARROW,
        ids::SPECTRAL_ARROW,
        ids::TRIDENT,
    ]
    .contains(&kind)
    {
        return None;
    }
    // `cannotAttackWithItem(mainHand, 5)`.
    let required = vanillite_game::spear::minimum_attack_charge(main_hand);
    #[allow(clippy::cast_precision_loss)]
    let optimistic = (p.survival.attack_ticker as f32 + 5.0)
        / vanillite_game::mob::attack::attack_strength_delay(ents, player);
    if required > 0.0 && optimistic < required {
        return None;
    }
    Some(target)
}

/// `Entity.getPickResult`: a mob's spawn egg, or the item of a vehicle or decoration.
fn pick_result(ents: &Entities, target: EntityId) -> Option<ItemStack> {
    use vanillite_data::registry::ITEM;
    let kind = ents.store.get(target)?.kind();
    if kind == vanillite_data::entity::ids::PLAYER {
        return None;
    }
    if ents.decor(target).is_some() {
        return vanillite_game::decoration::pick_result(ents, target);
    }
    let name = kind.name();
    let item = match name.strip_prefix("minecraft:").unwrap_or(name) {
        "end_crystal" => "minecraft:end_crystal".to_owned(),
        "leash_knot" => "minecraft:lead".to_owned(),
        "painting" => "minecraft:painting".to_owned(),
        "armor_stand" => "minecraft:armor_stand".to_owned(),
        "spawner_minecart" => "minecraft:minecart".to_owned(),
        "item_frame" | "glow_item_frame" => name.to_owned(),
        other => {
            let egg = format!("minecraft:{other}_spawn_egg");
            if ITEM.id(&egg).is_some() {
                egg
            } else if other.ends_with("minecart")
                || other.ends_with("boat")
                || other.ends_with("raft")
            {
                name.to_owned()
            } else {
                return None;
            }
        }
    };
    ITEM.id(&item).map(|id| ItemStack::new(id, 1))
}

/// `handlePickItemFromEntity` + `tryPickItem`: the entity's item goes to the hotbar.
fn pick_item_from_entity(ents: &mut Entities, player: EntityId, network_id: i32) {
    let Some(target) = in_reach(ents, player, network_id) else {
        return;
    };
    let Some(stack) = pick_result(ents, target) else {
        return;
    };
    let Some(p) = ents.player_mut(player) else {
        return;
    };
    let infinite = p.game_mode == GameMode::Creative;
    p.inventory.pick_item(stack, infinite);
    let slot = i32::try_from(p.inventory.selected).unwrap_or(0);
    let sink = Arc::clone(&p.connection.sink);
    sink.send(&vanillite_protocol::packets::play::clientbound::SetHeldSlot { slot });
    p.inventory.broadcast_changes(sink.as_ref());
}
