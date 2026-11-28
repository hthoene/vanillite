//! Entities per section, in Vanilla's order.

use rustc_hash::FxHashMap;
use vanillite_base::Aabb;
use vanillite_base::SectionPos;
use vanillite_base::math::floor;
use vanillite_data::entity::EntityTypeId;

use super::EntityId;

/// The classes an entity is indexed under besides "all", so that typed queries skip the
/// others (like Vanilla's `ClassInstanceMultiMap`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityClass {
    /// Players.
    Player,
    /// Everything else.
    Other,
}

/// Which entities of a section a query visits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Subset {
    /// Every entity.
    All,
    /// Only players.
    Players,
    /// Everything but players.
    Others,
}

/// The entities of one section, in insertion order (Vanilla keeps `ArrayList` order); the
/// class lists keep that order too.
#[derive(Debug, Default)]
struct Section {
    all: Vec<EntityId>,
    /// The bounding boxes of `all`, in the same order: box queries test them in one sweep
    /// instead of looking up every entity.
    boxes: Boxes,
    /// The types of `all`, in the same order: typed queries skip the others without looking
    /// up the entities (Vanilla's `ClassInstanceMultiMap.find`).
    kinds: Vec<EntityTypeId>,
    /// The types in `kinds`: typed queries skip sections without any of theirs.
    present: KindSet,
    players: Vec<EntityId>,
    others: Vec<EntityId>,
}

/// Bounding boxes as one column per coordinate, so that a box query compares eight boxes at
/// once (the sweep over a crowded section is the hot part of `pushEntities` in mob farms).
#[derive(Debug, Default)]
struct Boxes {
    min_x: Vec<f64>,
    min_y: Vec<f64>,
    min_z: Vec<f64>,
    max_x: Vec<f64>,
    max_y: Vec<f64>,
    max_z: Vec<f64>,
}

impl Boxes {
    fn push(&mut self, b: Aabb) {
        self.min_x.push(b.min_x);
        self.min_y.push(b.min_y);
        self.min_z.push(b.min_z);
        self.max_x.push(b.max_x);
        self.max_y.push(b.max_y);
        self.max_z.push(b.max_z);
    }

    fn remove(&mut self, index: usize) {
        self.min_x.remove(index);
        self.min_y.remove(index);
        self.min_z.remove(index);
        self.max_x.remove(index);
        self.max_y.remove(index);
        self.max_z.remove(index);
    }

    fn set(&mut self, index: usize, b: Aabb) {
        self.min_x[index] = b.min_x;
        self.min_y[index] = b.min_y;
        self.min_z[index] = b.min_z;
        self.max_x[index] = b.max_x;
        self.max_y[index] = b.max_y;
        self.max_z[index] = b.max_z;
    }

    /// Whether box `i` intersects `q` ([`Aabb::intersects`]).
    #[inline]
    fn intersects(&self, i: usize, q: &Aabb) -> bool {
        (self.min_x[i] < q.max_x)
            & (self.max_x[i] > q.min_x)
            & (self.min_y[i] < q.max_y)
            & (self.max_y[i] > q.min_y)
            & (self.min_z[i] < q.max_z)
            & (self.max_z[i] > q.min_z)
    }

    /// Calls `f` with the index of every box that intersects `q` ([`Aabb::intersects`]), in
    /// ascending order.
    #[inline]
    fn for_each_intersecting(&self, q: &Aabb, mut f: impl FnMut(usize)) {
        const LANES: usize = 8;
        let n = self.min_x.len();
        let lanes = |v: &[f64], i: usize| -> [f64; LANES] {
            v[i..i + LANES].try_into().unwrap_or([f64::NAN; LANES])
        };
        let mut i = 0;
        while i + LANES <= n {
            let (ax, ay, az) = (
                lanes(&self.min_x, i),
                lanes(&self.min_y, i),
                lanes(&self.min_z, i),
            );
            let (bx, by, bz) = (
                lanes(&self.max_x, i),
                lanes(&self.max_y, i),
                lanes(&self.max_z, i),
            );
            let mut mask = 0u32;
            for j in 0..LANES {
                let h = (ax[j] < q.max_x)
                    & (bx[j] > q.min_x)
                    & (ay[j] < q.max_y)
                    & (by[j] > q.min_y)
                    & (az[j] < q.max_z)
                    & (bz[j] > q.min_z);
                mask |= u32::from(h) << j;
            }
            while mask != 0 {
                f(i + mask.trailing_zeros() as usize);
                mask &= mask - 1;
            }
            i += LANES;
        }
        for i in i..n {
            if self.intersects(i, q) {
                f(i);
            }
        }
    }
}

