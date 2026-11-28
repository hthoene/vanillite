//! Points of interest (`PoiManager`, `PoiSection`, `PoiRecord`, `PoiTypes`): job sites, beds,
//! bells, bee homes, portals, lodestones and lightning rods with their claim tickets, and the
//! village distance of every section (`PoiManager.DistanceTracker`).
//!
//! - The manager keeps the sections of the live chunks. A chunk that becomes live loads its
//!   stored record (Vanilla's `poi/`) and checks it against its blocks like
//!   `PoiManager.checkConsistencyWithBlocks`: a missing section is scanned when its palette
//!   may hold a POI state, an invalid one is rebuilt keeping the tickets of the records that
//!   stay. Before a save or unload the level writes the chunk's sections back.
//! - Block changes reach the manager through [`crate::level::Level`]
//!   (`updatePOIOnBlockStateChange`).
//! - Queries visit the records in Vanilla's order (`getInSquare`: chunks x first, then z;
//!   sections bottom up; per section the types, then each type's `HashSet` in Java's hash
//!   order), so `findFirst`, `min` ties and shuffles pick the same records.
//! - Vanilla loads the POI of chunks that are not loaded from disk on demand; here only the
//!   live chunks have POI.
//! - The village distance (`sectionsToVillage`) is the Chebyshev distance in sections to the
//!   nearest section with an occupied village POI, at most 7 (what the fixed point of
//!   `DistanceTracker` computes).

use vanillite_base::hash::{FxHashMap, FxHashSet};
use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, ChunkPos, SectionPos};
use vanillite_data::block::BlockState;
use vanillite_nbt::{Compound, List, Tag};

/// Declares the POI types with their name, maximum tickets and valid range.
macro_rules! poi_types {
    ($($ty:ident $name:literal $tickets:literal $range:literal;)*) => {
        /// `PoiTypes`, in registration order.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u8)]
        pub enum PoiType {
            $($ty,)*
        }

        impl PoiType {
            /// All types in registration order.
            pub const ALL: &[PoiType] = &[$(PoiType::$ty,)*];

            /// The id without namespace.
            pub fn name(self) -> &'static str {
                match self {
                    $(PoiType::$ty => $name,)*
                }
            }

            /// `maxTickets`.
            pub fn max_tickets(self) -> i32 {
                match self {
                    $(PoiType::$ty => $tickets,)*
                }
            }

            /// `validRange`.
            pub fn valid_range(self) -> i32 {
                match self {
                    $(PoiType::$ty => $range,)*
                }
            }
        }
    };
}

poi_types! {
    Armorer "armorer" 1 1;
    Butcher "butcher" 1 1;
    Cartographer "cartographer" 1 1;
    Cleric "cleric" 1 1;
    Farmer "farmer" 1 1;
    Fisherman "fisherman" 1 1;
    Fletcher "fletcher" 1 1;
    Leatherworker "leatherworker" 1 1;
    Librarian "librarian" 1 1;
    Mason "mason" 1 1;
    Shepherd "shepherd" 1 1;
    Toolsmith "toolsmith" 1 1;
    Weaponsmith "weaponsmith" 1 1;
    Home "home" 1 1;
    Meeting "meeting" 32 6;
    Beehive "beehive" 0 1;
    BeeNest "bee_nest" 0 1;
    NetherPortal "nether_portal" 0 1;
    Lodestone "lodestone" 0 1;
    TestInstance "test_instance" 0 1;
    LightningRod "lightning_rod" 0 1;
}

impl PoiType {
    /// `minecraft:<name>` or `<name>` to the type.
    pub fn by_name(name: &str) -> Option<PoiType> {
        let name = name.strip_prefix("minecraft:").unwrap_or(name);
        PoiType::ALL.iter().copied().find(|t| t.name() == name)
    }

    /// `#acquirable_job_site`: the workstations of the 13 professions.
    pub fn is_job_site(self) -> bool {
        (self as u8) <= PoiType::Weaponsmith as u8
    }

    /// `#village`: job sites, beds and bells.
    pub fn is_village(self) -> bool {
        self.is_job_site() || matches!(self, PoiType::Home | PoiType::Meeting)
    }

    /// `#bee_home`.
    pub fn is_bee_home(self) -> bool {
        matches!(self, PoiType::Beehive | PoiType::BeeNest)
    }

