//! The merchant side of villagers and wandering traders (`AbstractVillager` and the trading
//! parts of `Villager`): offers from the trade sets, special prices, restocking, trade
//! experience and levels, starting a trade, and the merchant NBT (`Offers`, `Inventory`).
//!
//! Two contexts reach a merchant: its own tick ([`Tick`]) and the merchant menu of the trading
//! player (the [`Level`] with the entities in it). Both generate the offers on first use
//! (`getOffers`) from the trade sets with the set's random sequence.

use vanillite_base::Vec3;
use vanillite_base::random::RandomSource;
use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::packets::play::clientbound::{MerchantOffers as OffersPacket, SoundSource};
use vanillite_world::entity::{EntityId, Pose};

use super::{Merchant, state, state_mut};
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::fam::Interact;
use crate::mob::{Kind, MobData, Tick};
use crate::trading::MerchantOffers;

/// `getAmbientSoundInterval` of villagers and wandering traders (the `Mob` default).
const AMBIENT_INTERVAL: i32 = 80;
/// `VillagerData.NEXT_LEVEL_XP_THRESHOLDS`.
const LEVEL_XP: [i32; 5] = [0, 10, 70, 150, 250];

// --- NBT ------------------------------------------------------------------------------------

/// `addAdditionalSaveData` of `AbstractVillager`: `Offers` and the inventory.
pub fn save(m: &Merchant, out: &mut Compound) {
    if let Some(offers) = &m.offers {
        out.insert("Offers", crate::trading::nbt::offers_to_nbt(offers));
    }
    let items: Vec<Tag> = m
        .inventory
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| Tag::Compound(crate::item_nbt::stack_to_nbt(s)))
        .collect();
    out.insert("Inventory", List(items));
}

/// `readAdditionalSaveData` of `AbstractVillager`.
pub fn load(m: &mut Merchant, tag: &Compound) {
    m.offers = match tag.get("Offers") {
        Some(Tag::Compound(c)) => Some(crate::trading::nbt::offers_from_nbt(c)),
        _ => None,
    };
    if let Some(Tag::List(list)) = tag.get("Inventory") {
        // `fromItemList`: cleared, then each stack added.
        m.inventory = Default::default();
        for c in list.0.iter().filter_map(Tag::as_compound) {
            let stack = crate::item_nbt::stack_from_nbt(c);
            add_item(m, stack);
        }
    }
}

/// The trading fields of `Villager`'s NBT: `Xp`, `LastRestock`, `RestocksToday`.
pub fn save_villager(m: &Merchant, out: &mut Compound) {
    out.insert("Xp", m.villager_xp);
    out.insert("LastRestock", m.last_restock);
    out.insert("RestocksToday", m.restocks_today);
}

/// Reads [`save_villager`].
pub fn load_villager(m: &mut Merchant, tag: &Compound) {
    let int = |key: &str| {
        tag.get(key)
            .and_then(Tag::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0)
    };
    m.villager_xp = int("Xp");
    m.restocks_today = int("RestocksToday");
    m.last_restock = tag.get("LastRestock").and_then(Tag::as_i64).unwrap_or(0);
}

// --- Inventory (`SimpleContainer(8)`) -----------------------------------------------------------

/// `getMaxStackSize(stack)` of a `SimpleContainer`.
fn slot_max(stack: &ItemStack) -> i32 {
    crate::menu::CONTAINER_MAX.min(crate::item::max_stack_size(stack))
}

/// `SimpleContainer.addItem`: the rest that did not fit.
pub fn add_item(m: &mut Merchant, stack: ItemStack) -> ItemStack {
    if stack.is_empty() {
        return ItemStack::EMPTY;
    }
    let mut rest = stack;
    for slot in &mut m.inventory {
        if !slot.is_empty() && slot.same_item_same_components(&rest) {
            let room = (slot_max(slot) - slot.count).max(0);
            let moved = rest.count.min(room);
            if moved > 0 {
                slot.grow(moved);
                rest.shrink(moved);
            }
            if rest.is_empty() {
                return ItemStack::EMPTY;
            }
        }
    }
    if let Some(slot) = m.inventory.iter_mut().find(|s| s.is_empty()) {
        *slot = std::mem::take(&mut rest);
    }
    rest
}

