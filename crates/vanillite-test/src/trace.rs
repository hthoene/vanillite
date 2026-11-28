//! The trace format: what a scenario looked like after every tick.
//!
//! Written by the Java recorder for Vanilla and by [`Trace::to_json`] for blessed goldens; one
//! JSON line per frame so diffs stay readable:
//!
//! ```text
//! {"format":1,
//! "region":[[x0,y0,z0],[x1,y1,z1]],
//! "palette":["minecraft:stone", "minecraft:oak_log[axis=x]", ...],
//! "frames":[
//! {"tick":0,"blocks":[count,palette,...],"block_ticks":[...],"fluid_ticks":[...],"entities":[...]},
//! {"tick":1,"changes":[[x,y,z,palette],...],"block_ticks":[...],"fluid_ticks":[...],"entities":[...]}]}
//! ```
//!
//! - Frame 0 holds every block of the region as run-length pairs in y, z, x order (x fastest);
//!   later frames hold the blocks that changed.
//! - Block states are `name[key=value,...]` with properties sorted by name.
//! - `level_rng` (scenarios with entities): the state of the level random after the tick.
//! - Ticks: `[x, y, z, block or fluid, delay, priority]` in Vanilla's drain order; the delay is
//!   relative to the game time of the frame.
//! - Entities: `[type, x, y, z, dx, dy, dz]` or `[type, x, y, z, dx, dy, dz, {detail}]`,
//!   doubles as IEEE bit patterns (signed 64-bit), so the comparison is exact. Their order is
//!   not significant. The detail object (living entities and items):
//!   - `name`: the scenario name of the entity;
//!   - `item`, `count`: the stack of an item entity;
//!   - `rot`: yaw, pitch, head yaw, body yaw; `hp`: health (floats as signed 32-bit bit
//!     patterns); `death`: the death animation tick once dying;
//!   - mobs: `rng` (the state of the entity's random: the first tick it differs is the first
//!     diverging random call), `target` (name or type of the attack target), `goals` and `targets` (the running
//!     goals of the goal and target selector by Vanilla class name, sorted), `path` (the
//!     navigation path while not done: `to` target block, `next` node index, `reach`, `nodes`
//!     as `[x, y, z, path type]`).
//! - View (scenarios with `"client_view": true`): `"view":["<player> <event>", ...]`, what
//!   the players' clients got during the tick (see [`crate::client_view`]). The recorder
//!   writes the raw packets instead (`"packets":{"<player>":["<id + body hex>", ...]}`);
//!   parsing normalizes them.
//! - Block entities (scenarios with `"block_entities": true`, #244): `"block_entities":
//!   ["x,y,z <nbt>", ...]`, sorted, the block entities of the region with the canonical text of
//!   their full saved data ([`canonical_nbt`]).
//! - Light (scenarios with `"light": true`): `"light":[count,level,...]` in every frame, run-length
//!   pairs over the region in y, z, x order; a level is `sky << 4 | block` as a client sees it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Map, Value};
use vanillite_base::BlockPos;

/// Format version the recorder writes.
pub const FORMAT: u64 = 1;

/// A pending block or fluid tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickObs {
    pub pos: BlockPos,
    /// Block or fluid name.
    pub kind: String,
    /// Trigger tick minus the current game time.
    pub delay: i64,
    /// `TickPriority` value (-3 extremely high … 3 extremely low).
    pub priority: i32,
}

/// An entity in the region.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityObs {
    /// Entity type name.
    pub kind: String,
    pub pos: [f64; 3],
    pub motion: [f64; 3],
    /// The detail object of living entities and items (see the module documentation).
    pub detail: Option<Map<String, Value>>,
}

impl EntityObs {
    /// Exact sort and comparison key.
    fn key(&self) -> (&str, [u64; 6], String) {
        let p = self.pos.map(f64::to_bits);
        let m = self.motion.map(f64::to_bits);
        let detail = self
            .detail
            .as_ref()
            .map(|d| canonical(&Value::Object(d.clone())))
            .unwrap_or_default();
        (&self.kind, [p[0], p[1], p[2], m[0], m[1], m[2]], detail)
    }

    /// The scenario name of the entity, if it has one.
    pub fn name(&self) -> Option<&str> {
        self.detail.as_ref()?.get("name")?.as_str()
    }

