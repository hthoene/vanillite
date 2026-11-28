//! `Item.use` of the utility items that act right away or start being used: written,
//! writable and knowledge books, empty maps, goat horns (`InstrumentItem`), spyglasses and
//! bundles. The cool-down check of `ServerPlayerGameMode.useItem` comes first.

use vanillite_base::Vec3;
use vanillite_base::random::RandomSource;
use vanillite_config::GameMode;
use vanillite_data::item::ids as items;
use vanillite_protocol::codec::Reader;
use vanillite_protocol::components::ids;
use vanillite_protocol::packets::play::clientbound::{OpenBook, SoundSource};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::EntityId;

use crate::entities::Entities;
use crate::item::{self, ItemStack};
use crate::level::Level;
use crate::stats::{Stat, StatType};
use crate::survival::player::Env;
use crate::survival::{Effect, play_sound, sound_seed};

/// One `minecraft:instrument` entry (`Instrument`).
#[derive(Clone, Debug, PartialEq)]
pub struct Instrument {
    /// `sound_event` id.
    pub sound: String,
    /// `use_duration` in seconds.
    pub use_duration: f32,
    /// `range` in blocks.
    pub range: f32,
    /// `durability_damage`.
    pub durability_damage: i32,
}

impl Instrument {
    /// Reads a registry entry.
    pub fn from_json(json: &serde_json::Value) -> Option<Instrument> {
        let sound = match json.get("sound_event")? {
            serde_json::Value::String(s) => s.clone(),
            other => other.get("sound_id")?.as_str()?.to_owned(),
        };
        #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
        Some(Instrument {
            sound,
            use_duration: json.get("use_duration")?.as_f64()? as f32,
            range: json.get("range")?.as_f64()? as f32,
            durability_damage: json
                .get("durability_damage")
                .and_then(serde_json::Value::as_i64)
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(0),
        })
    }
}

/// The `instrument` of a stack: a registry entry or a direct one; a goat horn without a patch
/// has its default `ponder_goat_horn`.
pub fn instrument(stack: &ItemStack) -> Option<Instrument> {
    let Some(value) = stack.component(ids::INSTRUMENT) else {
        if stack.item != vanillite_data::item::ids::GOAT_HORN.0
            || stack.patch().is_some_and(|p| p.is_removed(ids::INSTRUMENT))
        {
            return None;
        }
        let data = super::play::game_data()?;
        let id = *data
            .holder_set("minecraft:instrument", "minecraft:ponder_goat_horn")
            .first()?;
        return data.instruments.get(usize::try_from(id).ok()?).cloned();
    };
    let mut r = Reader::new(value);
    let id = r.varint().ok()?;
    if id != 0 {
        let data = super::play::game_data()?;
        return data
            .instruments
            .get(usize::try_from(id.checked_sub(1)?).ok()?)
            .cloned();
    }
    // A direct sound event holder: an id + 1, or 0 and the event itself.
    let sound_id = r.varint().ok()?;
    let sound = if sound_id == 0 {
        r.string(32767).ok()?.to_owned()
    } else {
        vanillite_data::registry::SOUND_EVENT
            .name(u16::try_from(sound_id.checked_sub(1)?).ok()?)?
            .to_owned()
    };
    if sound_id == 0 && r.u8().ok()? != 0 {
        r.f32().ok()?;
    }
    let use_duration = r.f32().ok()?;
    let range = r.f32().ok()?;
    let durability_damage = r.varint().ok()?;
    Some(Instrument {
        sound,
        use_duration,
        range,
        durability_damage,
    })
}

/// `InstrumentItem.getUseDuration`: `floor(use_duration * 20)`.
pub fn instrument_use_ticks(stack: &ItemStack) -> i32 {
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    instrument(stack).map_or(0, |i| (i.use_duration * 20.0).floor() as i32)
}

fn hand_index(ents: &Entities, player: EntityId, hand: Hand) -> usize {
    match hand {
        Hand::MainHand => ents.player(player).map_or(0, |p| p.inventory.selected),
        Hand::OffHand => crate::inventory::OFFHAND,
    }
}