    /// `PoiTypes.forState`.
    pub fn of_state(state: BlockState) -> Option<PoiType> {
        let table = by_state();
        match table.get(usize::from(state.0)).copied() {
            Some(NONE) | None => None,
            Some(i) => PoiType::ALL.get(usize::from(i)).copied(),
        }
    }

    /// `PoiTypes.hasPoi`.
    pub fn has_poi(state: BlockState) -> bool {
        by_state()
            .get(usize::from(state.0))
            .is_some_and(|&i| i != NONE)
    }
}

const NONE: u8 = u8::MAX;

/// The type of every block state (`TYPE_BY_STATE`).
fn by_state() -> &'static [u8] {
    static TABLE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = vec![NONE; vanillite_data::block::STATE_COUNT];
        for info in vanillite_data::block::BLOCKS.iter() {
            let name = info.name.strip_prefix("minecraft:").unwrap_or(info.name);
            let ty = match name {
                "blast_furnace" => PoiType::Armorer,
                "smoker" => PoiType::Butcher,
                "cartography_table" => PoiType::Cartographer,
                "brewing_stand" => PoiType::Cleric,
                "composter" => PoiType::Farmer,
                "barrel" => PoiType::Fisherman,
                "fletching_table" => PoiType::Fletcher,
                "cauldron" | "lava_cauldron" | "water_cauldron" | "powder_snow_cauldron" => {
                    PoiType::Leatherworker
                }
                "lectern" => PoiType::Librarian,
                "stonecutter" => PoiType::Mason,
                "loom" => PoiType::Shepherd,
                "smithing_table" => PoiType::Toolsmith,
                "grindstone" => PoiType::Weaponsmith,
                "bell" => PoiType::Meeting,
                "beehive" => PoiType::Beehive,
                "bee_nest" => PoiType::BeeNest,
                "nether_portal" => PoiType::NetherPortal,
                "lodestone" => PoiType::Lodestone,
                "test_instance_block" => PoiType::TestInstance,
                n if n.ends_with("_bed") => PoiType::Home,
                n if n.ends_with("lightning_rod") => PoiType::LightningRod,
                _ => continue,
            };
            for s in info.first_state..info.first_state + info.state_count {
                let state = BlockState(s);
                if ty == PoiType::Home && state.value("part") != Some("head") {
                    continue;
                }
                table[usize::from(s)] = ty as u8;
            }
        }
        table
    })
}

/// `PoiManager.Occupancy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occupancy {
    /// `HAS_SPACE`: a ticket is free.
    HasSpace,
    /// `IS_OCCUPIED`: a ticket is taken.
    IsOccupied,
    Any,
}

/// `PoiRecord`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoiRecord {
    pub pos: BlockPos,
    pub ty: PoiType,
    pub free_tickets: i32,
}

impl PoiRecord {
    fn new(pos: BlockPos, ty: PoiType) -> PoiRecord {
        PoiRecord {
            pos,
            ty,
            free_tickets: ty.max_tickets(),
        }
    }

    /// `hasSpace`.
    pub fn has_space(&self) -> bool {
        self.free_tickets > 0
    }

    /// `isOccupied`.
    pub fn is_occupied(&self) -> bool {
        self.free_tickets != self.ty.max_tickets()
    }

    fn matches(&self, occupancy: Occupancy) -> bool {
        match occupancy {
            Occupancy::HasSpace => self.has_space(),
            Occupancy::IsOccupied => self.is_occupied(),
            Occupancy::Any => true,
        }
    }
}

/// `Vec3i.hashCode` spread like `HashMap.hash`.
fn java_hash(pos: BlockPos) -> u32 {
    let h = pos
        .y
        .wrapping_add(pos.z.wrapping_mul(31))
        .wrapping_mul(31)
        .wrapping_add(pos.x) as u32;
    h ^ (h >> 16)
}

/// One type's `HashSet<PoiRecord>` of a section: the records with their insertion order and
/// the table size, to visit them in Java's iteration order.
#[derive(Clone, Debug, Default)]
struct TypeSet {
    /// Record index into the section and insertion sequence.
    entries: Vec<(u16, u64)>,
    /// `HashMap` table length (0 before the first insertion).
    capacity: usize,
}

impl TypeSet {
    fn insert(&mut self, index: u16, seq: u64) {
        if self.capacity == 0 {
            self.capacity = 16;
        }
        self.entries.push((index, seq));
        // `HashMap.putVal`: resize when the size passes 3/4 of the table.
        if self.entries.len() > self.capacity / 4 * 3 {
            self.capacity *= 2;
        }
    }
}

