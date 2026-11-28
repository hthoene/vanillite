//! Block state providers (`net.minecraft.world.level.levelgen.feature.stateproviders`).
//!
//! Every type of `BlockStateProviderTypes` is ported. Nested providers are
//! `Holder<BlockStateProvider>`s and go through [`FeatureParser::state_provider`] (registry
//! references to `worldgen/block_state_provider` or inline definitions). Noise based providers
//! build their noise at parse time, seeded like Vanilla's constructors
//! (`parameters.create(new WorldgenRandom(new LegacyRandomSource(seed)))`).
//!
//! Providers are cheap to clone (nested data sits behind [`Arc`]s), since registry entries are
//! parsed once and cloned into every feature that refers to them.

use std::sync::Arc;

use serde_json::Value;
use vanillite_base::math::{clamp_f, clamped_map};
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Direction};
use vanillite_data::block::{BlockState, PROPERTIES, PropertyInfo};

use super::FeatureParser;
use super::predicate::{BlockPredicate, ordered_blocks, parse_direction};
use super::region::WorldGenRegion;
use crate::Error;
use crate::anchor::codec_int;
use crate::blocks::parse_block_state;
use crate::carver::providers::{IntProvider, WeightedList};
use crate::noise::{Noise, NormalNoise};
use crate::random::AnyRandom;

/// A block state provider (`BlockStateProvider`).
#[derive(Clone, Debug)]
pub enum BlockStateProvider {
    /// `simple` (`SimpleStateProvider`) or a plain block state.
    Simple(BlockState),
    /// `weighted` (`WeightedStateProvider`).
    Weighted(Arc<WeightedList<BlockState>>),
    /// `randomized_int` (`RandomizedIntStateProvider`).
    RandomizedInt(Arc<RandomizedIntProvider>),
    /// `rotated` (`RotatedBlockProvider`).
    Rotated(Arc<RotatedProvider>),
    /// `noise` (`NoiseProvider`).
    Noise(Arc<NoiseProvider>),
    /// `dual_noise` (`DualNoiseProvider`).
    DualNoise(Arc<DualNoiseProvider>),
    /// `noise_threshold` (`NoiseThresholdProvider`).
    NoiseThreshold(Arc<NoiseThresholdProvider>),
    /// `rule_based` (`RuleBasedStateProvider`).
    RuleBased(Arc<RuleBasedProvider>),
    /// `copy_properties` (`CopyPropertiesProvider`): the source's state with the properties of
    /// the block at the position.
    CopyProperties(Arc<BlockStateProvider>),
    /// `random_block` (`RandomBlockProvider`): the default state of a block picked uniformly
    /// from the set, in the set's order.
    RandomBlock(Arc<[BlockState]>),
}

/// `RandomizedIntStateProvider`: the source's state with an integer property set to a sample.
#[derive(Debug)]
pub struct RandomizedIntProvider {
    pub source: BlockStateProvider,
    pub property: String,
    pub values: IntProvider,
}

/// `RotatedBlockProvider`: the source's state turned towards a fixed or random direction.
#[derive(Debug)]
pub struct RotatedProvider {
    pub state: BlockStateProvider,
    /// `None` = `Direction.getRandom(random)` on every call.
    pub direction: Option<Direction>,
}

/// `NoiseProvider`: one of `states`, chosen by the noise at the position.
#[derive(Debug)]
pub struct NoiseProvider {
    /// `NoiseBasedStateProvider.noise`.
    pub noise: Noise,
    pub scale: f32,
    pub states: Vec<BlockState>,
}

/// `DualNoiseProvider`: a slow noise picks how many and which of `states` are candidates, the
/// fast noise picks among them.
#[derive(Debug)]
pub struct DualNoiseProvider {
    pub base: NoiseProvider,
    pub variety_min: i32,
    pub variety_max: i32,
    pub slow_noise: Noise,
    pub slow_scale: f32,
}

/// `NoiseThresholdProvider`.
#[derive(Debug)]
pub struct NoiseThresholdProvider {
    pub noise: Noise,
    pub scale: f32,
    pub threshold: f32,
    pub high_chance: f32,
    pub default_state: BlockState,
    pub low_states: Vec<BlockState>,
    pub high_states: Vec<BlockState>,
}

