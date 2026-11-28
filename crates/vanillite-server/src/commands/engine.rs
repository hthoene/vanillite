//! Command execution like Vanilla's `ExecutionContext`: a queue of entries, each with the
//! frame it runs in. Commands, functions and `execute` chains are entries; an entry adds its
//! follow-ups to the front of the queue, so they run before the entries queued earlier.
//! `return` discards the rest of its frame. The quota (`maxCommandSequenceLength`) and the fork
//! limit (`maxCommandForks`) bound one execution (SEC: recursion and fork bombs).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, Parsed};
use vanillite_game::commands::CommandId;
use vanillite_protocol::text::Component;

use super::functions::Instantiated;
use super::source::CommandSource;
use super::{Env, SignedArguments, dispatch, execute, functions};

/// Vanilla's `MAX_QUEUE_DEPTH`.
const MAX_QUEUE: usize = 10_000_000;

/// A parsed command line ready to run.
#[derive(Clone)]
pub(crate) struct Line {
    /// The parse result.
    pub(crate) parsed: Arc<Parsed<CommandId>>,
    /// Signed arguments of a player's command.
    pub(crate) signed: Option<Arc<SignedArguments>>,
}

/// Where a result goes (`CommandResultCallback`).
#[derive(Clone, Default)]
pub(crate) enum Callback {
    /// Nowhere.
    #[default]
    Empty,
    /// `execute store`.
    Store(Arc<execute::Store>),
    /// Both, in order.
    Chain(Arc<(Callback, Callback)>),
    /// Adds the results of several functions.
    Accumulate(Arc<Accumulator>),
    /// `commands.function.result` to the source, then the inner callback.
    Announce(Arc<(CommandSource, String, Callback)>),
    /// `execute if|unless function`: keeps the source when the result passes.
    Condition(Arc<Condition>),
    /// Counts the successful results (a command block's `successCount`).
    Count(Arc<AtomicI32>),
}

impl Callback {
    /// `CommandResultCallback.chain`.
    pub(crate) fn chain(first: &Callback, second: &Callback) -> Callback {
        match (first, second) {
            (Callback::Empty, s) => s.clone(),
            (f, Callback::Empty) => f.clone(),
            (f, s) => Callback::Chain(Arc::new((f.clone(), s.clone()))),
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Callback::Empty)
    }

    /// `onResult`.
    pub(crate) fn fire(&self, env: &mut Env<'_>, success: bool, value: i32) {
        match self {
            Callback::Empty => {}
            Callback::Store(store) => execute::store(store, env, success, value),
            Callback::Chain(pair) => {
                pair.0.fire(env, success, value);
                pair.1.fire(env, success, value);
            }
            Callback::Accumulate(acc) => {
                acc.any.store(true, Ordering::Relaxed);
                let _ = acc
                    .sum
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |s| {
                        Some(s.wrapping_add(value))
                    });
            }
            Callback::Announce(inner) => {
                let (source, id, next) = &**inner;
                source.send_success(
                    env.global,
                    Component::translatable_args(
                        "commands.function.result",
                        vec![
                            vanillite_protocol::text::Arg::String(id.clone()),
                            vanillite_protocol::text::Arg::Int(value),
                        ],
                    ),
                    true,
                );
                next.fire(env, success, value);
            }
            Callback::Count(count) => {
                if success {
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }
            Callback::Condition(c) => {
                let pass = if c.when_nonzero {
                    value != 0
                } else {
                    value == 0
                };
                if pass {
                    c.passed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(c.source.clone());
                }
            }
        }
    }
}

/// The sum of function results (`FunctionCommand`'s accumulator).
#[derive(Default)]
pub(crate) struct Accumulator {
    any: AtomicBool,
    sum: AtomicI32,
}

/// One source of `execute if|unless function`.
pub(crate) struct Condition {
    when_nonzero: bool,
    source: CommandSource,
    passed: Arc<Mutex<Vec<CommandSource>>>,
}

/// `ChainModifiers`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mods {
    pub(crate) forked: bool,
    pub(crate) is_return: bool,
}

