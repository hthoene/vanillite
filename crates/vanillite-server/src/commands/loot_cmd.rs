//! `/loot` (Vanilla's `LootCommand`) and the loot context commands share: loot tables,
//! predicates and item modifiers by id or inline.

use vanillite_base::{BlockPos, Vec3};
use vanillite_command::Args;
use vanillite_command::error::{Arg as ErrArg, CommandError};
use vanillite_command::game_values::{GameValue, InlineOrId};
use vanillite_game::commands::{LootSource, LootTarget, LootTool};
use vanillite_game::item::ItemStack;
use vanillite_game::loot::{BlockLootContext, EntityLootContext, LootContext, LootRandom};
use vanillite_protocol::text::{Arg, Component};
use vanillite_world::entity::EntityId;

use super::Env;
use super::exec::resolve_position;
use super::items::{self, ItemInput};
use super::select;
use super::source::CommandSource;
use super::tracker::Tracker;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

/// A `ResourceOrIdArgument` value of `name`: the id or the inline definition as JSON.
pub(crate) fn resource_or_inline(
    args: &Args,
    name: &str,
) -> Result<(Option<String>, Option<serde_json::Value>), CommandError> {
    match args.game(name) {
        Some(GameValue::InlineOrId(InlineOrId::Id(id))) => Ok((Some(id.clone()), None)),
        Some(GameValue::InlineOrId(InlineOrId::Inline(tag))) => {
            Ok((None, Some(crate::world_setup::nbt_to_json(tag))))
        }
        _ => Err(missing(name)),
    }
}

/// What a loot context of a command holds besides the level's data.
#[derive(Clone, Copy, Default)]
pub(crate) struct Params<'a> {
    pub(crate) origin: Option<Vec3>,
    pub(crate) this_entity: Option<EntityId>,
    pub(crate) tool: Option<&'a ItemStack>,
    pub(crate) entity: Option<&'a EntityLootContext<'a>>,
    /// The loot table's `random_sequence`: its random instead of the level's
    /// (`LootContext.Builder.create(randomSequence)`).
    pub(crate) sequence: Option<&'a str>,
}

