//! Client-view parity (#196): what a scenario player's client receives, normalized per tick.
//!
//! Both sides deliver the raw clientbound play packets (packet id + body) a player got during a
//! tick: the Java recorder from the player's connection (`"packets"` in its frames, hex), the
//! scenario world from the player's packet sink. One [`ClientView`] per player turns them into
//! the events the player's client shows, so both go through the same normalization:
//!
//! - `add <type> <x> <y> <z>`: an entity appears (position rounded to 1/100 block);
//!   `remove <type>`: it goes away (`?` for an entity the client never got).
//! - `login <dimension>`, `respawn <dimension> <game mode> keep=<flags>`: the client's level
//!   (both forget every entity).
//! - `block <x> <y> <z> <state>`: a block change in view (`block_update` and
//!   `section_blocks_update` alike; the last state per position and tick).
//! - `game_event <id> <param>`, `level_event <id> <x> <y> <z> <data>`, `entity_event <type>
//!   <event>`.
//! - `boss#<n> <name> <progress> <color> <overlay> <flags>` or `boss#<n> removed`: a boss bar
//!   that changed during the tick, numbered in the order the client got them (their UUIDs are
//!   random).
//! - `abilities <flags> <flying speed> <walking speed>`, `held <slot>`, `slot <slot> <item>
//!   <count>` (`inv:<i>` of `set_player_inventory`, `<container>:<i>` of the container
//!   packets), `attr <attribute> <value>` (the player's own attributes from
//!   `update_attributes`: the value the client computes from base and modifiers, 4 decimals):
//!   the state after the tick where it changed.
//! - `<packet> <body hex>`: the scoreboard and team packets as sent (`set_objective`,
//!   `set_display_objective`, `set_score`, `reset_score`, `set_player_team`);
//!   `map_item_data <bytes> <hash>`: a map update by its size and FNV-1a hash of its body.
//!
//! Entity ids, packet order and batching within a tick, chunk data, light, keep-alives and
//! everything else are left out. So is the game mode the player joins in: the Java recorder
//! joins players in the world's creative mode and then sets the scenario's (a `game_event 3`
//! in the join tick, the previous game mode of later respawns), Vanillite's harness joins them
//! in it; the game mode after the join tick is compared (abilities, game events, respawns). A frame's view lists the events of every player as
//! `<player> <event>`, sorted.

use std::collections::{BTreeMap, HashMap};

use vanillite_protocol::Packet;
use vanillite_protocol::codec::Reader;
use vanillite_protocol::packets::play::clientbound as cb;

/// The client of one scenario player.
#[derive(Clone, Debug, Default)]
pub struct ClientView {
    /// Entity type by network id.
    entities: HashMap<i32, String>,
    /// Boss bars by UUID: ordinal and state (`None` once removed).
    bosses: Vec<(u128, Option<String>)>,
    abilities: Option<String>,
    held: Option<i32>,
    slots: BTreeMap<String, String>,
    /// Whether `update_attributes` counts (`client_attributes`).
    with_attributes: bool,
    /// The player's own network id (`login`).
    own_id: Option<i32>,
    /// The player's own attribute values by attribute name.
    attributes: BTreeMap<String, String>,
    /// Whether the client got its join tick.
    joined: bool,
}

/// The state a tick's changes are compared with.
#[derive(Default)]
struct Before {
    bosses: Vec<Option<String>>,
    abilities: Option<String>,
    held: Option<i32>,
    slots: BTreeMap<String, String>,
    attributes: BTreeMap<String, String>,
}

fn round(v: f64) -> String {
    format!("{:.2}", (v * 100.0).round() / 100.0 + 0.0)
}

fn item(stack: &vanillite_protocol::item::ItemStack) -> String {
    if stack.count <= 0 || stack.item == 0 {
        return "empty".to_owned();
    }
    let name = vanillite_data::registry::ITEM
        .name(stack.item)
        .unwrap_or("?");
    format!("{name} {}", stack.count)
}

fn game_mode(m: cb::GameMode) -> &'static str {
    match m {
        cb::GameMode::Survival => "survival",
        cb::GameMode::Creative => "creative",
        cb::GameMode::Adventure => "adventure",
        cb::GameMode::Spectator => "spectator",
    }
}