/// `RuleBasedStateProvider`.
#[derive(Debug)]
pub struct RuleBasedProvider {
    pub fallback: Option<BlockStateProvider>,
    /// `Rule(ifTrue, then)` in order.
    pub rules: Vec<(BlockPredicate, BlockStateProvider)>,
}

impl BlockStateProvider {
    /// Parses `BlockStateProvider.DIRECT_CODEC`: a block state (`"minecraft:stone"`,
    /// `{"id": …, "properties": …}`) or a typed provider (dispatch on `type`).
    pub fn parse(json: &Value, parser: &mut FeatureParser<'_>) -> Result<Self, Error> {
        let kind = match json {
            Value::Object(map) => match map.get("type") {
                None => return Ok(Self::Simple(parse_block_state(json)?)),
                Some(kind) => kind.as_str().ok_or_else(|| {
                    Error::new(format!(
                        "block state provider: type is not a string: {json}"
                    ))
                })?,
            },
            _ => return Ok(Self::Simple(parse_block_state(json)?)),
        };
        let kind = kind.strip_prefix("minecraft:").unwrap_or(kind);
        let provider = match kind {
            "simple" => Self::Simple(parse_block_state(field(json, "state")?)?),
            "weighted" => Self::Weighted(Arc::new(WeightedList::from_json(
                field(json, "entries")?,
                parse_block_state,
            )?)),
            "randomized_int" => Self::RandomizedInt(Arc::new(RandomizedIntProvider {
                source: parser.state_provider(field(json, "source")?)?,
                property: field(json, "property")?
                    .as_str()
                    .ok_or_else(|| Error::new("randomized_int: property is not a string"))?
                    .to_owned(),
                values: IntProvider::from_json(field(json, "values")?)?,
            })),
            "rotated" => Self::Rotated(Arc::new(RotatedProvider {
                state: parser.state_provider(field(json, "state")?)?,
                direction: json.get("direction").map(parse_direction).transpose()?,
            })),
            "noise" | "dual_noise" | "noise_threshold" => Self::parse_noise_kind(kind, json)?,
            "rule_based" => {
                let fallback = json
                    .get("fallback")
                    .map(|f| parser.state_provider(f))
                    .transpose()?;
                let rules = field(json, "rules")?
                    .as_array()
                    .ok_or_else(|| Error::new("rule_based: rules is not a list"))?
                    .iter()
                    .map(|rule| {
                        Ok((
                            parser.block_predicate(field(rule, "if_true")?)?,
                            parser.state_provider(field(rule, "then")?)?,
                        ))
                    })
                    .collect::<Result<_, Error>>()?;
                Self::RuleBased(Arc::new(RuleBasedProvider { fallback, rules }))
            }
            "copy_properties" => {
                Self::CopyProperties(Arc::new(parser.state_provider(field(json, "source")?)?))
            }
            "random_block" => {
                Self::RandomBlock(ordered_blocks(field(json, "blocks")?, parser)?.into())
            }
            other => {
                return Err(Error::new(format!(
                    "unknown block state provider type {other}"
                )));
            }
        };
        Ok(provider)
    }

    /// Parses a noise based provider (`noise`, `dual_noise`, `noise_threshold`; `kind` without
    /// namespace). They refer to no other provider, so no feature parser is needed: other
    /// systems (enchantment effects) use them too.
    pub fn parse_noise_kind(kind: &str, json: &Value) -> Result<Self, Error> {
        Ok(match kind {
            "noise" => Self::Noise(Arc::new(NoiseProvider::parse(json)?)),
            "dual_noise" => {
                let (variety_min, variety_max) = parse_int_range(field(json, "variety")?)?;
                if variety_min < 1 || variety_max > 64 {
                    return Err(Error::new(format!(
                        "dual_noise: variety [{variety_min}-{variety_max}] outside of [1-64]"
                    )));
                }
                Self::DualNoise(Arc::new(DualNoiseProvider {
                    variety_min,
                    variety_max,
                    slow_noise: create_noise(json, "slow_noise")?,
                    slow_scale: positive_float(json, "slow_scale")?,
                    base: NoiseProvider::parse(json)?,
                }))
            }
            "noise_threshold" => {
                let threshold = float(json, "threshold")?;
                let high_chance = float(json, "high_chance")?;
                if !(-1.0..=1.0).contains(&threshold) || !(0.0..=1.0).contains(&high_chance) {
                    return Err(Error::new(
                        "noise_threshold: threshold or high_chance out of range",
                    ));
                }
                Self::NoiseThreshold(Arc::new(NoiseThresholdProvider {
                    noise: create_noise(json, "noise")?,
                    scale: positive_float(json, "scale")?,
                    threshold,
                    high_chance,
                    default_state: parse_block_state(field(json, "default_state")?)?,
                    low_states: state_list(json, "low_states")?,
                    high_states: state_list(json, "high_states")?,
                }))
            }
            other => {
                return Err(Error::new(format!(
                    "{other} is not a noise based block state provider"
                )));
            }
        })
    }