fn held(ents: &Entities, player: EntityId, hand: Hand) -> ItemStack {
    let i = hand_index(ents, player, hand);
    ents.player(player)
        .map(|p| p.inventory.get(i).clone())
        .unwrap_or_default()
}

fn award_used(ents: &mut Entities, player: EntityId, item: u16) {
    if let Some(p) = ents.player_mut(player) {
        p.stats.award(Stat::of(StatType::Used, item), 1);
    }
}

/// `Player.drop(stack, thrownFromHand)`: thrown from the eyes in look direction
/// (`createItemStackToDrop(stack, false, thrownFromHand)`); a throw from the hand counts for
/// the drop statistics.
pub fn drop_from_player(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    stack: ItemStack,
    thrown: bool,
) {
    if stack.is_empty() {
        return;
    }
    let Some(e) = ents.store.get(player) else {
        return;
    };
    let pos = e.pos();
    let rot = e.rot();
    // `getEyeY() - 0.3F`.
    let eye_y = pos.y + crate::survival::player::eye_height(e.dims().height) - f64::from(0.3f32);
    let deg = std::f32::consts::PI / 180.0;
    let sin_x = vanillite_base::math::sin(f64::from(rot.x_rot * deg));
    let cos_x = vanillite_base::math::cos(f64::from(rot.x_rot * deg));
    let sin_y = vanillite_base::math::sin(f64::from(rot.y_rot * deg));
    let cos_y = vanillite_base::math::cos(f64::from(rot.y_rot * deg));
    // The throw draws from the player's own random.
    let Some((dir, pow2, spread)) = ents.player_mut(player).map(|p| {
        let r = &mut p.survival.random;
        let dir = r.next_float() * std::f32::consts::TAU;
        let pow2 = 0.02 * r.next_float();
        (dir, pow2, r.next_float() - r.next_float())
    }) else {
        return;
    };
    let velocity = Vec3::new(
        f64::from(-sin_y * cos_x * 0.3) + f64::from(dir).cos() * f64::from(pow2),
        f64::from(-sin_x * 0.3 + 0.1 + spread * 0.1),
        f64::from(cos_y * cos_x * 0.3) + f64::from(dir).sin() * f64::from(pow2),
    );
    if thrown && let Some(p) = ents.player_mut(player) {
        p.stats
            .award(Stat::of(StatType::Dropped, stack.item), stack.count);
        p.stats.award(crate::stats::custom::DROP, 1);
    }
    let (uuid, y_rot, _, item_random) = crate::mob::new_item_entity();
    env.out.push(Effect::DropItem {
        random: item_random,
        uuid,
        pos: Vec3::new(pos.x, eye_y, pos.z),
        velocity,
        y_rot,
        pickup_delay: 40,
        stack,
    });
}

/// `Inventory.add(stack)`, else `drop(stack, false)`.
fn give(ents: &mut Entities, env: &mut Env<'_>, player: EntityId, mut stack: ItemStack) {
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    let added = ents
        .player_mut(player)
        .is_some_and(|p| p.inventory.add(None, &mut stack, creative));
    if !added || !stack.is_empty() {
        drop_from_player(ents, env, player, stack, false);
    }
}

