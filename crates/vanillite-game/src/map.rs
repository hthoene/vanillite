//! Maps (`MapItemSavedData`, `MapIndex`, `MapItem`): map ids, scale, lock state, colours,
//! banner and frame markers, the decorations of the players carrying a map and the
//! `map_item_data` updates for them.
//!
//! All worlds share one [`Maps`] store (map ids are world-wide); the server loads entries from
//! `data/minecraft/maps/<id>.dat` on first use and writes the changed ones with every save. Each
//! map has its own lock, so worlds updating different maps never wait for each other.
//!
//! A map knows the players carrying it (`carriedBy`) by UUID with what other carriers need
//! of them (name, dimension, position, whether they still carry the map and whether they
//! wear map-invisibility equipment), refreshed by each player's own inventory tick: the
//! players may live on other world threads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use vanillite_base::BlockPos;
use vanillite_chunk::HeightmapKind;
use vanillite_data::block::BlockState;
use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::codec::{Reader, write_varint};
use vanillite_protocol::components::ids as components;
use vanillite_protocol::packets::play::clientbound::{
    MapDecoration as WireDecoration, MapItemData, MapPatch,
};
use vanillite_protocol::text::Component;

use crate::item::ItemStack;
use crate::level::{BlockEntity, Level};

/// `MapItemSavedData.MAX_SCALE`.
pub const MAX_SCALE: i8 = 4;
/// `TRACKED_DECORATION_LIMIT`.
const TRACKED_DECORATION_LIMIT: i32 = 256;

/// The saved data id of the map index (`maps/last_id`).
pub const INDEX_ID: &str = "minecraft:maps/last_id";

/// The saved data id of map `id`.
pub fn data_id(id: i32) -> String {
    format!("minecraft:maps/{id}")
}

/// `MapColor` ids the map renderer names.
mod color {
    pub const NONE: u8 = 0;
    pub const DIRT: u8 = 10;
    pub const STONE: u8 = 11;
    pub const WATER: u8 = 12;
    pub const COLOR_ORANGE: u8 = 15;
    pub const COLOR_BROWN: u8 = 26;
}

/// `MapColor.Brightness`.
#[derive(Clone, Copy)]
enum Brightness {
    Low = 0,
    Normal = 1,
    High = 2,
    Lowest = 3,
}

/// `MapColor.getPackedId(brightness)`.
fn packed(color: u8, brightness: Brightness) -> i8 {
    i8::from_ne_bytes([color.wrapping_shl(2) | brightness as u8])
}

/// The registry id of a map decoration type (`minecraft:map_decoration_type`).
fn decoration_type(name: &str) -> u16 {
    vanillite_data::registry::MAP_DECORATION_TYPE
        .id(name)
        .unwrap_or(0)
}

/// The ids of the player decorations: on the map, off the map, off limits.
fn player_types() -> [u16; 3] {
    static IDS: std::sync::OnceLock<[u16; 3]> = std::sync::OnceLock::new();
    *IDS.get_or_init(|| {
        [
            decoration_type("minecraft:player"),
            decoration_type("minecraft:player_off_map"),
            decoration_type("minecraft:player_off_limits"),
        ]
    })
}

/// `MapDecorationType.trackCount`.
fn track_count(kind: u16) -> bool {
    let name = vanillite_data::registry::MAP_DECORATION_TYPE
        .name(kind)
        .unwrap_or("");
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    matches!(
        name,
        "player" | "frame" | "red_marker" | "blue_marker" | "player_off_map" | "player_off_limits"
    ) || name.starts_with("banner_")
}

/// The dye colours in id order (`DyeColor`).
const DYE_COLORS: [&str; 16] = [
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];

/// A banner marker (`MapBanner`).
#[derive(Clone, Debug, PartialEq)]
pub struct Banner {
    pub pos: BlockPos,
    /// `DyeColor` id.
    pub color: u8,
    pub name: Option<Component>,
}

impl Banner {
    /// `getId()`.
    fn id(&self) -> String {
        format!("banner-{},{},{}", self.pos.x, self.pos.y, self.pos.z)
    }

    /// `getDecoration()`.
    fn decoration(&self) -> u16 {
        let color = DYE_COLORS
            .get(usize::from(self.color))
            .copied()
            .unwrap_or("white");
        decoration_type(&format!("minecraft:banner_{color}"))
    }

    /// `MapBanner.fromWorld`: the banner block entity at `pos`, if any.
    pub fn from_world(level: &Level, pos: BlockPos) -> Option<Banner> {
        let Some(BlockEntity::Banner(banner)) = level.block_entity(pos) else {
            return None;
        };
        let name = level.block(pos).block().name();
        let name = name.strip_prefix("minecraft:").unwrap_or(name);
        let base = name
            .strip_suffix("_wall_banner")
            .or_else(|| name.strip_suffix("_banner"))
            .unwrap_or("white");
        let color = DYE_COLORS
            .iter()
            .position(|c| *c == base)
            .and_then(|i| u8::try_from(i).ok())
            .unwrap_or(0);
        Some(Banner {
            pos,
            color,
            name: banner.custom_name.clone(),
        })
    }

    fn load(c: &Compound) -> Option<Banner> {
        let pos = match c.get("pos") {
            Some(Tag::IntArray(v)) if v.len() == 3 => BlockPos::new(v[0], v[1], v[2]),
            _ => return None,
        };
        let color = match c.get("color") {
            Some(Tag::String(s)) => DYE_COLORS
                .iter()
                .position(|d| d == s)
                .and_then(|i| u8::try_from(i).ok())
                .unwrap_or(0),
            _ => 0,
        };
        let name = c.get("name").map(Component::from_nbt);
        Some(Banner { pos, color, name })
    }

    fn save(&self) -> Tag {
        let mut c = Compound::new();
        c.insert(
            "pos",
            Tag::IntArray(vec![self.pos.x, self.pos.y, self.pos.z]),
        );
        let color = DYE_COLORS
            .get(usize::from(self.color))
            .copied()
            .unwrap_or("white");
        c.insert("color", Tag::String(color.to_owned()));
        if let Some(name) = &self.name {
            c.insert("name", name.to_nbt());
        }
        Tag::Compound(c)
    }
}

/// An item frame marker (`MapFrame`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub pos: BlockPos,
    pub rotation: i32,
    pub entity_id: i32,
}

impl Frame {
    fn load(c: &Compound) -> Option<Frame> {
        let pos = match c.get("pos") {
            Some(Tag::IntArray(v)) if v.len() == 3 => BlockPos::new(v[0], v[1], v[2]),
            _ => return None,
        };
        let int = |k| match c.get(k) {
            Some(Tag::Int(v)) => Some(*v),
            _ => None,
        };
        Some(Frame {
            pos,
            rotation: int("rotation")?,
            entity_id: int("entity_id")?,
        })
    }

