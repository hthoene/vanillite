//! Functions of the data packs (`ServerFunctionLibrary`, `CommandFunction`, `MacroFunction`):
//! loaded and parsed once on start and `/reload`, macro lines parsed per argument set with a
//! small cache, the `minecraft:tick` and `minecraft:load` tags run by the game loop.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, Args, FunctionRef, GameValue};
use vanillite_datapack::OpenedPack;
use vanillite_game::commands::CommandId;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::text::{Arg, Component};

use super::engine::{Callback, Engine, Line};
use super::source::{CommandSource, SourceKind};
use super::{Env, error_message};
use crate::global::Global;
use crate::world::WorldId;

/// Longest function line after joining continuations (`checkCommandLineLength`).
const MAX_LINE: usize = 2_000_000;
/// Instantiations a macro function keeps (`MAX_CACHE_ENTRIES`).
const MACRO_CACHE: usize = 8;

/// A function ready to run: its parsed lines.
pub(crate) struct Instantiated {
    /// The lines.
    pub(crate) lines: Vec<Line>,
}

enum MacroEntry {
    Plain(Line),
    /// Text segments around the variables; `vars` index the function's parameters.
    Template {
        segments: Vec<String>,
        vars: Vec<usize>,
    },
}

struct Macro {
    parameters: Vec<String>,
    entries: Vec<MacroEntry>,
    cache: Mutex<VecDeque<(Vec<String>, Arc<Instantiated>)>>,
}

enum Body {
    Plain(Arc<Instantiated>),
    Macro(Macro),
}

/// A loaded function.
pub(crate) struct Function {
    /// Its identifier.
    pub(crate) id: Arc<str>,
    body: Body,
}

impl Function {
    /// Whether it is a `MacroFunction`.
    pub(crate) fn is_macro(&self) -> bool {
        matches!(self.body, Body::Macro(_))
    }

    /// `CommandFunction.instantiate`; the error is the reason (`FunctionInstantiationException`).
    pub(crate) fn instantiate(
        &self,
        arguments: Option<&Compound>,
        global: &Global,
    ) -> Result<Arc<Instantiated>, Box<Component>> {
        let m = match &self.body {
            Body::Plain(i) => return Ok(Arc::clone(i)),
            Body::Macro(m) => m,
        };
        let Some(arguments) = arguments else {
            return Err(Box::new(Component::translatable_args(
                "commands.function.error.missing_arguments",
                vec![Arg::String(self.id.to_string())],
            )));
        };
        let mut values = Vec::with_capacity(m.parameters.len());
        for p in &m.parameters {
            let Some(tag) = arguments.get(p) else {
                return Err(Box::new(Component::translatable_args(
                    "commands.function.error.missing_argument",
                    vec![Arg::String(self.id.to_string()), Arg::String(p.clone())],
                )));
            };
            values.push(stringify(tag));
        }
        {
            let mut cache = m.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(i) = cache.iter().position(|(k, _)| *k == values)
                && let Some(entry) = cache.remove(i)
            {
                let found = Arc::clone(&entry.1);
                cache.push_back(entry);
                return Ok(found);
            }
        }
        let source = compile_source(global);
        let mut lines = Vec::with_capacity(m.entries.len());
        for entry in &m.entries {
            match entry {
                MacroEntry::Plain(line) => lines.push(line.clone()),
                MacroEntry::Template { segments, vars } => {
                    let mut command = String::new();
                    for (i, var) in vars.iter().enumerate() {
                        command.push_str(segments.get(i).map_or("", String::as_str));
                        command.push_str(values.get(*var).map_or("", String::as_str));
                        if command.len() > MAX_LINE {
                            return Err(Box::new(Component::text("Command too long")));
                        }
                    }
                    if segments.len() > vars.len()
                        && let Some(last) = segments.last()
                    {
                        command.push_str(last);
                    }
                    let parsed = source
                        .as_ref()
                        .ok_or_else(|| Box::new(Component::text("server stopped")))
                        .and_then(|s| {
                            global.commands().parse_command(&command, s).map_err(|e| {
                                Box::new(Component::translatable_args(
                                    "commands.function.error.parse",
                                    vec![
                                        Arg::String(self.id.to_string()),
                                        Arg::String(command.clone()),
                                        Arg::from(error_message(&e)),
                                    ],
                                ))
                            })
                        })?;
                    lines.push(Line {
                        parsed: Arc::new(parsed),
                        signed: None,
                    });
                }
            }
        }
        let instantiated = Arc::new(Instantiated { lines });
        let mut cache = m.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= MACRO_CACHE {
            cache.pop_front();
        }
        cache.push_back((values, Arc::clone(&instantiated)));
        Ok(instantiated)
    }
}

