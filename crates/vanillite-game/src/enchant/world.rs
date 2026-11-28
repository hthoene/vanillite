//! The enchantment entity effects that change the world (`replace_block`, `replace_disk`,
//! `set_block_properties`, `run_function`, `summon_entity`, `explode`) and their parts read
//! from the enchantment definitions: block predicates (`BlockPredicate`), block state
//! providers (`BlockStateProvider`), float providers and the particle sources of
//! `spawn_particles`. The effects that need the level wait in a [`EnchantAction`]
//! (`WorldAction::Enchant`) until the level is at hand.

use std::sync::Arc;

use serde_json::Value;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockState};
use vanillite_world::entity::EntityId;
use vanillite_worldgen::anchor::{VerticalAnchor, WorldGenerationContext};
use vanillite_worldgen::carver::providers::IntProvider;
use vanillite_worldgen::feature::state_provider::{self, BlockStateProvider as NoiseProvider};

use super::{FloatRandom, LevelBased};
use crate::level::Level;
use crate::level::explosion::{BlockInteraction, CustomExplosion, ExplosionLook, ExplosionStyle};
use crate::level::particles::Particle;

#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn f32_of(v: &Value) -> Option<f32> {
    v.as_f64().map(|v| v as f32)
}

fn int_of(v: &Value) -> Option<i32> {
    v.as_i64().and_then(|v| i32::try_from(v).ok())
}

fn kind_of(o: &serde_json::Map<String, Value>) -> &str {
    o.get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_start_matches("minecraft:")
}

pub(crate) fn qualify(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// A holder set (`"ns:id"`, `"#ns:tag"` or a list of ids) as its entries.
pub(crate) fn holder_entries(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(a) => a.iter().map(|e| e.as_str().map(qualify)).collect(),
        _ => None,
    }
}

/// `Vec3i.CODEC` (`[x, y, z]`), zero when absent.
fn vec3i(v: Option<&Value>) -> Option<[i32; 3]> {
    match v {
        None => Some([0; 3]),
        Some(v) => match v.as_array()?.as_slice() {
            [x, y, z] => Some([int_of(x)?, int_of(y)?, int_of(z)?]),
            _ => None,
        },
    }
}

fn offset(pos: BlockPos, o: [i32; 3]) -> BlockPos {
    pos.offset(o[0], o[1], o[2])
}

/// `FloatProvider`.
#[derive(Clone, Debug, PartialEq)]
pub enum FloatProvider {
    Constant(f32),
    /// `uniform`: `min_inclusive`, `max_exclusive`.
    Uniform(f32, f32),
    /// `clamped_normal`: mean, deviation, min, max.
    ClampedNormal(f32, f32, f32, f32),
    /// `trapezoid`: min, max, plateau.
    Trapezoid(f32, f32, f32),
}

impl FloatProvider {
    /// Parses `FloatProviders.CODEC`: a number or a typed provider.
    pub fn parse(v: &Value) -> Option<FloatProvider> {
        if let Some(n) = f32_of(v) {
            return Some(FloatProvider::Constant(n));
        }
        let o = v.as_object()?;
        let f = |k: &str| o.get(k).and_then(f32_of);
        Some(match kind_of(o) {
            "constant" => FloatProvider::Constant(f("value")?),
            "uniform" => FloatProvider::Uniform(f("min_inclusive")?, f("max_exclusive")?),
            "clamped_normal" => {
                FloatProvider::ClampedNormal(f("mean")?, f("deviation")?, f("min")?, f("max")?)
            }
            "trapezoid" => FloatProvider::Trapezoid(f("min")?, f("max")?, f("plateau")?),
            _ => return None,
        })
    }

    /// `sample(random)`.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    pub fn sample(&self, random: &mut dyn FloatRandom) -> f32 {
        match *self {
            FloatProvider::Constant(v) => v,
            // `Mth.randomBetween`.
            FloatProvider::Uniform(min, max) => random.next_float() * (max - min) + min,
            // `Mth.clamp(Mth.normal(random, mean, deviation), min, max)`.
            FloatProvider::ClampedNormal(mean, deviation, min, max) => {
                (mean + random.next_gaussian() as f32 * deviation).clamp(min, max)
            }
            FloatProvider::Trapezoid(min, max, plateau) => {
                let range = max - min;
                let plateau_start = (range - plateau) / 2.0;
                let plateau_end = range - plateau_start;
                min + random.next_float() * plateau_end + random.next_float() * plateau_start
            }
        }
    }
}

/// An entry of a block or fluid holder set: a registry id or a tag.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Id(u16),
    Tag(String),
}

impl Entry {
    fn parse_blocks(v: &Value) -> Option<Vec<Entry>> {
        holder_entries(v)?
            .into_iter()
            .map(|e| match e.strip_prefix('#') {
                Some(tag) => Some(Entry::Tag(qualify(tag))),
                None => BlockId::by_name(&e).map(|b| Entry::Id(b.0)),
            })
            .collect()
    }

    fn parse_fluids(v: &Value) -> Option<Vec<Entry>> {
        holder_entries(v)?
            .into_iter()
            .map(|e| match e.strip_prefix('#') {
                Some(tag) => Some(Entry::Tag(qualify(tag))),
                None => vanillite_data::registry::FLUID.id(&e).map(Entry::Id),
            })
            .collect()
    }

    fn has_block(entries: &[Entry], block: u16) -> bool {
        entries.iter().any(|e| match e {
            Entry::Id(id) => *id == block,
            Entry::Tag(tag) => {
                crate::player::play::game_data().is_some_and(|d| d.block_in_tag(block, tag))
            }
        })
    }