    fn save(&self) -> Tag {
        let mut c = Compound::new();
        c.insert(
            "pos",
            Tag::IntArray(vec![self.pos.x, self.pos.y, self.pos.z]),
        );
        c.insert("rotation", Tag::Int(self.rotation));
        c.insert("entity_id", Tag::Int(self.entity_id));
        Tag::Compound(c)
    }
}

/// A decoration on a map (`MapDecoration`).
#[derive(Clone, Debug, PartialEq)]
pub struct Decoration {
    /// `minecraft:map_decoration_type` id.
    pub kind: u16,
    pub x: i8,
    pub y: i8,
    /// 0–15.
    pub rot: i8,
    pub name: Option<Component>,
}

/// What a map knows of a player carrying it (`HoldingPlayer` and the player fields
/// `tickCarriedBy` reads).
#[derive(Clone, Debug, PartialEq)]
pub struct Carrier {
    pub uuid: u128,
    /// `getPlainTextName`.
    pub name: String,
    pub dimension: String,
    pub x: f64,
    pub z: f64,
    pub y_rot: f32,
    /// `getInventory().contains(mapMatcher)`, and not removed from the world.
    pub has_map: bool,
    /// `hasMapInvisibilityItemEquipped`.
    pub invisible: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Holder {
    carrier: Carrier,
    dirty_data: bool,
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    dirty_decorations: bool,
    tick: i32,
    step: i32,
}

impl Holder {
    fn new(carrier: Carrier) -> Holder {
        Holder {
            carrier,
            dirty_data: true,
            min_x: 0,
            min_y: 0,
            max_x: 127,
            max_y: 127,
            dirty_decorations: true,
            tick: 0,
            step: 0,
        }
    }

    fn mark_colors_dirty(&mut self, x: i32, y: i32) {
        if self.dirty_data {
            self.min_x = self.min_x.min(x);
            self.min_y = self.min_y.min(y);
            self.max_x = self.max_x.max(x);
            self.max_y = self.max_y.max(y);
        } else {
            self.dirty_data = true;
            self.min_x = x;
            self.min_y = y;
            self.max_x = x;
            self.max_y = y;
        }
    }
}

/// One map (`MapItemSavedData`).
#[derive(Clone, Debug, PartialEq)]
pub struct MapData {
    /// `dimension`.
    pub dimension: String,
    /// `xCenter`.
    pub x_center: i32,
    /// `zCenter`.
    pub z_center: i32,
    /// `scale` (0–4).
    pub scale: i8,
    /// `trackingPosition`.
    pub tracking_position: bool,
    /// `unlimitedTracking`.
    pub unlimited_tracking: bool,
    /// `locked`.
    pub locked: bool,
    /// `colors` (128 × 128).
    pub colors: Vec<i8>,
    /// `bannerMarkers`.
    pub banners: Vec<Banner>,
    /// `frameMarkers`.
    pub frames: Vec<Frame>,
    /// `decorations`, in insertion order.
    decorations: Vec<(String, Decoration)>,
    tracked_count: i32,
    /// `carriedBy`.
    holders: Vec<Holder>,
    /// `setDirty`: the map is saved with the next save.
    pub dirty: bool,
}

impl MapData {
    fn new(
        dimension: String,
        x_center: i32,
        z_center: i32,
        scale: i8,
        tracking_position: bool,
        unlimited_tracking: bool,
        locked: bool,
    ) -> MapData {
        MapData {
            dimension,
            x_center,
            z_center,
            scale,
            tracking_position,
            unlimited_tracking,
            locked,
            colors: vec![0; 16384],
            banners: Vec::new(),
            frames: Vec::new(),
            decorations: Vec::new(),
            tracked_count: 0,
            holders: Vec::new(),
            dirty: false,
        }
    }

    /// Reads the `data` compound (`MapItemSavedData.CODEC`).
    pub fn load(c: &Compound) -> Option<MapData> {
        let int = |k: &str| match c.get(k) {
            Some(Tag::Int(v)) => Some(*v),
            _ => None,
        };
        let flag = |k: &str, default: bool| match c.get(k) {
            Some(Tag::Byte(v)) => *v != 0,
            _ => default,
        };
        let dimension = match c.get("dimension") {
            Some(Tag::String(s)) => s.clone(),
            _ => return None,
        };
        let mut data = MapData::new(
            dimension,
            int("xCenter")?,
            int("zCenter")?,
            match c.get("scale") {
                Some(Tag::Byte(v)) => (*v).clamp(0, MAX_SCALE),
                _ => 0,
            },
            flag("trackingPosition", true),
            flag("unlimitedTracking", false),
            flag("locked", false),
        );
        if let Some(Tag::ByteArray(b)) = c.get("colors")
            && b.len() == 16384
        {
            data.colors = b.clone();
        }
        let list = |k: &str| -> Vec<Compound> {
            match c.get(k) {
                Some(Tag::List(l)) => {
                    l.0.iter()
                        .filter_map(|t| match t {
                            Tag::Compound(c) => Some(c.clone()),
                            _ => None,
                        })
                        .collect()
                }
                _ => Vec::new(),
            }
        };
        for banner in list("banners").iter().filter_map(Banner::load) {
            let (id, kind, name) = (banner.id(), banner.decoration(), banner.name.clone());
            let (x, z) = (f64::from(banner.pos.x), f64::from(banner.pos.z));
            data.banners.retain(|b| b.id() != id);
            data.banners.push(banner);
            data.add_decoration(kind, None, &id, x, z, 180.0, name);
        }
        for frame in list("frames").iter().filter_map(Frame::load) {
            let key = frame_key(frame.entity_id);
            let (x, z, rot) = (
                f64::from(frame.pos.x),
                f64::from(frame.pos.z),
                f64::from(frame.rotation),
            );
            data.frames.retain(|f| f.pos != frame.pos);
            data.frames.push(frame);
            data.add_decoration(
                decoration_type("minecraft:frame"),
                None,
                &key,
                x,
                z,
                rot,
                None,
            );
        }
        Some(data)
    }