    /// `getState` of a noise based provider, which reads no blocks; `None` for the other
    /// types.
    pub fn noise_state<R: RandomSource>(
        &self,
        random: &mut R,
        pos: BlockPos,
    ) -> Option<BlockState> {
        match self {
            Self::Noise(provider) => Some(provider.random_state_at(&provider.states, pos)),
            Self::DualNoise(provider) => Some(provider.get_state(pos)),
            Self::NoiseThreshold(provider) => Some(provider.get_state(random, pos)),
            _ => None,
        }
    }

    /// `getState(level, random, pos)`.
    pub fn get_state<R: RandomSource>(
        &self,
        level: &WorldGenRegion<'_>,
        random: &mut R,
        pos: BlockPos,
    ) -> BlockState {
        match self {
            Self::Simple(state) => *state,
            Self::Weighted(list) => *list.get_random(random),
            Self::RandomizedInt(provider) => provider.get_state(level, random, pos),
            Self::Rotated(provider) => provider.get_state(level, random, pos),
            Self::Noise(provider) => provider.random_state_at(&provider.states, pos),
            Self::DualNoise(provider) => provider.get_state(pos),
            Self::NoiseThreshold(provider) => provider.get_state(random, pos),
            Self::RuleBased(provider) => provider
                .get_optional_state(level, random, pos)
                .unwrap_or_else(|| level.block(pos)),
            Self::CopyProperties(source) => {
                with_properties_of(source.get_state(level, random, pos), level.block(pos))
            }
            Self::RandomBlock(blocks) => {
                random_element(blocks, random).unwrap_or_else(|| level.block(pos))
            }
        }
    }

    /// `getOptionalState(level, random, pos)`: `None` where a rule based provider has no rule
    /// (and no fallback) or a random block provider has no blocks.
    pub fn get_optional_state<R: RandomSource>(
        &self,
        level: &WorldGenRegion<'_>,
        random: &mut R,
        pos: BlockPos,
    ) -> Option<BlockState> {
        match self {
            Self::RuleBased(provider) => provider.get_optional_state(level, random, pos),
            Self::RandomBlock(blocks) => random_element(blocks, random),
            _ => Some(self.get_state(level, random, pos)),
        }
    }
}

impl RandomizedIntProvider {
    /// `RandomizedIntStateProvider.getState`: the value is only sampled when the state has an
    /// integer property of that name.
    ///
    /// Panics like Vanilla's `setValue` if the sample is not a value of the property.
    fn get_state<R: RandomSource>(
        &self,
        level: &WorldGenRegion<'_>,
        random: &mut R,
        pos: BlockPos,
    ) -> BlockState {
        let state = self.source.get_state(level, random, pos);
        let Some(property) =
            block_properties(state).find(|p| p.name == self.property && is_integer_property(p))
        else {
            return state;
        };
        let value = self.values.sample(random);
        property
            .values
            .iter()
            .find(|v| v.parse::<i32>() == Ok(value))
            .and_then(|v| state.with(property.name, v))
            .unwrap_or_else(|| {
                panic!(
                    "Cannot set property {} to {value} on {}, it is not an allowed value",
                    property.name,
                    state.block().name()
                )
            })
    }
}