    fn has_fluid(entries: &[Entry], fluid: u16) -> bool {
        entries.iter().any(|e| match e {
            Entry::Id(id) => *id == fluid,
            Entry::Tag(tag) => crate::player::play::game_data().is_some_and(|d| {
                d.holder_set("minecraft:fluid", &format!("#{tag}"))
                    .contains(&u32::from(fluid))
            }),
        })
    }
}

/// A block predicate (`BlockPredicate`) on a position of the level.
#[derive(Clone, Debug, PartialEq)]
pub enum BlockTest {
    True,
    MatchingBlocks([i32; 3], Vec<Entry>),
    MatchingFluids([i32; 3], Vec<Entry>),
    HasSturdyFace([i32; 3], Direction),
    Solid([i32; 3]),
    Replaceable([i32; 3]),
    WouldSurvive([i32; 3], BlockState),
    InsideWorldBounds([i32; 3]),
    AnyOf(Vec<BlockTest>),
    AllOf(Vec<BlockTest>),
    Not(Box<BlockTest>),
    /// `unobstructed`: no entity that blocks building is in the block (the offset is not
    /// used, like in Vanilla).
    Unobstructed,
    /// `volume_match`: the predicate at every offset of the box.
    VolumeMatch([i32; 3], [i32; 3], Box<BlockTest>),
    /// `matching_biomes`: the biome holder set entries (`"ns:id"` or `"#tag"`).
    MatchingBiomes(Vec<String>),
    /// `height_range`: the Y lies within the anchors resolved against the level.
    HeightRange(VerticalAnchor, VerticalAnchor),
    /// Unknown types: never true.
    Unsupported,
}

fn direction(name: &str) -> Option<Direction> {
    Some(match name {
        "down" => Direction::Down,
        "up" => Direction::Up,
        "north" => Direction::North,
        "south" => Direction::South,
        "west" => Direction::West,
        "east" => Direction::East,
        _ => return None,
    })
}

impl BlockTest {
    /// Parses `BlockPredicate.CODEC`.
    pub fn parse(v: &Value) -> BlockTest {
        Self::try_parse(v).unwrap_or(BlockTest::Unsupported)
    }

    fn try_parse(v: &Value) -> Option<BlockTest> {
        let o = v.as_object()?;
        let off = || vec3i(o.get("offset"));
        let list = |k: &str| -> Option<Vec<BlockTest>> {
            Some(o.get(k)?.as_array()?.iter().map(BlockTest::parse).collect())
        };
        Some(match kind_of(o) {
            "true" => BlockTest::True,
            "matching_blocks" => {
                BlockTest::MatchingBlocks(off()?, Entry::parse_blocks(o.get("blocks")?)?)
            }
            "matching_block_tag" => BlockTest::MatchingBlocks(
                off()?,
                vec![Entry::Tag(qualify(o.get("tag")?.as_str()?))],
            ),
            "matching_fluids" => {
                BlockTest::MatchingFluids(off()?, Entry::parse_fluids(o.get("fluids")?)?)
            }
            "has_sturdy_face" => {
                BlockTest::HasSturdyFace(off()?, direction(o.get("direction")?.as_str()?)?)
            }
            "solid" => BlockTest::Solid(off()?),
            "replaceable" => BlockTest::Replaceable(off()?),
            "would_survive" => BlockTest::WouldSurvive(
                off()?,
                vanillite_worldgen::blocks::parse_block_state(o.get("state")?).ok()?,
            ),
            "inside_world_bounds" => BlockTest::InsideWorldBounds(off()?),
            "matching_biomes" => BlockTest::MatchingBiomes(holder_entries(o.get("biomes")?)?),
            "height_range" => BlockTest::HeightRange(
                VerticalAnchor::parse(o.get("min_inclusive")?).ok()?,
                VerticalAnchor::parse(o.get("max_inclusive")?).ok()?,
            ),
            "any_of" => BlockTest::AnyOf(list("predicates")?),
            "all_of" => BlockTest::AllOf(list("predicates")?),
            "not" => BlockTest::Not(Box::new(BlockTest::parse(o.get("predicate")?))),
            "unobstructed" => BlockTest::Unobstructed,
            "volume_match" => BlockTest::VolumeMatch(
                vec3i(o.get("min"))?,
                vec3i(o.get("max"))?,
                Box::new(BlockTest::parse(o.get("match")?)),
            ),
            _ => BlockTest::Unsupported,
        })
    }