    /// The `data` compound.
    pub fn save(&self) -> Compound {
        let mut c = Compound::new();
        c.insert("dimension", Tag::String(self.dimension.clone()));
        c.insert("xCenter", Tag::Int(self.x_center));
        c.insert("zCenter", Tag::Int(self.z_center));
        c.insert("scale", Tag::Byte(self.scale));
        c.insert("colors", Tag::ByteArray(self.colors.clone()));
        c.insert(
            "trackingPosition",
            Tag::Byte(i8::from(self.tracking_position)),
        );
        c.insert(
            "unlimitedTracking",
            Tag::Byte(i8::from(self.unlimited_tracking)),
        );
        c.insert("locked", Tag::Byte(i8::from(self.locked)));
        c.insert(
            "banners",
            Tag::List(List(self.banners.iter().map(Banner::save).collect())),
        );
        c.insert(
            "frames",
            Tag::List(List(self.frames.iter().map(Frame::save).collect())),
        );
        c
    }

    /// `MapItemSavedData.createFresh(originX, originZ, scale, …)`.
    pub fn fresh(
        origin_x: f64,
        origin_z: f64,
        scale: i8,
        tracking_position: bool,
        unlimited_tracking: bool,
        dimension: String,
    ) -> MapData {
        let scale = scale.clamp(0, MAX_SCALE);
        let size = 128i32 << scale;
        let area_x = ((origin_x + 64.0) / f64::from(size)).floor() as i32;
        let area_z = ((origin_z + 64.0) / f64::from(size)).floor() as i32;
        MapData::new(
            dimension,
            area_x * size + size / 2 - 64,
            area_z * size + size / 2 - 64,
            scale,
            tracking_position,
            unlimited_tracking,
            false,
        )
    }

    /// `scaled()`: a fresh map one scale up around the same centre.
    pub fn scaled(&self) -> MapData {
        MapData::fresh(
            f64::from(self.x_center),
            f64::from(self.z_center),
            (self.scale + 1).clamp(0, MAX_SCALE),
            self.tracking_position,
            self.unlimited_tracking,
            self.dimension.clone(),
        )
    }

    /// `locked()`: a locked copy with the colours, banners and decorations (no frames).
    pub fn locked(&self) -> MapData {
        let mut copy = MapData::new(
            self.dimension.clone(),
            self.x_center,
            self.z_center,
            self.scale,
            self.tracking_position,
            self.unlimited_tracking,
            true,
        );
        copy.banners = self.banners.clone();
        copy.decorations = self.decorations.clone();
        copy.tracked_count = self.tracked_count;
        copy.colors = self.colors.clone();
        copy
    }

    /// The decorations, in order.
    pub fn decorations(&self) -> impl Iterator<Item = &Decoration> {
        self.decorations.iter().map(|(_, d)| d)
    }

    fn set_decorations_dirty(&mut self) {
        for h in &mut self.holders {
            h.dirty_decorations = true;
        }
    }

    /// `removeDecoration`.
    fn remove_decoration(&mut self, key: &str) {
        if let Some(i) = self.decorations.iter().position(|(k, _)| k == key) {
            let (_, d) = self.decorations.remove(i);
            if track_count(d.kind) {
                self.tracked_count -= 1;
            }
        }
        self.set_decorations_dirty();
    }

    /// `calculateRotation`: spinning in the Nether (with a level), else the yaw.
    fn rotation(&self, game_time: Option<i64>, y_rot: f64) -> i8 {
        if self.dimension == "minecraft:the_nether"
            && let Some(time) = game_time
        {
            let s = (time / 10) as i32;
            return (s
                .wrapping_mul(s)
                .wrapping_mul(34_187_121)
                .wrapping_add(s.wrapping_mul(121))
                >> 15
                & 15) as i8;
        }
        let adjusted = if y_rot < 0.0 {
            y_rot - 8.0
        } else {
            y_rot + 8.0
        };
        (adjusted * 16.0 / 360.0) as i32 as i8
    }

    /// `addDecoration`.
    #[allow(clippy::too_many_arguments)]
    fn add_decoration(
        &mut self,
        kind: u16,
        game_time: Option<i64>,
        key: &str,
        x: f64,
        z: f64,
        y_rot: f64,
        name: Option<Component>,
    ) {
        let scaling = (1i32 << self.scale) as f32;
        let xd = (x - f64::from(self.x_center)) as f32 / scaling;
        let yd = (z - f64::from(self.z_center)) as f32 / scaling;
        let Some((kind, px, py, rot)) = self.location(kind, game_time, y_rot, xd, yd) else {
            self.remove_decoration(key);
            return;
        };
        let new = Decoration {
            kind,
            x: px,
            y: py,
            rot: rot & 15,
            name,
        };
        let previous = match self.decorations.iter_mut().find(|(k, _)| k == key) {
            Some((_, d)) if *d == new => return,
            Some((_, d)) => Some(std::mem::replace(d, new.clone())),
            None => {
                self.decorations.push((key.to_owned(), new.clone()));
                None
            }
        };
        if previous.is_some_and(|p| track_count(p.kind)) {
            self.tracked_count -= 1;
        }
        if track_count(new.kind) {
            self.tracked_count += 1;
        }
        self.set_decorations_dirty();
    }

    /// `calculateDecorationLocationAndType`.
    fn location(
        &self,
        kind: u16,
        game_time: Option<i64>,
        y_rot: f64,
        xd: f32,
        yd: f32,
    ) -> Option<(u16, i8, i8, i8)> {
        let cx = clamp_map_coordinate(xd);
        let cy = clamp_map_coordinate(yd);
        let [player, off_map, off_limits] = player_types();
        if kind == player {
            let kind = if is_inside_map(xd, yd) {
                kind
            } else if xd.abs() < 320.0 && yd.abs() < 320.0 {
                off_map
            } else if self.unlimited_tracking {
                off_limits
            } else {
                return None;
            };
            return Some((kind, cx, cy, self.rotation(game_time, y_rot)));
        }
        if !is_inside_map(xd, yd) && !self.unlimited_tracking {
            return None;
        }
        Some((kind, cx, cy, self.rotation(game_time, y_rot)))
    }

    /// `tickCarriedBy(player, stack, null)`: `carrier` is the ticking player's current state;
    /// `statics` are the `map_decorations` of the stack (key, type, x, z, rotation).
    pub fn tick_carried_by(
        &mut self,
        carrier: &Carrier,
        statics: &[(String, u16, f64, f64, f32)],
        game_time: i64,
    ) {
        self.tick_carried_in(carrier, statics, game_time, None);
    }

    /// `isTrackedCountOverLimit(limit)`.
    pub fn is_tracked_count_over_limit(&self, limit: i32) -> bool {
        self.tracked_count > limit
    }

    /// `removedFromFrame(pos, entityId)`.
    pub fn removed_from_frame(&mut self, pos: BlockPos, entity_id: i32) {
        self.remove_decoration(&frame_key(entity_id));
        self.frames.retain(|f| f.pos != pos);
        self.dirty = true;
    }