/// `PoiSection`.
#[derive(Clone, Debug, Default)]
struct PoiSection {
    valid: bool,
    /// By section-relative index (`SectionPos.sectionRelativePos`).
    records: FxHashMap<u16, PoiRecord>,
    /// `byType`, in type order.
    by_type: Vec<(PoiType, TypeSet)>,
    seq: u64,
}

fn relative(pos: BlockPos) -> u16 {
    ((pos.x & 15) << 8 | (pos.z & 15) << 4 | (pos.y & 15)) as u16
}

impl PoiSection {
    fn new_valid() -> PoiSection {
        PoiSection {
            valid: true,
            ..PoiSection::default()
        }
    }

    /// `add(record)`: `false` if a record of the type is already there.
    fn add_record(&mut self, record: PoiRecord) -> bool {
        let key = relative(record.pos);
        if let Some(old) = self.records.get(&key) {
            if old.ty == record.ty {
                return false;
            }
            log::warn!("POI data mismatch: already registered at {:?}", record.pos);
            self.unlink(key, old.ty);
        }
        self.records.insert(key, record);
        self.seq += 1;
        let seq = self.seq;
        let set = match self.by_type.iter().position(|(t, _)| *t == record.ty) {
            Some(i) => &mut self.by_type[i].1,
            None => {
                let at = self
                    .by_type
                    .iter()
                    .position(|(t, _)| *t > record.ty)
                    .unwrap_or(self.by_type.len());
                self.by_type.insert(at, (record.ty, TypeSet::default()));
                &mut self.by_type[at].1
            }
        };
        set.insert(key, seq);
        true
    }

    fn unlink(&mut self, key: u16, ty: PoiType) {
        if let Some((_, set)) = self.by_type.iter_mut().find(|(t, _)| *t == ty) {
            set.entries.retain(|(k, _)| *k != key);
        }
    }

    fn remove(&mut self, pos: BlockPos) -> bool {
        let key = relative(pos);
        match self.records.remove(&key) {
            Some(r) => {
                self.unlink(key, r.ty);
                true
            }
            None => {
                log::warn!("POI data mismatch: never registered at {pos:?}");
                false
            }
        }
    }

    fn clear(&mut self) {
        self.records.clear();
        self.by_type.clear();
    }

    /// `getRecords(predicate, occupancy)` in Vanilla's order.
    fn visit(
        &self,
        pred: &dyn Fn(PoiType) -> bool,
        occupancy: Occupancy,
        out: &mut Vec<PoiRecord>,
    ) {
        for (ty, set) in &self.by_type {
            if !pred(*ty) {
                continue;
            }
            let mask = set.capacity.saturating_sub(1) as u32;
            let mut order: Vec<(u32, u64, u16)> = set
                .entries
                .iter()
                .filter_map(|&(k, seq)| {
                    let r = self.records.get(&k)?;
                    Some((java_hash(r.pos) & mask, seq, k))
                })
                .collect();
            order.sort_unstable();
            for (_, _, k) in order {
                if let Some(r) = self.records.get(&k)
                    && r.matches(occupancy)
                {
                    out.push(*r);
                }
            }
        }
    }

    fn is_village_center(&self) -> bool {
        self.records
            .values()
            .any(|r| r.ty.is_village() && r.is_occupied())
    }
}

/// `PoiManager` of one dimension.
#[derive(Clone, Debug, Default)]
pub struct PoiManager {
    /// Whether this is the level's manager (not the empty one left while the entities are
    /// taken out for a tick).
    pub live: bool,
    sections: FxHashMap<SectionPos, PoiSection>,
    /// The chunks whose POI are loaded, with the unknown tags of their record.
    chunks: FxHashMap<ChunkPos, Compound>,
    /// Chunks with changes not yet written back.
    dirty: FxHashSet<ChunkPos>,
    /// Village center sections by chunk column: their section y.
    centers: FxHashMap<ChunkPos, Vec<i32>>,
    /// Lodestones removed since the server started: a compass far away finds its lodestone
    /// gone while the chunk is not loaded (Vanilla loads the POI section from disk).
    removed_lodestones: FxHashSet<BlockPos>,
}

impl PoiManager {
    /// The level's manager.
    pub fn new_live() -> PoiManager {
        PoiManager {
            live: true,
            ..PoiManager::default()
        }
    }

    fn set_dirty(&mut self, section: SectionPos) {
        self.dirty.insert(section.chunk());
        self.update_center(section);
    }