    /// `test(level, pos)`.
    pub fn test(&self, level: &Level, pos: BlockPos) -> bool {
        match self {
            BlockTest::True => true,
            BlockTest::MatchingBlocks(o, entries) => {
                Entry::has_block(entries, level.block(offset(pos, *o)).block().0)
            }
            BlockTest::MatchingFluids(o, entries) => Entry::has_fluid(
                entries,
                level.block(offset(pos, *o)).fluid_state().info().fluid,
            ),
            BlockTest::HasSturdyFace(o, d) => {
                crate::block::is_face_sturdy(level.block(offset(pos, *o)), *d)
            }
            BlockTest::Solid(o) => crate::block::is_solid(level.block(offset(pos, *o))),
            BlockTest::Replaceable(o) => {
                crate::block::can_be_replaced(level.block(offset(pos, *o)))
            }
            BlockTest::WouldSurvive(o, state) => level.can_survive(*state, offset(pos, *o)),
            BlockTest::InsideWorldBounds(o) => {
                let y = offset(pos, *o).y;
                y >= level.min_y() && y <= level.max_y()
            }
            BlockTest::AnyOf(l) => l.iter().any(|t| t.test(level, pos)),
            BlockTest::AllOf(l) => l.iter().all(|t| t.test(level, pos)),
            BlockTest::Not(t) => !t.test(level, pos),
            BlockTest::Unobstructed => super::location::unobstructed(
                level,
                vanillite_data::block::ids::STONE.default_state(),
                pos,
            ),
            BlockTest::VolumeMatch(min, max, t) => (min[0]..=max[0]).all(|x| {
                (min[2]..=max[2])
                    .all(|z| (min[1]..=max[1]).all(|y| t.test(level, pos.offset(x, y, z))))
            }),
            BlockTest::MatchingBiomes(entries) => {
                const BIOME: &str = "minecraft:worldgen/biome";
                let Some(name) = level
                    .climate()
                    .map(|c| c.biomes.get(level.biome(pos)).name.clone())
                else {
                    return false;
                };
                let Some(d) = crate::player::play::game_data() else {
                    return false;
                };
                use vanillite_protocol::components_nbt::RegistryNames;
                let id = vanillite_protocol::item::InstalledRegistries
                    .entry_id(BIOME, &name)
                    .and_then(|id| u32::try_from(id).ok());
                entries.iter().any(|e| match e.strip_prefix('#') {
                    Some(_) => id.is_some_and(|id| d.holder_set(BIOME, e).contains(&id)),
                    None => *e == name,
                })
            }
            BlockTest::HeightRange(min, max) => {
                // `WorldGenerationContext.of(level)` of a server level: its own range and sea
                // level.
                let context = WorldGenerationContext::new(
                    level.min_y(),
                    level.max_y() - level.min_y() + 1,
                    crate::mob::sea_level(level),
                );
                pos.y >= min.resolve_y(&context) && pos.y <= max.resolve_y(&context)
            }
            BlockTest::Unsupported => false,
        }
    }
}

/// A noise based provider of worldgen, equal by identity (its noise is built once at load).
#[derive(Clone, Debug)]
pub struct NoiseState(pub Arc<NoiseProvider>);

impl PartialEq for NoiseState {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A block state provider (`BlockStateProvider`).
#[derive(Clone, Debug, PartialEq)]
pub enum StateProvider {
    /// A plain state or `simple`.
    Simple(BlockState),
    /// `weighted`: states with their weights.
    Weighted(Vec<(BlockState, i32)>),
    /// `random_block`: the default state of one of the blocks.
    RandomBlock(Vec<BlockState>),
    /// `rule_based`: the first rule whose predicate holds, else the fallback.
    RuleBased(Option<Box<StateProvider>>, Vec<(BlockTest, StateProvider)>),
    /// `noise`, `dual_noise` and `noise_threshold` (worldgen's providers: they read no
    /// blocks).
    Noise(NoiseState),
    /// `randomized_int`: the source's state with an integer property set to a sample.
    RandomizedInt(Box<StateProvider>, String, IntProvider),
    /// `rotated`: the source's state turned towards a fixed or random direction.
    Rotated(Box<StateProvider>, Option<Direction>),
    /// `copy_properties`: the source's state with the properties of the block at the
    /// position.
    CopyProperties(Box<StateProvider>),
    /// Registry references and unknown types: the effect does nothing.
    Unsupported,
}

impl StateProvider {
    /// Parses `BlockStateProvider.CODEC`: a block state or a typed provider.
    pub fn parse(v: &Value) -> StateProvider {
        let parse_state = |v: &Value| vanillite_worldgen::blocks::parse_block_state(v).ok();
        let Some(o) = v.as_object().filter(|o| o.contains_key("type")) else {
            return parse_state(v).map_or(StateProvider::Unsupported, StateProvider::Simple);
        };
        let parsed = match kind_of(o) {
            "simple" => o
                .get("state")
                .and_then(parse_state)
                .map(StateProvider::Simple),
            "weighted" => o.get("entries").and_then(Value::as_array).and_then(|a| {
                a.iter()
                    .map(|e| Some((parse_state(e.get("data")?)?, int_of(e.get("weight")?)?)))
                    .collect::<Option<Vec<_>>>()
                    .map(StateProvider::Weighted)
            }),
            "random_block" => o
                .get("blocks")
                .and_then(holder_entries)
                .and_then(|entries| {
                    entries
                        .iter()
                        .map(|e| BlockId::by_name(e).map(|b| b.default_state()))
                        .collect::<Option<Vec<_>>>()
                        .map(StateProvider::RandomBlock)
                }),
            "rule_based" => o.get("rules").and_then(Value::as_array).map(|rules| {
                StateProvider::RuleBased(
                    o.get("fallback").map(|f| Box::new(StateProvider::parse(f))),
                    rules
                        .iter()
                        .map(|r| {
                            (
                                r.get("if_true")
                                    .map_or(BlockTest::Unsupported, BlockTest::parse),
                                r.get("then")
                                    .map_or(StateProvider::Unsupported, StateProvider::parse),
                            )
                        })
                        .collect(),
                )
            }),
            kind @ ("noise" | "dual_noise" | "noise_threshold") => {
                NoiseProvider::parse_noise_kind(kind, v)
                    .ok()
                    .map(|p| StateProvider::Noise(NoiseState(Arc::new(p))))
            }
            "randomized_int" => o
                .get("source")
                .zip(o.get("values"))
                .and_then(|(src, values)| {
                    Some(StateProvider::RandomizedInt(
                        Box::new(StateProvider::parse(src)),
                        o.get("property")?.as_str()?.to_owned(),
                        IntProvider::from_json(values).ok()?,
                    ))
                }),
            "rotated" => o.get("state").and_then(|state| {
                let direction = match o.get("direction") {
                    None => None,
                    Some(d) => Some(direction(d.as_str()?)?),
                };
                Some(StateProvider::Rotated(
                    Box::new(StateProvider::parse(state)),
                    direction,
                ))
            }),
            "copy_properties" => o
                .get("source")
                .map(|src| StateProvider::CopyProperties(Box::new(StateProvider::parse(src)))),
            _ => None,
        };
        parsed.unwrap_or(StateProvider::Unsupported)
    }