impl Section {
    fn list(&self, subset: Subset) -> &[EntityId] {
        match subset {
            Subset::All => &self.all,
            Subset::Players => &self.players,
            Subset::Others => &self.others,
        }
    }
}

/// A set of entity types: the classes a typed query (`getEntitiesOfClass`) looks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindSet {
    bits: [u64; 4],
    /// A type past the bits is in the set (only for the types of a section).
    high: bool,
}

impl KindSet {
    /// The set of `kinds`. Types past the bits are never skipped (see [`Self::contains`]).
    pub const fn of(kinds: &[EntityTypeId]) -> KindSet {
        let mut bits = [0u64; 4];
        let mut i = 0;
        while i < kinds.len() {
            let k = kinds[i].0 as usize;
            if k < 256 {
                bits[k >> 6] |= 1 << (k & 63);
            }
            i += 1;
        }
        KindSet { bits, high: false }
    }

    fn insert(&mut self, kind: EntityTypeId) {
        let k = kind.0 as usize;
        if k < 256 {
            self.bits[k >> 6] |= 1 << (k & 63);
        } else {
            self.high = true;
        }
    }

    /// Whether a section with the types `present` may hold an entity of this set.
    #[inline]
    fn meets(&self, present: &KindSet) -> bool {
        present.high
            || (self.bits[0] & present.bits[0])
                | (self.bits[1] & present.bits[1])
                | (self.bits[2] & present.bits[2])
                | (self.bits[3] & present.bits[3])
                != 0
    }

    /// Whether `kind` is in the set (always for a type past the bits).
    #[inline]
    pub const fn contains(&self, kind: EntityTypeId) -> bool {
        let k = kind.0 as usize;
        k >= 256 || self.bits[k >> 6] & (1 << (k & 63)) != 0
    }
}

/// Entity ids per section (`EntitySectionStorage`). Empty sections are dropped.
#[derive(Debug, Default)]
pub struct SectionIndex {
    sections: FxHashMap<i64, Section>,
    /// The Y of the non-empty sections of each chunk, ascending.
    columns: FxHashMap<(i32, i32), Vec<i32>>,
    /// The players by region of [`REGION`] sections on X and Z, in no order: who is near a
    /// position without walking the sections around it.
    player_regions: FxHashMap<(i32, i32), Vec<EntityId>>,
}

/// Sections per player region on X and Z, as a shift (8 sections, 128 blocks).
const REGION: i32 = 3;

impl SectionIndex {
    /// Gives back the room of emptied sections and regions.
    pub fn shrink_to_fit(&mut self) {
        self.sections.shrink_to_fit();
        self.columns.shrink_to_fit();
        self.player_regions.shrink_to_fit();
    }

    pub(super) fn insert(
        &mut self,
        section: SectionPos,
        class: EntityClass,
        kind: EntityTypeId,
        id: EntityId,
        aabb: Aabb,
    ) {
        let entry = self.sections.entry(section.as_long()).or_insert_with(|| {
            let ys = self.columns.entry((section.x, section.z)).or_default();
            if let Err(index) = ys.binary_search(&section.y) {
                ys.insert(index, section.y);
            }
            Section::default()
        });
        entry.all.push(id);
        entry.boxes.push(aabb);
        entry.kinds.push(kind);
        entry.present.insert(kind);
        match class {
            EntityClass::Player => {
                entry.players.push(id);
                self.player_regions
                    .entry((section.x >> REGION, section.z >> REGION))
                    .or_default()
                    .push(id);
            }
            EntityClass::Other => entry.others.push(id),
        }
    }