    fn update_center(&mut self, section: SectionPos) {
        let center = self
            .sections
            .get(&section)
            .is_some_and(PoiSection::is_village_center);
        let column = self.centers.entry(section.chunk()).or_default();
        let has = column.contains(&section.y);
        if center && !has {
            column.push(section.y);
        } else if !center && has {
            column.retain(|&y| y != section.y);
        }
        if column.is_empty() {
            self.centers.remove(&section.chunk());
        }
    }

    /// Whether the POI of the chunk are loaded.
    pub fn is_loaded(&self, chunk: ChunkPos) -> bool {
        self.chunks.contains_key(&chunk)
    }

    /// `add(pos, type)`: `false` if the chunk is not loaded or the POI is already there.
    pub fn add(&mut self, pos: BlockPos, ty: PoiType) -> bool {
        if !self.is_loaded(pos.chunk()) {
            return false;
        }
        let section = pos.section();
        let added = self
            .sections
            .entry(section)
            .or_insert_with(PoiSection::new_valid)
            .add_record(PoiRecord::new(pos, ty));
        if added {
            self.set_dirty(section);
            if ty == PoiType::Lodestone {
                self.removed_lodestones.remove(&pos);
            }
        }
        added
    }

    /// `remove(pos)`.
    pub fn remove(&mut self, pos: BlockPos) {
        let section = pos.section();
        let lodestone = self.get_type(pos) == Some(PoiType::Lodestone);
        if let Some(s) = self.sections.get_mut(&section)
            && s.remove(pos)
        {
            self.set_dirty(section);
            if lodestone {
                self.removed_lodestones.insert(pos);
            }
        }
    }

    /// `existsAtPosition(LODESTONE, pos)` of a chunk whose POI are not loaded: `false` once
    /// the lodestone there was removed (until one is placed there again).
    pub fn lodestone_removed(&self, pos: BlockPos) -> bool {
        self.removed_lodestones.contains(&pos)
    }

    fn record_mut(&mut self, pos: BlockPos) -> Option<&mut PoiRecord> {
        self.sections
            .get_mut(&pos.section())?
            .records
            .get_mut(&relative(pos))
    }

    /// The record at a position.
    pub fn record(&self, pos: BlockPos) -> Option<&PoiRecord> {
        self.sections
            .get(&pos.section())?
            .records
            .get(&relative(pos))
    }

    /// `acquireTicket` of the record at `pos`.
    pub fn acquire(&mut self, pos: BlockPos) -> bool {
        let Some(r) = self.record_mut(pos) else {
            return false;
        };
        if r.free_tickets <= 0 {
            return false;
        }
        r.free_tickets -= 1;
        self.set_dirty(pos.section());
        true
    }

    /// `release(pos)`: frees a ticket; `false` when none was taken or there is no POI (Vanilla
    /// throws for a position without a POI).
    pub fn release(&mut self, pos: BlockPos) -> bool {
        let Some(r) = self.record_mut(pos) else {
            log::warn!("POI never registered at {pos:?}");
            return false;
        };
        let freed = r.free_tickets < r.ty.max_tickets();
        if freed {
            r.free_tickets += 1;
        }
        self.set_dirty(pos.section());
        freed
    }

    /// `getType(pos)`.
    pub fn get_type(&self, pos: BlockPos) -> Option<PoiType> {
        self.record(pos).map(|r| r.ty)
    }

    /// `exists(pos, predicate)`.
    pub fn exists(&self, pos: BlockPos, pred: impl Fn(PoiType) -> bool) -> bool {
        self.get_type(pos).is_some_and(pred)
    }

    /// `getInChunk(predicate, chunk, occupancy)`, appended to `out`.
    fn in_chunk(
        &self,
        pred: &dyn Fn(PoiType) -> bool,
        chunk: ChunkPos,
        occupancy: Occupancy,
        out: &mut Vec<PoiRecord>,
    ) {
        if !self.is_loaded(chunk) {
            return;
        }
        let mut ys: Vec<i32> = self
            .sections
            .keys()
            .filter(|s| s.x == chunk.x && s.z == chunk.z)
            .map(|s| s.y)
            .collect();
        ys.sort_unstable();
        for y in ys {
            if let Some(s) = self.sections.get(&SectionPos::new(chunk.x, y, chunk.z)) {
                s.visit(pred, occupancy, out);
            }
        }
    }

