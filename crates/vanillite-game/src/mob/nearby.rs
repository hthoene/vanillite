//! The players near each chunk: an index from every chunk within a radius of a player's chunk
//! to those players, updated when a player changes chunk, so a system asks only the players
//! around a chunk instead of all of them. Natural spawning uses [`RADIUS`]
//! (`DistanceManager.hasPlayersNearby`), the activation range its own radius
//! ([`super::activation`]).

use vanillite_base::hash::FxHashMap;
use vanillite_base::{BlockPos, ChunkPos, Vec3};
use vanillite_world::entity::EntityId;

/// Chunk radius of the natural spawn chunk counter (`NaturalSpawnChunkCounter`).
pub const RADIUS: i32 = 8;

/// Squared distance from a chunk's centre within which a player allows spawning there
/// (`playerIsCloseEnoughForSpawning`); every such player is in the chunk's entry.
pub const SPAWN_DISTANCE_SQR: f64 = 16384.0;

#[derive(Debug)]
struct Player {
    id: EntityId,
    pos: Vec3,
    chunk: ChunkPos,
    stamp: u32,
    live: bool,
}

/// The players of one world (or one region) by chunk. Players are known by a slot that stays
/// theirs while they are in the index.
#[derive(Debug)]
pub struct NearbyPlayers {
    /// Chunk radius of the square around a player's chunk.
    radius: i32,
    /// Whether [`Self::nearest_distance_sqr`] gets a grid for crowds.
    crowd_grid: bool,
    slots: FxHashMap<EntityId, u32>,
    players: Vec<Player>,
    free: Vec<u32>,
    chunks: FxHashMap<ChunkPos, Vec<u32>>,
    /// The players by the chunk they are in, for crowds.
    grid: Grid,
    stamp: u32,
}

fn within(a: ChunkPos, b: ChunkPos, radius: i32) -> bool {
    (a.x - b.x).abs() <= radius && (a.z - b.z).abs() <= radius
}

impl Default for NearbyPlayers {
    /// The index of natural spawning: [`RADIUS`], with the crowd grid.
    fn default() -> Self {
        NearbyPlayers::new(RADIUS, true)
    }
}

/// `ChunkMap.euclideanDistanceSquared`: from the chunk's centre to a position.
pub fn chunk_center_distance_sqr(chunk: ChunkPos, pos: Vec3) -> f64 {
    let x = f64::from(chunk.x) * 16.0 + 8.0;
    let z = f64::from(chunk.z) * 16.0 + 8.0;
    let dx = x - pos.x;
    let dz = z - pos.z;
    dx * dx + dz * dz
}

impl NearbyPlayers {
    /// An empty index of the chunks within `radius` chunks of a player's chunk; `crowd_grid`
    /// prepares [`Self::nearest_distance_sqr`] for crowds of players.
    pub fn new(radius: i32, crowd_grid: bool) -> NearbyPlayers {
        NearbyPlayers {
            radius: radius.max(0),
            crowd_grid,
            slots: FxHashMap::default(),
            players: Vec::new(),
            free: Vec::new(),
            chunks: FxHashMap::default(),
            grid: Grid::default(),
            stamp: 0,
        }
    }

    /// The chunk radius.
    pub fn radius(&self) -> i32 {
        self.radius
    }

    /// Changes the chunk radius; the index is empty until the next [`Self::update`].
    pub fn set_radius(&mut self, radius: i32) {
        if radius.max(0) != self.radius {
            *self = NearbyPlayers::new(radius, self.crowd_grid);
        }
    }