/// What `discard` of a frame removes.
#[derive(Clone, Copy, Debug)]
enum Control {
    /// The whole queue (the outermost frame).
    Clear,
    /// Entries at this depth or deeper.
    DiscardFrom(u32),
}

/// A frame (`Frame`).
#[derive(Clone)]
struct Frame {
    depth: u32,
    ret: Callback,
    control: Control,
}

/// `BuildContexts`: runs the redirect modifiers from `stage` on, then queues the command.
struct Build {
    line: Line,
    stage: usize,
    original: CommandSource,
    sources: Vec<CommandSource>,
    /// Sources known only when the entry runs (`execute if function`).
    later: Option<Arc<Mutex<Vec<CommandSource>>>>,
    mods: Mods,
}

enum Action {
    Build(Build),
    /// `ExecuteCommand` for the first of `sources`, then the rest (`ContinuationTask`).
    Execute {
        line: Line,
        sources: VecDeque<CommandSource>,
        mods: Mods,
    },
    /// `CallFunction`.
    Call {
        function: Arc<Instantiated>,
        callback: Callback,
        return_parent: bool,
        source: CommandSource,
    },
    /// The lines of a function from `index` on.
    Lines {
        function: Arc<Instantiated>,
        index: usize,
        source: CommandSource,
    },
    /// `FallthroughTask`.
    Fallthrough,
    /// The end of several functions with one callback.
    Accumulated {
        acc: Arc<Accumulator>,
        callback: Callback,
    },
    /// `IsolatedCall` of `execute if function`.
    Isolated {
        functions: Arc<[Arc<Instantiated>]>,
        source: CommandSource,
        output: Callback,
    },
    /// Feedback once the entries before it ran (`debug function`).
    Message {
        source: CommandSource,
        message: Component,
    },
}

struct Entry {
    frame: Frame,
    action: Action,
}

/// One execution (`ExecutionContext`).
pub(crate) struct Engine {
    command_limit: i32,
    fork_limit: usize,
    quota: i32,
    overflow: bool,
    queue: VecDeque<Entry>,
    new_top: Vec<Entry>,
    depth: u32,
}

impl Engine {
    /// An execution with the limits of the game rules.
    pub(crate) fn new(command_limit: i32, fork_limit: i32) -> Engine {
        Engine {
            command_limit,
            fork_limit: usize::try_from(fork_limit.max(0)).unwrap_or(0),
            quota: command_limit,
            overflow: false,
            queue: VecDeque::new(),
            new_top: Vec::new(),
            depth: 0,
        }
    }

    fn top_frame(&self, ret: Callback) -> Frame {
        if self.depth == 0 {
            Frame {
                depth: 0,
                ret,
                control: Control::Clear,
            }
        } else {
            let depth = self.depth.saturating_add(1);
            Frame {
                depth,
                ret,
                control: Control::DiscardFrom(depth),
            }
        }
    }

    /// `queueInitialCommandExecution`.
    pub(crate) fn queue_command(&mut self, line: Line, source: CommandSource, ret: Callback) {
        let frame = self.top_frame(ret);
        self.queue_next(Entry {
            frame,
            action: Action::Build(Build {
                line,
                stage: 0,
                original: source.clone(),
                sources: vec![source],
                later: None,
                mods: Mods::default(),
            }),
        });
    }

    /// `queueInitialFunctionCall`.
    pub(crate) fn queue_function(
        &mut self,
        function: Arc<Instantiated>,
        source: CommandSource,
        ret: Callback,
    ) {
        let frame = self.top_frame(ret);
        let callback = source.callback.clone();
        self.queue_next(Entry {
            frame,
            action: Action::Call {
                function,
                callback,
                return_parent: false,
                source,
            },
        });
    }

    fn queue_next(&mut self, entry: Entry) {
        if self.new_top.len().saturating_add(self.queue.len()) > MAX_QUEUE {
            self.overflow = true;
            self.new_top.clear();
            self.queue.clear();
        }
        if !self.overflow {
            self.new_top.push(entry);
        }
    }