/// `MacroFunction.stringify`.
fn stringify(tag: &Tag) -> String {
    match tag {
        Tag::Float(v) => format_decimal(f64::from(*v)),
        Tag::Double(v) => format_decimal(*v),
        Tag::Byte(v) => v.to_string(),
        Tag::Short(v) => v.to_string(),
        Tag::Long(v) => v.to_string(),
        Tag::Int(v) => v.to_string(),
        Tag::String(s) => s.clone(),
        other => vanillite_nbt::snbt::to_string(other),
    }
}

/// `DecimalFormat("#")` with up to 15 fraction digits, rounding half-even.
fn format_decimal(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "∞".into() } else { "-∞".into() };
    }
    let s = format!("{v:.15}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    match s {
        "-0" => "-0".into(),
        "" => "0".into(),
        other => other.to_owned(),
    }
}

/// The loaded functions and function tags.
#[derive(Default)]
pub(crate) struct FunctionLibrary {
    functions: BTreeMap<String, Arc<Function>>,
    tags: BTreeMap<String, Vec<Arc<Function>>>,
}

impl FunctionLibrary {
    /// A function.
    pub(crate) fn get(&self, id: &str) -> Option<&Arc<Function>> {
        self.functions.get(id)
    }

    /// The functions of a tag.
    pub(crate) fn tag(&self, id: &str) -> Option<&Vec<Arc<Function>>> {
        self.tags.get(id)
    }

    /// Function names.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.functions.keys().map(String::as_str)
    }

    /// Tag names.
    pub(crate) fn tag_names(&self) -> impl Iterator<Item = &str> {
        self.tags.keys().map(String::as_str)
    }
}

/// The source functions are parsed with (`createCompilationContext`).
fn compile_source(global: &Global) -> Option<CommandSource> {
    Some(CommandSource::new(
        global.arc()?,
        SourceKind::Server,
        global.systems.function_level,
        WorldId(0),
        vanillite_base::Vec3::ZERO,
        (0.0, 0.0),
        None,
    ))
}

fn walk(dir: &Path, prefix: &str, ext: &str, out: &mut BTreeMap<String, PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if path.is_dir() {
            walk(&path, &format!("{prefix}{name}/"), ext, out);
        } else if let Some(stem) = name.strip_suffix(ext) {
            out.insert(format!("{prefix}{stem}"), path);
        }
    }
}

/// `<namespace>:<path>` → file for `data/<namespace>/<dir>/**/*<ext>` of every root; later
/// roots override earlier ones.
pub(crate) fn list(roots: &[PathBuf], dir: &str, ext: &str) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    for root in roots {
        let Ok(namespaces) = std::fs::read_dir(root.join("data")) else {
            continue;
        };
        for ns in namespaces.filter_map(Result::ok) {
            let Some(namespace) = ns.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let mut found = BTreeMap::new();
            walk(&ns.path().join(dir), "", ext, &mut found);
            for (path, file) in found {
                out.insert(format!("{namespace}:{path}"), file);
            }
        }
    }
    out
}

/// Loads every function and function tag of the selected data packs in load order
/// (`ServerFunctionLibrary.reload`).
pub(crate) fn load(global: &Global, packs: &[OpenedPack]) -> FunctionLibrary {
    let Some(source) = compile_source(global) else {
        return FunctionLibrary::default();
    };
    let roots: Vec<PathBuf> = packs.iter().flat_map(|p| p.layers.clone()).collect();
    let mut functions = BTreeMap::new();
    for (id, file) in list(&roots, "function", ".mcfunction") {
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) => {
                log::error!("Failed to load function {id}: {e}");
                continue;
            }
        };
        match compile(&id, &text, global, &source) {
            Ok(f) => {
                functions.insert(id, Arc::new(f));
            }
            Err(e) => log::error!("Failed to load function {id}: {e}"),
        }
    }
    let tags = load_tags(packs, &functions);
    log::info!("Loaded {} functions", functions.len());
    FunctionLibrary { functions, tags }
}