/// `SimpleContainer.canAddItem`.
pub fn can_add_item(m: &Merchant, stack: &ItemStack) -> bool {
    m.inventory.iter().any(|s| {
        s.is_empty()
            || (s.same_item_same_components(stack) && s.count < crate::item::max_stack_size(s))
    })
}

// --- Offers -------------------------------------------------------------------------------------

/// The trade sets the merchant's offers come from (`updateTrades`).
fn trade_sets(m: &MobData) -> Vec<String> {
    match m.kind {
        // `WanderingTrader.updateTrades`.
        Kind::WanderingTrader => vec![
            "minecraft:wandering_trader/buying".into(),
            "minecraft:wandering_trader/uncommon".into(),
            "minecraft:wandering_trader/common".into(),
        ],
        _ => {
            let v = &state(m).villager;
            let profession = vanillite_data::registry::VILLAGER_PROFESSION
                .name(v.profession)
                .unwrap_or("minecraft:none");
            let path = profession.strip_prefix("minecraft:").unwrap_or(profession);
            if matches!(path, "none" | "nitwit") || !(1..=5).contains(&v.level) {
                Vec::new()
            } else {
                vec![format!("minecraft:{path}/level_{}", v.level)]
            }
        }
    }
}

/// `addOffersFromTradeSet(level, offers, set)` for the merchant `m` at `pos`.
fn add_offers_from_set(
    level: &Level,
    m: &MobData,
    id: EntityId,
    pos: Vec3,
    key: &str,
    offers: &mut MerchantOffers,
) {
    let Some(data) = crate::player::play::game_data() else {
        return;
    };
    let Some(set) = data.trades.sets.get(key) else {
        log::debug!("Missing expected trade set {key}");
        return;
    };
    static TYPES: std::sync::OnceLock<crate::survival::damage::DamageTypes> =
        std::sync::OnceLock::new();
    let types = TYPES.get_or_init(|| crate::survival::damage::DamageTypes::new(Vec::new()));
    let facts = crate::predicate::EntityFacts {
        kind: Some(m.kind.type_id()),
        baby: m.baby,
        on_fire: m.is_on_fire(),
        villager_variant: (m.kind == Kind::Villager).then(|| state(m).villager.villager_type),
        ..Default::default()
    };
    let no_level = |_: &str| 0;
    let entity = crate::loot::EntityLootContext {
        this: facts,
        attacker: None,
        direct_attacker: None,
        last_damage_player: None,
        source: None,
        types,
        tags: &*data,
        attacker_enchantment_level: &no_level,
    };
    let in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let holder_set = |registry: &str, entry: &str| data.holder_set(registry, entry);
    // `exploration_map` of the cartographer's map trades, from the merchant's position.
    let generation = level.generation.clone();
    let maps = level.maps_shared();
    let refs = std::sync::Arc::clone(level.structure_refs());
    let (min_y, height) = (level.min_y(), level.max_y() - level.min_y() + 1);
    let sea_level = generation.as_ref().map_or(63, |g| g.terrain().sea_level());
    let explore = |request: &crate::loot::ExplorationMap, origin: Vec3| {
        crate::map::exploration_map(
            generation.as_ref()?,
            maps.as_deref()?,
            min_y,
            height,
            sea_level,
            request,
            origin,
            &refs,
        )
    };
    let ctx = crate::loot::LootContext {
        origin: Some(pos),
        this_entity: Some(id),
        exploration_map: Some(&explore),
        item_in_tag: &in_tag,
        enchanting: Some(&data.enchanting),
        holder_set: &holder_set,
        this_present: true,
        entity: Some(&entity),
        ..crate::loot::LootContext::empty()
    };
    let sequence = set.random_sequence.as_deref().unwrap_or(key);
    level.with_random_sequence(sequence, |rng| {
        let mut eval = crate::loot::LootEval {
            tables: &data.loot,
            ctx: &ctx,
            rng,
            additional_cost_allowed: true,
        };
        data.trades.add_offers(set, &mut eval, &holder_set, offers);
    });
}

