//! Loot tables (Vanilla `LootTable`, 26.3 format) of block drops, entity drops and containers.
//!
//! [`LootTables::load`] parses every `data/<ns>/loot_table/{blocks,chests,archaeology,pots}/**.json`
//! of the given packs once into a compact typed tree, together with the named predicates
//! (`data/<ns>/predicate/`) and item modifiers (`data/<ns>/item_modifier/`) they may refer to.
//! [`LootTables::block_drops`] evaluates the table of a broken block state, [`LootTables::roll`]
//! any table against a [`LootContext`] and [`LootTables::fill`] fills a container like
//! `LootTable.fill`, with Vanilla's rolls and formulas:
//!
//! - pools: `condition`, `modifier`, `rolls + floor(bonus_rolls * luck)`; one roll expands the
//!   entries (composite entries `alternatives`, `sequence`, `group`), keeps those with a
//!   positive weight (`weight + quality * luck`) and picks one by weight;
//! - entries: `item`, `tag`, `empty`, `loot_table` (other loaded tables or inline ones),
//!   `dynamic` (block entity contents: none yet);
//! - conditions: `survives_explosion`, `match_tool`, `match_block`, `table_bonus`,
//!   `random_chance`, `random_chance_with_enchanted_bonus`, `inverted`, `any_of`, `all_of`,
//!   `entity_properties` and `location_check` (best effort, see below), named predicates;
//! - modifiers: `set_count`, `apply_bonus`, `explosion_decay`, `limit_count`, `copy_state`,
//!   `sequence`, `discard`, `copy_components` and `copy_name` from the block entity,
//!   `enchant_randomly`, `enchant_with_levels`, `set_enchantments`, `set_damage`, `set_name`,
//!   `set_potion`, `set_stew_effect`, `set_instrument`, `set_ominous_bottle_amplifier`,
//!   `set_components`, `filtered`, named item modifiers; `copy_custom_data` and
//!   `enchanted_count_increase` do nothing (no attacking entity); `exploration_map` needs the
//!   context's level access;
//! - results larger than the item's max stack size are split as `LootTable.createStackSplitter`.
//!
//! The context has no level access, so:
//! - `entity_properties` on `this` with an empty predicate holds if the context has a breaker,
//!   an explosion source, a `this` entity or entity loot parameters (`this_present`); any
//!   non-empty entity predicate needs entity loot parameters;
//! - `location_check` with only `block`/`fluid` predicates holds (the world is assumed
//!   consistent, e.g. the other half of a double plant exists); `biomes` asks the context for
//!   the biome at the origin; other location predicates are unsupported.
//!
//! Unsupported conditions evaluate to false, unsupported modifiers do nothing; each kind is
//! logged once at load.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use vanillite_base::Vec3;
use vanillite_base::random::RandomSource;
use vanillite_data::block::{BLOCKS, BlockId, BlockState, PROPERTIES};
use vanillite_data::item::{ITEMS, ItemId, ids as items};
use vanillite_protocol::codec::{Reader, write_len, write_str};
use vanillite_protocol::components::ids;
use vanillite_protocol::item::ItemStack;

/// Most nested tables, predicate and modifier references one evaluation follows (Vanilla
/// detects reference cycles; this bounds them).
const MAX_DEPTH: u32 = 32;
/// Longest block state property key or value read back from a `block_state` component.
const MAX_STATE_STRING: usize = 32767;

/// The random numbers loot evaluation draws (object safe; every Vanilla [`RandomSource`] is one).
pub trait LootRandom {
    /// `nextInt(bound)`; `bound` is always positive.
    fn next_int(&mut self, bound: i32) -> i32;
    /// `nextFloat()`: uniform in `[0, 1)`.
    fn next_float(&mut self) -> f32;
    /// `nextBoolean()` (the default equals it for `LegacyRandomSource`).
    fn next_boolean(&mut self) -> bool {
        self.next_int(2) == 1
    }
}

impl<R: RandomSource> LootRandom for R {
    fn next_int(&mut self, bound: i32) -> i32 {
        self.next_int_bounded(bound)
    }

    fn next_float(&mut self) -> f32 {
        RandomSource::next_float(self)
    }

    fn next_boolean(&mut self) -> bool {
        RandomSource::next_boolean(self)
    }
}

/// `Mth.nextInt(random, min, max)`.
fn next_int_between(rng: &mut dyn LootRandom, min: i32, max: i32) -> i32 {
    if min >= max {
        return min;
    }
    match max.checked_sub(min).and_then(|d| d.checked_add(1)) {
        Some(bound) => rng.next_int(bound).wrapping_add(min),
        None => min,
    }
}

/// The parameters of a loot roll (Vanilla `LootContextParams` and the context around them);
/// every parameter is optional, as in Vanilla's parameter sets.
pub struct LootContext<'a> {
    /// `origin`.
    pub origin: Option<Vec3>,
    /// `this_entity` (the player opening a container, the brushing player, …).
    pub this_entity: Option<vanillite_world::entity::EntityId>,
    /// `block_state`.
    pub block_state: Option<BlockState>,
    /// `tool` (may be an empty stack).
    pub tool: Option<&'a ItemStack>,
    /// `explosion_radius`.
    pub explosion_radius: Option<f32>,
    /// The components of the block entity (`block_entity`, `collectComponents`), by component
    /// type id, as wire values.
    pub block_entity: Option<&'a dyn Fn(u16) -> Option<Box<[u8]>>>,
    /// `LootParams.getLuck`.
    pub luck: f32,
    /// Enchantment level on the tool by enchantment id, e.g. `"minecraft:fortune"` → 3 (0 if
    /// none).
    pub enchantment_level: &'a dyn Fn(&str) -> i32,
    /// Whether an item is in an item tag (tag id without '#').
    pub item_in_tag: &'a dyn Fn(u16, &str) -> bool,
    /// The enchantment registry (`enchant_randomly`, `enchant_with_levels`); without it these
    /// modifiers do nothing.
    pub enchanting: Option<&'a crate::enchant::Enchanting>,
    /// Resolves a holder set entry of a registry (`"ns:id"` or `"#ns:tag"`) to registry ids in
    /// Vanilla's order.
    pub holder_set: &'a dyn Fn(&str, &str) -> Vec<u32>,
    /// Whether the biome at the origin is the given biome (`"ns:id"` or `"#ns:tag"`), for
    /// `location_check`; `None` without level access (the check fails).
    pub biome: Option<&'a dyn Fn(&str) -> bool>,
    /// Whether `this_entity` is set without an entity id: the breaker or the explosion source
    /// of block loot (`Block.dropResources(..., breaker, tool)`).
    pub this_present: bool,
    /// The loaded block at an offset from the origin's block (`location_check` with a `block`
    /// predicate); `None` without level access (the check holds).
    pub neighbour: Option<&'a dyn Fn(i32, i32, i32) -> Option<BlockState>>,
    /// The entity parameters of entity loot (`LootContextParamSets.ENTITY`).
    pub entity: Option<&'a EntityLootContext<'a>>,
    /// `exploration_map` with level access: the filled map for the request at the origin, if
    /// a structure was found.
    pub exploration_map: Option<&'a ExploreFn<'a>>,
    /// The level's weather as (raining, thundering) for `weather_check`; `None` without level
    /// access (the check fails).
    pub weather: Option<(bool, bool)>,
    /// The score of an entity in an objective for `entity_scores` (`None`: no score); without
    /// it the check fails.
    pub score: Option<&'a ScoreFn<'a>>,
}

/// The score of an entity in an objective (see [`LootContext`]).
pub type ScoreFn<'a> = dyn Fn(vanillite_world::entity::EntityId, &str) -> Option<i32> + 'a;

/// Makes the map of an `exploration_map` request at an origin (see [`LootContext`]).
pub type ExploreFn<'a> = dyn Fn(&ExplorationMap, Vec3) -> Option<ItemStack> + 'a;

/// The arguments of `exploration_map` (`ExplorationMapFunction`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExplorationMap {
    /// `destination`: a structure tag id without `#`.
    pub destination: String,
    /// `decoration`: a map decoration type id.
    pub decoration: String,
    /// `zoom`.
    pub zoom: i8,
    /// `search_radius`.
    pub search_radius: i32,
    /// `skip_existing_chunks`.
    pub skip_existing_chunks: bool,
}

impl fmt::Debug for LootContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LootContext")
            .field("origin", &self.origin)
            .field("this_entity", &self.this_entity)
            .field("block_state", &self.block_state)
            .field("tool", &self.tool)
            .field("explosion_radius", &self.explosion_radius)
            .field("luck", &self.luck)
            .finish_non_exhaustive()
    }
}

fn no_enchantment(_: &str) -> i32 {
    0
}

fn no_tag(_: u16, _: &str) -> bool {
    false
}

fn no_holders(_: &str, _: &str) -> Vec<u32> {
    Vec::new()
}

impl LootContext<'_> {
    /// A context without any parameter.
    pub fn empty() -> LootContext<'static> {
        LootContext {
            origin: None,
            this_entity: None,
            block_state: None,
            tool: None,
            explosion_radius: None,
            block_entity: None,
            luck: 0.0,
            enchantment_level: &no_enchantment,
            item_in_tag: &no_tag,
            enchanting: None,
            holder_set: &no_holders,
            biome: None,
            this_present: false,
            neighbour: None,
            entity: None,
            exploration_map: None,
            weather: None,
            score: None,
        }
    }
}

/// What a block loot table is evaluated against.
pub struct BlockLootContext<'a> {
    pub state: BlockState,
    /// The tool (main hand stack; may be empty).
    pub tool: &'a ItemStack,
    /// Enchantment level on the tool by enchantment id, e.g. `"minecraft:fortune"` → 3 (0 if
    /// none).
    pub enchantment_level: &'a dyn Fn(&str) -> i32,
    /// Whether an item is in an item tag (tag id without '#').
    pub item_in_tag: &'a dyn Fn(u16, &str) -> bool,
    /// Explosion radius if destroyed by an explosion (None for players).
    pub explosion_radius: Option<f32>,
    /// The components of the broken block's block entity (`collectComponents`), by component
    /// type id, as wire values; `None` without a block entity.
    pub block_entity: Option<&'a dyn Fn(u16) -> Option<Box<[u8]>>>,
    /// Whether `THIS_ENTITY` is set for block loot: the breaker or the explosion source
    /// (`Block.dropResources(..., breaker, tool)`); a block destroyed by a shape update has none.
    pub this_present: bool,
    /// The entity parameters of entity loot (`LootContextParamSets.ENTITY`); `None` for
    /// block loot.
    pub entity: Option<&'a EntityLootContext<'a>>,
}

impl<'a> BlockLootContext<'a> {
    /// The general loot context with these parameters.
    pub fn as_loot(&self) -> LootContext<'a> {
        LootContext {
            block_state: Some(self.state),
            tool: Some(self.tool),
            explosion_radius: self.explosion_radius,
            block_entity: self.block_entity,
            enchantment_level: self.enchantment_level,
            item_in_tag: self.item_in_tag,
            this_present: self.this_present,
            entity: self.entity,
            ..LootContext::empty()
        }
    }
}

/// The entity parameters of a loot context (`THIS_ENTITY`, `ATTACKING_ENTITY`,
/// `DIRECT_ATTACKING_ENTITY`, `LAST_DAMAGE_PLAYER`, `DAMAGE_SOURCE`).
pub struct EntityLootContext<'a> {
    pub this: crate::predicate::EntityFacts,
    pub attacker: Option<crate::predicate::EntityFacts>,
    pub direct_attacker: Option<crate::predicate::EntityFacts>,
    pub last_damage_player: Option<crate::predicate::EntityFacts>,
    pub source: Option<crate::predicate::SourceFacts>,
    pub types: &'a crate::survival::damage::DamageTypes,
    pub tags: &'a dyn crate::predicate::EntityTags,
    /// Enchantment level on the attacker's weapon by enchantment id (Looting).
    pub attacker_enchantment_level: &'a dyn Fn(&str) -> i32,
}

impl fmt::Debug for EntityLootContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntityLootContext")
            .field("this", &self.this)
            .field("attacker", &self.attacker)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for BlockLootContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockLootContext")
            .field("state", &self.state)
            .field("tool", &self.tool)
            .field("explosion_radius", &self.explosion_radius)
            .finish_non_exhaustive()
    }
}

/// The parsed block loot tables of a set of packs.
#[derive(Debug, Default)]
pub struct LootTables {
    tables: Vec<Table>,
    /// Table index by table id (e.g. `minecraft:blocks/stone`).
    by_name: HashMap<Box<str>, u32>,
    /// Table index by block id; `u32::MAX` for blocks without a table.
    by_block: Box<[u32]>,
    /// Named predicates (`data/<ns>/predicate/`), referenced by index.
    predicates: Vec<Cond>,
    /// Named item modifiers (`data/<ns>/item_modifier/`), referenced by index.
    modifiers: Vec<Func>,
    /// Predicate and item modifier indices by id.
    predicate_ids: HashMap<String, u32>,
    modifier_ids: HashMap<String, u32>,
    /// Files that failed to parse, with the reason.
    broken: Vec<String>,
    /// Unsupported features met while parsing, with the first file using each.
    unsupported: Vec<String>,
    /// The block tags of the same packs (sorted block ids by tag id), for block predicates
    /// with a tag; `None`: the installed game data's.
    block_tags: Option<std::sync::Arc<HashMap<String, Vec<u16>>>>,
}

impl LootTables {
    /// Sets the block tags of the packs these tables came from ([`BlockSet::Tag`]).
    pub fn set_block_tags(&mut self, tags: std::sync::Arc<HashMap<String, Vec<u16>>>) {
        self.block_tags = Some(tags);
    }

    /// Loads every `data/<ns>/loot_table/**.json` of the packs in `roots` (each root contains
    /// `data/`; later roots override earlier ones), like the reloadable `loot_table` registry.
    /// Broken tables are logged and skipped.
    pub fn load(roots: &[PathBuf]) -> LootTables {
        let mut table_files = BTreeMap::new();
        let mut predicate_files = BTreeMap::new();
        let mut modifier_files = BTreeMap::new();
        for root in roots {
            let data = root.join("data");
            let Ok(dir) = std::fs::read_dir(&data) else {
                log::warn!("loot tables: {} has no data directory", root.display());
                continue;
            };
            let mut namespaces: Vec<(String, PathBuf)> = dir
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
                .collect();
            namespaces.sort();
            for (ns, path) in namespaces {
                collect_json(
                    &path.join("loot_table"),
                    &format!("{ns}:"),
                    &mut table_files,
                );
                collect_json(
                    &path.join("predicate"),
                    &format!("{ns}:"),
                    &mut predicate_files,
                );
                collect_json(
                    &path.join("item_modifier"),
                    &format!("{ns}:"),
                    &mut modifier_files,
                );
            }
        }
        let tables = LootTables::from_files(
            read_files(&table_files),
            read_files(&predicate_files),
            read_files(&modifier_files),
        );
        log::debug!(
            "loot tables: {} tables, {} predicates, {} item modifiers",
            tables.tables.len(),
            tables.predicates.len(),
            tables.modifiers.len()
        );
        tables
    }

    /// An empty set (tests).
    pub fn empty() -> LootTables {
        LootTables {
            by_block: vec![u32::MAX; BLOCKS.len()].into_boxed_slice(),
            ..LootTables::default()
        }
    }

    /// Number of loaded tables.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Whether no table is loaded.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// The files that failed to parse, with the reason (already logged).
    pub fn broken(&self) -> &[String] {
        &self.broken
    }

    /// The unsupported features the tables use, with the first file using each (already
    /// logged).
    pub fn unsupported(&self) -> &[String] {
        &self.unsupported
    }

    /// The `random_sequence` of the loot table of `state`, if it has one.
    pub fn block_sequence(&self, state: BlockState) -> Option<&str> {
        let block = usize::from(state.block().0);
        self.by_block
            .get(block)
            .and_then(|&i| self.tables.get(usize::try_from(i).ok()?))
            .and_then(|t| t.sequence.as_deref())
    }

    /// Drops of breaking `ctx.state` (table `minecraft:blocks/<block path>`; no table → nothing).
    /// Appends to `out`, with emptied stacks as empty stacks.
    pub fn block_drops(
        &self,
        ctx: &BlockLootContext<'_>,
        rng: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
    ) {
        self.block_drops_at(ctx, None, rng, out);
    }

    /// [`Self::block_drops`] with level access: `neighbour` gives the loaded block at an
    /// offset from the broken block (`location_check`).
    pub fn block_drops_at(
        &self,
        ctx: &BlockLootContext<'_>,
        neighbour: Option<&dyn Fn(i32, i32, i32) -> Option<BlockState>>,
        rng: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
    ) {
        if !ctx.state.is_valid() {
            return;
        }
        let block = usize::from(ctx.state.block().0);
        let Some(table) = self
            .by_block
            .get(block)
            .and_then(|&i| self.tables.get(usize::try_from(i).ok()?))
        else {
            return;
        };
        let ctx = LootContext {
            neighbour,
            ..ctx.as_loot()
        };
        let mut eval = Eval {
            tables: self,
            ctx: &ctx,
            rng,
            out,
            post: Vec::new(),
            depth: 0,
            allow_additional_cost: false,
            // `Block.getDrops` keeps stacks a function emptied: `popResource` still draws
            // their position.
            keep_empty: true,
        };
        eval.table(table);
    }

    /// Whether the table `table_id` (e.g. `minecraft:chests/simple_dungeon`) is loaded.
    pub fn contains(&self, table_id: &str) -> bool {
        self.by_name.contains_key(table_id)
    }

    /// `LootTable.getRandomItems(context)`: the stacks of the table `table_id`, split by max
    /// stack size, appended to `out` (an unknown table gives nothing, as Vanilla's empty
    /// table).
    pub fn roll(
        &self,
        table_id: &str,
        ctx: &LootContext<'_>,
        rng: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
    ) {
        self.roll_with(table_id, ctx, rng, out, false);
    }

    /// [`Self::roll`] that keeps discarded results as empty stacks, as `/loot` gets them from
    /// `LootTable.getRandomItems` (an empty dropped item still takes an entity seed, an empty
    /// replaced slot is cleared).
    pub fn roll_keeping_empty(
        &self,
        table_id: &str,
        ctx: &LootContext<'_>,
        rng: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
    ) {
        self.roll_with(table_id, ctx, rng, out, true);
    }