    /// Whether the provider gives states.
    fn supported(&self) -> bool {
        match self {
            StateProvider::Unsupported => false,
            StateProvider::RuleBased(fallback, rules) => {
                fallback.as_ref().is_none_or(|f| f.supported())
                    && rules.iter().all(|(_, then)| then.supported())
            }
            StateProvider::RandomizedInt(source, ..)
            | StateProvider::Rotated(source, _)
            | StateProvider::CopyProperties(source) => source.supported(),
            _ => true,
        }
    }

    /// `getState(level, random, pos)` (the current state where a rule based provider has no
    /// rule or a random block provider no blocks).
    pub fn get_state(&self, level: &Level, random: &mut LegacyRandom, pos: BlockPos) -> BlockState {
        self.get_optional_state(level, random, pos)
            .unwrap_or_else(|| level.block(pos))
    }

    fn get_optional_state(
        &self,
        level: &Level,
        random: &mut LegacyRandom,
        pos: BlockPos,
    ) -> Option<BlockState> {
        match self {
            StateProvider::Simple(s) => Some(*s),
            StateProvider::Weighted(list) => {
                // `WeightedList.getRandomOrThrow`.
                let total: i32 = list.iter().map(|(_, w)| *w).sum();
                if total <= 0 {
                    return None;
                }
                let mut pick = random.next_int_bounded(total);
                for (state, weight) in list {
                    pick -= weight;
                    if pick < 0 {
                        return Some(*state);
                    }
                }
                None
            }
            StateProvider::RandomBlock(blocks) => {
                let n = i32::try_from(blocks.len()).ok().filter(|&n| n > 0)?;
                let i = usize::try_from(random.next_int_bounded(n)).ok()?;
                blocks.get(i).copied()
            }
            StateProvider::RuleBased(fallback, rules) => {
                for (test, then) in rules {
                    if test.test(level, pos)
                        && let Some(s) = then.get_optional_state(level, random, pos)
                    {
                        return Some(s);
                    }
                }
                fallback
                    .as_ref()
                    .and_then(|f| f.get_optional_state(level, random, pos))
            }
            StateProvider::Noise(p) => p.0.noise_state(random, pos),
            // `RandomizedIntStateProvider.getState`: a value the property does not have keeps
            // the state (Vanilla throws).
            StateProvider::RandomizedInt(source, name, values) => {
                let state = source.get_state(level, random, pos);
                let Some(property) = state_provider::integer_property(state, name) else {
                    return Some(state);
                };
                let value = values.sample(random).to_string();
                Some(state.with(property.name, &value).unwrap_or(state))
            }
            StateProvider::Rotated(source, direction) => {
                // `Direction.getRandom(random)` before the source's state.
                let direction = match direction {
                    Some(d) => *d,
                    None => {
                        let i = usize::try_from(random.next_int_bounded(6)).unwrap_or(0);
                        Direction::ALL.get(i).copied().unwrap_or(Direction::Down)
                    }
                };
                Some(state_provider::rotate(
                    source.get_state(level, random, pos),
                    direction,
                ))
            }
            StateProvider::CopyProperties(source) => Some(state_provider::with_properties_of(
                source.get_state(level, random, pos),
                level.block(pos),
            )),
            StateProvider::Unsupported => None,
        }
    }
}

/// `replace_block`, and the block part of `replace_disk`.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockChange {
    pub offset: [i32; 3],
    pub predicate: Option<BlockTest>,
    pub state: StateProvider,
    /// `trigger_game_event`.
    pub game_event: Option<String>,
}

impl BlockChange {
    fn parse(o: &serde_json::Map<String, Value>) -> Option<BlockChange> {
        let state = StateProvider::parse(o.get("block_state")?);
        state.supported().then(|| BlockChange {
            offset: vec3i(o.get("offset")).unwrap_or([0; 3]),
            predicate: o.get("predicate").map(BlockTest::parse),
            state,
            game_event: o
                .get("trigger_game_event")
                .and_then(Value::as_str)
                .map(qualify),
        })
    }
}

/// `replace_disk`.
#[derive(Clone, Debug, PartialEq)]
pub struct DiskChange {
    pub radius: LevelBased,
    pub height: LevelBased,
    pub change: BlockChange,
}

/// `set_block_properties`.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyChange {
    pub properties: Vec<(String, String)>,
    pub offset: [i32; 3],
    pub game_event: Option<String>,
}

/// `summon_entity`.
#[derive(Clone, Debug, PartialEq)]
pub struct Summon {
    /// `entity`: the entity type entries (`"ns:id"` or `"#tag"`).
    pub entities: Vec<String>,
    /// `join_team`.
    pub join_team: bool,
}

/// `Level.ExplosionInteraction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interaction {
    None,
    Block,
    Mob,
    Tnt,
    Trigger,
}

/// `explode`.
#[derive(Clone, Debug, PartialEq)]
pub struct Explode {
    pub attribute_to_user: bool,
    pub damage_type: Option<String>,
    pub knockback: Option<LevelBased>,
    /// `immune_blocks` entries.
    pub immune: Option<Vec<String>>,
    pub offset: Vec3,
    pub radius: LevelBased,
    pub create_fire: bool,
    pub interaction: Interaction,
    pub look: Arc<ExplosionLook>,
}

/// A particle of the data packs (`ParticleTypes.CODEC`): its type and options.
fn particle(v: &Value) -> Option<Particle> {
    let o = v.as_object()?;
    let id = qualify(o.get("type")?.as_str()?);
    let mut options = vanillite_nbt::Compound::new();
    for (k, v) in o {
        if k != "type" {
            options.insert(
                k.as_str(),
                vanillite_protocol::item_defaults::json_to_nbt(v),
            );
        }
    }
    crate::level::particle_options::from_options(&id, Some(&options)).ok()
}