    /// A float of the detail written as bit pattern (`hp`) or an element of an array of them
    /// (`rot`).
    pub fn float_bits(v: &Value) -> Option<f32> {
        v.as_i64()
            .and_then(|b| i32::try_from(b).ok())
            .map(|b| f32::from_bits(b as u32))
    }

    /// The differences of `actual` from this (expected) observation of the same entity.
    fn differences(&self, actual: &Self) -> Vec<String> {
        let mut out = Vec::new();
        for (what, e, a) in [
            ("pos", self.pos, actual.pos),
            ("motion", self.motion, actual.motion),
        ] {
            if e.map(f64::to_bits) != a.map(f64::to_bits) {
                out.push(format!("{what} {a:?}, expected {e:?}"));
            }
        }
        let empty = Map::new();
        let (ed, ad) = (
            self.detail.as_ref().unwrap_or(&empty),
            actual.detail.as_ref().unwrap_or(&empty),
        );
        let mut keys: Vec<&String> = ed.keys().chain(ad.keys()).collect();
        keys.sort();
        keys.dedup();
        for k in keys {
            let (e, a) = (ed.get(k), ad.get(k));
            if e == a {
                continue;
            }
            let show = |v: Option<&Value>| match (k.as_str(), v) {
                (_, None) => "none".to_owned(),
                ("hp", Some(v)) => format!("{:?}", Self::float_bits(v)),
                ("rot", Some(Value::Array(a))) => {
                    format!("{:?}", a.iter().map(Self::float_bits).collect::<Vec<_>>())
                }
                (_, Some(v)) => v.to_string(),
            };
            out.push(format!("{k} {}, expected {}", show(a), show(e)));
        }
        out
    }
}

/// The state of the region after one tick.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub tick: u32,
    /// Palette index per block of the region, y, z, x order; shared with the frame before when
    /// no block changed (long replays of large regions would not fit into memory otherwise).
    pub blocks: Arc<Vec<u32>>,
    pub block_ticks: Vec<TickObs>,
    pub fluid_ticks: Vec<TickObs>,
    pub entities: Vec<EntityObs>,
    /// `sky << 4 | block` per block of the region, y, z, x order; empty if not observed.
    pub light: Vec<u8>,
    /// State of the level random after the tick (scenarios with entities).
    pub level_rng: Option<i64>,
    /// What the players' clients got during the tick, `<player> <event>` sorted
    /// (`"client_view"` scenarios).
    pub view: Vec<String>,
    /// `x,y,z <nbt>` of the region's block entities, sorted (`"block_entities"` scenarios).
    pub block_entities: Vec<String>,
}

/// A recorded scenario run.
#[derive(Clone, Debug)]
pub struct Trace {
    pub min: BlockPos,
    pub max: BlockPos,
    pub palette: Vec<String>,
    index: HashMap<String, u32>,
    pub frames: Vec<Frame>,
    /// For blessed traces: SHA-1 of the scenario they were blessed for.
    pub blessed_for: Option<String>,
    /// The players' clients of recorded raw packets.
    clients: crate::client_view::ClientViews,
}

impl Trace {
    /// An empty trace of a region.
    pub fn new(min: BlockPos, max: BlockPos) -> Self {
        Self {
            min,
            max,
            palette: Vec::new(),
            index: HashMap::new(),
            frames: Vec::new(),
            blessed_for: None,
            clients: crate::client_view::ClientViews::with_attributes(true),
        }
    }

    /// Size of the region in x, y and z.
    pub fn size(&self) -> [usize; 3] {
        [
            (self.max.x - self.min.x + 1).max(0) as usize,
            (self.max.y - self.min.y + 1).max(0) as usize,
            (self.max.z - self.min.z + 1).max(0) as usize,
        ]
    }

    /// Position of the block with index `i` (y, z, x order).
    pub fn pos_of(&self, i: usize) -> BlockPos {
        let [sx, _, sz] = self.size();
        BlockPos::new(
            self.min.x + (i % sx) as i32,
            self.min.y + (i / (sx * sz)) as i32,
            self.min.z + (i / sx % sz) as i32,
        )
    }

