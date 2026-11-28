//! `/data` and `/tag` (Vanilla's `DataCommands` with its `DataAccessor`s, `TagCommand`), and
//! NBT as chat text (`TextComponentTagVisitor`).

use vanillite_command::error::CommandError;
use vanillite_command::nbt_path::NbtPath;
use vanillite_command::{Arg as ErrArg, ArgValue, Args};
use vanillite_game::commands::{DataOp, ModifyOp, ModifySource, TagOp};
use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::text::{Arg, Color, Component, NamedColor, java_double, java_float};

use super::data::{self, DataTarget};
use super::exec::format_list;
use super::source::CommandSource;
use super::tracker::Tracker;
use super::{Env, execute, nbt_ops, select};

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

fn error(key: &'static str, args: Vec<ErrArg>) -> CommandError {
    CommandError::new(key, args)
}

/// An error argument of a tag: its SNBT (`translatableEscape` of a `Tag`).
fn tag_arg(tag: &Tag) -> ErrArg {
    ErrArg::Str(vanillite_nbt::snbt::to_string(tag))
}

fn path_arg<'a>(args: &'a Args, name: &str) -> Result<&'a NbtPath, CommandError> {
    args.nbt_path(name).ok_or_else(|| missing(name))
}

/// A data accessor: where it reads and writes, and how its messages name it.
struct Accessor {
    target: DataTarget,
    kind: &'static str,
    name: Vec<Arg>,
}

impl Accessor {
    /// `ArgProvider.access`: a single entity, a loaded block position or a storage id.
    fn of(
        args: &Args,
        name: &str,
        source: &CommandSource,
        env: &Env<'_>,
    ) -> Result<Self, CommandError> {
        if let Some(ArgValue::Coordinates(_)) = args.get(name) {
            execute::loaded_block_pos(args, name, source, env)?;
        }
        let target = data::target(args, name, source, env)?;
        let (kind, name) = match &target {
            DataTarget::Entity { id, .. } => {
                let display = select::local(&env.level.entities, env.world, *id)
                    .map(|t| t.display())
                    .unwrap_or_else(Component::empty);
                ("entity", vec![Arg::from(display)])
            }
            DataTarget::Block(pos) => ("block", vec![pos.x.into(), pos.y.into(), pos.z.into()]),
            DataTarget::Storage(id) => ("storage", vec![Arg::from(Component::text(id.as_str()))]),
        };
        Ok(Accessor { target, kind, name })
    }

    fn read(&self, env: &Env<'_>) -> Result<Compound, CommandError> {
        data::read(&self.target, env)
    }

    fn write(&self, env: &mut Env<'_>, data: Compound) -> Result<(), CommandError> {
        data::write(&self.target, env, data)
    }

    /// `getModifiedSuccess`.
    fn modified(&self) -> Component {
        Component::translatable_args(
            format!("commands.data.{}.modified", self.kind),
            self.name.clone(),
        )
    }

    /// `getPrintSuccess(tag)`.
    fn query(&self, tag: &Tag) -> Component {
        let mut args = self.name.clone();
        args.push(Arg::from(pretty(tag)));
        Component::translatable_args(format!("commands.data.{}.query", self.kind), args)
    }

    /// `getPrintSuccess(path, scale, value)`.
    fn got(&self, path: &NbtPath, scale: f64, value: i32) -> Component {
        let mut args = vec![Arg::from(path.text.as_str())];
        args.extend(self.name.iter().cloned());
        args.push(Arg::from(format!("{scale:.2}")));
        args.push(Arg::Int(value));
        Component::translatable_args(format!("commands.data.{}.get", self.kind), args)
    }
}

/// `CompoundTag.merge`: compounds merge recursively, other values replace.
pub(crate) fn merge_compound(target: &mut Compound, other: &Compound) {
    for (key, value) in other.iter() {
        if let Tag::Compound(inner) = value
            && let Some(Tag::Compound(existing)) = target.get_mut(key)
        {
            merge_compound(existing, inner);
        } else {
            target.insert(key, value.clone());
        }
    }
}