impl Explode {
    fn parse(o: &serde_json::Map<String, Value>) -> Option<Explode> {
        let interaction = match o.get("block_interaction")?.as_str()? {
            "none" => Interaction::None,
            "block" => Interaction::Block,
            "mob" => Interaction::Mob,
            "tnt" => Interaction::Tnt,
            "trigger" => Interaction::Trigger,
            _ => return None,
        };
        let offset = match o.get("offset") {
            None => Vec3::ZERO,
            Some(v) => match v.as_array()?.as_slice() {
                [x, y, z] => Vec3::new(x.as_f64()?, y.as_f64()?, z.as_f64()?),
                _ => return None,
            },
        };
        let block_particles = match o.get("block_particles") {
            None => Vec::new(),
            Some(v) => v
                .as_array()?
                .iter()
                .map(|e| {
                    // `WeightedList` of a map codec: the entry fields next to `weight`.
                    let data = e.get("data").unwrap_or(e);
                    Some((
                        particle(data.get("particle")?)?,
                        data.get("scaling").map_or(Some(1.0), f32_of)?,
                        data.get("speed").map_or(Some(1.0), f32_of)?,
                        int_of(e.get("weight")?)?,
                    ))
                })
                .collect::<Option<Vec<_>>>()?,
        };
        let sound = match o.get("sound")? {
            Value::String(s) => qualify(s),
            Value::Object(s) => qualify(s.get("sound_id")?.as_str()?),
            _ => return None,
        };
        Some(Explode {
            attribute_to_user: o
                .get("attribute_to_user")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            damage_type: o.get("damage_type").and_then(Value::as_str).map(qualify),
            knockback: match o.get("knockback_multiplier") {
                None => None,
                Some(v) => Some(LevelBased::parse(v)?),
            },
            immune: match o.get("immune_blocks") {
                None => None,
                Some(v) => Some(holder_entries(v)?),
            },
            offset,
            radius: LevelBased::parse(o.get("radius")?)?,
            create_fire: o
                .get("create_fire")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            interaction,
            look: Arc::new(ExplosionLook {
                small: particle(o.get("small_particle")?)?,
                large: particle(o.get("large_particle")?)?,
                block_particles,
                sound,
            }),
        })
    }
}

/// `SpawnParticlesEffect.PositionSource`: `entity_position` (`false`) or `in_bounding_box`,
/// with its offset and scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionSource {
    pub in_box: bool,
    pub offset: f32,
    pub scale: f32,
}

impl PositionSource {
    fn parse(v: &Value) -> Option<PositionSource> {
        let in_box = match v.get("type")?.as_str()? {
            "entity_position" => false,
            "in_bounding_box" => true,
            _ => return None,
        };
        let scale = v.get("scale").map_or(Some(1.0), f32_of)?;
        // `Cannot scale an entity position coordinate source`.
        if !in_box && scale.to_bits() != 1.0_f32.to_bits() {
            return None;
        }
        Some(PositionSource {
            in_box,
            offset: v.get("offset").map_or(Some(0.0), f32_of)?,
            scale,
        })
    }

    /// `getCoordinate(position, center, boundingBoxSpan, random)`.
    fn coordinate(&self, position: f64, center: f64, span: f32, random: &mut LegacyRandom) -> f64 {
        let base = if self.in_box {
            center + (random.next_double() - 0.5) * f64::from(span * self.scale)
        } else {
            position
        };
        base + f64::from(self.offset)
    }
}

/// `SpawnParticlesEffect.VelocitySource`.
#[derive(Clone, Debug, PartialEq)]
pub struct VelocitySource {
    pub movement_scale: f32,
    pub base: FloatProvider,
}

impl VelocitySource {
    fn parse(v: &Value) -> Option<VelocitySource> {
        Some(VelocitySource {
            movement_scale: v.get("movement_scale").map_or(Some(0.0), f32_of)?,
            base: v
                .get("base")
                .map_or(Some(FloatProvider::Constant(0.0)), FloatProvider::parse)?,
        })
    }

    fn velocity(&self, movement: f64, random: &mut LegacyRandom) -> f64 {
        movement * f64::from(self.movement_scale) + f64::from(self.base.sample(random))
    }
}

/// `spawn_particles`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParticleSpawn {
    pub particle: Particle,
    pub horizontal_position: PositionSource,
    pub vertical_position: PositionSource,
    pub horizontal_velocity: VelocitySource,
    pub vertical_velocity: VelocitySource,
    pub speed: FloatProvider,
}

impl ParticleSpawn {
    pub(crate) fn parse(o: &serde_json::Map<String, Value>) -> Option<ParticleSpawn> {
        Some(ParticleSpawn {
            particle: particle(o.get("particle")?)?,
            horizontal_position: PositionSource::parse(o.get("horizontal_position")?)?,
            vertical_position: PositionSource::parse(o.get("vertical_position")?)?,
            horizontal_velocity: VelocitySource::parse(o.get("horizontal_velocity")?)?,
            vertical_velocity: VelocitySource::parse(o.get("vertical_velocity")?)?,
            speed: o
                .get("speed")
                .map_or(Some(FloatProvider::Constant(0.0)), FloatProvider::parse)?,
        })
    }