    /// Palette index of a block state name, adding it if new.
    pub fn intern(&mut self, state: &str) -> u32 {
        if let Some(&i) = self.index.get(state) {
            return i;
        }
        let i = self.palette.len() as u32;
        self.palette.push(state.to_owned());
        self.index.insert(state.to_owned(), i);
        i
    }

    /// Parses a trace written by the recorder or by [`Trace::to_json`].
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if v.get("format").and_then(Value::as_u64) != Some(FORMAT) {
            return Err(format!("trace format is not {FORMAT}"));
        }
        let region = v.get("region").ok_or("region is missing")?;
        let mut trace = Self::new(pos(&region[0])?, pos(&region[1])?);
        trace.blessed_for = v
            .get("blessed_for")
            .and_then(Value::as_str)
            .map(str::to_owned);
        for s in v
            .get("palette")
            .and_then(Value::as_array)
            .ok_or("palette")?
        {
            trace.intern(s.as_str().ok_or("palette entries are strings")?);
        }
        let mut last: Arc<Vec<u32>> = Arc::default();
        for f in v.get("frames").and_then(Value::as_array).ok_or("frames")? {
            let frame = trace.frame_of(f, &last)?;
            last.clone_from(&frame.blocks);
            trace.frames.push(frame);
        }
        Ok(trace)
    }

    /// A frame of the trace from its JSON object; `last` holds the blocks of the frame before.
    fn frame_of(&mut self, f: &Value, last: &Arc<Vec<u32>>) -> Result<Frame, String> {
        let view = self.view_of(f)?;
        let trace = &*self;
        let volume: usize = trace.size().iter().product();
        let tick = f
            .get("tick")
            .and_then(Value::as_u64)
            .and_then(|t| u32::try_from(t).ok())
            .ok_or("frame tick")?;
        let blocks = if let Some(runs) = f.get("blocks").and_then(Value::as_array) {
            let mut blocks = Vec::with_capacity(volume);
            for pair in runs.chunks(2) {
                let count = pair[0].as_u64().ok_or("run count")? as usize;
                let p = pair
                    .get(1)
                    .and_then(Value::as_u64)
                    .ok_or("run palette index")? as u32;
                blocks.extend(std::iter::repeat_n(p, count));
            }
            Arc::new(blocks)
        } else {
            let changes = f
                .get("changes")
                .and_then(Value::as_array)
                .ok_or("changes")?;
            if changes.is_empty() {
                Arc::clone(last)
            } else {
                let mut blocks = Vec::clone(last);
                for c in changes {
                    let p = pos(c)?;
                    let i = trace.index_of(p).ok_or("change outside the region")?;
                    let state = c.get(3).and_then(Value::as_u64).ok_or("change state")?;
                    *blocks.get_mut(i).ok_or("changes before a full frame")? = state as u32;
                }
                Arc::new(blocks)
            }
        };
        if blocks.len() != volume || blocks.iter().any(|&b| b as usize >= trace.palette.len()) {
            return Err(format!("frame {tick}: blocks do not match the region"));
        }
        let mut light = Vec::new();
        if let Some(runs) = f.get("light").and_then(Value::as_array) {
            for pair in runs.chunks(2) {
                let count = pair[0].as_u64().ok_or("light run count")? as usize;
                let level = pair
                    .get(1)
                    .and_then(Value::as_u64)
                    .and_then(|l| u8::try_from(l).ok())
                    .ok_or("light run level")?;
                light.extend(std::iter::repeat_n(level, count));
            }
            if light.len() != volume {
                return Err(format!("frame {tick}: light does not match the region"));
            }
        }
        Ok(Frame {
            tick,
            blocks,
            block_ticks: ticks(f.get("block_ticks"))?,
            fluid_ticks: ticks(f.get("fluid_ticks"))?,
            entities: entities(f.get("entities"))?,
            light,
            level_rng: f.get("level_rng").and_then(Value::as_i64),
            view,
            block_entities: {
                let mut list = f
                    .get("block_entities")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|e| e.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                    })
                    .unwrap_or(Some(Vec::new()))
                    .ok_or("block entities are strings")?;
                list.sort_unstable();
                list
            },
        })
    }

    /// The `view` of a frame, or its normalized raw `packets`.
    fn view_of(&mut self, f: &Value) -> Result<Vec<String>, String> {
        if let Some(view) = f.get("view").and_then(Value::as_array) {
            return view
                .iter()
                .map(|e| {
                    e.as_str()
                        .map(str::to_owned)
                        .ok_or("view entries are strings")
                })
                .collect::<Result<_, _>>()
                .map_err(str::to_owned);
        }
        let Some(players) = f.get("packets").and_then(Value::as_object) else {
            return Ok(Vec::new());
        };
        let mut bodies: Vec<(&str, Vec<Vec<u8>>)> = Vec::new();
        for (name, packets) in players {
            let packets = packets
                .as_array()
                .ok_or("packets per player are arrays")?
                .iter()
                .map(|p| p.as_str().and_then(unhex).ok_or("packets are hex strings"))
                .collect::<Result<Vec<_>, _>>()?;
            bodies.push((name, packets));
        }
        Ok(self.clients.frame(
            bodies
                .iter()
                .map(|(n, p)| (*n, p.iter().map(Vec::as_slice).collect())),
        ))
    }

    /// A streamed trace (`ScenarioRecorder --stream`) from its header line, without frames.
    pub fn stream_header(line: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if v.get("format").and_then(Value::as_u64) != Some(FORMAT) {
            return Err(format!("trace format is not {FORMAT}"));
        }
        let region = v.get("region").ok_or("region is missing")?;
        Ok(Self::new(pos(&region[0])?, pos(&region[1])?))
    }

    /// The next frame of a streamed trace: adds the palette entries it carries; `last` holds
    /// the blocks of the frame before (empty before the first).
    pub fn stream_frame(&mut self, line: &str, last: &Arc<Vec<u32>>) -> Result<Frame, String> {
        let f: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if let Some(added) = f.get("palette").and_then(Value::as_array) {
            for s in added {
                self.intern(s.as_str().ok_or("palette entries are strings")?);
            }
        }
        self.frame_of(&f, last)
    }

    /// A checksum of everything a frame of this trace observed, independent of the palette
    /// order and of the order of the entities, so both sides' frames compare by it. `names`
    /// caches the hash of each palette entry (start with an empty one per trace).
    pub fn checksum(&self, frame: &Frame, names: &mut Vec<u64>) -> u64 {
        for name in &self.palette[names.len()..] {
            names.push(fnv(name.as_bytes()));
        }
        let mut h = Checksum::default();
        h.add(u64::from(frame.tick));
        h.add(frame.blocks.len() as u64);
        for &b in frame.blocks.iter() {
            h.add(names[b as usize]);
        }
        for list in [&frame.block_ticks, &frame.fluid_ticks] {
            h.add(list.len() as u64);
            for t in list {
                h.add(u64::from(t.pos.x as u32) << 32 | u64::from(t.pos.z as u32));
                h.add(u64::from(t.pos.y as u32));
                h.add(fnv(t.kind.as_bytes()));
                h.add(t.delay as u64);
                h.add(i64::from(t.priority) as u64);
            }
        }
        let mut keys: Vec<_> = frame.entities.iter().map(EntityObs::key).collect();
        keys.sort_unstable();
        h.add(keys.len() as u64);
        for (kind, bits, detail) in keys {
            h.add(fnv(kind.as_bytes()));
            for b in bits {
                h.add(b);
            }
            h.add(fnv(detail.as_bytes()));
        }
        h.add(frame.light.len() as u64);
        for chunk in frame.light.chunks(8) {
            let mut w = [0; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            h.add(u64::from_le_bytes(w));
        }
        h.add(frame.level_rng.map_or(u64::MAX, |r| r as u64));
        h.add(u64::from(frame.level_rng.is_some()));
        for e in &frame.view {
            h.add(fnv(e.as_bytes()));
        }
        // Only when observed, so the checksums of other scenarios stay as they were.
        if !frame.block_entities.is_empty() {
            h.add(frame.block_entities.len() as u64);
            for e in &frame.block_entities {
                h.add(fnv(e.as_bytes()));
            }
        }
        h.0
    }

    /// Index of a position in the block list, if it is inside the region.
    pub fn index_of(&self, p: BlockPos) -> Option<usize> {
        let [sx, sy, sz] = self.size();
        let (x, y, z) = (p.x - self.min.x, p.y - self.min.y, p.z - self.min.z);
        if x < 0 || y < 0 || z < 0 || x as usize >= sx || y as usize >= sy || z as usize >= sz {
            return None;
        }
        Some((y as usize * sz + z as usize) * sx + x as usize)
    }

    /// The trace in the recorder's format.
    pub fn to_json(&self) -> String {
        let mut s = format!("{{\"format\":{FORMAT},");
        if let Some(b) = &self.blessed_for {
            let _ = write!(s, "\n\"blessed_for\":\"{b}\",");
        }
        let (a, b) = (self.min, self.max);
        let _ = write!(
            s,
            "\n\"region\":[[{},{},{}],[{},{},{}]],\n\"palette\":[",
            a.x, a.y, a.z, b.x, b.y, b.z
        );
        for (i, p) in self.palette.iter().enumerate() {
            let _ = write!(s, "{}\n\"{p}\"", if i == 0 { "" } else { "," });
        }
        s.push_str("],\n\"frames\":[");
        let mut last: Option<&[u32]> = None;
        for (fi, f) in self.frames.iter().enumerate() {
            let _ = write!(
                s,
                "{}\n{{\"tick\":{}",
                if fi == 0 { "" } else { "," },
                f.tick
            );
            match last {
                None => {
                    s.push_str(",\"blocks\":[");
                    let mut first = true;
                    let mut run = 0;
                    for (i, &b) in f.blocks.iter().enumerate() {
                        run += 1;
                        if f.blocks.get(i + 1) != Some(&b) {
                            let _ = write!(s, "{}{run},{b}", if first { "" } else { "," });
                            first = false;
                            run = 0;
                        }
                    }
                }
                Some(prev) => {
                    s.push_str(",\"changes\":[");
                    let mut first = true;
                    for (i, (&b, &p)) in f.blocks.iter().zip(prev).enumerate() {
                        if b != p {
                            let q = self.pos_of(i);
                            let sep = if first { "" } else { "," };
                            let _ = write!(s, "{sep}[{},{},{},{b}]", q.x, q.y, q.z);
                            first = false;
                        }
                    }
                }
            }
            last = Some(f.blocks.as_slice());
            s.push_str("],\"block_ticks\":");
            write_ticks(&mut s, &f.block_ticks);
            s.push_str(",\"fluid_ticks\":");
            write_ticks(&mut s, &f.fluid_ticks);
            s.push_str(",\"entities\":[");
            for (i, e) in f.entities.iter().enumerate() {
                let p = e.pos.map(|v| v.to_bits() as i64);
                let m = e.motion.map(|v| v.to_bits() as i64);
                let _ = write!(
                    s,
                    "{}[\"{}\",{},{},{},{},{},{}",
                    if i == 0 { "" } else { "," },
                    e.kind,
                    p[0],
                    p[1],
                    p[2],
                    m[0],
                    m[1],
                    m[2]
                );
                if let Some(d) = &e.detail {
                    let _ = write!(s, ",{}", Value::Object(d.clone()));
                }
                s.push(']');
            }
            s.push(']');
            if !f.light.is_empty() {
                s.push_str(",\"light\":[");
                let mut run = 0;
                for (i, &level) in f.light.iter().enumerate() {
                    run += 1;
                    if f.light.get(i + 1) != Some(&level) {
                        let sep = if run == i + 1 { "" } else { "," };
                        let _ = write!(s, "{sep}{run},{level}");
                        run = 0;
                    }
                }
                s.push(']');
            }
            if let Some(r) = f.level_rng {
                let _ = write!(s, ",\"level_rng\":{r}");
            }
            if !f.view.is_empty() {
                let _ = write!(s, ",\"view\":{}", Value::from(f.view.clone()));
            }
            if !f.block_entities.is_empty() {
                let _ = write!(
                    s,
                    ",\"block_entities\":{}",
                    Value::from(f.block_entities.clone())
                );
            }
            s.push('}');
        }
        s.push_str("]}\n");
        s
    }

    /// The tick of the first difference of `actual` from this (expected) trace, `None` if
    /// they are equal. A difference outside the frames (region, frame count) counts as tick 0.
    pub fn first_divergence(&self, actual: &Self) -> Option<u32> {
        let first = self.diff(actual, 1).into_iter().next()?;
        Some(
            first
                .strip_prefix("tick ")
                .and_then(|r| r.split(' ').next())
                .and_then(|t| t.parse().ok())
                .unwrap_or(0),
        )
    }

    /// Every difference of `actual` from this (expected) trace, at most `limit` lines, frame by
    /// frame. Blocks compare by state name, ticks in order, entities as a set; all exactly.
    pub fn diff(&self, actual: &Self, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        if (self.min, self.max) != (actual.min, actual.max) {
            out.push(format!(
                "region {:?}..{:?} != {:?}..{:?}",
                actual.min, actual.max, self.min, self.max
            ));
            return out;
        }
        if self.frames.len() != actual.frames.len() {
            out.push(format!(
                "{} frames, expected {}",
                actual.frames.len(),
                self.frames.len()
            ));
        }
        for (e, a) in self.frames.iter().zip(&actual.frames) {
            let t = e.tick;
            for (i, (&eb, &ab)) in e.blocks.iter().zip(a.blocks.iter()).enumerate() {
                let (es, as_) = (&self.palette[eb as usize], &actual.palette[ab as usize]);
                if es != as_ {
                    let p = self.pos_of(i);
                    out.push(format!(
                        "tick {t} block {},{},{}: {as_}, expected {es}",
                        p.x, p.y, p.z
                    ));
                }
            }
            for (what, ev, av) in [
                ("block ticks", &e.block_ticks, &a.block_ticks),
                ("fluid ticks", &e.fluid_ticks, &a.fluid_ticks),
            ] {
                if ev != av {
                    out.push(format!("tick {t} {what}: {av:?}, expected {ev:?}"));
                }
            }
            let mut ee: Vec<_> = e.entities.iter().map(EntityObs::key).collect();
            let mut ae: Vec<_> = a.entities.iter().map(EntityObs::key).collect();
            ee.sort_unstable();
            ae.sort_unstable();
            if ee != ae {
                entity_diff(t, &e.entities, &a.entities, &mut out);
            }
            if e.view != a.view {
                view_diff(t, &e.view, &a.view, &mut out);
            }
            if e.block_entities != a.block_entities {
                block_entity_diff(t, &e.block_entities, &a.block_entities, &mut out);
            }
            if e.level_rng != a.level_rng {
                out.push(format!(
                    "tick {t} level random state {:?}, expected {:?}",
                    a.level_rng, e.level_rng
                ));
            }
            if !e.light.is_empty() && a.light.len() != e.light.len() {
                out.push(format!("tick {t}: light not observed"));
            }
            if a.light.len() == e.light.len() {
                for (i, (&el, &al)) in e.light.iter().zip(&a.light).enumerate() {
                    if el != al && out.len() < limit {
                        let p = self.pos_of(i);
                        out.push(format!(
                            "tick {t} light {},{},{}: sky {} block {}, expected sky {} block {}",
                            p.x,
                            p.y,
                            p.z,
                            al >> 4,
                            al & 15,
                            el >> 4,
                            el & 15
                        ));
                    }
                }
            }
            if out.len() >= limit {
                break;
            }
        }
        out.truncate(limit);
        out
    }
}

