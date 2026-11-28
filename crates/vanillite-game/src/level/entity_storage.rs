//! Entities stored with their chunk (Vanilla's `entities/` records and
//! `PersistentEntitySectionManager`).
//!
//! - When a chunk becomes live its stored entities are created in stored order; kinds the game
//!   does not model yet stay parked as stored data and are written back unchanged.
//! - Before a chunk unloads and when the world saves, the live entities of the chunk are written
//!   back section by section from the bottom (`getExistingSectionsInChunk`), each in section
//!   order; after the unload they are removed.
//! - An entity ticks only while its chunk is entity ticking (`Visibility.TICKING`): the chunk
//!   runtime reports the changes and the entity store keeps its tick order like Vanilla's
//!   `startTicking` and `stopTicking`.
//!
//! Stored fields the game does not model (custom names, tags, custom data, …) are kept from the
//! loaded data and written back.

use std::sync::Arc;

use vanillite_base::{BlockPos, ChunkPos, Vec3};
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_nbt::{Budget, Compound, List, Tag};
use vanillite_world::chunks::Visibility;
use vanillite_world::entity::{EntityFlags, EntityId, NewEntity, Rotation};

use super::Level;
use crate::entities::PERSISTENT_DATA;
use crate::entities::{FallingBlockData, PrimedTntData};
use crate::item_entity::ItemData;

impl Level {
    /// Applies the visibility changes of the chunk runtime
    /// (`PersistentEntitySectionManager.updateChunkStatus`): a chunk that becomes visible
    /// queues its stored entities for [`Level::tick_entity_manager`], one that becomes hidden
    /// writes them back and removes them; entity ticking chunks put their entities in the tick
    /// order.
    pub(super) fn apply_visibility(&mut self) {
        let mut changes = std::mem::take(&mut self.visibility_scratch);
        self.chunks.take_visibility_changes(&mut changes);
        for change in &changes {
            let pos = change.pos;
            let ticking = change.new == Visibility::Ticking;
            if change.new == Visibility::Hidden {
                // A pending load completes before the unload (`storeChunkSections` waits for
                // it), so stored entities are never overwritten.
                if !self.chunks.entities_loaded(pos) {
                    self.load_entities(pos);
                }
                self.write_entities(pos);
                self.remove_entities(pos);
                self.chunks.set_entities_loaded(pos, false);
                self.write_poi(pos);
                self.entities.poi.unload_chunk(pos);
            }
            self.entities.store.set_chunk_ticking(pos.x, pos.z, ticking);
            if change.old == Visibility::Hidden {
                self.load_poi(pos);
                self.pending_entity_loads.push(pos);
            }
        }
        changes.clear();
        self.visibility_scratch = changes;
    }

    /// `PersistentEntitySectionManager.tick` at the end of the level tick
    /// (`processPendingLoads`): the chunks that became visible create their stored entities;
    /// from then on their scheduled ticks run (`isPositionTickingWithEntitiesLoaded`). The
    /// chunks load by x, then z: Vanilla creates them in the order their IO completes, which
    /// the parity recorder fixes to this order.
    pub fn tick_entity_manager(&mut self) {
        let mut pending = std::mem::take(&mut self.pending_entity_loads);
        pending.sort_by_key(|c| (c.x, c.z));
        for &pos in &pending {
            if self.chunks.visibility(pos) != Visibility::Hidden
                && !self.chunks.entities_loaded(pos)
            {
                self.load_entities(pos);
                self.chunks.set_entities_loaded(pos, true);
            }
        }
        pending.clear();
        self.pending_entity_loads = pending;
    }

    /// `postLoadProtoChunk` of a chunk that became full: the entities generation added (kept
    /// in the chunk's stored tags as `entities`, like Vanilla's proto chunks) join its stored
    /// entities and load with them. Generated entities have no UUID yet and get one here.
    /// Chunks saved full with generation entities still in their tags (earlier builds) are
    /// healed the same way when they load.
    pub(super) fn take_generation_entities(&mut self, chunk: ChunkPos) {
        /// The root key of the list in network NBT: list tag, name length, name.
        const KEY: &[u8] = b"\x09\x00\x08entities";
        let Some(bytes) = self.chunks.chunk(chunk).and_then(|c| c.extra.clone()) else {
            return;
        };
        if !bytes.windows(KEY.len()).any(|w| w == KEY) {
            return;
        }
        let Ok(Some(Tag::Compound(mut tags))) =
            vanillite_nbt::read::read_network(&mut &bytes[..], &mut Budget::file())
        else {
            return;
        };
        let Some(Tag::List(List(list))) = tags.remove("entities") else {
            return;
        };
        let mut rest = Vec::new();
        if vanillite_nbt::write::write_network_compound(&tags, &mut rest).is_err() {
            return;
        }
        self.chunks.set_stored_extra(chunk, Some(rest.into()));
        let loaded = self.chunks.entities_loaded(chunk);
        let mut stored = self.chunks.stored_entities(chunk).to_vec();
        for tag in list {
            let Tag::Compound(mut nbt) = tag else {
                continue;
            };
            if uuid_of(&nbt, "UUID").is_none() {
                nbt.insert("UUID", crate::mob::nbt::uuid_tag(crate::mob::new_uuid()));
            }
            if loaded {
                self.load_entity(nbt);
                continue;
            }
            let mut out = Vec::new();
            if vanillite_nbt::write::write_network_compound(&nbt, &mut out).is_ok() {
                stored.push(out.into());
            }
        }
        if !loaded {
            self.chunks.set_stored_entities(chunk, stored);
        }
    }