fn decode<P: Packet>(r: &mut Reader<'_>) -> Result<P, String> {
    let p = P::decode(r).map_err(|e| format!("{}: {e}", P::NAME))?;
    r.finish(P::ID).map_err(|e| format!("{}: {e}", P::NAME))?;
    Ok(p)
}

impl ClientView {
    /// The events of one tick from the packets (id + body each) the client got in it.
    pub fn tick<'a>(&mut self, packets: impl IntoIterator<Item = &'a [u8]>) -> Vec<String> {
        let before = Before {
            bosses: self.bosses.iter().map(|(_, s)| s.clone()).collect(),
            abilities: self.abilities.clone(),
            held: self.held,
            slots: self.slots.clone(),
            attributes: self.attributes.clone(),
        };
        let mut events = Vec::new();
        let mut blocks: BTreeMap<(i32, i32, i32), String> = BTreeMap::new();
        for body in packets {
            if let Err(e) = self.packet(body, &mut events, &mut blocks) {
                events.push(format!("undecodable {e}"));
            }
        }
        for ((x, y, z), state) in blocks {
            events.push(format!("block {x} {y} {z} {state}"));
        }
        for (i, (_, now)) in self.bosses.iter().enumerate() {
            let was = before.bosses.get(i).cloned().flatten();
            if *now != was {
                match now {
                    Some(s) => events.push(format!("boss#{i} {s}")),
                    None => events.push(format!("boss#{i} removed")),
                }
            }
        }
        if self.abilities != before.abilities
            && let Some(a) = &self.abilities
        {
            events.push(format!("abilities {a}"));
        }
        if self.held != before.held
            && let Some(h) = self.held
        {
            events.push(format!("held {h}"));
        }
        for (slot, stack) in &self.slots {
            if before.slots.get(slot) != Some(stack) {
                events.push(format!("slot {slot} {stack}"));
            }
        }
        for (name, value) in &self.attributes {
            if before.attributes.get(name) != Some(value) {
                events.push(format!("attr {name} {value}"));
            }
        }
        if !std::mem::replace(&mut self.joined, true) {
            events.retain(|e| !e.starts_with("game_event 3 "));
        }
        events.sort();
        events
    }

    fn packet(
        &mut self,
        body: &[u8],
        events: &mut Vec<String>,
        blocks: &mut BTreeMap<(i32, i32, i32), String>,
    ) -> Result<(), String> {
        let mut r = Reader::new(body);
        let id = r.varint().map_err(|e| e.to_string())?;
        let r = &mut r;
        match id {
            cb::AddEntity::ID => {
                let p: cb::AddEntity = decode(r)?;
                let kind = u16::try_from(p.entity_type)
                    .ok()
                    .and_then(|t| vanillite_data::registry::ENTITY_TYPE.name(t))
                    .unwrap_or("?")
                    .to_owned();
                events.push(format!(
                    "add {kind} {} {} {}",
                    round(p.position.x),
                    round(p.position.y),
                    round(p.position.z)
                ));
                self.entities.insert(p.entity_id, kind);
            }
            cb::RemoveEntities::ID => {
                let p: cb::RemoveEntities = decode(r)?;
                for id in p.ids.0 {
                    let kind = self.entities.remove(&id);
                    events.push(format!("remove {}", kind.as_deref().unwrap_or("?")));
                }
            }
            cb::Login::ID => {
                let p: cb::Login = decode(r)?;
                self.entities.clear();
                self.own_id = Some(p.entity_id);
                self.attributes.clear();
                events.push(format!("login {}", p.spawn.dimension));
            }
            cb::Respawn::ID => {
                let p: cb::Respawn = decode(r)?;
                self.entities.clear();
                // `KEEP_ATTRIBUTE_MODIFIERS` (bit 0) keeps the attributes.
                if p.data_to_keep & 1 == 0 {
                    self.attributes.clear();
                }
                events.push(format!(
                    "respawn {} {} keep={}",
                    p.spawn.dimension,
                    game_mode(p.spawn.game_mode),
                    p.data_to_keep
                ));
            }
            cb::BlockUpdate::ID => {
                let p: cb::BlockUpdate = decode(r)?;
                blocks.insert((p.pos.x, p.pos.y, p.pos.z), state(p.state));
            }
            cb::SectionBlocksUpdate::ID => {
                let p: cb::SectionBlocksUpdate = decode(r)?;
                // `SectionPos.asLong`: x 22 bits, z 22 bits, y 20 bits.
                let sx = (p.section >> 42) as i32;
                let sy = (p.section << 44 >> 44) as i32;
                let sz = (p.section << 22 >> 42) as i32;
                for c in p.changes {
                    let (x, z, y) = ((c >> 8) & 15, (c >> 4) & 15, c & 15);
                    let pos = (sx * 16 + x as i32, sy * 16 + y as i32, sz * 16 + z as i32);
                    blocks.insert(pos, state((c >> 12) as i32));
                }
            }
            cb::GameEvent::ID => {
                let p: cb::GameEvent = decode(r)?;
                events.push(format!("game_event {} {}", p.event, p.param));
            }
            cb::LevelEvent::ID => {
                let p: cb::LevelEvent = decode(r)?;
                events.push(format!(
                    "level_event {} {} {} {} {}",
                    p.event, p.pos.x, p.pos.y, p.pos.z, p.data
                ));
            }
            cb::EntityEvent::ID => {
                let p: cb::EntityEvent = decode(r)?;
                // Events of the player itself (permission levels) are left out.
                if let Some(kind) = self.entities.get(&p.entity_id) {
                    events.push(format!("entity_event {kind} {}", p.event));
                }
            }
            cb::BossEvent::ID => {
                let p: cb::BossEvent = decode(r)?;
                self.boss(p);
            }
            cb::PlayerAbilities::ID => {
                let p: cb::PlayerAbilities = decode(r)?;
                self.abilities = Some(format!(
                    "{} {} {}",
                    p.flags, p.flying_speed, p.walking_speed
                ));
            }
            cb::UpdateAttributes::ID if self.with_attributes => {
                let p: cb::UpdateAttributes = decode(r)?;
                self.update_attributes(&p.body)
                    .map_err(|e| format!("update_attributes: {e}"))?;
            }
            cb::SetHeldSlot::ID => {
                let p: cb::SetHeldSlot = decode(r)?;
                self.held = Some(p.slot);
            }
            cb::SetPlayerInventory::ID => {
                let p: cb::SetPlayerInventory = decode(r)?;
                self.slots.insert(format!("inv:{}", p.slot), item(&p.item));
            }
            cb::ContainerSetSlot::ID => {
                let p: cb::ContainerSetSlot = decode(r)?;
                self.slots
                    .insert(format!("{}:{}", p.container_id, p.slot), item(&p.item));
            }
            cb::ContainerSetContent::ID => {
                let p: cb::ContainerSetContent = decode(r)?;
                for (i, stack) in p.items.iter().enumerate() {
                    self.slots
                        .insert(format!("{}:{i}", p.container_id), item(stack));
                }
            }
            cb::SetObjective::ID
            | cb::SetDisplayObjective::ID
            | cb::SetScore::ID
            | cb::ResetScore::ID
            | cb::SetPlayerTeam::ID => {
                let name = match id {
                    cb::SetObjective::ID => "set_objective",
                    cb::SetDisplayObjective::ID => "set_display_objective",
                    cb::SetScore::ID => "set_score",
                    cb::ResetScore::ID => "reset_score",
                    _ => "set_player_team",
                };
                events.push(format!("{name} {}", crate::scenario::to_hex(r.take_rest())));
            }
            cb::MapItemData::ID => {
                let rest = r.take_rest();
                let hash = rest.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
                    (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
                });
                events.push(format!("map_item_data {} {hash:016x}", rest.len()));
            }
            _ => {}
        }
        Ok(())
    }

    /// `update_attributes` of the player itself: per attribute the value the client computes
    /// (`AttributeInstance.calculateValue` without the attribute's range).
    fn update_attributes(&mut self, body: &[u8]) -> Result<(), String> {
        let mut r = Reader::new(body);
        let e = |e: vanillite_protocol::codec::Error| e.to_string();
        let f64_of = |r: &mut Reader<'_>| -> Result<f64, String> {
            let b = r.take(8).map_err(e)?;
            Ok(f64::from_be_bytes(b.try_into().map_err(|_| "f64")?))
        };
        let entity = r.varint().map_err(e)?;
        if Some(entity) != self.own_id {
            return Ok(());
        }
        let count = r.varint().map_err(e)?;
        for _ in 0..count {
            let id = r.varint().map_err(e)?;
            let base = f64_of(&mut r)?;
            let mods = r.varint().map_err(e)?;
            let mut list = Vec::new();
            for _ in 0..mods {
                r.string(32767).map_err(e)?;
                let amount = f64_of(&mut r)?;
                let op = r.take(1).map_err(e)?[0];
                list.push((op, amount));
            }
            let mut v = base;
            for &(op, a) in &list {
                if op == 0 {
                    v += a;
                }
            }
            let mut result = v;
            for &(op, a) in &list {
                if op == 1 {
                    result += v * a;
                }
            }
            for &(op, a) in &list {
                if op == 2 {
                    result *= 1.0 + a;
                }
            }
            let name = usize::try_from(id)
                .ok()
                .and_then(|i| vanillite_data::entity::ATTRIBUTES.get(i))
                .map_or_else(|| format!("?{id}"), |a| a.name.to_owned());
            self.attributes.insert(name, format!("{:.4}", result + 0.0));
        }
        Ok(())
    }

    fn boss(&mut self, p: cb::BossEvent) {
        use cb::BossEventOperation as Op;
        let index = match self.bosses.iter().position(|(u, _)| *u == p.id.0) {
            Some(i) => i,
            None => {
                self.bosses.push((p.id.0, None));
                self.bosses.len() - 1
            }
        };
        let state = &mut self.bosses[index].1;
        // The fields of a bar: name, progress, color, overlay, flags.
        let mut fields: Vec<String> = state
            .as_deref()
            .map(|s| s.split('\t').map(str::to_owned).collect())
            .unwrap_or_default();
        let flags = |f: cb::BossBarFlags| {
            format!(
                "darken={} music={} fog={}",
                f.darken_screen, f.play_music, f.create_fog
            )
        };
        match p.operation {
            Op::Add {
                name,
                progress,
                color,
                overlay,
                flags: f,
            } => {
                fields = vec![
                    name.to_json().to_string(),
                    progress.to_string(),
                    format!("{color:?}"),
                    format!("{overlay:?}"),
                    flags(f),
                ];
            }
            Op::Remove => {
                *state = None;
                return;
            }
            _ if fields.len() != 5 => return,
            Op::UpdateProgress(v) => fields[1] = v.to_string(),
            Op::UpdateName(name) => fields[0] = name.to_json().to_string(),
            Op::UpdateStyle { color, overlay } => {
                fields[2] = format!("{color:?}");
                fields[3] = format!("{overlay:?}");
            }
            Op::UpdateProperties(f) => fields[4] = flags(f),
        }
        *state = Some(fields.join("\t"));
    }
}