/// `CommandFunction.fromLines`.
fn compile(
    id: &str,
    text: &str,
    global: &Global,
    source: &CommandSource,
) -> Result<Function, String> {
    let raw: Vec<&str> = text.lines().collect();
    let mut plain: Vec<Line> = Vec::new();
    let mut macro_entries: Option<Vec<MacroEntry>> = None;
    let mut parameters: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        let number = i.saturating_add(1);
        let mut line = raw.get(i).map_or("", |l| l.trim()).to_owned();
        while line.ends_with('\\') {
            i = i.saturating_add(1);
            let Some(next) = raw.get(i) else {
                return Err("Line continuation at end of file".into());
            };
            line.pop();
            line.push_str(next.trim());
            if line.len() > MAX_LINE {
                return Err(format!("Command too long: {} characters", line.len()));
            }
        }
        i = i.saturating_add(1);
        if line.len() > MAX_LINE {
            return Err(format!("Command too long: {} characters", line.len()));
        }
        let Some(first) = line.chars().next() else {
            continue;
        };
        match first {
            '#' => continue,
            '/' => {
                if line.get(1..2) == Some("/") {
                    return Err(format!(
                        "Unknown or invalid command '{line}' on line {number} (if you intended to make a comment, use '#' not '//')"
                    ));
                }
                let name: String = line
                    .get(1..)
                    .unwrap_or("")
                    .chars()
                    .take_while(|c| vanillite_command::reader::is_allowed_in_unquoted_string(*c))
                    .collect();
                return Err(format!(
                    "Unknown or invalid command '{line}' on line {number} (did you mean '{name}'? Do not use a preceding forwards slash.)"
                ));
            }
            '$' => {
                let command = line.get(1..).unwrap_or("");
                let (segments, vars) = template(command)
                    .map_err(|e| format!("Can't parse function line {number}: '{command}': {e}"))?;
                let entries = macro_entries
                    .get_or_insert_with(|| plain.drain(..).map(MacroEntry::Plain).collect());
                let vars = vars
                    .into_iter()
                    .map(|v| match parameters.iter().position(|p| *p == v) {
                        Some(i) => i,
                        None => {
                            parameters.push(v);
                            parameters.len().saturating_sub(1)
                        }
                    })
                    .collect();
                entries.push(MacroEntry::Template { segments, vars });
            }
            _ => {
                let parsed = global
                    .commands()
                    .parse_command(&line, source)
                    .map_err(|e| {
                        format!(
                            "Whilst parsing command on line {number}: {}",
                            error_text(&e, &line)
                        )
                    })?;
                let entry = Line {
                    parsed: Arc::new(parsed),
                    signed: None,
                };
                match &mut macro_entries {
                    Some(entries) => entries.push(MacroEntry::Plain(entry)),
                    None => plain.push(entry),
                }
            }
        }
    }
    let id: Arc<str> = id.into();
    let body = match macro_entries {
        Some(entries) => Body::Macro(Macro {
            parameters,
            entries,
            cache: Mutex::new(VecDeque::new()),
        }),
        None => Body::Plain(Arc::new(Instantiated { lines: plain })),
    };
    Ok(Function { id, body })
}

/// `CommandSyntaxException.getMessage`: the message and the context.
fn error_text(e: &CommandError, _line: &str) -> String {
    let mut text = error_message(e).plain();
    if let (Some(c), Some(cursor)) = (e.context(), e.cursor) {
        text.push_str(&format!(
            " at position {cursor}: {}{}<--[HERE]",
            if c.ellipsis { "..." } else { "" },
            c.before
        ));
    }
    text
}