impl RotatedProvider {
    /// `RotatedBlockProvider.getState`.
    fn get_state<R: RandomSource>(
        &self,
        level: &WorldGenRegion<'_>,
        random: &mut R,
        pos: BlockPos,
    ) -> BlockState {
        let direction = match self.direction {
            Some(direction) => direction,
            // `Direction.getRandom(random)`.
            None => Direction::ALL[random.next_int_bounded(6) as usize],
        };
        rotate(self.state.get_state(level, random, pos), direction)
    }
}

/// The state part of `RotatedBlockProvider.getState`: the axis, facing and horizontal
/// facing set towards `direction` where the block has those properties.
pub fn rotate(state: BlockState, direction: Direction) -> BlockState {
    let state = try_set_value(state, &AXIS, axis_name(direction));
    let state = try_set_value(state, &FACING, direction_name(direction));
    if matches!(direction, Direction::Up | Direction::Down) {
        state
    } else {
        try_set_value(state, &HORIZONTAL_FACING, direction_name(direction))
    }
}

/// The integer property `name` of the state (`RandomizedIntStateProvider`), if it has one.
pub fn integer_property(state: BlockState, name: &str) -> Option<&'static PropertyInfo> {
    block_properties(state).find(|p| p.name == name && is_integer_property(p))
}

impl NoiseProvider {
    /// The `noise`, `scale` and `states` fields (`NoiseProvider.noiseProviderCodec`).
    fn parse(json: &Value) -> Result<Self, Error> {
        Ok(Self {
            noise: create_noise(json, "noise")?,
            scale: positive_float(json, "scale")?,
            states: state_list(json, "states")?,
        })
    }

    /// `getRandomState(states, pos, scale)` with the provider's own scale.
    fn random_state_at(&self, states: &[BlockState], pos: BlockPos) -> BlockState {
        random_state(states, noise_value(&self.noise, pos, f64::from(self.scale)))
    }
}

impl DualNoiseProvider {
    /// `DualNoiseProvider.getState`.
    ///
    /// Vanilla fills a list with `localVariety` candidates and then picks one by the fast noise;
    /// the noises use no randomness, so only the picked candidate is computed.
    fn get_state(&self, pos: BlockPos) -> BlockState {
        let variety_noise = f64::from(self.slow_noise_value(pos));
        let local_variety = clamped_map(
            variety_noise,
            -1.0,
            1.0,
            f64::from(self.variety_min),
            f64::from(self.variety_max) + 1.0,
        ) as i32;
        let base = &self.base;
        let fast = noise_value(&base.noise, pos, f64::from(base.scale));
        let index = random_index(local_variety as usize, fast) as i32;
        let candidate_pos = pos.offset(index.wrapping_mul(54545), 0, index.wrapping_mul(34234));
        random_state(&base.states, self.slow_noise_value(candidate_pos))
    }

    /// `getSlowNoiseValue(pos)`: the coordinates are scaled in `float`.
    fn slow_noise_value(&self, pos: BlockPos) -> f32 {
        let scale = self.slow_scale;
        self.slow_noise.get(
            f64::from(pos.x as f32 * scale),
            f64::from(pos.y as f32 * scale),
            f64::from(pos.z as f32 * scale),
        )
    }
}

impl NoiseThresholdProvider {
    /// `NoiseThresholdProvider.getState`.
    fn get_state<R: RandomSource>(&self, random: &mut R, pos: BlockPos) -> BlockState {
        let local_value = f64::from(noise_value(&self.noise, pos, f64::from(self.scale)));
        if local_value < f64::from(self.threshold) {
            pick(&self.low_states, random)
        } else if random.next_float() < self.high_chance {
            pick(&self.high_states, random)
        } else {
            self.default_state
        }
    }
}

impl RuleBasedProvider {
    /// `RuleBasedStateProvider.getOptionalState`.
    fn get_optional_state<R: RandomSource>(
        &self,
        level: &WorldGenRegion<'_>,
        random: &mut R,
        pos: BlockPos,
    ) -> Option<BlockState> {
        for (if_true, then) in &self.rules {
            if if_true.test(level, pos)
                && let Some(state) = then.get_optional_state(level, random, pos)
            {
                return Some(state);
            }
        }
        self.fallback
            .as_ref()
            .and_then(|fallback| fallback.get_optional_state(level, random, pos))
    }
}