    pub(super) fn remove(&mut self, section: SectionPos, class: EntityClass, id: EntityId) {
        let key = section.as_long();
        let Some(entry) = self.sections.get_mut(&key) else {
            return;
        };
        if let Some(index) = entry.all.iter().position(|&e| e == id) {
            entry.all.remove(index);
            entry.boxes.remove(index);
            entry.kinds.remove(index);
            entry.present = KindSet::default();
            for &kind in &entry.kinds {
                entry.present.insert(kind);
            }
        }
        match class {
            EntityClass::Player => {
                remove_ordered(&mut entry.players, id);
                let region = (section.x >> REGION, section.z >> REGION);
                if let Some(list) = self.player_regions.get_mut(&region) {
                    if let Some(index) = list.iter().position(|&e| e == id) {
                        list.swap_remove(index);
                    }
                    if list.is_empty() {
                        self.player_regions.remove(&region);
                    }
                }
            }
            EntityClass::Other => remove_ordered(&mut entry.others, id),
        }
        if entry.all.is_empty() {
            self.sections.remove(&key);
            let column = (section.x, section.z);
            if let Some(ys) = self.columns.get_mut(&column) {
                if let Ok(index) = ys.binary_search(&section.y) {
                    ys.remove(index);
                }
                if ys.is_empty() {
                    self.columns.remove(&column);
                }
            }
        }
    }

    /// Records the new bounding box of `id` in `section`.
    #[inline]
    pub(super) fn set_box(&mut self, section: SectionPos, id: EntityId, aabb: Aabb) {
        if let Some(entry) = self.sections.get_mut(&section.as_long())
            && let Some(index) = entry.all.iter().position(|&e| e == id)
        {
            entry.boxes.set(index, aabb);
        }
    }

    /// The chunks with entities.
    pub fn chunks(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.columns.keys().copied()
    }