    fn roll_with(
        &self,
        table_id: &str,
        ctx: &LootContext<'_>,
        rng: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
        keep_empty: bool,
    ) {
        let Some(table) = self
            .by_name
            .get(table_id)
            .and_then(|&i| self.tables.get(usize::try_from(i).ok()?))
        else {
            log::warn!("loot tables: unknown loot table {table_id}");
            return;
        };
        let mut eval = Eval {
            tables: self,
            ctx,
            rng,
            out,
            post: Vec::new(),
            depth: 0,
            allow_additional_cost: false,
            keep_empty,
        };
        eval.table(table);
    }

    /// `LootTable.getRandomItems` of the table `name` (e.g. `minecraft:spawners/trial_chamber/key`)
    /// with an empty context (no block, no tool, no entity). Appends to `out`; unknown tables
    /// drop nothing.
    pub fn random_items(&self, name: &str, rng: &mut dyn LootRandom, out: &mut Vec<ItemStack>) {
        if self.by_name.contains_key(name) {
            self.roll(name, &LootContext::empty(), rng, out);
        }
    }

    /// `LootTable.fill(container, params, seed)` with the context's random `rng`: rolls the
    /// table `table_id` and spreads the stacks over the empty slots of `items` in random order,
    /// splitting stacks into the free slots (`shuffleAndSplitItems`).
    pub fn fill(
        &self,
        table_id: &str,
        ctx: &LootContext<'_>,
        rng: &mut dyn LootRandom,
        items: &mut [ItemStack],
    ) {
        let mut stacks = Vec::new();
        self.roll(table_id, ctx, rng, &mut stacks);
        // `getAvailableSlots`.
        let mut slots: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_empty())
            .map(|(i, _)| i)
            .collect();
        shuffle(&mut slots, rng);
        shuffle_and_split(&mut stacks, slots.len(), rng);
        for stack in stacks {
            let Some(slot) = slots.pop() else {
                log::warn!("Tried to over-fill a container");
                return;
            };
            if let Some(target) = items.get_mut(slot) {
                *target = stack;
            }
        }
    }

    /// Drops of the loot table `table` (e.g. `minecraft:entities/cow`) for an entity loot
    /// context; a missing table drops nothing. Appends to `out`.
    pub fn table_drops(
        &self,
        table: &str,
        ctx: &BlockLootContext<'_>,
        rng: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
    ) {
        if self.by_name.contains_key(table) {
            let ctx = LootContext {
                block_state: ctx.state.is_valid().then_some(ctx.state),
                tool: None,
                ..ctx.as_loot()
            };
            self.roll(table, &ctx, rng, out);
        }
    }

    /// A parser that resolves the named predicates and item modifiers of this set.
    fn parser(&self) -> Parser<'_> {
        Parser {
            predicates: &self.predicate_ids,
            modifiers: &self.modifier_ids,
            unsupported: Vec::new(),
        }
    }

    /// `LootTable.getRandomItems` of an inline table (`ResourceOrIdArgument` with a value), for
    /// `/loot`: discarded results stay as empty stacks ([`Self::roll_keeping_empty`]).
    pub fn roll_inline(
        &self,
        json: &Value,
        ctx: &LootContext<'_>,
        rng: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
    ) -> Result<(), String> {
        let table = self.parser().table(json)?;
        let mut eval = Eval {
            tables: self,
            ctx,
            rng,
            out,
            post: Vec::new(),
            depth: 0,
            allow_additional_cost: false,
            keep_empty: true,
        };
        eval.table(&table);
        Ok(())
    }

    /// Whether the named predicate is loaded.
    pub fn has_predicate(&self, id: &str) -> bool {
        self.predicate_ids.contains_key(id)
    }

    /// Whether the named item modifier is loaded.
    pub fn has_modifier(&self, id: &str) -> bool {
        self.modifier_ids.contains_key(id)
    }

    /// `LootItemCondition.test` of a named predicate (`None`: unknown) or, with `inline`, of
    /// an inline predicate (a list is `all_of`).
    pub fn test_predicate(
        &self,
        id: Option<&str>,
        inline: Option<&Value>,
        ctx: &LootContext<'_>,
        rng: &mut dyn LootRandom,
    ) -> Result<bool, String> {
        let owned;
        let cond = match (id, inline) {
            (Some(id), _) => {
                let i = *self
                    .predicate_ids
                    .get(id)
                    .ok_or_else(|| format!("unknown predicate {id}"))?;
                self.predicates
                    .get(usize::try_from(i).unwrap_or(usize::MAX))
                    .ok_or_else(|| format!("unknown predicate {id}"))?
            }
            (None, Some(Value::Array(list))) => {
                let mut parser = self.parser();
                let all = list
                    .iter()
                    .map(|c| parser.cond(c))
                    .collect::<Res<Vec<_>>>()?;
                owned = Cond::AllOf(all.into());
                &owned
            }
            (None, Some(v)) => {
                owned = self.parser().cond(v)?;
                &owned
            }
            (None, None) => return Err("no predicate".to_owned()),
        };
        let mut out = Vec::new();
        let mut eval = Eval {
            tables: self,
            ctx,
            rng,
            out: &mut out,
            post: Vec::new(),
            depth: 0,
            allow_additional_cost: false,
            keep_empty: false,
        };
        Ok(eval.test(cond))
    }

    /// `LootItemFunction.apply` of a named item modifier or, with `inline`, of an inline one
    /// (a list is a `sequence`).
    pub fn modify(
        &self,
        id: Option<&str>,
        inline: Option<&Value>,
        stack: &mut ItemStack,
        ctx: &LootContext<'_>,
        rng: &mut dyn LootRandom,
    ) -> Result<(), String> {
        let owned;
        let func = match (id, inline) {
            (Some(id), _) => {
                let i = *self
                    .modifier_ids
                    .get(id)
                    .ok_or_else(|| format!("unknown item modifier {id}"))?;
                self.modifiers
                    .get(usize::try_from(i).unwrap_or(usize::MAX))
                    .ok_or_else(|| format!("unknown item modifier {id}"))?
            }
            (None, Some(Value::Array(list))) => {
                let mut parser = self.parser();
                let all = list
                    .iter()
                    .map(|f| parser.func(f))
                    .collect::<Res<Vec<_>>>()?;
                owned = Func {
                    condition: None,
                    kind: FuncKind::Sequence(all.into()),
                };
                &owned
            }
            (None, Some(v)) => {
                owned = self.parser().func(v)?;
                &owned
            }
            (None, None) => return Err("no item modifier".to_owned()),
        };
        let mut out = Vec::new();
        let mut eval = Eval {
            tables: self,
            ctx,
            rng,
            out: &mut out,
            post: Vec::new(),
            depth: 0,
            allow_additional_cost: false,
            keep_empty: false,
        };
        eval.apply(func, stack);
        Ok(())
    }

    /// [`Self::block_drops`] with the table's random sequence of `level`
    /// (`LootContext.Builder.create(randomSequence)`), else with `fallback` (the level random).
    pub fn roll_block(
        &self,
        level: &crate::level::Level,
        pos: vanillite_base::BlockPos,
        ctx: &BlockLootContext<'_>,
        fallback: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
    ) {
        let neighbour = |dx, dy, dz| level.chunks.block(pos.offset(dx, dy, dz)).map(BlockState);
        let neighbour: &dyn Fn(i32, i32, i32) -> Option<BlockState> = &neighbour;
        match self.block_random_sequence(ctx.state) {
            Some(key) => level.with_random_sequence(key, |r| {
                self.block_drops_at(ctx, Some(neighbour), r, out);
            }),
            None => self.block_drops_at(ctx, Some(neighbour), fallback, out),
        }
    }

    /// [`Self::table_drops`] with the table's random sequence of `level`, else with
    /// `fallback` (the level random).
    pub fn roll_table(
        &self,
        level: &crate::level::Level,
        table: &str,
        ctx: &BlockLootContext<'_>,
        fallback: &mut dyn LootRandom,
        out: &mut Vec<ItemStack>,
    ) {
        match self.random_sequence(table) {
            Some(key) => {
                level.with_random_sequence(key, |r| self.table_drops(table, ctx, r, out));
            }
            None => self.table_drops(table, ctx, fallback, out),
        }
    }

    /// The `random_sequence` of the block's table.
    pub fn block_random_sequence(&self, state: BlockState) -> Option<&str> {
        if !state.is_valid() {
            return None;
        }
        self.by_block
            .get(usize::from(state.block().0))
            .and_then(|&i| self.tables.get(usize::try_from(i).ok()?))?
            .sequence
            .as_deref()
    }

    /// The `random_sequence` of the table `table`.
    pub fn random_sequence(&self, table: &str) -> Option<&str> {
        self.by_name
            .get(table)
            .and_then(|&i| self.tables.get(usize::try_from(i).ok()?))?
            .sequence
            .as_deref()
    }

    /// Whether a table with this id is loaded.
    pub fn has_table(&self, table: &str) -> bool {
        self.by_name.contains_key(table)
    }

    /// Builds the set from parsed JSON files: `(id, file content or read error)`.
    fn from_files(
        tables: Vec<(String, Result<Value, String>)>,
        predicates: Vec<(String, Result<Value, String>)>,
        modifiers: Vec<(String, Result<Value, String>)>,
    ) -> LootTables {
        let index = |files: &[(String, Result<Value, String>)]| -> HashMap<String, u32> {
            files
                .iter()
                .enumerate()
                .filter_map(|(i, (id, _))| Some((id.clone(), u32::try_from(i).ok()?)))
                .collect()
        };
        let predicate_ids = index(&predicates);
        let modifier_ids = index(&modifiers);
        let mut parser = Parser {
            predicates: &predicate_ids,
            modifiers: &modifier_ids,
            unsupported: Vec::new(),
        };
        let mut broken = Vec::new();
        let mut unsupported: BTreeMap<String, (String, usize)> = BTreeMap::new();
        let mut note = |parser: &mut Parser<'_>, id: &str| {
            for feature in parser.unsupported.drain(..) {
                unsupported
                    .entry(feature)
                    .or_insert_with(|| (id.to_owned(), 0))
                    .1 += 1;
            }
        };

        let mut parsed_predicates = Vec::with_capacity(predicates.len());
        for (id, json) in &predicates {
            let cond = json.as_ref().map_err(Clone::clone).and_then(|v| {
                let o = object(v)?;
                parser.cond_typed(o)
            });
            note(&mut parser, id);
            parsed_predicates.push(cond.unwrap_or_else(|e| {
                log::warn!("predicate {id}: {e}; it evaluates to false");
                broken.push(format!("predicate {id}: {e}"));
                Cond::Const(false)
            }));
        }
        let mut parsed_modifiers = Vec::with_capacity(modifiers.len());
        for (id, json) in &modifiers {
            let func = json
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|v| parser.func(v));
            note(&mut parser, id);
            parsed_modifiers.push(func.unwrap_or_else(|e| {
                log::warn!("item modifier {id}: {e}; it does nothing");
                broken.push(format!("item modifier {id}: {e}"));
                Func {
                    condition: None,
                    kind: FuncKind::Noop,
                }
            }));
        }
        let mut parsed_tables = Vec::with_capacity(tables.len());
        let mut by_name = HashMap::with_capacity(tables.len());
        for (id, json) in &tables {
            let table = json
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|v| parser.table(v));
            note(&mut parser, id);
            match table {
                Ok(table) => {
                    if let Ok(i) = u32::try_from(parsed_tables.len()) {
                        by_name.insert(Box::from(id.as_str()), i);
                        parsed_tables.push(table);
                    }
                }
                Err(e) => {
                    log::warn!("loot table {id}: {e}; skipped");
                    broken.push(format!("loot table {id}: {e}"));
                }
            }
        }

        let mut missing = Vec::new();
        for table in &parsed_tables {
            table.missing_references(&by_name, &mut missing);
        }
        missing.sort();
        missing.dedup();
        for name in &missing {
            log::warn!("loot tables: referenced table {name} is not loaded; it drops nothing");
        }

        let unsupported: Vec<String> = unsupported
            .into_iter()
            .map(|(feature, (first, count))| {
                let line = format!("{feature} (in {first} and {count} places in total)");
                log::warn!("loot tables: unsupported {line}");
                line
            })
            .collect();

        let by_block = BLOCKS
            .iter()
            .map(|info| {
                let (ns, path) = info
                    .name
                    .split_once(':')
                    .unwrap_or(("minecraft", info.name));
                // `Blocks.wallVariant`: wall torches, signs, heads, banners and coral fans
                // use the table of their standing block (`overrideLootTable`).
                by_name
                    .get(format!("{ns}:blocks/{path}").as_str())
                    .or_else(|| {
                        path.contains("wall_").then(|| {
                            by_name
                                .get(format!("{ns}:blocks/{}", path.replace("wall_", "")).as_str())
                        })?
                    })
                    .copied()
                    .unwrap_or(u32::MAX)
            })
            .collect();

        LootTables {
            tables: parsed_tables,
            by_name,
            by_block,
            predicates: parsed_predicates,
            modifiers: parsed_modifiers,
            predicate_ids,
            modifier_ids,
            broken,
            unsupported,
            block_tags: None,
        }
    }
}

/// The loot table directories [`LootTables::load`] reads.
pub const LOADED_KINDS: [&str; 5] = ["blocks", "chests", "archaeology", "pots", "spawners"];

/// `Util.shuffle(list, random)`.
fn shuffle<T>(list: &mut [T], rng: &mut dyn LootRandom) {
    for i in (2..=list.len()).rev() {
        let bound = i32::try_from(i).unwrap_or(i32::MAX);
        let to = usize::try_from(rng.next_int(bound)).unwrap_or(0);
        list.swap(i - 1, to);
    }
}

/// `LootTable.shuffleAndSplitItems`: splits stacks of more than one item while there are more
/// free slots than stacks, then shuffles.
fn shuffle_and_split(result: &mut Vec<ItemStack>, available: usize, rng: &mut dyn LootRandom) {
    let mut splittable = Vec::new();
    let mut kept = Vec::with_capacity(result.len());
    for stack in result.drain(..) {
        if stack.is_empty() {
            continue;
        }
        if stack.count > 1 {
            splittable.push(stack);
        } else {
            kept.push(stack);
        }
    }
    *result = kept;
    while available.saturating_sub(result.len() + splittable.len()) > 0 && !splittable.is_empty() {
        let last = i32::try_from(splittable.len() - 1).unwrap_or(0);
        let index = usize::try_from(next_int_between(rng, 0, last)).unwrap_or(0);
        if index >= splittable.len() {
            break;
        }
        let mut stack = splittable.remove(index);
        let remove = next_int_between(rng, 1, stack.count / 2);
        let copy = stack.split(remove);
        if stack.count > 1 && rng.next_boolean() {
            splittable.push(stack);
        } else {
            result.push(stack);
        }
        if copy.count > 1 && rng.next_boolean() {
            splittable.push(copy);
        } else {
            result.push(copy);
        }
    }
    result.append(&mut splittable);
    shuffle(result, rng);
}

/// Adds every `*.json` below `dir` as `<prefix><relative path without .json>`.
pub(crate) fn collect_json(dir: &Path, prefix: &str, out: &mut BTreeMap<String, PathBuf>) {
    fn walk(dir: &Path, rel: &str, prefix: &str, out: &mut BTreeMap<String, PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if path.is_dir() {
                walk(&path, &format!("{rel}{name}/"), prefix, out);
            } else if let Some(stem) = name.strip_suffix(".json") {
                out.insert(format!("{prefix}{rel}{stem}"), path);
            }
        }
    }
    walk(dir, "", prefix, out);
}

