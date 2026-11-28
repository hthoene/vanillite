//! The world as the player handlers see it ([`PlayLevel`] over the world's [`Level`]), sounds
//! and crack progress of the tick, and the item entity tick.

use std::collections::{HashMap, HashSet};

use vanillite_base::hash::FxHashSet;
use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_data::entity::ids as entity_ids;
use vanillite_datapack::LoadedPacks;
use vanillite_game::entities::Entities;
use vanillite_game::item::{ItemStack, ToolTable};
use vanillite_game::item_entity;
use vanillite_game::level::Level;
use vanillite_game::loot::LootTables;
use vanillite_game::player::play::GameData;
use vanillite_game::player::world::{PlayLevel, SoundSource};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{BlockDestruction, Sound, SoundEvent};
use vanillite_protocol::types::BlockPos as WirePos;
use vanillite_world::entity::{EntityFlags, EntityId, EntityStore};

/// Builds the gameplay data from the loaded data packs and installs the registry sizes and
/// tags the protocol needs to validate client item stacks.
pub fn game_data(packs: Option<&LoadedPacks>) -> GameData {
    let Some(packs) = packs else {
        return GameData::default();
    };
    let tag_ids = |registry: &str, tag: &str| -> Vec<u16> {
        packs
            .tag_ids(registry, tag)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|i| u16::try_from(i).ok())
            .collect()
    };
    let mut tools = ToolTable::new(&|tag| tag_ids("minecraft:block", tag));
    let mut loot = LootTables::load(packs.roots());
    log::info!("Loaded {} loot tables", loot.len());
    let enchantments = packs
        .registry("minecraft:enchantment")
        .map(|r| {
            r.entries()
                .iter()
                .zip(0u32..)
                .map(|(e, i)| (e.id.clone(), i))
                .collect()
        })
        .unwrap_or_default();
    let item_tags: HashMap<String, Vec<u16>> = packs
        .tags("minecraft:item")
        .map(|tags| {
            tags.keys()
                .map(|t| (t.clone(), tag_ids("minecraft:item", t)))
                .collect()
        })
        .unwrap_or_default();
    let block_tags: HashMap<String, Vec<u16>> = packs
        .tags("minecraft:block")
        .map(|tags| {
            tags.keys()
                .map(|t| {
                    let mut ids = tag_ids("minecraft:block", t);
                    ids.sort_unstable();
                    ids.dedup();
                    (t.clone(), ids)
                })
                .collect()
        })
        .unwrap_or_default();
    let block_tags = std::sync::Arc::new(block_tags);
    tools.set_block_tags(std::sync::Arc::clone(&block_tags));
    loot.set_block_tags(std::sync::Arc::clone(&block_tags));
    let entity_tags: HashMap<String, Vec<u16>> = packs
        .tags("minecraft:entity_type")
        .map(|tags| {
            tags.keys()
                .map(|t| (t.clone(), tag_ids("minecraft:entity_type", t)))
                .collect()
        })
        .unwrap_or_default();
    let enchants = vanillite_game::enchant::Enchantments {
        list: packs
            .registry("minecraft:enchantment")
            .map(|r| {
                r.entries()
                    .iter()
                    .map(|e| {
                        let json = serde_json::from_str(e.json.get()).unwrap_or_default();
                        vanillite_game::enchant::Enchantment::parse(&e.id, &json)
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut info = vanillite_protocol::item::RegistryInfo::default();
    for r in packs.registries() {
        info.sizes.insert(r.name.to_owned(), r.entries().len());
        info.names.insert(
            r.name.to_owned(),
            r.entries().iter().map(|e| e.id.clone()).collect(),
        );
    }
    for registry in packs.tagged_registries() {
        let tags: HashSet<String> = packs
            .tags(registry)
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        info.tags.insert(registry.to_owned(), tags);
    }
    vanillite_protocol::item::install_registries(info);
    let trial_spawner_configs = packs
        .registry("minecraft:trial_spawner")
        .map(|r| {
            r.entries()
                .iter()
                .filter_map(|e| {
                    let json = serde_json::from_str::<serde_json::Value>(e.json.get()).ok()?;
                    Some((
                        e.id.clone(),
                        vanillite_game::block::trial_spawner::TrialConfig::from_json(&json),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let instruments = packs
        .registry("minecraft:instrument")
        .map(|r| {
            r.entries()
                .iter()
                .map(|e| {
                    serde_json::from_str::<serde_json::Value>(e.json.get())
                        .ok()
                        .and_then(|json| {
                            vanillite_game::player::items::Instrument::from_json(&json)
                        })
                        .unwrap_or(vanillite_game::player::items::Instrument {
                            sound: String::new(),
                            use_duration: 0.0,
                            range: 0.0,
                            durability_damage: 0,
                        })
                })
                .collect()
        })
        .unwrap_or_default();
    let painting_variants = packs
        .registry("minecraft:painting_variant")
        .map(|r| {
            r.entries()
                .iter()
                .map(|e| {
                    let json = serde_json::from_str::<serde_json::Value>(e.json.get()).ok();
                    let get = |k: &str| {
                        json.as_ref()
                            .and_then(|j| j.get(k))
                            .and_then(serde_json::Value::as_i64)
                            .and_then(|v| i32::try_from(v).ok())
                            .unwrap_or(1)
                    };
                    (get("width"), get("height"))
                })
                .collect()
        })
        .unwrap_or_default();
    let recipes = vanillite_game::recipe::Recipes::load(packs.roots(), &|tag| {
        item_tags.get(tag).cloned().unwrap_or_default()
    });
    log::info!(
        "Loaded {} crafting, {} cooking, {} stonecutter and {} smithing recipes",
        recipes.crafting.len(),
        recipes.cooking.len(),
        recipes.stonecutting.len(),
        recipes.smithing.len()
    );
    let entries: Vec<(String, serde_json::Value)> = packs
        .registry("minecraft:enchantment")
        .map(|r| {
            r.entries()
                .iter()
                .map(|e| {
                    let json = serde_json::from_str(e.json.get()).unwrap_or_default();
                    (e.id.clone(), json)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut enchanting = vanillite_game::enchant::Enchanting::load(
        &entries,
        &|tag| item_tags.get(tag).cloned().unwrap_or_default(),
        &|tag| {
            packs
                .tag_ids("minecraft:enchantment", tag)
                .unwrap_or_default()
        },
    );
    let names = |registry: &str| -> Vec<String> {
        packs
            .registry(registry)
            .map(|r| r.entries().iter().map(|e| e.id.clone()).collect())
            .unwrap_or_default()
    };
    enchanting.set_trims(
        &names("minecraft:trim_material"),
        &names("minecraft:trim_pattern"),
    );
    let jukebox_songs = packs
        .registry("minecraft:jukebox_song")
        .map(|r| {
            r.entries()
                .iter()
                .map(|e| {
                    let json =
                        serde_json::from_str::<serde_json::Value>(e.json.get()).unwrap_or_default();
                    vanillite_game::block::jukebox::JukeboxSong::from_json(&e.id, &json)
                })
                .collect()
        })
        .unwrap_or_default();
    let banner_patterns = vanillite_game::menu::BannerPatterns {
        names: names("minecraft:banner_pattern"),
        tags: packs
            .tags("minecraft:banner_pattern")
            .map(|tags| {
                tags.keys()
                    .map(|t| {
                        let ids = packs
                            .tag_ids("minecraft:banner_pattern", t)
                            .unwrap_or_default();
                        (t.clone(), ids)
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    let registry_tags = packs
        .tagged_registries()
        .map(|registry| {
            let tags = packs
                .tags(registry)
                .map(|tags| {
                    tags.keys()
                        .map(|t| (t.clone(), packs.tag_ids(registry, t).unwrap_or_default()))
                        .collect()
                })
                .unwrap_or_default();
            (registry.to_owned(), tags)
        })
        .collect();
    let json_entries = |registry: &str| -> Vec<(String, serde_json::Value)> {
        packs
            .registry(registry)
            .map(|r| {
                r.entries()
                    .iter()
                    .map(|e| {
                        let json = serde_json::from_str(e.json.get()).unwrap_or_default();
                        (e.id.clone(), json)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let trades = vanillite_game::trading::data::TradeData::load(
        &json_entries("minecraft:villager_trade"),
        &json_entries("minecraft:trade_set"),
        &|tag| {
            packs
                .tag_ids("minecraft:villager_trade", tag)
                .unwrap_or_default()
        },
        &|id| {
            packs
                .registry("minecraft:villager_trade")
                .and_then(|r| r.id_of(id))
                .and_then(|i| u32::try_from(i).ok())
        },
    );
    log::info!(
        "Loaded {} villager trades in {} trade sets",
        trades.trades.len(),
        trades.sets.len()
    );
    GameData {
        trades,
        tools,
        loot,
        enchantments,
        item_tags,
        trial_spawner_configs,
        entity_tags,
        enchants,
        enderman_holdable: tag_ids("minecraft:block", "minecraft:enderman_holdable"),
        block_tags,
        recipes,
        enchanting,
        banner_patterns,
        registry_tags,
        instruments,
        painting_variants,
        compostables: vanillite_game::block::composter::load_providers(packs.roots()),
        jukebox_songs,
    }
}

/// A sound to send.
#[derive(Clone, Debug)]
struct SoundEffect {
    except: Option<i32>,
    pos: Vec3,
    sound: u16,
    source: SoundSource,
    volume: f32,
    pitch: f32,
    seed: i64,
}

/// Sounds and crack progress of the player handlers, sent once the players are back in the
/// level. Block changes and level events go through the level's output.
#[derive(Debug, Default)]
pub struct LevelEffects {
    progress: Vec<(i32, BlockPos, i32)>,
    sounds: Vec<SoundEffect>,
}

/// [`PlayLevel`] over a world's [`Level`] while its players are handled.
#[allow(missing_debug_implementations)]
pub struct WorldLevel<'a> {
    pub level: &'a mut Level,
    pub effects: &'a mut LevelEffects,
    /// Players in spectator mode (they never block building).
    pub spectators: &'a FxHashSet<EntityId>,
    /// The acting player with its current box (its stored position may lag behind).
    pub actor: Option<(EntityId, Aabb)>,
    /// The world's dimension.
    pub dimension: &'a str,
}

impl PlayLevel for WorldLevel<'_> {
    fn spectators(&self) -> Vec<EntityId> {
        self.spectators.iter().copied().collect()
    }

    fn level_mut(&mut self) -> Option<&mut Level> {
        Some(self.level)
    }

    fn hook_world(&self) -> vanillite_game::hooks::HookWorld<'_> {
        vanillite_game::hooks::HookWorld::of(self.level)
    }

    fn level(&self) -> Option<&Level> {
        Some(self.level)
    }

    fn dimension(&self) -> &str {
        self.dimension
    }

    fn block(&self, pos: BlockPos) -> BlockState {
        self.level.block(pos)
    }

    fn lights_portal(&self, pos: BlockPos, forward: vanillite_base::Direction) -> bool {
        vanillite_game::block::portal::lights_portal(self.level, pos, forward)
    }

    fn end_portal_frame_filled(&mut self, pos: BlockPos) {
        use vanillite_game::block::portal;
        self.level
            .update_neighbour_for_output_signal(pos, vanillite_data::block::ids::END_PORTAL_FRAME);
        self.level.level_event(None, 1503, pos, 0);
        if let Some(corner) = portal::find_end_portal(self.level, pos) {
            portal::open_end_portal(self.level, corner);
        }
    }

    fn is_loaded(&self, pos: BlockPos) -> bool {
        self.level.is_loaded(pos)
    }

    fn within_border(&self, pos: BlockPos) -> bool {
        self.level.border.is_block_within_bounds(pos)
    }

    fn min_y(&self) -> i32 {
        self.level.min_y()
    }

    fn max_y(&self) -> i32 {
        self.level.max_y()
    }

    fn set_block(&mut self, pos: BlockPos, state: BlockState, update_flags: u32) -> bool {
        self.level.set_block(pos, state, update_flags)
    }

    fn level_event(&mut self, except: Option<i32>, event: i32, pos: BlockPos, data: i32) {
        // The handlers only ever exclude the acting player.
        let source = except.and(self.actor.map(|(id, _)| id));
        self.level.level_event(source, event, pos, data);
    }

    fn destroy_progress(&mut self, breaker: i32, pos: BlockPos, progress: i32) {
        self.effects.progress.push((breaker, pos, progress));
    }

    fn play_sound(
        &mut self,
        except: Option<i32>,
        pos: Vec3,
        sound: u16,
        source: SoundSource,
        volume: f32,
        pitch: f32,
    ) {
        // `Level.playSound` seeds from `threadSafeRandom`, never from the level random.
        let seed = vanillite_game::survival::sound_seed();
        self.effects.sounds.push(SoundEffect {
            except,
            pos,
            sound,
            source,
            volume,
            pitch,
            seed,
        });
    }

    fn spawn_item(
        &mut self,
        pos: Vec3,
        stack: ItemStack,
        velocity: Vec3,
        pickup_delay: i16,
        thrower: Option<u128>,
    ) {
        vanillite_game::block::drops::spawn_item(
            self.level,
            pos,
            stack,
            velocity,
            pickup_delay,
            thrower,
        );
    }

    fn placed_by(&mut self, pos: BlockPos, state: BlockState) {
        if vanillite_game::mob::boss::wither::is_wither_skull(state) {
            self.level.output().summon_checks.push(pos);
        }
    }

    fn game_event(
        &mut self,
        event: u16,
        pos: BlockPos,
        source: Option<vanillite_world::entity::EntityId>,
        affected: Option<BlockState>,
    ) {
        let context = vanillite_game::level::game_event::GameEventContext {
            source,
            affected,
            owner: None,
        };
        self.level.game_event_block(event, pos, context);
    }

    fn use_end_crystal(
        &mut self,
        on: BlockPos,
        player: Option<vanillite_world::entity::EntityId>,
    ) -> bool {
        let seed = self.level.random().next_long();
        let uuid =
            vanillite_game::mob::random_uuid(&mut vanillite_base::random::LegacyRandom::new(seed));
        vanillite_game::mob::boss::dragon::fight::use_end_crystal(self.level, uuid, on, player)
    }

    fn is_unobstructed(&self, state: BlockState, pos: BlockPos) -> bool {
        // `ScaffoldingBlock.getCollisionShape` of a placement context is empty.
        if state.block() == vanillite_data::block::ids::SCAFFOLDING {
            return true;
        }
        let (dx, dy, dz) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
        let store = &self.level.entities.store;
        for b in state.collision_shape() {
            let shape = Aabb::new(
                b[0] + dx,
                b[1] + dy,
                b[2] + dz,
                b[3] + dx,
                b[4] + dy,
                b[5] + dz,
            );
            if let Some((_, own)) = self.actor
                && own.intersects(&shape)
            {
                return false;
            }
            // Every entity that blocks building (`Level.isUnobstructed`), not only players.
            let mut blocked = false;
            store.for_each_in(&shape, false, |id| {
                let is_actor = self.actor.is_some_and(|(a, _)| a == id);
                if !is_actor
                    && !self.spectators.contains(&id)
                    && vanillite_game::block::dispenser::blocks_building(self.level, id)
                {
                    blocked = true;
                }
            });
            if blocked {
                return false;
            }
        }
        true
    }

    fn random_float(&mut self) -> f32 {
        self.level.random().next_float()
    }

    fn random_double(&mut self) -> f64 {
        self.level.random().next_double()
    }

    fn random_gaussian(&mut self) -> f64 {
        self.level.random().next_gaussian()
    }

    fn random_boolean(&mut self) -> bool {
        self.level.random().next_boolean()
    }

    fn random_int(&mut self, bound: i32) -> i32 {
        self.level.random().next_int_bounded(bound.max(1))
    }

    fn random_sequences(
        &self,
    ) -> Option<std::sync::Arc<std::sync::Mutex<vanillite_game::random_sequences::RandomSequences>>>
    {
        Some(std::sync::Arc::clone(self.level.random_sequences()))
    }
}

/// Players near `pos` within `range` blocks (by their feet position), except `except`.
fn players_near(
    store: &EntityStore,
    pos: Vec3,
    range: f64,
    except: Option<i32>,
    mut f: impl FnMut(EntityId),
) {
    let area = Aabb::new(
        pos.x - range,
        pos.y - range - 2.0,
        pos.z - range,
        pos.x + range,
        pos.y + range,
        pos.z + range,
    );
    store.for_each_in(&area, true, |id| {
        let Some(e) = store.get(id) else {
            return;
        };
        if except.is_some_and(|n| n == e.network_id().0) {
            return;
        }
        let p = e.pos();
        let (dx, dy, dz) = (p.x - pos.x, p.y - pos.y, p.z - pos.z);
        if dx * dx + dy * dy + dz * dz < range * range {
            f(id);
        }
    });
}

fn wire(pos: BlockPos) -> WirePos {
    WirePos {
        x: pos.x,
        y: pos.y,
        z: pos.z,
    }
}

fn center(pos: BlockPos) -> Vec3 {
    Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    )
}

impl LevelEffects {
    /// Sends the sounds and crack progress collected while the players were handled.
    pub(crate) fn flush(&mut self, entities: &Entities) {
        let store = &entities.store;
        for (breaker, pos, progress) in self.progress.drain(..) {
            let packet = SharedPacket::of(&BlockDestruction {
                id: breaker,
                pos: wire(pos),
                progress: u8::try_from(progress).unwrap_or(255),
            });
            players_near(store, center(pos), 32.0, Some(breaker), |id| {
                if let Some(p) = entities.player(id) {
                    p.connection.sink.send_shared(&packet);
                }
            });
        }
        for s in self.sounds.drain(..) {
            let packet = SharedPacket::of(&Sound::at(
                SoundEvent(i32::from(s.sound)),
                s.source,
                s.pos.x,
                s.pos.y,
                s.pos.z,
                s.volume,
                s.pitch,
                s.seed,
            ));
            let range = if s.volume > 1.0 {
                16.0 * f64::from(s.volume)
            } else {
                16.0
            };
            players_near(store, s.pos, range, s.except, |id| {
                if let Some(p) = entities.player(id) {
                    p.connection.sink.send_shared(&packet);
                }
            });
        }
    }
}

/// A random version 4 UUID (`Mth.createInsecureUUID`).
pub(crate) fn is_item(store: &EntityStore, id: EntityId) -> bool {
    store.get(id).is_some_and(|e| e.kind() == entity_ids::ITEM)
}

/// The rules of item entities for [`tick_item`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ItemSettings {
    /// Ticks an item lies on the ground before it despawns (Vanilla 6000).
    pub lifetime: i16,
    /// Horizontal merge radius (Vanilla 0.5); vertically 0.5 less.
    pub merge_radius: f64,
}

/// Reused buffers of [`tick_item`].
#[derive(Debug, Default)]
pub struct ItemScratch {
    boxes: Vec<vanillite_base::Aabb>,
    near: Vec<EntityId>,
    gone: Vec<EntityId>,
}

/// `ItemEntity.tick` of one item entity: movement, fluids, fire and lava, merging with its
/// neighbours, despawning. Draws from the item's own random. `level` is the item's level
/// (its entities are `entities`). Returns what fire, lava and water did to it: unless that
/// [`is_none`](item_entity::Burn::is_none), the caller passes it to
/// [`item_entity::burn_effects`] (game events, sounds, a destroyed item's removal).
#[allow(clippy::too_many_arguments)]
pub fn tick_item(
    entities: &mut Entities,
    level: &Level,
    id: EntityId,
    min_y: i32,
    settings: ItemSettings,
    scratch: &mut ItemScratch,
    fire: &item_entity::FireWorld<'_>,
) -> item_entity::Burn {
    let mut burn = item_entity::Burn::default();
    let lifetime = settings.lifetime;
    let view = level.view();
    let fluids = |aabb: Aabb| {
        let eye = Vec3::new(
            (aabb.min_x + aabb.max_x) / 2.0,
            aabb.min_y + f64::from(0.2125_f32),
            (aabb.min_z + aabb.max_z) / 2.0,
        );
        vanillite_game::mob::living::fluid_currents(level, fire.facts, aabb, eye)
    };
    scratch.gone.clear();
    let Entities { store, items, .. } = entities;
    let Some(entity) = store.get(id) else {
        return burn;
    };
    if entity.kind() != entity_ids::ITEM {
        return burn;
    }
    let row = entity.kind_row();
    burn.invulnerable = entity.flags().contains(EntityFlags::INVULNERABLE);
    let input = item_entity::TickInput {
        pos: entity.pos(),
        velocity: entity.velocity(),
        on_ground: entity.flags().contains(EntityFlags::ON_GROUND),
        network_id: entity.network_id().0,
        min_y,
        lifetime,
        fast_lava: level.settings.fast_lava,
        no_gravity: entity.flags().contains(EntityFlags::NO_GRAVITY),
    };
    let Some(data) = items.get_mut(row) else {
        return burn;
    };
    let ticked = item_entity::tick(
        data,
        input,
        &view,
        &fluids,
        fire,
        &mut scratch.boxes,
        &mut burn,
    );
    if ticked.discard {
        entities.despawn(id);
        return item_entity::Burn::default();
    }
    let rate = if ticked.moved_block { 2 } else { 40 };
    let merge = data.tick_count.is_multiple_of(rate) && data.is_mergable(lifetime);
    let on_fire = data.fire_ticks > 0;
    if let Some(mut e) = store.get_mut(id) {
        e.set_pos(ticked.pos);
        e.set_velocity(ticked.velocity);
        e.set_flags(EntityFlags::ON_GROUND, ticked.on_ground);
        if ticked.needs_sync {
            e.set_flags(EntityFlags::HAS_IMPULSE, true);
        }
        if e.as_ref().flags().contains(EntityFlags::ON_FIRE) != on_fire {
            e.set_flags(EntityFlags::ON_FIRE, on_fire);
            e.mark_data_dirty();
        }
    }
    if burn.destroyed {
        return burn;
    }
    if merge {
        merge_with_neighbours(
            entities,
            id,
            lifetime,
            settings.merge_radius,
            &mut scratch.near,
            &mut scratch.gone,
        );
    }
    for &gone in &scratch.gone {
        entities.despawn(gone);
    }
    // `age++` after the merge, then the lifetime.
    if !scratch.gone.contains(&id)
        && entities
            .item_mut(id)
            .is_some_and(|d| item_entity::age_tick(d, lifetime))
    {
        entities.despawn(id);
    }
    burn
}

/// `ItemEntity.teleport`: an item that arrived merges with its neighbours at once (when it
/// can be merged).
pub(crate) fn merge_after_teleport(entities: &mut Entities, id: EntityId, settings: ItemSettings) {
    if !entities
        .item(id)
        .is_some_and(|d| d.is_mergable(settings.lifetime))
    {
        return;
    }
    let mut near = Vec::new();
    let mut gone = Vec::new();
    merge_with_neighbours(
        entities,
        id,
        settings.lifetime,
        settings.merge_radius,
        &mut near,
        &mut gone,
    );
    for g in gone {
        entities.despawn(g);
    }
}

/// `ItemEntity.mergeWithNeighbours` within `radius` (Vanilla 0.5) horizontally and `radius`
/// minus 0.5 vertically.
fn merge_with_neighbours(
    entities: &mut Entities,
    id: EntityId,
    lifetime: i16,
    radius: f64,
    near: &mut Vec<EntityId>,
    gone: &mut Vec<EntityId>,
) {
    let Some(aabb) = entities.store.get(id).map(|e| e.aabb()) else {
        return;
    };
    near.clear();
    entities.store.for_each_in(
        &aabb.inflate(radius, radius - 0.5, radius),
        false,
        |other| {
            if other != id {
                near.push(other);
            }
        },
    );
    for &other in near.iter() {
        if gone.contains(&other) {
            continue;
        }
        let (Some(this), Some(that)) = (entities.item(id), entities.item(other)) else {
            continue;
        };
        if !that.is_mergable(lifetime) || !this.is_mergable(lifetime) {
            continue;
        }
        // `tryToMerge`.
        if this.target != that.target || !item_entity::are_mergable(&this.stack, &that.stack) {
            continue;
        }
        let (to, from) = if that.stack.count < this.stack.count {
            (id, other)
        } else {
            (other, id)
        };
        let Some(mut from_data) = entities.item(from).cloned() else {
            continue;
        };
        let Some(to_data) = entities.item_mut(to) else {
            continue;
        };
        to_data.stack = item_entity::merge_stacks(&to_data.stack, &mut from_data.stack);
        to_data.pickup_delay = to_data.pickup_delay.max(from_data.pickup_delay);
        to_data.age = to_data.age.min(from_data.age);
        if let Some(mut e) = entities.store.get_mut(to) {
            e.mark_data_dirty();
        }
        if let Some(from_item) = entities.item_mut(from) {
            from_item.stack = from_data.stack.clone();
        }
        if from_data.stack.is_empty() {
            gone.push(from);
            if from == id {
                break;
            }
        } else if let Some(mut e) = entities.store.get_mut(from) {
            e.mark_data_dirty();
        }
    }
}