    /// The entities of a chunk, section by section from the bottom
    /// (`getExistingSectionsInChunk`), each in section order.
    pub fn in_chunk(&self, x: i32, z: i32) -> impl Iterator<Item = EntityId> + '_ {
        self.columns
            .get(&(x, z))
            .into_iter()
            .flatten()
            .flat_map(move |&y| self.in_section(SectionPos::new(x, y, z), false).iter())
            .copied()
    }

    /// The entities (or only the players) of one section.
    pub fn in_section(&self, section: SectionPos, players_only: bool) -> &[EntityId] {
        self.sections.get(&section.as_long()).map_or(&[], |s| {
            s.list(if players_only {
                Subset::Players
            } else {
                Subset::All
            })
        })
    }

    /// Number of non-empty sections.
    pub fn len(&self) -> usize {
        self.sections.len()
    }

    /// Whether no section holds entities.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Calls `f` for the entities of all sections near `aabb` in Vanilla's order
    /// (`forEachAccessibleNonEmptySection`: X ascending, then the packed section key, which
    /// orders Z and Y as unsigned fields). The caller filters by bounding box.
    pub(super) fn for_each_near(&self, aabb: &Aabb, subset: Subset, mut f: impl FnMut(EntityId)) {
        self.for_each_section_near(aabb, |section| {
            section.list(subset).iter().copied().for_each(&mut f);
        });
    }

    /// Calls `f` for every entity whose bounding box intersects `aabb`, in the order of
    /// [`Self::for_each_near`].
    pub(super) fn for_each_intersecting(&self, aabb: &Aabb, mut f: impl FnMut(EntityId)) {
        self.for_each_section_near(aabb, |section| {
            section
                .boxes
                .for_each_intersecting(aabb, |i| f(section.all[i]));
        });
    }

    /// Like [`Self::for_each_intersecting`] for the entities of the types in `kinds`.
    pub(super) fn for_each_intersecting_of(
        &self,
        aabb: &Aabb,
        kinds: &KindSet,
        mut f: impl FnMut(EntityId),
    ) {
        self.for_each_section_near(aabb, |section| {
            if !kinds.meets(&section.present) {
                return;
            }
            // The types first: they are one small column, and most entities of a crowded
            // section are of other types.
            for (i, &kind) in section.kinds.iter().enumerate() {
                if kinds.contains(kind) && section.boxes.intersects(i, aabb) {
                    f(section.all[i]);
                }
            }
        });
    }

    /// The non-empty sections near `aabb` in the order of [`Self::for_each_near`].
    fn for_each_section_near(&self, aabb: &Aabb, mut f: impl FnMut(&Section)) {
        let min_x = floor(aabb.min_x - 2.0) >> 4;
        let min_y = floor(aabb.min_y - 4.0) >> 4;
        let min_z = floor(aabb.min_z - 2.0) >> 4;
        let max_x = floor(aabb.max_x + 2.0) >> 4;
        let max_y = floor(aabb.max_y) >> 4;
        let max_z = floor(aabb.max_z + 2.0) >> 4;
        let span =
            |min: i32, max: i32| u64::try_from(i64::from(max) - i64::from(min) + 1).unwrap_or(0);
        let volume = span(min_x, max_x)
            .saturating_mul(span(min_y, max_y))
            .saturating_mul(span(min_z, max_z));
        let in_range = |s: SectionPos| {
            (min_x..=max_x).contains(&s.x)
                && (min_y..=max_y).contains(&s.y)
                && (min_z..=max_z).contains(&s.z)
        };
        if volume <= self.sections.len() as u64 {
            // The sections in key order without collecting them: a field of the key orders
            // as unsigned, so 0 and up come before the negatives.
            let unsigned = |min: i32, max: i32| (min.max(0)..=max).chain(min..=max.min(-1));
            for x in min_x..=max_x {
                for z in unsigned(min_z, max_z) {
                    for y in unsigned(min_y, max_y) {
                        let key = SectionPos::new(x, y, z).as_long();
                        if let Some(section) = self.sections.get(&key) {
                            f(section);
                        }
                    }
                }
            }
            return;
        }
        let mut keys: Vec<i64> = self
            .sections
            .keys()
            .copied()
            .filter(|&key| in_range(SectionPos::from_long(key)))
            .collect();
        // X sits in the top bits with its sign, so signed order is X ascending; within one X
        // the remaining bits compare like Vanilla's sorted long set.
        keys.sort_unstable();
        for key in keys {
            if let Some(section) = self.sections.get(&key) {
                f(section);
            }
        }
    }

    /// Calls `f` for every player whose position is within `range` blocks of (`x`, `z`) on
    /// both axes, and for some farther ones, in no particular order, until `f` returns `true`.
    pub(super) fn players_around(
        &self,
        x: f64,
        z: f64,
        range: f64,
        mut f: impl FnMut(EntityId) -> bool,
    ) {
        let region = |v: f64| floor(v) >> (4 + REGION);
        let (min_x, max_x) = (region(x - range), region(x + range));
        let (min_z, max_z) = (region(z - range), region(z + range));
        let span = |min: i32, max: i32| u64::try_from(i64::from(max) - i64::from(min) + 1);
        let area = span(min_x, max_x)
            .unwrap_or(u64::MAX)
            .saturating_mul(span(min_z, max_z).unwrap_or(u64::MAX));
        if area > self.player_regions.len() as u64 {
            for (&(rx, rz), list) in &self.player_regions {
                if (min_x..=max_x).contains(&rx)
                    && (min_z..=max_z).contains(&rz)
                    && list.iter().any(|&id| f(id))
                {
                    return;
                }
            }
            return;
        }
        for rx in min_x..=max_x {
            for rz in min_z..=max_z {
                if let Some(list) = self.player_regions.get(&(rx, rz))
                    && list.iter().any(|&id| f(id))
                {
                    return;
                }
            }
        }
    }
}

fn remove_ordered(list: &mut Vec<EntityId>, id: EntityId) {
    if let Some(index) = list.iter().position(|&e| e == id) {
        list.remove(index);
    }
}

#[cfg(test)]
mod tests {
    use vanillite_base::SlotMap;

    use super::*;