/// Reads and parses the files of [`collect_json`].
pub(crate) fn read_files(
    files: &BTreeMap<String, PathBuf>,
) -> Vec<(String, Result<Value, String>)> {
    files
        .iter()
        .map(|(id, path)| {
            let json = std::fs::read(path)
                .map_err(|e| format!("{}: {e}", path.display()))
                .and_then(|bytes| {
                    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
                });
            (id.clone(), json)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The typed representation.

#[derive(Debug)]
struct Table {
    pools: Vec<Pool>,
    modifier: Option<Func>,
    /// `random_sequence`: the named random the rolls draw from.
    sequence: Option<Box<str>>,
}

#[derive(Debug)]
struct Pool {
    entries: Vec<Entry>,
    condition: Option<Cond>,
    modifier: Option<Func>,
    rolls: IntProvider,
    bonus_rolls: FloatProvider,
}

#[derive(Debug)]
struct Entry {
    kind: EntryKind,
    condition: Option<Cond>,
    modifier: Option<Func>,
    weight: i32,
    quality: i32,
}

#[derive(Debug)]
enum EntryKind {
    Item(u16),
    /// A `tag` entry: all items of the set as one entry, or (`expand`) one entry per item.
    Items {
        items: ItemSet,
        expand: bool,
    },
    Empty,
    /// Block entity contents (`dynamic`, `LootContext.addDynamicDrops`): `minecraft:sherds`
    /// are the sides of a decorated pot; other names drop nothing.
    Dynamic(Box<str>),
    /// A `loot_table` entry.
    Tables {
        tables: Vec<TableRef>,
        expand: bool,
    },
    Alternatives(Vec<Entry>),
    Sequence(Vec<Entry>),
    Group(Vec<Entry>),
}

#[derive(Debug)]
enum TableRef {
    Named(Box<str>),
    Inline(Box<Table>),
}

#[derive(Debug)]
enum ItemSet {
    Items(Box<[u16]>),
    Tag(Box<str>),
}

#[derive(Debug)]
enum Cond {
    /// A named predicate (index into [`LootTables::predicates`]).
    Ref(u32),
    Const(bool),
    Inverted(Box<Cond>),
    AnyOf(Box<[Cond]>),
    AllOf(Box<[Cond]>),
    SurvivesExplosion,
    RandomChance(FloatProvider),
    TableBonus {
        enchantment: Box<str>,
        chances: Box<[f32]>,
    },
    MatchTool(Option<Box<ItemPredicate>>),
    MatchBlock(Box<BlockPredicate>),
    /// `location_check` with `biomes`: holder set entries (`id` or `#tag`).
    Biome(Box<[Box<str>]>),
    /// `location_check` with a `block` predicate, at a block offset from the origin.
    LocationBlock([i32; 3], Box<BlockPredicate>),
    /// `entity_properties` with a non-empty predicate (entity loot).
    Entity(EntityTarget, Box<crate::predicate::EntityPredicate>),
    /// `entity_properties` on `this` with an empty predicate.
    ThisPresent,
    KilledByPlayer,
    DamageSource(Box<crate::predicate::DamageSourcePredicate>),
    /// `weather_check`.
    Weather {
        raining: Option<bool>,
        thundering: Option<bool>,
    },
    /// `int_value_check`: the value drawn, then tested.
    IntValue(IntProvider, Bounds),
    /// `entity_scores` of `this` (the other targets have no entity here).
    Scores(EntityTarget, Box<[(Box<str>, Bounds)]>),
    /// `random_chance_with_enchanted_bonus`.
    EnchantedChance {
        enchantment: Box<str>,
        unenchanted: f32,
        enchanted: crate::enchant::LevelBased,
    },
}

/// `LootContext.EntityTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntityTarget {
    This,
    Attacker,
    DirectAttacker,
    AttackingPlayer,
}

#[derive(Debug)]
struct ItemPredicate {
    items: Option<ItemSet>,
    count: Bounds,
    enchantments: Box<[EnchantmentPredicate]>,
    /// The `minecraft:stored_enchantments` predicates (enchanted books).
    stored_enchantments: Box<[EnchantmentPredicate]>,
    /// Component types that must be present (`AnyValue` predicates).
    present: Box<[u16]>,
}

#[derive(Debug)]
struct EnchantmentPredicate {
    /// Any of these enchantments (any enchantment at all if `None`), with a level in
    /// `levels`.
    enchantments: Option<Box<[Box<str>]>>,
    levels: Bounds,
}

/// `MinMaxBounds.Ints`.
#[derive(Debug, Clone, Copy, Default)]
struct Bounds {
    min: Option<i32>,
    max: Option<i32>,
}

impl Bounds {
    fn matches(self, value: i32) -> bool {
        self.min.is_none_or(|m| m <= value) && self.max.is_none_or(|m| value <= m)
    }

    fn is_any(self) -> bool {
        self.min.is_none() && self.max.is_none()
    }
}

#[derive(Debug)]
struct BlockPredicate {
    blocks: Option<BlockSet>,
    state: Box<[(Box<str>, ValueMatcher)]>,
}

/// The `blocks` holder set of a block predicate.
#[derive(Debug)]
enum BlockSet {
    Ids(Box<[BlockId]>),
    /// A block tag (without `#`), resolved against the loaded data packs.
    Tag(Box<str>),
}

impl BlockSet {
    fn contains(&self, block: BlockId, tags: Option<&HashMap<String, Vec<u16>>>) -> bool {
        match self {
            BlockSet::Ids(ids) => ids.contains(&block),
            BlockSet::Tag(tag) => match tags {
                Some(tags) => tags
                    .get(&**tag)
                    .is_some_and(|v| v.binary_search(&block.0).is_ok()),
                None => {
                    crate::player::play::game_data().is_some_and(|d| d.block_in_tag(block.0, tag))
                }
            },
        }
    }
}

#[derive(Debug)]
enum ValueMatcher {
    Exact(Box<str>),
    Range(Option<Box<str>>, Option<Box<str>>),
}

#[derive(Debug)]
struct Func {
    condition: Option<Cond>,
    kind: FuncKind,
}

#[derive(Debug)]
enum FuncKind {
    /// A named item modifier (index into [`LootTables::modifiers`]).
    Ref(u32),
    Sequence(Box<[Func]>),
    SetCount {
        count: IntProvider,
        add: bool,
    },
    ApplyBonus {
        enchantment: Box<str>,
        formula: Formula,
    },
    ExplosionDecay,
    LimitCount {
        min: Option<IntProvider>,
        max: Option<IntProvider>,
    },
    CopyState {
        properties: Box<[Box<str>]>,
    },
    Discard,
    /// `enchanted_count_increase`: Looting adds `count × level` items.
    EnchantedCountIncrease {
        enchantment: Box<str>,
        count: FloatProvider,
        limit: i32,
    },
    /// `furnace_smelt`: the smelted item (the cooked meats).
    FurnaceSmelt,
    /// `copy_components` / `copy_name` from the block entity: component type ids.
    CopyComponents(Box<[u16]>),
    /// `enchant_randomly`: `options` holder set entries (all enchantments if `None`).
    EnchantRandomly {
        options: Option<Box<[Box<str>]>>,
        only_compatible: bool,
        /// `include_additional_cost_component` (villager trades).
        additional_cost: bool,
    },
    /// `enchant_with_levels`.
    EnchantWithLevels {
        levels: IntProvider,
        options: Option<Box<[Box<str>]>>,
        /// `include_additional_cost_component` (villager trades).
        additional_cost: bool,
    },
    /// `set_random_dyes`: the number of random dyes mixed into the colour.
    SetRandomDyes(IntProvider),
    /// `set_random_potion`: `options` holder set entries (all potions if `None`).
    SetRandomPotion(Option<Box<[Box<str>]>>),
    /// `set_enchantments`: enchantment ids with their level providers.
    SetEnchantments {
        enchantments: Box<[(Box<str>, IntProvider)]>,
        add: bool,
    },
    /// `set_damage`.
    SetDamage {
        damage: FloatProvider,
        add: bool,
    },
    /// `set_name`: the component type (`custom_name` or `item_name`) and the encoded text.
    SetName {
        target: u16,
        name: Option<Box<[u8]>>,
    },
    /// `set_potion`: the `minecraft:potion` id.
    SetPotion(u32),
    /// `set_stew_effect`: `(mob effect id, duration, instantaneous)`.
    SetStewEffect(Box<[(u32, IntProvider, bool)]>),
    /// `set_instrument`: holder set entries of `minecraft:instrument`.
    SetInstrument(Box<[Box<str>]>),
    /// `exploration_map`.
    ExplorationMap(Box<ExplorationMap>),
    /// `set_ominous_bottle_amplifier`.
    SetOminousBottleAmplifier(IntProvider),
    /// `set_components`: component type ids with their persistent value (`None` removes
    /// the component). Encoded when applied, as the dynamic registries are known then.
    SetComponents(Box<[(u16, Option<vanillite_nbt::Tag>)]>),
    /// `set_custom_data`: merged into the stack's custom data.
    SetCustomData(Box<vanillite_nbt::Compound>),
    /// `set_lore`: the lines (component NBT) put into the lore list by `mode`.
    SetLore {
        lore: Box<[vanillite_nbt::Tag]>,
        mode: ListOperation,
    },
    /// `filtered`.
    Filtered {
        filter: Option<Box<ItemPredicate>>,
        on_pass: Option<Box<Func>>,
        on_fail: Option<Box<Func>>,
    },
    Noop,
}

#[derive(Debug)]
enum Formula {
    OreDrops,
    UniformBonusCount { multiplier: i32 },
    BinomialWithBonusCount { extra: i32, probability: f32 },
}

#[derive(Debug)]
enum IntProvider {
    Const(i32),
    Uniform(Box<(IntProvider, IntProvider)>),
    Binomial(Box<(IntProvider, FloatProvider)>),
    /// `add`: the sum of the inputs, in order.
    Sum(Box<[IntProvider]>),
}

#[derive(Debug)]
enum FloatProvider {
    Const(f32),
    Uniform(Box<(FloatProvider, FloatProvider)>),
}

impl Table {
    /// Adds the names of referenced tables that are not loaded.
    fn missing_references(&self, loaded: &HashMap<Box<str>, u32>, out: &mut Vec<String>) {
        fn entry(e: &Entry, loaded: &HashMap<Box<str>, u32>, out: &mut Vec<String>) {
            match &e.kind {
                EntryKind::Tables { tables, .. } => {
                    for t in tables {
                        match t {
                            TableRef::Named(name) if !loaded.contains_key(name) => {
                                out.push(name.to_string());
                            }
                            TableRef::Named(_) => {}
                            TableRef::Inline(table) => table.missing_references(loaded, out),
                        }
                    }
                }
                EntryKind::Alternatives(children)
                | EntryKind::Sequence(children)
                | EntryKind::Group(children) => {
                    for c in children {
                        entry(c, loaded, out);
                    }
                }
                _ => {}
            }
        }
        for pool in &self.pools {
            for e in &pool.entries {
                entry(e, loaded, out);
            }
        }
    }
}

/// The smelting result of the items mob loot smelts (`furnace_smelt` without recipes).
fn smelted(item: u16) -> Option<u16> {
    const PAIRS: [(&str, &str); 8] = [
        ("minecraft:beef", "minecraft:cooked_beef"),
        ("minecraft:porkchop", "minecraft:cooked_porkchop"),
        ("minecraft:chicken", "minecraft:cooked_chicken"),
        ("minecraft:mutton", "minecraft:cooked_mutton"),
        ("minecraft:rabbit", "minecraft:cooked_rabbit"),
        ("minecraft:cod", "minecraft:cooked_cod"),
        ("minecraft:salmon", "minecraft:cooked_salmon"),
        ("minecraft:potato", "minecraft:baked_potato"),
    ];
    let name = ITEMS.get(usize::from(item))?.name;
    let cooked = PAIRS.iter().find(|(raw, _)| *raw == name)?.1;
    vanillite_data::registry::ITEM.id(cooked)
}

// ---------------------------------------------------------------------------------------------
// Parsing.

type Res<T> = Result<T, String>;

/// `ns:path` for an identifier that may omit the default namespace.
fn qualify(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

fn object(v: &Value) -> Res<&Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| format!("expected an object, got {v}"))
}

fn string(v: &Value) -> Res<&str> {
    v.as_str()
        .ok_or_else(|| format!("expected a string, got {v}"))
}

/// `Codec.INT`: any JSON number, truncated.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn int(v: &Value) -> Res<i32> {
    if let Some(i) = v.as_i64() {
        return i32::try_from(i).map_err(|_| format!("{i} is out of the int range"));
    }
    v.as_f64()
        .map(|f| f as i32)
        .ok_or_else(|| format!("expected a number, got {v}"))
}

/// `Codec.FLOAT`.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn float(v: &Value) -> Res<f32> {
    v.as_f64()
        .map(|f| f as f32)
        .ok_or_else(|| format!("expected a number, got {v}"))
}

fn field<'v>(o: &'v Map<String, Value>, key: &str) -> Res<&'v Value> {
    o.get(key).ok_or_else(|| format!("missing field `{key}`"))
}

/// The `type` of a dispatched object, qualified.
fn type_of(o: &Map<String, Value>) -> Res<String> {
    Ok(qualify(string(field(o, "type")?)?))
}

/// A compact list (`ExtraCodecs.compactListCodec`): one element or a list.
fn compact_list(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

/// A holder set of a static registry: `#tag`, one id or a list of ids.
enum IdSet {
    Ids(Vec<String>),
    Tag(String),
}

fn id_set(v: &Value) -> Res<IdSet> {
    if let Some(s) = v.as_str()
        && let Some(tag) = s.strip_prefix('#')
    {
        return Ok(IdSet::Tag(qualify(tag)));
    }
    compact_list(v)
        .into_iter()
        .map(|e| string(e).map(qualify))
        .collect::<Res<Vec<_>>>()
        .map(IdSet::Ids)
}

fn item_id(name: &str) -> Res<u16> {
    ItemId::by_name(name)
        .map(|i| i.0)
        .ok_or_else(|| format!("unknown item {name}"))
}

fn block_id(name: &str) -> Res<BlockId> {
    BlockId::by_name(name).ok_or_else(|| format!("unknown block {name}"))
}

fn item_set(v: &Value) -> Res<ItemSet> {
    Ok(match id_set(v)? {
        IdSet::Tag(tag) => ItemSet::Tag(tag.into()),
        IdSet::Ids(ids) => ItemSet::Items(
            ids.iter()
                .map(|n| item_id(n))
                .collect::<Res<Vec<_>>>()?
                .into(),
        ),
    })
}

/// `MinMaxBounds.Ints`: a number or `{min, max}`.
/// `CompoundTag.merge`: compounds merge, other values replace.
fn merge_compound(into: &mut vanillite_nbt::Compound, from: &vanillite_nbt::Compound) {
    for (k, v) in from.iter() {
        match (into.get_mut(k), v) {
            (Some(vanillite_nbt::Tag::Compound(a)), vanillite_nbt::Tag::Compound(b)) => {
                merge_compound(a, b);
            }
            _ => into.insert(k, v.clone()),
        }
    }
}

/// `ListOperation` of `set_lore` (`mode` with `offset` and `size`).
#[derive(Debug, Clone, Copy)]
enum ListOperation {
    ReplaceAll,
    ReplaceSection { offset: usize, size: Option<usize> },
    Insert { offset: usize },
    Append,
}

impl ListOperation {
    fn parse(o: &Map<String, Value>) -> Res<ListOperation> {
        let count = |k: &str| -> Res<Option<usize>> {
            o.get(k)
                .map(|v| usize::try_from(int(v)?).map_err(|_| format!("negative `{k}`")))
                .transpose()
        };
        Ok(match string(field(o, "mode")?)? {
            "replace_all" => ListOperation::ReplaceAll,
            "replace_section" => ListOperation::ReplaceSection {
                offset: count("offset")?.unwrap_or(0),
                size: count("size")?,
            },
            "insert" => ListOperation::Insert {
                offset: count("offset")?.unwrap_or(0),
            },
            "append" => ListOperation::Append,
            other => return Err(format!("unknown list operation {other}")),
        })
    }

    /// `apply(original, replacement, maxSize)`: `None` keeps the original (an offset past
    /// its end or a result over `max`).
    fn apply<T: Clone>(self, original: &[T], replacement: &[T], max: usize) -> Option<Vec<T>> {
        let result: Vec<T> = match self {
            ListOperation::ReplaceAll => replacement.to_vec(),
            ListOperation::ReplaceSection { offset, size } => {
                if offset > original.len() {
                    return None;
                }
                let size = size.unwrap_or(replacement.len());
                let end = offset.saturating_add(size).min(original.len());
                let mut out = original[..offset].to_vec();
                out.extend_from_slice(replacement);
                out.extend_from_slice(&original[end..]);
                out
            }
            ListOperation::Insert { offset } => {
                if offset > original.len() {
                    return None;
                }
                let mut out = original[..offset].to_vec();
                out.extend_from_slice(replacement);
                out.extend_from_slice(&original[offset..]);
                out
            }
            ListOperation::Append => {
                let mut out = original.to_vec();
                out.extend_from_slice(replacement);
                out
            }
        };
        (result.len() <= max).then_some(result)
    }
}

fn bounds(v: &Value) -> Res<Bounds> {
    if v.is_number() {
        let n = int(v)?;
        return Ok(Bounds {
            min: Some(n),
            max: Some(n),
        });
    }
    let o = object(v)?;
    Ok(Bounds {
        min: o.get("min").map(int).transpose()?,
        max: o.get("max").map(int).transpose()?,
    })
}

/// A block state property value: a string (numbers and booleans are taken as their text).
fn state_value(v: &Value) -> Res<Box<str>> {
    match v {
        Value::String(s) => Ok(s.as_str().into()),
        Value::Number(n) => Ok(n.to_string().into()),
        Value::Bool(b) => Ok(b.to_string().into()),
        other => Err(format!("expected a property value, got {other}")),
    }
}

struct Parser<'n> {
    predicates: &'n HashMap<String, u32>,
    modifiers: &'n HashMap<String, u32>,
    /// Unsupported features met since the last drain.
    unsupported: Vec<String>,
}