    fn queue(&mut self, frame: &Frame, action: Action) {
        self.queue_next(Entry {
            frame: frame.clone(),
            action,
        });
    }

    fn discard(&mut self, control: Control) {
        match control {
            Control::Clear => self.queue.clear(),
            Control::DiscardFrom(depth) => {
                while self.queue.front().is_some_and(|e| e.frame.depth >= depth) {
                    self.queue.pop_front();
                }
            }
        }
    }

    fn push_new(&mut self) {
        for entry in self.new_top.drain(..).rev() {
            self.queue.push_front(entry);
        }
    }

    fn cost(&mut self) {
        self.quota = self.quota.saturating_sub(1);
    }

    /// `runCommandQueue`.
    pub(crate) fn run(&mut self, env: &mut Env<'_>) {
        self.push_new();
        loop {
            if self.quota <= 0 {
                log::info!(
                    "Command execution stopped due to limit (executed {} commands)",
                    self.command_limit
                );
                break;
            }
            let Some(entry) = self.queue.pop_front() else {
                break;
            };
            self.depth = entry.frame.depth;
            self.execute(entry, env);
            if self.overflow {
                log::error!(
                    "Command execution stopped due to command queue overflow (max {MAX_QUEUE})"
                );
                break;
            }
            self.push_new();
        }
        self.depth = 0;
    }

    fn execute(&mut self, entry: Entry, env: &mut Env<'_>) {
        let Entry { frame, action } = entry;
        match action {
            Action::Build(build) => self.build(build, &frame, env),
            Action::Execute {
                line,
                mut sources,
                mods,
            } => {
                let Some(source) = sources.pop_front() else {
                    return;
                };
                let rest = !sources.is_empty();
                // `ContinuationTask`: this source's command first, then the others.
                let follow = rest.then(|| Action::Execute {
                    line: line.clone(),
                    sources,
                    mods,
                });
                self.cost();
                run_command(&line, &source, mods, env);
                if let Some(follow) = follow {
                    self.queue(&frame, follow);
                }
            }
            Action::Call {
                function,
                callback,
                return_parent,
                source,
            } => {
                self.cost();
                let depth = frame.depth.saturating_add(1);
                let control = if return_parent {
                    frame.control
                } else {
                    Control::DiscardFrom(depth)
                };
                let inner = Frame {
                    depth,
                    ret: callback,
                    control,
                };
                if !function.lines.is_empty() {
                    self.queue(
                        &inner,
                        Action::Lines {
                            function,
                            index: 0,
                            source,
                        },
                    );
                }
            }
            Action::Lines {
                function,
                index,
                source,
            } => {
                let Some(line) = function.lines.get(index).cloned() else {
                    return;
                };
                self.queue(
                    &frame,
                    Action::Build(Build {
                        line,
                        stage: 0,
                        original: source.clone(),
                        sources: vec![source.clone()],
                        later: None,
                        mods: Mods::default(),
                    }),
                );
                let next = index.saturating_add(1);
                if next < function.lines.len() {
                    self.queue(
                        &frame,
                        Action::Lines {
                            function,
                            index: next,
                            source,
                        },
                    );
                }
            }
            Action::Fallthrough => {
                frame.ret.fire(env, false, 0);
                self.discard(frame.control);
            }
            Action::Accumulated { acc, callback } => {
                if acc.any.load(Ordering::Relaxed) {
                    callback.fire(env, true, acc.sum.load(Ordering::Relaxed));
                }
            }
            Action::Message { source, message } => {
                source.send_success(env.global, message, true);
            }
            Action::Isolated {
                functions,
                source,
                output,
            } => {
                let depth = frame.depth.saturating_add(1);
                let inner = Frame {
                    depth,
                    ret: output,
                    control: Control::DiscardFrom(depth),
                };
                for function in functions.iter() {
                    self.queue(
                        &inner,
                        Action::Call {
                            function: Arc::clone(function),
                            callback: inner.ret.clone(),
                            return_parent: true,
                            source: source.clone(),
                        },
                    );
                }
                self.queue(&inner, Action::Fallthrough);
            }
        }
    }