    /// `SpawnParticlesEffect.apply`: the particle packet at `position` of an entity with
    /// the known movement and box size, its coordinates drawn from the entity's random.
    pub fn packet(
        &self,
        position: Vec3,
        movement: Vec3,
        (width, height): (f32, f32),
        random: &mut LegacyRandom,
    ) -> crate::level::particles::LevelParticles {
        let h = &self.horizontal_position;
        let x = h.coordinate(position.x, position.x, width, random);
        let y = self.vertical_position.coordinate(
            position.y,
            position.y + f64::from(height / 2.0),
            height,
            random,
        );
        let z = h.coordinate(position.z, position.z, width, random);
        let vx = self.horizontal_velocity.velocity(movement.x, random);
        let vy = self.vertical_velocity.velocity(movement.y, random);
        let vz = self.horizontal_velocity.velocity(movement.z, random);
        let speed = self.speed.sample(random);
        crate::level::particles::packet(
            self.particle.clone(),
            Vec3::new(x, y, z),
            0,
            Vec3::new(vx, vy, vz),
            f64::from(speed),
        )
    }
}

/// The world effects of [`super::EntityEffect`].
#[derive(Clone, Debug, PartialEq)]
pub enum WorldEffect {
    ReplaceBlock(BlockChange),
    ReplaceDisk(DiskChange),
    SetBlockProperties(PropertyChange),
    RunFunction(String),
    Summon(Summon),
    Explode(Explode),
}

impl WorldEffect {
    /// Parses the world effect types; `None` for other types or definitions that are not
    /// understood.
    pub fn parse(kind: &str, o: &serde_json::Map<String, Value>) -> Option<WorldEffect> {
        Some(match kind {
            "replace_block" => WorldEffect::ReplaceBlock(BlockChange::parse(o)?),
            "replace_disk" => WorldEffect::ReplaceDisk(DiskChange {
                radius: LevelBased::parse(o.get("radius")?)?,
                height: LevelBased::parse(o.get("height")?)?,
                change: BlockChange::parse(o)?,
            }),
            "set_block_properties" => WorldEffect::SetBlockProperties(PropertyChange {
                properties: o
                    .get("properties")?
                    .as_object()?
                    .iter()
                    .map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect::<Option<Vec<_>>>()?,
                offset: vec3i(o.get("offset"))?,
                // The codec's default is no event; the Java constructor's is `block_change`.
                game_event: o
                    .get("trigger_game_event")
                    .and_then(Value::as_str)
                    .map(qualify),
            }),
            "run_function" => WorldEffect::RunFunction(qualify(o.get("function")?.as_str()?)),
            "summon_entity" => WorldEffect::Summon(Summon {
                entities: holder_entries(o.get("entity")?)?,
                join_team: o.get("join_team").and_then(Value::as_bool).unwrap_or(false),
            }),
            "explode" => WorldEffect::Explode(Explode::parse(o)?),
            _ => return None,
        })
    }
}

/// A world effect of an enchantment waiting for the level.
#[derive(Clone, Debug, PartialEq)]
pub struct EnchantAction {
    pub effect: WorldEffect,
    /// The enchantment level.
    pub level: i32,
    /// The affected entity.
    pub entity: EntityId,
    /// The effect's position (the entity's, or where a projectile hit).
    pub pos: Vec3,
    /// The owner of the enchanted item (`EnchantedItemInUse.owner`).
    pub owner: Option<EntityId>,
    /// The damage type of an `explode` effect, resolved where the damage types are known.
    pub damage: Option<crate::survival::damage::DamageKind>,
}

/// Runs `f` with the entity's random (`Entity.getRandom`): a mob's or player's own, else the
/// shared entity random.
fn with_entity_random<T>(
    level: &mut Level,
    id: EntityId,
    f: impl FnOnce(&mut Level, &mut LegacyRandom) -> T,
) -> T {
    let taken = if let Some(m) = level.entities.mob_mut(id) {
        Some(std::mem::replace(&mut m.random, LegacyRandom::new(0)))
    } else {
        level
            .entities
            .player_mut(id)
            .map(|p| std::mem::replace(&mut p.survival.random, LegacyRandom::new(0)))
    };
    let Some(mut random) = taken else {
        return crate::experience_orb::with_entity_random(|r| f(level, r));
    };
    let out = f(level, &mut random);
    if let Some(m) = level.entities.mob_mut(id) {
        m.random = random;
    } else if let Some(p) = level.entities.player_mut(id) {
        p.survival.random = random;
    }
    out
}

/// `serverLevel.gameEvent(entity, event, pos)`.
fn game_event(level: &mut Level, event: Option<&str>, entity: EntityId, pos: BlockPos) {
    let Some(id) = event.and_then(|e| vanillite_data::registry::GAME_EVENT.id(e)) else {
        return;
    };
    let context = crate::level::game_event::GameEventContext {
        source: Some(entity),
        affected: None,
        owner: None,
    };
    level.game_event(
        id,
        Vec3::new(
            f64::from(pos.x) + 0.5,
            f64::from(pos.y) + 0.5,
            f64::from(pos.z) + 0.5,
        ),
        context,
    );
}

fn block_of(pos: Vec3) -> BlockPos {
    BlockPos::new(
        vanillite_base::math::floor(pos.x),
        vanillite_base::math::floor(pos.y),
        vanillite_base::math::floor(pos.z),
    )
}

/// `replace_block` / one block of `replace_disk` at `pos`: the predicate, then
/// `setBlockAndUpdate` with the provider's state and the game event.
fn replace_at(
    level: &mut Level,
    change: &BlockChange,
    entity: EntityId,
    pos: BlockPos,
    random: &mut LegacyRandom,
) {
    if change
        .predicate
        .as_ref()
        .is_some_and(|p| !p.test(level, pos))
    {
        return;
    }
    let state = change.state.get_state(level, random, pos);
    if level.set_block_and_update(pos, state) {
        game_event(level, change.game_event.as_deref(), entity, pos);
    }
}