    /// Creates the stored entities of a chunk that became live (`addEntityWithoutEvent` in
    /// stored order).
    pub(super) fn load_entities(&mut self, chunk: ChunkPos) {
        let stored: Vec<Arc<[u8]>> = self.chunks.stored_entities(chunk).to_vec();
        if stored.is_empty() {
            return;
        }
        self.entity_chunks.insert(chunk);
        let mut parked = Vec::new();
        for data in stored {
            let mut input = &data[..];
            let Ok(Some(Tag::Compound(nbt))) =
                vanillite_nbt::read::read_network(&mut input, &mut Budget::file())
            else {
                continue;
            };
            let mut loaded = Vec::new();
            if self.load_with_passengers(nbt, &mut loaded, true).is_none() {
                // A kind the game does not model, the vehicle's or a passenger's: the whole
                // stack stays parked.
                self.discard_loaded(&loaded);
                parked.push(data);
            }
        }
        if !parked.is_empty() {
            self.parked_entities.insert(chunk, parked);
        }
    }

    /// Whether the entities of a chunk are in the world (`areEntitiesLoaded`): its stored
    /// entities were loaded, whether it has any or not.
    pub fn entities_loaded(&self, chunk: ChunkPos) -> bool {
        self.chunks.entities_loaded(chunk)
    }

    /// `EntityType.loadEntityRecursive` + `addFreshEntityWithPassengers` of stored entity data
    /// (a modelled kind, passengers of kinds that are not modelled left out); whether it loaded.
    pub fn load_entity(&mut self, nbt: Compound) -> bool {
        let mut loaded = Vec::new();
        self.load_with_passengers(nbt, &mut loaded, false).is_some()
    }

    /// [`Self::load_entity`] of an entity new to the world (placed with a structure): the
    /// plugins' spawn event fires for it and its passengers with `reason`
    /// (`vanillite_abi::spawn_reason`); the ones a plugin cancels are not added.
    pub fn load_new_entity(&mut self, nbt: Compound, reason: u32) -> bool {
        let mut loaded = Vec::new();
        let ok = self.load_with_passengers(nbt, &mut loaded, false).is_some();
        if self
            .entities
            .hooks_for(vanillite_abi::event::ENTITY_SPAWN)
            .is_some()
        {
            for id in loaded {
                if !self.entities.spawned(id, reason, 0) {
                    self.entity_nbt.remove(&id);
                }
            }
        }
        ok
    }

    /// [`Self::load_entity`] of an entity whose constructor already took `seed` from the
    /// entity seed stream (`/summon`): an arrow's own random starts from it instead of taking
    /// another seed.
    pub fn load_entity_seeded(&mut self, nbt: Compound, seed: Option<i64>) -> bool {
        self.next_entity_seed = seed;
        let loaded = self.load_entity(nbt);
        self.next_entity_seed = None;
        loaded
    }

    /// `teleportCrossDimension` into this level of an entity saved in another one, without its
    /// passengers: `EntityType.create` (a new entity: its random takes the next entity seed)
    /// and `restoreFrom` the stored data. Returns the new entity.
    pub fn load_travelled_entity(&mut self, nbt: Compound) -> Option<EntityId> {
        let uuid = uuid_of(&nbt, "UUID")?;
        self.next_entity_seed = Some(crate::mob::new_entity_seed());
        let loaded = self.load_entity(nbt);
        self.next_entity_seed = None;
        if !loaded {
            return None;
        }
        let columns = self.entities.store.columns();
        let row = columns.uuid.iter().rposition(|&u| u == uuid)?;
        columns.id.get(row).copied()
    }