/// `Mth.floor` of a double into an int (saturating).
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
fn floor_int(v: f64) -> i32 {
    v.floor() as i32
}

/// Runs a `data` subcommand.
pub(crate) fn run_data(
    op: DataOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let accessor = Accessor::of(args, "target", source, env)?;
    match op {
        DataOp::Merge => {
            let nbt = args.compound("nbt").ok_or_else(|| missing("nbt"))?;
            let old = accessor.read(env)?;
            if nbt_ops::depth_of(&Tag::Compound(nbt.clone()), 0) {
                return Err(error("arguments.nbtpath.too_deep", vec![]));
            }
            let mut result = old.clone();
            merge_compound(&mut result, nbt);
            if result == old {
                return Err(error("commands.data.merge.failed", vec![]));
            }
            accessor.write(env, result)?;
            source.send_success(env.global, accessor.modified(), true);
            Ok(1)
        }
        DataOp::Get => {
            let root = Tag::Compound(accessor.read(env)?);
            let Some(path) = args.nbt_path("path") else {
                source.send_success(env.global, accessor.query(&root), false);
                return Ok(1);
            };
            let tag = data::single_tag(path, &root)?;
            if let Some(scale) = args.double("scale") {
                let Some(value) = number(&tag) else {
                    return Err(data::not_numeric(path));
                };
                let result = floor_int(value * scale);
                source.send_success(env.global, accessor.got(path, *scale, result), false);
                return Ok(result);
            }
            let result = match &tag {
                Tag::Compound(c) => len_int(c.len()),
                Tag::String(s) => len_int(s.encode_utf16().count()),
                Tag::List(l) => len_int(l.0.len()),
                Tag::ByteArray(a) => len_int(a.len()),
                Tag::IntArray(a) => len_int(a.len()),
                Tag::LongArray(a) => len_int(a.len()),
                other => floor_int(number(other).unwrap_or(0.0)),
            };
            source.send_success(env.global, accessor.query(&tag), false);
            Ok(result)
        }
        DataOp::Remove => {
            let path = path_arg(args, "path")?;
            let mut root = Tag::Compound(accessor.read(env)?);
            let count = nbt_ops::remove(path, &mut root);
            if count == 0 {
                return Err(error("commands.data.merge.failed", vec![]));
            }
            if let Tag::Compound(c) = root {
                accessor.write(env, c)?;
            }
            source.send_success(env.global, accessor.modified(), true);
            Ok(len_int(count))
        }
        DataOp::Modify(op, from) => {
            let values = modify_source(from, args, source, env)?;
            let path = path_arg(args, "targetPath")?;
            let mut root = Tag::Compound(accessor.read(env)?);
            let result = match op {
                ModifyOp::Insert => {
                    let index = *args.int("index").ok_or_else(|| missing("index"))?;
                    insert(path, index, &mut root, &values)?
                }
                ModifyOp::Prepend => insert(path, 0, &mut root, &values)?,
                ModifyOp::Append => insert(path, -1, &mut root, &values)?,
                ModifyOp::Set => match values.last() {
                    Some(value) => nbt_ops::set(path, &mut root, value)?,
                    None => 0,
                },
                ModifyOp::Merge => merge_at(path, &mut root, &values)?,
            };
            if result == 0 {
                return Err(error("commands.data.merge.failed", vec![]));
            }
            if let Tag::Compound(c) = root {
                accessor.write(env, c)?;
            }
            source.send_success(env.global, accessor.modified(), true);
            Ok(len_int(result))
        }
    }
}