    /// Whether a player is in the index.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Takes the tick's players (unique ids) and their positions: players that changed
    /// chunk move in the index, new ones are added and missing ones removed.
    pub fn update(&mut self, players: &[(EntityId, Vec3)]) {
        self.stamp = self.stamp.wrapping_add(1);
        let stamp = self.stamp;
        for &(id, pos) in players {
            let chunk = BlockPos::containing(pos.x, pos.y, pos.z).chunk();
            if let Some(&slot) = self.slots.get(&id) {
                let p = &mut self.players[slot as usize];
                let old = p.chunk;
                p.pos = pos;
                p.chunk = chunk;
                p.stamp = stamp;
                if old != chunk {
                    self.move_square(slot, Some(old), Some(chunk));
                }
                continue;
            }
            let player = Player {
                id,
                pos,
                chunk,
                stamp,
                live: true,
            };
            let slot = if let Some(slot) = self.free.pop() {
                self.players[slot as usize] = player;
                slot
            } else {
                self.players.push(player);
                u32::try_from(self.players.len() - 1).unwrap_or(u32::MAX)
            };
            self.slots.insert(id, slot);
            self.move_square(slot, None, Some(chunk));
        }
        if self.slots.len() > players.len() {
            for slot in 0..self.players.len() {
                let p = &mut self.players[slot];
                if !p.live || p.stamp == stamp {
                    continue;
                }
                p.live = false;
                let (id, chunk) = (p.id, p.chunk);
                let slot = u32::try_from(slot).unwrap_or(u32::MAX);
                self.slots.remove(&id);
                self.free.push(slot);
                self.move_square(slot, Some(chunk), None);
            }
        }
        if self.crowd_grid {
            self.grid.build(&self.players);
        }
    }

    /// Moves a slot from the chunks around `old` to the chunks around `new`.
    fn move_square(&mut self, slot: u32, old: Option<ChunkPos>, new: Option<ChunkPos>) {
        let r = self.radius;
        if let Some(o) = old {
            for x in o.x - r..=o.x + r {
                for z in o.z - r..=o.z + r {
                    let key = ChunkPos::new(x, z);
                    if new.is_some_and(|n| within(key, n, r)) {
                        continue;
                    }
                    if let Some(list) = self.chunks.get_mut(&key) {
                        if let Some(i) = list.iter().position(|&s| s == slot) {
                            list.swap_remove(i);
                        }
                        if list.is_empty() {
                            self.chunks.remove(&key);
                        }
                    }
                }
            }
        }
        if let Some(n) = new {
            for x in n.x - r..=n.x + r {
                for z in n.z - r..=n.z + r {
                    let key = ChunkPos::new(x, z);
                    if old.is_some_and(|o| within(key, o, r)) {
                        continue;
                    }
                    self.chunks.entry(key).or_default().push(slot);
                }
            }
        }
    }