    /// `BuildContexts.execute`.
    fn build(&mut self, build: Build, frame: &Frame, env: &mut Env<'_>) {
        let Build {
            line,
            mut stage,
            original,
            mut sources,
            later,
            mut mods,
        } = build;
        if let Some(later) = later {
            sources = std::mem::take(&mut *later.lock().unwrap_or_else(PoisonError::into_inner));
        }
        let parsed = Arc::clone(&line.parsed);
        loop {
            sources = forward_remote(sources, &line, stage, mods, env);
            let Some(st) = parsed.stages.get(stage) else {
                break;
            };
            if st.forks {
                mods.forked = true;
            }
            if let Some(modifier) = st.modifier {
                if is_custom_modifier(modifier) {
                    self.custom_modifier(
                        modifier, &line, stage, original, sources, mods, frame, env,
                    );
                    return;
                }
                self.cost();
                let mut next = Vec::new();
                for source in &sources {
                    let modified = env.at(source.world, |env| {
                        execute::modify(modifier, &st.args, source, env)
                    });
                    match modified {
                        Ok(new) => {
                            if next.len().saturating_add(new.len()) >= self.fork_limit {
                                let limit = i32::try_from(self.fork_limit).unwrap_or(i32::MAX);
                                handle_error(
                                    &original,
                                    env,
                                    &CommandError::new(
                                        "command.forkLimit",
                                        vec![ErrArg::Int(limit)],
                                    ),
                                    mods.forked,
                                );
                                return;
                            }
                            next.extend(new);
                        }
                        Err(e) => {
                            handle_error(source, env, &e, mods.forked);
                            if !mods.forked {
                                return;
                            }
                        }
                    }
                }
                sources = next;
            }
            stage = stage.saturating_add(1);
        }
        if sources.is_empty() {
            if mods.is_return {
                self.queue(frame, Action::Fallthrough);
            }
            return;
        }
        let command = parsed.command;
        if is_custom_command(command) {
            for source in sources {
                self.custom_command(command, &line, source, mods, frame, env);
            }
            return;
        }
        if mods.is_return
            && let Some(first) = sources.first()
        {
            let returning = first.with_callback(Callback::chain(&first.callback, &frame.ret));
            sources = vec![returning];
        }
        self.queue(
            frame,
            Action::Execute {
                line,
                sources: sources.into(),
                mods,
            },
        );
    }

    /// `return run` and `execute if|unless function`.
    #[allow(clippy::too_many_arguments)]
    fn custom_modifier(
        &mut self,
        modifier: CommandId,
        line: &Line,
        stage: usize,
        original: CommandSource,
        sources: Vec<CommandSource>,
        mods: Mods,
        frame: &Frame,
        env: &mut Env<'_>,
    ) {
        let next_stage = stage.saturating_add(1);
        match modifier {
            CommandId::ReturnRun => {
                if sources.is_empty() {
                    if mods.is_return {
                        self.queue(frame, Action::Fallthrough);
                    }
                    return;
                }
                self.discard(frame.control);
                self.queue(
                    frame,
                    Action::Build(Build {
                        line: line.clone(),
                        stage: next_stage,
                        original,
                        sources,
                        later: None,
                        mods: Mods {
                            is_return: true,
                            ..mods
                        },
                    }),
                );
            }
            _ => {
                // `scheduleFunctionConditionsAndTest`.
                let when_nonzero = modifier == CommandId::ExecuteIfFunction;
                let args = match line.parsed.stages.get(stage) {
                    Some(s) => &s.args,
                    None => return,
                };
                let found = match functions::resolve(env.global, args, "name") {
                    Ok(found) => found,
                    Err(e) => {
                        handle_error(&original, env, &e, mods.forked);
                        return;
                    }
                };
                if found.is_empty() {
                    return;
                }
                let mut instantiated = Vec::with_capacity(found.len());
                for function in &found {
                    match function.instantiate(None, env.global) {
                        Ok(i) => instantiated.push(i),
                        Err(reason) => {
                            let e = CommandError::new(
                                "commands.execute.function.instantiationFailure",
                                vec![
                                    ErrArg::Str(function.id.to_string()),
                                    ErrArg::Component(reason),
                                ],
                            );
                            handle_error(&original, env, &e, mods.forked);
                            // Vanilla stops instantiating at the first failure and runs the
                            // functions instantiated before it.
                            break;
                        }
                    }
                }
                let instantiated: Arc<[Arc<Instantiated>]> = instantiated.into();
                let passed = Arc::new(Mutex::new(Vec::with_capacity(sources.len())));
                for source in sources {
                    let context =
                        functions::function_sender(&source.with_callback(Callback::Empty));
                    let output = Callback::Condition(Arc::new(Condition {
                        when_nonzero,
                        source,
                        passed: Arc::clone(&passed),
                    }));
                    self.queue(
                        frame,
                        Action::Isolated {
                            functions: Arc::clone(&instantiated),
                            source: context,
                            output,
                        },
                    );
                }
                // The sources that pass are only known after the calls: the continuation
                // reads them when it runs.
                self.queue(
                    frame,
                    Action::Build(Build {
                        line: line.clone(),
                        stage: next_stage,
                        original,
                        sources: Vec::new(),
                        later: Some(passed),
                        mods,
                    }),
                );
            }
        }
    }