/// `StringTemplate.fromString`.
fn template(input: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let mut segments = Vec::new();
    let mut vars = Vec::new();
    let mut start = 0usize;
    let mut search = 0usize;
    while let Some(found) = input.get(search..).and_then(|s| s.find('$')) {
        let index = search.saturating_add(found);
        if input.get(index.saturating_add(1)..index.saturating_add(2)) == Some("(") {
            segments.push(input.get(start..index).unwrap_or("").to_owned());
            let open = index.saturating_add(2);
            let Some(end) = input.get(open..).and_then(|s| s.find(')')) else {
                return Err("Unterminated macro variable".into());
            };
            let end = open.saturating_add(end);
            let variable = input.get(open..end).unwrap_or("");
            if !variable.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Err(format!("Invalid macro variable name '{variable}'"));
            }
            vars.push(variable.to_owned());
            start = end.saturating_add(1);
            search = start;
        } else {
            search = index.saturating_add(1);
        }
    }
    if start == 0 {
        return Err("No variables in macro".into());
    }
    if start != input.len() {
        segments.push(input.get(start..).unwrap_or("").to_owned());
    }
    Ok((segments, vars))
}

/// One value of a tag file.
enum TagValue {
    Id { id: String, required: bool },
    Tag { id: String, required: bool },
}

/// `TagLoader`: the function tags of all packs, resolved; within a pack the top overlay's file
/// wins.
fn load_tags(
    packs: &[OpenedPack],
    functions: &BTreeMap<String, Arc<Function>>,
) -> BTreeMap<String, Vec<Arc<Function>>> {
    let mut raw: BTreeMap<String, Vec<TagValue>> = BTreeMap::new();
    for pack in packs {
        let files = list(&pack.layers, "tags/function", ".json");
        for (id, file) in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                log::error!("Couldn't read tag list {id} from {}", file.display());
                continue;
            };
            let entry = raw.entry(id).or_default();
            if json.get("replace").and_then(serde_json::Value::as_bool) == Some(true) {
                entry.clear();
            }
            for value in json
                .get("values")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                let (text, required) = match value {
                    serde_json::Value::String(s) => (s.clone(), true),
                    serde_json::Value::Object(o) => (
                        o.get("id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        o.get("required")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(true),
                    ),
                    _ => continue,
                };
                entry.push(match text.strip_prefix('#') {
                    Some(t) => TagValue::Tag {
                        id: normalize(t),
                        required,
                    },
                    None => TagValue::Id {
                        id: normalize(&text),
                        required,
                    },
                });
            }
        }
    }
    let mut resolved: BTreeMap<String, Option<Vec<Arc<Function>>>> = BTreeMap::new();
    let names: Vec<String> = raw.keys().cloned().collect();
    for name in names {
        resolve_tag(&name, &raw, functions, &mut resolved, &mut Vec::new());
    }
    resolved
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect()
}