    /// The number of chunks within [`RADIUS`] of a player's chunk
    /// (`getNaturalSpawnChunkCount`).
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// The chunks within [`RADIUS`] of a player's chunk, in no particular order.
    pub fn chunks(&self) -> impl Iterator<Item = ChunkPos> + '_ {
        self.chunks.keys().copied()
    }

    /// The chunks of [`Self::chunks`] with a player close enough for spawning
    /// ([`Self::any_close_for_spawning`]).
    pub fn chunks_close_for_spawning(&self) -> impl Iterator<Item = ChunkPos> + '_ {
        self.chunks.iter().filter_map(|(&chunk, slots)| {
            slots
                .iter()
                .any(|&s| {
                    chunk_center_distance_sqr(chunk, self.players[s as usize].pos)
                        < SPAWN_DISTANCE_SQR
                })
                .then_some(chunk)
        })
    }

    /// One more than the highest slot in use: the length of per-slot arrays.
    pub fn slot_count(&self) -> usize {
        self.players.len()
    }

    /// The players in the index: slot, position and chunk.
    pub fn players(&self) -> impl Iterator<Item = (u32, Vec3, ChunkPos)> + '_ {
        self.players
            .iter()
            .zip(0u32..)
            .filter(|(p, _)| p.live)
            .map(|(p, slot)| (slot, p.pos, p.chunk))
    }

    /// Whether the player in `slot` is close enough to `chunk` for spawning.
    pub fn is_close_for_spawning(&self, slot: u32, chunk: ChunkPos) -> bool {
        chunk_center_distance_sqr(chunk, self.players[slot as usize].pos) < SPAWN_DISTANCE_SQR
    }

    /// The slots of the players whose chunk is within [`RADIUS`] of `chunk`, in no order.
    pub fn around(&self, chunk: ChunkPos) -> &[u32] {
        self.chunks.get(&chunk).map_or(&[], Vec::as_slice)
    }

    /// Whether a player stands at most `range` blocks from `pos` on both the X and the Z axis
    /// (any height). `range` must not reach past the radius: at most 16 × radius blocks.
    pub fn any_within_xz(&self, pos: Vec3, range: f64) -> bool {
        let chunk = BlockPos::containing(pos.x, pos.y, pos.z).chunk();
        self.around(chunk).iter().any(|&s| {
            let p = self.players[s as usize].pos;
            (p.x - pos.x).abs() <= range && (p.z - pos.z).abs() <= range
        })
    }

    /// Whether a player is close enough to `chunk` for spawning
    /// (`anyPlayerCloseEnoughForSpawning`).
    pub fn any_close_for_spawning(&self, chunk: ChunkPos) -> bool {
        self.around(chunk).iter().any(|&s| {
            chunk_center_distance_sqr(chunk, self.players[s as usize].pos) < SPAWN_DISTANCE_SQR
        })
    }

    /// Appends the slots of the players close enough to `chunk` for spawning
    /// (`getPlayersCloseForSpawning`), in no order.
    pub fn close_for_spawning(&self, chunk: ChunkPos, out: &mut Vec<u32>) {
        out.extend(self.around(chunk).iter().copied().filter(|&s| {
            chunk_center_distance_sqr(chunk, self.players[s as usize].pos) < SPAWN_DISTANCE_SQR
        }));
    }

    /// The squared distance from `at` to the nearest player as natural spawning needs it: a
    /// value that compares with `near`, 64² and 128² like the exact one (`near` is at most
    /// 64²); infinity when no player is within [`RADIUS`] chunks and 128 blocks.
    pub fn nearest_distance_sqr(&self, at: Vec3, near: f64) -> f64 {
        let chunk = BlockPos::containing(at.x, at.y, at.z).chunk();
        self.nearest_distance_sqr_around(at, near, self.around(chunk))
    }

    /// [`Self::nearest_distance_sqr`] with the players around the chunk of `at` already
    /// looked up ([`Self::around`]).
    pub fn nearest_distance_sqr_around(&self, at: Vec3, near: f64, around: &[u32]) -> f64 {
        if around.len() > CROWD && self.grid.is_built() {
            // A crowd: the first player within each limit, from the players by chunk.
            for limit in [near, 4096.0, SPAWN_DISTANCE_SQR] {
                if let Some(d) = self.grid.within(&self.players, at, limit) {
                    return d;
                }
            }
            return f64::INFINITY;
        }
        let mut best = f64::INFINITY;
        for &s in around {
            let d = self.players[s as usize].pos.distance_to_sqr(at);
            if d <= near {
                return d;
            }
            best = best.min(d);
        }
        best
    }
}

/// Players around a chunk above which [`NearbyPlayers::nearest_distance_sqr`] searches by
/// chunk instead of trying all of them.
const CROWD: usize = 64;

/// Largest area in chunks of the players' bounding box that gets a [`Grid`].
const GRID_AREA: i64 = 1 << 16;

/// The players of a tick by chunk, over the bounding box of their chunks (empty when the box
/// is larger than [`GRID_AREA`]).
#[derive(Debug, Default)]
struct Grid {
    min: ChunkPos,
    width: i32,
    height: i32,
    /// Start of each cell's slots in `slots` (one more entry than cells).
    starts: Vec<u32>,
    slots: Vec<u32>,
    /// Lowest and highest player y of each cell.
    heights: Vec<(f64, f64)>,
}

impl Grid {
    fn is_built(&self) -> bool {
        !self.starts.is_empty()
    }

