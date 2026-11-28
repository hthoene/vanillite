//! `/give`, `/clear`, `/item` and `/enchant` (Vanilla's `GiveCommand`,
//! `ClearInventoryCommands`, `ItemCommands` with its `ItemAccessor`s, `EnchantCommand`), and
//! item stacks from command arguments (`ItemInput`).
//!
//! Players in other worlds than the command's are changed on their world between ticks where
//! the outcome does not depend on their state (`give`); commands that count what they change
//! (`clear`, `item`, `enchant`) see the entities of the command's world only.

use vanillite_base::Vec3;
use vanillite_base::random::RandomSource as _;
use vanillite_command::error::CommandError;
use vanillite_command::{
    Arg as ErrArg, ArgValue, Args, GameValue, ItemArg, ItemPredicateArg, ItemTest, ResourceOrTag,
    SlotSourceArg,
};
use vanillite_data::entity::ids as entity_ids;
use vanillite_data::item::{ItemId, Rarity};
use vanillite_data::registry::{BLOCK, DATA_COMPONENT_TYPE, ITEM};
use vanillite_game::block::container::ContainerRef;
use vanillite_game::commands::ItemOp;
use vanillite_game::entities::Entities;
use vanillite_game::inventory::OFFHAND;
use vanillite_game::item::{self, EquipmentSlot, ItemStack};
use vanillite_game::level::Level;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::codec::{DecodeLimited as _, Encode as _, Reader, write_varint};
use vanillite_protocol::components::ids as component_ids;
use vanillite_protocol::components_nbt;
use vanillite_protocol::item::InstalledRegistries;
use vanillite_protocol::text::{Arg, Color, Component, HoverEvent, NamedColor, OpaqueNbt};
use vanillite_world::entity::EntityId;

use super::Env;
use super::source::CommandSource;
use super::tracker::Tracker;
use super::{dispatch, execute, select};
use select::Target;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

fn error(key: &'static str, args: Vec<ErrArg>) -> CommandError {
    CommandError::new(key, args)
}

/// What a system that does not exist yet answers (loot item modifiers, slot sources, item
/// component predicates); logged so the cause is not lost behind the generic message.
fn unsupported(what: &str) -> CommandError {
    log::warn!("command failed: {what} is not supported yet");
    error("command.failed", vec![])
}