fn normalize(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

fn resolve_tag(
    name: &str,
    raw: &BTreeMap<String, Vec<TagValue>>,
    functions: &BTreeMap<String, Arc<Function>>,
    resolved: &mut BTreeMap<String, Option<Vec<Arc<Function>>>>,
    visiting: &mut Vec<String>,
) -> Option<Vec<Arc<Function>>> {
    if let Some(done) = resolved.get(name) {
        return done.clone();
    }
    if visiting.iter().any(|v| v == name) {
        log::error!("Tag cycle at {name}");
        return None;
    }
    let values = raw.get(name)?;
    visiting.push(name.to_owned());
    let mut out: Vec<Arc<Function>> = Vec::new();
    let mut missing = Vec::new();
    let add = |f: &Arc<Function>, out: &mut Vec<Arc<Function>>| {
        if !out.iter().any(|o| Arc::ptr_eq(o, f)) {
            out.push(Arc::clone(f));
        }
    };
    for value in values {
        match value {
            TagValue::Id { id, required } => match functions.get(id) {
                Some(f) => add(f, &mut out),
                None if *required => missing.push(id.clone()),
                None => {}
            },
            TagValue::Tag { id, required } => {
                match resolve_tag(id, raw, functions, resolved, visiting) {
                    Some(fs) => fs.iter().for_each(|f| add(f, &mut out)),
                    None if *required => missing.push(format!("#{id}")),
                    None => {}
                }
            }
        }
    }
    visiting.pop();
    let result = if missing.is_empty() {
        Some(out)
    } else {
        log::error!(
            "Couldn't load tag {name} as it is missing following references: {}",
            missing.join(", ")
        );
        None
    };
    resolved.insert(name.to_owned(), result.clone());
    result
}

/// The text of a function argument for messages.
pub(crate) fn name_arg(args: &Args, name: &str) -> String {
    match args.game(name) {
        Some(GameValue::Function(FunctionRef { tag: true, id })) => format!("#{id}"),
        Some(GameValue::Function(FunctionRef { id, .. })) => id.clone(),
        _ => String::new(),
    }
}

/// `FunctionArgument.getFunctions`: the function or the functions of the tag.
pub(crate) fn resolve(
    global: &Global,
    args: &Args,
    name: &str,
) -> Result<Vec<Arc<Function>>, CommandError> {
    let Some(GameValue::Function(f)) = args.game(name) else {
        return Err(CommandError::new("arguments.function.unknown", vec![]));
    };
    let library = global.systems.functions();
    if f.tag {
        // `ServerFunctionLibrary.getTag`: an unknown tag is empty.
        Ok(library.tag(&f.id).cloned().unwrap_or_default())
    } else {
        library
            .get(&f.id)
            .map(|f| vec![Arc::clone(f)])
            .ok_or_else(|| {
                CommandError::new(
                    "arguments.function.unknown",
                    vec![ErrArg::Str(f.id.clone())],
                )
            })
    }
}

/// The macro arguments of a `function` command.
pub(crate) fn arguments(
    command: CommandId,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<Option<Compound>, CommandError> {
    match command {
        CommandId::FunctionWithArguments => Ok(args.compound("arguments").cloned()),
        CommandId::FunctionWith | CommandId::FunctionWithPath => {
            let data = super::data::read_source(args, source, env)?;
            if command == CommandId::FunctionWith {
                return Ok(Some(data));
            }
            let path = args
                .nbt_path("path")
                .ok_or_else(|| CommandError::new("command.exception", vec![]))?;
            match super::data::single_tag(path, &Tag::Compound(data))? {
                Tag::Compound(c) => Ok(Some(c)),
                other => Err(CommandError::new(
                    "commands.function.error.argument_not_compound",
                    vec![ErrArg::Str(super::data::type_name(&other).into())],
                )),
            }
        }
        _ => Ok(None),
    }
}

/// `FunctionCommand.modifySenderForExecution`: silent, at least game master level.
pub(crate) fn function_sender(source: &CommandSource) -> CommandSource {
    let mut out = source.clone();
    out.silent = true;
    out.level = out.level.max(vanillite_game::commands::level::GAMEMASTERS);
    out
}

/// `getGameLoopSender`: the server at the overworld spawn, game master level, silent.
pub(crate) fn game_loop_sender(global: &Global) -> Option<CommandSource> {
    let mut source = CommandSource::new(
        global.arc()?,
        SourceKind::Server,
        vanillite_game::commands::level::GAMEMASTERS,
        WorldId(0),
        global.systems.spawn(),
        (0.0, 0.0),
        None,
    );
    source.silent = true;
    Some(source)
}

/// `ServerFunctionManager.tick`: the `#minecraft:load` functions after a (re)load, then
/// `#minecraft:tick`, each in its own execution. Runs on the overworld before its tick.
pub(crate) fn tick(env: &mut Env<'_>) {
    let global = env.global;
    let library = global.systems.functions();
    let load = global.systems.take_post_reload();
    let Some(sender) = game_loop_sender(global) else {
        return;
    };
    if load && let Some(functions) = library.tag("minecraft:load") {
        for f in functions {
            run_function(f, &sender, env);
        }
    }
    if let Some(functions) = library.tag("minecraft:tick") {
        for f in functions {
            run_function(f, &sender, env);
        }
    }
}

/// `ServerFunctionManager.execute`.
pub(crate) fn run_function(function: &Function, sender: &CommandSource, env: &mut Env<'_>) {
    let Ok(instantiated) = function.instantiate(None, env.global) else {
        return;
    };
    let mut engine = new_engine(env.global);
    engine.queue_function(instantiated, sender.clone(), Callback::Empty);
    engine.run(env);
}

/// An execution with the limits of the game rules (`Commands.executeCommandInContext`).
pub(crate) fn new_engine(global: &Global) -> Engine {
    use vanillite_game::survival::rules::Rule;
    let rules = global.level.rules();
    let get = |name: &str| Rule::by_name(name).map_or(65536, |r| rules.get(r));
    Engine::new(get("max_command_sequence_length"), get("max_command_forks"))
}