    /// `tickCarriedBy(player, stack, placedInFrame)`: `frame` is the frame the map is in
    /// (its block position, rotation `direction2D * 90` and network id).
    pub fn tick_carried_in(
        &mut self,
        carrier: &Carrier,
        statics: &[(String, u16, f64, f64, f32)],
        game_time: i64,
        frame: Option<Frame>,
    ) {
        let in_frame = frame.is_some();
        match self
            .holders
            .iter()
            .position(|h| h.carrier.uuid == carrier.uuid)
        {
            Some(i) => {
                let c = &mut self.holders[i].carrier;
                if c.name != carrier.name {
                    c.name.clone_from(&carrier.name);
                }
                if c.dimension != carrier.dimension {
                    c.dimension.clone_from(&carrier.dimension);
                }
                (c.x, c.z, c.y_rot) = (carrier.x, carrier.z, carrier.y_rot);
                (c.has_map, c.invisible) = (carrier.has_map, carrier.invisible);
            }
            None => self.holders.push(Holder::new(carrier.clone())),
        }
        if !carrier.has_map {
            self.remove_decoration(&carrier.name);
        }
        let player = player_types()[0];
        // Java's loop over `carriedBy` with removals skips the entry after a removed one.
        let mut i = 0;
        while i < self.holders.len() {
            let h = &self.holders[i];
            let (uuid, has_map, invisible) =
                (h.carrier.uuid, h.carrier.has_map, h.carrier.invisible);
            let (x, z, y_rot) = (h.carrier.x, h.carrier.z, h.carrier.y_rot);
            let same_dimension = h.carrier.dimension == self.dimension;
            // The name is lent out while the decorations change.
            let name = std::mem::take(&mut self.holders[i].carrier.name);
            let removed = if has_map || in_frame {
                if !in_frame && same_dimension && self.tracking_position {
                    self.add_decoration(
                        player,
                        Some(game_time),
                        &name,
                        x,
                        z,
                        f64::from(y_rot),
                        None,
                    );
                }
                false
            } else {
                self.holders.remove(i);
                self.remove_decoration(&name);
                true
            };
            if uuid != carrier.uuid && invisible {
                self.remove_decoration(&name);
            }
            if !removed && let Some(h) = self.holders.get_mut(i) {
                h.carrier.name = name;
            }
            i += 1;
        }
        if let Some(frame) = frame
            && self.tracking_position
        {
            if let Some(existing) = self.frames.iter().find(|f| f.pos == frame.pos)
                && existing.entity_id != frame.entity_id
            {
                let key = frame_key(existing.entity_id);
                self.remove_decoration(&key);
            }
            self.add_decoration(
                decoration_type("minecraft:frame"),
                Some(game_time),
                &frame_key(frame.entity_id),
                f64::from(frame.pos.x),
                f64::from(frame.pos.z),
                f64::from(frame.rotation),
                None,
            );
            let old = self.frames.iter().position(|f| f.pos == frame.pos);
            let changed = old.is_none_or(|i| self.frames[i] != frame);
            if let Some(i) = old {
                self.frames[i] = frame;
            } else {
                self.frames.push(frame);
            }
            if changed {
                self.dirty = true;
            }
        }
        let missing = statics
            .iter()
            .any(|(k, ..)| !self.decorations.iter().any(|(d, _)| d == k));
        if missing {
            for (key, kind, x, z, rot) in statics {
                if !self.decorations.iter().any(|(d, _)| d == key) {
                    self.add_decoration(*kind, Some(game_time), key, *x, *z, f64::from(*rot), None);
                }
            }
        }
    }

    /// The player no longer carries the map (it left the inventory or the player left).
    pub fn carrier_gone(&mut self, uuid: u128) {
        if let Some(h) = self.holders.iter_mut().find(|h| h.carrier.uuid == uuid) {
            h.carrier.has_map = false;
        }
    }

    fn set_color(&mut self, x: i32, y: i32, value: i8) {
        if let Some(c) = usize::try_from(x + y * 128)
            .ok()
            .and_then(|i| self.colors.get_mut(i))
        {
            *c = value;
        }
        self.dirty = true;
        for h in &mut self.holders {
            h.mark_colors_dirty(x, y);
        }
    }

    /// `updateColor`: whether the colour changed.
    fn update_color(&mut self, x: i32, y: i32, value: i8) -> bool {
        let old = usize::try_from(x + y * 128)
            .ok()
            .and_then(|i| self.colors.get(i))
            .copied();
        if old != Some(value) {
            self.set_color(x, y, value);
            true
        } else {
            false
        }
    }

    /// `toggleBanner`: adds or removes the marker of the banner at `pos`; `false` when there
    /// is no banner, it is outside the map or there are too many markers.
    pub fn toggle_banner(&mut self, level: &Level, pos: BlockPos) -> bool {
        let x = f64::from(pos.x) + 0.5;
        let z = f64::from(pos.z) + 0.5;
        let scale = f64::from(1i32 << self.scale);
        let xd = (x - f64::from(self.x_center)) / scale;
        let yd = (z - f64::from(self.z_center)) / scale;
        if !(xd >= -63.0 && yd >= -63.0 && xd <= 63.0 && yd <= 63.0) {
            return false;
        }
        let Some(banner) = Banner::from_world(level, pos) else {
            return false;
        };
        let id = banner.id();
        if let Some(i) = self.banners.iter().position(|b| *b == banner) {
            self.banners.remove(i);
            self.remove_decoration(&id);
            self.dirty = true;
            return true;
        }
        if self.tracked_count <= TRACKED_DECORATION_LIMIT {
            let (kind, name) = (banner.decoration(), banner.name.clone());
            self.banners.retain(|b| b.id() != id);
            self.banners.push(banner);
            self.add_decoration(kind, Some(level.game_time()), &id, x, z, 180.0, name);
            self.dirty = true;
            return true;
        }
        false
    }

    /// `checkBanners`: markers in the column whose banner changed or went away are removed.
    fn check_banners(&mut self, level: &Level, x: i32, z: i32) {
        if self.banners.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.banners.len() {
            let expected = self.banners[i].clone();
            if expected.pos.x == x
                && expected.pos.z == z
                && Banner::from_world(level, expected.pos).as_ref() != Some(&expected)
            {
                self.banners.remove(i);
                self.remove_decoration(&expected.id());
                self.dirty = true;
                continue;
            }
            i += 1;
        }
    }