fn len_int(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// The value of a numeric tag.
fn number(tag: &Tag) -> Option<f64> {
    match tag {
        Tag::Byte(v) => Some(f64::from(*v)),
        Tag::Short(v) => Some(f64::from(*v)),
        Tag::Int(v) => Some(f64::from(*v)),
        Tag::Long(v) => format!("{v}").parse().ok(),
        Tag::Float(v) => Some(f64::from(*v)),
        Tag::Double(v) => Some(*v),
        _ => None,
    }
}

/// The source values of `data modify`.
fn modify_source(
    from: ModifySource,
    args: &Args,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Vec<Tag>, CommandError> {
    let read_source = || -> Result<Vec<Tag>, CommandError> {
        let accessor = Accessor::of(args, "source", source, env)?;
        let root = Tag::Compound(accessor.read(env)?);
        match args.nbt_path("sourcePath") {
            Some(path) => nbt_ops::get(path, &root),
            None => Ok(vec![root]),
        }
    };
    match from {
        ModifySource::From => read_source(),
        ModifySource::String => {
            let start = args.int("start").copied();
            let end = args.int("end").copied();
            read_source()?
                .iter()
                .map(|tag| {
                    let text = as_text(tag)?;
                    Ok(Tag::String(match start {
                        Some(start) => substring(&text, start, end)?,
                        None => text,
                    }))
                })
                .collect()
        }
        ModifySource::Value => Ok(vec![
            args.tag("value").ok_or_else(|| missing("value"))?.clone(),
        ]),
        // Context providers (number providers over a loot context) do not exist yet.
        ModifySource::Compute => Err(error("command.failed", vec![])),
    }
}

/// `DataCommands.getAsText`.
fn as_text(tag: &Tag) -> Result<String, CommandError> {
    match tag {
        Tag::String(s) => Ok(s.clone()),
        Tag::Byte(_)
        | Tag::Short(_)
        | Tag::Int(_)
        | Tag::Long(_)
        | Tag::Float(_)
        | Tag::Double(_) => Ok(vanillite_nbt::snbt::to_string(tag)),
        other => Err(error(
            "commands.data.modify.expected_value",
            vec![tag_arg(other)],
        )),
    }
}

/// `DataCommands.substring`: offsets in UTF-16 units, negative ones from the end.
fn substring(text: &str, start: i32, end: Option<i32>) -> Result<String, CommandError> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let len = i64::try_from(units.len()).unwrap_or(i64::MAX);
    let offset = |i: i32| {
        if i >= 0 {
            i64::from(i)
        } else {
            len.saturating_add(i64::from(i))
        }
    };
    let from = offset(start);
    let to = end.map_or(len, offset);
    let invalid = || {
        error(
            "commands.data.modify.invalid_substring",
            vec![
                ErrArg::Int(i32::try_from(from).unwrap_or(i32::MIN)),
                ErrArg::Int(i32::try_from(to).unwrap_or(i32::MIN)),
            ],
        )
    };
    if from < 0 || to > len || from > to {
        return Err(invalid());
    }
    let range = usize::try_from(from).map_err(|_| invalid())?
        ..usize::try_from(to).map_err(|_| invalid())?;
    let part = units.get(range).ok_or_else(invalid)?;
    Ok(String::from_utf16_lossy(part))
}

/// `NbtPath.insert`: the number of changed lists.
fn insert(
    path: &NbtPath,
    index: i32,
    root: &mut Tag,
    values: &[Tag],
) -> Result<usize, CommandError> {
    let depth = path.nodes.len();
    if values.iter().any(|v| nbt_ops::depth_of(v, depth)) {
        return Err(error("arguments.nbtpath.too_deep", vec![]));
    }
    nbt_ops::modify_or_create(path, root, &|| Tag::List(List(Vec::new())), &mut |target| {
        let len = match target {
            Tag::List(l) => l.0.len(),
            Tag::ByteArray(a) => a.len(),
            Tag::IntArray(a) => a.len(),
            Tag::LongArray(a) => a.len(),
            other => {
                return Err(error(
                    "commands.data.modify.expected_list",
                    vec![tag_arg(other)],
                ));
            }
        };
        let len = i64::try_from(len).unwrap_or(i64::MAX);
        let mut at = if index < 0 {
            len.saturating_add(i64::from(index)).saturating_add(1)
        } else {
            i64::from(index)
        };
        let mut modified = false;
        for value in values {
            let added = usize::try_from(at)
                .ok()
                .and_then(|i| nbt_ops::insert_element(target, i, value));
            match added {
                Some(true) => {
                    at = at.saturating_add(1);
                    modified = true;
                }
                Some(false) => {}
                None => {
                    return Err(error(
                        "commands.data.modify.invalid_index",
                        vec![ErrArg::Int(i32::try_from(at).unwrap_or(i32::MIN))],
                    ));
                }
            }
        }
        Ok(usize::from(modified))
    })
}

/// `data modify ... merge`: the number of changed compounds.
fn merge_at(path: &NbtPath, root: &mut Tag, values: &[Tag]) -> Result<usize, CommandError> {
    let mut combined = Compound::new();
    for value in values {
        if nbt_ops::depth_of(value, 0) {
            return Err(error("arguments.nbtpath.too_deep", vec![]));
        }
        let Tag::Compound(c) = value else {
            return Err(error(
                "commands.data.modify.expected_object",
                vec![tag_arg(value)],
            ));
        };
        merge_compound(&mut combined, c);
    }
    nbt_ops::modify_or_create(
        path,
        root,
        &|| Tag::Compound(Compound::new()),
        &mut |target| {
            let Tag::Compound(object) = target else {
                return Err(error(
                    "commands.data.modify.expected_object",
                    vec![tag_arg(target)],
                ));
            };
            let original = object.clone();
            merge_compound(object, &combined);
            Ok(usize::from(original != *object))
        },
    )
}

/// Runs a `tag` subcommand. Players in other worlds are changed there and count as changed;
/// their tags are not listed.
pub(crate) fn run_tag(
    op: TagOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let selector = args.selector("targets").ok_or_else(|| missing("targets"))?;
    let targets = select::many(env, source, selector)?;
    let mut tracker = Tracker::<Component>::default();
    match op {
        TagOp::Add | TagOp::Remove => {
            let name = args.string("name").ok_or_else(|| missing("name"))?.clone();
            let add = op == TagOp::Add;
            for target in &targets {
                let changed = match target.entity {
                    Some(id) if add => env.level.entities.add_tag(id, &name),
                    Some(id) => env.level.entities.remove_tag(id, &name),
                    None => {
                        let (uuid, name) = (target.uuid, name.clone());
                        env.global.run_in_world(target.world, move |world| {
                            let entities = world.entities_mut();
                            if let Some(id) = select::find_uuid(entities, uuid) {
                                if add {
                                    entities.add_tag(id, &name);
                                } else {
                                    entities.remove_tag(id, &name);
                                }
                            }
                        });
                        true
                    }
                };
                tracker.track(&target.display(), i32::from(changed));
            }
            let key = if add { "add" } else { "remove" };
            tracker.feedback(
                source,
                env,
                true,
                false,
                Some(error(
                    if add {
                        "commands.tag.add.failed"
                    } else {
                        "commands.tag.remove.failed"
                    },
                    vec![],
                )),
                |one, n, _| match one {
                    Some(display) => Component::translatable_args(
                        format!("commands.tag.{key}.success.single"),
                        vec![Arg::from(name.as_str()), Arg::from(display.clone())],
                    ),
                    None => Component::translatable_args(
                        format!("commands.tag.{key}.success.multiple"),
                        vec![Arg::from(name.as_str()), Arg::Int(n)],
                    ),
                },
            )
        }
        TagOp::List => {
            let mut all: Vec<String> = Vec::new();
            for target in &targets {
                let tags = target
                    .entity
                    .map_or(&[][..], |id| env.level.entities.tags_of(id));
                for tag in tags {
                    if !all.contains(tag) {
                        all.push(tag.clone());
                    }
                }
                tracker.track(&target.display(), len_int(tags.len()));
            }
            if all.is_empty() {
                tracker.feedback(source, env, false, false, None, |one, n, _| match one {
                    Some(display) => Component::translatable(
                        "commands.tag.list.single.empty",
                        vec![display.clone()],
                    ),
                    None => Component::translatable_args(
                        "commands.tag.list.multiple.empty",
                        vec![Arg::Int(n)],
                    ),
                })?;
            } else {
                all.sort();
                let count = len_int(all.len());
                let list = format_list(
                    all.iter()
                        .map(|t| Component::text(t.as_str()).color(Color::Named(NamedColor::Green)))
                        .collect(),
                );
                tracker.feedback(source, env, false, false, None, |one, n, _| match one {
                    Some(display) => Component::translatable_args(
                        "commands.tag.list.single.success",
                        vec![Arg::from(display.clone()), Arg::Int(count), Arg::from(list)],
                    ),
                    None => Component::translatable_args(
                        "commands.tag.list.multiple.success",
                        vec![Arg::Int(n), Arg::Int(count), Arg::from(list)],
                    ),
                })?;
            }
            Ok(len_int(all.len()))
        }
    }
}

// --- NBT as text ------------------------------------------------------------------------------

/// `StringTag.quoteAndEscape`.
fn quote_and_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len().saturating_add(2));
    let mut quote = None;
    for c in input.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' | '\'' => {
                let q = *quote.get_or_insert(if c == '"' { '\'' } else { '"' });
                if q == c {
                    out.push('\\');
                }
                out.push(c);
            }
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if c < ' ' => out.push_str(&format!("\\x{:02X}", u32::from(c))),
            c => out.push(c),
        }
    }
    let q = quote.unwrap_or('"');
    format!("{q}{out}{q}")
}