    /// `EntityType.create` of a stored entity: `None` for kinds the game does not model,
    /// `Some(None)` for data that makes no entity (leash knots, empty items).
    fn create_entity(&mut self, nbt: Compound) -> Option<Option<EntityId>> {
        let next = self.next_entity_seed.take();
        let seed = next.unwrap_or_else(|| {
            uuid_of(&nbt, "UUID").map_or(0, |uuid| uuid as i64) ^ self.game_time()
        });
        if let Some(loaded) = crate::mob::nbt::load_mob(&nbt, seed) {
            let network_ids = Arc::clone(self.network_ids_arc());
            let id = crate::mob::spawn(
                &mut self.entities,
                &network_ids,
                loaded.uuid,
                loaded.pos,
                loaded.rot,
                loaded.data,
            );
            if let Some(mut e) = self.entities.store.get_mut(id) {
                e.set_velocity(loaded.velocity);
                e.set_flags(EntityFlags::ON_GROUND, loaded.on_ground);
            }
            // `Raider.readAdditionalSaveData`: back into its raid.
            crate::raid::rejoin_loaded(&mut self.entities, id);
            return Some(Some(id));
        }
        if let Some((kind, pos, velocity, rot, uuid, data)) =
            crate::mob::nbt::load_special(&nbt, seed)
        {
            let mut new = NewEntity::new(kind, self.network_ids().next(), uuid, pos);
            new.velocity = velocity;
            new.rot = rot;
            return Some(Some(self.entities.spawn_special(new, data)));
        }
        if let Some((kind, base, decor, flags)) = crate::decoration::load(&nbt) {
            let network_ids = Arc::clone(self.network_ids_arc());
            let id = crate::decoration::spawn(
                &mut self.entities,
                &network_ids,
                kind,
                base.uuid,
                base.pos,
                base.rot,
                decor,
            );
            if let Some(mut e) = self.entities.store.get_mut(id) {
                e.set_velocity(base.velocity);
                for f in [
                    EntityFlags::INVISIBLE,
                    EntityFlags::GLOWING,
                    EntityFlags::NO_GRAVITY,
                    EntityFlags::ON_GROUND,
                ] {
                    e.set_flags(f, flags.contains(f));
                }
            }
            return Some(Some(id));
        }
        if let Some((pos, uuid, cloud)) = crate::mob::nbt::load_cloud(&nbt) {
            let radius = cloud.radius;
            let new = NewEntity::new(ids::AREA_EFFECT_CLOUD, self.network_ids().next(), uuid, pos);
            let id = self.entities.spawn_cloud(new, cloud);
            if let Some(mut e) = self.entities.store.get_mut(id) {
                e.set_dims(vanillite_world::entity::Dimensions {
                    width: radius * 2.0,
                    height: 0.5,
                });
            }
            return Some(Some(id));
        }
        if let Some((pos, velocity, uuid, orb)) = crate::mob::nbt::load_orb(&nbt) {
            let mut new = NewEntity::new(ids::EXPERIENCE_ORB, self.network_ids().next(), uuid, pos);
            new.velocity = velocity;
            return Some(Some(self.entities.spawn_experience_orb(new, orb)));
        }
        if let Some((e, data)) = crate::mob::nbt::load_arrow(&nbt, next) {
            let kind = crate::mob::projectile::arrow_of(&data.item).0;
            let mut new = NewEntity::new(kind, self.network_ids().next(), e.uuid, e.pos);
            new.velocity = e.velocity;
            new.rot = e.rot;
            return Some(Some(self.entities.spawn_arrow(new, data)));
        }
        if let Some((e, data)) = crate::mob::missile::load(&nbt) {
            let mut new = NewEntity::new(
                data.kind.type_id(),
                self.network_ids().next(),
                e.uuid,
                e.pos,
            );
            new.velocity = e.velocity;
            new.rot = e.rot;
            return Some(Some(self.entities.spawn_missile(new, data)));
        }
        if let Some((e, data)) = crate::mob::nbt::load_boat(&nbt, seed) {
            let network_ids = Arc::clone(self.network_ids_arc());
            let id = crate::vehicle::spawn_boat(
                &mut self.entities,
                &network_ids,
                e.uuid,
                e.pos,
                e.rot.y_rot,
                data,
            );
            if let Some(mut ent) = self.entities.store.get_mut(id) {
                ent.set_velocity(e.velocity);
            }
            return Some(Some(id));
        }
        if let Some((e, data)) = crate::mob::nbt::load_minecart(&nbt, seed) {
            let network_ids = Arc::clone(self.network_ids_arc());
            let id = crate::minecart::spawn(
                &mut self.entities,
                &network_ids,
                e.uuid,
                e.pos,
                e.rot,
                data,
            );
            if let Some(mut ent) = self.entities.store.get_mut(id) {
                ent.set_velocity(e.velocity);
                ent.set_flags(EntityFlags::ON_GROUND, e.on_ground);
            }
            return Some(Some(id));
        }
        let kind = nbt
            .get("id")
            .and_then(Tag::as_str)
            .and_then(EntityTypeId::by_name)?;
        if kind == ids::LEASH_KNOT {
            // Knots come back with the leashes of their mobs.
            return Some(None);
        }
        if kind != ids::ITEM && kind != ids::FALLING_BLOCK && kind != ids::TNT {
            return None;
        }
        let base = Base::load(&nbt);
        let network_id = self.network_ids().next();
        let uuid = base.uuid.unwrap_or_else(|| self.new_uuid());
        let mut new = NewEntity::new(kind, network_id, uuid, base.pos);
        new.velocity = base.motion;
        new.rot = base.rot;
        new.flags.set(EntityFlags::ON_GROUND, base.on_ground);
        new.flags.set(EntityFlags::NO_GRAVITY, base.no_gravity);
        let short = |name: &str, default: i16| {
            crate::mob::entity_nbt::get_int(&nbt, name).map_or(default, |v| v as i16)
        };
        let id = if kind == ids::ITEM {
            let stack = match nbt.get("Item") {
                Some(Tag::Compound(c)) => crate::inventory::stack_from_nbt(c),
                _ => crate::item::ItemStack::EMPTY,
            };
            // `readAdditionalSaveData`: an item entity without an item is discarded.
            if stack.is_empty() {
                return Some(None);
            }
            let mut data = ItemData::new(
                stack,
                short("PickupDelay", 0),
                uuid_of(&nbt, "Thrower"),
                crate::item_entity::loaded_random(seed),
            );
            data.age = short("Age", 0);
            data.health = short("Health", 5);
            data.fire_ticks = i32::from(short("Fire", 0)).max(0);
            data.target = uuid_of(&nbt, "Owner");
            data.fall_distance =
                crate::mob::entity_nbt::get_double(&nbt, "fall_distance").unwrap_or(0.0);
            let items = &mut self.entities.items;
            self.entities.store.spawn(new, |id| items.push(id, data))
        } else if kind == ids::FALLING_BLOCK {
            let state = nbt
                .get("BlockState")
                .and_then(super::block_entity::read_state_codec)
                .unwrap_or_else(|| vanillite_data::block::ids::SAND.default_state());
            let mut data = FallingBlockData::new(
                state,
                flag(&nbt, "DropItem").unwrap_or(true),
                BlockPos::containing(base.pos.x, base.pos.y, base.pos.z),
                crate::mob::new_entity_seed(),
            );
            data.time = int(&nbt, "Time").unwrap_or(0);
            data.hurt_entities = flag(&nbt, "HurtEntities")
                .unwrap_or_else(|| crate::block::falling::is_anvil(state.block()));
            data.fall_hurt_amount =
                crate::mob::entity_nbt::get_float(&nbt, "FallHurtAmount").unwrap_or(0.0);
            data.fall_hurt_max = int(&nbt, "FallHurtMax").unwrap_or(40);
            data.cancel_drop = flag(&nbt, "CancelDrop").unwrap_or(false);
            data.fall_distance =
                crate::mob::entity_nbt::get_double(&nbt, "fall_distance").unwrap_or(0.0);
            self.entities.spawn_falling_block(new, data)
        } else {
            let state = nbt
                .get("block_state")
                .and_then(super::block_entity::read_state_codec)
                .unwrap_or_else(|| vanillite_data::block::ids::TNT.default_state());
            let power = crate::mob::entity_nbt::get_float(&nbt, "explosion_power")
                .map_or(4.0, |v| v.clamp(0.0, 128.0));
            let data = PrimedTntData {
                fuse: i32::from(short("fuse", 80)),
                state,
                owner: None,
                explosion_power: power,
                fall_distance: crate::mob::entity_nbt::get_double(&nbt, "fall_distance")
                    .unwrap_or(0.0),
            };
            self.entities.spawn_primed_tnt(new, data)
        };
        self.entity_nbt.insert(id, nbt);
        Some(Some(id))
    }