/// `NoiseBasedStateProvider.getNoiseValue(pos, scale)`.
fn noise_value(noise: &Noise, pos: BlockPos, scale: f64) -> f32 {
    noise.get(
        f64::from(pos.x) * scale,
        f64::from(pos.y) * scale,
        f64::from(pos.z) * scale,
    )
}

/// The index `NoiseProvider.getRandomState(states, noiseValue)` picks among `len` states.
fn random_index(len: usize, noise_value: f32) -> usize {
    let placement = clamp_f((1.0 + noise_value) / 2.0, 0.0, 0.9999);
    (placement * len as f32) as i32 as usize
}

/// `NoiseProvider.getRandomState(states, noiseValue)`.
fn random_state(states: &[BlockState], noise_value: f32) -> BlockState {
    states[random_index(states.len(), noise_value)]
}

/// `Util.getRandom(list, random)`.
fn pick<R: RandomSource>(states: &[BlockState], random: &mut R) -> BlockState {
    states[random.next_int_bounded(states.len() as i32) as usize]
}

/// `Util.getRandomSafe(list, random)`.
fn random_element<R: RandomSource>(states: &[BlockState], random: &mut R) -> Option<BlockState> {
    (!states.is_empty()).then(|| pick(states, random))
}

/// A Vanilla block state property by identity: its name and its possible values.
struct Property {
    name: &'static str,
    values: &'static [&'static str],
}

/// `BlockStateProperties.AXIS`.
const AXIS: Property = Property {
    name: "axis",
    values: &["x", "y", "z"],
};
/// `BlockStateProperties.FACING`.
const FACING: Property = Property {
    name: "facing",
    values: &["north", "east", "south", "west", "up", "down"],
};
/// `BlockStateProperties.HORIZONTAL_FACING`.
const HORIZONTAL_FACING: Property = Property {
    name: "facing",
    values: &["north", "south", "west", "east"],
};

/// The properties of the state's block.
fn block_properties(state: BlockState) -> impl Iterator<Item = &'static PropertyInfo> {
    state
        .block()
        .info()
        .properties
        .iter()
        .map(|&p| &PROPERTIES[usize::from(p)])
}

/// `Property.equals`: same name and the same possible values (properties with the same name
/// but other values, such as `HORIZONTAL_AXIS` and `AXIS`, are different properties).
fn same_property(info: &PropertyInfo, name: &str, values: &[&str]) -> bool {
    info.name == name
        && info.values.len() == values.len()
        && values.iter().all(|v| info.values.contains(v))
}

/// Whether a property is an `IntegerProperty` (all its values are integers).
fn is_integer_property(info: &PropertyInfo) -> bool {
    info.values.iter().all(|v| v.parse::<i32>().is_ok())
}

/// `state.trySetValue(property, value)`: unchanged if the block lacks that exact property.
fn try_set_value(state: BlockState, property: &Property, value: &str) -> BlockState {
    if block_properties(state).any(|p| same_property(p, property.name, property.values)) {
        state.with(property.name, value).unwrap_or(state)
    } else {
        state
    }
}

/// `state.withPropertiesOf(source)`: every property of `source`'s block that `state` has too
/// is copied.
pub fn with_properties_of(state: BlockState, source: BlockState) -> BlockState {
    let mut result = state;
    for property in block_properties(source) {
        if block_properties(result).any(|p| same_property(p, property.name, property.values))
            && let Some(value) = source.value(property.name)
        {
            result = result.with(property.name, value).unwrap_or(result);
        }
    }
    result
}

/// `Direction.getSerializedName()`.
const fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Down => "down",
        Direction::Up => "up",
        Direction::North => "north",
        Direction::South => "south",
        Direction::West => "west",
        Direction::East => "east",
    }
}

/// `direction.getAxis().getSerializedName()`.
const fn axis_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Down | Direction::Up => "y",
        Direction::North | Direction::South => "z",
        Direction::West | Direction::East => "x",
    }
}

fn field<'v>(json: &'v Value, name: &str) -> Result<&'v Value, Error> {
    json.get(name).ok_or_else(|| {
        Error::new(format!(
            "block state provider: missing field {name} in {json}"
        ))
    })
}

/// `Codec.FLOAT.fieldOf(name)`.
fn float(json: &Value, name: &str) -> Result<f32, Error> {
    let value = field(json, name)?;
    value
        .as_f64()
        .map(|v| v as f32)
        .ok_or_else(|| Error::new(format!("block state provider: {name} is not a number")))
}