    /// `function`, `return <value>` and `return fail`.
    fn custom_command(
        &mut self,
        command: CommandId,
        line: &Line,
        source: CommandSource,
        mods: Mods,
        frame: &Frame,
        env: &mut Env<'_>,
    ) {
        match command {
            CommandId::ReturnValue => {
                let value = line.parsed.args.int("value").copied().unwrap_or(0);
                source.callback.fire(env, true, value);
                frame.ret.fire(env, true, value);
                self.discard(frame.control);
            }
            CommandId::ReturnFail => {
                source.callback.fire(env, false, 0);
                frame.ret.fire(env, false, 0);
                self.discard(frame.control);
            }
            CommandId::DebugFunction => {
                if let Err(e) = self.debug_function(line, &source, mods, frame, env) {
                    handle_error(&source, env, &e, mods.forked);
                    source.callback.fire(env, false, 0);
                }
            }
            _ => {
                if let Err(e) = self.function_command(command, line, &source, mods, frame, env) {
                    handle_error(&source, env, &e, mods.forked);
                    source.callback.fire(env, false, 0);
                }
            }
        }
    }

    /// `FunctionCommand.FunctionCustomExecutor.runGuarded`.
    fn function_command(
        &mut self,
        command: CommandId,
        line: &Line,
        source: &CommandSource,
        mods: Mods,
        frame: &Frame,
        env: &mut Env<'_>,
    ) -> Result<(), CommandError> {
        let args = &line.parsed.args;
        let found = functions::resolve(env.global, args, "name")?;
        if found.is_empty() {
            let name = functions::name_arg(args, "name");
            return Err(CommandError::new(
                "commands.function.scheduled.no_functions",
                vec![ErrArg::Str(name)],
            ));
        }
        let arguments = functions::arguments(command, args, source, env)?;
        let common = functions::function_sender(source);
        let message = if let [only] = found.as_slice() {
            Component::translatable_args(
                "commands.function.scheduled.single",
                vec![vanillite_protocol::text::Arg::String(only.id.to_string())],
            )
        } else {
            Component::translatable(
                "commands.function.scheduled.multiple",
                vec![super::exec::format_list(
                    found
                        .iter()
                        .map(|f| Component::text(f.id.to_string()))
                        .collect(),
                )],
            )
        };
        source.send_success(env.global, message, true);
        self.queue_functions(
            &found,
            arguments.as_ref(),
            source,
            &common,
            frame,
            mods,
            env,
        )
    }