impl Parser<'_> {
    fn unsupported(&mut self, feature: String) {
        self.unsupported.push(feature);
    }

    fn table(&mut self, v: &Value) -> Res<Table> {
        let o = object(v)?;
        let pools = match o.get("pools") {
            Some(p) => p
                .as_array()
                .ok_or("`pools` is not a list")?
                .iter()
                .enumerate()
                .map(|(i, p)| self.pool(p).map_err(|e| format!("pools[{i}]: {e}")))
                .collect::<Res<Vec<_>>>()?,
            None => Vec::new(),
        };
        let modifier = o
            .get("modifier")
            .map(|m| self.func(m))
            .transpose()
            .map_err(|e| format!("modifier: {e}"))?;
        let sequence = o
            .get("random_sequence")
            .map(|s| string(s).map(Box::from))
            .transpose()?;
        Ok(Table {
            pools,
            modifier,
            sequence,
        })
    }

    fn pool(&mut self, v: &Value) -> Res<Pool> {
        let o = object(v)?;
        let entries = field(o, "entries")?
            .as_array()
            .ok_or("`entries` is not a list")?
            .iter()
            .enumerate()
            .map(|(i, e)| self.entry(e).map_err(|e| format!("entries[{i}]: {e}")))
            .collect::<Res<Vec<_>>>()?;
        Ok(Pool {
            entries,
            condition: self.opt_cond(o)?,
            modifier: self.opt_func(o)?,
            rolls: self.int_provider(field(o, "rolls")?)?,
            bonus_rolls: o
                .get("bonus_rolls")
                .map(|b| self.float_provider(b))
                .transpose()?
                .unwrap_or(FloatProvider::Const(0.0)),
        })
    }

    fn opt_cond(&mut self, o: &Map<String, Value>) -> Res<Option<Cond>> {
        o.get("condition")
            .map(|c| self.cond(c))
            .transpose()
            .map_err(|e| format!("condition: {e}"))
    }

    fn opt_func(&mut self, o: &Map<String, Value>) -> Res<Option<Func>> {
        o.get("modifier")
            .map(|m| self.func(m))
            .transpose()
            .map_err(|e| format!("modifier: {e}"))
    }

    fn entry(&mut self, v: &Value) -> Res<Entry> {
        let o = object(v)?;
        let ty = type_of(o)?;
        let children = |p: &mut Self| -> Res<Vec<Entry>> {
            match o.get("children") {
                Some(c) => c
                    .as_array()
                    .ok_or("`children` is not a list")?
                    .iter()
                    .enumerate()
                    .map(|(i, e)| p.entry(e).map_err(|e| format!("children[{i}]: {e}")))
                    .collect(),
                None => Ok(Vec::new()),
            }
        };
        let expand = o.get("expand").and_then(Value::as_bool).unwrap_or(false);
        let kind = match ty.as_str() {
            "minecraft:item" => EntryKind::Item(item_id(&qualify(string(field(o, "name")?)?))?),
            "minecraft:tag" => EntryKind::Items {
                items: item_set(field(o, "items")?)?,
                expand,
            },
            "minecraft:empty" => EntryKind::Empty,
            "minecraft:dynamic" => EntryKind::Dynamic(qualify(string(field(o, "name")?)?).into()),
            "minecraft:loot_table" => EntryKind::Tables {
                tables: self.table_refs(field(o, "value")?)?,
                expand,
            },
            "minecraft:alternatives" => EntryKind::Alternatives(children(self)?),
            "minecraft:sequence" => EntryKind::Sequence(children(self)?),
            "minecraft:group" => EntryKind::Group(children(self)?),
            other => {
                self.unsupported(format!("entry type {other} (drops nothing)"));
                EntryKind::Empty
            }
        };
        Ok(Entry {
            kind,
            condition: self.opt_cond(o)?,
            modifier: self.opt_func(o)?,
            weight: o.get("weight").map(int).transpose()?.unwrap_or(1),
            quality: o.get("quality").map(int).transpose()?.unwrap_or(0),
        })
    }

    /// `LootTable.LIST_CODEC`: `#tag`, or one or more table ids or inline tables.
    fn table_refs(&mut self, v: &Value) -> Res<Vec<TableRef>> {
        if let Some(tag) = v.as_str().and_then(|s| s.strip_prefix('#')) {
            self.unsupported(format!("loot table tag #{tag} (drops nothing)"));
            return Ok(Vec::new());
        }
        compact_list(v)
            .into_iter()
            .map(|e| match e {
                Value::String(name) => Ok(TableRef::Named(qualify(name).into())),
                other => Ok(TableRef::Inline(Box::new(self.table(other)?))),
            })
            .collect()
    }

    /// A condition holder: a named predicate or an inline one.
    fn cond(&mut self, v: &Value) -> Res<Cond> {
        match v {
            Value::String(name) => {
                let name = qualify(name);
                self.predicates
                    .get(&name)
                    .map(|&i| Cond::Ref(i))
                    .ok_or_else(|| format!("unknown predicate {name}"))
            }
            other => self.cond_typed(object(other)?),
        }
    }

    /// A holder set of conditions (`terms`).
    fn cond_set(&mut self, v: &Value) -> Res<Box<[Cond]>> {
        if let Some(tag) = v.as_str().and_then(|s| s.strip_prefix('#')) {
            self.unsupported(format!("predicate tag #{tag} (false)"));
            return Ok(Box::new([Cond::Const(false)]));
        }
        compact_list(v).into_iter().map(|c| self.cond(c)).collect()
    }

    fn cond_typed(&mut self, o: &Map<String, Value>) -> Res<Cond> {
        let ty = type_of(o)?;
        Ok(match ty.as_str() {
            "minecraft:inverted" => Cond::Inverted(Box::new(self.cond(field(o, "term")?)?)),
            "minecraft:any_of" => Cond::AnyOf(self.cond_set(field(o, "terms")?)?),
            "minecraft:all_of" => Cond::AllOf(self.cond_set(field(o, "terms")?)?),
            "minecraft:survives_explosion" => Cond::SurvivesExplosion,
            "minecraft:random_chance" => {
                Cond::RandomChance(self.float_provider(field(o, "chance")?)?)
            }
            "minecraft:random_chance_with_enchanted_bonus" => Cond::EnchantedChance {
                enchantment: qualify(string(field(o, "enchantment")?)?).into(),
                unenchanted: float(field(o, "unenchanted_chance")?)?,
                enchanted: crate::enchant::LevelBased::parse(field(o, "enchanted_chance")?)
                    .ok_or("unsupported `enchanted_chance`")?,
            },
            "minecraft:table_bonus" => {
                let chances = field(o, "chances")?
                    .as_array()
                    .ok_or("`chances` is not a list")?
                    .iter()
                    .map(float)
                    .collect::<Res<Box<[f32]>>>()?;
                if chances.is_empty() {
                    return Err("`chances` is empty".into());
                }
                Cond::TableBonus {
                    enchantment: qualify(string(field(o, "enchantment")?)?).into(),
                    chances,
                }
            }
            "minecraft:match_tool" => match o.get("predicate") {
                None => Cond::MatchTool(None),
                Some(p) => match self.item_predicate(p)? {
                    Some(p) => Cond::MatchTool(Some(Box::new(p))),
                    None => Cond::Const(false),
                },
            },
            "minecraft:match_block" => match self.block_predicate(o)? {
                Some(p) => Cond::MatchBlock(Box::new(p)),
                None => Cond::Const(false),
            },
            "minecraft:entity_properties" => {
                let target = match string(field(o, "entity")?)? {
                    "this" => EntityTarget::This,
                    "attacker" => EntityTarget::Attacker,
                    "direct_attacker" => EntityTarget::DirectAttacker,
                    "attacking_player" => EntityTarget::AttackingPlayer,
                    other => return Err(format!("unknown entity target {other}")),
                };
                match o.get("predicate") {
                    None => Cond::ThisPresent,
                    Some(p) if target == EntityTarget::This && object(p)?.is_empty() => {
                        Cond::ThisPresent
                    }
                    Some(p) => {
                        let predicate = crate::predicate::EntityPredicate::parse(p);
                        if predicate.unsupported {
                            let keys: Vec<&str> = object(p)?.keys().map(String::as_str).collect();
                            self.unsupported(format!(
                                "entity_properties predicate with {} (false)",
                                keys.join(", ")
                            ));
                        }
                        Cond::Entity(target, Box::new(predicate))
                    }
                }
            }
            "minecraft:location_check" => match o.get("predicate") {
                None => Cond::Const(true),
                Some(p) => {
                    let p = object(p)?;
                    let other: Vec<&str> = p
                        .keys()
                        .map(String::as_str)
                        .filter(|k| !matches!(*k, "block" | "fluid" | "biomes"))
                        .collect();
                    if other.is_empty() {
                        let offset = |k: &str| {
                            o.get(k)
                                .and_then(Value::as_i64)
                                .and_then(|v| i32::try_from(v).ok())
                                .unwrap_or(0)
                        };
                        let offset = [offset("offsetX"), offset("offsetY"), offset("offsetZ")];
                        let mut terms = Vec::new();
                        if let Some(b) = p.get("biomes") {
                            terms.push(Cond::Biome(holder_entries(b)?));
                        }
                        // A block predicate this parser does not know holds (the
                        // neighbourhood is assumed to be as Vanilla left it).
                        if let Some(b) = p.get("block")
                            && let Some(block) = self.block_predicate(object(b)?)?
                        {
                            terms.push(Cond::LocationBlock(offset, Box::new(block)));
                        }
                        match terms.len() {
                            0 => Cond::Const(true),
                            1 => terms.pop().unwrap_or(Cond::Const(true)),
                            _ => Cond::AllOf(terms.into()),
                        }
                    } else {
                        self.unsupported(format!(
                            "location_check predicate with {} (false)",
                            other.join(", ")
                        ));
                        Cond::Const(false)
                    }
                }
            },
            "minecraft:killed_by_player" => Cond::KilledByPlayer,
            "minecraft:weather_check" => Cond::Weather {
                raining: o.get("raining").and_then(Value::as_bool),
                thundering: o.get("thundering").and_then(Value::as_bool),
            },
            "minecraft:int_value_check" => Cond::IntValue(
                self.int_provider(field(o, "value")?)?,
                bounds(field(o, "test")?)?,
            ),
            "minecraft:entity_scores" => {
                let target = match string(field(o, "entity")?)? {
                    "this" => EntityTarget::This,
                    "attacker" => EntityTarget::Attacker,
                    "direct_attacker" => EntityTarget::DirectAttacker,
                    "attacking_player" => EntityTarget::AttackingPlayer,
                    other => return Err(format!("unknown entity target {other}")),
                };
                let scores = object(field(o, "scores")?)?
                    .iter()
                    .map(|(k, v)| Ok((Box::from(k.as_str()), bounds(v)?)))
                    .collect::<Res<Box<[_]>>>()?;
                Cond::Scores(target, scores)
            }
            "minecraft:damage_source_properties" => {
                let p = crate::predicate::DamageSourcePredicate::parse(
                    o.get("predicate").unwrap_or(&Value::Null),
                );
                if p.unsupported {
                    self.unsupported("damage_source_properties predicate part (false)".into());
                }
                Cond::DamageSource(Box::new(p))
            }
            other => {
                self.unsupported(format!("condition {other} (false)"));
                Cond::Const(false)
            }
        })
    }

    /// `ItemPredicate`; `None` if it uses unsupported parts.
    fn item_predicate(&mut self, v: &Value) -> Res<Option<ItemPredicate>> {
        let o = object(v)?;
        let items = o.get("items").map(item_set).transpose()?;
        let count = o.get("count").map(bounds).transpose()?.unwrap_or_default();
        if let Some(c) = o.get("components")
            && !object(c)?.is_empty()
        {
            self.unsupported("match_tool with exact `components` (false)".into());
            return Ok(None);
        }
        let mut enchantments = Vec::new();
        let mut stored_enchantments = Vec::new();
        let mut present = Vec::new();
        if let Some(p) = o.get("predicates") {
            for (key, value) in object(p)? {
                let name = qualify(key);
                let list = if name == "minecraft:enchantments" {
                    &mut enchantments
                } else if name == "minecraft:stored_enchantments" {
                    &mut stored_enchantments
                } else {
                    // A component type that is no predicate type: `AnyValue`.
                    match vanillite_data::registry::DATA_COMPONENT_TYPE.id(&name) {
                        Some(id) if !COMPONENT_PREDICATE_TYPES.contains(&name.as_str()) => {
                            present.push(id);
                            continue;
                        }
                        _ => {
                            self.unsupported(format!("item predicate {key} (false)"));
                            return Ok(None);
                        }
                    }
                };
                for e in value
                    .as_array()
                    .ok_or("enchantments predicate is not a list")?
                {
                    let e = object(e)?;
                    let levels = e.get("levels").map(bounds).transpose()?.unwrap_or_default();
                    let ids = match e.get("enchantments").map(id_set).transpose()? {
                        Some(IdSet::Ids(ids)) => Some(ids.into_iter().map(Box::from).collect()),
                        Some(IdSet::Tag(tag)) => {
                            self.unsupported(format!(
                                "enchantment tag #{tag} in a predicate (false)"
                            ));
                            return Ok(None);
                        }
                        None => None,
                    };
                    list.push(EnchantmentPredicate {
                        enchantments: ids,
                        levels,
                    });
                }
            }
        }
        Ok(Some(ItemPredicate {
            items,
            count,
            enchantments: enchantments.into(),
            stored_enchantments: stored_enchantments.into(),
            present: present.into(),
        }))
    }

    /// `BlockPredicate` (the fields of `match_block`); `None` if it uses unsupported parts.
    fn block_predicate(&mut self, o: &Map<String, Value>) -> Res<Option<BlockPredicate>> {
        let blocks = match o.get("blocks").map(id_set).transpose()? {
            None => None,
            Some(IdSet::Ids(names)) => Some(BlockSet::Ids(
                names
                    .iter()
                    .map(|n| block_id(n))
                    .collect::<Res<Box<[_]>>>()?,
            )),
            Some(IdSet::Tag(tag)) => Some(BlockSet::Tag(tag.into())),
        };
        let mut state = Vec::new();
        if let Some(s) = o.get("state") {
            for (name, value) in object(s)? {
                let matcher = match value {
                    Value::Object(range) => ValueMatcher::Range(
                        range.get("min").map(state_value).transpose()?,
                        range.get("max").map(state_value).transpose()?,
                    ),
                    other => ValueMatcher::Exact(state_value(other)?),
                };
                state.push((Box::from(name.as_str()), matcher));
            }
        }
        let component_check = ["components", "predicates"].iter().any(|k| {
            o.get(*k)
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty())
        });
        if o.contains_key("nbt") || component_check {
            self.unsupported("match_block with block entity data (false)".into());
            return Ok(None);
        }
        Ok(Some(BlockPredicate {
            blocks,
            state: state.into(),
        }))
    }

    /// A modifier holder: a named item modifier, a typed function or a list of them.
    fn func(&mut self, v: &Value) -> Res<Func> {
        match v {
            Value::String(_) => self.func_ref(v),
            Value::Array(_) => Ok(Func {
                condition: None,
                kind: FuncKind::Sequence(self.func_list(v)?),
            }),
            other => self.func_typed(object(other)?),
        }
    }

    fn func_ref(&mut self, v: &Value) -> Res<Func> {
        let name = qualify(string(v)?);
        let index = self
            .modifiers
            .get(&name)
            .copied()
            .ok_or_else(|| format!("unknown item modifier {name}"))?;
        Ok(Func {
            condition: None,
            kind: FuncKind::Ref(index),
        })
    }

    /// A holder set of typed functions: a list of item modifier names or typed functions.
    fn func_list(&mut self, v: &Value) -> Res<Box<[Func]>> {
        if let Some(tag) = v.as_str().and_then(|s| s.strip_prefix('#')) {
            self.unsupported(format!("item modifier tag #{tag} (does nothing)"));
            return Ok(Box::new([]));
        }
        compact_list(v)
            .into_iter()
            .map(|f| match f {
                Value::String(_) => self.func_ref(f),
                other => self.func_typed(object(other)?),
            })
            .collect()
    }

    fn func_typed(&mut self, o: &Map<String, Value>) -> Res<Func> {
        let ty = type_of(o)?;
        let condition = self.opt_cond(o)?;
        let kind = match ty.as_str() {
            "minecraft:set_count" => FuncKind::SetCount {
                count: self.int_provider(field(o, "count")?)?,
                add: o.get("add").and_then(Value::as_bool).unwrap_or(false),
            },
            "minecraft:apply_bonus" => {
                let formula = qualify(string(field(o, "formula")?)?);
                let parameters = o.get("parameters").map(object).transpose()?;
                let param = |key: &str| -> Res<&Value> {
                    parameters
                        .and_then(|p| p.get(key))
                        .ok_or_else(|| format!("missing parameter `{key}`"))
                };
                let formula = match formula.as_str() {
                    "minecraft:ore_drops" => Formula::OreDrops,
                    "minecraft:uniform_bonus_count" => Formula::UniformBonusCount {
                        multiplier: int(param("bonusMultiplier")?)?,
                    },
                    "minecraft:binomial_with_bonus_count" => Formula::BinomialWithBonusCount {
                        extra: int(param("extra")?)?,
                        probability: float(param("probability")?)?,
                    },
                    other => return Err(format!("no formula type with id {other}")),
                };
                FuncKind::ApplyBonus {
                    enchantment: qualify(string(field(o, "enchantment")?)?).into(),
                    formula,
                }
            }
            "minecraft:explosion_decay" => FuncKind::ExplosionDecay,
            "minecraft:limit_count" => {
                let limit = object(field(o, "limit")?)?;
                FuncKind::LimitCount {
                    min: limit.get("min").map(|m| self.int_provider(m)).transpose()?,
                    max: limit.get("max").map(|m| self.int_provider(m)).transpose()?,
                }
            }
            "minecraft:copy_state" => {
                let block = block_id(&qualify(string(field(o, "block")?)?))?;
                let names = field(o, "properties")?
                    .as_array()
                    .ok_or("`properties` is not a list")?
                    .iter()
                    .map(string)
                    .collect::<Res<Vec<_>>>()?;
                // As Vanilla, names the block does not have are dropped.
                let properties = names
                    .into_iter()
                    .filter(|n| property_values(block, n).is_some())
                    .map(Box::from)
                    .collect();
                FuncKind::CopyState { properties }
            }
            "minecraft:sequence" => FuncKind::Sequence(self.func_list(field(o, "functions")?)?),
            "minecraft:discard" => FuncKind::Discard,
            "minecraft:enchanted_count_increase" => FuncKind::EnchantedCountIncrease {
                enchantment: qualify(string(field(o, "enchantment")?)?).into(),
                count: self.float_provider(field(o, "count")?)?,
                limit: o.get("limit").map(int).transpose()?.unwrap_or(0),
            },
            "minecraft:furnace_smelt" => FuncKind::FurnaceSmelt,
            "minecraft:copy_components"
                if o.get("source").and_then(Value::as_str) == Some("block_entity") =>
            {
                let include = o
                    .get("include")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .filter_map(|n| {
                                vanillite_data::registry::DATA_COMPONENT_TYPE.id(&qualify(n))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                FuncKind::CopyComponents(include)
            }
            "minecraft:copy_name"
                if o.get("source").and_then(Value::as_str) == Some("block_entity") =>
            {
                FuncKind::CopyComponents(Box::new([
                    vanillite_protocol::components::ids::CUSTOM_NAME,
                ]))
            }
            "minecraft:enchant_randomly" => FuncKind::EnchantRandomly {
                options: o.get("options").map(holder_entries).transpose()?,
                only_compatible: o
                    .get("only_compatible")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                additional_cost: o
                    .get("include_additional_cost_component")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            "minecraft:enchant_with_levels" => FuncKind::EnchantWithLevels {
                levels: self.int_provider(field(o, "levels")?)?,
                options: o.get("options").map(holder_entries).transpose()?,
                additional_cost: o
                    .get("include_additional_cost_component")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            "minecraft:set_random_dyes" => {
                FuncKind::SetRandomDyes(self.int_provider(field(o, "number_of_dyes")?)?)
            }
            "minecraft:set_random_potion" => {
                FuncKind::SetRandomPotion(o.get("options").map(holder_entries).transpose()?)
            }
            "minecraft:set_enchantments" => {
                let mut enchantments = Vec::new();
                if let Some(map) = o.get("enchantments") {
                    for (id, level) in object(map)? {
                        enchantments.push((qualify(id).into(), self.int_provider(level)?));
                    }
                }
                FuncKind::SetEnchantments {
                    enchantments: enchantments.into(),
                    add: o.get("add").and_then(Value::as_bool).unwrap_or(false),
                }
            }
            "minecraft:set_damage" => FuncKind::SetDamage {
                damage: self.float_provider(field(o, "damage")?)?,
                add: o.get("add").and_then(Value::as_bool).unwrap_or(false),
            },
            "minecraft:set_name" => {
                let target = match o.get("target").and_then(Value::as_str) {
                    None | Some("custom_name") => ids::CUSTOM_NAME,
                    Some("item_name") => ids::ITEM_NAME,
                    Some(other) => return Err(format!("unknown set_name target {other}")),
                };
                let name = o.get("name").map(|n| {
                    let mut out = Vec::new();
                    vanillite_protocol::codec::Encode::encode(
                        &vanillite_protocol::text::Component::from_json(n),
                        &mut out,
                    );
                    out.into_boxed_slice()
                });
                FuncKind::SetName { target, name }
            }
            "minecraft:set_potion" => {
                let name = qualify(string(field(o, "id")?)?);
                let id = vanillite_data::registry::POTION
                    .id(&name)
                    .ok_or_else(|| format!("unknown potion {name}"))?;
                FuncKind::SetPotion(u32::from(id))
            }
            "minecraft:set_stew_effect" => {
                let mut effects = Vec::new();
                for e in o
                    .get("effects")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    let e = object(e)?;
                    let name = qualify(string(field(e, "type")?)?);
                    let id = vanillite_data::registry::MOB_EFFECT
                        .id(&name)
                        .ok_or_else(|| format!("unknown mob effect {name}"))?;
                    // `MobEffect.isInstantaneous`: `InstantenousMobEffect` subclasses.
                    let instantaneous = matches!(
                        name.as_str(),
                        "minecraft:instant_health"
                            | "minecraft:instant_damage"
                            | "minecraft:saturation"
                    );
                    effects.push((
                        u32::from(id),
                        self.int_provider(field(e, "duration")?)?,
                        instantaneous,
                    ));
                }
                FuncKind::SetStewEffect(effects.into())
            }
            "minecraft:set_instrument" => {
                FuncKind::SetInstrument(holder_entries(field(o, "options")?)?)
            }
            "minecraft:set_ominous_bottle_amplifier" => {
                FuncKind::SetOminousBottleAmplifier(self.int_provider(field(o, "amplifier")?)?)
            }
            "minecraft:filtered" => {
                let filter = self.item_predicate(field(o, "item_filter")?)?.map(Box::new);
                let on_pass = o.get("on_pass").map(|f| self.func(f)).transpose()?;
                let on_fail = o.get("on_fail").map(|f| self.func(f)).transpose()?;
                FuncKind::Filtered {
                    filter,
                    on_pass: on_pass.map(Box::new),
                    on_fail: on_fail.map(Box::new),
                }
            }
            "minecraft:exploration_map" => {
                let with_ns = |s: &str| {
                    if s.contains(':') {
                        s.to_owned()
                    } else {
                        format!("minecraft:{s}")
                    }
                };
                let destination = o.get("destination").and_then(Value::as_str).map_or_else(
                    || "minecraft:on_treasure_maps".to_owned(),
                    |s| with_ns(s.trim_start_matches('#')),
                );
                let decoration = o
                    .get("decoration")
                    .and_then(Value::as_str)
                    .map_or_else(|| "minecraft:mansion".to_owned(), with_ns);
                let zoom = o
                    .get("zoom")
                    .and_then(Value::as_i64)
                    .and_then(|z| i8::try_from(z).ok())
                    .unwrap_or(2);
                let search_radius = o
                    .get("search_radius")
                    .and_then(Value::as_i64)
                    .and_then(|r| i32::try_from(r).ok())
                    .unwrap_or(50);
                let skip_existing_chunks = o
                    .get("skip_existing_chunks")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                FuncKind::ExplorationMap(Box::new(ExplorationMap {
                    destination,
                    decoration,
                    zoom,
                    search_radius,
                    skip_existing_chunks,
                }))
            }
            "minecraft:set_components" => {
                let mut components = Vec::new();
                for (key, value) in object(field(o, "components")?)? {
                    let (name, removed) = match key.strip_prefix('!') {
                        Some(name) => (name, true),
                        None => (key.as_str(), false),
                    };
                    let id = vanillite_protocol::components_nbt::type_by_name(name)
                        .ok_or_else(|| format!("unknown component {name}"))?;
                    let value = (!removed).then(|| crate::trading::data::json_to_nbt(value));
                    components.push((id, value));
                }
                FuncKind::SetComponents(components.into())
            }
            "minecraft:set_custom_data" => {
                let tag = match field(o, "tag")? {
                    Value::String(snbt) => {
                        vanillite_nbt::snbt::parse_compound(snbt).map_err(|e| e.to_string())?
                    }
                    other => match crate::trading::data::json_to_nbt(other) {
                        vanillite_nbt::Tag::Compound(c) => c,
                        _ => return Err("`tag` is not a compound".into()),
                    },
                };
                FuncKind::SetCustomData(Box::new(tag))
            }
            "minecraft:set_lore" => {
                let lore = field(o, "lore")?
                    .as_array()
                    .ok_or("`lore` is not a list")?
                    .iter()
                    .map(crate::trading::data::json_to_nbt)
                    .collect();
                FuncKind::SetLore {
                    lore,
                    mode: ListOperation::parse(o)?,
                }
            }
            "minecraft:copy_components" | "minecraft:copy_name" | "minecraft:copy_custom_data" => {
                FuncKind::Noop
            }
            other => {
                self.unsupported(format!("modifier {other} (does nothing)"));
                FuncKind::Noop
            }
        };
        Ok(Func { condition, kind })
    }

    fn int_provider(&mut self, v: &Value) -> Res<IntProvider> {
        if v.is_number() {
            return Ok(IntProvider::Const(int(v)?));
        }
        let o = object(v)?;
        Ok(match type_of(o)?.as_str() {
            "minecraft:constant" => IntProvider::Const(int(field(o, "value")?)?),
            "minecraft:uniform" => IntProvider::Uniform(Box::new((
                self.int_provider(field(o, "min")?)?,
                self.int_provider(field(o, "max")?)?,
            ))),
            "minecraft:binomial" => IntProvider::Binomial(Box::new((
                self.int_provider(field(o, "n")?)?,
                self.float_provider(field(o, "p")?)?,
            ))),
            "minecraft:add" => IntProvider::Sum(
                field(o, "inputs")?
                    .as_array()
                    .ok_or("`inputs` is not a list")?
                    .iter()
                    .map(|v| self.int_provider(v))
                    .collect::<Res<Vec<_>>>()?
                    .into(),
            ),
            other => return Err(format!("unsupported int provider {other}")),
        })
    }

    fn float_provider(&mut self, v: &Value) -> Res<FloatProvider> {
        if v.is_number() {
            return Ok(FloatProvider::Const(float(v)?));
        }
        let o = object(v)?;
        Ok(match type_of(o)?.as_str() {
            "minecraft:constant" => FloatProvider::Const(float(field(o, "value")?)?),
            "minecraft:uniform" => FloatProvider::Uniform(Box::new((
                self.float_provider(field(o, "min")?)?,
                self.float_provider(field(o, "max")?)?,
            ))),
            other => return Err(format!("unsupported float provider {other}")),
        })
    }
}

/// The types of `minecraft:data_component_predicate_type` (component predicates with fields;
/// other component type names are `AnyValue` checks).
const COMPONENT_PREDICATE_TYPES: [&str; 15] = [
    "minecraft:damage",
    "minecraft:enchantments",
    "minecraft:stored_enchantments",
    "minecraft:potion_contents",
    "minecraft:custom_data",
    "minecraft:container",
    "minecraft:bundle_contents",
    "minecraft:firework_explosion",
    "minecraft:fireworks",
    "minecraft:writable_book_content",
    "minecraft:written_book_content",
    "minecraft:attribute_modifiers",
    "minecraft:trim",
    "minecraft:jukebox_playable",
    "minecraft:villager/variant",
];

/// A holder set (`RegistryCodecs.holderSet`): `"id"`, `"#tag"` or a list of ids, qualified.
fn holder_entries(v: &Value) -> Res<Box<[Box<str>]>> {
    if let Some(tag) = v.as_str().and_then(|s| s.strip_prefix('#')) {
        return Ok(Box::new([format!("#{}", qualify(tag)).into()]));
    }
    compact_list(v)
        .into_iter()
        .map(|e| string(e).map(|s| qualify(s).into()))
        .collect()
}

/// The values of `block`'s property `name`, in Vanilla's order.
pub(crate) fn property_values(block: BlockId, name: &str) -> Option<&'static [&'static str]> {
    block
        .info()
        .properties
        .iter()
        .filter_map(|&p| PROPERTIES.get(usize::from(p)))
        .find(|p| p.name == name)
        .map(|p| p.values)
}

/// A property value as the property compares it: integers numerically, booleans false < true,
/// enums by declaration order. `None` if `value` is not a value of the property.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PropertyKey {
    Int(i64),
    Bool(bool),
    Index(usize),
}

