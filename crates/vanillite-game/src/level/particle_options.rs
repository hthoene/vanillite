//! Particle options (`ParticleTypes.CODEC`): the options of a particle type read like the
//! types' map codecs (from NBT, the `/particle` argument and data pack JSON converted to NBT)
//! and written like their stream codecs.

use vanillite_data::block::BlockId;
use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::codec::{Encode, write_varint};
use vanillite_protocol::packets::play::clientbound::Particle;

/// Why particle options do not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionsError {
    /// The particle type is unknown.
    NotFound,
    /// The options do not fit the type (the codec's message).
    Invalid(String),
}

fn invalid(message: String) -> OptionsError {
    OptionsError::Invalid(message)
}

/// The options compound of a particle and its SNBT for error messages.
struct Options<'a> {
    map: &'a Compound,
}

impl Options<'_> {
    fn snbt(&self) -> String {
        vanillite_nbt::snbt::to_string(&Tag::Compound(self.map.clone()))
    }

    /// `fieldOf(key)`.
    fn get(&self, key: &str) -> Result<&Tag, OptionsError> {
        self.map
            .get(key)
            .ok_or_else(|| invalid(format!("No key {key} in MapLike[{}]", self.snbt())))
    }
}

fn number(tag: &Tag) -> Result<f64, OptionsError> {
    match *tag {
        Tag::Byte(v) => Ok(f64::from(v)),
        Tag::Short(v) => Ok(f64::from(v)),
        Tag::Int(v) => Ok(f64::from(v)),
        // `Number.doubleValue` of a long; precision loss is Java's too.
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        Tag::Long(v) => Ok(v as f64),
        Tag::Float(v) => Ok(f64::from(v)),
        Tag::Double(v) => Ok(v),
        _ => Err(invalid(format!(
            "Not a number: {}",
            vanillite_nbt::snbt::to_string(tag)
        ))),
    }
}

/// `Codec.FLOAT`.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn float(tag: &Tag) -> Result<f32, OptionsError> {
    number(tag).map(|v| v as f32)
}

/// `Codec.INT` (`Number.intValue`: Java's narrowing casts).
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn int(tag: &Tag) -> Result<i32, OptionsError> {
    match *tag {
        Tag::Byte(v) => Ok(i32::from(v)),
        Tag::Short(v) => Ok(i32::from(v)),
        Tag::Int(v) => Ok(v),
        Tag::Long(v) => Ok(v as i32),
        Tag::Float(v) => Ok(v as i32),
        Tag::Double(v) => Ok(v as i32),
        _ => Err(invalid(format!(
            "Not a number: {}",
            vanillite_nbt::snbt::to_string(tag)
        ))),
    }
}

/// `ExtraCodecs.POSITIVE_INT`.
fn positive_int(tag: &Tag) -> Result<i32, OptionsError> {
    let v = int(tag)?;
    if v > 0 {
        Ok(v)
    } else {
        Err(invalid(format!("Value must be positive: {v}")))
    }
}

/// A list of exactly `n` numbers (`Util.fixedSize`).
fn numbers(tag: &Tag, n: usize) -> Result<Vec<f64>, OptionsError> {
    let items: Vec<&Tag> = match tag {
        Tag::List(List(items)) => items.iter().collect(),
        _ => {
            return Err(invalid(format!(
                "Not a list: {}",
                vanillite_nbt::snbt::to_string(tag)
            )));
        }
    };
    if items.len() != n {
        return Err(invalid(format!(
            "Input is not a list of {n} elements: {}",
            vanillite_nbt::snbt::to_string(tag)
        )));
    }
    items.into_iter().map(number).collect()
}

/// `ARGB.as8BitChannel`: `floor(v * 255)` in the low byte.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn channel(v: f64) -> i32 {
    let v = (v as f32 * 255.0).floor();
    (v as i32) & 0xFF
}

/// `ARGB.colorFromFloat(alpha, red, green, blue)`.
fn color_from_floats(a: f64, r: f64, g: f64, b: f64) -> i32 {
    (channel(a) << 24) | (channel(r) << 16) | (channel(g) << 8) | channel(b)
}

