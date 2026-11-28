//! Composters (`ComposterBlock`): players and hoppers put compostable items in, a full
//! composter gives bone meal. How many layers an item adds is the `minecraft:context_int_provider`
//! its `minecraft:compostable` component names, evaluated in a `BLOCK_INTERACT` loot context.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use serde_json::Value;
use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::{BlockId, BlockState, ids};
use vanillite_data::item::ITEMS;
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::StateExt;

use super::{BlockBehaviour, Interaction, ItemInteraction, UseContext, sound_id};
use crate::item::{self, ItemStack};
use crate::level::game_event::{GameEventContext, events};
use crate::level::{Level, SOUND_BLOCKS};
use crate::stats::{Stat, StatType};

/// A `minecraft:context_int_provider` as far as compostables use it.
#[derive(Clone, Debug, PartialEq)]
pub enum IntProvider {
    /// A plain number.
    Constant(i32),
    /// `minecraft:number_dispatcher`: the value of the first case whose condition holds.
    Dispatcher {
        cases: Vec<(BlockCondition, IntProvider)>,
        default: Box<IntProvider>,
    },
    /// `minecraft:weighted_list`: one entry by weight (`WeightedList.getRandomOrThrow`).
    Weighted(Vec<(IntProvider, i32)>),
}

/// A `minecraft:match_block` condition on the block of the loot context.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockCondition {
    /// Matching blocks (`None` = any).
    pub blocks: Option<Vec<BlockId>>,
    /// Required property values.
    pub state: Vec<(String, String)>,
}

impl BlockCondition {
    fn test(&self, state: BlockState) -> bool {
        self.blocks
            .as_ref()
            .is_none_or(|b| b.contains(&state.block()))
            && self
                .state
                .iter()
                .all(|(k, v)| state.value(k).is_some_and(|x| x == v))
    }