pub(crate) fn property_key(values: &[&str], value: &str) -> Option<PropertyKey> {
    if (value == "true" || value == "false") && values.contains(&value) {
        return Some(PropertyKey::Bool(value == "true"));
    }
    if let Ok(n) = value.parse::<i64>()
        && values.iter().any(|v| v.parse::<i64>() == Ok(n))
    {
        return Some(PropertyKey::Int(n));
    }
    values
        .iter()
        .position(|v| *v == value)
        .map(PropertyKey::Index)
}

// ---------------------------------------------------------------------------------------------
// Evaluation.

/// One expanded pool entry of a roll.
struct Candidate<'t> {
    leaf: Leaf<'t>,
    /// Modifiers of the entry and its composite parents, outermost first.
    chain: Vec<&'t Func>,
    weight: i32,
}

enum Leaf<'t> {
    Entry(&'t Entry),
    Item(u16),
    Table(&'t TableRef),
}

struct Eval<'t, 'x> {
    tables: &'t LootTables,
    ctx: &'x LootContext<'x>,
    rng: &'x mut dyn LootRandom,
    out: &'x mut Vec<ItemStack>,
    /// Modifiers every created stack passes, applied last to first.
    post: Vec<&'t Func>,
    depth: u32,
    /// `ADDITIONAL_COST_COMPONENT_ALLOWED` (villager trade contexts).
    allow_additional_cost: bool,
    /// Empty results (a `discard` modifier) stay in the output as empty stacks, as
    /// `LootTable.getRandomItems` passes them on (`/loot`).
    keep_empty: bool,
}

impl<'t> Eval<'t, '_> {
    fn enter(&mut self) -> bool {
        if self.depth >= MAX_DEPTH {
            log::warn!("loot tables: references nested deeper than {MAX_DEPTH}; ignored");
            return false;
        }
        self.depth += 1;
        true
    }

    fn table(&mut self, table: &'t Table) {
        if !self.enter() {
            return;
        }
        let mark = self.post.len();
        if let Some(m) = &table.modifier {
            self.post.push(m);
        }
        for pool in &table.pools {
            self.pool(pool);
        }
        self.post.truncate(mark);
        self.depth -= 1;
    }

    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    fn pool(&mut self, pool: &'t Pool) {
        if let Some(c) = &pool.condition
            && !self.test(c)
        {
            return;
        }
        let mark = self.post.len();
        if let Some(m) = &pool.modifier {
            self.post.push(m);
        }
        let rolls = self.int(&pool.rolls);
        let bonus = self.float(&pool.bonus_rolls);
        // `Mth.floor`; Rust's float to int cast saturates like Java's.
        let count = rolls.wrapping_add((bonus * self.ctx.luck).floor() as i32);
        for _ in 0..count {
            self.roll(pool);
        }
        self.post.truncate(mark);
    }

    /// `LootPool.addRandomItem`.
    fn roll(&mut self, pool: &'t Pool) {
        let mut candidates = Vec::new();
        let mut chain = Vec::new();
        for entry in &pool.entries {
            self.expand(entry, &mut chain, &mut candidates);
        }
        let total = candidates
            .iter()
            .fold(0i32, |sum, c| sum.wrapping_add(c.weight));
        if total == 0 || candidates.is_empty() {
            return;
        }
        if candidates.len() == 1 {
            if let Some(c) = candidates.pop() {
                self.create(&c);
            }
            return;
        }
        if total < 0 {
            // Java's `nextInt` throws for an overflowed total.
            return;
        }
        let mut index = self.rng.next_int(total);
        for c in &candidates {
            index = index.wrapping_sub(c.weight);
            if index < 0 {
                self.create(c);
                return;
            }
        }
    }

    /// `LootPoolEntryContainer.expand`: adds the entries `entry` yields; whether it succeeded.
    fn expand(
        &mut self,
        entry: &'t Entry,
        chain: &mut Vec<&'t Func>,
        out: &mut Vec<Candidate<'t>>,
    ) -> bool {
        if let Some(c) = &entry.condition
            && !self.test(c)
        {
            return false;
        }
        let pushed = entry.modifier.as_ref().map(|m| chain.push(m)).is_some();
        let weight = entry_weight(entry, self.ctx.luck);
        let result = match &entry.kind {
            EntryKind::Item(_) | EntryKind::Empty | EntryKind::Dynamic(_) => {
                if weight > 0 {
                    out.push(Candidate {
                        leaf: Leaf::Entry(entry),
                        chain: chain.clone(),
                        weight,
                    });
                }
                true
            }
            EntryKind::Items {
                items,
                expand: true,
            } => {
                if weight > 0 {
                    for item in self.items_of(items) {
                        out.push(Candidate {
                            leaf: Leaf::Item(item),
                            chain: chain.clone(),
                            weight,
                        });
                    }
                }
                true
            }
            EntryKind::Tables {
                tables,
                expand: true,
            } => {
                if weight > 0 {
                    for table in tables {
                        out.push(Candidate {
                            leaf: Leaf::Table(table),
                            chain: chain.clone(),
                            weight,
                        });
                    }
                }
                true
            }
            EntryKind::Items { .. } | EntryKind::Tables { .. } => {
                if weight > 0 {
                    out.push(Candidate {
                        leaf: Leaf::Entry(entry),
                        chain: chain.clone(),
                        weight,
                    });
                }
                true
            }
            EntryKind::Alternatives(children) => {
                let mut any = false;
                for child in children {
                    if self.expand(child, chain, out) {
                        any = true;
                        break;
                    }
                }
                any
            }
            EntryKind::Sequence(children) => {
                let mut all = true;
                for child in children {
                    if !self.expand(child, chain, out) {
                        all = false;
                        break;
                    }
                }
                all
            }
            EntryKind::Group(children) => {
                for child in children {
                    self.expand(child, chain, out);
                }
                true
            }
        };
        if pushed {
            chain.pop();
        }
        result
    }

    /// The items of a set, in id order for tags.
    fn items_of(&self, items: &ItemSet) -> Vec<u16> {
        match items {
            ItemSet::Items(ids) => ids.to_vec(),
            ItemSet::Tag(tag) => (0..ITEMS.len())
                .filter_map(|i| u16::try_from(i).ok())
                .filter(|&i| (self.ctx.item_in_tag)(i, tag))
                .collect(),
        }
    }

    /// `LootPoolEntry.createItemStack`.
    fn create(&mut self, candidate: &Candidate<'t>) {
        let mark = self.post.len();
        self.post.extend(candidate.chain.iter().copied());
        match candidate.leaf {
            Leaf::Item(item) => self.emit(ItemStack::new(item, 1)),
            Leaf::Table(table) => self.nested(table),
            Leaf::Entry(entry) => match &entry.kind {
                EntryKind::Item(item) => self.emit(ItemStack::new(*item, 1)),
                EntryKind::Items { items, .. } => {
                    for item in self.items_of(items) {
                        self.emit(ItemStack::new(item, 1));
                    }
                }
                EntryKind::Tables { tables, .. } => {
                    for table in tables {
                        self.nested(table);
                    }
                }
                EntryKind::Dynamic(name) if &**name == "minecraft:sherds" => {
                    // `DecoratedPotBlock.getDrops`: left, back, front, right.
                    let decorations = self
                        .ctx
                        .block_entity
                        .and_then(|f| f(vanillite_protocol::components::ids::POT_DECORATIONS));
                    if let Some(value) = decorations {
                        for stack in crate::block::pot::sherds_of(&value) {
                            self.emit(stack);
                        }
                    }
                }
                _ => {}
            },
        }
        self.post.truncate(mark);
    }

    fn nested(&mut self, table: &'t TableRef) {
        let tables = self.tables;
        match table {
            TableRef::Named(name) => {
                if let Some(t) = tables
                    .by_name
                    .get(name)
                    .and_then(|&i| tables.tables.get(usize::try_from(i).ok()?))
                {
                    self.table(t);
                }
            }
            TableRef::Inline(t) => self.table(t),
        }
    }

    /// Passes a new stack through the pending modifiers into the output, split by max stack
    /// size (`LootTable.createStackSplitter`).
    fn emit(&mut self, mut stack: ItemStack) {
        for i in (0..self.post.len()).rev() {
            if let Some(&f) = self.post.get(i) {
                self.apply(f, &mut stack);
            }
        }
        if stack.is_empty() {
            if self.keep_empty {
                self.out.push(ItemStack::default());
            }
            return;
        }
        let max = crate::item::max_stack_size(&stack).max(1);
        if stack.count < max {
            self.out.push(stack);
            return;
        }
        let mut count = stack.count;
        while count > 0 {
            let n = count.min(max);
            self.out.push(stack.with_count(n));
            count -= n;
        }
    }

    fn test(&mut self, cond: &'t Cond) -> bool {
        match cond {
            Cond::Ref(i) => {
                let tables = self.tables;
                let Some(c) = usize::try_from(*i)
                    .ok()
                    .and_then(|i| tables.predicates.get(i))
                else {
                    return false;
                };
                if !self.enter() {
                    return false;
                }
                let result = self.test(c);
                self.depth -= 1;
                result
            }
            Cond::Const(b) => *b,
            Cond::Inverted(c) => !self.test(c),
            Cond::AnyOf(terms) => terms.iter().any(|c| self.test(c)),
            Cond::AllOf(terms) => terms.iter().all(|c| self.test(c)),
            Cond::SurvivesExplosion => match self.ctx.explosion_radius {
                Some(radius) => self.rng.next_float() <= 1.0 / radius,
                None => true,
            },
            Cond::RandomChance(chance) => {
                let chance = self.float(chance);
                self.rng.next_float() < chance
            }
            Cond::TableBonus {
                enchantment,
                chances,
            } => {
                let level = usize::try_from((self.ctx.enchantment_level)(enchantment)).unwrap_or(0);
                let chance = chances
                    .get(level.min(chances.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or(0.0);
                self.rng.next_float() < chance
            }
            Cond::MatchTool(None) => self.ctx.tool.is_some(),
            Cond::MatchTool(Some(p)) => self
                .ctx
                .tool
                .is_some_and(|tool| self.item_matches(p, tool, self.ctx.enchantment_level)),
            Cond::MatchBlock(p) => self
                .ctx
                .block_state
                .is_some_and(|state| block_matches(p, state, self.tables.block_tags.as_deref())),
            // Without level access the neighbour is assumed to be as Vanilla left it.
            Cond::LocationBlock([dx, dy, dz], p) => self.ctx.neighbour.is_none_or(|at| {
                at(*dx, *dy, *dz)
                    .is_some_and(|s| block_matches(p, s, self.tables.block_tags.as_deref()))
            }),
            Cond::Biome(biomes) => {
                self.ctx.origin.is_some()
                    && self
                        .ctx
                        .biome
                        .is_some_and(|biome| biomes.iter().any(|b| biome(b)))
            }
            Cond::ThisPresent => {
                self.ctx.this_present || self.ctx.this_entity.is_some() || self.ctx.entity.is_some()
            }
            Cond::Entity(target, p) => {
                let Some(e) = self.ctx.entity else {
                    return false;
                };
                let facts = match target {
                    EntityTarget::This => Some(e.this),
                    EntityTarget::Attacker => e.attacker,
                    EntityTarget::DirectAttacker => e.direct_attacker,
                    EntityTarget::AttackingPlayer => e.last_damage_player,
                };
                p.test(facts.as_ref(), e.tags)
            }
            Cond::KilledByPlayer => self
                .ctx
                .entity
                .is_some_and(|e| e.last_damage_player.is_some()),
            Cond::Weather {
                raining,
                thundering,
            } => self.ctx.weather.is_some_and(|(rain, thunder)| {
                raining.is_none_or(|r| r == rain) && thundering.is_none_or(|t| t == thunder)
            }),
            Cond::IntValue(value, test) => {
                let v = self.int(value);
                test.matches(v)
            }
            Cond::Scores(target, scores) => {
                let (EntityTarget::This, Some(id), Some(score)) =
                    (target, self.ctx.this_entity, self.ctx.score)
                else {
                    return false;
                };
                scores.iter().all(|(objective, range)| {
                    score(id, objective).is_some_and(|v| range.matches(v))
                })
            }
            Cond::DamageSource(p) => self.ctx.entity.is_some_and(|e| {
                e.source
                    .as_ref()
                    .is_some_and(|s| p.test(s, e.types, e.tags))
            }),
            Cond::EnchantedChance {
                enchantment,
                unenchanted,
                enchanted,
            } => {
                let level = self
                    .ctx
                    .entity
                    .map_or(0, |e| (e.attacker_enchantment_level)(enchantment));
                let chance = if level > 0 {
                    enchanted.calculate(level)
                } else {
                    *unenchanted
                };
                self.rng.next_float() < chance
            }
        }
    }

    /// `ItemPredicate.test(stack)`; `enchantment_level` gives the stack's level of an
    /// enchantment id.
    fn item_matches(
        &self,
        p: &ItemPredicate,
        stack: &ItemStack,
        enchantment_level: &dyn Fn(&str) -> i32,
    ) -> bool {
        let (item, count) = if stack.is_empty() {
            (0, 0)
        } else {
            (stack.item, stack.count)
        };
        let item_ok = match &p.items {
            None => true,
            Some(ItemSet::Items(ids)) => ids.contains(&item),
            Some(ItemSet::Tag(tag)) => (self.ctx.item_in_tag)(item, tag),
        };
        item_ok
            && p.count.matches(count)
            && p.present.iter().all(|&id| stack.component(id).is_some())
            && p.enchantments.iter().all(|e| match &e.enchantments {
                Some(ids) => ids.iter().any(|id| {
                    let level = enchantment_level(id);
                    level != 0 && (e.levels.is_any() || e.levels.matches(level))
                }),
                None => any_enchantment(&crate::item::enchantments(stack), e.levels),
            })
            && (p.stored_enchantments.is_empty() || {
                let stored = crate::item::stored_enchantments(stack);
                p.stored_enchantments.iter().all(|e| match &e.enchantments {
                    Some(ids) => ids.iter().any(|id| {
                        let ids = (self.ctx.holder_set)("minecraft:enchantment", id);
                        stored
                            .iter()
                            .any(|(s, level)| ids.contains(s) && e.levels.matches(*level))
                    }),
                    None => any_enchantment(&stored, e.levels),
                })
            })
    }

    /// The level of the enchantment `name` in the stack's `enchantments`.
    fn stack_enchantment_level(&self, stack: &ItemStack, name: &str) -> i32 {
        let ids = (self.ctx.holder_set)("minecraft:enchantment", name);
        crate::item::enchantments(stack)
            .into_iter()
            .find(|(e, _)| ids.contains(e))
            .map_or(0, |(_, level)| level)
    }

    /// The enchantment ids of an `options` holder set (every enchantment without one).
    fn enchantment_options(&self, options: Option<&[Box<str>]>) -> Vec<u32> {
        match options {
            Some(entries) => entries
                .iter()
                .flat_map(|e| (self.ctx.holder_set)("minecraft:enchantment", e))
                .collect(),
            None => self.ctx.enchanting.map_or_else(Vec::new, |e| {
                (0..e.list.len())
                    .filter_map(|i| u32::try_from(i).ok())
                    .collect()
            }),
        }
    }

    fn apply(&mut self, func: &'t Func, stack: &mut ItemStack) {
        if let Some(c) = &func.condition
            && !self.test(c)
        {
            return;
        }
        match &func.kind {
            FuncKind::Ref(i) => {
                let tables = self.tables;
                if let Some(f) = usize::try_from(*i)
                    .ok()
                    .and_then(|i| tables.modifiers.get(i))
                    && self.enter()
                {
                    self.apply(f, stack);
                    self.depth -= 1;
                }
            }
            FuncKind::Sequence(funcs) => {
                for f in funcs {
                    self.apply(f, stack);
                }
            }
            FuncKind::SetCount { count, add } => {
                let base = if *add { stack.count } else { 0 };
                stack.count = base.wrapping_add(self.int(count));
            }
            FuncKind::ApplyBonus {
                enchantment,
                formula,
            } => {
                let level = (self.ctx.enchantment_level)(enchantment);
                stack.count = self.bonus(formula, stack.count, level);
            }
            FuncKind::ExplosionDecay => {
                if let Some(radius) = self.ctx.explosion_radius {
                    let probability = 1.0 / radius;
                    let mut kept = 0i32;
                    for _ in 0..stack.count {
                        if self.rng.next_float() <= probability {
                            kept += 1;
                        }
                    }
                    stack.count = kept;
                }
            }
            FuncKind::LimitCount { min, max } => {
                let min = min.as_ref().map(|m| self.int(m));
                let max = max.as_ref().map(|m| self.int(m));
                let value = stack.count;
                stack.count = match (min, max) {
                    (None, None) => value,
                    (None, Some(max)) => value.min(max),
                    (Some(min), None) => value.max(min),
                    // `Mth.clamp`.
                    (Some(min), Some(max)) => {
                        if value < min {
                            min
                        } else {
                            value.min(max)
                        }
                    }
                };
            }
            FuncKind::CopyState { properties } => {
                if let Some(state) = self.ctx.block_state {
                    copy_state(stack, state, properties);
                }
            }
            FuncKind::Discard => *stack = ItemStack::EMPTY,
            FuncKind::EnchantedCountIncrease {
                enchantment,
                count,
                limit,
            } => {
                let level = self
                    .ctx
                    .entity
                    .map_or(0, |e| (e.attacker_enchantment_level)(enchantment));
                if level > 0 {
                    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                    let extra = (level as f32 * self.float(count)).round() as i32;
                    stack.count = stack.count.saturating_add(extra);
                    if *limit > 0 {
                        stack.count = stack.count.min(*limit);
                    }
                }
            }
            FuncKind::FurnaceSmelt => {
                if let Some(cooked) = smelted(stack.item) {
                    stack.item = cooked;
                }
            }
            FuncKind::CopyComponents(include) => {
                if let Some(source) = self.ctx.block_entity {
                    for &id in include.iter() {
                        if let Some(value) = source(id) {
                            stack.set_component(id, value);
                        }
                    }
                }
            }
            FuncKind::EnchantRandomly {
                options,
                only_compatible,
                additional_cost,
            } => {
                if let Some(level) =
                    self.enchant_randomly(stack, options.as_deref(), *only_compatible)
                    && *additional_cost
                    && self.allow_additional_cost
                {
                    let bound = 5i32.wrapping_add(level.wrapping_mul(10));
                    let cost = 2i32
                        .wrapping_add(self.rng.next_int(bound.max(1)))
                        .wrapping_add(level.wrapping_mul(3));
                    set_additional_trade_cost(stack, cost);
                }
            }
            FuncKind::EnchantWithLevels {
                levels,
                options,
                additional_cost,
            } => {
                let cost = self.int(levels);
                let Some(e) = self.ctx.enchanting else {
                    return;
                };
                let candidates = self.enchantment_options(options.as_deref());
                let chosen =
                    crate::enchant::select_enchantment(e, &mut *self.rng, stack, cost, &candidates);
                // `EnchantmentHelper.enchantItem`.
                if stack.item == items::BOOK.0 {
                    *stack = ItemStack::new(items::ENCHANTED_BOOK.0, 1);
                }
                for (id, level) in chosen {
                    upgrade_enchantment(stack, id, level);
                }
                if *additional_cost && self.allow_additional_cost && !stack.is_empty() && cost > 0 {
                    set_additional_trade_cost(stack, cost);
                }
            }
            FuncKind::SetRandomDyes(count) => {
                // `SetRandomDyesFunction.run`: `Util.getRandom(DyeColor.VALUES, random)` each.
                let rolls = self.int(count);
                if rolls > 0 {
                    let mut dyes = Vec::with_capacity(usize::try_from(rolls).unwrap_or(0));
                    for _ in 0..rolls {
                        dyes.push(self.rng.next_int(16));
                    }
                    let current = stack
                        .component(ids::DYED_COLOR)
                        .and_then(|v| Reader::new(v).i32().ok());
                    let rgb = crate::recipe::apply_dyes(current, &dyes);
                    *stack = stack.with_count(1);
                    stack.set_component(ids::DYED_COLOR, Box::new(rgb.to_be_bytes()));
                }
            }
            FuncKind::SetRandomPotion(options) => {
                // `HolderSet.getRandomElement` / `Registry.getRandom`: `Util.getRandomSafe`.
                let potions: Vec<u32> = match options {
                    Some(options) => options
                        .iter()
                        .flat_map(|o| (self.ctx.holder_set)("minecraft:potion", o))
                        .collect(),
                    None => (0..vanillite_data::registry::POTION.len())
                        .filter_map(|i| u32::try_from(i).ok())
                        .collect(),
                };
                if potions.is_empty() {
                    return;
                }
                let len = i32::try_from(potions.len()).unwrap_or(i32::MAX);
                let pick = usize::try_from(self.rng.next_int(len)).unwrap_or(0);
                if let Some(&potion) = potions.get(pick) {
                    let mut out = vec![1];
                    vanillite_protocol::codec::write_varint(
                        &mut out,
                        i32::try_from(potion).unwrap_or(0),
                    );
                    out.extend_from_slice(&[0, 0, 0]);
                    stack.set_component(ids::POTION_CONTENTS, out.into_boxed_slice());
                }
            }
            FuncKind::SetEnchantments { enchantments, add } => {
                if stack.item == items::BOOK.0 {
                    // `transmuteCopy`: count and components stay.
                    stack.item = items::ENCHANTED_BOOK.0;
                }
                let component = enchantment_component(stack);
                let mut list = crate::enchant::read(stack.component(component));
                for (name, level) in enchantments.iter() {
                    let value = self.int(level);
                    for id in (self.ctx.holder_set)("minecraft:enchantment", name) {
                        let current = list.iter().find(|(e, _)| *e == id).map_or(0, |e| e.1);
                        let level = if *add {
                            current.wrapping_add(value)
                        } else {
                            value
                        };
                        set_enchantment(&mut list, id, level.clamp(0, 255));
                    }
                }
                stack.set_component(component, crate::enchant::write(&list));
            }
            FuncKind::SetDamage { damage, add } => {
                if crate::item::is_damageable(stack) {
                    let max = crate::item::max_damage(stack);
                    #[allow(
                        clippy::as_conversions,
                        clippy::cast_precision_loss,
                        clippy::cast_possible_truncation
                    )]
                    {
                        let base = if *add {
                            1.0 - crate::item::damage(stack) as f32 / max as f32
                        } else {
                            0.0
                        };
                        let pct = 1.0 - (self.float(damage) + base).clamp(0.0, 1.0);
                        crate::item::set_damage(stack, (pct * max as f32).floor() as i32);
                    }
                } else {
                    log::warn!("Couldn't set damage of loot item {stack:?}");
                }
            }
            FuncKind::SetName { target, name } => {
                if let Some(name) = name {
                    stack.set_component(*target, name.clone());
                }
            }
            FuncKind::SetPotion(potion) => {
                // `PotionContents.EMPTY.withPotion(potion)`: no colour, effects or name.
                let mut out = vec![1];
                vanillite_protocol::codec::write_varint(
                    &mut out,
                    i32::try_from(*potion).unwrap_or(0),
                );
                out.extend_from_slice(&[0, 0, 0]);
                stack.set_component(ids::POTION_CONTENTS, out.into_boxed_slice());
            }
            FuncKind::SetStewEffect(effects) => {
                if stack.item != items::SUSPICIOUS_STEW.0 || effects.is_empty() {
                    return;
                }
                let len = i32::try_from(effects.len()).unwrap_or(i32::MAX);
                let pick = usize::try_from(self.rng.next_int(len)).unwrap_or(0);
                let Some((effect, duration, instantaneous)) = effects.get(pick) else {
                    return;
                };
                let mut duration = self.int(duration);
                if !instantaneous {
                    duration = duration.wrapping_mul(20);
                }
                let mut list = read_stew_effects(stack.component(ids::SUSPICIOUS_STEW_EFFECTS));
                list.push((*effect, duration));
                let mut out = Vec::new();
                write_len(&mut out, list.len());
                for (effect, duration) in list {
                    vanillite_protocol::codec::write_varint(
                        &mut out,
                        i32::try_from(effect).unwrap_or(0),
                    );
                    vanillite_protocol::codec::write_varint(&mut out, duration);
                }
                stack.set_component(ids::SUSPICIOUS_STEW_EFFECTS, out.into_boxed_slice());
            }
            FuncKind::SetInstrument(options) => {
                let instruments: Vec<u32> = options
                    .iter()
                    .flat_map(|o| (self.ctx.holder_set)("minecraft:instrument", o))
                    .collect();
                if instruments.is_empty() {
                    return;
                }
                let len = i32::try_from(instruments.len()).unwrap_or(i32::MAX);
                let pick = usize::try_from(self.rng.next_int(len)).unwrap_or(0);
                if let Some(&id) = instruments.get(pick) {
                    // A registry holder: id + 1.
                    let mut out = Vec::new();
                    vanillite_protocol::codec::write_varint(
                        &mut out,
                        i32::try_from(id).unwrap_or(-1).wrapping_add(1),
                    );
                    stack.set_component(ids::INSTRUMENT, out.into_boxed_slice());
                }
            }
            FuncKind::SetOminousBottleAmplifier(amplifier) => {
                let value = self.int(amplifier).clamp(0, 4);
                let mut out = Vec::new();
                vanillite_protocol::codec::write_varint(&mut out, value);
                stack.set_component(ids::OMINOUS_BOTTLE_AMPLIFIER, out.into_boxed_slice());
            }
            FuncKind::SetCustomData(tag) => {
                // `CustomData.update(CUSTOM_DATA, stack, t -> t.merge(tag))`.
                let Some(id) =
                    vanillite_protocol::components_nbt::type_by_name("minecraft:custom_data")
                else {
                    return;
                };
                let names = &vanillite_protocol::item::InstalledRegistries;
                let mut data = match stack
                    .component(id)
                    .and_then(|w| vanillite_protocol::components_nbt::to_nbt(id, w, names).ok())
                {
                    Some(vanillite_nbt::Tag::Compound(c)) => c,
                    _ => vanillite_nbt::Compound::new(),
                };
                merge_compound(&mut data, tag);
                if let Ok(wire) = vanillite_protocol::components_nbt::from_nbt(
                    id,
                    &vanillite_nbt::Tag::Compound(data),
                    names,
                ) {
                    stack.patch_mut().set(id, wire);
                }
            }
            FuncKind::SetLore { lore, mode } => {
                let Some(id) = vanillite_protocol::components_nbt::type_by_name("minecraft:lore")
                else {
                    return;
                };
                let names = &vanillite_protocol::item::InstalledRegistries;
                let old: Vec<vanillite_nbt::Tag> = match stack
                    .component(id)
                    .and_then(|w| vanillite_protocol::components_nbt::to_nbt(id, w, names).ok())
                {
                    Some(vanillite_nbt::Tag::List(l)) => l.0,
                    _ => Vec::new(),
                };
                // `ItemLore.MAX_LINES`.
                let Some(lines) = mode.apply(&old, lore, 256) else {
                    return;
                };
                if let Ok(wire) = vanillite_protocol::components_nbt::from_nbt(
                    id,
                    &vanillite_nbt::Tag::List(vanillite_nbt::List(lines)),
                    names,
                ) {
                    stack.patch_mut().set(id, wire);
                }
            }
            FuncKind::SetComponents(components) => {
                // `applyComponentsAndValidate`: a patch that leaves the stack invalid is undone.
                let old = stack.patch.clone();
                for (id, value) in components {
                    let Some(value) = value else {
                        stack.remove_component(*id);
                        continue;
                    };
                    match vanillite_protocol::components_nbt::from_nbt(
                        *id,
                        value,
                        &vanillite_protocol::item::InstalledRegistries,
                    ) {
                        Ok(wire) => stack.set_component(*id, wire),
                        Err(e) => log::debug!("set_components: component {id}: {e}"),
                    }
                }
                // `validateStrict` (without the container and bundle contents checks).
                let max_stack = crate::item::max_stack_size(stack);
                if stack.count > max_stack || (crate::item::max_damage(stack) > 0 && max_stack > 1)
                {
                    log::debug!("set_components: invalid stack, patch not applied");
                    stack.patch = old;
                }
            }
            FuncKind::Filtered {
                filter,
                on_pass,
                on_fail,
            } => {
                let pass = {
                    let level = |name: &str| self.stack_enchantment_level(stack, name);
                    filter
                        .as_ref()
                        .is_some_and(|p| self.item_matches(p, stack, &level))
                };
                let next = if pass { on_pass } else { on_fail };
                if let Some(f) = next {
                    self.apply(f, stack);
                }
            }
            FuncKind::ExplorationMap(request) => {
                // `ExplorationMapFunction.run`: any stack (the explorer map items of 26.3) gets
                // new map data to the structure (`applyNewSavedData`: its type is kept).
                if stack.is_empty() {
                    return;
                }
                if let (Some(origin), Some(explore)) = (self.ctx.origin, self.ctx.exploration_map)
                    && let Some(map) = explore(request, origin)
                {
                    if stack.item == items::MAP.0 {
                        *stack = map;
                    } else {
                        for id in [
                            vanillite_protocol::components::ids::MAP_ID,
                            vanillite_protocol::components::ids::MAP_DECORATIONS,
                        ] {
                            if let Some(value) = map.component(id) {
                                stack.set_component(id, value.into());
                            }
                        }
                    }
                }
            }
            FuncKind::Noop => {}
        }
    }

    /// `EnchantRandomlyFunction.run`.
    fn enchant_randomly(
        &mut self,
        stack: &mut ItemStack,
        options: Option<&[Box<str>]>,
        only_compatible: bool,
    ) -> Option<i32> {
        let e = self.ctx.enchanting?;
        let is_book = stack.item == items::BOOK.0;
        let check = !is_book && only_compatible;
        let candidates: Vec<u32> = self
            .enchantment_options(options)
            .into_iter()
            .filter(|&id| !check || e.can_enchant(id, stack))
            .collect();
        if candidates.is_empty() {
            log::warn!("Couldn't find a compatible enchantment for {stack:?}");
            return None;
        }
        let len = i32::try_from(candidates.len()).unwrap_or(i32::MAX);
        let pick = usize::try_from(self.rng.next_int(len)).unwrap_or(0);
        let &id = candidates.get(pick)?;
        let max = e.get(id).map_or(1, |info| info.max_level);
        let level = next_int_between(&mut *self.rng, 1, max);
        if is_book {
            *stack = ItemStack::new(items::ENCHANTED_BOOK.0, 1);
        }
        upgrade_enchantment(stack, id, level);
        Some(level)
    }

    /// `ApplyBonusCount.Formula.calculateNewCount`.
    fn bonus(&mut self, formula: &Formula, count: i32, level: i32) -> i32 {
        match formula {
            Formula::OreDrops => {
                if level > 0 {
                    let bound = level.saturating_add(2);
                    let bonus = (self.rng.next_int(bound) - 1).max(0);
                    count.wrapping_mul(bonus + 1)
                } else {
                    count
                }
            }
            Formula::UniformBonusCount { multiplier } => {
                let bound = multiplier.wrapping_mul(level).wrapping_add(1);
                if bound > 0 {
                    count.wrapping_add(self.rng.next_int(bound))
                } else {
                    count
                }
            }
            Formula::BinomialWithBonusCount { extra, probability } => {
                let mut count = count;
                for _ in 0..level.saturating_add(*extra) {
                    if self.rng.next_float() < *probability {
                        count = count.wrapping_add(1);
                    }
                }
                count
            }
        }
    }

    fn int(&mut self, p: &IntProvider) -> i32 {
        match p {
            IntProvider::Const(v) => *v,
            IntProvider::Uniform(range) => {
                let min = self.int(&range.0);
                let max = self.int(&range.1);
                // `Mth.nextInt`.
                if min >= max {
                    return min;
                }
                match max.checked_sub(min).and_then(|d| d.checked_add(1)) {
                    Some(bound) => self.rng.next_int(bound).wrapping_add(min),
                    None => min,
                }
            }
            IntProvider::Binomial(np) => {
                let n = self.int(&np.0);
                let p = self.float(&np.1);
                let mut result = 0;
                for _ in 0..n {
                    if self.rng.next_float() < p {
                        result += 1;
                    }
                }
                result
            }
            IntProvider::Sum(inputs) => inputs
                .iter()
                .fold(0i32, |sum, p| sum.wrapping_add(self.int(p))),
        }
    }

    fn float(&mut self, p: &FloatProvider) -> f32 {
        match p {
            FloatProvider::Const(v) => *v,
            FloatProvider::Uniform(range) => {
                let min = self.float(&range.0);
                let max = self.float(&range.1);
                // `Mth.nextFloat`.
                if min >= max {
                    min
                } else {
                    self.rng.next_float() * (max - min) + min
                }
            }
        }
    }
}