/// `updateTrades`: the offers of the merchant's trade sets added to `offers`.
fn add_trades(level: &Level, m: &MobData, id: EntityId, pos: Vec3, offers: &mut MerchantOffers) {
    for key in trade_sets(m) {
        add_offers_from_set(level, m, id, pos, &key, offers);
    }
}

/// `getOffers` on the merchant in the level: generated on first use.
pub(crate) fn ensure_offers_in(level: &Level, ents: &mut Entities, id: EntityId) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = ents.mob(id) else {
        return;
    };
    if state(m).merchant.offers.is_some() {
        return;
    }
    let mut offers = MerchantOffers::default();
    add_trades(level, m, id, pos, &mut offers);
    if let Some(m) = ents.mob_mut(id) {
        let villager = m.kind == Kind::Villager;
        let s = state_mut(m);
        s.merchant.offers = Some(offers);
        // `Villager.updateTrades`: the special prices of a trading player.
        if villager && s.merchant.trading_player.is_some() {
            update_special_prices(m);
        }
    }
}

/// `getOffers` of a merchant in the level (the merchant menu): generated on first use.
pub fn offers_in_level(level: &mut Level, id: EntityId) -> Option<&MerchantOffers> {
    let mut ents = std::mem::take(&mut level.entities);
    ensure_offers_in(level, &mut ents, id);
    level.entities = ents;
    level
        .entities
        .mob(id)
        .and_then(|m| state(m).merchant.offers.as_ref())
}

/// `getOffers` in the merchant's tick.
fn ensure_offers(t: &mut Tick<'_, '_>) {
    let id = t.id;
    ensure_offers_in(t.level, t.ents, id);
}

/// The `merchant_offers` packet of the merchant's current offers for a menu
/// (`sendMerchantOffers`).
pub fn offers_packet(level: &mut Level, id: EntityId, container_id: i32) -> Option<OffersPacket> {
    let offers = crate::trading::nbt::to_wire(offers_in_level(level, id)?);
    let m = level.entities.mob(id)?;
    let villager = m.kind == Kind::Villager;
    let s = state(m);
    Some(OffersPacket {
        container_id,
        offers,
        villager_level: if villager { s.villager.level.max(1) } else { 1 },
        villager_xp: if villager { s.merchant.villager_xp } else { 0 },
        show_progress: villager,
        can_restock: villager,
    })
}

/// `Villager.updateSpecialPrices(player)` with the trading player's reputation and Hero of the
/// Village amplifier; whether the trading player must get the offers again.
fn update_special_prices(m: &mut MobData) -> bool {
    if m.kind != Kind::Villager {
        return false;
    }
    let uuid = state(m).merchant.trading_uuid;
    let reputation = super::villager::player_reputation(m, uuid);
    let s = state_mut(m);
    let hero = s
        .merchant
        .trading_hero
        .map_or(0.0, |amp| f64::from(0.3f32 + 0.0625f32 * amp as f32));
    let Some(offers) = s.merchant.offers.as_mut() else {
        return false;
    };
    for offer in &mut offers.0 {
        offer.reset_special_price_diff();
    }
    for offer in &mut offers.0 {
        if reputation != 0 {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let diff = (reputation as f32 * offer.price_multiplier).floor() as i32;
            offer.add_to_special_price_diff(-diff);
        }
        if hero > 0.0 {
            #[allow(clippy::cast_possible_truncation)]
            let reduction = (hero * f64::from(offer.base_cost_a.count)).floor() as i32;
            offer.add_to_special_price_diff(-reduction.max(1));
        }
    }
    s.merchant.trading_player.is_some() && !offers.0.is_empty()
}