/// `ExtraCodecs.RGB_COLOR_CODEC`: an int or `[r, g, b]`.
fn rgb(tag: &Tag) -> Result<i32, OptionsError> {
    if let Ok(v) = int(tag) {
        return Ok(v);
    }
    match numbers(tag, 3)?.as_slice() {
        [r, g, b] => Ok(color_from_floats(1.0, *r, *g, *b)),
        _ => Err(invalid("Not a color".to_owned())),
    }
}

/// `ExtraCodecs.ARGB_COLOR_CODEC`: an int or `[r, g, b, a]`.
fn argb(tag: &Tag) -> Result<i32, OptionsError> {
    if let Ok(v) = int(tag) {
        return Ok(v);
    }
    match numbers(tag, 4)?.as_slice() {
        [r, g, b, a] => Ok(color_from_floats(*a, *r, *g, *b)),
        _ => Err(invalid("Not a color".to_owned())),
    }
}

/// `ScalableParticleOptionsBase.SCALE`.
fn scale(tag: &Tag) -> Result<f32, OptionsError> {
    let v = float(tag)?;
    if (0.01..=4.0).contains(&v) {
        Ok(v)
    } else {
        Err(invalid(format!(
            "Value must be within range [0.01;4.0]: {v}"
        )))
    }
}