/// `UniformContainerBase.EntryBase.getWeight`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::as_conversions
)]
fn entry_weight(entry: &Entry, luck: f32) -> i32 {
    ((entry.weight as f32 + entry.quality as f32 * luck).floor() as i32).max(0)
}

/// `EnchantmentPredicate.containedIn` without `enchantments`: some enchantment of the list has
/// a level in `levels`.
fn any_enchantment(list: &[(u32, i32)], levels: Bounds) -> bool {
    list.iter().any(|(_, level)| levels.matches(*level))
}

/// `EnchantmentHelper.getComponentType`: enchanted books store their enchantments.
fn enchantment_component(stack: &ItemStack) -> u16 {
    if stack.item == items::ENCHANTED_BOOK.0 {
        ids::STORED_ENCHANTMENTS
    } else {
        ids::ENCHANTMENTS
    }
}

/// `ItemEnchantments.Mutable.set`: level 0 removes the enchantment.
fn set_enchantment(list: &mut Vec<(u32, i32)>, id: u32, level: i32) {
    match list.iter().position(|(e, _)| *e == id) {
        Some(i) if level <= 0 => {
            list.remove(i);
        }
        Some(i) => {
            if let Some(entry) = list.get_mut(i) {
                entry.1 = level;
            }
        }
        None if level > 0 => list.push((id, level)),
        None => {}
    }
}