/// Runs `f` with a loot context of the server's data at the command's level and the level's
/// random (`LootParams` without a seed), or the random of the table's sequence.
pub(crate) fn with_context<R>(
    env: &mut Env<'_>,
    params: Params<'_>,
    f: impl FnOnce(&vanillite_game::player::play::GameData, &LootContext<'_>, &mut dyn LootRandom) -> R,
) -> R {
    let data = env.global.game();
    let at = params.origin.map(|o| BlockPos::containing(o.x, o.y, o.z));
    let level = &mut *env.level;
    let biome: Option<String> = at.and_then(|at| {
        level
            .climate()
            .map(|c| c.biomes.get(level.biome(at)).name.clone())
    });
    let biome_matches = |entry: &str| biome.as_deref() == Some(entry);
    let item_in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let holder_set = |registry: &str, entry: &str| data.holder_set(registry, entry);
    let tool_level = |name: &str| {
        params
            .tool
            .map_or(0, |tool| data.enchantment_level(tool, name))
    };
    let global = env.global;
    // Taken out while the level's random is borrowed for `f`.
    let entities = std::mem::take(&mut level.entities);
    let score = |id: EntityId, objective: &str| score_of(global, &entities, id, objective);
    let ctx = LootContext {
        origin: params.origin,
        this_entity: params.this_entity,
        tool: params.tool,
        enchantment_level: &tool_level,
        item_in_tag: &item_in_tag,
        enchanting: Some(&data.enchanting),
        holder_set: &holder_set,
        biome: Some(&biome_matches),
        entity: params.entity,
        weather: Some((level.is_raining(), level.is_thundering())),
        score: Some(&score),
        ..LootContext::empty()
    };
    let result = match params.sequence {
        Some(key) => level.with_random_sequence(key, |r| f(&data, &ctx, r)),
        None => f(&data, &ctx, level.random()),
    };
    level.entities = entities;
    result
}

/// `ItemCommands.applyModifier`: the item modifier of `modifier` on a copy of `stack` in the
/// command's loot context, limited to the max stack size.
pub(crate) fn apply_modifier(
    env: &mut Env<'_>,
    source: &CommandSource,
    modifier: &(Option<String>, Option<serde_json::Value>),
    stack: &ItemStack,
) -> Result<ItemStack, CommandError> {
    let params = Params {
        origin: Some(source.pos),
        this_entity: own_local(source, env),
        ..Params::default()
    };
    let mut out = stack.clone();
    with_context(env, params, |data, ctx, rng| {
        data.loot.modify(
            modifier.0.as_deref(),
            modifier.1.as_ref(),
            &mut out,
            ctx,
            rng,
        )
    })
    .map_err(|e| CommandError::new("command.exception", vec![ErrArg::Str(e)]))?;
    let max = vanillite_game::item::max_stack_size(&out);
    if out.count > max {
        out.count = max;
    }
    Ok(out)
}

/// `EntityHasScoreCondition`: the score of the entity (a player by name, any other entity
/// by UUID) in `objective`.
fn score_of(
    global: &crate::global::Global,
    entities: &vanillite_game::entities::Entities,
    id: EntityId,
    objective: &str,
) -> Option<i32> {
    let holder = match entities.player(id) {
        Some(p) => p.profile.name.clone(),
        None => vanillite_protocol::types::Uuid(entities.store.get(id)?.uuid()).to_string(),
    };
    global
        .systems
        .scoreboard()
        .score(&holder, objective)
        .map(|s| s.value)
}

/// `LootItemCondition.test` of a predicate argument (`ResourceOrIdArgument.lootPredicate`)
/// with `this` as `THIS_ENTITY` at `origin` (`LootContextParamSets.COMMAND` and `SELECTOR`);
/// `weather` is the level's (raining, thundering); `chunks` are the level's blocks for
/// `location_check` (without them a block predicate holds).
#[allow(clippy::too_many_arguments)]
pub(crate) fn test_predicate(
    entities: &vanillite_game::entities::Entities,
    chunks: Option<&vanillite_world::chunks::Chunks>,
    global: &crate::global::Global,
    this: Option<EntityId>,
    origin: Vec3,
    weather: (bool, bool),
    predicate: &(Option<String>, Option<serde_json::Value>),
    rng: &mut dyn LootRandom,
) -> Result<bool, String> {
    let data = &*global.game();
    let no_level = |_: &str| 0;
    let entity = this
        .and_then(|id| vanillite_game::mob::attack::facts(entities, id))
        .map(|facts| EntityLootContext {
            this: facts,
            attacker: None,
            direct_attacker: None,
            last_damage_player: None,
            source: None,
            types: &global.level.damage_types,
            tags: data,
            attacker_enchantment_level: &no_level,
        });
    let item_in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let holder_set = |registry: &str, entry: &str| data.holder_set(registry, entry);
    let score = |id: EntityId, objective: &str| score_of(global, entities, id, objective);
    // `LocationCheck`: the block at the origin moved by the offset, if loaded.
    let at = BlockPos::containing(origin.x, origin.y, origin.z);
    let neighbour = |dx: i32, dy: i32, dz: i32| {
        chunks
            .and_then(|c| c.block(at.offset(dx, dy, dz)))
            .map(vanillite_data::block::BlockState)
    };
    let neighbour: &dyn Fn(i32, i32, i32) -> Option<vanillite_data::block::BlockState> = &neighbour;
    let ctx = LootContext {
        origin: Some(origin),
        this_entity: this,
        neighbour: chunks.map(|_| neighbour),
        item_in_tag: &item_in_tag,
        enchanting: Some(&data.enchanting),
        holder_set: &holder_set,
        entity: entity.as_ref(),
        weather: Some(weather),
        score: Some(&score),
        ..LootContext::empty()
    };
    data.loot
        .test_predicate(predicate.0.as_deref(), predicate.1.as_ref(), &ctx, rng)
}

/// `getSourceHandItem`.
fn hand_item(
    env: &Env<'_>,
    source: &CommandSource,
    offhand: bool,
) -> Result<ItemStack, CommandError> {
    let own = super::dispatch::self_target(env, source)?;
    if own.kind.info().attributes.is_empty() {
        return Err(CommandError::new(
            "commands.drop.no_held_items",
            vec![ErrArg::Component(Box::new(own.display()))],
        ));
    }
    let player = own.entity.and_then(|id| env.level.entities.player(id));
    Ok(player.map_or(ItemStack::EMPTY, |p| {
        if offhand {
            p.inventory.offhand.clone()
        } else {
            p.inventory.selected_item().clone()
        }
    }))
}

fn tool(
    t: LootTool,
    args: &Args,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<ItemStack, CommandError> {
    match t {
        LootTool::Empty => Ok(ItemStack::EMPTY),
        LootTool::Item => ItemInput::of(args, "tool", env)?.create(1),
        LootTool::MainHand => hand_item(env, source, false),
        LootTool::OffHand => hand_item(env, source, true),
    }
}

/// The source's own entity when it is in this world.
fn own_local(source: &CommandSource, env: &Env<'_>) -> Option<EntityId> {
    source
        .entity
        .as_ref()
        .filter(|e| e.world == env.world)
        .and_then(|e| e.id)
}

/// Rolls a table given by `loot_table` (id or inline).
fn roll_arg(
    args: &Args,
    env: &mut Env<'_>,
    params: Params<'_>,
) -> Result<Vec<ItemStack>, CommandError> {
    let (id, inline) = resource_or_inline(args, "loot_table")?;
    let sequence = id.as_deref().and_then(|id| {
        env.global
            .game()
            .loot
            .random_sequence(id)
            .map(str::to_owned)
    });
    let params = Params {
        sequence: sequence.as_deref(),
        ..params
    };
    let mut out = Vec::new();
    let result = with_context(env, params, |data, ctx, rng| match (&id, &inline) {
        (Some(id), _) => {
            data.loot.roll_keeping_empty(id, ctx, rng, &mut out);
            Ok(())
        }
        (None, Some(json)) => data.loot.roll_inline(json, ctx, rng, &mut out),
        (None, None) => Ok(()),
    });
    result.map_err(|e| CommandError::new("command.exception", vec![ErrArg::Str(e)]))?;
    Ok(out)
}

/// The drops of a source, with the table id for the messages of `mine` and `kill`.
fn drops(
    src: LootSource,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<(Vec<ItemStack>, Option<String>), CommandError> {
    let this = own_local(source, env);
    match src {
        LootSource::Loot => {
            let params = Params {
                origin: Some(source.pos),
                this_entity: this,
                ..Params::default()
            };
            Ok((roll_arg(args, env, params)?, None))
        }
        LootSource::Fish(t) => {
            let pos = block_pos(args, "pos", source)?;
            let tool = tool(t, args, source, env)?;
            let params = Params {
                origin: Some(center(pos)),
                this_entity: this,
                tool: Some(&tool),
                ..Params::default()
            };
            Ok((roll_arg(args, env, params)?, None))
        }
        LootSource::Mine(t) => {
            let pos = block_pos(args, "pos", source)?;
            let tool = tool(t, args, source, env)?;
            let state = env.level.block(pos);
            let name = state.block().info().name;
            let (ns, path) = name.split_once(':').unwrap_or(("minecraft", name));
            let table = format!("{ns}:blocks/{path}");
            let data = env.global.game();
            if state.is_air() || !data.loot.contains(&table) {
                return Err(CommandError::new(
                    "commands.drop.no_loot_table.block",
                    vec![ErrArg::Component(Box::new(Component::translatable(
                        format!("block.{ns}.{path}"),
                        vec![],
                    )))],
                ));
            }
            let entity = env.level.block_entity(pos).cloned();
            let kept = env.level.block_entities.kept_components(pos).cloned();
            let components = |id: u16| {
                entity.as_ref().and_then(|e| {
                    vanillite_game::level::block_entity::collect_component_with(
                        e,
                        kept.as_ref(),
                        id,
                    )
                })
            };
            let item_in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
            let tool_level = |name: &str| data.enchantment_level(&tool, name);
            let ctx = BlockLootContext {
                state,
                tool: &tool,
                enchantment_level: &tool_level,
                item_in_tag: &item_in_tag,
                explosion_radius: None,
                this_present: this.is_some(),
                entity: None,
                block_entity: Some(&components),
            };
            let mut out = Vec::new();
            match data.loot.block_random_sequence(state) {
                Some(key) => env
                    .level
                    .with_random_sequence(key, |r| data.loot.block_drops(&ctx, r, &mut out)),
                None => data.loot.block_drops(&ctx, env.level.random(), &mut out),
            }
            Ok((out, Some(table)))
        }
        LootSource::Kill => {
            let target = select::single(
                env,
                source,
                args.selector("target").ok_or_else(|| missing("target"))?,
            )?;
            let name = target.kind.name();
            let (ns, path) = name.split_once(':').unwrap_or(("minecraft", name));
            let table = format!("{ns}:entities/{path}");
            let data = env.global.game();
            let Some(id) = target.entity.filter(|_| data.loot.contains(&table)) else {
                return Err(CommandError::new(
                    "commands.drop.no_loot_table.entity",
                    vec![ErrArg::Component(Box::new(target.display()))],
                ));
            };
            let ents = &env.level.entities;
            let Some(this_facts) = vanillite_game::mob::attack::facts(ents, id) else {
                return Err(CommandError::new(
                    "commands.drop.no_loot_table.entity",
                    vec![ErrArg::Component(Box::new(target.display()))],
                ));
            };
            let killer = this.and_then(|k| vanillite_game::mob::attack::facts(ents, k));
            let player = this
                .filter(|&k| ents.player(k).is_some())
                .and_then(|k| vanillite_game::mob::attack::facts(ents, k));
            let types = &env.global.level.damage_types;
            let no_level = |_: &str| 0;
            let entity = EntityLootContext {
                this: this_facts,
                attacker: killer,
                direct_attacker: killer,
                last_damage_player: player,
                source: Some(vanillite_game::predicate::SourceFacts {
                    kind: types.magic,
                    is_direct: true,
                    direct: None,
                    cause: None,
                }),
                types,
                tags: &*data,
                attacker_enchantment_level: &no_level,
            };
            let sequence = data.loot.random_sequence(&table).map(str::to_owned);
            let params = Params {
                origin: Some(source.pos),
                this_entity: Some(id),
                entity: Some(&entity),
                sequence: sequence.as_deref(),
                ..Params::default()
            };
            let mut out = Vec::new();
            with_context(env, params, |data, ctx, rng| {
                data.loot.roll_keeping_empty(&table, ctx, rng, &mut out);
            });
            Ok((out, Some(table)))
        }
    }
}

fn block_pos(args: &Args, name: &str, source: &CommandSource) -> Result<BlockPos, CommandError> {
    let c = args.coordinates(name).ok_or_else(|| missing(name))?;
    let p = resolve_position(source, c);
    Ok(BlockPos::containing(p.x, p.y, p.z))
}

fn center(pos: BlockPos) -> Vec3 {
    Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    )
}

/// `BlockItemAccessor.getContainer`: the container block entity at `pos`.
fn container(
    env: &Env<'_>,
    pos: BlockPos,
) -> Result<vanillite_game::block::container::ContainerRef, CommandError> {
    if vanillite_game::block::container::ContainerRef::at(env.level, pos).is_none() {
        return Err(CommandError::new(
            "commands.item.target.not_a_container",
            vec![ErrArg::Int(pos.x), ErrArg::Int(pos.y), ErrArg::Int(pos.z)],
        ));
    }
    Ok(vanillite_game::block::container::ContainerRef::Block(pos))
}

/// `canMergeItems`.
fn can_merge(a: &ItemStack, b: &ItemStack) -> bool {
    a.count <= vanillite_game::item::max_stack_size(a) && a.same_item_same_components(b)
}

/// Puts the drops where the target says; tracks the used drops.
fn output(
    target: LootTarget,
    drops: &[ItemStack],
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
    used: &mut Tracker<ItemStack>,
) -> Result<(), CommandError> {
    match target {
        LootTarget::Give => {
            let players = select::many(
                env,
                source,
                args.selector("players").ok_or_else(|| missing("players"))?,
            )?;
            // `Inventory.add` takes no empty stack.
            for drop in drops.iter().filter(|d| !d.is_empty()) {
                for player in &players {
                    let stack = drop.clone();
                    let added = match player.entity {
                        Some(id) => {
                            let Some(p) = env.level.entities.player_mut(id) else {
                                continue;
                            };
                            let infinite = p.game_mode == vanillite_config::GameMode::Creative;
                            let mut s = stack;
                            let added = p.inventory.add(None, &mut s, infinite);
                            items::send_inventory(&mut env.level.entities, id);
                            added
                        }
                        None => {
                            items::on_player(env, player, move |level, id| {
                                if let Some(p) = level.entities.player_mut(id) {
                                    let infinite =
                                        p.game_mode == vanillite_config::GameMode::Creative;
                                    let mut s = stack;
                                    p.inventory.add(None, &mut s, infinite);
                                    items::send_inventory(&mut level.entities, id);
                                }
                            });
                            true
                        }
                    };
                    if added {
                        used.track(drop, 1);
                    }
                }
            }
        }
        LootTarget::Spawn => {
            let c = args
                .coordinates("targetPos")
                .ok_or_else(|| missing("targetPos"))?;
            let pos = resolve_position(source, c);
            for drop in drops {
                if drop.is_empty() {
                    // An empty item entity takes its seed and is gone on its first tick.
                    vanillite_game::mob::new_entity_uuid();
                    used.track(drop, 1);
                    continue;
                }
                // `new ItemEntity(level, x, y, z, stack)`: the motion comes from the item's own
                // random, not the level's.
                vanillite_game::block::drops::spawn_item_moving(
                    env.level,
                    pos,
                    drop.clone(),
                    None,
                    10,
                    None,
                );
                used.track(drop, 1);
            }
        }
        LootTarget::Insert => {
            let pos = block_pos(args, "targetPos", source)?;
            let c = container(env, pos)?;
            for drop in drops {
                // `distributeToContainer`.
                let mut stack = drop.clone();
                let mut changed = false;
                let size = c.size(env.level);
                for slot in 0..size {
                    if stack.is_empty() {
                        break;
                    }
                    if !c.can_place(env.level, slot, &stack) {
                        continue;
                    }
                    let current = c.get(env.level, slot);
                    if current.is_empty() {
                        c.set(env.level, slot, std::mem::take(&mut stack));
                        changed = true;
                        break;
                    }
                    if can_merge(&current, &stack) {
                        let space = vanillite_game::item::max_stack_size(&stack) - current.count;
                        let n = stack.count.min(space);
                        if let Some(cur) = c.get_mut(env.level, slot) {
                            cur.count += n;
                        }
                        stack.count -= n;
                        changed = true;
                    }
                }
                if changed {
                    c.set_changed(env.level);
                    used.track(drop, 1);
                }
            }
        }
        LootTarget::ReplaceBlock => {
            let pos = block_pos(args, "targetPos", source)?;
            let c = container(env, pos)?;
            let start = slot_arg(args)?;
            let count = args
                .int("count")
                .copied()
                .map_or(drops.len(), |n| usize::try_from(n).unwrap_or(0));
            let size = c.size(env.level);
            let Some(start) = usize::try_from(start).ok().filter(|&s| s < size) else {
                return Err(CommandError::new(
                    "commands.item.target.no_such_slot",
                    vec![ErrArg::Int(start)],
                ));
            };
            for i in 0..count {
                let slot = start + i;
                let add = drops.get(i).cloned().unwrap_or_default();
                if slot < size && c.can_place(env.level, slot, &add) {
                    c.set(env.level, slot, add.clone());
                    used.track(&add, 1);
                }
            }
        }
        LootTarget::ReplaceEntity => {
            let targets = select::many(
                env,
                source,
                args.selector("entities")
                    .ok_or_else(|| missing("entities"))?,
            )?;
            let start = slot_arg(args)?;
            let count = args
                .int("count")
                .copied()
                .map_or(drops.len(), |n| usize::try_from(n).unwrap_or(0));
            for t in &targets {
                let Some(id) = t.entity else {
                    continue;
                };
                for i in 0..count {
                    let add = drops.get(i).cloned().unwrap_or_default();
                    let index = start.saturating_add(i32::try_from(i).unwrap_or(i32::MAX));
                    let entities = &mut env.level.entities;
                    if let Some(slot) = items::entity_slot(entities, id, index)
                        && items::slot_set(entities, id, slot, add.clone())
                    {
                        used.track(&add, 1);
                    }
                }
                items::sync_inventory(&mut env.level.entities, id);
            }
        }
    }
    Ok(())
}

/// The `slot` argument (`SlotArgument`: one slot).
fn slot_arg(args: &Args) -> Result<i32, CommandError> {
    match args.game("slot") {
        Some(GameValue::Slot(slot)) => Ok(*slot),
        _ => args.int("slot").copied().ok_or_else(|| missing("slot")),
    }
}

/// `/loot <target> <source>`.
pub(crate) fn run(
    target: LootTarget,
    src: LootSource,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let (drops, table) = drops(src, args, source, env)?;
    let mut used = Tracker::<ItemStack>::default();
    output(target, &drops, args, source, env, &mut used)?;
    used.feedback(source, env, false, false, None, |one, n, _| {
        match (one, &table) {
            (Some(drop), None) => Component::translatable_args(
                "commands.drop.success.single",
                vec![
                    Arg::Int(drop.count),
                    Arg::from(items::item_display(drop, None)),
                ],
            ),
            (None, None) => {
                Component::translatable_args("commands.drop.success.multiple", vec![Arg::Int(n)])
            }
            (Some(drop), Some(table)) => Component::translatable_args(
                "commands.drop.success.single_with_table",
                vec![
                    Arg::Int(drop.count),
                    Arg::from(items::item_display(drop, None)),
                    Arg::String(table.clone()),
                ],
            ),
            (None, Some(table)) => Component::translatable_args(
                "commands.drop.success.multiple_with_table",
                vec![Arg::Int(n), Arg::String(table.clone())],
            ),
        }
    })
}