    /// Writes the live entities of a chunk and its parked ones back into the chunk's stored
    /// entities (`storeChunkSections`).
    pub(super) fn write_entities(&mut self, chunk: ChunkPos) {
        if self.chunks.chunk(chunk).is_none() {
            return;
        }
        let ids: Vec<EntityId> = self
            .entities
            .store
            .sections()
            .in_chunk(chunk.x, chunk.z)
            .collect();
        let mut list: Vec<Arc<[u8]>> = Vec::with_capacity(ids.len());
        for id in ids {
            // `shouldBeSaved`: passengers go with their root vehicle, a root vehicle with
            // exactly one player aboard goes with that player's data.
            if self.entities.vehicle_of(id).is_some() || self.player_passengers(id) == 1 {
                continue;
            }
            let Some(nbt) = self.save_with_passengers(id) else {
                continue;
            };
            let mut bytes = Vec::new();
            if vanillite_nbt::write::write_network_compound(&nbt, &mut bytes).is_ok() {
                list.push(bytes.into());
            }
        }
        if let Some(parked) = self.parked_entities.get(&chunk) {
            list.extend(parked.iter().cloned());
        }
        if list.is_empty() {
            self.entity_chunks.remove(&chunk);
        } else {
            self.entity_chunks.insert(chunk);
        }
        self.chunks.set_stored_entities(chunk, list);
    }