    /// `HoldingPlayer.nextUpdatePacket` of the carrier `uuid`.
    pub fn next_update_packet(&mut self, uuid: u128, id: i32) -> Option<MapItemData> {
        let (scale, locked) = (self.scale, self.locked);
        let colors = &self.colors;
        let h = self.holders.iter_mut().find(|h| h.carrier.uuid == uuid)?;
        let patch = if h.dirty_data {
            h.dirty_data = false;
            let width = h.max_x + 1 - h.min_x;
            let height = h.max_y + 1 - h.min_y;
            let mut out = Vec::with_capacity(usize::try_from(width * height).unwrap_or(0));
            for y in 0..height {
                let row = usize::try_from(h.min_x + (h.min_y + y) * 128).unwrap_or(0);
                let width = usize::try_from(width).unwrap_or(0);
                if let Some(src) = colors.get(row..row + width) {
                    out.extend(src.iter().map(|&c| c as u8));
                }
            }
            Some(MapPatch {
                start_x: h.min_x as u8,
                start_y: h.min_y as u8,
                width: width as u8,
                height: height as u8,
                colors: out,
            })
        } else {
            None
        };
        let send_decorations = h.dirty_decorations && {
            let t = h.tick;
            h.tick = h.tick.wrapping_add(1);
            t % 5 == 0
        };
        if send_decorations {
            h.dirty_decorations = false;
        }
        if !send_decorations && patch.is_none() {
            return None;
        }
        let decorations = send_decorations.then(|| {
            self.decorations
                .iter()
                .map(|(_, d)| WireDecoration {
                    kind: i32::from(d.kind),
                    x: d.x,
                    y: d.y,
                    rot: d.rot,
                    name: d.name.clone(),
                })
                .collect()
        });
        Some(MapItemData {
            map_id: id,
            scale,
            locked,
            decorations,
            patch,
        })
    }
}

/// `isInsideMap`.
fn is_inside_map(xd: f32, yd: f32) -> bool {
    (-63.0..=63.0).contains(&xd) && (-63.0..=63.0).contains(&yd)
}

/// `clampMapCoordinate`.
fn clamp_map_coordinate(d: f32) -> i8 {
    if d <= -63.0 {
        -128
    } else if d >= 63.0 {
        127
    } else {
        // `deltaFromCenter * 2.0F + 0.5`: the addition is in double.
        (f64::from(d * 2.0) + 0.5) as i32 as i8
    }
}

fn frame_key(entity_id: i32) -> String {
    format!("frame-{entity_id}")
}

/// Loads the stored `data` compound of a saved data id.
pub type Loader = Box<dyn Fn(&str) -> Option<Compound> + Send + Sync>;

#[derive(Default)]
struct Inner {
    /// Loaded maps (`None`: no data for that id).
    maps: HashMap<i32, Option<Arc<Mutex<MapData>>>>,
    /// `MapIndex.lastMapId` once read.
    last_id: Option<i32>,
    index_dirty: bool,
}

/// The maps of the server.
pub struct Maps {
    loader: Loader,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Maps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Maps").finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Maps {
    /// A store that reads stored data with `loader`.
    pub fn new(loader: Loader) -> Maps {
        Maps {
            loader,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn entry(&self, id: i32) -> Option<Arc<Mutex<MapData>>> {
        let mut inner = lock(&self.inner);
        if let Some(entry) = inner.maps.get(&id) {
            return entry.clone();
        }
        let data = (self.loader)(&data_id(id))
            .and_then(|c| MapData::load(&c))
            .map(|d| Arc::new(Mutex::new(d)));
        inner.maps.insert(id, data.clone());
        data
    }

    /// `getMapData(id)`: a copy.
    pub fn get(&self, id: i32) -> Option<MapData> {
        self.entry(id).map(|e| lock(&e).clone())
    }

    /// Runs `f` on map `id`.
    pub fn with<R>(&self, id: i32, f: impl FnOnce(&mut MapData) -> R) -> Option<R> {
        let entry = self.entry(id)?;
        let mut data = lock(&entry);
        Some(f(&mut data))
    }

    /// `setMapData(id, data)`.
    pub fn set(&self, id: i32, mut data: MapData) {
        data.dirty = true;
        lock(&self.inner)
            .maps
            .insert(id, Some(Arc::new(Mutex::new(data))));
    }

    /// `getFreeMapId()`.
    pub fn free_id(&self) -> i32 {
        let mut inner = lock(&self.inner);
        let last = match inner.last_id {
            Some(v) => v,
            None => (self.loader)(INDEX_ID)
                .and_then(|c| match c.get("map") {
                    Some(Tag::Int(v)) => Some(*v),
                    _ => None,
                })
                .unwrap_or(-1),
        };
        let id = last.saturating_add(1);
        inner.last_id = Some(id);
        inner.index_dirty = true;
        id
    }

    /// The saved data to write: changed maps and the index.
    pub fn take_dirty(&self) -> Vec<(String, Compound)> {
        let mut inner = lock(&self.inner);
        let mut out = Vec::new();
        let mut ids: Vec<i32> = inner.maps.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            if let Some(Some(entry)) = inner.maps.get(&id) {
                let mut data = lock(entry);
                if std::mem::take(&mut data.dirty) {
                    out.push((data_id(id), data.save()));
                }
            }
        }
        if std::mem::take(&mut inner.index_dirty)
            && let Some(last) = inner.last_id
        {
            let mut c = Compound::new();
            c.insert("map", Tag::Int(last));
            out.push((INDEX_ID.to_owned(), c));
        }
        out
    }
}

/// `MapPostProcessing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostProcessing {
    /// `LOCK`.
    Lock,
    /// `SCALE`.
    Scale,
}

/// The `map_id` of a stack.
pub fn map_id(stack: &ItemStack) -> Option<i32> {
    stack
        .component(components::MAP_ID)
        .and_then(|v| Reader::new(v).varint().ok())
}

/// Sets the `map_id` of a stack.
pub fn set_map_id(stack: &mut ItemStack, id: i32) {
    let mut out = Vec::with_capacity(5);
    write_varint(&mut out, id);
    stack.set_component(components::MAP_ID, out.into_boxed_slice());
}

/// Sets `map_post_processing`.
pub fn set_post_processing(stack: &mut ItemStack, value: PostProcessing) {
    let id = match value {
        PostProcessing::Lock => 0,
        PostProcessing::Scale => 1,
    };
    stack.set_component(components::MAP_POST_PROCESSING, Box::new([id]));
}

/// Whether the item is a `MapItem`: `filled_map` and the explorer map items
/// (`buried_treasure_map`, `woodland_mansion_map`, ...), not the empty `map`.
pub fn is_map_item(item: u16) -> bool {
    static IDS: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
    IDS.get_or_init(|| {
        vanillite_data::registry::ITEM
            .entries
            .iter()
            .enumerate()
            .filter(|(_, name)| name.ends_with("_map") && **name != "minecraft:map")
            .filter_map(|(id, _)| u16::try_from(id).ok())
            .collect()
    })
    .contains(&item)
}

/// `MapItem.create`: a `filled_map` with a new map around the origin.
pub fn create(
    maps: &Maps,
    origin_x: i32,
    origin_z: i32,
    scale: i8,
    tracking_position: bool,
    unlimited_tracking: bool,
    dimension: String,
) -> ItemStack {
    let data = MapData::fresh(
        f64::from(origin_x),
        f64::from(origin_z),
        scale,
        tracking_position,
        unlimited_tracking,
        dimension,
    );
    let id = maps.free_id();
    maps.set(id, data);
    let mut stack = ItemStack::new(vanillite_data::item::ids::FILLED_MAP.0, 1);
    set_map_id(&mut stack, id);
    stack
}

/// The `map_decorations` of a stack: key, type, x, z and rotation.
pub fn static_decorations(stack: &ItemStack) -> Vec<(String, u16, f64, f64, f32)> {
    use vanillite_protocol::codec::DecodeLimited;
    let Some(value) = stack.component(components::MAP_DECORATIONS) else {
        return Vec::new();
    };
    let Ok(vanillite_protocol::types::Nbt(Some(Tag::Compound(c)))) =
        vanillite_protocol::types::Nbt::decode_limited(&mut Reader::new(value), 1 << 21)
    else {
        return Vec::new();
    };
    let number = |t: Option<&Tag>| -> f64 {
        match t {
            Some(Tag::Double(v)) => *v,
            Some(Tag::Float(v)) => f64::from(*v),
            Some(Tag::Int(v)) => f64::from(*v),
            _ => 0.0,
        }
    };
    c.iter()
        .filter_map(|(key, entry)| {
            let Tag::Compound(e) = entry else {
                return None;
            };
            let Some(Tag::String(kind)) = e.get("type") else {
                return None;
            };
            let kind = if kind.contains(':') {
                kind.clone()
            } else {
                format!("minecraft:{kind}")
            };
            let kind = vanillite_data::registry::MAP_DECORATION_TYPE.id(&kind)?;
            Some((
                key.to_string(),
                kind,
                number(e.get("x")),
                number(e.get("z")),
                number(e.get("rotation")) as f32,
            ))
        })
        .collect()
}

/// `MapItemSavedData.addTargetDecoration`: a decoration at `pos` in the stack's
/// `map_decorations`.
pub fn add_target_decoration(stack: &mut ItemStack, pos: BlockPos, key: &str, kind: &str) {
    use vanillite_protocol::codec::{DecodeLimited, Encode};
    let mut c = stack
        .component(components::MAP_DECORATIONS)
        .and_then(|v| {
            vanillite_protocol::types::Nbt::decode_limited(&mut Reader::new(v), 1 << 21).ok()
        })
        .and_then(|n| match n.0 {
            Some(Tag::Compound(c)) => Some(c),
            _ => None,
        })
        .unwrap_or_default();
    let mut entry = Compound::new();
    entry.insert("type", Tag::String(kind.to_owned()));
    entry.insert("x", Tag::Double(f64::from(pos.x)));
    entry.insert("z", Tag::Double(f64::from(pos.z)));
    entry.insert("rotation", Tag::Float(180.0));
    c.insert(key, Tag::Compound(entry));
    let mut out = Vec::new();
    vanillite_protocol::types::Nbt(Some(Tag::Compound(c))).encode(&mut out);
    stack.set_component(components::MAP_DECORATIONS, out.into_boxed_slice());
}

/// Scale and lock state of a map (what the cartography table reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapInfo {
    /// `scale`.
    pub scale: i8,
    /// `locked`.
    pub locked: bool,
}