/// `TextComponentTagVisitor` without indentation and with `RichStyling`.
struct Pretty {
    parts: Vec<Component>,
    depth: usize,
}

const KEY: Color = Color::Named(NamedColor::Aqua);
const STRING: Color = Color::Named(NamedColor::Green);
const NUMBER: Color = Color::Named(NamedColor::Gold);
const NUMBER_TYPE: Color = Color::Named(NamedColor::Red);
/// Elements of arrays and wrapped lists shown at most.
const MAX_LENGTH: usize = 128;
/// Nesting shown at most.
const MAX_DEPTH: usize = 64;

impl Pretty {
    fn plain(&mut self, text: &str) {
        self.parts.push(Component::text(text));
    }

    fn styled(&mut self, text: &str, color: Color) {
        self.parts.push(Component::text(text).color(color));
    }

    fn folded(&mut self) {
        self.styled("<...>", Color::Named(NamedColor::Gray));
    }

    fn number(&mut self, value: String, suffix: Option<&str>) {
        self.styled(&value, NUMBER);
        if let Some(suffix) = suffix {
            self.styled(suffix, NUMBER_TYPE);
        }
    }

    fn array<T: ToString>(&mut self, prefix: &str, values: &[T], suffix: Option<&str>) {
        self.plain("[");
        self.styled(prefix, NUMBER_TYPE);
        self.plain(";");
        for (i, v) in values.iter().take(MAX_LENGTH).enumerate() {
            self.plain(" ");
            self.number(v.to_string(), suffix);
            if i.saturating_add(1) != values.len() {
                self.plain(",");
            }
        }
        if values.len() > MAX_LENGTH {
            self.folded();
        }
        self.plain("]");
    }