    /// `getInSquare(predicate, center, radius, occupancy)` in Vanilla's order.
    pub fn in_square(
        &self,
        pred: impl Fn(PoiType) -> bool,
        center: BlockPos,
        radius: i32,
        occupancy: Occupancy,
    ) -> Vec<PoiRecord> {
        let chunks = radius.div_euclid(16) + 1;
        let c = center.chunk();
        let mut out = Vec::new();
        for z in c.z - chunks..=c.z + chunks {
            for x in c.x - chunks..=c.x + chunks {
                self.in_chunk(&pred, ChunkPos::new(x, z), occupancy, &mut out);
            }
        }
        out.retain(|r| {
            (r.pos.x - center.x).abs() <= radius && (r.pos.z - center.z).abs() <= radius
        });
        out
    }

    /// `getInRange(predicate, center, radius, occupancy)`.
    pub fn in_range(
        &self,
        pred: impl Fn(PoiType) -> bool,
        center: BlockPos,
        radius: i32,
        occupancy: Occupancy,
    ) -> Vec<PoiRecord> {
        let r2 = f64::from(radius * radius);
        let mut out = self.in_square(pred, center, radius, occupancy);
        out.retain(|r| r.pos.dist_sqr(center) <= r2);
        out
    }

    /// `getCountInRange`.
    pub fn count_in_range(
        &self,
        pred: impl Fn(PoiType) -> bool,
        center: BlockPos,
        radius: i32,
        occupancy: Occupancy,
    ) -> usize {
        self.in_range(pred, center, radius, occupancy).len()
    }

    /// `find(predicate, filter, center, radius, occupancy)`: the first in order.
    pub fn find(
        &self,
        pred: impl Fn(PoiType) -> bool,
        filter: impl FnMut(BlockPos) -> bool,
        center: BlockPos,
        radius: i32,
        occupancy: Occupancy,
    ) -> Option<BlockPos> {
        let mut filter = filter;
        self.in_range(pred, center, radius, occupancy)
            .into_iter()
            .map(|r| r.pos)
            .find(|&p| filter(p))
    }

    /// `findClosest(predicate, filter, center, radius, occupancy)` (`Stream.min`: the first of
    /// equally close ones).
    pub fn find_closest(
        &self,
        pred: impl Fn(PoiType) -> bool,
        filter: impl FnMut(BlockPos) -> bool,
        center: BlockPos,
        radius: i32,
        occupancy: Occupancy,
    ) -> Option<BlockPos> {
        self.find_closest_with_type(pred, filter, center, radius, occupancy)
            .map(|(_, p)| p)
    }

    /// `findClosestWithType` with a position filter.
    pub fn find_closest_with_type(
        &self,
        pred: impl Fn(PoiType) -> bool,
        filter: impl FnMut(BlockPos) -> bool,
        center: BlockPos,
        radius: i32,
        occupancy: Occupancy,
    ) -> Option<(PoiType, BlockPos)> {
        let mut filter = filter;
        let mut best: Option<(f64, PoiType, BlockPos)> = None;
        for r in self.in_range(pred, center, radius, occupancy) {
            if !filter(r.pos) {
                continue;
            }
            let d = r.pos.dist_sqr(center);
            if best.is_none_or(|(b, _, _)| d < b) {
                best = Some((d, r.ty, r.pos));
            }
        }
        best.map(|(_, t, p)| (t, p))
    }

    /// `findAllClosestFirstWithType` (a stable sort by distance).
    pub fn find_all_closest_first_with_type(
        &self,
        pred: impl Fn(PoiType) -> bool,
        filter: impl FnMut(BlockPos) -> bool,
        center: BlockPos,
        radius: i32,
        occupancy: Occupancy,
    ) -> Vec<(PoiType, BlockPos)> {
        let mut filter = filter;
        let mut out: Vec<(PoiType, BlockPos)> = self
            .in_range(pred, center, radius, occupancy)
            .into_iter()
            .filter(|r| filter(r.pos))
            .map(|r| (r.ty, r.pos))
            .collect();
        out.sort_by(|a, b| a.1.dist_sqr(center).total_cmp(&b.1.dist_sqr(center)));
        out
    }

    /// `take(predicate, filter, center, radius)`: claims a ticket of the first free POI.
    pub fn take(
        &mut self,
        pred: impl Fn(PoiType) -> bool,
        filter: impl FnMut(PoiType, BlockPos) -> bool,
        center: BlockPos,
        radius: i32,
    ) -> Option<BlockPos> {
        let mut filter = filter;
        let pos = self
            .in_range(pred, center, radius, Occupancy::HasSpace)
            .into_iter()
            .find(|r| filter(r.ty, r.pos))?
            .pos;
        self.acquire(pos);
        Some(pos)
    }