    /// A spread of sections on both sides of zero on every axis, one entity each.
    fn index(count: i32, class: EntityClass) -> (SectionIndex, Vec<(SectionPos, EntityId)>) {
        let mut ids: SlotMap<EntityId, ()> = SlotMap::new();
        let mut index = SectionIndex::default();
        let mut placed = Vec::new();
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = |span: i32| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            i32::try_from(seed % u64::try_from(span).unwrap()).unwrap() - span / 2
        };
        for _ in 0..count {
            let section = SectionPos::new(next(12), next(8), next(12));
            let id = ids.insert(());
            let low = (
                f64::from(section.x * 16),
                f64::from(section.y * 16),
                f64::from(section.z * 16),
            );
            let aabb = Aabb::new(low.0, low.1, low.2, low.0 + 1.0, low.1 + 1.0, low.2 + 1.0);
            index.insert(section, class, EntityTypeId(0), id, aabb);
            placed.push((section, id));
        }
        (index, placed)
    }

    /// The sections `for_each_near` covers for a box, as it computes them.
    fn covered(aabb: &Aabb, s: SectionPos) -> bool {
        (floor(aabb.min_x - 2.0) >> 4..=floor(aabb.max_x + 2.0) >> 4).contains(&s.x)
            && (floor(aabb.min_y - 4.0) >> 4..=floor(aabb.max_y) >> 4).contains(&s.y)
            && (floor(aabb.min_z - 2.0) >> 4..=floor(aabb.max_z + 2.0) >> 4).contains(&s.z)
    }

    /// Vanilla walks the sorted section keys; the walk without a sort visits the same order
    /// on both sides of zero, and so does the search over all sections for large boxes.
    #[test]
    fn sections_are_visited_in_key_order() {
        for (count, half) in [(400, 20.0), (400, 70.0), (12, 90.0), (3, 500.0)] {
            let (index, placed) = index(count, EntityClass::Other);
            let aabb = Aabb::new(-half, -half, -half, half, half, half);
            let mut expected: Vec<(i64, usize)> = placed
                .iter()
                .enumerate()
                .filter(|(_, (s, _))| covered(&aabb, *s))
                .map(|(i, (s, _))| (s.as_long(), i))
                .collect();
            // Entities of one section keep their insertion order.
            expected.sort();
            let expected: Vec<EntityId> = expected.iter().map(|&(_, i)| placed[i].1).collect();
            let mut visited = Vec::new();
            index.for_each_near(&aabb, Subset::All, |id| visited.push(id));
            assert!(!expected.is_empty(), "{count} sections, box {half}");
            assert_eq!(visited, expected, "{count} sections, box {half}");
        }
    }

    /// Every player within the range on both axes is found, by the walk over the regions and
    /// by the search over all of them.
    #[test]
    fn players_around_finds_every_player_in_range() {
        for (count, range) in [(300, 40.0), (300, 128.0), (4, 128.0), (4, 3000.0)] {
            let (index, placed) = index(count, EntityClass::Player);
            for (x, z) in [
                (0.0, 0.0),
                (-37.5, 81.25),
                (95.0, -96.0),
                (-127.999, -128.0),
            ] {
                let mut found = Vec::new();
                index.players_around(x, z, range, |id| {
                    found.push(id);
                    false
                });
                for (section, id) in &placed {
                    // The player can be anywhere in its section.
                    let near = |v: f64, s: i32| {
                        let lo = f64::from(s * 16);
                        v + range >= lo && v - range < lo + 16.0
                    };
                    if near(x, section.x) && near(z, section.z) {
                        assert!(found.contains(id), "{section:?} from {x}, {z} in {range}");
                    }
                }
                let mut unique = found.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(unique.len(), found.len(), "a player was visited twice");
            }
        }
    }

    #[test]
    fn players_around_stops_when_asked() {
        let (index, _) = index(300, EntityClass::Player);
        let mut calls = 0;
        index.players_around(0.0, 0.0, 128.0, |_| {
            calls += 1;
            true
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn removed_players_leave_their_region() {
        let (mut index, placed) = index(50, EntityClass::Player);
        for (section, id) in &placed {
            index.remove(*section, EntityClass::Player, *id);
        }
        assert!(index.player_regions.is_empty());
        assert!(index.is_empty());
    }
}