/// The UUID and Hero of the Village amplifier of a player.
fn player_facts(ents: &Entities, player: EntityId) -> Option<(u128, Option<i32>)> {
    let uuid = ents.store.get(player)?.uuid();
    let hero = ents.player(player).and_then(|p| {
        p.survival
            .effects
            .amplifier(crate::effect::MobEffect::HERO_OF_THE_VILLAGE)
    });
    Some((uuid, hero))
}

/// Asks the open merchant menu of `player` to update its result and get the offers again.
fn resend_offers(ents: &mut Entities, player: EntityId) {
    if let Some(p) = ents.player_mut(player)
        && let Some(menu) = p.inventory.container.as_deref_mut()
        && menu.kind == crate::menu::container::MenuKind::Merchant
    {
        menu.merchant.update_sell = true;
        menu.merchant.send_offers = true;
    }
}

/// `updateSpecialPrices` in the merchant's tick (the player's facts refreshed).
fn update_prices_in_tick(t: &mut Tick<'_, '_>) {
    let Some(player) = state(t.mob_ref()).merchant.trading_player else {
        return;
    };
    if let Some((uuid, hero)) = player_facts(t.ents, player) {
        let s = state_mut(t.mob());
        s.merchant.trading_uuid = uuid;
        s.merchant.trading_hero = hero;
    }
    if update_special_prices(t.mob()) {
        resend_offers(t.ents, player);
    }
}

// --- Restocking ---------------------------------------------------------------------------------

/// `Villager.shouldRestock(level)`.
pub fn should_restock(t: &mut Tick<'_, '_>) -> bool {
    ensure_offers(t);
    let game_time = t.game_time();
    let day = t.cx.env.day_count;
    let s = &mut state_mut(t.mob()).merchant;
    let mut new_day = game_time > s.last_restock + 12000;
    new_day |= s.last_restock_check_day > 0 && day > s.last_restock_check_day;
    s.last_restock_check_day = day;
    if new_day {
        s.last_restock = game_time;
        // `resetNumberOfRestocks`: `catchUpDemand`, then no restocks today.
        catch_up_demand(t);
        state_mut(t.mob()).merchant.restocks_today = 0;
    }
    let s = &state(t.mob_ref()).merchant;
    let allowed =
        s.restocks_today == 0 || (s.restocks_today < 2 && game_time > s.last_restock + 2400);
    allowed
        && s.offers
            .as_ref()
            .is_some_and(|o| o.0.iter().any(crate::trading::MerchantOffer::needs_restock))
}

/// `Villager.catchUpDemand`.
fn catch_up_demand(t: &mut Tick<'_, '_>) {
    let s = &mut state_mut(t.mob()).merchant;
    let missed = 2 - s.restocks_today;
    if let Some(offers) = s.offers.as_mut() {
        if missed > 0 {
            for o in &mut offers.0 {
                o.reset_uses();
            }
        }
        for _ in 0..missed.max(0) {
            for o in &mut offers.0 {
                o.update_demand();
            }
        }
    }
    update_prices_in_tick(t);
}

/// `Villager.restock()`.
pub fn restock(t: &mut Tick<'_, '_>) {
    ensure_offers(t);
    let game_time = t.game_time();
    let s = &mut state_mut(t.mob()).merchant;
    if let Some(offers) = s.offers.as_mut() {
        for o in &mut offers.0 {
            o.update_demand();
        }
        for o in &mut offers.0 {
            o.reset_uses();
        }
    }
    update_prices_in_tick(t);
    let s = &mut state_mut(t.mob()).merchant;
    s.last_restock = game_time;
    s.restocks_today += 1;
}