/// `MapItem.getSavedData(stack, level)` as [`MapInfo`].
pub fn info(level: &Level, stack: &ItemStack) -> Option<MapInfo> {
    level.maps()?.with(map_id(stack)?, |data| MapInfo {
        scale: data.scale,
        locked: data.locked,
    })
}

/// `MapItem.onCraftedPostProcess`: a stack with `map_post_processing` becomes a scaled or
/// locked copy of its map under a new id.
pub fn on_crafted(level: &mut Level, stack: &mut ItemStack) {
    let Some(value) = stack
        .component(components::MAP_POST_PROCESSING)
        .map(<[u8]>::to_vec)
    else {
        return;
    };
    stack.patch_mut().unset(components::MAP_POST_PROCESSING);
    let Some(maps) = level.maps() else {
        return;
    };
    let Some(id) = map_id(stack) else {
        return;
    };
    let Some(data) = maps.with(id, |original| match value.first() {
        Some(0) => Some(original.locked()),
        Some(1) => Some(original.scaled()),
        _ => None,
    }) else {
        return;
    };
    let Some(data) = data else {
        return;
    };
    let id = maps.free_id();
    maps.set(id, data);
    set_map_id(stack, id);
}

/// `MapItem.useOn` on a banner: toggles its marker. `None` when the block is no banner (the
/// default use goes on), else whether it succeeded.
pub fn use_on_banner(level: &Level, stack: &ItemStack, pos: BlockPos) -> Option<bool> {
    if !crate::block::decor::is_banner(level.block(pos).block()) {
        return None;
    }
    let Some(id) = map_id(stack) else {
        return Some(true);
    };
    let Some(maps) = level.maps() else {
        return Some(true);
    };
    Some(
        maps.with(id, |data| data.toggle_banner(level, pos))
            .unwrap_or(true),
    )
}

/// The map colour of a state (`getMapColor`).
fn map_color(state: BlockState) -> u8 {
    state.misc().map_color
}

/// `getCorrectStateForFluidBlock`: a fluid without a sturdy top shows as its fluid.
fn fluid_corrected(state: BlockState) -> BlockState {
    let fluid = state.fluid_state();
    if !fluid.is_empty()
        && !vanillite_worldgen::behaviour::is_face_sturdy(
            state,
            vanillite_base::Direction::Up,
            vanillite_worldgen::behaviour::SupportType::Full,
        )
    {
        fluid.block_state()
    } else {
        state
    }
}