/// `Item.use` of the utility items; `true` when the item was one of them. Items used over
/// time (goat horns, spyglasses, bundles) start being used here.
pub fn use_item(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &vanillite_world::entity::NetworkIds,
    player: EntityId,
    hand: Hand,
) -> bool {
    let stack = held(ents, player, hand);
    if stack.is_empty() {
        return false;
    }
    let Some(p) = ents.player(player) else {
        return false;
    };
    let creative = p.game_mode == GameMode::Creative;
    let id = item::item_id(&stack);
    let pos = ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos());
    if id == items::WRITTEN_BOOK || id == items::WRITABLE_BOOK {
        // `openItemGui`: a written book (resolved first) opens on the client.
        if stack.component(ids::WRITTEN_BOOK_CONTENT).is_some()
            && let Some(p) = ents.player_mut(player)
        {
            if crate::book::needs_resolving(&stack) {
                p.connection
                    .chat
                    .requests
                    .push(super::ChatRequest::OpenBook(hand));
            } else {
                p.connection.sink.send(&OpenBook { hand });
            }
        }
        award_used(ents, player, stack.item);
        return true;
    }
    if id == items::KNOWLEDGE_BOOK {
        use_knowledge_book(ents, player, hand, &stack, creative);
        return true;
    }
    if id == items::MAP {
        use_empty_map(level, ents, env, player, hand, &stack, creative, pos);
        return true;
    }
    if id == items::SPYGLASS {
        play_sound(
            env.out,
            Some(player),
            pos,
            "minecraft:item.spyglass.use",
            SoundSource::Players,
            1.0,
            1.0,
            sound_seed(),
        );
        award_used(ents, player, stack.item);
        super::using::start(ents, env, network_ids, player, hand);
        return true;
    }
    if instrument(&stack).is_some() {
        use_instrument(ents, env, network_ids, player, hand, &stack, creative, pos);
        return true;
    }
    if crate::bundle::has_contents(&stack) {
        super::using::start(ents, env, network_ids, player, hand);
        return true;
    }
    false
}

/// `KnowledgeBookItem.use`: consumed; its recipes are unlocked when all exist.
fn use_knowledge_book(
    ents: &mut Entities,
    player: EntityId,
    hand: Hand,
    stack: &ItemStack,
    creative: bool,
) {
    // `recipes`: an NBT list of recipe ids.
    let mut recipes = Vec::new();
    if let Some(value) = stack.component(ids::RECIPES) {
        use vanillite_protocol::codec::DecodeLimited;
        let tag = vanillite_protocol::types::Nbt::decode_limited(&mut Reader::new(value), 1 << 21)
            .ok()
            .and_then(|n| n.0);
        if let Some(vanillite_nbt::Tag::List(list)) = tag {
            for t in &list.0 {
                if let vanillite_nbt::Tag::String(id) = t {
                    recipes.push(if id.contains(':') {
                        id.clone()
                    } else {
                        format!("minecraft:{id}")
                    });
                }
            }
        }
    }
    let i = hand_index(ents, player, hand);
    let Some(p) = ents.player_mut(player) else {
        return;
    };
    // `consume(1, player)`.
    if !creative && let Some(s) = p.inventory.get_mut(i) {
        s.shrink(1);
    }
    if recipes.is_empty() {
        return;
    }
    let Some(data) = super::play::game_data() else {
        return;
    };
    if let Some(missing) = recipes.iter().find(|r| !data.recipes.book.contains(r)) {
        log::error!("Invalid recipe: {missing}");
        return;
    }
    let refs: Vec<&str> = recipes.iter().map(String::as_str).collect();
    p.unlock_recipes(&refs);
    p.stats.award(Stat::of(StatType::Used, stack.item), 1);
}

/// `EmptyMapItem.use`: a new map centred on the player; it replaces the last empty map in the
/// hand, else goes to the inventory.
#[allow(clippy::too_many_arguments)]
fn use_empty_map(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    hand: Hand,
    stack: &ItemStack,
    creative: bool,
    pos: Vec3,
) {
    let Some(maps) = level.maps() else {
        return;
    };
    let i = hand_index(ents, player, hand);
    let left = {
        let Some(p) = ents.player_mut(player) else {
            return;
        };
        if !creative && let Some(s) = p.inventory.get_mut(i) {
            s.shrink(1);
        }
        p.stats.award(Stat::of(StatType::Used, stack.item), 1);
        p.inventory.get(i).clone()
    };
    crate::survival::play_entity_sound(
        env.out,
        ents,
        None,
        player,
        "minecraft:ui.cartography_table.take_result",
        SoundSource::Players,
        1.0,
        1.0,
    );
    let at = crate::player::play::block_pos_of(pos);
    let map = crate::map::create(maps, at.x, at.z, 0, true, false, env.dimension.to_owned());
    if left.is_empty() {
        if let Some(p) = ents.player_mut(player) {
            p.inventory.set(i, map);
        }
    } else {
        give(ents, env, player, map);
    }
}

