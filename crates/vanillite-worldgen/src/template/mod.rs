//! Structure templates (`world/level/levelgen/structure/templatesystem`): templates loaded from
//! the data packs' `structure/**.nbt` files, their placement with rotation, mirror, bounding
//! box clipping, block entities and waterlogging, and the structure processors.
//!
//! The `fossil` and `template` features place templates; the structures stage (jigsaw pieces,
//! ocean ruins, shipwrecks, …) builds on the same API:
//! [`StructureTemplateManager`] → [`StructureTemplate::place_in_world`] with
//! [`StructurePlaceSettings`] and processors from [`ProcessorLists`].
//!
//! Not ported yet: placing template entities (`placeEntities`; they are loaded and kept in
//! [`StructureTemplate::entities`]) and DataFixerUpper upgrades of templates saved by older
//! versions (the Vanilla pack only ships current data versions).

pub mod block_entity;
pub mod bounding_box;
pub mod liquid;
pub mod manager;
pub mod processor;
pub mod rule;
pub mod settings;
pub mod structure;
pub mod transform;
pub mod voxel;

use std::sync::Arc;

use serde_json::Value;
use vanillite_data::block::{BlockId, BlockState};
use vanillite_datapack::LoadedPacks;

pub use bounding_box::BoundingBox;
pub use manager::{StructureTemplateManager, TemplateRef};
pub use processor::{ProcessorList, ProcessorLists, StructureProcessor};
pub use settings::{LiquidSettings, SettingsRandom, StructurePlaceSettings, TemplateRandom};
pub use structure::{StructureBlockInfo, StructureTemplate};
pub use transform::{Mirror, Rotation};

use crate::Error;
use crate::blocks::qualify;

/// What parsing template users needs: the shared template cache and the processor lists.
#[derive(Debug)]
pub struct TemplateLoader {
    templates: Arc<StructureTemplateManager>,
    processor_lists: ProcessorLists,
}

impl TemplateLoader {
    /// A loader with a fresh template cache over the packs.
    pub fn new(packs: &LoadedPacks) -> Self {
        Self::with_manager(Arc::new(StructureTemplateManager::new(packs)))
    }

    /// A loader sharing an existing template cache.
    pub fn with_manager(templates: Arc<StructureTemplateManager>) -> Self {
        Self {
            templates,
            processor_lists: ProcessorLists::new(),
        }
    }

    /// The template cache.
    pub fn manager(&self) -> &Arc<StructureTemplateManager> {
        &self.templates
    }

    /// The template `id`, read when it is used (`StructureTemplateManager.getOrCreate(id)`).
    pub fn template(&self, id: &str) -> TemplateRef {
        TemplateRef::new(&self.templates, id)
    }

    /// A `Holder<StructureProcessorList>`.
    pub fn processor_list(
        &mut self,
        packs: &LoadedPacks,
        json: &Value,
    ) -> Result<Arc<ProcessorList>, Error> {
        self.processor_lists.get(packs, json)
    }
}

/// A string field, `minecraft:` qualified.
pub(crate) fn qualified_str(json: &Value, name: &str) -> Result<String, Error> {
    json.get(name)
        .and_then(Value::as_str)
        .map(qualify)
        .ok_or_else(|| Error::new(format!("missing string field {name}")))
}

/// `BlockStateParser.parseForBlock(blocks, text, true)`: `name[property=value,...]{nbt}`; the
/// NBT part is ignored. `None` when the text does not parse.
pub fn parse_state_string(text: &str) -> Option<BlockState> {
    let text = text.trim();
    let name_end = text.find(['[', '{']).unwrap_or(text.len());
    let block = BlockId::by_name(&qualify(text[..name_end].trim()))?;
    let mut state = block.default_state();
    let rest = &text[name_end..];
    if let Some(properties) = rest.strip_prefix('[') {
        let end = properties.find(']')?;
        let mut seen: Vec<&str> = Vec::new();
        for pair in properties[..end]
            .split(',')
            .filter(|p| !p.trim().is_empty())
        {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            if seen.contains(&key) {
                return None;
            }
            seen.push(key);
            state = state.with(key, value.trim())?;
        }
        let after = properties[end + 1..].trim_start();
        if !after.is_empty() && !after.starts_with('{') {
            return None;
        }
    } else if !rest.is_empty() && !rest.starts_with('{') {
        return None;
    }
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::packs;

    #[test]
    fn block_state_strings_parse() {
        let stairs =
            parse_state_string("minecraft:oak_stairs[facing=east,half=top]{a:1}").expect("stairs");
        assert_eq!(stairs.value("facing"), Some("east"));
        assert_eq!(stairs.value("half"), Some("top"));
        assert!(parse_state_string("minecraft:stone[nope=1]").is_none());
        assert!(parse_state_string("minecraft:air").is_some());
    }

    /// Every template of the Vanilla pack loads and every processor list parses.
    #[test]
    fn vanilla_templates_and_processor_lists_load() {
        let packs = packs();
        let mut loader = TemplateLoader::new(packs);
        let ids = loader.manager().list_templates().expect("list templates");
        assert!(ids.len() > 1000, "only {} templates", ids.len());
        for id in &ids {
            let template = loader
                .manager()
                .get(id)
                .unwrap_or_else(|e| panic!("{id}: {e}"))
                .unwrap_or_else(|| panic!("{id}: missing"));
            let size = template.size();
            assert!(size.x >= 0 && size.y >= 0 && size.z >= 0, "{id}");
            assert!(!template.palettes().is_empty(), "{id}");
        }
        let fossil = loader.template("minecraft:fossil/spine_1").get();
        assert!(!fossil.palettes()[0].is_empty());

        let registry = packs
            .registry("minecraft:worldgen/processor_list")
            .expect("processor lists");
        assert!(!registry.entries().is_empty());
        for entry in registry.entries() {
            loader
                .processor_list(packs, &Value::String(entry.id.clone()))
                .unwrap_or_else(|e| panic!("{}: {e}", entry.id));
        }
    }
}