/// `ExtraCodecs.POSITIVE_FLOAT.fieldOf(name)`.
fn positive_float(json: &Value, name: &str) -> Result<f32, Error> {
    let value = float(json, name)?;
    if value > 0.0 {
        Ok(value)
    } else {
        Err(Error::new(format!(
            "{name}: value must be positive: {value}"
        )))
    }
}

/// `ExtraCodecs.nonEmptyList(BlockState.CODEC.listOf()).fieldOf(name)`.
fn state_list(json: &Value, name: &str) -> Result<Vec<BlockState>, Error> {
    let states = field(json, name)?
        .as_array()
        .ok_or_else(|| Error::new(format!("block state provider: {name} is not a list")))?
        .iter()
        .map(parse_block_state)
        .collect::<Result<Vec<_>, _>>()?;
    if states.is_empty() {
        return Err(Error::new(format!("{name}: list must have contents")));
    }
    Ok(states)
}

/// `NormalNoise.DIRECT_CODEC.fieldOf(name)` instantiated as the noise providers do:
/// `parameters.create(new WorldgenRandom(new LegacyRandomSource(seed)))` with the `seed` field.
fn create_noise(json: &Value, name: &str) -> Result<Noise, Error> {
    let seed = field(json, "seed")?
        .as_i64()
        .ok_or_else(|| Error::new("block state provider: seed is not a long"))?;
    let parameters = NormalNoise::from_json(field(json, name)?)?;
    Ok(parameters.create(&mut AnyRandom::Legacy(LegacyRandom::new(seed))))
}

/// `InclusiveRange.codec(Codec.INT)`: `n`, `[min, max]` or
/// `{"min_inclusive": min, "max_inclusive": max}`.
fn parse_int_range(json: &Value) -> Result<(i32, i32), Error> {
    let int = |v: &Value| codec_int(v).ok_or_else(|| Error::new(format!("{v} is not an int")));
    let (min, max) = match json {
        Value::Array(items) if items.len() == 2 => (int(&items[0])?, int(&items[1])?),
        Value::Object(_) => (
            int(field(json, "min_inclusive")?)?,
            int(field(json, "max_inclusive")?)?,
        ),
        other => {
            let value = int(other)?;
            (value, value)
        }
    };
    if min > max {
        return Err(Error::new(
            "min_inclusive must be less than or equal to max_inclusive",
        ));
    }
    Ok((min, max))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use vanillite_data::block::BlockId;

    use super::*;

    #[test]
    fn noise_index_clamps_like_vanilla() {
        assert_eq!(random_index(8, -1.5), 0);
        assert_eq!(random_index(8, 0.0), 4);
        assert_eq!(random_index(8, 1.0), 7);
        assert_eq!(random_index(8, f32::NAN), 0);
    }

    #[test]
    fn inclusive_ranges() {
        assert_eq!(parse_int_range(&json!([1, 3])).unwrap(), (1, 3));
        assert_eq!(parse_int_range(&json!(2)).unwrap(), (2, 2));
        assert_eq!(
            parse_int_range(&json!({"min_inclusive": 1, "max_inclusive": 4})).unwrap(),
            (1, 4)
        );
        assert!(parse_int_range(&json!([3, 1])).is_err());
    }

    #[test]
    fn rotation_respects_property_identity() {
        let state = |name: &str| BlockId::by_name(name).unwrap().default_state();
        // `AXIS` applies to logs but not to the nether portal's `HORIZONTAL_AXIS`.
        let log = try_set_value(state("minecraft:oak_log"), &AXIS, "x");
        assert_eq!(log.value("axis"), Some("x"));
        let portal = state("minecraft:nether_portal");
        assert_eq!(try_set_value(portal, &AXIS, "z"), portal);
        // `FACING` does not apply to a hopper (down and horizontal only).
        let hopper = state("minecraft:hopper");
        assert_eq!(try_set_value(hopper, &FACING, "north"), hopper);
        let furnace = try_set_value(state("minecraft:furnace"), &HORIZONTAL_FACING, "west");
        assert_eq!(furnace.value("facing"), Some("west"));
    }
}