    /// `getRandom(predicate, filter, occupancy, center, radius, random)`
    /// (`Util.toShuffledList`, then the first that passes).
    pub fn get_random(
        &self,
        pred: impl Fn(PoiType) -> bool,
        filter: impl FnMut(BlockPos) -> bool,
        occupancy: Occupancy,
        center: BlockPos,
        radius: i32,
        random: &mut impl RandomSource,
    ) -> Option<BlockPos> {
        let mut filter = filter;
        let mut list = self.in_range(pred, center, radius, occupancy);
        shuffle(&mut list, random);
        list.into_iter().map(|r| r.pos).find(|&p| filter(p))
    }

    // --- Village distance --------------------------------------------------------------

    /// `sectionsToVillage(section)`: 0 in a village center, up to 7 (no village near).
    pub fn sections_to_village(&self, section: SectionPos) -> i32 {
        let mut best = 7;
        for dz in -6..=6 {
            for dx in -6..=6 {
                let Some(ys) = self
                    .centers
                    .get(&ChunkPos::new(section.x + dx, section.z + dz))
                else {
                    continue;
                };
                for &y in ys {
                    let d = dx.abs().max(dz.abs()).max((y - section.y).abs());
                    best = best.min(d);
                }
            }
        }
        best
    }

    /// `ServerLevel.isCloseToVillage(pos, sections)`.
    pub fn is_close_to_village(&self, pos: BlockPos, sections: i32) -> bool {
        sections <= 6 && self.sections_to_village(pos.section()) <= sections
    }

    /// `ServerLevel.isVillage(pos)`.
    pub fn is_village(&self, pos: BlockPos) -> bool {
        self.is_close_to_village(pos, 1)
    }

    /// `ServerLevel.isVillage(sectionPos)`.
    pub fn is_village_section(&self, section: SectionPos) -> bool {
        self.sections_to_village(section) <= 1
    }

    // --- Chunk life cycle ----------------------------------------------------------------

    /// A chunk became live: its stored record (the root without `DataVersion`) and the
    /// block states of its sections (`checkConsistencyWithBlocks`). `sections` yields the
    /// section y and a test whether its palette may hold a POI state together with a reader of
    /// its 4096 states (`x << 8 | z << 4 | y` order is not required: the reader gets the
    /// relative coordinates).
    pub fn load_chunk(
        &mut self,
        chunk: ChunkPos,
        stored: Option<Compound>,
        sections: &mut dyn Iterator<Item = (i32, &dyn ChunkSectionBlocks)>,
    ) {
        if self.is_loaded(chunk) {
            return;
        }
        let mut extra = stored.unwrap_or_default();
        let mut unpacked: FxHashMap<i32, PoiSection> = FxHashMap::default();
        if let Some(Tag::Compound(stored)) = extra.remove("Sections") {
            for (key, tag) in stored.iter() {
                let (Ok(y), Some(data)) = (key.parse::<i32>(), tag.as_compound()) else {
                    continue;
                };
                unpacked.insert(y, unpack_section(chunk, y, data));
            }
        }
        self.chunks.insert(chunk, extra);
        let mut changed = false;
        for (y, blocks) in sections {
            let section_pos = SectionPos::new(chunk.x, y, chunk.z);
            let may = blocks.may_have_poi();
            match unpacked.remove(&y) {
                Some(mut s) => {
                    if !s.valid {
                        // `PoiSection.refresh`: rebuilt from the blocks, keeping the records
                        // (and their tickets) of positions that still have a POI.
                        let old: FxHashMap<u16, PoiRecord> = std::mem::take(&mut s.records);
                        s.clear();
                        if may {
                            scan(section_pos, blocks, &mut |pos, ty| {
                                let record = old
                                    .get(&relative(pos))
                                    .copied()
                                    .unwrap_or_else(|| PoiRecord::new(pos, ty));
                                s.add_record(record);
                            });
                        }
                        s.valid = true;
                        changed = true;
                    }
                    self.sections.insert(section_pos, s);
                }
                None => {
                    if may {
                        let mut s = PoiSection::new_valid();
                        scan(section_pos, blocks, &mut |pos, ty| {
                            s.add_record(PoiRecord::new(pos, ty));
                        });
                        self.sections.insert(section_pos, s);
                        changed = true;
                    }
                }
            }
            self.update_center(section_pos);
        }
        // Sections outside the height stay as stored.
        for (y, s) in unpacked {
            let section_pos = SectionPos::new(chunk.x, y, chunk.z);
            self.sections.insert(section_pos, s);
            self.update_center(section_pos);
        }
        if changed {
            self.dirty.insert(chunk);
        }
    }