/// `ItemStack.enchant(enchantment, level)`: `ItemEnchantments.Mutable.upgrade`.
fn upgrade_enchantment(stack: &mut ItemStack, id: u32, level: i32) {
    let component = enchantment_component(stack);
    let mut list = crate::enchant::read(stack.component(component));
    let current = list.iter().find(|(e, _)| *e == id).map_or(0, |e| e.1);
    if level > current {
        set_enchantment(&mut list, id, level.min(255));
    }
    stack.set_component(component, crate::enchant::write(&list));
}

/// A `suspicious_stew_effects` value: `(mob effect id, duration)`.
fn read_stew_effects(data: Option<&[u8]>) -> Vec<(u32, i32)> {
    let Some(data) = data else {
        return Vec::new();
    };
    let mut r = Reader::new(data);
    let Ok(n) = r.len_prefix("stew effects", 1024, 2) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let (Ok(effect), Ok(duration)) = (r.varint(), r.varint()) else {
            break;
        };
        if let Ok(effect) = u32::try_from(effect) {
            out.push((effect, duration));
        }
    }
    out
}

/// `BlockPredicate.matchesState`.
fn block_matches(
    p: &BlockPredicate,
    state: BlockState,
    tags: Option<&HashMap<String, Vec<u16>>>,
) -> bool {
    let block = state.block();
    if let Some(blocks) = &p.blocks
        && !blocks.contains(block, tags)
    {
        return false;
    }
    p.state.iter().all(|(name, matcher)| {
        let (Some(values), Some(actual)) = (property_values(block, name), state.value(name)) else {
            return false;
        };
        let Some(actual) = property_key(values, actual) else {
            return false;
        };
        match matcher {
            ValueMatcher::Exact(expected) => property_key(values, expected) == Some(actual),
            ValueMatcher::Range(min, max) => {
                let min_ok = min
                    .as_ref()
                    .is_none_or(|m| property_key(values, m).is_some_and(|m| actual >= m));
                let max_ok = max
                    .as_ref()
                    .is_none_or(|m| property_key(values, m).is_some_and(|m| actual <= m));
                min_ok && max_ok
            }
        }
    })
}

/// `CopyBlockState.run`: adds `properties` of `state` to the stack's `minecraft:block_state`.
fn copy_state(stack: &mut ItemStack, state: BlockState, properties: &[Box<str>]) {
    let mut pairs = stack
        .component(ids::BLOCK_STATE)
        .and_then(decode_block_state)
        .unwrap_or_default();
    for name in properties {
        if let Some(value) = state.value(name) {
            match pairs.iter_mut().find(|(k, _)| k == name.as_ref()) {
                Some(pair) => pair.1 = value.to_owned(),
                None => pairs.push((name.to_string(), value.to_owned())),
            }
        }
    }
    let mut buf = Vec::new();
    write_len(&mut buf, pairs.len());
    for (k, v) in &pairs {
        write_str(&mut buf, k);
        write_str(&mut buf, v);
    }
    stack.set_component(ids::BLOCK_STATE, buf.into_boxed_slice());
}

/// Reads a `minecraft:block_state` component value (VarInt count, then key/value strings).
fn decode_block_state(data: &[u8]) -> Option<Vec<(String, String)>> {
    let mut r = Reader::new(data);
    let count = r.len_prefix("block state properties", 1024, 2).ok()?;
    let mut pairs = Vec::with_capacity(count);
    for _ in 0..count {
        let k = r.string(MAX_STATE_STRING).ok()?;
        let v = r.string(MAX_STATE_STRING).ok()?;
        pairs.push((k.to_owned(), v.to_owned()));
    }
    Some(pairs)
}

/// `DataComponents.ADDITIONAL_TRADE_COST` (a var int) on a stack.
fn set_additional_trade_cost(stack: &mut ItemStack, cost: i32) {
    let Some(id) =
        vanillite_data::registry::DATA_COMPONENT_TYPE.id("minecraft:additional_trade_cost")
    else {
        return;
    };
    let mut out = Vec::new();
    vanillite_protocol::codec::write_varint(&mut out, cost);
    stack.set_component(id, out.into_boxed_slice());
}

// --- Standalone loot parts (villager trades and other data that embeds them) ------------------

/// A parser for loot parts outside loot tables: named predicates and item modifiers are not
/// available.
fn standalone_parser<'n>(names: &'n HashMap<String, u32>) -> Parser<'n> {
    Parser {
        predicates: names,
        modifiers: names,
        unsupported: Vec::new(),
    }
}

/// The empty name map of [`standalone_parser`].
fn no_names() -> &'static HashMap<String, u32> {
    static EMPTY: std::sync::OnceLock<HashMap<String, u32>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(HashMap::new)
}

/// Logs the unsupported parts a standalone parse met.
fn note_unsupported(parser: &mut Parser<'_>, what: &str) {
    for feature in parser.unsupported.drain(..) {
        log::warn!("{what}: unsupported {feature}");
    }
}

/// An item modifier (`LootItemFunction`, a list is a sequence) parsed on its own.
#[derive(Debug)]
pub struct LootFunction(Func);

/// A loot condition (`LootItemCondition`) parsed on its own.
#[derive(Debug)]
pub struct LootCondition(Cond);

/// A number provider of ints (`ContextIntProvider`).
#[derive(Debug)]
pub struct LootInt(IntProvider);

/// A number provider of floats (`ContextFloatProvider`).
#[derive(Debug)]
pub struct LootFloat(FloatProvider);

/// How a standalone loot part is evaluated: the context, its random and whether
/// `ADDITIONAL_COST_COMPONENT_ALLOWED` is set.
pub struct LootEval<'a> {
    pub tables: &'a LootTables,
    pub ctx: &'a LootContext<'a>,
    pub rng: &'a mut dyn LootRandom,
    pub additional_cost_allowed: bool,
}

impl fmt::Debug for LootEval<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LootEval")
            .field("additional_cost_allowed", &self.additional_cost_allowed)
            .finish_non_exhaustive()
    }
}

impl<'a> LootEval<'a> {
    fn eval<'s>(&'s mut self, out: &'s mut Vec<ItemStack>) -> Eval<'a, 's> {
        Eval {
            tables: self.tables,
            ctx: self.ctx,
            rng: &mut *self.rng,
            out,
            post: Vec::new(),
            depth: 0,
            allow_additional_cost: self.additional_cost_allowed,
            keep_empty: false,
        }
    }
}

impl LootFunction {
    /// Parses a modifier object, a list of them or (failing) a reference.
    pub fn parse(v: &Value, what: &str) -> Result<LootFunction, String> {
        let mut parser = standalone_parser(no_names());
        let f = parser.func(v);
        note_unsupported(&mut parser, what);
        f.map(LootFunction)
    }

    /// `apply(stack, context)`.
    pub fn apply(&self, eval: &mut LootEval<'_>, stack: &mut ItemStack) {
        let mut out = Vec::new();
        let mut e = eval.eval(&mut out);
        e.apply(&self.0, stack);
    }
}

impl LootCondition {
    /// Parses a condition object.
    pub fn parse(v: &Value, what: &str) -> Result<LootCondition, String> {
        let mut parser = standalone_parser(no_names());
        let c = parser.cond(v);
        note_unsupported(&mut parser, what);
        c.map(LootCondition)
    }

    /// `test(context)`.
    pub fn test(&self, eval: &mut LootEval<'_>) -> bool {
        let mut out = Vec::new();
        eval.eval(&mut out).test(&self.0)
    }
}

impl LootInt {
    /// A constant.
    pub fn constant(v: i32) -> LootInt {
        LootInt(IntProvider::Const(v))
    }

    /// Parses a number or a provider object.
    pub fn parse(v: &Value) -> Result<LootInt, String> {
        standalone_parser(no_names()).int_provider(v).map(LootInt)
    }

    /// `getInt(context)`.
    pub fn sample(&self, eval: &mut LootEval<'_>) -> i32 {
        let mut out = Vec::new();
        eval.eval(&mut out).int(&self.0)
    }
}

impl LootFloat {
    /// A constant.
    pub fn constant(v: f32) -> LootFloat {
        LootFloat(FloatProvider::Const(v))
    }

    /// Parses a number or a provider object.
    pub fn parse(v: &Value) -> Result<LootFloat, String> {
        standalone_parser(no_names())
            .float_provider(v)
            .map(LootFloat)
    }

    /// `getFloat(context)`.
    pub fn sample(&self, eval: &mut LootEval<'_>) -> f32 {
        let mut out = Vec::new();
        eval.eval(&mut out).float(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vanillite_base::random::XoroshiroRandom;
    use vanillite_data::item::ids as items;

    fn state(name: &str, props: &[(&str, &str)]) -> BlockState {
        let mut s = BlockId::by_name(name).expect("block").default_state();
        for (k, v) in props {
            s = s.with(k, v).expect("property");
        }
        s
    }

    /// Drops of `state` with `tool` and the given enchantment levels; counts summed per item.
    fn drops(
        tables: &LootTables,
        state: BlockState,
        tool: &ItemStack,
        enchantments: &[(&str, i32)],
        explosion_radius: Option<f32>,
        seed: i64,
    ) -> Vec<ItemStack> {
        let level = |id: &str| {
            enchantments
                .iter()
                .find(|(e, _)| *e == id)
                .map_or(0, |(_, l)| *l)
        };
        let in_tag = |_: u16, _: &str| false;
        let ctx = BlockLootContext {
            state,
            tool,
            enchantment_level: &level,
            item_in_tag: &in_tag,
            explosion_radius,
            this_present: true,
            entity: None,
            block_entity: None,
        };
        let mut rng = XoroshiroRandom::new(seed);
        let mut out = Vec::new();
        tables.block_drops(&ctx, &mut rng, &mut out);
        out
    }

    /// Drops of an entity table with the killer's Looting level and the given facts.
    fn entity_drops(
        tables: &LootTables,
        table: &str,
        on_fire: bool,
        by_player: bool,
        looting: i32,
        seed: i64,
    ) -> Vec<ItemStack> {
        struct NoTags;
        impl crate::predicate::EntityTags for NoTags {
            fn entity_in_tag(&self, _: vanillite_data::entity::EntityTypeId, _: &str) -> bool {
                false
            }
        }
        let types = crate::survival::damage::DamageTypes::built_in();
        let level = |id: &str| {
            if id == "minecraft:looting" {
                looting
            } else {
                0
            }
        };
        let player = crate::predicate::EntityFacts {
            kind: Some(vanillite_data::entity::ids::PLAYER),
            ..crate::predicate::EntityFacts::default()
        };
        let entity = EntityLootContext {
            this: crate::predicate::EntityFacts {
                kind: Some(vanillite_data::entity::ids::COW),
                on_fire,
                ..crate::predicate::EntityFacts::default()
            },
            attacker: by_player.then_some(player),
            direct_attacker: by_player.then_some(player),
            last_damage_player: by_player.then_some(player),
            source: None,
            types: &types,
            tags: &NoTags,
            attacker_enchantment_level: &level,
        };
        let none = |_: &str| 0;
        let in_tag = |_: u16, _: &str| false;
        let empty = ItemStack::default();
        let ctx = BlockLootContext {
            state: BlockState::AIR,
            tool: &empty,
            enchantment_level: &none,
            item_in_tag: &in_tag,
            explosion_radius: None,
            this_present: true,
            entity: Some(&entity),
            block_entity: None,
        };
        let mut rng = XoroshiroRandom::new(seed);
        let mut out = Vec::new();
        tables.table_drops(table, &ctx, &mut rng, &mut out);
        out
    }

    #[test]
    fn entity_loot_conditions_and_looting() {
        let json: Value = serde_json::from_str(
            r#"{"type": "minecraft:entity", "pools": [
                {"rolls": 1, "entries": [{"type": "minecraft:item", "name": "minecraft:beef",
                    "modifier": [
                        {"type": "minecraft:set_count", "count": 1},
                        {"type": "minecraft:furnace_smelt", "condition":
                            {"type": "minecraft:entity_properties", "entity": "this",
                             "predicate": {"minecraft:flags": {"is_on_fire": true}}}},
                        {"type": "minecraft:enchanted_count_increase",
                         "enchantment": "minecraft:looting", "count": 1.0}]}]},
                {"rolls": 1, "condition": {"type": "minecraft:killed_by_player"},
                 "entries": [{"type": "minecraft:item", "name": "minecraft:leather"}]}]}"#,
        )
        .expect("json");
        let tables = LootTables::from_files(
            vec![("minecraft:entities/cow".to_owned(), Ok(json))],
            Vec::new(),
            Vec::new(),
        );
        let t = "minecraft:entities/cow";
        let raw = entity_drops(&tables, t, false, false, 0, 1);
        assert_eq!(summary(&raw), vec![("minecraft:beef", 1)]);
        let cooked = entity_drops(&tables, t, true, true, 0, 1);
        assert_eq!(
            summary(&cooked),
            vec![("minecraft:cooked_beef", 1), ("minecraft:leather", 1)]
        );
        let looted = entity_drops(&tables, t, false, true, 3, 1);
        assert_eq!(summary(&looted)[0], ("minecraft:beef", 4));
    }

    fn summary(stacks: &[ItemStack]) -> Vec<(&'static str, i32)> {
        stacks
            .iter()
            .map(|s| (ItemId(s.item).name(), s.count))
            .collect()
    }