/// JSON text with the object keys sorted, whatever order the map keeps.
/// The client view events of a frame that only one side has.
fn view_diff(t: u32, expected: &[String], actual: &[String], out: &mut Vec<String>) {
    let mut missing: Vec<&String> = expected.iter().collect();
    let mut extra = Vec::new();
    for a in actual {
        match missing.iter().position(|e| *e == a) {
            Some(i) => {
                missing.remove(i);
            }
            None => extra.push(a),
        }
    }
    for e in missing {
        out.push(format!("tick {t} client view: missing `{e}`"));
    }
    for a in extra {
        out.push(format!("tick {t} client view: unexpected `{a}`"));
    }
}

/// The block entities of a frame that differ, by position.
fn block_entity_diff(t: u32, expected: &[String], actual: &[String], out: &mut Vec<String>) {
    let split = |e: &String| {
        let (pos, nbt) = e.split_once(' ').unwrap_or((e.as_str(), ""));
        (pos.to_owned(), nbt.to_owned())
    };
    let ex: std::collections::BTreeMap<String, String> = expected.iter().map(split).collect();
    let ac: std::collections::BTreeMap<String, String> = actual.iter().map(split).collect();
    for (pos, e) in &ex {
        match ac.get(pos) {
            None => out.push(format!(
                "tick {t} block entity {pos}: missing, expected {e}"
            )),
            Some(a) if a != e => {
                out.push(format!("tick {t} block entity {pos}: {a}, expected {e}"))
            }
            Some(_) => {}
        }
    }
    for (pos, a) in &ac {
        if !ex.contains_key(pos) {
            out.push(format!("tick {t} block entity {pos}: {a}, expected none"));
        }
    }
}