fn with_namespace(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// `BlockParticleOption.BLOCK_STATE_CODEC`: a state compound or a block id.
fn block_state(tag: &Tag) -> Result<u16, OptionsError> {
    let unknown = |name: &str| {
        invalid(format!(
            "Unknown registry key in ResourceKey[minecraft:root / minecraft:block]: {name}"
        ))
    };
    match tag {
        Tag::String(name) => BlockId::by_name(&with_namespace(name))
            .map(|b| b.default_state().0)
            .ok_or_else(|| unknown(name)),
        Tag::Compound(c) => {
            let name = c
                .get("Name")
                .and_then(Tag::as_str)
                .ok_or_else(|| invalid("No key Name in MapLike".to_owned()))?;
            if BlockId::by_name(&with_namespace(name)).is_none() {
                return Err(unknown(name));
            }
            let mut fixed = c.clone();
            fixed.insert("Name", Tag::String(with_namespace(name)));
            Ok(crate::level::block_entity::read_block_state(&fixed).0)
        }
        _ => Err(invalid(format!(
            "Not a block state: {}",
            vanillite_nbt::snbt::to_string(tag)
        ))),
    }
}

/// `ItemStackTemplate.CODEC` written by its stream codec. Component changes are not sent.
fn item(tag: &Tag, out: &mut Vec<u8>) -> Result<(), OptionsError> {
    let (id, count) = match tag {
        Tag::String(id) => (id.clone(), 1),
        Tag::Compound(c) => {
            let id = c
                .get("id")
                .and_then(Tag::as_str)
                .ok_or_else(|| invalid("No key id in MapLike".to_owned()))?;
            let count = match c.get("count") {
                Some(t) => int(t)?,
                None => 1,
            };
            if !(1..=99).contains(&count) {
                return Err(invalid(format!("Value {count} outside of range [1:99]")));
            }
            (id.to_owned(), count)
        }
        _ => {
            return Err(invalid(format!(
                "Not an item: {}",
                vanillite_nbt::snbt::to_string(tag)
            )));
        }
    };
    let item = vanillite_data::registry::ITEM
        .id(&with_namespace(&id))
        .ok_or_else(|| {
            invalid(format!(
                "Unknown registry key in ResourceKey[minecraft:root / minecraft:item]: {id}"
            ))
        })?;
    write_varint(out, i32::from(item));
    write_varint(out, count);
    // An empty `DataComponentPatch`: no added, no removed components.
    write_varint(out, 0);
    write_varint(out, 0);
    Ok(())
}

/// `BlockPos.asLong`.
fn block_pos_long(x: i32, y: i32, z: i32) -> i64 {
    ((i64::from(x) & 0x3FF_FFFF) << 38)
        | ((i64::from(z) & 0x3FF_FFFF) << 12)
        | (i64::from(y) & 0xFFF)
}

/// `VibrationParticleOption`: a block destination (entity sources are refused) and the
/// arrival time.
fn vibration(options: &Options<'_>, out: &mut Vec<u8>) -> Result<(), OptionsError> {
    let Tag::Compound(destination) = options.get("destination")? else {
        return Err(invalid("Not a map".to_owned()));
    };
    let kind = destination
        .get("type")
        .and_then(Tag::as_str)
        .map(with_namespace)
        .ok_or_else(|| invalid("No key type in MapLike".to_owned()))?;
    if kind == "minecraft:entity" {
        return Err(invalid(
            "Entity position sources are not allowed".to_owned(),
        ));
    }
    if kind != "minecraft:block" {
        return Err(invalid(format!(
            "Unknown registry key in ResourceKey[minecraft:root / minecraft:position_source_type]: {kind}"
        )));
    }
    let pos: Vec<i32> = match destination.get("pos") {
        Some(Tag::IntArray(v)) => v.clone(),
        Some(Tag::List(List(v))) => v.iter().map(int).collect::<Result<_, _>>()?,
        _ => return Err(invalid("No key pos in MapLike".to_owned())),
    };
    let [x, y, z] = pos.as_slice() else {
        return Err(invalid(format!("Input is not a list of 3 ints: {pos:?}")));
    };
    let arrival = int(options.get("arrival_in_ticks")?)?;
    write_varint(out, 0);
    block_pos_long(*x, *y, *z).encode(out);
    write_varint(out, arrival);
    Ok(())
}

/// `ParticleArgument.readParticle`: the type's map codec over the options (an empty compound
/// when none are given), written by its stream codec.
pub fn from_options(id: &str, given: Option<&Compound>) -> Result<Particle, OptionsError> {
    let kind = vanillite_data::registry::PARTICLE_TYPE
        .id(id)
        .ok_or(OptionsError::NotFound)?;
    let empty = Compound::new();
    let o = Options {
        map: given.unwrap_or(&empty),
    };
    let mut out = Vec::new();
    let path = id.strip_prefix("minecraft:").unwrap_or(id);
    match path {
        "block" | "block_marker" | "falling_dust" | "dust_pillar" | "block_crumble" => {
            write_varint(&mut out, i32::from(block_state(o.get("block_state")?)?));
        }
        "entity_effect" | "tinted_leaves" | "flash" => argb(o.get("color")?)?.encode(&mut out),
        "dust" => {
            rgb(o.get("color")?)?.encode(&mut out);
            scale(o.get("scale")?)?.encode(&mut out);
        }
        "dust_color_transition" => {
            rgb(o.get("from_color")?)?.encode(&mut out);
            rgb(o.get("to_color")?)?.encode(&mut out);
            scale(o.get("scale")?)?.encode(&mut out);
        }
        "effect" | "instant_effect" => {
            let color = o.map.get("color").map_or(Ok(-1), rgb)?;
            let power = o.map.get("power").map_or(Ok(1.0), float)?;
            color.encode(&mut out);
            power.encode(&mut out);
        }
        "dragon_breath" => o.map.get("power").map_or(Ok(1.0), float)?.encode(&mut out),
        "geyser" | "geyser_plume" => positive_int(o.get("water_blocks")?)?.encode(&mut out),
        "geyser_base" | "geyser_poof" => {
            positive_int(o.get("water_blocks")?)?.encode(&mut out);
            float(o.get("burst_impulse_base")?)?.encode(&mut out);
        }
        "sculk_charge" => float(o.get("roll")?)?.encode(&mut out),
        "shriek" => write_varint(&mut out, int(o.get("delay")?)?),
        "trail" => {
            for v in numbers(o.get("target")?, 3)? {
                v.encode(&mut out);
            }
            rgb(o.get("color")?)?.encode(&mut out);
            write_varint(&mut out, positive_int(o.get("duration")?)?);
        }
        "vibration" => vibration(&o, &mut out)?,
        "item" => item(o.get("item")?, &mut out)?,
        _ => {}
    }
    Ok(Particle {
        kind: i32::from(kind),
        options: out,
    })
}