    /// Whether the chunk has POI changes not yet written back.
    pub fn is_dirty(&self, chunk: ChunkPos) -> bool {
        self.dirty.contains(&chunk)
    }

    /// The chunks with POI changes not yet written back.
    pub fn dirty_chunks(&self) -> Vec<ChunkPos> {
        let mut v: Vec<ChunkPos> = self.dirty.iter().copied().collect();
        v.sort_unstable_by_key(|c| (c.x, c.z));
        v
    }

    /// The record of a loaded chunk (`SectionStorage.writeChunk`) without `DataVersion`;
    /// `None` without sections. Clears the chunk's dirty mark.
    pub fn save_chunk(&mut self, chunk: ChunkPos) -> Option<Compound> {
        self.dirty.remove(&chunk);
        let extra = self.chunks.get(&chunk)?;
        let mut ys: Vec<i32> = self
            .sections
            .keys()
            .filter(|s| s.x == chunk.x && s.z == chunk.z)
            .map(|s| s.y)
            .collect();
        if ys.is_empty() && extra.is_empty() {
            return None;
        }
        ys.sort_unstable();
        let mut sections = Compound::new();
        for y in ys {
            let Some(s) = self.sections.get(&SectionPos::new(chunk.x, y, chunk.z)) else {
                continue;
            };
            let mut records: Vec<&PoiRecord> = s.records.values().collect();
            records.sort_unstable_by_key(|r| relative(r.pos));
            let list: Vec<Tag> = records
                .into_iter()
                .map(|r| {
                    // `optionalFieldOf(.., 0)`: no `free_tickets` when 0.
                    let mut c = Compound::new()
                        .with("type", format!("minecraft:{}", r.ty.name()))
                        .with("pos", Tag::IntArray(vec![r.pos.x, r.pos.y, r.pos.z]));
                    if r.free_tickets != 0 {
                        c.insert("free_tickets", r.free_tickets);
                    }
                    Tag::Compound(c)
                })
                .collect();
            let mut section = Compound::new().with("Records", List(list));
            if s.valid {
                section.insert("Valid", true);
            }
            sections.insert(y.to_string(), section);
        }
        let mut root = extra.clone();
        root.insert("Sections", sections);
        Some(root)
    }

    /// Forgets a chunk that stopped being live (after [`Self::save_chunk`]).
    pub fn unload_chunk(&mut self, chunk: ChunkPos) {
        if self.chunks.remove(&chunk).is_none() {
            return;
        }
        self.sections
            .retain(|s, _| s.x != chunk.x || s.z != chunk.z);
        self.centers.remove(&chunk);
        self.dirty.remove(&chunk);
    }
}

/// The blocks of a chunk section as the manager scans them.
pub trait ChunkSectionBlocks {
    /// `LevelChunkSection.maybeHas(PoiTypes::hasPoi)`.
    fn may_have_poi(&self) -> bool;
    /// The state at section-relative coordinates.
    fn state(&self, x: u32, y: u32, z: u32) -> BlockState;
}

/// `updateFromSection`: every POI state of the section in `SectionPos.blocksInside` order
/// (`BlockPos.betweenClosed`: x fastest, then y, then z).
fn scan(
    section: SectionPos,
    blocks: &dyn ChunkSectionBlocks,
    out: &mut dyn FnMut(BlockPos, PoiType),
) {
    let origin = section.origin();
    for z in 0..16u32 {
        for y in 0..16u32 {
            for x in 0..16u32 {
                if let Some(ty) = PoiType::of_state(blocks.state(x, y, z)) {
                    #[allow(clippy::cast_possible_wrap)]
                    out(origin.offset(x as i32, y as i32, z as i32), ty);
                }
            }
        }
    }
}