/// The canonical text of a tag, the same as `ScenarioRecorder.Trace.canonical` on the Java
/// side: compound keys sorted, `attributes` lists sorted by `id`, typed numbers (`1b 2s 3 4L`), floats and doubles as `1.0f` /
/// `1.0d` when integral and below 10^6, else as their bit pattern (`f:3fc00000`, `d:...`),
/// strings quoted with `\` and `"` escaped.
pub fn canonical_nbt(out: &mut String, tag: &vanillite_nbt::Tag) {
    use vanillite_nbt::Tag;
    fn quote(out: &mut String, s: &str) {
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    }
    fn list<T: std::fmt::Display>(out: &mut String, prefix: &str, v: &[T]) {
        out.push_str(prefix);
        for (i, x) in v.iter().enumerate() {
            let _ = write!(out, "{}{x}", if i == 0 { "" } else { "," });
        }
        out.push(']');
    }
    match tag {
        Tag::Compound(c) => {
            let mut entries: Vec<(&str, &Tag)> = c.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (i, (k, v)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                quote(out, k);
                out.push(':');
                match v {
                    // `AttributeMap.pack` lists the instances in hash order (identity hashes):
                    // sorted by id on both sides.
                    Tag::List(l) if k == "attributes" => {
                        let id = |t: &Tag| match t {
                            Tag::Compound(c) => match c.get("id") {
                                Some(Tag::String(s)) => s.clone(),
                                _ => String::new(),
                            },
                            _ => String::new(),
                        };
                        let mut sorted = l.0.clone();
                        sorted.sort_by_key(id);
                        canonical_nbt(out, &Tag::List(vanillite_nbt::List(sorted)));
                    }
                    _ => canonical_nbt(out, v),
                }
            }
            out.push('}');
        }
        Tag::List(l) => {
            out.push('[');
            for (i, v) in l.0.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_nbt(out, v);
            }
            out.push(']');
        }
        Tag::ByteArray(v) => list(out, "[B;", v),
        Tag::IntArray(v) => list(out, "[I;", v),
        Tag::LongArray(v) => list(out, "[L;", v),
        Tag::Byte(v) => {
            let _ = write!(out, "{v}b");
        }
        Tag::Short(v) => {
            let _ = write!(out, "{v}s");
        }
        Tag::Int(v) => {
            let _ = write!(out, "{v}");
        }
        Tag::Long(v) => {
            let _ = write!(out, "{v}L");
        }
        Tag::Float(v) => {
            if v.round_ties_even() == *v && v.abs() < 1.0e6 && v.to_bits() != 0x8000_0000 {
                let _ = write!(out, "{}.0f", *v as i64);
            } else {
                let _ = write!(out, "f:{:x}", v.to_bits());
            }
        }
        Tag::Double(v) => {
            if v.round_ties_even() == *v && v.abs() < 1.0e6 && v.to_bits() != 0x8000_0000_0000_0000
            {
                let _ = write!(out, "{}.0d", *v as i64);
            } else {
                let _ = write!(out, "d:{:x}", v.to_bits());
            }
        }
        Tag::String(v) => quote(out, v),
    }
}