    fn sub(&mut self, tag: &Tag) {
        self.depth = self.depth.saturating_add(1);
        self.visit(tag);
        self.depth = self.depth.saturating_sub(1);
    }

    fn visit(&mut self, tag: &Tag) {
        match tag {
            Tag::String(s) => {
                let quoted = quote_and_escape(s);
                let mut chars = quoted.chars();
                let q = chars.next().unwrap_or('"').to_string();
                chars.next_back();
                self.plain(&q);
                self.styled(chars.as_str(), STRING);
                self.plain(&q);
            }
            Tag::Byte(v) => self.number(v.to_string(), Some("b")),
            Tag::Short(v) => self.number(v.to_string(), Some("s")),
            Tag::Int(v) => self.number(v.to_string(), None),
            Tag::Long(v) => self.number(v.to_string(), Some("L")),
            Tag::Float(v) => self.number(java_float(*v), Some("f")),
            Tag::Double(v) => self.number(java_double(*v), Some("d")),
            Tag::ByteArray(a) => self.array("B", a, Some("b")),
            Tag::IntArray(a) => self.array("I", a, None),
            Tag::LongArray(a) => self.array("L", a, Some("L")),
            Tag::List(l) => self.list(&l.0),
            Tag::Compound(c) => self.compound(c),
        }
    }