/// `InstrumentItem.use`: the horn sounds for everyone else (the player hears its own), the
/// item cools down for its use duration.
#[allow(clippy::too_many_arguments)]
fn use_instrument(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &vanillite_world::entity::NetworkIds,
    player: EntityId,
    hand: Hand,
    stack: &ItemStack,
    creative: bool,
    pos: Vec3,
) {
    let Some(instrument) = instrument(stack) else {
        return;
    };
    super::using::start(ents, env, network_ids, player, hand);
    crate::survival::play_entity_sound(
        env.out,
        ents,
        Some(player),
        player,
        &instrument.sound,
        SoundSource::Records,
        instrument.range / 16.0,
        1.0,
    );
    let i = hand_index(ents, player, hand);
    let Some(p) = ents.player_mut(player) else {
        return;
    };
    p.survival.game_events.push((
        crate::level::game_event::events().instrument_play,
        pos,
        None,
    ));
    // `hurtAndBreak(durabilityDamage, player, hand)`.
    let change = if instrument.durability_damage > 0 {
        crate::durability::change(
            p.inventory.get(i),
            instrument.durability_damage,
            creative,
            env.random,
        )
    } else {
        0
    };
    let mut broke = false;
    if change != 0
        && let Some(s) = p.inventory.get_mut(i)
    {
        let d = item::damage(s).saturating_add(change);
        broke = crate::advancements::view::apply_damage(
            Some(&mut p.advancements),
            Some(&mut p.stats),
            s,
            d,
        );
    }
    if instrument.use_duration > 0.0 {
        #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
        let ticks = (instrument.use_duration * 20.0).floor() as i32;
        let sink = std::sync::Arc::clone(&p.connection.sink);
        p.survival.cooldowns.add(stack, ticks, sink.as_ref());
    }
    p.stats.award(Stat::of(StatType::Used, stack.item), 1);
    if broke {
        crate::durability::broken_in_env(ents, env, player, crate::player::using::hand_slot(hand));
    }
}

/// `BundleItem.onUseTick`: the first tick, then every second tick after ten, one stack drops
/// out. `remaining` is Vanilla's `ticksRemaining`.
pub fn bundle_use_tick(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    hand: Hand,
    remaining: i32,
    duration: i32,
) {
    let first = remaining == duration;
    if !(first || (remaining < duration.saturating_sub(10) && remaining % 2 == 0)) {
        return;
    }
    let i = hand_index(ents, player, hand);
    let Some(p) = ents.player_mut(player) else {
        return;
    };
    let selection = p.inventory.bundle_selection;
    let Some(bundle) = p.inventory.get_mut(i) else {
        return;
    };
    let Some(mut contents) = crate::bundle::Contents::of(bundle, selection) else {
        return;
    };
    let Some(removed) = contents.remove_one() else {
        return;
    };
    let keep = contents.store(bundle);
    let bundle_item = bundle.item;
    p.inventory.bundle_selection = keep;
    let pos = ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos());
    // `playRemoveOneSound`: for the players around.
    let pitch = 0.8 + env.random.next_float() * 0.4;
    play_sound(
        env.out,
        Some(player),
        pos,
        "minecraft:item.bundle.remove_one",
        SoundSource::Players,
        0.8,
        pitch,
        sound_seed(),
    );
    drop_from_player(ents, env, player, removed, true);
    // `playDropContentsSound` at the block position, for everyone.
    let at = crate::player::play::block_pos_of(pos);
    let pitch = 0.8 + env.random.next_float() * 0.4;
    play_sound(
        env.out,
        None,
        Vec3::new(
            f64::from(at.x) + 0.5,
            f64::from(at.y) + 0.5,
            f64::from(at.z) + 0.5,
        ),
        "minecraft:item.bundle.drop_contents",
        SoundSource::Players,
        0.8,
        pitch,
        sound_seed(),
    );
    award_used(ents, player, bundle_item);
}