    /// `EntityType.loadEntityRecursive`: an entity with its `Tags` and the passengers stored in
    /// its `Passengers`, each riding it. The created entities are added to `loaded`; `None` if the
    /// entity's kind is not modelled, or with `strict` a passenger's (the caller discards what
    /// was created). Without `strict` such passengers are left out, as Vanilla drops entities
    /// it cannot load.
    fn load_with_passengers(
        &mut self,
        mut nbt: Compound,
        loaded: &mut Vec<EntityId>,
        strict: bool,
    ) -> Option<Option<EntityId>> {
        let passengers = match nbt.remove("Passengers") {
            Some(Tag::List(list)) => list.0,
            _ => Vec::new(),
        };
        let tags: Vec<String> = match nbt.get("Tags") {
            Some(Tag::List(list)) => list
                .0
                .iter()
                .filter_map(|t| t.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        let persistent = match nbt.get(PERSISTENT_DATA) {
            Some(Tag::Compound(c)) => Some(c.clone()),
            _ => None,
        };
        let base = crate::mob::entity_nbt::base_keys(&nbt);
        let Some(vehicle) = self.create_entity(nbt)? else {
            return Some(None);
        };
        crate::mob::entity_nbt::load_base(&mut self.entities, vehicle, &base, false);
        self.entities.set_tags(vehicle, tags);
        self.entities.set_persistent_data(vehicle, persistent);
        loaded.push(vehicle);
        for tag in passengers {
            let Tag::Compound(c) = tag else {
                continue;
            };
            let passenger = match self.load_with_passengers(c, loaded, strict) {
                Some(passenger) => passenger,
                None if strict => return None,
                None => None,
            };
            if let Some(passenger) = passenger {
                self.entities.riding.add(passenger, vehicle, false);
                if let Some(mut e) = self.entities.store.get_mut(passenger) {
                    e.set_flags(EntityFlags::PASSENGER, true);
                }
            }
        }
        Some(Some(vehicle))
    }

    /// Removes entities created by a load that is given up.
    fn discard_loaded(&mut self, loaded: &[EntityId]) {
        for &id in loaded {
            self.entities.despawn(id);
            self.entity_nbt.remove(&id);
        }
    }

    /// `getRootVehicle`.
    fn root_vehicle(&self, mut id: EntityId) -> EntityId {
        while let Some(vehicle) = self.entities.vehicle_of(id) {
            id = vehicle;
        }
        id
    }

    /// `countPlayerPassengers`: the players among the direct and indirect passengers.
    fn player_passengers(&self, id: EntityId) -> usize {
        self.entities
            .riding
            .passengers_of(id)
            .iter()
            .map(|&p| usize::from(self.entities.player(p).is_some()) + self.player_passengers(p))
            .sum()
    }

    /// `Entity.saveWithoutId` with `Tags` and `Passengers`: the entity and the entities riding
    /// it; players and kinds that are not saved are left out.
    fn save_with_passengers(&self, id: EntityId) -> Option<Compound> {
        let mut nbt = self.save_entity(id)?;
        // `Entity.tags`, as `/tag` left them.
        let tags = self.entities.tags_of(id);
        if tags.is_empty() {
            nbt.remove("Tags");
        } else {
            nbt.insert(
                "Tags",
                Tag::List(List(tags.iter().map(|t| Tag::String(t.clone())).collect())),
            );
        }
        let passengers: Vec<Tag> = self
            .entities
            .riding
            .passengers_of(id)
            .iter()
            .filter_map(|&p| self.save_with_passengers(p))
            .map(Tag::Compound)
            .collect();
        if passengers.is_empty() {
            nbt.remove("Passengers");
        } else {
            nbt.insert("Passengers", Tag::List(List(passengers)));
        }
        Some(nbt)
    }

    /// `ServerPlayer.saveParentVehicle`: a riding player's `RootVehicle` (the vehicle it rides
    /// as `Attach`, the root vehicle with its passengers as `Entity`), when the root vehicle
    /// carries no other player.
    pub fn save_root_vehicle(&self, player: EntityId) -> Option<Compound> {
        let vehicle = self.entities.vehicle_of(player)?;
        let root = self.root_vehicle(vehicle);
        if self.player_passengers(root) != 1 {
            return None;
        }
        let entity = self.save_with_passengers(root)?;
        let mut out = Compound::with_capacity(2);
        out.insert("Attach", uuid_tag(self.entities.store.get(vehicle)?.uuid()));
        out.insert("Entity", Tag::Compound(entity));
        Some(out)
    }

    /// `PlayerList.remove` of a riding player: a root vehicle that carries no other player
    /// leaves the world with it, with all its passengers (`UNLOADED_WITH_PLAYER`).
    pub fn remove_root_vehicle(&mut self, player: EntityId) {
        let Some(vehicle) = self.entities.vehicle_of(player) else {
            return;
        };
        let root = self.root_vehicle(vehicle);
        if self.player_passengers(root) != 1 {
            return;
        }
        self.entities.riding.remove(player);
        if let Some(mut e) = self.entities.store.get_mut(player) {
            e.set_flags(EntityFlags::PASSENGER, false);
        }
        let mut stack = vec![root];
        let mut all = Vec::new();
        while let Some(id) = stack.pop() {
            stack.extend_from_slice(self.entities.riding.passengers_of(id));
            all.push(id);
        }
        self.discard_loaded(&all);
    }

    /// `ServerPlayer.loadAndSpawnParentVehicle`: creates the saved root vehicle with its
    /// passengers and lets the player ride the one it rode; without it the vehicles are
    /// discarded again.
    pub fn spawn_root_vehicle(&mut self, player: EntityId, data: &Compound) {
        let Some(Tag::Compound(entity)) = data.get("Entity") else {
            return;
        };
        let attach = uuid_of(data, "Attach");
        let mut loaded = Vec::new();
        let target = match self.load_with_passengers(entity.clone(), &mut loaded, false) {
            Some(Some(_)) => loaded.iter().copied().find(|&id| {
                self.entities
                    .store
                    .get(id)
                    .is_some_and(|e| Some(e.uuid()) == attach)
            }),
            _ => None,
        };
        let Some(vehicle) = target else {
            log::warn!("Couldn't reattach entity to player");
            self.discard_loaded(&loaded);
            return;
        };
        self.entities.riding.add(player, vehicle, true);
        if let Some(mut e) = self.entities.store.get_mut(player) {
            e.set_flags(EntityFlags::PASSENGER, true);
        }
        if let Some(p) = self.entities.player_mut(player) {
            p.connection.movement.passenger = true;
        }
    }

    /// `Entity.save`: the stored data of a live entity of a modelled kind, `None` for players
    /// and kinds that are not saved. Its tags are saved with it.
    pub fn save_entity(&self, id: EntityId) -> Option<Compound> {
        let mut nbt = self.save_entity_kind(id)?;
        crate::mob::entity_nbt::save_base(&self.entities, id, &mut nbt);
        let tags = self.entities.tags_of(id);
        if tags.is_empty() {
            nbt.remove("Tags");
        } else {
            nbt.insert(
                "Tags",
                List(tags.iter().map(|t| Tag::String(t.clone())).collect()),
            );
        }
        match self.entities.persistent_data(id) {
            Some(data) => nbt.insert(PERSISTENT_DATA, data.clone()),
            None => {
                nbt.remove(PERSISTENT_DATA);
            }
        }
        Some(nbt)
    }

    /// [`Level::save_entity`] without the tags.
    fn save_entity_kind(&self, id: EntityId) -> Option<Compound> {
        let entity = self.entities.store.get(id)?;
        let kind = entity.kind();
        let ents = &self.entities;
        if crate::entities::is_mob_kind(kind) {
            let leash = self.leash_of(id);
            return ents.mob(id).filter(|m| m.is_alive()).map(|m| {
                if leash.is_some() {
                    let mut m = m.clone();
                    m.pending_leash = leash;
                    crate::mob::nbt::save_mob(&entity, &m)
                } else {
                    crate::mob::nbt::save_mob(&entity, m)
                }
            });
        }
        if let Some(b) = ents.boat(id) {
            let leash = self.leash_of(id).or(b.pending_leash);
            return Some(crate::mob::nbt::save_boat(&entity, b, leash));
        }
        if let Some(m) = ents.minecart(id) {
            return Some(crate::mob::nbt::save_minecart(&entity, m));
        }
        if crate::mob::projectile::is_arrow_kind(kind) {
            return ents.arrow(id).map(|a| {
                let owner = a.owner.and_then(|o| ents.store.get(o)).map(|o| o.uuid());
                crate::mob::nbt::save_arrow(&entity, a, owner)
            });
        }
        if let Some(d) = ents.missile(id) {
            let owner = d.owner.and_then(|o| ents.store.get(o)).map(|o| o.uuid());
            return Some(crate::mob::missile::save(&entity, d, owner));
        }
        if kind == ids::EXPERIENCE_ORB {
            return self
                .entities
                .experience_orb(id)
                .map(|o| crate::mob::nbt::save_orb(&entity, o));
        }
        if let Some(s) = ents.special(id) {
            return crate::mob::nbt::save_special(&entity, s);
        }
        if let Some(c) = ents.cloud(id) {
            return Some(crate::mob::nbt::save_cloud(&entity, c));
        }
        if ents.decor(id).is_some() {
            return crate::decoration::save(ents, id);
        }
        if kind != ids::ITEM && kind != ids::FALLING_BLOCK && kind != ids::TNT {
            return None;
        }
        if entity.flags().contains(EntityFlags::PASSENGER) {
            return None;
        }
        let mut nbt = self.entity_nbt.get(&id).cloned().unwrap_or_default();
        nbt.insert("id", kind.name());
        let pos = entity.pos();
        let motion = entity.velocity();
        let rot = entity.rot();
        nbt.insert("Pos", doubles(pos));
        nbt.insert("Motion", doubles(motion));
        nbt.insert(
            "Rotation",
            List(vec![Tag::Float(rot.y_rot), Tag::Float(rot.x_rot)]),
        );
        nbt.insert("OnGround", entity.flags().contains(EntityFlags::ON_GROUND));
        if entity.flags().contains(EntityFlags::NO_GRAVITY) {
            nbt.insert("NoGravity", true);
        } else {
            nbt.remove("NoGravity");
        }
        nbt.insert("UUID", uuid_tag(entity.uuid()));
        // Fields the game does not model keep their loaded value, else Vanilla's default.
        or_default(&mut nbt, "fall_distance", Tag::Double(0.0));
        or_default(&mut nbt, "Fire", Tag::Short(0));
        or_default(&mut nbt, "Air", Tag::Short(300));
        or_default(&mut nbt, "Invulnerable", Tag::Byte(0));
        or_default(&mut nbt, "PortalCooldown", Tag::Int(0));
        if kind == ids::ITEM {
            let data = self.entities.item(id)?;
            nbt.insert("Health", data.health);
            #[allow(clippy::cast_possible_truncation)]
            nbt.insert("Fire", data.fire_ticks.clamp(0, 32767) as i16);
            nbt.insert("Age", data.age);
            nbt.insert("PickupDelay", data.pickup_delay);
            nbt.insert("fall_distance", Tag::Double(data.fall_distance));
            match data.thrower {
                Some(thrower) => nbt.insert("Thrower", uuid_tag(thrower)),
                None => {
                    nbt.remove("Thrower");
                }
            }
            match data.target {
                Some(target) => nbt.insert("Owner", uuid_tag(target)),
                None => {
                    nbt.remove("Owner");
                }
            }
            if data.stack.is_empty() {
                nbt.remove("Item");
            } else {
                nbt.insert("Item", crate::inventory::stack_to_nbt(&data.stack));
            }
        } else if kind == ids::FALLING_BLOCK {
            let data = self.entities.falling_block(id)?;
            nbt.insert(
                "BlockState",
                super::block_entity::write_state_codec(data.state),
            );
            nbt.insert("Time", data.time);
            nbt.insert("DropItem", data.drop_item);
            nbt.insert("HurtEntities", data.hurt_entities);
            nbt.insert("FallHurtAmount", Tag::Float(data.fall_hurt_amount));
            nbt.insert("FallHurtMax", Tag::Int(data.fall_hurt_max));
            nbt.insert("CancelDrop", data.cancel_drop);
            nbt.insert("fall_distance", Tag::Double(data.fall_distance));
        } else {
            let data = self.entities.primed_tnt(id)?;
            nbt.insert("fuse", data.fuse as i16);
            nbt.insert("fall_distance", Tag::Double(data.fall_distance));
            nbt.insert(
                "block_state",
                super::block_entity::write_state_codec(data.state),
            );
            if data.explosion_power != 4.0 {
                nbt.insert("explosion_power", data.explosion_power);
            } else {
                nbt.remove("explosion_power");
            }
            if let Some(owner) = data.owner.and_then(|o| self.entities.store.get(o)) {
                nbt.insert("owner", uuid_tag(owner.uuid()));
            }
        }
        Some(nbt)
    }

    /// The leash holder of a mob or boat as stored: a knot by its block, else the holder's UUID.
    fn leash_of(&self, id: EntityId) -> Option<crate::mob::LeashRef> {
        let ents = &self.entities;
        let holder = ents.store.get(*ents.leashes.get(&id)?)?;
        if holder.kind() == ids::LEASH_KNOT
            && let Some(&pos) = ents.knots.get(holder.kind_row())
        {
            return Some(crate::mob::LeashRef::Knot(pos));
        }
        Some(crate::mob::LeashRef::Entity(holder.uuid()))
    }

    /// Removes the entities of a chunk that unloaded (they were written back before).
    pub(super) fn remove_entities(&mut self, chunk: ChunkPos) {
        let ids: Vec<EntityId> = self
            .entities
            .store
            .sections()
            .in_chunk(chunk.x, chunk.z)
            .filter(|&id| {
                self.entities
                    .store
                    .get(id)
                    .is_some_and(|e| e.kind() != ids::PLAYER)
            })
            .collect();
        for id in ids {
            self.entities.despawn(id);
            self.entity_nbt.remove(&id);
        }
        self.parked_entities.remove(&chunk);
        self.entity_chunks.remove(&chunk);
    }

    /// Loads the POI of a chunk that became live and checks them against its blocks
    /// (`PoiManager.checkConsistencyWithBlocks` of every section).
    pub(super) fn load_poi(&mut self, chunk: ChunkPos) {
        self.flush_poi_changes();
        let Some(c) = self.chunks.chunk(chunk) else {
            return;
        };
        let stored = c.poi.as_deref().and_then(|bytes| {
            match vanillite_nbt::read::read_network(&mut &bytes[..], &mut Budget::file()) {
                Ok(Some(Tag::Compound(root))) => Some(root),
                _ => {
                    log::warn!("Unreadable POI of chunk {}, {}", chunk.x, chunk.z);
                    None
                }
            }
        });
        let min = c.chunk.min_section();
        let blocks: Vec<SectionBlocks<'_>> = c
            .chunk
            .sections()
            .iter()
            .map(|s| SectionBlocks(s))
            .collect();
        let mut iter = blocks.iter().enumerate().map(|(i, s)| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let y = min + i as i32;
            (y, s as &dyn crate::poi::ChunkSectionBlocks)
        });
        self.entities.poi.load_chunk(chunk, stored, &mut iter);
    }

    /// Writes the POI of a live chunk back into its stored record if they changed.
    pub(super) fn write_poi(&mut self, chunk: ChunkPos) {
        if !self.entities.poi.is_dirty(chunk) {
            return;
        }
        let bytes = self.entities.poi.save_chunk(chunk).and_then(|root| {
            let mut bytes = Vec::new();
            vanillite_nbt::write::write_network_compound(&root, &mut bytes).ok()?;
            Some(Arc::<[u8]>::from(bytes))
        });
        self.chunks.set_stored_poi(chunk, bytes);
    }

    /// Writes the changed POI of every live chunk back (before a save).
    pub(super) fn write_all_poi(&mut self) {
        self.flush_poi_changes();
        for chunk in self.entities.poi.dirty_chunks() {
            self.write_poi(chunk);
        }
    }

    /// Writes the entities of every chunk that has or had entities back (before a save).
    pub(super) fn write_all_entities(&mut self) {
        let mut chunks: Vec<ChunkPos> = self.entity_chunks.iter().copied().collect();
        chunks.extend(
            self.entities
                .store
                .sections()
                .chunks()
                .map(|(x, z)| ChunkPos::new(x, z)),
        );
        chunks.sort_unstable_by_key(|c| (c.x, c.z));
        chunks.dedup();
        for chunk in chunks {
            // Chunks whose entities are still loading keep their stored entities
            // (`storeChunkSections` of a pending chunk).
            if self.chunks.visibility(chunk) != Visibility::Hidden
                && self.chunks.entities_loaded(chunk)
            {
                self.write_entities(chunk);
            }
        }
        let store = &self.entities.store;
        self.entity_nbt.retain(|&id, _| store.contains(id));
    }
}

/// The fields of `Entity.load` the game models.
struct Base {
    pos: Vec3,
    motion: Vec3,
    rot: Rotation,
    on_ground: bool,
    no_gravity: bool,
    uuid: Option<u128>,
}

impl Base {
    fn load(nbt: &Compound) -> Base {
        let list = |name: &str| -> Vec<f64> {
            match nbt.get(name) {
                Some(Tag::List(list)) => list
                    .0
                    .iter()
                    .map(|t| match t {
                        Tag::Double(v) => *v,
                        Tag::Float(v) => f64::from(*v),
                        _ => 0.0,
                    })
                    .collect(),
                _ => Vec::new(),
            }
        };
        let vec = |v: Vec<f64>| {
            if v.len() == 3 {
                Vec3::new(v[0], v[1], v[2])
            } else {
                Vec3::ZERO
            }
        };
        let pos = vec(list("Pos"));
        let motion = vec(list("Motion"));
        // Motion above 10 blocks per tick is dropped per axis.
        let clamp = |v: f64| if v.abs() > 10.0 { 0.0 } else { v };
        let rotation = list("Rotation");
        let (y_rot, x_rot) = match rotation[..] {
            [y, x] => (y as f32, x as f32),
            _ => (0.0, 0.0),
        };
        let y_rot = if y_rot.is_finite() {
            y_rot % 360.0
        } else {
            0.0
        };
        let x_rot = if x_rot.is_finite() {
            x_rot.clamp(-90.0, 90.0) % 360.0
        } else {
            0.0
        };
        Base {
            pos: Vec3::new(
                pos.x.clamp(-3.000_051_2e7, 3.000_051_2e7),
                pos.y.clamp(-2.0e7, 2.0e7),
                pos.z.clamp(-3.000_051_2e7, 3.000_051_2e7),
            ),
            motion: Vec3::new(clamp(motion.x), clamp(motion.y), clamp(motion.z)),
            rot: Rotation {
                y_rot,
                x_rot,
                y_head_rot: y_rot,
            },
            on_ground: flag(nbt, "OnGround").unwrap_or(false),
            no_gravity: flag(nbt, "NoGravity").unwrap_or(false),
            uuid: uuid_of(nbt, "UUID"),
        }
    }
}

/// Inserts `value` unless the compound has `name`.
fn or_default(nbt: &mut Compound, name: &str, value: Tag) {
    if nbt.get(name).is_none() {
        nbt.insert(name, value);
    }
}

fn int(nbt: &Compound, name: &str) -> Option<i32> {
    crate::mob::entity_nbt::get_int(nbt, name)
}

fn flag(nbt: &Compound, name: &str) -> Option<bool> {
    crate::mob::entity_nbt::get_bool(nbt, name)
}

fn doubles(v: Vec3) -> List {
    List(vec![Tag::Double(v.x), Tag::Double(v.y), Tag::Double(v.z)])
}

/// `UUIDUtil.CODEC`: four ints, most significant first.
fn uuid_tag(uuid: u128) -> Tag {
    Tag::IntArray(
        (0..4)
            .map(|i| (uuid >> (96 - 32 * i)) as u32 as i32)
            .collect(),
    )
}

fn uuid_of(nbt: &Compound, name: &str) -> Option<u128> {
    match nbt.get(name) {
        Some(Tag::IntArray(v)) if v.len() == 4 => Some(
            v.iter()
                .fold(0u128, |acc, &part| acc << 32 | u128::from(part as u32)),
        ),
        _ => None,
    }
}

/// A chunk section as the POI manager scans it.
struct SectionBlocks<'a>(&'a vanillite_chunk::Section);

impl crate::poi::ChunkSectionBlocks for SectionBlocks<'_> {
    fn may_have_poi(&self) -> bool {
        self.0
            .blocks()
            .may_contain(|v| crate::poi::PoiType::has_poi(vanillite_data::block::BlockState(v)))
    }

    fn state(&self, x: u32, y: u32, z: u32) -> vanillite_data::block::BlockState {
        vanillite_data::block::BlockState(self.0.block(x, y, z))
    }
}