/// `EnchantmentEntityEffect.apply` of a world effect.
///
/// Returns the entity a `summon_entity` effect with `join_team` made, which then joins the
/// team of the affected entity (the caller has the scoreboard).
pub fn apply(level: &mut Level, a: &EnchantAction) -> Option<EntityId> {
    let entity = a.entity;
    match &a.effect {
        WorldEffect::ReplaceBlock(change) => {
            let pos = offset(block_of(a.pos), change.offset);
            with_entity_random(level, entity, |level, random| {
                replace_at(level, change, entity, pos, random);
            });
        }
        WorldEffect::ReplaceDisk(disk) => {
            let center = offset(block_of(a.pos), disk.change.offset);
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let (dist, height) = (
                disk.radius.calculate(a.level) as i32,
                disk.height.calculate(a.level) as i32,
            );
            // `BlockPos.betweenClosed(center - (dist, 0, dist), center + (dist, min(height - 1,
            // 0), dist))`: x fastest, then y, then z.
            let (y0, y1) = {
                let top = center.y.saturating_add(height.saturating_sub(1).min(0));
                (center.y.min(top), center.y.max(top))
            };
            let limit = f64::from(dist) * f64::from(dist);
            let span = |c: i32| {
                let (a, b) = (c.saturating_sub(dist), c.saturating_add(dist));
                a.min(b)..=a.max(b)
            };
            with_entity_random(level, entity, |level, random| {
                for z in span(center.z) {
                    for y in y0..=y1 {
                        for x in span(center.x) {
                            let dx = f64::from(x) + 0.5 - a.pos.x;
                            let dz = f64::from(z) + 0.5 - a.pos.z;
                            if dx * dx + dz * dz < limit {
                                replace_at(
                                    level,
                                    &disk.change,
                                    entity,
                                    BlockPos::new(x, y, z),
                                    random,
                                );
                            }
                        }
                    }
                }
            });
        }
        WorldEffect::SetBlockProperties(change) => {
            let pos = offset(block_of(a.pos), change.offset);
            let state = level.block(pos);
            let modified = change
                .properties
                .iter()
                .fold(state, |s, (k, v)| s.with(k, v).unwrap_or(s));
            if modified != state && level.set_block_and_update(pos, modified) {
                game_event(level, change.game_event.as_deref(), entity, pos);
            }
        }
        WorldEffect::RunFunction(id) => {
            let rot = level
                .entities
                .store
                .get(entity)
                .map_or((0.0, 0.0), |e| (e.rot().y_rot, e.rot().x_rot));
            if let Some(runner) = level.command_runner() {
                runner.run_function(level, id, entity, a.pos, rot);
            }
        }
        WorldEffect::Summon(summon) => {
            return apply_summon(level, a, summon).filter(|_| summon.join_team);
        }
        WorldEffect::Explode(e) => apply_explode(level, a, e),
    }
    None
}

/// `SummonEntityEffect.apply`: in spawnable bounds, an entity type picked with the level
/// random, made like `EntityType.spawn(level, pos, TRIGGERED)` (at the block's centre with its
/// yaw from the level random; a mob is finalized there and plays its ambient sound) and then
/// moved to the exact position; a lightning bolt's cause is a player owner. Returns the new
/// entity.
fn apply_summon(level: &mut Level, a: &EnchantAction, summon: &Summon) -> Option<EntityId> {
    let block = block_of(a.pos);
    let in_bounds = (-30_000_000..30_000_000).contains(&block.x)
        && (-30_000_000..30_000_000).contains(&block.z)
        && (-20_000_000..20_000_000).contains(&block.y);
    if !in_bounds {
        return None;
    }
    let types: Vec<String> = crate::player::play::game_data().map_or_else(Vec::new, |d| {
        summon
            .entities
            .iter()
            .flat_map(|e| d.holder_set("minecraft:entity_type", e))
            .filter_map(|id| u16::try_from(id).ok())
            .filter_map(|id| vanillite_data::registry::ENTITY_TYPE.name(id))
            .map(str::to_owned)
            .collect()
    });
    let n = i32::try_from(types.len()).ok().filter(|&n| n > 0)?;
    // `HolderSet.getRandomElement(level.getRandom())`.
    let pick = level.random().next_int_bounded(n);
    let kind = usize::try_from(pick).ok().and_then(|i| types.get(i))?;
    if let Some(mob) =
        crate::mob::kinds::Kind::by_name(kind).filter(|k| k.family() != crate::mob::Family::Decor)
    {
        let id = crate::mob::animal::spawn_from_item(
            level,
            mob,
            &crate::item::ItemStack::EMPTY,
            block,
            crate::mob::fam::SpawnReason::Triggered,
            false,
            false,
        )?;
        // `snapTo(position, yRot, xRot)`.
        if let Some(mut e) = level.entities.store.get_mut(id) {
            e.set_pos(a.pos);
            e.set_old_pos();
        }
        return Some(id);
    }
    // `snapTo(..., wrapDegrees(level.getRandom().nextFloat() * 360), 0)`.
    let yaw =
        vanillite_base::math::wrap_degrees_f(RandomSource::next_float(level.random()) * 360.0);
    if kind == "minecraft:lightning_bolt" {
        let bolt = crate::lightning::spawn(level, a.pos, false);
        let cause = a.owner.filter(|&o| level.entities.player(o).is_some());
        if let Some(b) = level.entities.bolt_mut(bolt) {
            b.cause = cause;
        }
        return Some(bolt);
    }
    let mut nbt = vanillite_nbt::Compound::new();
    nbt.insert("id", kind.as_str());
    nbt.insert(
        "Pos",
        vanillite_nbt::Tag::List(vanillite_nbt::List(vec![
            vanillite_nbt::Tag::Double(a.pos.x),
            vanillite_nbt::Tag::Double(a.pos.y),
            vanillite_nbt::Tag::Double(a.pos.z),
        ])),
    );
    nbt.insert(
        "Rotation",
        vanillite_nbt::Tag::List(vanillite_nbt::List(vec![
            vanillite_nbt::Tag::Float(yaw),
            vanillite_nbt::Tag::Float(0.0),
        ])),
    );
    // `EntityType.create`: the constructor takes its seed and UUID like `/summon`'s.
    let (uuid, seed) = level.new_uuid_and_seed();
    nbt.insert("UUID", crate::mob::nbt::uuid_tag(uuid));
    level
        .load_entity_seeded(nbt, Some(seed))
        .then(|| level.entities.store.by_uuid(uuid))
        .flatten()
}