/// `Villager.updateTrades` after a profession or level change (the offers of the new level
/// added; `None` offers are generated on first use anyway).
pub fn update_trades(t: &mut Tick<'_, '_>) {
    if state(t.mob_ref()).merchant.offers.is_none() {
        ensure_offers(t);
        return;
    }
    let (id, pos) = (t.id, t.pos());
    let mut offers = state_mut(t.mob())
        .merchant
        .offers
        .take()
        .unwrap_or_default();
    add_trades(t.level, t.mob_ref(), id, pos, &mut offers);
    state_mut(t.mob()).merchant.offers = Some(offers);
    update_prices_in_tick(t);
}

/// `AbstractVillager.stopTrading` (also when the villager dies or teleports).
pub fn stop_trading(t: &mut Tick<'_, '_>) {
    state_mut(t.mob()).merchant.trading_player = None;
}

/// `stopTrading` from the merchant menu (`MerchantMenu.removed`).
pub fn stop_trading_in_level(level: &mut Level, id: EntityId) {
    if let Some(m) = level.entities.mob_mut(id) {
        state_mut(m).merchant.trading_player = None;
    }
}

/// Whether a merchant still trades and lives (`stillValid` without the distance).
pub fn still_trading(level: &Level, id: EntityId) -> bool {
    level
        .entities
        .mob(id)
        .is_some_and(|m| m.is_alive() && state(m).merchant.trading_player.is_some())
}

// --- Trading in the menu -------------------------------------------------------------------------

/// `makeSound(sound)` of a merchant in the level (volume 1, the voice pitch).
fn sound_in_level(level: &mut Level, id: EntityId, sound: &str) {
    let Some(pos) = level.entities.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = level.entities.mob_mut(id) else {
        return;
    };
    if m.silent {
        return;
    }
    let volume = crate::mob::fam::mob_sound_volume(m);
    let a = m.random.next_float();
    let b = m.random.next_float();
    let pitch = if m.baby {
        (a - b) * 0.2 + 1.5
    } else {
        (a - b) * 0.2 + 1.0
    };
    let sound = vanillite_data::registry::SOUND_EVENT.id(sound).unwrap_or(0);
    level.play_sound(
        None,
        pos.x,
        pos.y,
        pos.z,
        sound,
        crate::level::SOUND_NEUTRAL,
        volume,
        pitch,
    );
}

/// `getTradeUpdatedSound(valid)`.
fn trade_updated_sound(kind: Kind, valid: bool) -> &'static str {
    match (kind, valid) {
        (Kind::WanderingTrader, true) => "minecraft:entity.wandering_trader.yes",
        (Kind::WanderingTrader, false) => "minecraft:entity.wandering_trader.no",
        (_, true) => "minecraft:entity.villager.yes",
        (_, false) => "minecraft:entity.villager.no",
    }
}

/// `notifyTradeUpdated(result)`: a yes or no at most once a second.
pub fn notify_trade_updated_in_level(level: &mut Level, id: EntityId, result: &ItemStack) {
    let Some(m) = level.entities.mob_mut(id) else {
        return;
    };
    if m.ambient_sound_time <= -AMBIENT_INTERVAL + 20 {
        return;
    }
    m.ambient_sound_time = -AMBIENT_INTERVAL;
    let sound = trade_updated_sound(m.kind, !result.is_empty());
    sound_in_level(level, id, sound);
}