    /// A scratch pack directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "vanillite-loot-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Scratch(dir)
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, content).expect("write");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Extracts the loot data of the Vanilla server jar (found as
    /// `target/vanilla/<version>/versions/<version>/server-<version>.jar` in this checkout or a
    /// parent of it) into `dir`; false if the jar is absent.
    fn extract_vanilla(dir: &Path) -> bool {
        let v = vanillite_data::version::VERSION_ID;
        let rel = format!("target/vanilla/{v}/versions/{v}/server-{v}.jar");
        let Some(jar) = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .map(|a| a.join(&rel))
            .find(|p| p.is_file())
        else {
            return false;
        };
        let file = std::fs::File::open(&jar).expect("open jar");
        let mut zip = zip::ZipArchive::new(file).expect("read jar");
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).expect("jar entry");
            let Ok(name) = entry.name().map(|n| n.to_string()) else {
                continue;
            };
            let wanted = [
                "data/minecraft/loot_table/blocks/",
                "data/minecraft/loot_table/chests/",
                "data/minecraft/loot_table/archaeology/",
                "data/minecraft/loot_table/pots/",
                "data/minecraft/predicate/",
                "data/minecraft/item_modifier/",
            ];
            if entry.is_dir() || !wanted.iter().any(|w| name.starts_with(w)) {
                continue;
            }
            let Some(rel) = entry.enclosed_name() else {
                continue;
            };
            let to = dir.join(rel);
            std::fs::create_dir_all(to.parent().expect("parent")).expect("dirs");
            let mut out = std::fs::File::create(&to).expect("create");
            std::io::copy(&mut entry, &mut out).expect("extract");
        }
        true
    }

    #[test]
    fn loot_vanilla_block_tables() {
        let scratch = Scratch::new("vanilla");
        if !extract_vanilla(&scratch.0) {
            eprintln!("Vanilla server jar not found; skipped");
            return;
        }
        let tables = LootTables::load(std::slice::from_ref(&scratch.0));
        assert!(tables.broken.is_empty(), "broken: {:#?}", tables.broken);
        assert!(tables.len() > 1000, "only {} tables", tables.len());
        // The only partly supported features of the Vanilla block tables.
        assert!(
            tables.unsupported.is_empty(),
            "unsupported: {:#?}",
            tables.unsupported
        );

        let empty = ItemStack::EMPTY;
        let wooden_pickaxe = ItemStack::new(items::WOODEN_PICKAXE.0, 1);
        let diamond_pickaxe = ItemStack::new(items::DIAMOND_PICKAXE.0, 1);
        let stone = state("minecraft:stone", &[]);

        assert_eq!(
            summary(&drops(&tables, stone, &wooden_pickaxe, &[], None, 1)),
            [("minecraft:cobblestone", 1)]
        );
        assert_eq!(
            summary(&drops(&tables, stone, &empty, &[], None, 1)),
            [("minecraft:cobblestone", 1)]
        );
        assert_eq!(
            summary(&drops(
                &tables,
                stone,
                &diamond_pickaxe,
                &[("minecraft:silk_touch", 1)],
                None,
                1
            )),
            [("minecraft:stone", 1)]
        );
        let diamond_ore = state("minecraft:diamond_ore", &[]);
        assert_eq!(
            summary(&drops(&tables, diamond_ore, &wooden_pickaxe, &[], None, 1)),
            [("minecraft:diamond", 1)]
        );
        // Fortune III: 1 to 4 diamonds.
        for seed in 0..50 {
            let d = drops(
                &tables,
                diamond_ore,
                &diamond_pickaxe,
                &[("minecraft:fortune", 3)],
                None,
                seed,
            );
            assert_eq!(d.len(), 1);
            assert!((1..=4).contains(&d[0].count), "{d:?}");
        }
        assert_eq!(
            summary(&drops(
                &tables,
                state("minecraft:grass_block", &[]),
                &empty,
                &[],
                None,
                1
            )),
            [("minecraft:dirt", 1)]
        );
        assert_eq!(
            summary(&drops(
                &tables,
                state("minecraft:oak_slab", &[("type", "double")]),
                &empty,
                &[],
                None,
                1
            )),
            [("minecraft:oak_slab", 2)]
        );
        assert_eq!(
            summary(&drops(
                &tables,
                state("minecraft:oak_slab", &[("type", "bottom")]),
                &empty,
                &[],
                None,
                1
            )),
            [("minecraft:oak_slab", 1)]
        );
        // Snow layers by layer count (match_block plus entity_properties).
        assert_eq!(
            summary(&drops(
                &tables,
                state("minecraft:snow", &[("layers", "5")]),
                &ItemStack::new(items::WOODEN_SHOVEL.0, 1),
                &[],
                None,
                1
            )),
            [("minecraft:snowball", 5)]
        );

        // copy_state: a silk-touched beehive keeps its honey level.
        let beehive = state("minecraft:beehive", &[("honey_level", "3")]);
        let d = drops(
            &tables,
            beehive,
            &diamond_pickaxe,
            &[("minecraft:silk_touch", 1)],
            None,
            1,
        );
        assert_eq!(summary(&d), [("minecraft:beehive", 1)]);
        let mut expected = Vec::new();
        write_len(&mut expected, 1);
        write_str(&mut expected, "honey_level");
        write_str(&mut expected, "3");
        assert_eq!(d[0].component(ids::BLOCK_STATE), Some(expected.as_slice()));
        let d = drops(&tables, beehive, &empty, &[], None, 1);
        assert_eq!(summary(&d), [("minecraft:beehive", 1)]);
        assert_eq!(d[0].component(ids::BLOCK_STATE), None);

        // Double plants drop from one half only (match_block plus location_check).
        let shears = ItemStack::new(items::SHEARS.0, 1);
        for half in ["lower", "upper"] {
            assert_eq!(
                summary(&drops(
                    &tables,
                    state("minecraft:tall_grass", &[("half", half)]),
                    &shears,
                    &[],
                    None,
                    1
                )),
                [("minecraft:short_grass", 2)]
            );
        }

        // Every state evaluates, by hand and in explosions.
        let mut with_drops = 0;
        for id in 0..vanillite_data::block::STATE_COUNT {
            let s = BlockState(u16::try_from(id).expect("state id"));
            if !drops(&tables, s, &wooden_pickaxe, &[], None, 7).is_empty() {
                with_drops += 1;
            }
            drops(&tables, s, &empty, &[], Some(4.0), 7);
        }
        assert!(with_drops > 10_000, "{with_drops}");

        // Explosions: survives_explosion with radius 1 always keeps the drop.
        assert_eq!(
            summary(&drops(&tables, stone, &empty, &[], Some(1.0), 1)),
            [("minecraft:cobblestone", 1)]
        );
        // With a huge radius almost nothing survives.
        let survived: usize = (0..100)
            .map(|seed| drops(&tables, stone, &empty, &[], Some(1.0e6), seed).len())
            .sum();
        assert!(survived < 5, "{survived}");

        // Blocks without a table drop nothing.
        assert!(drops(&tables, BlockState::AIR, &empty, &[], None, 1).is_empty());
    }

    /// Fills a container of `size` slots from `table` with the Vanilla registries.
    fn fill_container(tables: &LootTables, table: &str, size: usize, seed: i64) -> Vec<ItemStack> {
        let data = crate::menu::tests::install_data();
        let in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
        let holders = |registry: &str, entry: &str| data.holder_set(registry, entry);
        let ctx = LootContext {
            origin: Some(Vec3::new(0.5, 64.5, 0.5)),
            item_in_tag: &in_tag,
            enchanting: Some(&data.enchanting),
            holder_set: &holders,
            ..LootContext::empty()
        };
        let mut items = vec![ItemStack::EMPTY; size];
        let mut rng = vanillite_base::random::LegacyRandom::new(seed);
        tables.fill(table, &ctx, &mut rng, &mut items);
        items
    }

    #[test]
    fn loot_vanilla_container_tables() {
        let scratch = Scratch::new("containers");
        if !extract_vanilla(&scratch.0) {
            eprintln!("Vanilla server jar not found; skipped");
            return;
        }
        let tables = LootTables::load(std::slice::from_ref(&scratch.0));
        assert!(tables.broken.is_empty(), "broken: {:#?}", tables.broken);
        assert!(tables.unsupported.is_empty(), "{:#?}", tables.unsupported);
        let containers: Vec<String> = tables
            .by_name
            .keys()
            .filter(|k| !k.starts_with("minecraft:blocks/"))
            .map(|k| k.to_string())
            .collect();
        assert!(containers.len() > 60, "only {} tables", containers.len());
        let mut stored = 0;
        let mut named = 0;
        for table in &containers {
            for seed in 1..20 {
                for stack in fill_container(&tables, table, 27, seed) {
                    if stack.component(ids::STORED_ENCHANTMENTS).is_some() {
                        stored += 1;
                    }
                    if stack.component(ids::ITEM_NAME).is_some() {
                        named += 1;
                    }
                }
            }
        }
        // Enchanted books from `enchant_randomly` / `enchant_with_levels`.
        assert!(stored > 0);
        // Explorer maps without a structure search are discarded by their `filtered` check.
        assert_eq!(named, 0);

        // The same seed fills the same slots.
        let a = fill_container(&tables, "minecraft:chests/simple_dungeon", 27, 42);
        let b = fill_container(&tables, "minecraft:chests/simple_dungeon", 27, 42);
        assert_eq!(a, b);
        let filled = a.iter().filter(|s| !s.is_empty()).count();
        assert!((2..=27).contains(&filled), "{filled}");
        // A pot holds the one stack of its table.
        let pot = fill_container(&tables, "minecraft:pots/trial_chambers/corridor", 1, 7);
        assert!(!pot.iter().all(ItemStack::is_empty));
    }

    #[test]
    fn loot_fill_splits_stacks_over_free_slots() {
        let pack = Scratch::new("fill");
        pack.write(
            "data/minecraft/loot_table/chests/test.json",
            r#"{"pools": [{"rolls": 1, "entries": [
                {"type": "minecraft:item", "name": "minecraft:cobblestone",
                 "modifier": {"type": "minecraft:set_count", "count": 64}}]}]}"#,
        );
        let tables = LootTables::load(std::slice::from_ref(&pack.0));
        let ctx = LootContext::empty();
        let mut items = vec![ItemStack::EMPTY; 9];
        items[4] = ItemStack::new(items::STONE.0, 1);
        let mut rng = XoroshiroRandom::new(3);
        tables.fill("minecraft:chests/test", &ctx, &mut rng, &mut items);
        // The occupied slot stays; the 64 items spread over several free slots.
        assert_eq!(items[4], ItemStack::new(items::STONE.0, 1));
        let cobble: Vec<i32> = items
            .iter()
            .filter(|s| s.item == items::COBBLESTONE.0)
            .map(|s| s.count)
            .collect();
        assert_eq!(cobble.iter().sum::<i32>(), 64);
        assert!(cobble.len() > 1, "{cobble:?}");
    }

    #[test]
    fn loot_empty_set_drops_nothing() {
        let tables = LootTables::empty();
        assert!(tables.is_empty());
        let d = drops(
            &tables,
            state("minecraft:stone", &[]),
            &ItemStack::EMPTY,
            &[],
            None,
            1,
        );
        assert!(d.is_empty());
    }

    #[test]
    fn loot_custom_tables() {
        let pack = Scratch::new("custom");
        // Larger than a stack: split like Vanilla.
        pack.write(
            "data/minecraft/loot_table/blocks/stone.json",
            r#"{"type": "minecraft:block", "pools": [{"rolls": 1, "entries": [
                {"type": "minecraft:item", "name": "minecraft:cobblestone",
                 "modifier": {"type": "minecraft:set_count", "count": 100}}]}]}"#,
        );
        // Weighted choice with a named predicate and an item modifier reference.
        pack.write(
            "data/minecraft/loot_table/blocks/dirt.json",
            r#"{"pools": [{"rolls": {"type": "minecraft:uniform", "min": 2, "max": 2},
                "condition": "test:always",
                "entries": [
                  {"type": "minecraft:item", "name": "minecraft:diamond", "weight": 3,
                   "modifier": "test:three"},
                  {"type": "minecraft:item", "name": "minecraft:emerald", "weight": 0},
                  {"type": "minecraft:empty", "weight": 1}]}]}"#,
        );
        pack.write(
            "data/test/predicate/always.json",
            r#"{"type": "minecraft:inverted", "term": {"type": "minecraft:any_of", "terms": []}}"#,
        );
        pack.write(
            "data/test/item_modifier/three.json",
            r#"[{"type": "minecraft:set_count", "count": 2},
                {"type": "minecraft:set_count", "count": 1, "add": true},
                {"type": "minecraft:limit_count", "limit": {"max": 3}}]"#,
        );
        // A nested table reference and a broken table.
        pack.write(
            "data/minecraft/loot_table/blocks/sand.json",
            r#"{"pools": [{"rolls": 1, "entries": [
                {"type": "minecraft:loot_table", "value": "minecraft:blocks/stone"}]}]}"#,
        );
        pack.write(
            "data/minecraft/loot_table/blocks/gravel.json",
            r#"{"pools": [{"rolls": 1, "entries": [{"type": "minecraft:item"}]}]}"#,
        );
        let tables = LootTables::load(std::slice::from_ref(&pack.0));
        assert_eq!(tables.len(), 3);
        assert_eq!(tables.broken.len(), 1, "{:?}", tables.broken);

        let empty = ItemStack::EMPTY;
        let stone = state("minecraft:stone", &[]);
        assert_eq!(
            summary(&drops(&tables, stone, &empty, &[], None, 1)),
            [("minecraft:cobblestone", 64), ("minecraft:cobblestone", 36)]
        );
        assert_eq!(
            summary(&drops(
                &tables,
                state("minecraft:sand", &[]),
                &empty,
                &[],
                None,
                1
            )),
            [("minecraft:cobblestone", 64), ("minecraft:cobblestone", 36)]
        );
        assert!(
            drops(
                &tables,
                state("minecraft:gravel", &[]),
                &empty,
                &[],
                None,
                1
            )
            .is_empty()
        );

        let mut diamonds = 0;
        for seed in 0..200 {
            for s in drops(
                &tables,
                state("minecraft:dirt", &[]),
                &empty,
                &[],
                None,
                seed,
            ) {
                assert_eq!(ItemId(s.item).name(), "minecraft:diamond");
                assert_eq!(s.count, 3);
                diamonds += 1;
            }
        }
        // Two rolls of 3:1 odds: about 300 of 400.
        assert!((250..350).contains(&diamonds), "{diamonds}");

        // A later root overrides an earlier one.
        let other = Scratch::new("override");
        other.write(
            "data/minecraft/loot_table/blocks/stone.json",
            r#"{"pools": [{"rolls": 1, "entries": [
                {"type": "minecraft:item", "name": "minecraft:stone"}]}]}"#,
        );
        let tables = LootTables::load(&[pack.0.clone(), other.0.clone()]);
        assert_eq!(
            summary(&drops(&tables, stone, &empty, &[], None, 1)),
            [("minecraft:stone", 1)]
        );
    }

    #[test]
    fn named_and_inline_predicates_modifiers_and_tables() {
        let parse = |t: &str| serde_json::from_str::<Value>(t).expect("json");
        let tables = LootTables::from_files(
            Vec::new(),
            vec![(
                "test:never".to_owned(),
                Ok(parse(
                    r#"{"type": "minecraft:random_chance", "chance": 0.0}"#,
                )),
            )],
            vec![(
                "test:five".to_owned(),
                Ok(parse(r#"{"type": "minecraft:set_count", "count": 5}"#)),
            )],
        );
        let ctx = LootContext::empty();
        let mut rng = XoroshiroRandom::new(1);
        assert!(tables.has_predicate("test:never"));
        assert_eq!(
            tables.test_predicate(Some("test:never"), None, &ctx, &mut rng),
            Ok(false)
        );
        // An inline list is `all_of`; a named reference resolves inside it.
        let inline = parse(
            r#"[{"type": "minecraft:random_chance", "chance": 1.0},
                {"type": "minecraft:inverted", "term": "test:never"}]"#,
        );
        assert_eq!(
            tables.test_predicate(None, Some(&inline), &ctx, &mut rng),
            Ok(true)
        );
        assert!(
            tables
                .test_predicate(Some("test:unknown"), None, &ctx, &mut rng)
                .is_err()
        );
        let mut stack = ItemStack::new(items::STONE.0, 1);
        tables
            .modify(Some("test:five"), None, &mut stack, &ctx, &mut rng)
            .expect("modifier");
        assert_eq!(stack.count, 5);
        let inline = parse(r#"[{"type": "minecraft:set_count", "count": 2}]"#);
        tables
            .modify(None, Some(&inline), &mut stack, &ctx, &mut rng)
            .expect("modifier");
        assert_eq!(stack.count, 2);
        let table = parse(
            r#"{"pools": [{"rolls": 1, "entries": [
                {"type": "minecraft:item", "name": "minecraft:stone",
                 "modifier": "test:five"}]}]}"#,
        );
        let mut out = Vec::new();
        tables
            .roll_inline(&table, &ctx, &mut rng, &mut out)
            .expect("table");
        assert_eq!(summary(&out), vec![("minecraft:stone", 5)]);
    }

    /// The filter of the enchanted villager trades: `[{}]` is "any enchantment".
    #[test]
    fn filtered_any_enchantment() {
        let parse = |t: &str| serde_json::from_str::<Value>(t).expect("json");
        let tables = LootTables::from_files(Vec::new(), Vec::new(), Vec::new());
        let ctx = LootContext::empty();
        let mut rng = XoroshiroRandom::new(1);
        for (key, component) in [
            ("minecraft:enchantments", ids::ENCHANTMENTS),
            ("minecraft:stored_enchantments", ids::STORED_ENCHANTMENTS),
        ] {
            let filter = parse(&format!(
                r#"{{"type": "minecraft:filtered",
                    "item_filter": {{"predicates": {{"{key}": [{{}}]}}}},
                    "on_fail": {{"type": "minecraft:discard"}}}}"#
            ));
            let mut plain = ItemStack::new(items::BOOK.0, 1);
            tables
                .modify(None, Some(&filter), &mut plain, &ctx, &mut rng)
                .expect("modifier");
            assert!(plain.is_empty(), "{key}");
            let mut enchanted = ItemStack::new(items::BOOK.0, 1);
            // One entry: enchantment 0, level 1.
            enchanted.set_component(component, Box::new([1, 0, 1]));
            tables
                .modify(None, Some(&filter), &mut enchanted, &ctx, &mut rng)
                .expect("modifier");
            assert!(!enchanted.is_empty(), "{key}");
        }
    }
}