fn count_int(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn targets(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Vec<Target>, CommandError> {
    let selector = args.selector(name).ok_or_else(|| missing(name))?;
    select::many(env, source, selector)
}

/// Runs `f` on a player's level and entity: now when it is in this world, otherwise on its
/// world between ticks. Returns whether it ran now.
pub(crate) fn on_player(
    env: &mut Env<'_>,
    target: &Target,
    f: impl FnOnce(&mut Level, EntityId) + Send + 'static,
) -> bool {
    if let Some(id) = target.entity {
        f(env.level, id);
        return true;
    }
    let uuid = target.uuid;
    env.global.run_in_world(target.world, move |world| {
        let level = world.level_mut();
        if let Some(id) = select::find_uuid(&level.entities, uuid) {
            f(level, id);
        }
    });
    false
}

/// `containerMenu.broadcastChanges` of a player.
pub(crate) fn sync_inventory(entities: &mut Entities, id: EntityId) {
    if let Some(player) = entities.player_mut(id) {
        let sink = std::sync::Arc::clone(&player.connection.sink);
        player.inventory.broadcast_changes(sink.as_ref());
        // The inventory's container listener (`slotChanged`): `inventory_changed` at once.
        player.advancements.check_inventory(&player.inventory);
    }
}

/// [`sync_inventory`] without the container listener: the slots only (`Inventory.add` of
/// `/loot give`, which leaves `inventory_changed` to the player's next tick like Vanilla).
pub(crate) fn send_inventory(entities: &mut Entities, id: EntityId) {
    if let Some(player) = entities.player_mut(id) {
        let sink = std::sync::Arc::clone(&player.connection.sink);
        player.inventory.broadcast_changes(sink.as_ref());
    }
}

// --- Item stacks ------------------------------------------------------------------------------

/// The translation key of an item's default name (`Item.getDescriptionId`).
fn item_name_key(item: u16) -> String {
    let info = ItemId(item).info();
    let path = info.name.rsplit(':').next().unwrap_or("");
    let block_named = (info.block != u16::MAX && BLOCK.name(info.block) == Some(info.name))
        && !matches!(info.name, "minecraft:resin_clump" | "minecraft:nether_wart")
        || info.name == "minecraft:air";
    if block_named {
        format!("block.minecraft.{path}")
    } else {
        format!("item.minecraft.{path}")
    }
}

/// `ItemStack.getHoverName`: the custom name or the item's name.
fn hover_name(stack: &ItemStack) -> (Component, bool) {
    if let Some(bytes) = stack.component(component_ids::CUSTOM_NAME)
        && let Ok(name) = Component::decode_limited(&mut Reader::new(bytes), bytes.len())
    {
        return (name, true);
    }
    (
        Component::translatable(item_name_key(stack.item), vec![]),
        false,
    )
}

/// `ItemStack.getRarity`: enchanted items are one step rarer.
fn rarity_color(stack: &ItemStack) -> NamedColor {
    let enchanted = !item::enchantments(stack).is_empty();
    match (ItemId(stack.item).info().rarity, enchanted) {
        (Rarity::Common, false) => NamedColor::White,
        (Rarity::Uncommon, false) => NamedColor::Yellow,
        (Rarity::Common | Rarity::Uncommon | Rarity::Rare, true) | (Rarity::Rare, false) => {
            NamedColor::Aqua
        }
        (Rarity::Epic, _) => NamedColor::LightPurple,
    }
}

/// `ItemStack.getDisplayName`: the name in brackets in the rarity colour, showing the item on
/// hover; `components` is the component patch as NBT when known.
pub(crate) fn item_display(stack: &ItemStack, components: Option<&Compound>) -> Component {
    let (name, custom) = hover_name(stack);
    let mut name = Component::empty().append(name);
    if custom {
        name = name.italic(true);
    }
    let mut out = Component::translatable("chat.square_brackets", vec![name]);
    if !stack.is_empty() {
        out = out
            .color(Color::Named(rarity_color(stack)))
            .hover(HoverEvent::ShowItem {
                id: ITEM.name(stack.item).unwrap_or("minecraft:air").to_owned(),
                count: stack.count,
                components: components.map(|c| OpaqueNbt(Tag::Compound(c.clone()))),
            });
    }
    out
}

fn qualify(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// The wire value of a component written in an item argument (`ItemParser.readComponent`: the
/// persistent codec of the component type, see [`components_nbt::from_nbt`]). Text components
/// are normalised like Vanilla re-encodes them; the ranges of the persistent codecs that the
/// wire form does not check are checked here.
fn encode_component(name: &str, value: &Tag, env: &Env<'_>) -> Result<Vec<u8>, CommandError> {
    let malformed = || {
        error(
            "arguments.item.component.malformed",
            vec![
                ErrArg::Str(name.to_owned()),
                ErrArg::Str(vanillite_nbt::snbt::to_string(value)),
            ],
        )
    };
    let int = |tag: &Tag, min: i32, max: i32| {
        tag.as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .filter(|v| (min..=max).contains(v))
            .ok_or_else(malformed)
    };
    let component = |tag: &Tag| Component::try_from_nbt(tag).map_err(|_| malformed());
    let mut out = Vec::new();
    match name {
        "minecraft:max_stack_size" => write_varint(&mut out, int(value, 1, 99)?),
        "minecraft:max_damage" => write_varint(&mut out, int(value, 1, i32::MAX)?),
        "minecraft:ominous_bottle_amplifier" => write_varint(&mut out, int(value, 0, 4)?),
        "minecraft:damage" | "minecraft:repair_cost" => {
            write_varint(&mut out, int(value, 0, i32::MAX)?);
        }
        "minecraft:unbreakable" => {
            if !matches!(value, Tag::Compound(c) if c.is_empty()) {
                return Err(malformed());
            }
        }
        "minecraft:enchantment_glint_override" => out.push(u8::from(int(value, 0, 1)? == 1)),
        "minecraft:custom_name" | "minecraft:item_name" => component(value)?.encode(&mut out),
        "minecraft:lore" => {
            let Tag::List(lines) = value else {
                return Err(malformed());
            };
            if lines.0.len() > 256 {
                return Err(malformed());
            }
            write_varint(&mut out, count_int(lines.0.len()));
            for line in &lines.0 {
                component(line)?.encode(&mut out);
            }
        }
        "minecraft:enchantments" | "minecraft:stored_enchantments" => {
            let Tag::Compound(levels) = value else {
                return Err(malformed());
            };
            write_varint(&mut out, count_int(levels.len()));
            for (enchantment, level) in levels.iter() {
                let id = env
                    .global
                    .game()
                    .enchantments
                    .get(&qualify(enchantment))
                    .and_then(|id| i32::try_from(*id).ok())
                    .ok_or_else(malformed)?;
                write_varint(&mut out, id);
                write_varint(&mut out, int(level, 1, 255)?);
            }
        }
        "minecraft:rarity" => {
            let rarity = ["common", "uncommon", "rare", "epic"]
                .iter()
                .position(|r| value.as_str() == Some(*r))
                .ok_or_else(malformed)?;
            write_varint(&mut out, count_int(rarity));
        }
        "minecraft:custom_data" => {
            if !matches!(value, Tag::Compound(_)) {
                return Err(malformed());
            }
            vanillite_nbt::write::write_network(value, &mut out).map_err(|_| malformed())?;
        }
        _ => {
            let type_id = DATA_COMPONENT_TYPE.id(name).ok_or_else(malformed)?;
            return components_nbt::from_nbt(type_id, value, &InstalledRegistries)
                .map(Vec::from)
                .map_err(|e| {
                    error(
                        "arguments.item.component.malformed",
                        vec![ErrArg::Str(name.to_owned()), ErrArg::Str(e.to_string())],
                    )
                });
        }
    }
    let type_id = DATA_COMPONENT_TYPE.id(name).ok_or_else(malformed)?;
    vanillite_protocol::components::validate(type_id, &out, &InstalledRegistries)
        .map_err(|_| malformed())?;
    Ok(out)
}

/// An item argument (`ItemInput`): a stack of one with its components, and the components as
/// NBT for the hover text.
pub(crate) struct ItemInput {
    stack: ItemStack,
    components: Option<Compound>,
}

impl ItemInput {
    /// `ItemArgument.getItem`.
    pub(crate) fn of(args: &Args, name: &str, env: &Env<'_>) -> Result<ItemInput, CommandError> {
        let Some(GameValue::Item(arg)) = args.game(name) else {
            return Err(missing(name));
        };
        Self::from_arg(arg, env)
    }

    fn from_arg(arg: &ItemArg, env: &Env<'_>) -> Result<ItemInput, CommandError> {
        let item = ITEM.id(&arg.item).ok_or_else(|| {
            error(
                "argument.item.id.invalid",
                vec![ErrArg::Str(arg.item.clone())],
            )
        })?;
        let mut stack = ItemStack::new(item, 1);
        let mut nbt = Compound::new();
        for (component, value) in &arg.components {
            let type_id = DATA_COMPONENT_TYPE
                .id(component)
                .ok_or_else(|| unsupported(&format!("item component {component}")))?;
            match value {
                Some(value) => {
                    let bytes = encode_component(component, value, env)?;
                    // The hover text shows the decoded value, as Vanilla encodes the patch.
                    let shown = components_nbt::to_nbt(type_id, &bytes, &InstalledRegistries)
                        .unwrap_or_else(|_| value.clone());
                    stack.set_component(type_id, bytes.into());
                    nbt.insert(component.clone(), shown);
                }
                None => {
                    stack.remove_component(type_id);
                    nbt.insert(format!("!{component}"), Compound::new());
                }
            }
        }
        Ok(ItemInput {
            stack,
            components: (!nbt.is_empty()).then_some(nbt),
        })
    }

    /// `createItemStack(count)`.
    pub(crate) fn create(&self, count: i32) -> Result<ItemStack, CommandError> {
        let max = item::max_stack_size(&self.stack);
        if count > max {
            return Err(error(
                "arguments.item.overstacked",
                vec![
                    ErrArg::Str(ITEM.name(self.stack.item).unwrap_or("").to_owned()),
                    ErrArg::Int(max),
                ],
            ));
        }
        // `ItemStack.validateComponents`.
        if max > 1 && effective_component(&self.stack, component_ids::MAX_DAMAGE).is_some() {
            return Err(error(
                "arguments.item.malformed",
                vec![ErrArg::Str(
                    "Item cannot be both damageable and stackable".to_owned(),
                )],
            ));
        }
        Ok(self.stack.with_count(count))
    }

    fn display(&self, stack: &ItemStack) -> Component {
        item_display(stack, self.components.as_ref())
    }
}

// --- give ---------------------------------------------------------------------------------------

/// `LivingEntity.createItemStackToDrop(stack, false, false)` with `setNoPickUpDelay` and the
/// player as the only one to pick it up (`GiveCommand`).
pub(crate) fn drop_for(level: &mut Level, id: EntityId, stack: ItemStack) {
    let Some(uuid) = level.entities.store.get(id).map(|e| e.uuid()) else {
        return;
    };
    if let Some(item) = spawn_drop(level, id, stack)
        && let Some(data) = level.entities.item_mut(item)
    {
        data.target = Some(uuid);
    }
}

/// `createItemStackToDrop(stack, false, false)` added to the level: the stack from the
/// player's hand height, thrown where it looks with the player's own random.
fn spawn_drop(level: &mut Level, id: EntityId, stack: ItemStack) -> Option<EntityId> {
    let (pos, rot) = level.entities.store.get(id).map(|e| (e.pos(), e.rot()))?;
    let eye = f64::from(entity_ids::PLAYER.info().eye_height);
    let deg = std::f64::consts::PI / 180.0;
    let (sin_x, cos_x) = (
        vanillite_base::math::sin(f64::from(rot.x_rot) * deg),
        vanillite_base::math::cos(f64::from(rot.x_rot) * deg),
    );
    let (sin_y, cos_y) = (
        vanillite_base::math::sin(f64::from(rot.y_rot) * deg),
        vanillite_base::math::cos(f64::from(rot.y_rot) * deg),
    );
    // The player's own random (`createItemStackToDrop`).
    let random = level
        .entities
        .player_mut(id)
        .map(|p| &mut p.survival.random)?;
    let dir = random.next_float() * std::f32::consts::TAU;
    let pow2 = 0.02 * random.next_float();
    let spread = random.next_float() - random.next_float();
    let velocity = Vec3::new(
        f64::from(-sin_y * cos_x * 0.3) + f64::from(dir).cos() * f64::from(pow2),
        f64::from(-sin_x * 0.3 + 0.1 + spread * 0.1),
        f64::from(cos_y * cos_x * 0.3) + f64::from(dir).sin() * f64::from(pow2),
    );
    let at = Vec3::new(pos.x, pos.y + eye - 0.3, pos.z);
    vanillite_game::block::drops::spawn_item(level, at, stack, velocity, 0, None)
}

/// `GiveCommand.giveItem` for one player: stacks into the inventory, the rest dropped.
pub(crate) fn give_to(
    level: &mut Level,
    id: EntityId,
    prototype: &ItemStack,
    count: i32,
    max: i32,
) {
    let mut remaining = count;
    while remaining > 0 {
        let size = max.min(remaining).max(1);
        remaining = remaining.saturating_sub(size);
        let mut stack = prototype.with_count(size);
        let Some(player) = level.entities.player_mut(id) else {
            return;
        };
        let infinite = player.game_mode == vanillite_config::GameMode::Creative;
        let added = player.inventory.add(None, &mut stack, infinite);
        if !(added && stack.is_empty()) {
            drop_for(level, id, stack);
            continue;
        }
        // A fake drop of the prototype for the pickup animation (`makeFakeItem`: never
        // picked up, gone on its next tick), then the pickup sound.
        if let Some(fake) = spawn_drop(level, id, prototype.with_count(1))
            && let Some(data) = level.entities.item_mut(fake)
        {
            data.pickup_delay = i16::MAX;
            data.age = 5999;
        }
        let Some((pos, random)) = level.entities.store.get(id).map(|e| e.pos()).zip(
            level
                .entities
                .player_mut(id)
                .map(|p| &mut p.survival.random),
        ) else {
            continue;
        };
        let pitch = ((random.next_float() - random.next_float()) * 0.7 + 1.0) * 2.0;
        if let Some(sound) =
            vanillite_data::registry::SOUND_EVENT.id("minecraft:entity.item.pickup")
        {
            // `SoundSource.PLAYERS`.
            level.play_sound(None, pos.x, pos.y, pos.z, sound, 7, 0.2, pitch);
        }
    }
    sync_inventory(&mut level.entities, id);
}

/// `/give <targets> <item> [<count>]`.
pub(crate) fn give(
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let players = targets(args, "targets", source, env)?;
    let input = ItemInput::of(args, "item", env)?;
    let count = args.int("count").copied().unwrap_or(1);
    let prototype = input.create(1)?;
    let display = input.display(&prototype);
    let max = item::max_stack_size(&prototype);
    let allowed = max.saturating_mul(100);
    if count > allowed {
        return Err(error(
            "commands.give.failed.toomanyitems",
            vec![ErrArg::Int(allowed), ErrArg::Component(Box::new(display))],
        ));
    }
    let mut tracker = Tracker::<Component>::default();
    for player in &players {
        let stack = prototype.clone();
        on_player(env, player, move |level, id| {
            give_to(level, id, &stack, count, max)
        });
        tracker.track(&player.display(), 1);
    }
    tracker.feedback(source, env, true, false, None, |one, n, _| match one {
        Some(player) => Component::translatable_args(
            "commands.give.success.single",
            vec![
                Arg::Int(count),
                Arg::from(display),
                Arg::from(player.clone()),
            ],
        ),
        None => Component::translatable_args(
            "commands.give.success.multiple",
            vec![Arg::Int(count), Arg::from(display), Arg::Int(n)],
        ),
    })
}

// --- clear --------------------------------------------------------------------------------------

/// An item predicate argument (`ItemPredicateArgument`): the items it accepts (`None`: all)
/// and its component tests, every group needing one matching alternative.
#[derive(Clone, Debug, Default)]
pub(crate) struct ItemFilter {
    items: Option<Vec<u16>>,
    tests: Vec<Vec<ItemCheck>>,
}

/// One resolved component test.
#[derive(Clone, Debug)]
enum ItemCheck {
    /// The `count` pseudo component and predicate.
    Count(IntBounds),
    /// `component`: the component is present.
    Has(u16),
    /// `component=value`: the component's wire value equals the value's.
    Equals(u16, Box<[u8]>),
    /// `damage~{durability, damage}`.
    Damage(IntBounds, IntBounds),
    /// `custom_data~{...}`: `compareNbt` against the stack's custom data.
    CustomData(Compound),
    /// `enchantments~[...]`: each entry finds an enchantment (ids, levels).
    Enchantments(Vec<(Option<Vec<u32>>, IntBounds)>),
    Not(Box<ItemCheck>),
}

/// `MinMaxBounds.Ints`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct IntBounds {
    min: Option<i64>,
    max: Option<i64>,
}

impl IntBounds {
    fn any(self) -> bool {
        self == IntBounds::default()
    }

    fn matches(self, v: i64) -> bool {
        self.min.is_none_or(|m| v >= m) && self.max.is_none_or(|m| v <= m)
    }
}

fn tag_int(tag: &Tag) -> Option<i64> {
    Some(match tag {
        Tag::Byte(v) => i64::from(*v),
        Tag::Short(v) => i64::from(*v),
        Tag::Int(v) => i64::from(*v),
        Tag::Long(v) => *v,
        _ => return None,
    })
}

/// `MinMaxBounds.Ints.CODEC`: a number or `{min, max}`.
fn int_bounds(tag: &Tag) -> Option<IntBounds> {
    if let Some(v) = tag_int(tag) {
        return Some(IntBounds {
            min: Some(v),
            max: Some(v),
        });
    }
    let Tag::Compound(c) = tag else {
        return None;
    };
    let field = |k: &str| c.get(k).map(|t| tag_int(t).ok_or(())).transpose();
    Some(IntBounds {
        min: field("min").ok()?,
        max: field("max").ok()?,
    })
}

fn qualified(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    }
}

fn malformed(kind: &str, name: &str) -> CommandError {
    error(
        if kind == "component" {
            "arguments.item.component.malformed"
        } else {
            "arguments.item.predicate.malformed"
        },
        vec![
            ErrArg::Str(name.to_owned()),
            ErrArg::Str("invalid value".to_owned()),
        ],
    )
}

/// A persistent component type by name.
fn component_type(name: &str) -> Result<u16, CommandError> {
    vanillite_protocol::components_nbt::type_by_name(name)
        .filter(|&t| !vanillite_protocol::components_nbt::is_transient(t))
        .ok_or_else(|| {
            error(
                "arguments.item.component.unknown",
                vec![ErrArg::Str(name.to_owned())],
            )
        })
}

/// The enchantment ids of a holder set (an id, `#tag` or a list).
fn enchantment_set(tag: &Tag, env: &Env<'_>) -> Option<Vec<u32>> {
    let data = vanillite_game::player::play::game_data()?;
    let id = |name: &str| data.enchantments.get(&qualified(name)).copied();
    match tag {
        Tag::String(s) => match s.strip_prefix('#') {
            Some(t) => Some(
                env.global
                    .systems
                    .tags()
                    .get("minecraft:enchantment")
                    .and_then(|tags| tags.get(&qualified(t)))
                    .map(|members| members.iter().filter_map(|m| id(m)).collect())
                    .unwrap_or_default(),
            ),
            None => Some(vec![id(s)?]),
        },
        Tag::List(l) => {
            l.0.iter()
                .map(|t| match t {
                    Tag::String(s) => id(s),
                    _ => None,
                })
                .collect()
        }
        _ => None,
    }
}

fn check(test: &ItemTest, env: &Env<'_>) -> Result<ItemCheck, CommandError> {
    Ok(match test {
        ItemTest::Not(t) => ItemCheck::Not(Box::new(check(t, env)?)),
        ItemTest::Has(name) if qualified(name) == "minecraft:count" => {
            ItemCheck::Count(IntBounds::default())
        }
        ItemTest::Has(name) => ItemCheck::Has(component_type(&qualified(name))?),
        ItemTest::Equals(name, value) => {
            let name = qualified(name);
            if name == "minecraft:count" {
                ItemCheck::Count(int_bounds(value).ok_or_else(|| malformed("component", &name))?)
            } else {
                let t = component_type(&name)?;
                let wire = vanillite_game::item_nbt::component_from_nbt(t, value)
                    .ok_or_else(|| malformed("component", &name))?;
                ItemCheck::Equals(t, wire)
            }
        }
        ItemTest::Matches(name, value) => {
            let name = qualified(name);
            let bad = || malformed("predicate", &name);
            match name.as_str() {
                "minecraft:count" => ItemCheck::Count(int_bounds(value).ok_or_else(bad)?),
                "minecraft:damage" => {
                    let Tag::Compound(c) = value else {
                        return Err(bad());
                    };
                    let field = |k: &str| c.get(k).map_or(Some(IntBounds::default()), int_bounds);
                    ItemCheck::Damage(
                        field("durability").ok_or_else(bad)?,
                        field("damage").ok_or_else(bad)?,
                    )
                }
                "minecraft:custom_data" => ItemCheck::CustomData(match value {
                    Tag::Compound(c) => c.clone(),
                    Tag::String(s) => vanillite_nbt::snbt::parse_compound(s).map_err(|_| bad())?,
                    _ => return Err(bad()),
                }),
                "minecraft:enchantments" => {
                    let Tag::List(list) = value else {
                        return Err(bad());
                    };
                    let mut entries = Vec::new();
                    for e in &list.0 {
                        let Tag::Compound(c) = e else {
                            return Err(bad());
                        };
                        let set = match c.get("enchantments") {
                            Some(t) => Some(enchantment_set(t, env).ok_or_else(bad)?),
                            None => None,
                        };
                        let levels = c
                            .get("levels")
                            .map_or(Some(IntBounds::default()), int_bounds)
                            .ok_or_else(bad)?;
                        entries.push((set, levels));
                    }
                    ItemCheck::Enchantments(entries)
                }
                // The other predicate types are not supported yet.
                "minecraft:stored_enchantments"
                | "minecraft:potion_contents"
                | "minecraft:container"
                | "minecraft:bundle_contents"
                | "minecraft:firework_explosion"
                | "minecraft:fireworks"
                | "minecraft:writable_book_content"
                | "minecraft:written_book_content"
                | "minecraft:attribute_modifiers"
                | "minecraft:trim"
                | "minecraft:jukebox_playable"
                | "minecraft:villager/variant" => {
                    return Err(unsupported(&format!("item predicate {name}")));
                }
                // A component without a predicate type: its existence (`Unit`, `{}`).
                _ => {
                    let t = vanillite_protocol::components_nbt::type_by_name(&name).ok_or_else(
                        || {
                            error(
                                "arguments.item.predicate.unknown",
                                vec![ErrArg::Str(name.clone())],
                            )
                        },
                    )?;
                    if !matches!(value, Tag::Compound(c) if c.is_empty()) {
                        return Err(bad());
                    }
                    ItemCheck::Has(t)
                }
            }
        }
    })
}

/// The effective wire value of a component: the patch, else the item's default.
fn effective_component(stack: &ItemStack, type_id: u16) -> Option<Box<[u8]>> {
    vanillite_game::item_nbt::effective_component(stack, type_id).map(|v| v.into_owned().into())
}

impl ItemCheck {
    fn matches(&self, stack: &ItemStack) -> bool {
        match self {
            ItemCheck::Not(t) => !t.matches(stack),
            ItemCheck::Count(b) => b.matches(i64::from(stack.count)),
            ItemCheck::Has(t) => effective_component(stack, *t).is_some(),
            ItemCheck::Equals(t, wire) => {
                effective_component(stack, *t).is_some_and(|v| v == *wire)
            }
            ItemCheck::Damage(durability, damage) => {
                let Some(d) = effective_component(stack, component_ids::DAMAGE)
                    .and_then(|v| Reader::new(&v).varint().ok())
                else {
                    return false;
                };
                let max = item::max_damage(stack);
                durability.matches(i64::from(max) - i64::from(d)) && damage.matches(i64::from(d))
            }
            ItemCheck::CustomData(expected) => {
                let actual = DATA_COMPONENT_TYPE
                    .id("minecraft:custom_data")
                    .and_then(|t| Some((t, stack.component(t)?)))
                    .and_then(|(t, v)| vanillite_game::item_nbt::component_to_nbt(t, v));
                let empty = Compound::new();
                let actual = match &actual {
                    Some(Tag::Compound(c)) => c,
                    _ => &empty,
                };
                super::nbt_ops::compound_matches(expected, actual)
            }
            ItemCheck::Enchantments(entries) => {
                let present = item::enchantments(stack);
                entries.iter().all(|(set, levels)| match set {
                    // `EnchantmentPredicate.containedIn`.
                    Some(ids) => ids.iter().any(|id| {
                        present
                            .iter()
                            .find(|(e, _)| e == id)
                            .is_some_and(|&(_, level)| {
                                level != 0 && levels.matches(i64::from(level))
                            })
                    }),
                    None if !levels.any() => present
                        .iter()
                        .any(|&(_, level)| levels.matches(i64::from(level))),
                    None => !present.is_empty(),
                })
            }
        }
    }
}

impl ItemFilter {
    /// Whether a non-empty stack matches.
    pub(crate) fn matches(&self, stack: &ItemStack) -> bool {
        !stack.is_empty()
            && self.items.as_ref().is_none_or(|a| a.contains(&stack.item))
            && self
                .tests
                .iter()
                .all(|group| group.iter().any(|t| t.matches(stack)))
    }
}

/// [`ItemFilter`] of the item predicate argument `item`.
fn accepted_items(args: &Args, env: &Env<'_>) -> Result<ItemFilter, CommandError> {
    accepted_items_of(args, "item", env)
}

/// [`ItemFilter`] of the item predicate argument `name` (all items without one).
pub(crate) fn accepted_items_of(
    args: &Args,
    name: &str,
    env: &Env<'_>,
) -> Result<ItemFilter, CommandError> {
    let Some(GameValue::ItemPredicate(predicate)) = args.game(name) else {
        return Ok(ItemFilter::default());
    };
    let ItemPredicateArg { item, tests } = predicate.as_ref();
    let tests = tests
        .iter()
        .map(|group| group.iter().map(|t| check(t, env)).collect())
        .collect::<Result<_, _>>()?;
    let items = match item {
        None => None,
        Some(ResourceOrTag::Resource(id)) => Some(ITEM.id(id).into_iter().collect()),
        Some(ResourceOrTag::Tag(tag)) => Some(
            env.global
                .systems
                .tags()
                .get("minecraft:item")
                .and_then(|t| t.get(tag))
                .map(|entries| entries.iter().filter_map(|e| ITEM.id(e)).collect())
                .unwrap_or_default(),
        ),
    };
    Ok(ItemFilter { items, tests })
}

/// `Inventory.clearOrCountMatchingItems`: the inventory, the crafting grid and the carried
/// stack; `max` < 0 removes all.
fn clear_player(
    entities: &mut Entities,
    id: EntityId,
    accepted: &ItemFilter,
    counting: bool,
    max: i32,
) -> i32 {
    let Some(player) = entities.player_mut(id) else {
        return 0;
    };
    let mut count = 0i32;
    let mut take = |stack: &mut ItemStack| {
        if !accepted.matches(stack) {
            return;
        }
        if counting {
            count = count.saturating_add(stack.count);
            return;
        }
        let left = max.saturating_sub(count);
        let removed = if left < 0 {
            stack.count
        } else {
            left.min(stack.count)
        };
        stack.shrink(removed);
        count = count.saturating_add(removed);
    };
    let inventory = &mut player.inventory;
    for index in 0..=OFFHAND {
        if let Some(stack) = inventory.get_mut(index) {
            take(stack);
        }
    }
    inventory.craft.iter_mut().for_each(&mut take);
    take(&mut inventory.menu.carried);
    if !counting {
        sync_inventory(entities, id);
    }
    count
}

/// `/clear [<targets> [<item> [<maxCount>]]]`.
pub(crate) fn clear(
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let players = if args.has("targets") {
        targets(args, "targets", source, env)?
    } else {
        if source.uuid().is_none() {
            return Err(error("permissions.requires.player", vec![]));
        }
        vec![dispatch::self_target(env, source)?]
    };
    let accepted = accepted_items(args, env)?;
    let max = args.int("maxCount").copied().unwrap_or(-1);
    let counting = max == 0;
    let mut tracker = Tracker::<Component>::default();
    for player in &players {
        let count = match player.entity {
            Some(id) => clear_player(&mut env.level.entities, id, &accepted, counting, max),
            None => {
                if !counting {
                    let accepted = accepted.clone();
                    on_player(env, player, move |level, id| {
                        clear_player(&mut level.entities, id, &accepted, false, max);
                    });
                }
                0
            }
        };
        tracker.track(&player.display(), count);
    }
    if tracker.total() == 0 {
        return Err(match tracker.pick(true) {
            (Some(player), _) => error(
                "clear.failed.single",
                vec![ErrArg::Component(Box::new(player.clone()))],
            ),
            (None, n) => error("clear.failed.multiple", vec![ErrArg::Int(n)]),
        });
    }
    let key = if counting { "test" } else { "success" };
    tracker.feedback(source, env, true, false, None, |one, n, total| match one {
        Some(player) => Component::translatable_args(
            format!("commands.clear.{key}.single"),
            vec![Arg::Int(total), Arg::from(player.clone())],
        ),
        None => Component::translatable_args(
            format!("commands.clear.{key}.multiple"),
            vec![Arg::Int(total), Arg::Int(n)],
        ),
    })
}

// --- item ---------------------------------------------------------------------------------------

/// A slot of an entity (`SlotProvider.getSlot`): player inventory indices 0–40, the ender
/// chest, the cursor, the crafting grid, or the stack of an item entity.
#[derive(Clone, Copy, Debug)]
pub(crate) enum EntitySlot {
    Inventory(usize),
    EnderChest(usize),
    Cursor,
    Craft(usize),
    ItemEntity,
    /// An equipment slot of a mob (`LivingEntity.getSlot`).
    Equipment(EquipmentSlot),
}

/// The slot `index` of an entity, if it has it.
pub(crate) fn entity_slot(entities: &Entities, id: EntityId, index: i32) -> Option<EntitySlot> {
    let kind = entities.store.get(id)?.kind();
    if kind == entity_ids::ITEM {
        return (index == 0).then_some(EntitySlot::ItemEntity);
    }
    if entities.mob(id).is_some() {
        // `LivingEntity.getEquipmentSlot(slot)`.
        return Some(EntitySlot::Equipment(match index {
            98 => EquipmentSlot::MainHand,
            99 => EquipmentSlot::OffHand,
            100 => EquipmentSlot::Feet,
            101 => EquipmentSlot::Legs,
            102 => EquipmentSlot::Chest,
            103 => EquipmentSlot::Head,
            105 => EquipmentSlot::Body,
            106 => EquipmentSlot::Saddle,
            _ => return None,
        }));
    }
    let player = entities.player(id)?;
    let at = |base: i32| usize::try_from(index.saturating_sub(base)).ok();
    Some(match index {
        0..=35 => EntitySlot::Inventory(at(0)?),
        98 => EntitySlot::Inventory(player.inventory.selected),
        99 => EntitySlot::Inventory(OFFHAND),
        100..=103 => EntitySlot::Inventory(at(100)?.saturating_add(36)),
        200..=226 => EntitySlot::EnderChest(at(200)?),
        499 => EntitySlot::Cursor,
        500..=503 => EntitySlot::Craft(at(500)?),
        _ => return None,
    })
}

/// The stack in a slot of an entity (`SlotAccess.get`).
pub(crate) fn slot_get(entities: &Entities, id: EntityId, slot: EntitySlot) -> ItemStack {
    let stack = match slot {
        EntitySlot::ItemEntity => entities.item(id).map(|i| &i.stack),
        EntitySlot::Inventory(i) => entities.player(id).map(|p| p.inventory.get(i)),
        EntitySlot::EnderChest(i) => entities
            .player(id)
            .and_then(|p| p.inventory.ender_chest.get(i)),
        EntitySlot::Cursor => entities.player(id).map(|p| &p.inventory.menu.carried),
        EntitySlot::Craft(i) => entities.player(id).and_then(|p| p.inventory.craft.get(i)),
        EntitySlot::Equipment(e) => entities.mob(id).map(|m| m.equipment.get(e)),
    };
    stack.cloned().unwrap_or_default()
}

/// `SlotAccess.set`: armour slots below the head take only their own armour.
pub(crate) fn slot_set(
    entities: &mut Entities,
    id: EntityId,
    slot: EntitySlot,
    stack: ItemStack,
) -> bool {
    if let EntitySlot::ItemEntity = slot {
        return entities.item_mut(id).is_some_and(|i| {
            i.stack = stack;
            true
        });
    }
    if let EntitySlot::Equipment(e) = slot {
        // `createEquipmentSlotAccess`: below the head only items for that slot.
        let any = matches!(
            e,
            EquipmentSlot::Head | EquipmentSlot::MainHand | EquipmentSlot::OffHand
        );
        let fits = || item::equippable_slot(&stack) == Some(e);
        if !any && !stack.is_empty() && !fits() {
            return false;
        }
        return entities.mob_mut(id).is_some_and(|m| {
            m.equipment.set(e, stack);
            true
        });
    }
    let Some(player) = entities.player_mut(id) else {
        return false;
    };
    let inventory = &mut player.inventory;
    let target = match slot {
        EntitySlot::Inventory(i @ 36..=38) => {
            let wanted = match i {
                36 => EquipmentSlot::Feet,
                37 => EquipmentSlot::Legs,
                _ => EquipmentSlot::Chest,
            };
            if !stack.is_empty() && item::equipment_slot(&stack) != wanted {
                return false;
            }
            inventory.get_mut(i)
        }
        EntitySlot::Inventory(i) => inventory.get_mut(i),
        EntitySlot::EnderChest(i) => inventory.ender_chest.get_mut(i),
        EntitySlot::Cursor => Some(&mut inventory.menu.carried),
        EntitySlot::Craft(i) => inventory.craft.get_mut(i),
        EntitySlot::ItemEntity | EntitySlot::Equipment(_) => None,
    };
    target.is_some_and(|t| {
        *t = stack;
        true
    })
}

/// The slot range of a slot source argument: its name and indices. Slot sources from the
/// registry or inline need the loot context and are not supported.
fn slot_range(args: &Args, name: &str) -> Result<(String, Vec<i32>), CommandError> {
    match args.game(name) {
        Some(GameValue::SlotSource(SlotSourceArg::Range(range, slots))) => {
            Ok((range.clone(), slots.clone()))
        }
        Some(GameValue::SlotSource(SlotSourceArg::Source(_))) => Err(unsupported("slot source")),
        _ => Err(missing(name)),
    }
}

/// The stacks of the selected slots of an item accessor (`getSlots(...).itemCopies()`):
/// entities `source` or the container at block `source`, slots `slots`.
pub(crate) fn selected_stacks(
    block: bool,
    args: &Args,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Vec<ItemStack>, CommandError> {
    let (_, slots) = slot_range(args, "slots")?;
    if block {
        let c = args
            .coordinates("source")
            .ok_or_else(|| missing("source"))?;
        let p = super::exec::resolve_position(source, c);
        let pos = vanillite_base::BlockPos::containing(p.x, p.y, p.z);
        if !env.level.is_loaded(pos) {
            return Err(error("argument.pos.unloaded", vec![]));
        }
        let Some(container) = vanillite_game::block::container::ContainerRef::at(env.level, pos)
        else {
            return Err(error(
                "commands.item.source.not_a_container",
                vec![ErrArg::Int(pos.x), ErrArg::Int(pos.y), ErrArg::Int(pos.z)],
            ));
        };
        let container = match container {
            vanillite_game::block::container::ContainerRef::Double(..) => {
                vanillite_game::block::container::ContainerRef::Block(pos)
            }
            other => other,
        };
        let size = container.size(env.level);
        return Ok(slots
            .iter()
            .filter_map(|&i| usize::try_from(i).ok().filter(|&i| i < size))
            .map(|i| container.get(env.level, i))
            .collect());
    }
    let selector = args.selector("source").ok_or_else(|| missing("source"))?;
    let targets = select::optional(env, source, selector)?;
    let entities = &env.level.entities;
    Ok(targets
        .iter()
        .filter_map(|t| t.entity)
        .flat_map(|id| {
            slots
                .iter()
                .filter_map(move |&i| entity_slot(entities, id, i))
                .map(move |slot| slot_get(entities, id, slot))
        })
        .collect())
}

/// `BlockItemAccessor.getContainer`: the container block entity at the loaded block of the
/// accessor `name` (a double chest's half alone).
fn block_container(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
    target: bool,
) -> Result<(vanillite_base::BlockPos, ContainerRef), CommandError> {
    let pos = execute::loaded_block_pos(args, name, source, env)?;
    match ContainerRef::at(env.level, pos) {
        Some(ContainerRef::Block(_) | ContainerRef::Double(..)) => {
            Ok((pos, ContainerRef::Block(pos)))
        }
        _ => {
            let key = if target {
                "commands.item.target.not_a_container"
            } else {
                "commands.item.source.not_a_container"
            };
            Err(error(
                key,
                vec![ErrArg::Int(pos.x), ErrArg::Int(pos.y), ErrArg::Int(pos.z)],
            ))
        }
    }
}

/// Whether the item accessor `name` is a block.
fn is_block(args: &Args, name: &str) -> bool {
    matches!(args.get(name), Some(ArgValue::Coordinates(_)))
}

/// The entities of an item accessor.
fn item_entities(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Vec<Target>, CommandError> {
    targets(args, name, source, env)
}

/// `/item replace|modify block <pos>`: the container's selected slots get the provided stacks
/// (or the modifier's result for every non-empty one); `setItem` always succeeds.
fn item_block(
    op: ItemOp,
    provider: Option<Provider>,
    known: Option<Component>,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let (pos, container) = block_container(args, "target", source, env, true)?;
    let (slots_name, slots) = slot_range(args, "slots")?;
    let size = container.size(env.level);
    let here: Vec<usize> = slots
        .iter()
        .filter_map(|&i| usize::try_from(i).ok().filter(|&i| i < size))
        .collect();
    let mut selected = 0usize;
    let mut changed = 0i32;
    if op == ItemOp::Modify {
        let modifier = super::loot_cmd::resource_or_inline(args, "modifier")?;
        for i in here {
            let stack = container.get(env.level, i);
            if stack.is_empty() {
                continue;
            }
            selected += 1;
            let mut result = super::loot_cmd::apply_modifier(env, source, &modifier, &stack)?;
            limit_to_container(&mut result);
            container.set(env.level, i, result);
            changed += 1;
        }
    } else if let Some(mut provider) = provider {
        for i in here {
            if !provider.has_next() {
                break;
            }
            selected += 1;
            let mut stack = provider.next();
            limit_to_container(&mut stack);
            container.set(env.level, i, stack);
            changed += 1;
        }
    }
    if changed > 0 {
        container.set_changed(env.level);
    }
    if selected == 0 {
        return Err(error(
            "commands.item.target.no_such_slot",
            vec![ErrArg::Str(slots_name)],
        ));
    }
    let xyz = || [Arg::Int(pos.x), Arg::Int(pos.y), Arg::Int(pos.z)];
    let message = match (op, known) {
        (ItemOp::Modify, _) => Component::translatable_args(
            "commands.item.block.modify.success",
            std::iter::once(Arg::Int(changed)).chain(xyz()).collect(),
        ),
        (_, Some(item)) => Component::translatable_args(
            "commands.item.block.replace.success.known_item",
            std::iter::once(Arg::Int(changed))
                .chain(xyz())
                .chain(std::iter::once(Arg::from(item)))
                .collect(),
        ),
        (_, None) => Component::translatable_args(
            "commands.item.block.replace.success",
            std::iter::once(Arg::Int(changed)).chain(xyz()).collect(),
        ),
    };
    source.send_success(env.global, message, true);
    Ok(changed)
}

/// `BaseContainerBlockEntity.setItem`: `limitSize(getMaxStackSize(stack))` (every container
/// block entity keeps the item's own maximum).
fn limit_to_container(stack: &mut ItemStack) {
    let max = item::max_stack_size(stack);
    if stack.count > max {
        stack.count = max;
    }
}

/// The copies of the stacks in the source slots of an item accessor `name` (`getItems`).
fn source_stacks(
    args: &Args,
    name: &str,
    slots: &[i32],
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Vec<ItemStack>, CommandError> {
    if is_block(args, name) {
        let (_, container) = block_container(args, name, source, env, false)?;
        let size = container.size(env.level);
        return Ok(slots
            .iter()
            .filter_map(|&i| usize::try_from(i).ok().filter(|&i| i < size))
            .map(|i| container.get(env.level, i))
            .collect());
    }
    let sources = item_entities(args, name, source, env)?;
    let entities = &env.level.entities;
    Ok(sources
        .iter()
        .filter_map(|s| s.entity)
        .flat_map(|id| {
            slots
                .iter()
                .filter_map(move |&i| entity_slot(entities, id, i))
                .map(move |slot| slot_get(entities, id, slot))
        })
        .collect())
}

/// `ItemProvider` of the distributions: `of`, `cycle`, `of(...).orElseProvide(EMPTY)`.
struct Provider {
    items: Vec<ItemStack>,
    index: usize,
    op: ItemOp,
}

impl Provider {
    fn has_next(&self) -> bool {
        match self.op {
            ItemOp::Fill => !self.items.is_empty(),
            ItemOp::Override => true,
            _ => self.index < self.items.len(),
        }
    }

    fn next(&mut self) -> ItemStack {
        let i = match self.op {
            ItemOp::Fill => self.index.checked_rem(self.items.len()).unwrap_or(0),
            _ => self.index,
        };
        self.index = self.index.saturating_add(1);
        self.items.get(i).cloned().unwrap_or_default()
    }
}

/// `/item (replace|fill|override|modify) ...`.
pub(crate) fn item(
    op: ItemOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let block_target = is_block(args, "target");
    if op == ItemOp::Modify && block_target {
        return item_block(op, None, None, args, source, env);
    }
    if op == ItemOp::Modify {
        let targets = item_entities(args, "target", source, env)?;
        let (slots_name, slots) = slot_range(args, "slots")?;
        return modify_items(&targets, &slots_name, &slots, args, source, env);
    }
    if block_target {
        // The target must be a container before the source is read.
        block_container(args, "target", source, env, true)?;
    }
    let (items, known) = if args.has("item") {
        let input = ItemInput::of(args, "item", env)?;
        let stack = input.create(args.int("count").copied().unwrap_or(1))?;
        let display = input.display(&stack);
        (vec![stack], Some(display))
    } else {
        let (source_name, source_slots) = slot_range(args, "sourceSlots")?;
        let items = source_stacks(args, "source", &source_slots, source, env)?;
        if items.is_empty() {
            return Err(error(
                "commands.item.source.no_such_slot",
                vec![ErrArg::Str(source_name)],
            ));
        }
        let items = if args.has("modifier") {
            let modifier = super::loot_cmd::resource_or_inline(args, "modifier")?;
            items
                .iter()
                .map(|i| super::loot_cmd::apply_modifier(env, source, &modifier, i))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            items
        };
        (items, None)
    };
    let mut provider = Provider {
        items,
        index: 0,
        op,
    };
    if block_target {
        return item_block(op, Some(provider), known, args, source, env);
    }
    let targets = item_entities(args, "target", source, env)?;
    let (slots_name, slots) = slot_range(args, "slots")?;
    let mut selected = 0usize;
    let mut tracker = Tracker::<Component>::default();
    for target in &targets {
        let Some(id) = target.entity else {
            continue;
        };
        provider.index = 0;
        let entities = &mut env.level.entities;
        let here: Vec<EntitySlot> = slots
            .iter()
            .filter_map(|&i| entity_slot(entities, id, i))
            .collect();
        let mut changed = 0i32;
        for slot in here {
            if !provider.has_next() {
                break;
            }
            selected = selected.saturating_add(1);
            if slot_set(entities, id, slot, provider.next()) {
                changed = changed.saturating_add(1);
            }
        }
        if changed > 0 {
            sync_inventory(entities, id);
        }
        tracker.track(&target.display(), changed);
    }
    if selected == 0 {
        return Err(error(
            "commands.item.target.no_such_slot",
            vec![ErrArg::Str(slots_name)],
        ));
    }
    match known {
        Some(item) => tracker.feedback(
            source,
            env,
            true,
            false,
            Some(error(
                "commands.item.target.failed.known_item",
                vec![ErrArg::Component(Box::new(item.clone()))],
            )),
            |one, n, total| match one {
                Some(entity) => Component::translatable_args(
                    "commands.item.entity.replace.success.single.known_item",
                    vec![
                        Arg::Int(total),
                        Arg::from(entity.clone()),
                        Arg::from(item.clone()),
                    ],
                ),
                None => Component::translatable_args(
                    "commands.item.entity.replace.success.multiple.known_item",
                    vec![Arg::Int(n), Arg::from(item.clone())],
                ),
            },
        ),
        None => tracker.feedback(
            source,
            env,
            true,
            false,
            Some(error("commands.item.target.failed", vec![])),
            |one, n, total| match one {
                Some(entity) => Component::translatable_args(
                    "commands.item.entity.replace.success.single",
                    vec![Arg::Int(total), Arg::from(entity.clone())],
                ),
                None => Component::translatable_args(
                    "commands.item.entity.replace.success.multiple",
                    vec![Arg::Int(n)],
                ),
            },
        ),
    }
}

/// `ItemCommands.modifyItems` on entities: the modifier on every non-empty selected slot.
fn modify_items(
    targets: &[Target],
    slots_name: &str,
    slots: &[i32],
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let modifier = super::loot_cmd::resource_or_inline(args, "modifier")?;
    let mut selected = 0usize;
    let mut tracker = Tracker::<Component>::default();
    for target in targets {
        let Some(id) = target.entity else {
            continue;
        };
        let here: Vec<EntitySlot> = slots
            .iter()
            .filter_map(|&i| entity_slot(&env.level.entities, id, i))
            .collect();
        let mut changed = 0i32;
        for slot in here {
            let stack = slot_get(&env.level.entities, id, slot);
            if stack.is_empty() {
                continue;
            }
            selected = selected.saturating_add(1);
            let result = super::loot_cmd::apply_modifier(env, source, &modifier, &stack)?;
            if slot_set(&mut env.level.entities, id, slot, result) {
                changed = changed.saturating_add(1);
            }
        }
        if changed > 0 {
            sync_inventory(&mut env.level.entities, id);
        }
        tracker.track(&target.display(), changed);
    }
    if selected == 0 {
        return Err(error(
            "commands.item.target.no_such_slot",
            vec![ErrArg::Str(slots_name.to_owned())],
        ));
    }
    tracker.feedback(
        source,
        env,
        true,
        false,
        Some(error("commands.item.target.failed", vec![])),
        |one, n, total| match one {
            Some(entity) => Component::translatable_args(
                "commands.item.entity.modify.success.single",
                vec![Arg::Int(total), Arg::from(entity.clone())],
            ),
            None => Component::translatable_args(
                "commands.item.entity.modify.success.multiple",
                vec![Arg::Int(n)],
            ),
        },
    )
}

// --- enchant ------------------------------------------------------------------------------------

/// Enchantments of an `ItemEnchantments` wire value: `(id, level)`.
fn read_enchantments(data: &[u8]) -> Vec<(u32, i32)> {
    let mut r = Reader::new(data);
    let mut out = Vec::new();
    let Ok(n) = r.varint() else {
        return out;
    };
    for _ in 0..n.clamp(0, 256) {
        let (Ok(id), Ok(level)) = (r.varint(), r.varint()) else {
            break;
        };
        if let Ok(id) = u32::try_from(id) {
            out.push((id, level));
        }
    }
    out
}

/// `Enchantment.getFullname`: the description in red (curses) or gray, with the level.
fn full_name(
    env: &Env<'_>,
    name: &str,
    description: &Component,
    max_level: i32,
    level: i32,
) -> Component {
    let curse = env
        .global
        .systems
        .in_tag("minecraft:enchantment", "minecraft:curse", name);
    let mut out = description.clone();
    if out.style.color.is_none() {
        out.style.color = Some(Color::Named(if curse {
            NamedColor::Red
        } else {
            NamedColor::Gray
        }));
    }
    if level != 1 || max_level != 1 {
        out = out
            .append(Component::text(" "))
            .append(Component::translatable(
                format!("enchantment.level.{level}"),
                vec![],
            ));
    }
    out
}

/// `/enchant <targets> <enchantment> [<level>]`.
pub(crate) fn enchant(
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let selector = args.selector("targets").ok_or_else(|| missing("targets"))?;
    let targets = select::many(env, source, selector)?;
    let name = args
        .identifier("enchantment")
        .ok_or_else(|| missing("enchantment"))?
        .clone();
    let game = env.global.game();
    let Some((id, info)) = game
        .enchantments
        .get(&name)
        .and_then(|&id| Some((id, game.enchanting.get(id)?)))
    else {
        return Err(error(
            "argument.resource.not_found",
            vec![
                ErrArg::Str(name),
                ErrArg::Str("minecraft:enchantment".to_owned()),
            ],
        ));
    };
    let level = args.int("level").copied().unwrap_or(1);
    if level > info.max_level {
        return Err(error(
            "commands.enchant.failed.level",
            vec![ErrArg::Int(level), ErrArg::Int(info.max_level)],
        ));
    }
    let single = targets.len() == 1;
    let book = ITEM.id("minecraft:enchanted_book");
    let stored = DATA_COMPONENT_TYPE.id("minecraft:stored_enchantments");
    let mut tracker = Tracker::<Component>::default();
    for target in &targets {
        let display = target.display();
        if target.kind.info().attributes.is_empty() {
            if single {
                return Err(error(
                    "commands.enchant.failed.entity",
                    vec![ErrArg::Str(display.plain())],
                ));
            }
            continue;
        }
        // Only players of this world hold items.
        let entities = &mut env.level.entities;
        let Some(player) = target.entity.and_then(|id| entities.player_mut(id)) else {
            if single && target.entity.is_some() {
                return Err(error(
                    "commands.enchant.failed.itemless",
                    vec![ErrArg::Str(display.plain())],
                ));
            }
            continue;
        };
        let Some(stack) = player
            .inventory
            .selected_item_mut()
            .filter(|s| !s.is_empty())
        else {
            if single {
                return Err(error(
                    "commands.enchant.failed.itemless",
                    vec![ErrArg::Str(display.plain())],
                ));
            }
            continue;
        };
        let component = if Some(stack.item) == book {
            stored.unwrap_or(component_ids::ENCHANTMENTS)
        } else {
            component_ids::ENCHANTMENTS
        };
        let existing = stack
            .component(component)
            .map(read_enchantments)
            .unwrap_or_default();
        // `EnchantmentHelper.isEnchantmentCompatible` and `canEnchant`.
        let compatible = existing
            .iter()
            .all(|&(other, _)| game.enchanting.compatible(other, id));
        if !game.enchanting.can_enchant(id, stack) || !compatible {
            if single {
                return Err(error(
                    "commands.enchant.failed.incompatible",
                    vec![ErrArg::Str(hover_name(stack).0.plain())],
                ));
            }
            continue;
        }
        let removed = stack.patch().is_some_and(|p| p.is_removed(component));
        if level > 0 && !removed {
            let mut levels = existing;
            levels.push((id, level.min(255)));
            let mut bytes = Vec::new();
            write_varint(&mut bytes, count_int(levels.len()));
            for (e, l) in levels {
                write_varint(&mut bytes, i32::try_from(e).unwrap_or(0));
                write_varint(&mut bytes, l);
            }
            stack.set_component(component, bytes.into());
        }
        if let Some(id) = target.entity {
            sync_inventory(entities, id);
        }
        tracker.track(&display, 1);
    }
    let description = info
        .description
        .clone()
        .unwrap_or_else(|| Component::text(""));
    let full = full_name(env, &name, &description, info.max_level, level);
    tracker.feedback(
        source,
        env,
        true,
        false,
        Some(error("commands.enchant.failed", vec![])),
        |one, n, _| match one {
            Some(entity) => Component::translatable_args(
                "commands.enchant.success.single",
                vec![Arg::from(full), Arg::from(entity.clone())],
            ),
            None => Component::translatable_args(
                "commands.enchant.success.multiple",
                vec![Arg::from(full), Arg::Int(n)],
            ),
        },
    )
}