/// `ExplodeEffect.apply`: `serverLevel.explode` at the position plus the offset with a
/// `SimpleExplosionDamageCalculator` and the effect's particles and sound.
fn apply_explode(level: &mut Level, a: &EnchantAction, e: &Explode) {
    let pos = Vec3::new(
        a.pos.x + e.offset.x,
        a.pos.y + e.offset.y,
        a.pos.z + e.offset.z,
    );
    let s = &level.settings;
    let decay = |rule: bool| {
        if rule {
            BlockInteraction::DestroyWithDecay
        } else {
            BlockInteraction::Destroy
        }
    };
    let interaction = match e.interaction {
        Interaction::None => BlockInteraction::Keep,
        Interaction::Block => decay(s.block_explosion_drop_decay),
        Interaction::Mob if s.mob_griefing => decay(s.mob_explosion_drop_decay),
        Interaction::Mob => BlockInteraction::Keep,
        Interaction::Tnt => decay(s.tnt_explosion_drop_decay),
        Interaction::Trigger => BlockInteraction::TriggerBlock,
    };
    let immune = e.immune.as_ref().map(|entries| {
        let mut ids: Vec<u16> = crate::player::play::game_data().map_or_else(Vec::new, |d| {
            entries
                .iter()
                .flat_map(|x| d.holder_set("minecraft:block", x))
                .filter_map(|id| u16::try_from(id).ok())
                .collect()
        });
        ids.sort_unstable();
        ids
    });
    let damage = if e.damage_type.is_some() {
        a.damage.map(|kind| (kind, e.attribute_to_user))
    } else {
        None
    };
    let style = ExplosionStyle {
        damages_entities: e.damage_type.is_some(),
        knockback: e
            .knockback
            .as_ref()
            .map_or(1.0, |k| f64::from(k.calculate(a.level))),
        wind_sound: None,
        resistance: if immune.is_some() {
            crate::level::explosion::BlockResistance::Immune
        } else {
            crate::level::explosion::BlockResistance::Default
        },
        fire: e.create_fire,
        bad_respawn_point: false,
        damage,
    };
    level.explode_custom(
        e.attribute_to_user.then_some(a.entity),
        pos,
        e.radius.calculate(a.level).max(0.0),
        interaction,
        style,
        CustomExplosion {
            immune: immune.unwrap_or_default(),
            look: Arc::clone(&e.look),
        },
    );
}

/// Runs `f` with the random of an entity (`Entity.getRandom`) while the entities are out of
/// the level: a mob's or player's own, else the shared entity random.
pub fn with_ents_random<T>(
    ents: &mut crate::entities::Entities,
    id: EntityId,
    f: impl FnOnce(&mut LegacyRandom) -> T,
) -> T {
    if let Some(m) = ents.mob_mut(id) {
        return f(&mut m.random);
    }
    if let Some(p) = ents.player_mut(id) {
        return f(&mut p.survival.random);
    }
    crate::experience_orb::with_entity_random(f)
}

/// `SpawnParticlesEffect.apply` on `entity` at `pos`: the packet, its coordinates drawn from
/// the entity's random now.
pub fn spawn_particles(
    ents: &mut crate::entities::Entities,
    effect: &ParticleSpawn,
    entity: EntityId,
    pos: Vec3,
) -> Option<crate::level::particles::LevelParticles> {
    let dims = ents.store.get(entity)?.dims();
    let movement = crate::spear::known_movement(ents, entity);
    Some(with_ents_random(ents, entity, |r| {
        effect.packet(pos, movement, (dims.width, dims.height), r)
    }))
}

/// `PlaySoundEffect.apply` on `entity` at `pos` (nothing for a silent entity): the sound,
/// its source, volume and pitch (drawn from the entity's random now).
pub fn sound(
    ents: &mut crate::entities::Entities,
    sounds: &[String],
    volume: &FloatProvider,
    pitch: &FloatProvider,
    level: i32,
    entity: EntityId,
) -> Option<(
    String,
    vanillite_protocol::packets::play::clientbound::SoundSource,
    f32,
    f32,
)> {
    use vanillite_protocol::packets::play::clientbound::SoundSource;
    if ents.mob(entity).is_some_and(|m| m.silent) {
        return None;
    }
    let sound = super::EntityEffect::sound_of_level(sounds, level)?.to_owned();
    let source = if ents.player(entity).is_some() {
        SoundSource::Players
    } else {
        ents.mob(entity)
            .map_or(SoundSource::Neutral, |m| m.sound_source())
    };
    let (v, p) = with_ents_random(ents, entity, |r| (volume.sample(r), pitch.sample(r)));
    Some((sound, source, v, p))
}

/// The [`EnchantAction`] of a world effect on `entity` at `pos`, with the damage type of an
/// `explode` resolved.
pub fn action(
    effect: &WorldEffect,
    level: i32,
    entity: EntityId,
    pos: Vec3,
    owner: Option<EntityId>,
    types: &crate::survival::damage::DamageTypes,
) -> EnchantAction {
    let damage = match effect {
        WorldEffect::Explode(e) => e.damage_type.as_deref().and_then(|t| types.kind(t)),
        _ => None,
    };
    EnchantAction {
        effect: effect.clone(),
        level,
        entity,
        pos,
        owner,
        damage,
    }
}