/// `MapItem.update`: the carrier's surroundings are drawn, one column of 16 per tick. Unloaded
/// chunks are skipped (Vanilla loads them).
pub fn update(
    data: &mut MapData,
    level: &Level,
    dimension: &str,
    carrier: &Carrier,
    has_ceiling: bool,
) {
    if dimension != data.dimension {
        return;
    }
    let scale = 1i32 << data.scale;
    let center_x = data.x_center;
    let center_z = data.z_center;
    let player_x = (vanillite_base::math::floor(carrier.x) - center_x) / scale + 64;
    let player_y = (vanillite_base::math::floor(carrier.z) - center_z) / scale + 64;
    let mut radius = 128 / scale;
    if has_ceiling {
        radius /= 2;
    }
    let Some(hi) = data
        .holders
        .iter()
        .position(|h| h.carrier.uuid == carrier.uuid)
    else {
        return;
    };
    data.holders[hi].step += 1;
    let step = data.holders[hi].step;
    let min_y = level.min_y();
    let mut found_changes = false;
    for img_x in (player_x - radius + 1)..(player_x + radius) {
        if (img_x & 15) != (step & 15) && !found_changes {
            continue;
        }
        found_changes = false;
        let mut previous_height = 0.0f64;
        for img_y in (player_y - radius - 1)..(player_y + radius) {
            if !(img_x >= 0 && img_y >= -1 && img_x < 128 && img_y < 128) {
                continue;
            }
            let dist =
                (img_x - player_x) * (img_x - player_x) + (img_y - player_y) * (img_y - player_y);
            let dither_black = dist > (radius - 2) * (radius - 2);
            let min_x = (center_x / scale + img_x - 64) * scale;
            let min_z = (center_z / scale + img_y - 64) * scale;
            if level
                .chunks
                .chunk(vanillite_base::ChunkPos::new(min_x >> 4, min_z >> 4))
                .is_none()
            {
                continue;
            }
            // `Multiset` in insertion order; the first with the highest count wins.
            let mut counts = [0i32; 64];
            let mut order = [0u8; 64];
            let mut distinct = 0usize;
            let mut add = |c: u8, n: i32| {
                let c = c & 63;
                if counts[usize::from(c)] == 0 {
                    order[distinct] = c;
                    distinct += 1;
                }
                counts[usize::from(c)] += n;
            };
            let mut water_depth = 0i32;
            let mut height = 0.0f64;
            if has_ceiling {
                let mut noise = min_x.wrapping_add(min_z.wrapping_mul(231_871));
                noise = noise
                    .wrapping_mul(noise)
                    .wrapping_mul(31_287_121)
                    .wrapping_add(noise.wrapping_mul(11));
                if (noise >> 20 & 1) == 0 {
                    add(color::DIRT, 10);
                } else {
                    add(color::STONE, 100);
                }
                height = 100.0;
            } else {
                for dx in 0..scale {
                    for dz in 0..scale {
                        let (x, z) = (min_x + dx, min_z + dz);
                        let mut y =
                            level.heightmap_y(HeightmapKind::WorldSurface, BlockPos::new(x, 0, z));
                        let mut state;
                        if y <= min_y {
                            state = vanillite_data::block::ids::BEDROCK.default_state();
                        } else {
                            loop {
                                y -= 1;
                                state = level.block(BlockPos::new(x, y, z));
                                if map_color(state) != color::NONE || y <= min_y {
                                    break;
                                }
                            }
                            if y > min_y && !state.fluid_state().is_empty() {
                                let mut solid_y = y - 1;
                                loop {
                                    let below = level.block(BlockPos::new(x, solid_y, z));
                                    solid_y -= 1;
                                    water_depth += 1;
                                    if !(solid_y > min_y && !below.fluid_state().is_empty()) {
                                        break;
                                    }
                                }
                                state = fluid_corrected(state);
                            }
                        }
                        data.check_banners(level, x, z);
                        height += f64::from(y) / f64::from(scale * scale);
                        add(map_color(state), 1);
                    }
                }
            }
            water_depth /= scale * scale;
            let mut best = color::NONE;
            let mut best_n = 0;
            for &c in &order[..distinct] {
                let n = counts[usize::from(c)];
                if n > best_n {
                    best = c;
                    best_n = n;
                }
            }
            let parity = f64::from((img_x + img_y) & 1);
            let brightness = if best == color::WATER {
                let diff = f64::from(water_depth) * 0.1 + parity * 0.2;
                if diff < 0.5 {
                    Brightness::High
                } else if diff > 0.9 {
                    Brightness::Low
                } else {
                    Brightness::Normal
                }
            } else {
                let diff =
                    (height - previous_height) * 4.0 / f64::from(scale + 4) + (parity - 0.5) * 0.4;
                if diff > 0.6 {
                    Brightness::High
                } else if diff < -0.6 {
                    Brightness::Low
                } else {
                    Brightness::Normal
                }
            };
            previous_height = height;
            if img_y >= 0 && dist < radius * radius && (!dither_black || (img_x + img_y) & 1 != 0) {
                found_changes |= data.update_color(img_x, img_y, packed(best, brightness));
            }
        }
    }
}

/// `MapItem.renderBiomePreviewMap`: water biomes (`#water_on_map_outlines` at `watery`)
/// outlined on an exploration map.
pub fn render_biome_preview(
    data: &mut MapData,
    sample_y: i32,
    watery: &mut dyn FnMut(BlockPos) -> bool,
) {
    let scale = 1i32 << data.scale;
    let start_x = data.x_center / scale - 64;
    let start_z = data.z_center / scale - 64;
    let mut is_watery = vec![false; 16384];
    for row in 0..128 {
        for column in 0..128 {
            let pos = BlockPos::new(
                (start_x + column) * scale,
                sample_y,
                (start_z + row) * scale,
            );
            is_watery[(row * 128 + column) as usize] = watery(pos);
        }
    }
    let at = |x: i32, z: i32| is_watery[(z * 128 + x) as usize];
    for mx in 1..127 {
        for mz in 1..127 {
            let mut water = 0;
            for dx in -1..2 {
                for dz in -1..2 {
                    if (dx != 0 || dz != 0) && at(mx + dx, mz + dz) {
                        water += 1;
                    }
                }
            }
            let mut brightness = Brightness::Lowest;
            let mut new = color::NONE;
            if at(mx, mz) {
                new = color::COLOR_ORANGE;
                if water > 7 && mz % 2 == 0 {
                    let wave =
                        (mx + (vanillite_base::math::sin(f64::from(mz)) * 7.0) as i32) / 8 % 5;
                    match wave {
                        0 | 4 => brightness = Brightness::Low,
                        1 | 3 => brightness = Brightness::Normal,
                        2 => brightness = Brightness::High,
                        _ => {}
                    }
                } else if water > 7 {
                    new = color::NONE;
                } else if water > 5 {
                    brightness = Brightness::Normal;
                } else if water > 1 {
                    brightness = Brightness::Low;
                }
            } else if water > 0 {
                new = color::COLOR_BROWN;
                brightness = if water > 3 {
                    Brightness::Normal
                } else {
                    Brightness::Lowest
                };
            }
            if new != color::NONE {
                data.set_color(mx, mz, packed(new, brightness));
            }
        }
    }
}

