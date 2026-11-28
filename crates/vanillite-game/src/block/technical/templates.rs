//! The structure templates of a world for structure and test instance blocks
//! (`StructureTemplateManager` with its `generated/` directory, `StructureTemplate.fillFromWorld`
//! and `placeInWorld` on the live level).

use std::sync::Arc;

use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::BlockId;
use vanillite_worldgen::template::StructureProcessor;
use vanillite_worldgen::template::settings::StructurePlaceSettings;
use vanillite_worldgen::template::structure::{
    StructureBlockInfo, StructureEntityInfo, StructureTemplate,
};
use vanillite_worldgen::template::transform::{Mirror, Rotation};

use crate::level::Level;
use crate::level::place::PlaceArea;

/// The `StructurePlaceSettings` structure and test instance blocks place with.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaceSettings {
    /// `setMirror`.
    pub mirror: Mirror,
    /// `setRotation`.
    pub rotation: Rotation,
    /// `setIgnoreEntities`.
    pub ignore_entities: bool,
    /// `BlockRotProcessor` integrity (`1.0` places everything).
    pub integrity: f32,
    /// The seed of the integrity random (`StructureBlockEntity.createRandom`: `0` is the
    /// current time).
    pub seed: i64,
    /// `setKnownShape` (strict placement: no shape updates at the edge).
    pub strict: bool,
}

impl Default for PlaceSettings {
    fn default() -> Self {
        Self {
            mirror: Mirror::default(),
            rotation: Rotation::default(),
            ignore_entities: true,
            integrity: 1.0,
            seed: 0,
            strict: false,
        }
    }
}