fn unpack_section(chunk: ChunkPos, y: i32, data: &Compound) -> PoiSection {
    let mut s = PoiSection {
        valid: matches!(data.get("Valid"), Some(Tag::Byte(b)) if *b != 0),
        ..PoiSection::default()
    };
    if let Some(Tag::List(list)) = data.get("Records") {
        for r in list.0.iter().filter_map(Tag::as_compound) {
            let pos = match r.get("pos") {
                Some(Tag::IntArray(v)) if v.len() == 3 => BlockPos::new(v[0], v[1], v[2]),
                _ => continue,
            };
            if pos.section() != SectionPos::new(chunk.x, y, chunk.z) {
                continue;
            }
            let Some(ty) = r
                .get("type")
                .and_then(Tag::as_str)
                .and_then(PoiType::by_name)
            else {
                continue;
            };
            let free_tickets = match r.get("free_tickets") {
                Some(Tag::Int(v)) => *v,
                _ => 0,
            };
            s.add_record(PoiRecord {
                pos,
                ty,
                free_tickets,
            });
        }
    }
    s
}

/// `Util.shuffle(list, random)`.
pub fn shuffle<T>(list: &mut [T], random: &mut impl RandomSource) {
    let mut i = list.len();
    while i > 1 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let j = random.next_int_bounded(i as i32) as usize;
        list.swap(i - 1, j);
        i -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Blocks(Vec<(u32, u32, u32, BlockState)>);

    impl ChunkSectionBlocks for Blocks {
        fn may_have_poi(&self) -> bool {
            !self.0.is_empty()
        }
        fn state(&self, x: u32, y: u32, z: u32) -> BlockState {
            self.0
                .iter()
                .find(|b| (b.0, b.1, b.2) == (x, y, z))
                .map_or(BlockState::AIR, |b| b.3)
        }
    }

    fn state(name: &str) -> BlockState {
        vanillite_data::block::BlockId::by_name(&format!("minecraft:{name}"))
            .expect("block")
            .default_state()
    }

    #[test]
    fn types_of_states() {
        assert_eq!(PoiType::of_state(state("composter")), Some(PoiType::Farmer));
        assert_eq!(PoiType::of_state(state("bell")), Some(PoiType::Meeting));
        let bed = state("red_bed");
        let head = bed.with("part", "head").expect("head");
        let foot = bed.with("part", "foot").expect("foot");
        assert_eq!(PoiType::of_state(head), Some(PoiType::Home));
        assert_eq!(PoiType::of_state(foot), None);
        assert_eq!(PoiType::of_state(state("stone")), None);
    }

    #[test]
    fn tickets_and_village_distance() {
        let mut m = PoiManager::new_live();
        let chunk = ChunkPos::new(0, 0);
        let blocks = Blocks(vec![
            (3, 4, 5, state("composter")),
            (1, 1, 1, state("bell")),
        ]);
        let mut sections = std::iter::once((4, &blocks as &dyn ChunkSectionBlocks));
        m.load_chunk(chunk, None, &mut sections);
        let farmer = BlockPos::new(3, 68, 5);
        assert_eq!(m.get_type(farmer), Some(PoiType::Farmer));
        assert_eq!(m.sections_to_village(SectionPos::new(0, 4, 0)), 7);
        let taken = m.take(
            PoiType::is_job_site,
            |_, _| true,
            BlockPos::new(0, 64, 0),
            48,
        );
        assert_eq!(taken, Some(farmer));
        assert_eq!(m.sections_to_village(SectionPos::new(0, 4, 0)), 0);
        assert_eq!(m.sections_to_village(SectionPos::new(2, 5, -3)), 3);
        assert!(
            m.take(PoiType::is_job_site, |_, _| true, farmer, 48)
                .is_none()
        );
        assert!(m.release(farmer));
        assert_eq!(m.sections_to_village(SectionPos::new(0, 4, 0)), 7);
        let saved = m.save_chunk(chunk).expect("saved");
        let mut n = PoiManager::new_live();
        n.load_chunk(chunk, Some(saved.clone()), &mut std::iter::empty());
        assert_eq!(n.save_chunk(chunk), Some(saved));
    }

    #[test]
    fn java_hash_set_order() {
        // `HashSet` of 3 positions in one bucket table of 16: ordered by the spread hash.
        let mut s = PoiSection::new_valid();
        for pos in [
            BlockPos::new(5, 0, 0),
            BlockPos::new(1, 0, 0),
            BlockPos::new(3, 0, 0),
        ] {
            s.add_record(PoiRecord::new(pos, PoiType::Home));
        }
        let mut out = Vec::new();
        s.visit(&|_| true, Occupancy::Any, &mut out);
        let xs: Vec<i32> = out.iter().map(|r| r.pos.x).collect();
        assert_eq!(xs, vec![1, 3, 5]);
    }
}