    /// `DebugCommand.TraceCustomExecutor`: runs the functions with the source's output going
    /// to a trace file in `debug/`. There is no command tracer: the trace lists the functions
    /// called and their sizes.
    fn debug_function(
        &mut self,
        line: &Line,
        source: &CommandSource,
        mods: Mods,
        frame: &Frame,
        env: &mut Env<'_>,
    ) -> Result<(), CommandError> {
        use vanillite_protocol::text::Arg;
        if mods.is_return {
            return Err(CommandError::new(
                "commands.debug.function.noReturnRun",
                vec![],
            ));
        }
        let found = functions::resolve(env.global, &line.parsed.args, "name")?;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let (y, mo, d, h, mi, s) = crate::access::utc(secs);
        let output = format!("debug-trace-{y:04}-{mo:02}-{d:02}_{h:02}.{mi:02}.{s:02}.txt");
        let dir = env
            .global
            .systems
            .level_dir
            .parent()
            .map_or_else(|| std::path::PathBuf::from("debug"), |p| p.join("debug"));
        let mut trace = String::new();
        let mut tracker = super::tracker::Tracker::<String>::default();
        let mut function_source = source.clone();
        function_source.silent = true;
        function_source.level = function_source
            .level
            .min(vanillite_game::commands::level::GAMEMASTERS);
        let function_source = function_source.with_callback(Callback::Empty);
        for function in &found {
            match function.instantiate(None, env.global) {
                Ok(instantiated) => {
                    let size = instantiated.lines.len();
                    trace.push_str(&format!(
                        "{0}
    [F] {0} size={size}
",
                        function.id
                    ));
                    self.queue(
                        frame,
                        Action::Call {
                            function: instantiated,
                            callback: Callback::Empty,
                            return_parent: false,
                            source: function_source.clone(),
                        },
                    );
                    tracker.track(
                        &function.id.to_string(),
                        i32::try_from(size).unwrap_or(i32::MAX),
                    );
                }
                Err(reason) => source.send_failure(env.global, *reason),
            }
        }
        let written =
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join(&output), trace));
        if let Err(e) = written {
            log::warn!("Tracing failed: {e}");
            source.send_failure(
                env.global,
                Component::translatable("commands.debug.function.traceFailed", vec![]),
            );
        }
        let (single, count) = tracker.pick(true);
        let total = tracker.total();
        let message = match single {
            Some(id) => Component::translatable_args(
                "commands.debug.function.success.single",
                vec![
                    Arg::Int(total),
                    Arg::String(id.clone()),
                    Arg::String(output),
                ],
            ),
            None => Component::translatable_args(
                "commands.debug.function.success.multiple",
                vec![Arg::Int(total), Arg::Int(count), Arg::String(output)],
            ),
        };
        self.queue(
            frame,
            Action::Message {
                source: source.clone(),
                message,
            },
        );
        Ok(())
    }

    /// `FunctionCommand.queueFunctions`.
    #[allow(clippy::too_many_arguments)]
    fn queue_functions(
        &mut self,
        found: &[Arc<functions::Function>],
        arguments: Option<&vanillite_nbt::Compound>,
        original: &CommandSource,
        function_source: &CommandSource,
        frame: &Frame,
        mods: Mods,
        env: &mut Env<'_>,
    ) -> Result<(), CommandError> {
        let no_callback = function_source.with_callback(Callback::Empty);
        let decorate = |id: &str, callback: Callback| {
            if original.silent {
                callback
            } else {
                Callback::Announce(Arc::new((original.clone(), id.to_owned(), callback)))
            }
        };
        let mut calls = Vec::with_capacity(found.len());
        for function in found {
            let instantiated = function
                .instantiate(arguments, env.global)
                .map_err(|reason| {
                    CommandError::new(
                        "commands.function.instantiationFailure",
                        vec![
                            ErrArg::Str(function.id.to_string()),
                            ErrArg::Component(reason),
                        ],
                    )
                })?;
            calls.push((function.id.to_string(), instantiated));
        }
        if mods.is_return {
            let output = Callback::chain(&original.callback, &frame.ret);
            for (id, function) in calls {
                self.queue(
                    frame,
                    Action::Call {
                        function,
                        callback: decorate(&id, output.clone()),
                        return_parent: true,
                        source: no_callback.clone(),
                    },
                );
            }
            self.queue(frame, Action::Fallthrough);
            return Ok(());
        }
        let original_callback = original.callback.clone();
        if calls.len() == 1 || original_callback.is_empty() {
            for (id, function) in calls {
                self.queue(
                    frame,
                    Action::Call {
                        function,
                        callback: decorate(&id, original_callback.clone()),
                        return_parent: false,
                        source: no_callback.clone(),
                    },
                );
            }
            return Ok(());
        }
        let acc = Arc::new(Accumulator::default());
        for (id, function) in calls {
            self.queue(
                frame,
                Action::Call {
                    function,
                    callback: decorate(&id, Callback::Accumulate(Arc::clone(&acc))),
                    return_parent: false,
                    source: no_callback.clone(),
                },
            );
        }
        self.queue(
            frame,
            Action::Accumulated {
                acc,
                callback: original_callback,
            },
        );
        Ok(())
    }
}