/// Bytes of a hex string.
fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut kv: Vec<_> = m.iter().collect();
            kv.sort_by(|a, b| a.0.cmp(b.0));
            let inner: Vec<String> = kv
                .iter()
                .map(|(k, v)| format!("{k:?}:{}", canonical(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        v => v.to_string(),
    }
}

/// The differences of the entities of one frame: named entities field by field, the others
/// as the lists of what is only on one side.
fn entity_diff(t: u32, expected: &[EntityObs], actual: &[EntityObs], out: &mut Vec<String>) {
    let mut names: Vec<&str> = expected
        .iter()
        .chain(actual)
        .filter_map(EntityObs::name)
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let e = expected.iter().find(|o| o.name() == Some(name));
        let a = actual.iter().find(|o| o.name() == Some(name));
        match (e, a) {
            (Some(e), Some(a)) => {
                for d in e.differences(a) {
                    out.push(format!("tick {t} {name} ({}): {d}", e.kind));
                }
            }
            (Some(e), None) => out.push(format!("tick {t} {name} ({}): missing", e.kind)),
            (None, Some(a)) => out.push(format!("tick {t} {name} ({}): unexpected", a.kind)),
            (None, None) => {}
        }
    }
    let unnamed = |list: &[EntityObs]| -> Vec<EntityObs> {
        let mut v: Vec<EntityObs> = list
            .iter()
            .filter(|o| o.name().is_none())
            .cloned()
            .collect();
        v.sort_by(|a, b| a.key().cmp(&b.key()));
        v
    };
    let (ue, ua) = (unnamed(expected), unnamed(actual));
    let only = |of: &[EntityObs], other: &[EntityObs]| -> Vec<EntityObs> {
        of.iter()
            .filter(|o| !other.iter().any(|x| x.key() == o.key()))
            .cloned()
            .collect()
    };
    let (missing, extra) = (only(&ue, &ua), only(&ua, &ue));
    if missing.len() == 1 && extra.len() == 1 && missing[0].kind == extra[0].kind {
        for d in missing[0].differences(&extra[0]) {
            out.push(format!("tick {t} {}: {d}", missing[0].kind));
        }
        return;
    }
    if !missing.is_empty() {
        out.push(format!("tick {t} entities missing: {missing:?}"));
    }
    if !extra.is_empty() {
        out.push(format!("tick {t} entities unexpected: {extra:?}"));
    }
}