    fn build(&mut self, players: &[Player]) {
        self.starts.clear();
        self.slots.clear();
        let live = || players.iter().filter(|p| p.live);
        if live().count() <= CROWD {
            return;
        }
        let (mut min_x, mut min_z, mut max_x, mut max_z) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for p in live() {
            min_x = min_x.min(p.chunk.x);
            min_z = min_z.min(p.chunk.z);
            max_x = max_x.max(p.chunk.x);
            max_z = max_z.max(p.chunk.z);
        }
        let width = i64::from(max_x) - i64::from(min_x) + 1;
        let height = i64::from(max_z) - i64::from(min_z) + 1;
        if width * height > GRID_AREA {
            return;
        }
        self.min = ChunkPos::new(min_x, min_z);
        self.width = i32::try_from(width).unwrap_or(0);
        self.height = i32::try_from(height).unwrap_or(0);
        let cells = usize::try_from(width * height).unwrap_or(0);
        self.starts.resize(cells + 1, 0);
        self.heights.clear();
        self.heights
            .resize(cells, (f64::INFINITY, f64::NEG_INFINITY));
        for p in live() {
            let cell = self.cell(p.chunk);
            self.starts[cell + 1] += 1;
            let (low, high) = &mut self.heights[cell];
            *low = low.min(p.pos.y);
            *high = high.max(p.pos.y);
        }
        for i in 1..=cells {
            self.starts[i] += self.starts[i - 1];
        }
        self.slots.resize(self.starts[cells] as usize, 0);
        let mut next = self.starts.clone();
        for (p, slot) in players.iter().zip(0u32..) {
            if p.live {
                let cell = self.cell(p.chunk);
                self.slots[next[cell] as usize] = slot;
                next[cell] += 1;
            }
        }
    }

    /// The cell of a chunk inside the box.
    fn cell(&self, chunk: ChunkPos) -> usize {
        usize::try_from((chunk.z - self.min.z) * self.width + (chunk.x - self.min.x)).unwrap_or(0)
    }

    /// The squared distance of some player at most `limit` (squared) away from `at`.
    fn within(&self, players: &[Player], at: Vec3, limit: f64) -> Option<f64> {
        // Distance from `at` to a cell's range of player positions, as a bound per axis.
        let outside = |a: f64, lo: f64, hi: f64| {
            if a < lo {
                lo - a
            } else if a > hi {
                a - hi
            } else {
                0.0
            }
        };
        let gap = |a: f64, chunk: i32| {
            let lo = f64::from(chunk) * 16.0;
            outside(a, lo, lo + 16.0)
        };
        let reach = limit.sqrt();
        #[allow(clippy::cast_possible_truncation)]
        let span = |a: f64, min: i32, len: i32| {
            let lo = (((a - reach) / 16.0).floor() as i32).max(min);
            let hi = (((a + reach) / 16.0).floor() as i32).min(min + len - 1);
            lo..=hi
        };
        for z in span(at.z, self.min.z, self.height) {
            let gz = gap(at.z, z);
            for x in span(at.x, self.min.x, self.width) {
                let gx = gap(at.x, x);
                let cell = self.cell(ChunkPos::new(x, z));
                let (low, high) = self.heights[cell];
                let gy = outside(at.y, low, high);
                if gx * gx + gy * gy + gz * gz > limit {
                    continue;
                }
                let range = self.starts[cell] as usize..self.starts[cell + 1] as usize;
                for &s in &self.slots[range] {
                    let d = players[s as usize].pos.distance_to_sqr(at);
                    if d <= limit {
                        return Some(d);
                    }
                }
            }
        }
        None
    }
}

/// The players close enough to each chunk for spawning during one tick, computed once per
/// chunk (`LocalMobCapCalculator.playersNearChunk`).
#[derive(Debug, Default)]
pub struct CloseForSpawning {
    ranges: FxHashMap<ChunkPos, (u32, u32)>,
    slots: Vec<u32>,
}

impl CloseForSpawning {
    /// Forgets the last tick's lists.
    pub fn clear(&mut self) {
        self.ranges.clear();
        self.slots.clear();
    }

    /// The slots of the players close enough to `chunk` for spawning.
    pub fn get(&mut self, nearby: &NearbyPlayers, chunk: ChunkPos) -> &[u32] {
        let (start, end) = *self.ranges.entry(chunk).or_insert_with(|| {
            let start = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
            nearby.close_for_spawning(chunk, &mut self.slots);
            (start, u32::try_from(self.slots.len()).unwrap_or(u32::MAX))
        });
        &self.slots[start as usize..end as usize]
    }
}