/// Sources in another world than the execution's (after `execute in`, or a player that moved)
/// stay in this execution when that world is at hand ([`Env::at`], Vanilla's order). The
/// others continue there: the rest of the line from `stage` runs on that world's thread in an
/// execution of its own, without waiting, as cross-world work does. Their results still reach
/// `execute store` through their callbacks; a `return` there ends only that execution.
fn forward_remote(
    sources: Vec<CommandSource>,
    line: &Line,
    stage: usize,
    mods: Mods,
    env: &Env<'_>,
) -> Vec<CommandSource> {
    if sources.iter().all(|s| env.has(s.world)) {
        return sources;
    }
    let (local, remote): (Vec<_>, Vec<_>) = sources.into_iter().partition(|s| env.has(s.world));
    for source in remote {
        let line = line.clone();
        let world = source.world;
        env.global.run_in_world(world, move |w| {
            let global = Arc::clone(&source.global);
            let mut engine = super::functions::new_engine(&global);
            let frame = engine.top_frame(Callback::Empty);
            engine.queue_next(Entry {
                frame,
                action: Action::Build(Build {
                    line,
                    stage,
                    original: source.clone(),
                    sources: vec![source],
                    later: None,
                    mods,
                }),
            });
            let mut env = Env {
                level: w.level_mut(),
                global: &global,
                world,
                others: Vec::new(),
            };
            engine.run(&mut env);
        });
    }
    local
}

/// Commands that work on the queue themselves (`CustomCommandExecutor`).
fn is_custom_command(command: CommandId) -> bool {
    matches!(
        command,
        CommandId::ReturnValue
            | CommandId::ReturnFail
            | CommandId::Function
            | CommandId::FunctionWithArguments
            | CommandId::FunctionWith
            | CommandId::FunctionWithPath
            | CommandId::DebugFunction
    )
}

/// Modifiers that work on the queue themselves (`CustomModifierExecutor`).
fn is_custom_modifier(modifier: CommandId) -> bool {
    matches!(
        modifier,
        CommandId::ReturnRun | CommandId::ExecuteIfFunction | CommandId::ExecuteUnlessFunction
    )
}

/// `ExecuteCommand.execute`: runs the command for one source; the result goes to its callback.
fn run_command(line: &Line, source: &CommandSource, mods: Mods, env: &mut Env<'_>) {
    let empty = SignedArguments::new();
    let signed = line.signed.as_deref().unwrap_or(&empty);
    let result = env.at(source.world, |env| {
        dispatch::execute(source, env, &line.parsed, signed)
    });
    match result {
        Ok(result) => source.callback.fire(env, true, result),
        Err(e) => {
            source.callback.fire(env, false, 0);
            if !mods.forked {
                handle_error(source, env, &e, false);
            }
        }
    }
}

/// `handleError`: the message to the source unless forked.
pub(crate) fn handle_error(source: &CommandSource, env: &Env<'_>, e: &CommandError, forked: bool) {
    if !forked {
        source.send_failure(env.global, super::error_message(e));
    }
}