/// What exploration maps need of the server: the structure search and the biome tags.
pub trait Explorer: Send + Sync {
    /// `ChunkGenerator.findNearestMapStructure(level, #tag, origin, radius, skipKnown)`; with
    /// `known` (`skipKnownStructures`) it first claims the start the origin is in
    /// (`claimStructureAt`) and finds and references only unreferenced starts.
    #[allow(clippy::too_many_arguments)]
    fn nearest_structure(
        &self,
        terrain: &vanillite_worldgen::generator::TerrainGenerator,
        tag: &str,
        origin: BlockPos,
        radius: i32,
        min_y: i32,
        height: i32,
        known: Option<&crate::level::structure_refs::StructureReferences>,
    ) -> Option<BlockPos>;
    /// A test whether the biome at a position is in `#water_on_map_outlines`.
    fn water_on_map_outlines<'a>(
        &'a self,
        terrain: &'a vanillite_worldgen::generator::TerrainGenerator,
    ) -> Box<dyn FnMut(BlockPos) -> bool + 'a>;
}

static EXPLORER: std::sync::OnceLock<Box<dyn Explorer>> = std::sync::OnceLock::new();

/// Installs the structure search of exploration maps; later calls are ignored.
pub fn install_explorer(explorer: Box<dyn Explorer>) {
    let _ = EXPLORER.set(explorer);
}

/// `ExplorationMapFunction.run` with level access: a map around the nearest structure of the
/// destination tag with its biome preview and target decoration, if one is found.
#[allow(clippy::too_many_arguments)]
pub fn exploration_map(
    generation: &crate::level::Generation,
    maps: &Maps,
    min_y: i32,
    height: i32,
    sea_level: i32,
    request: &crate::loot::ExplorationMap,
    origin: vanillite_base::Vec3,
    refs: &crate::level::structure_refs::StructureReferences,
) -> Option<ItemStack> {
    let explorer = EXPLORER.get()?;
    if !generation.structures {
        return None;
    }
    let at = BlockPos::new(
        vanillite_base::math::floor(origin.x),
        vanillite_base::math::floor(origin.y),
        vanillite_base::math::floor(origin.z),
    );
    let terrain = generation.terrain();
    let found = explorer.nearest_structure(
        &terrain,
        &request.destination,
        at,
        request.search_radius,
        min_y,
        height,
        request.skip_existing_chunks.then_some(refs),
    )?;
    let mut map = create(
        maps,
        found.x,
        found.z,
        request.zoom,
        true,
        true,
        generation.dimension.clone(),
    );
    let id = map_id(&map)?;
    let mut watery = explorer.water_on_map_outlines(&terrain);
    maps.with(id, |data| {
        render_biome_preview(data, sea_level, &mut *watery)
    });
    add_target_decoration(&mut map, found, "+", &request.decoration);
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `clampMapCoordinate` adds 0.5 in double (#272 I5-10): just below a half pixel the
    /// marker stays on the lower pixel.
    #[test]
    fn clamp_map_coordinate_adds_in_double() {
        assert_eq!(
            clamp_map_coordinate(f32::from_bits(0.25_f32.to_bits() - 1)),
            0
        );
        assert_eq!(clamp_map_coordinate(0.25), 1);
        assert_eq!(clamp_map_coordinate(-10.3), -20);
        assert_eq!(clamp_map_coordinate(63.0), 127);
    }

    fn carrier(uuid: u128, x: f64) -> Carrier {
        Carrier {
            uuid,
            name: format!("p{uuid}"),
            dimension: "minecraft:overworld".into(),
            x,
            z: 0.0,
            y_rot: 0.0,
            has_map: true,
            invisible: false,
        }
    }

    /// A map in a floor or ceiling frame (`get2DDataValue() * 90` = -90, #272 I5-07): the
    /// frame marker gets rotation `(byte)(-98 * 16 / 360) & 15` = 12.
    #[test]
    fn floor_frame_marker_rotation() {
        let mut data = MapData::fresh(0.0, 0.0, 0, true, false, "minecraft:overworld".into());
        let frame = Frame {
            pos: BlockPos::new(3, 64, 5),
            rotation: -90,
            entity_id: 7,
        };
        data.tick_carried_in(&carrier(1, 0.0), &[], 0, Some(frame));
        let rots: Vec<i8> = data
            .decorations()
            .filter(|d| d.kind == decoration_type("minecraft:frame"))
            .map(|d| d.rot)
            .collect();
        assert_eq!(rots, vec![12]);
    }

    #[test]
    fn carriers_get_player_decorations_and_lose_them() {
        let mut data = MapData::fresh(0.0, 0.0, 0, true, false, "minecraft:overworld".into());
        data.tick_carried_by(&carrier(1, 0.0), &[], 0);
        data.tick_carried_by(&carrier(2, 200.0), &[], 0);
        let kinds: Vec<u16> = data.decorations().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            vec![
                decoration_type("minecraft:player"),
                decoration_type("minecraft:player_off_map")
            ]
        );
        data.carrier_gone(2);
        data.tick_carried_by(&carrier(1, 0.0), &[], 0);
        assert_eq!(data.decorations().count(), 1);
    }

    #[test]
    fn first_packet_has_all_colors_and_decorations() {
        let mut data = MapData::fresh(0.0, 0.0, 0, true, false, "minecraft:overworld".into());
        data.tick_carried_by(&carrier(1, 0.0), &[], 0);
        let p = data.next_update_packet(1, 7).expect("packet");
        let patch = p.patch.expect("patch");
        assert_eq!((patch.width, patch.height), (128, 128));
        assert_eq!(p.decorations.map(|d| d.len()), Some(1));
        assert!(data.next_update_packet(1, 7).is_none());
        data.update_color(3, 4, 5);
        let p = data.next_update_packet(1, 7).expect("packet");
        let patch = p.patch.expect("patch");
        assert_eq!((patch.start_x, patch.start_y, patch.width), (3, 4, 1));
    }

    #[test]
    fn saves_banners() {
        let mut data = MapData::fresh(0.0, 0.0, 0, true, false, "minecraft:overworld".into());
        data.banners.push(Banner {
            pos: BlockPos::new(1, 2, 3),
            color: 14,
            name: None,
        });
        let loaded = MapData::load(&data.save()).expect("loads");
        assert_eq!(loaded.banners, data.banners);
        assert_eq!(loaded.decorations().count(), 1);
    }
}