fn state(id: i32) -> String {
    u16::try_from(id)
        .ok()
        .filter(|&i| usize::from(i) < vanillite_data::block::STATE_COUNT)
        .map_or_else(
            || format!("?{id}"),
            |i| crate::world::state_name(vanillite_data::block::BlockState(i)),
        )
}

/// The client views of a scenario's players, by name.
#[derive(Clone, Debug, Default)]
pub struct ClientViews(BTreeMap<String, ClientView>, bool);

impl ClientViews {
    /// Views that also show the players' own attributes (`client_attributes`); a golden holds
    /// `update_attributes` only when its scenario asked for them.
    pub fn with_attributes(attributes: bool) -> Self {
        Self(BTreeMap::new(), attributes)
    }

    /// The frame's view of every player's packets (`(player, packets)`), `<player> <event>`
    /// sorted.
    pub fn frame<'a>(
        &mut self,
        players: impl IntoIterator<Item = (&'a str, Vec<&'a [u8]>)>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for (name, packets) in players {
            let attributes = self.1;
            let view = self.0.entry(name.to_owned()).or_insert_with(|| ClientView {
                with_attributes: attributes,
                ..ClientView::default()
            });
            out.extend(
                view.tick(packets)
                    .into_iter()
                    .map(|e| format!("{name} {e}")),
            );
        }
        out.sort();
        out
    }
}