    fn list(&mut self, list: &[Tag]) {
        if list.is_empty() {
            self.plain("[");
            self.plain("]");
        } else if self.depth >= MAX_DEPTH {
            self.plain("[");
            self.folded();
            self.plain("]");
        } else if list.len() >= 8 || list.iter().all(|t| number(t).is_some()) {
            self.plain("[");
            for (i, tag) in list.iter().enumerate() {
                if i != 0 {
                    self.plain(",");
                    self.plain(" ");
                }
                self.sub(tag);
            }
            self.plain("]");
        } else {
            self.plain("[");
            for (i, tag) in list.iter().take(MAX_LENGTH).enumerate() {
                self.plain("");
                self.sub(tag);
                if i.saturating_add(1) != list.len() {
                    self.plain(",");
                    self.plain(" ");
                }
            }
            if list.len() > MAX_LENGTH {
                self.plain("");
                self.folded();
            }
            self.plain("]");
        }
    }

    fn compound(&mut self, compound: &Compound) {
        if compound.is_empty() {
            self.plain("{");
            self.plain("}");
            return;
        }
        if self.depth >= MAX_DEPTH {
            self.plain("{");
            self.folded();
            self.plain("}");
            return;
        }
        self.plain("{");
        let ordered = compound.clone().in_java_hash_order();
        let count = ordered.len();
        for (i, (key, value)) in ordered.iter().enumerate() {
            self.plain("");
            let simple = !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
            if simple {
                self.styled(key, KEY);
            } else {
                let quoted = quote_and_escape(key);
                let mut chars = quoted.chars();
                let q = chars.next().unwrap_or('"').to_string();
                chars.next_back();
                self.parts.push(
                    Component::text(q.as_str())
                        .append(Component::text(chars.as_str()).color(KEY))
                        .append(Component::text(q.as_str())),
                );
            }
            self.plain(":");
            self.plain(" ");
            self.sub(value);
            if i.saturating_add(1) != count {
                self.plain(",");
                self.plain(" ");
            }
        }
        self.plain("}");
    }
}

/// `NbtUtils.toPrettyComponent`.
pub(crate) fn pretty(tag: &Tag) -> Component {
    let mut p = Pretty {
        parts: Vec::new(),
        depth: 0,
    };
    p.visit(tag);
    let mut out = Component::empty();
    out.extra = p.parts;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_number_forms() {
        assert_eq!(java_double(1.0), "1.0");
        assert_eq!(java_double(1.0e7), "1.0E7");
        assert_eq!(java_double(1.5e-5), "1.5E-5");
        assert_eq!(java_float(0.1), "0.1");
    }

    #[test]
    fn substrings_count_from_the_end() {
        assert_eq!(substring("hello", 1, Some(-1)).ok().as_deref(), Some("ell"));
        assert_eq!(substring("hello", -2, None).ok().as_deref(), Some("lo"));
        assert!(substring("hello", 3, Some(2)).is_err());
    }

    #[test]
    fn quotes_like_vanilla() {
        assert_eq!(quote_and_escape("a\"b"), "'a\"b'");
        assert_eq!(quote_and_escape("it's"), "\"it's\"");
        assert_eq!(quote_and_escape("x"), "\"x\"");
    }
}