/// `notifyTrade(offer)` with `rewardTradeXp`: the offer is used, the merchant gains experience
/// (a villager may level up) and an orb pops. Returns whether the trading player must get the
/// offers again (a new level brought new offers).
pub fn notify_trade_in_level(
    level: &mut Level,
    id: EntityId,
    offer: usize,
    _player_pos: Vec3,
) -> bool {
    let Some(pos) = level.entities.store.get(id).map(|e| e.pos()) else {
        return false;
    };
    let Some(m) = level.entities.mob_mut(id) else {
        return false;
    };
    let kind = m.kind;
    let (xp, reward) = {
        let Some(o) = state_mut(m)
            .merchant
            .offers
            .as_mut()
            .and_then(|o| o.0.get_mut(offer))
        else {
            return false;
        };
        o.increase_uses();
        (o.xp, o.reward_exp)
    };
    m.ambient_sound_time = -AMBIENT_INTERVAL;
    let orb = Vec3::new(pos.x, pos.y + 0.5, pos.z);
    if kind == Kind::WanderingTrader {
        // `WanderingTrader.rewardTradeXp`.
        if reward {
            let pop = 3 + m.random.next_int_bounded(4);
            pop_orb(level, orb, pop);
        }
        return false;
    }
    // `Villager.rewardTradeXp`.
    let mut pop = 3 + m.random.next_int_bounded(4);
    let s = state_mut(m);
    s.merchant.villager_xp += xp;
    s.merchant.last_traded_player = s.merchant.trading_player;
    let lvl = s.villager.level;
    let level_up = (1..5).contains(&lvl)
        && usize::try_from(lvl)
            .ok()
            .and_then(|l| LEVEL_XP.get(l))
            .is_some_and(|&max| s.merchant.villager_xp >= max);
    let mut resend = false;
    if level_up {
        // `increaseMerchantCareer`: the next level and its offers, then regeneration.
        s.villager.level = lvl + 1;
        let mut offers = s.merchant.offers.take().unwrap_or_default();
        let mut ents = std::mem::take(&mut level.entities);
        if let Some(m) = ents.mob(id) {
            add_trades(level, m, id, pos, &mut offers);
        }
        if let Some(m) = ents.mob_mut(id) {
            state_mut(m).merchant.offers = Some(offers);
            resend = update_special_prices(m);
            crate::effect::living::add_to_mob(
                m,
                crate::effect::EffectInstance::new(crate::effect::MobEffect::REGENERATION, 200, 0),
            );
        }
        level.entities = ents;
        pop += 5;
    }
    if reward {
        pop_orb(level, orb, pop);
    }
    resend
}

/// `addFreshEntity(new ExperienceOrb(level, x, y, z, popXp))`: one orb of the whole value, not
/// split or merged like `ExperienceOrb.award`.
fn pop_orb(level: &mut Level, pos: Vec3, value: i32) {
    let (entities, network_ids, _) = level.entity_parts();
    crate::experience_orb::spawn(entities, network_ids, pos, Vec3::ZERO, value);
}

// --- Starting a trade ----------------------------------------------------------------------------

/// The title of the trading screen (`getDisplayName`): the custom name, else the profession
/// (`getTypeName`) or the type.
fn title(m: &MobData) -> vanillite_protocol::text::Component {
    if let Some(name) = m
        .custom_name
        .as_deref()
        .and_then(crate::mob::nbt::component_tag)
    {
        return vanillite_protocol::text::Component::from_nbt(&name);
    }
    let key = if m.kind == Kind::Villager {
        let profession = vanillite_data::registry::VILLAGER_PROFESSION
            .name(state(m).villager.profession)
            .unwrap_or("minecraft:none");
        let path = profession.strip_prefix("minecraft:").unwrap_or(profession);
        format!("entity.minecraft.villager.{path}")
    } else {
        m.kind.translation_key().to_owned()
    };
    vanillite_protocol::text::Component::translatable(key, Vec::new())
}

/// `setUnhappy`: the head shakes for two seconds with a no.
fn set_unhappy(ix: &mut Interact<'_, '_>) {
    let target = ix.target;
    if let Some(m) = ix.ents.mob_mut(target) {
        state_mut(m).merchant.unhappy_counter = 40;
    }
    crate::mob::combat::mob_sound(ix.ents, ix.env, target, "minecraft:entity.villager.no");
}