    fn parse(v: &Value) -> Option<BlockCondition> {
        if v["type"].as_str()? != "minecraft:match_block" {
            return None;
        }
        let blocks = match &v["blocks"] {
            Value::Null => None,
            Value::String(s) => Some(vec![BlockId::by_name(s)?]),
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(|b| b.as_str().and_then(BlockId::by_name))
                    .collect(),
            ),
            _ => return None,
        };
        let state = v["state"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| {
                        let v = match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        (k.clone(), v)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(BlockCondition { blocks, state })
    }
}

impl IntProvider {
    /// Parses the JSON of a provider (the subset compostables use).
    pub fn parse(v: &Value) -> Option<IntProvider> {
        if let Some(n) = v.as_i64() {
            return Some(IntProvider::Constant(i32::try_from(n).ok()?));
        }
        match v["type"].as_str()? {
            "minecraft:constant" => Some(IntProvider::Constant(
                i32::try_from(v["value"].as_i64()?).ok()?,
            )),
            "minecraft:number_dispatcher" => {
                let cases = v["cases"]
                    .as_array()?
                    .iter()
                    .map(|c| {
                        Some((
                            BlockCondition::parse(&c["condition"])?,
                            IntProvider::parse(&c["value"])?,
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?;
                let default = match v.get("default") {
                    Some(d) => IntProvider::parse(d)?,
                    None => IntProvider::Constant(0),
                };
                Some(IntProvider::Dispatcher {
                    cases,
                    default: Box::new(default),
                })
            }
            "minecraft:weighted_list" => {
                let entries = v["distribution"]
                    .as_array()?
                    .iter()
                    .map(|e| {
                        Some((
                            IntProvider::parse(&e["data"])?,
                            i32::try_from(e["weight"].as_i64()?).ok()?,
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(IntProvider::Weighted(entries))
            }
            _ => None,
        }
    }

    /// `getInt(context)` with the context's block state and random.
    pub fn get<R: RandomSource>(&self, state: BlockState, random: &mut R) -> i32 {
        match self {
            IntProvider::Constant(n) => *n,
            IntProvider::Dispatcher { cases, default } => cases
                .iter()
                .find(|(c, _)| c.test(state))
                .map_or(default.as_ref(), |(_, v)| v)
                .get(state, random),
            IntProvider::Weighted(entries) => {
                let total: i32 = entries.iter().map(|(_, w)| *w).sum();
                if total <= 0 {
                    return 0;
                }
                let mut selection = random.next_int_bounded(total);
                for (value, weight) in entries {
                    if selection < *weight {
                        return value.get(state, random);
                    }
                    selection -= weight;
                }
                0
            }
        }
    }
}

/// Loads the `minecraft:context_int_provider` entries the compostable items refer to (later
/// packs override). The registry's other entries (cooking, brewing) are read elsewhere.
pub fn load_providers(roots: &[PathBuf]) -> HashMap<String, IntProvider> {
    let mut files = BTreeMap::new();
    for root in roots {
        let Ok(namespaces) = std::fs::read_dir(root.join("data")) else {
            continue;
        };
        for ns in namespaces.flatten() {
            let ns_name = ns.file_name().to_string_lossy().into_owned();
            crate::loot::collect_json(
                &ns.path().join("context_int_provider"),
                &format!("{ns_name}:"),
                &mut files,
            );
        }
    }
    let used: HashSet<&str> = ITEMS.iter().map(|i| i.compostable).collect();
    files.retain(|id, _| used.contains(id.as_str()));
    let mut out = HashMap::new();
    for (id, json) in crate::loot::read_files(&files) {
        match json.ok().and_then(|v| IntProvider::parse(&v)) {
            Some(p) => {
                out.insert(id, p);
            }
            None => log::warn!("Unsupported context_int_provider {id}"),
        }
    }
    out
}

/// Fills the composter behaviours (its `onPlace` and `tick` are in [`super::redstone`]).
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    if block == ids::COMPOSTER {
        b.use_item_on = Some(use_item_on);
        b.use_without_item = Some(use_without_item);
    }
}

/// The compostable provider of a stack's item, if any.
pub(super) fn compostable(stack: &ItemStack) -> Option<IntProvider> {
    if stack.is_empty() {
        return None;
    }
    // The stack's own `compostable` (patched or removed): a number or a provider id.
    use vanillite_protocol::components::ids::COMPOSTABLE;
    if let Some(p) = stack.patch() {
        if let Some(v) = p.get(COMPOSTABLE) {
            let mut r = vanillite_protocol::codec::Reader::new(v);
            return match crate::recipe::read_resolvable(&mut r, false)? {
                Value::String(id) => {
                    crate::player::play::game_data().and_then(|d| d.compostables.get(&id).cloned())
                }
                n => Some(IntProvider::Constant(
                    n.as_i64().and_then(|n| i32::try_from(n).ok())?,
                )),
            };
        }
        if p.is_removed(COMPOSTABLE) {
            return None;
        }
    }
    let name = item::item_id(stack).info().compostable;
    if name.is_empty() {
        return None;
    }
    crate::player::play::game_data().and_then(|d| d.compostables.get(name).cloned())
}

/// `ComposterBlock.useItemOn`.
fn use_item_on(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    let fill_level = state.int("level").unwrap_or(0);
    let Some(provider) = compostable(stack).filter(|_| fill_level < 8) else {
        return ItemInteraction::TryWithEmptyHand;
    };
    if fill_level < 7 {
        let new_state = add_layer(level, Some(ctx.player), state, pos, &provider);
        level.level_event(None, 1500, pos, i32::from(new_state != state));
        level.award_stat(ctx.player, Stat::of(StatType::Used, stack.item), 1);
        stack.shrink(1);
    }
    ItemInteraction::Done(Interaction::Success)
}

/// `ComposterBlock.useWithoutItem`: a full composter gives its bone meal.
fn use_without_item(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
) -> Interaction {
    if state.int("level") == Some(8) {
        extract_produce(level, Some(ctx.player), state, pos);
        Interaction::Success
    } else {
        Interaction::Pass
    }
}

/// `ComposterBlock.insertItem` (hoppers and farmers): a layer for one compostable item.
pub fn insert_item(
    level: &mut Level,
    source: Option<EntityId>,
    state: BlockState,
    pos: BlockPos,
    stack: &mut ItemStack,
) -> BlockState {
    match compostable(stack) {
        Some(provider) if state.int("level").unwrap_or(0) < 7 => {
            let new_state = add_layer(level, source, state, pos, &provider);
            stack.shrink(1);
            new_state
        }
        _ => state,
    }
}

/// `ComposterBlock.extractProduce`: bone meal pops out, the composter empties.
pub fn extract_produce(
    level: &mut Level,
    source: Option<EntityId>,
    state: BlockState,
    pos: BlockPos,
) -> BlockState {
    // `Vec3.atLowerCornerWithOffset(pos, 0.5, 1.01, 0.5).offsetRandomXZ(random, 0.7F)`.
    let random = level.random();
    let dx = f64::from((random.next_float() - 0.5) * 0.7);
    let dz = f64::from((random.next_float() - 0.5) * 0.7);
    let at = Vec3::new(
        f64::from(pos.x) + 0.5 + dx,
        f64::from(pos.y) + 1.01,
        f64::from(pos.z) + 0.5 + dz,
    );
    let bone_meal = ItemStack::new(vanillite_data::item::ids::BONE_MEAL.0, 1);
    // `setDefaultPickUpDelay`: 10 ticks.
    super::drops::spawn_item_moving(level, at, bone_meal, None, 10, None);
    let empty = empty(level, source, state, pos);
    level.play_block_sound(
        None,
        pos,
        sound_id("block.composter.empty"),
        SOUND_BLOCKS,
        1.0,
        1.0,
    );
    empty
}

/// `ComposterBlock.empty`.
pub fn empty(
    level: &mut Level,
    source: Option<EntityId>,
    state: BlockState,
    pos: BlockPos,
) -> BlockState {
    let new_state = state.set_int("level", 0);
    level.set_block_and_update(pos, new_state);
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext {
            source,
            affected: Some(new_state),
            owner: None,
        },
    );
    new_state
}

/// `ComposterBlock.addLayer`.
pub(super) fn add_layer(
    level: &mut Level,
    source: Option<EntityId>,
    state: BlockState,
    pos: BlockPos,
    provider: &IntProvider,
) -> BlockState {
    let layers = provider.get(state, level.random());
    if layers <= 0 {
        return state;
    }
    let new_level = (state.int("level").unwrap_or(0) + layers).clamp(0, 7);
    let new_state = state.set_int("level", new_level);
    level.set_block_and_update(pos, new_state);
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext {
            source,
            affected: Some(new_state),
            owner: None,
        },
    );
    if new_level == 7 {
        level.schedule_block_tick(pos, state.block(), 20);
    }
    new_state
}

#[cfg(test)]
mod provider_tests {
    use super::*;

    /// Ids use `/` on every platform, and only the providers of compostable items are loaded.
    #[test]
    fn nested_provider_ids() {
        let root = std::env::temp_dir().join(format!("vanillite-compost-{}", std::process::id()));
        let dir = root.join("data/minecraft/context_int_provider");
        for (file, json) in [
            ("compostable/low.json", "1"),
            ("cooking/other.json", r#"{"type": "minecraft:div"}"#),
        ] {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, json).expect("write");
        }
        let providers = load_providers(std::slice::from_ref(&root));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            providers,
            [(
                "minecraft:compostable/low".to_owned(),
                IntProvider::Constant(1)
            )]
            .into()
        );
    }
}