/// `StructureBlockEntity.createRandom(seed)`: the seed, or the current time for `0`.
pub fn random_seed(seed: i64) -> i64 {
    if seed != 0 {
        return seed;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

impl Level {
    /// `StructureTemplateManager.get(id)`: the world's `generated/` templates first, then the
    /// data packs'. `None` if there is none.
    pub fn structure_template(&self, id: &str) -> Option<Arc<StructureTemplate>> {
        self.structure_templates()?.get(id).ok().flatten()
    }

    /// `getOrCreate(id)` filled in place without saving: `template` becomes the cached
    /// template of `id` (structure blocks saving on a redstone pulse).
    pub fn set_structure_template(&self, id: &str, template: StructureTemplate) {
        if let Some(templates) = self.structure_templates() {
            templates.put(id, template);
        }
    }

    /// `StructureTemplateManager.remove(id)`: the next read loads the files again.
    pub fn forget_structure_template(&self, id: &str) {
        if let Some(templates) = self.structure_templates() {
            templates.remove(id);
        }
    }

    /// `StructureTemplateManager.save(id)`: writes `template` to
    /// `generated/<namespace>/structure/<path>.nbt` and caches it; `false` if that fails.
    pub fn save_structure_template(&self, id: &str, template: StructureTemplate) -> bool {
        let Some(templates) = self.structure_templates() else {
            return false;
        };
        templates.put(id, template);
        match templates.save(id) {
            Ok(saved) => saved,
            Err(e) => {
                log::warn!("Failed to save {e}");
                false
            }
        }
    }

    /// `StructureTemplate.fillFromWorld(level, origin, size, includeEntities, ignoreBlocks)`:
    /// the blocks (with their block entity data) and, with `include_entities`, the entities
    /// other than players of the box from `origin` spanning `size` (negative sizes span the
    /// other way). A size below one on any axis gives an empty template.
    pub fn capture_structure(
        &self,
        origin: BlockPos,
        size: BlockPos,
        include_entities: bool,
        ignore: &[BlockId],
    ) -> StructureTemplate {
        if size.x < 1 || size.y < 1 || size.z < 1 {
            return StructureTemplate::default();
        }
        let corner = origin.offset(size.x - 1, size.y - 1, size.z - 1);
        let min = BlockPos::new(
            origin.x.min(corner.x),
            origin.y.min(corner.y),
            origin.z.min(corner.z),
        );
        let max = BlockPos::new(
            origin.x.max(corner.x),
            origin.y.max(corner.y),
            origin.z.max(corner.z),
        );
        let mut blocks = Vec::new();
        for z in min.z..=max.z {
            for y in min.y..=max.y {
                for x in min.x..=max.x {
                    let pos = BlockPos::new(x, y, z);
                    let state = self.block(pos);
                    if ignore.contains(&state.block()) {
                        continue;
                    }
                    // `blockEntity.saveWithId`: the data and its type, no position.
                    let nbt = self.block_entity_nbt(pos).map(|mut nbt| {
                        for key in ["x", "y", "z"] {
                            nbt.remove(key);
                        }
                        nbt
                    });
                    blocks.push(StructureBlockInfo {
                        pos: BlockPos::new(x - min.x, y - min.y, z - min.z),
                        state,
                        nbt,
                    });
                }
            }
        }
        let mut entities = Vec::new();
        if include_entities {
            // `AABB.encapsulatingFullBlocks(min, max)`.
            let aabb = Aabb::new(
                f64::from(min.x),
                f64::from(min.y),
                f64::from(min.z),
                f64::from(max.x) + 1.0,
                f64::from(max.y) + 1.0,
                f64::from(max.z) + 1.0,
            );
            let mut ids = Vec::new();
            self.entities
                .store
                .for_each_non_player_in(&aabb, |id| ids.push(id));
            for id in ids {
                let (Some(entity), Some(nbt)) = (self.entities.store.get(id), self.save_entity(id))
                else {
                    continue;
                };
                let p = entity.pos();
                let pos = Vec3::new(
                    p.x - f64::from(min.x),
                    p.y - f64::from(min.y),
                    p.z - f64::from(min.z),
                );
                entities.push(StructureEntityInfo {
                    pos,
                    block_pos: BlockPos::containing(pos.x, pos.y, pos.z),
                    nbt,
                });
            }
        }
        StructureTemplate::filled(size, blocks, entities)
    }

    /// `StructureTemplate.placeInWorld(level, pos, pos, settings, random, UPDATE_CLIENTS)` on
    /// the live level (strict placement adds `UPDATE_KNOWN_SHAPE | UPDATE_SKIP_ON_PLACE |
    /// UPDATE_SKIP_BLOCK_ENTITY_SIDEEFFECTS`, `816`); whether anything was placed. Chunks that
    /// are not loaded keep nothing.
    pub fn place_structure(
        &mut self,
        template: &StructureTemplate,
        pos: BlockPos,
        settings: &PlaceSettings,
    ) -> bool {
        let mut place = StructurePlaceSettings::new()
            .with_mirror(settings.mirror)
            .with_rotation(settings.rotation);
        place.ignore_entities = settings.ignore_entities;
        place.known_shape = settings.strict;
        let rot = [StructureProcessor::BlockRot {
            rottable_blocks: None,
            integrity: settings.integrity.clamp(0.0, 1.0),
        }];
        if settings.integrity < 1.0 {
            place.clear_processors();
            place.add_processors(&rot);
            place = place.with_placement_random();
        }
        let mut random = vanillite_base::random::LegacyRandom::new(random_seed(settings.seed));
        let flags = 2 | if settings.strict { 816 } else { 0 };
        let bounds =
            template.bounding_box_with(pos, settings.rotation, BlockPos::ZERO, settings.mirror);
        // One block around the box: the placed blocks update their neighbours.
        let min = BlockPos::new(bounds.min_x() - 1, bounds.min_y(), bounds.min_z() - 1).chunk();
        let max = BlockPos::new(bounds.max_x() + 1, bounds.max_y(), bounds.max_z() + 1).chunk();
        let mut area = PlaceArea::copy_plain(self, min, max);
        let seed = self.seed();
        let placed = area.run_plain(seed, |region| {
            template.place_in_world(region, pos, pos, &place, &mut random, flags)
        });
        area.apply(self);
        placed
    }
}