/// `Villager.mobInteract` / `WanderingTrader.mobInteract`: opens the trading screen.
pub fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
    let target = ix.target;
    let Some(m) = ix.ents.mob(target) else {
        return Interaction::Pass;
    };
    let kind = m.kind;
    let spawn_egg = ix.stack.item == vanillite_data::item::ids::VILLAGER_SPAWN_EGG.0;
    let sleeping = ix
        .ents
        .store
        .get(target)
        .is_some_and(|e| e.pose() == Pose::Sleeping);
    let trading = state(m).merchant.trading_player.is_some();
    if spawn_egg || !m.is_alive() || trading || (kind == Kind::Villager && sleeping) {
        return Interaction::Pass;
    }
    if m.baby {
        if kind == Kind::WanderingTrader {
            return Interaction::Pass;
        }
        set_unhappy(ix);
        return Interaction::Success;
    }
    ensure_offers_in(ix.level, ix.ents, target);
    let no_offers = ix.ents.mob(target).is_none_or(|m| {
        state(m)
            .merchant
            .offers
            .as_ref()
            .is_none_or(|o| o.0.is_empty())
    });
    if ix.hand == vanillite_protocol::packets::play::serverbound::Hand::MainHand {
        if no_offers && kind == Kind::Villager {
            set_unhappy(ix);
        }
        if let Some(p) = ix.ents.player_mut(ix.player) {
            p.stats.award(crate::stats::custom::TALKED_TO_VILLAGER, 1);
        }
    }
    if no_offers {
        return Interaction::Consume;
    }
    // `startTrading`: special prices, the trading player, the screen.
    let player = ix.player;
    let facts = player_facts(ix.ents, player);
    let Some(m) = ix.ents.mob_mut(target) else {
        return Interaction::Pass;
    };
    {
        let s = state_mut(m);
        if let Some((uuid, hero)) = facts {
            s.merchant.trading_uuid = uuid;
            s.merchant.trading_hero = hero;
        }
        s.merchant.trading_player = Some(player);
    }
    update_special_prices(m);
    let title = title(m);
    let pos = ix
        .ents
        .store
        .get(target)
        .map_or(Vec3::default(), |e| e.pos());
    ix.cx.menus.push(crate::menu::container::MenuRequest {
        player: Some(player),
        menu: crate::menu::container::MenuKind::Merchant,
        storage: crate::menu::container::Storage::Merchant(target),
        title,
        pos: vanillite_base::BlockPos::containing(pos.x, pos.y, pos.z),
        block: vanillite_data::block::BlockId(0),
    });
    Interaction::Success
}

/// `customServerAiStep` of a merchant: it stops trading without offers
/// (`(offers == null || offers.isEmpty()) && isTrading()`).
pub fn check_trading(t: &mut Tick<'_, '_>) {
    let s = &state(t.mob_ref()).merchant;
    if s.trading_player.is_some() && s.offers.as_ref().is_none_or(|o| o.0.is_empty()) {
        stop_trading(t);
    }
}

/// The sound of the merchant's ambient sound while trading (`getAmbientSound`).
pub fn ambient_sound(m: &MobData) -> &'static str {
    let trading = state(m).merchant.trading_player.is_some();
    match (m.kind, trading) {
        (Kind::WanderingTrader, true) => "minecraft:entity.wandering_trader.trade",
        (Kind::WanderingTrader, false) => "minecraft:entity.wandering_trader.ambient",
        (_, true) => "minecraft:entity.villager.trade",
        (_, false) => "minecraft:entity.villager.ambient",
    }
}

/// `SoundSource` of merchant sounds.
pub const SOURCE: SoundSource = SoundSource::Neutral;

/// Writes merchant offers as the `Offers` tag (`MerchantOffers.CODEC`); zombie villagers keep
/// the offers of the villager they were.
pub fn save_offers(offers: &crate::trading::MerchantOffers, out: &mut Compound) {
    out.insert("Offers", crate::trading::nbt::offers_to_nbt(offers));
}

/// Reads the `Offers` tag (`None` without one).
pub fn load_offers(tag: &Compound) -> Option<crate::trading::MerchantOffers> {
    match tag.get("Offers") {
        Some(vanillite_nbt::Tag::Compound(c)) => Some(crate::trading::nbt::offers_from_nbt(c)),
        _ => None,
    }
}