fn pos(v: &Value) -> Result<BlockPos, String> {
    let c = |i: usize| {
        v.get(i)
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok())
            .ok_or_else(|| format!("bad position {v}"))
    };
    Ok(BlockPos::new(c(0)?, c(1)?, c(2)?))
}

fn ticks(v: Option<&Value>) -> Result<Vec<TickObs>, String> {
    let Some(a) = v.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    a.iter()
        .map(|t| {
            Ok(TickObs {
                pos: pos(t)?,
                kind: t
                    .get(3)
                    .and_then(Value::as_str)
                    .ok_or("tick type")?
                    .to_owned(),
                delay: t.get(4).and_then(Value::as_i64).ok_or("tick delay")?,
                priority: t
                    .get(5)
                    .and_then(Value::as_i64)
                    .and_then(|p| i32::try_from(p).ok())
                    .ok_or("tick priority")?,
            })
        })
        .collect()
}

fn entities(v: Option<&Value>) -> Result<Vec<EntityObs>, String> {
    let Some(a) = v.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    a.iter()
        .map(|e| {
            let bits = |i: usize| {
                e.get(i)
                    .and_then(Value::as_i64)
                    .map(|b| f64::from_bits(b as u64))
                    .ok_or_else(|| format!("bad entity {e}"))
            };
            Ok(EntityObs {
                kind: e
                    .get(0)
                    .and_then(Value::as_str)
                    .ok_or("entity type")?
                    .to_owned(),
                pos: [bits(1)?, bits(2)?, bits(3)?],
                motion: [bits(4)?, bits(5)?, bits(6)?],
                detail: e.get(7).and_then(Value::as_object).cloned(),
            })
        })
        .collect()
}

fn write_ticks(s: &mut String, ticks: &[TickObs]) {
    s.push('[');
    for (i, t) in ticks.iter().enumerate() {
        let _ = write!(
            s,
            "{}[{},{},{},\"{}\",{},{}]",
            if i == 0 { "" } else { "," },
            t.pos.x,
            t.pos.y,
            t.pos.z,
            t.kind,
            t.delay,
            t.priority
        );
    }
    s.push(']');
}

/// FNV-1a of bytes: stable across builds and platforms (checksums are stored).
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// An order-dependent 64-bit checksum of words (splitmix64 finaliser per step).
#[derive(Default)]
struct Checksum(u64);

impl Checksum {
    fn add(&mut self, word: u64) {
        let mut z = (self.0 ^ word).wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.0 = z ^ (z >> 31);
    }
}
