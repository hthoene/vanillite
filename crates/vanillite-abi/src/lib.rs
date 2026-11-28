//! The stable C ABI for plugins and providers: `repr(C)` tables generated from one declarative API definition.
//!
//! Everything that crosses the boundary between the server and a plugin library is declared
//! here, once, with the `abi!` macro: plain C types (integers, floats, `repr(C)` records,
//! `(ptr, len)` strings, function pointers), never Rust types such as `Box`, `Vec`, `String` or
//! trait objects. Plugins therefore depend neither on the server's compiler version nor on its
//! dependencies. Memory is freed by the side that allocated it.
//!
//! # Versioning
//!
//! - [`ABI_MAJOR`] changes on a breaking change; [`ABI_MINOR`] when functions, events or record
//!   fields are appended. A plugin built against `1.2` runs on a `1.4` host; a plugin that needs
//!   `1.4` is refused by a `1.2` host.
//! - Tables and records start with a `size` field: each side reads only what the other knows.
//!   Fields are only ever appended at the end. Arrays of records carry their element size
//!   (`stride`), so a record in an array can grow too.
//! - Enumerations are `u32` constants; unknown values are ignored or refused, never trusted.
//! - [`layout`] describes every declaration; the test `abi_snapshot` compares it with the
//!   recorded `abi.snapshot` and fails on a breaking change without a major version bump.
//!
//! # Entry point
//!
//! A plugin library exports `vanillite_plugin_abi` ([`ENTRY_SYMBOL`], type [`EntryFn`]). It
//! returns a [`PluginInfo`] before any other plugin code runs; the host validates it (ABI
//! version, panic strategy, target) and only then calls [`PluginInfo::load`] with the
//! [`HostApi`] table.
//!
//! # Threads and contexts
//!
//! Host functions whose first parameter is a [`PluginId`] are thread-safe and may be called
//! from any thread after `load`. Functions whose first parameter is a `*mut` [`Context`] touch
//! state of one owner (a world) and are only valid during the callback that received the
//! context; they fail with [`status::WRONG_OWNER`] when the context does not own the data.

#![no_std]

use core::ffi::c_void;

pub mod layout;

/// Major version of the plugin ABI: changes on breaking changes.
pub const ABI_MAJOR: u16 = 1;
/// Minor version of the plugin ABI: changes when declarations are appended.
pub const ABI_MINOR: u16 = 13;
/// The symbol every plugin library exports (type [`EntryFn`]).
pub const ENTRY_SYMBOL: &str = "vanillite_plugin_abi";

/// An opaque per-call context the host passes to callbacks. It is only valid during the call.
#[repr(C)]
#[derive(Debug)]
pub struct Context {
    _opaque: [u8; 0],
}

/// Declares the ABI: type aliases, constant groups and `repr(C)` records, and records each
/// declaration in [`layout::TYPES`], [`layout::CONSTS`] and [`layout::STRUCTS`].
macro_rules! abi {
    (
        types { $( $(#[$tmeta:meta])* pub type $tname:ident = $tty:ty; )* }
        consts { $(
            $(#[$cmeta:meta])* pub mod $cmod:ident: $cty:ty {
                $( $(#[$vmeta:meta])* $cname:ident = $cvalue:expr, )*
            }
        )* }
        structs { $(
            $(#[$smeta:meta])* pub struct $sname:ident {
                $( $(#[$fmeta:meta])* pub $fname:ident: $fty:ty, )*
            }
        )* }
    ) => {
        $( $(#[$tmeta])* pub type $tname = $tty; )*
        $(
            $(#[$cmeta])* pub mod $cmod {
                #[allow(unused_imports)]
                use super::*;
                $( $(#[$vmeta])* pub const $cname: $cty = $cvalue; )*
            }
        )*
        $(
            $(#[$smeta])*
            #[repr(C)]
            #[derive(Clone, Copy, Debug)]
            pub struct $sname { $( $(#[$fmeta])* pub $fname: $fty, )* }
        )*

        /// The declarations of this ABI as data, for the compatibility check.
        pub(crate) mod declared {
            use super::*;

            pub(crate) const TYPES: &[layout::TypeLayout] = &[
                $( layout::TypeLayout { name: stringify!($tname), definition: stringify!($tty) }, )*
            ];

            pub(crate) const CONSTS: &[layout::ConstLayout] = &[
                $( $( layout::ConstLayout {
                    name: concat!(stringify!($cmod), "::", stringify!($cname)),
                    ty: stringify!($cty),
                    value: $cmod::$cname as i64,
                }, )* )*
            ];

            pub(crate) const STRUCTS: &[layout::StructLayout] = &[
                $( layout::StructLayout {
                    name: stringify!($sname),
                    size: core::mem::size_of::<$sname>(),
                    align: core::mem::align_of::<$sname>(),
                    fields: &[ $( layout::FieldLayout {
                        name: stringify!($fname),
                        ty: stringify!($fty),
                        offset: core::mem::offset_of!($sname, $fname),
                    }, )* ],
                }, )*
            ];
        }
    };
}

abi! {
    types {
        /// Result of a call: [`status::OK`] or an error code of [`status`].
        pub type Status = i32;
        /// Identifies the calling plugin in host calls; the host passes it to `load`.
        pub type PluginId = u32;
        /// The plugin's entry point, exported as `vanillite_plugin_abi`.
        pub type EntryFn = unsafe extern "C" fn() -> *const PluginInfo;
        /// `PluginInfo::load`: register commands, events and providers.
        pub type LoadFn = unsafe extern "C" fn(host: *const HostApi, plugin: PluginId) -> Status;
        /// `PluginInfo::enable` and `PluginInfo::disable`.
        pub type LifecycleFn = unsafe extern "C" fn(ctx: *mut Context) -> Status;
        /// `PluginInfo::save_state`: writes the state to keep over a development reload.
        pub type SaveStateFn = unsafe extern "C" fn(sink: *const ByteSink) -> Status;
        /// `PluginInfo::restore_state`: takes over the state written by the previous build.
        pub type RestoreStateFn = unsafe extern "C" fn(state: Bytes) -> Status;
        /// Receives bytes (copied by the receiver before it returns).
        pub type ByteSinkFn = unsafe extern "C" fn(state: *mut c_void, data: Bytes);
        /// An event handler; `event` points to the record of the event's kind.
        pub type EventFn =
            unsafe extern "C" fn(user: *mut c_void, ctx: *mut Context, event: *mut EventHeader) -> Status;
        /// A scheduled task.
        pub type TaskFn = unsafe extern "C" fn(user: *mut c_void, ctx: *mut Context) -> Status;
        /// Frees user data once the host no longer needs it.
        pub type DropFn = unsafe extern "C" fn(user: *mut c_void);
        /// Runs a command; `result` receives the command's result value.
        pub type CommandFn = unsafe extern "C" fn(
            user: *mut c_void,
            ctx: *mut Context,
            invocation: *const CommandInvocation,
            result: *mut i32,
        ) -> Status;
        /// Answers a custom suggestion provider of a command argument.
        pub type SuggestFn = unsafe extern "C" fn(
            user: *mut c_void,
            ctx: *mut Context,
            request: *const SuggestRequest,
            sink: *const SuggestSink,
        ) -> Status;
        /// Adds one suggestion (copied by the host).
        pub type SuggestAddFn = unsafe extern "C" fn(state: *mut c_void, text: Str);
        /// A permission provider: writes one of [`permission`] to `out`.
        pub type PermissionFn =
            unsafe extern "C" fn(user: *mut c_void, player: Uuid, node: Str, out: *mut u32) -> Status;
        /// Creates the generator of one world; a null instance keeps the built-in generator.
        pub type GeneratorCreateFn = unsafe extern "C" fn(
            user: *mut c_void,
            world: *const WorldInfo,
            instance: *mut *mut c_void,
        ) -> Status;
        /// Fills the blocks of a batch of chunks (`chunks[i]` at `chunks + i * stride` bytes).
        pub type GeneratorRunFn = unsafe extern "C" fn(
            instance: *mut c_void,
            chunks: *const GenChunk,
            count: usize,
            stride: usize,
        ) -> Status;
        /// Destroys a generator instance.
        pub type GeneratorDestroyFn = unsafe extern "C" fn(instance: *mut c_void);
        /// Replaces a text (copied by the receiver before it returns; `format` from
        /// [`text_format`]). Since ABI 1.4.
        pub type TextSetFn = unsafe extern "C" fn(state: *mut c_void, text: Str, format: u32);
        /// Asks a plugin goal whether it should run (non-zero: yes). Since ABI 1.6.
        pub type GoalCheckFn = unsafe extern "C" fn(user: *mut c_void, mob: *const GoalMob) -> u32;
        /// Starts or stops a plugin goal. Since ABI 1.6.
        pub type GoalFn = unsafe extern "C" fn(user: *mut c_void, mob: *const GoalMob);
        /// Ticks a plugin goal: it writes what the mob does to `action`. Since ABI 1.6.
        pub type GoalTickFn =
            unsafe extern "C" fn(user: *mut c_void, mob: *const GoalMob, action: *mut GoalAction);
        /// Populates a batch of generated chunks (`chunks[i]` at `chunks + i * stride` bytes):
        /// their blocks are filled in, changes are kept. Since ABI 1.6.
        pub type PopulateFn = unsafe extern "C" fn(
            user: *mut c_void,
            world: *const WorldInfo,
            chunks: *const GenChunk,
            count: usize,
            stride: usize,
        ) -> Status;
        /// A packet listener; the event is valid for the call. Since ABI 1.7.
        pub type PacketFn =
            unsafe extern "C" fn(user: *mut c_void, ctx: *mut Context, event: *mut PacketEvent) -> Status;
        /// Replaces a packet's body (copied by the host before it returns); fails with
        /// [`status::INVALID_ARGUMENT`] for an empty or oversized body. Since ABI 1.7.
        pub type PacketSetFn = unsafe extern "C" fn(state: *mut c_void, body: Bytes) -> Status;
    }
    consts {
        /// Result codes ([`Status`]).
        pub mod status: Status {
            /// Success.
            OK = 0,
            /// A failure without a more specific code.
            ERROR = 1,
            /// The host or plugin does not support this call or value.
            UNSUPPORTED = 2,
            /// The thing does not exist (offline player, unknown key, stale handle).
            NOT_FOUND = 3,
            /// The context does not own this data; schedule a task on its owner instead.
            WRONG_OWNER = 4,
            /// An argument is invalid (bad UTF-8, unknown block state, out of range).
            INVALID_ARGUMENT = 5,
            /// The callee panicked; the panic was contained.
            PANICKED = 6,
            /// The call is not allowed in this phase (e.g. registering after `load`).
            WRONG_PHASE = 7,
            /// The plugin is disabled.
            DISABLED = 8,
        }
        /// Panic strategies ([`PluginInfo::panic_strategy`]).
        pub mod panic_strategy: u32 {
            /// `panic = "unwind"`: panics can be contained. Required.
            UNWIND = 0,
            /// `panic = "abort"`: a panic aborts the process. Refused by the host.
            ABORT = 1,
        }
        /// Log levels.
        pub mod log_level: u32 {
            /// Errors.
            ERROR = 1,
            /// Warnings.
            WARN = 2,
            /// Information.
            INFO = 3,
            /// Debug output.
            DEBUG = 4,
            /// Trace output.
            TRACE = 5,
        }
        /// Text formats of messages.
        pub mod text_format: u32 {
            /// Plain text.
            PLAIN = 0,
            /// A JSON text component, e.g. `{"text":"Hi","color":"gold"}`.
            JSON = 1,
        }
        /// Event kinds ([`EventHeader::kind`]) with the record each one points to.
        pub mod event: u32 {
            /// A player joined ([`PlayerEvent`]), after the join message.
            PLAYER_JOIN = 1,
            /// A player left ([`PlayerEvent`]).
            PLAYER_LEAVE = 2,
            /// A player sends a chat message ([`PlayerTextEvent`]); cancellable.
            PLAYER_CHAT = 3,
            /// A player runs a command ([`PlayerTextEvent`], without `/`); cancellable.
            PLAYER_COMMAND = 4,
            /// The players of a world that moved this tick ([`PlayersMovedEvent`]); batched.
            PLAYERS_MOVED = 5,
            /// An entity is about to take damage ([`EntityDamageEvent`]); cancellable,
            /// the amount can be changed.
            ENTITY_DAMAGE = 6,
            /// A player is about to break a block ([`BlockEvent`]); cancellable.
            BLOCK_BREAK = 7,
            /// A player is about to place a block ([`BlockEvent`]); cancellable.
            BLOCK_PLACE = 8,
            /// A player is about to fall asleep in a bed ([`BlockEvent`]: the bed's head);
            /// cancellable. Since ABI 1.3.
            PLAYER_BED_ENTER = 9,
            /// A player left its bed ([`BlockEvent`]: the bed's head). Since ABI 1.3.
            PLAYER_BED_LEAVE = 10,
            /// A bed or respawn anchor is about to become a player's respawn point
            /// ([`BlockEvent`]); cancellable. Since ABI 1.3.
            PLAYER_SPAWN_CHANGE = 11,
            /// A player logs in, after the server's own checks (bans, whitelist, player limit)
            /// in the login state, before configuration ([`LoginEvent`]); a refusal is a login
            /// disconnect. Runs on the global owner.
            /// Cancelled means refused: handlers may refuse with a message or let a refused
            /// player in. Since ABI 1.4.
            PLAYER_LOGIN = 12,
            /// A player died ([`PlayerDeathEvent`]): the death message and whether it keeps its
            /// inventory can be changed. Since ABI 1.4.
            PLAYER_DEATH = 13,
            /// A mob died ([`EntityDeathEvent`]). Since ABI 1.4.
            ENTITY_DEATH = 14,
            /// A player respawned, after death or the end credits ([`PlayerWorldEvent`]).
            /// Since ABI 1.4.
            PLAYER_RESPAWN = 15,
            /// A player arrived in another world through a portal or a teleport
            /// ([`PlayerWorldEvent`]). Since ABI 1.4.
            PLAYER_CHANGE_WORLD = 16,
            /// A player's game mode is about to change by `/gamemode` or the forced default
            /// game mode ([`GameModeEvent`]); cancellable. Since ABI 1.4.
            PLAYER_GAME_MODE_CHANGE = 17,
            /// A player uses its item on a block or in the air ([`InteractEvent`]);
            /// cancellable: nothing happens and the client is corrected. Since ABI 1.4.
            PLAYER_INTERACT = 18,
            /// A player sent a message on a channel a plugin registered
            /// ([`PluginMessageEvent`]). Since ABI 1.5.
            PLAYER_PLUGIN_MESSAGE = 19,
            /// A player completed an advancement ([`AdvancementEvent`]). Since ABI 1.5.
            PLAYER_ADVANCEMENT_DONE = 20,
            /// A player moved or turned ([`GameEvent`]: `entity` the player, `from`, `to`);
            /// cancellable: the player is set back to `from`. Handlers may change `to`: the
            /// player is teleported there. Fired per player and movement packet. Since ABI 1.6.
            PLAYER_MOVE = 21,
            /// A player is about to be teleported ([`GameEvent`]: `entity`, `cause` from
            /// [`teleport_cause`], `from`, `to`); cancellable, `to` can be changed (also its
            /// world for players). Since ABI 1.6.
            PLAYER_TELEPORT = 22,
            /// A player uses its item on an entity ([`GameEvent`]: `entity` the player,
            /// `other` the entity clicked, `cause` the [`hand`], `key` the item's id, `to` the
            /// point clicked); cancellable. Since ABI 1.6.
            PLAYER_INTERACT_ENTITY = 23,
            /// A player is about to attack an entity (left click; [`GameEvent`]: `entity` the
            /// player, `other` the target, `key` the item's id); cancellable. Since ABI 1.6.
            PLAYER_ATTACK_ENTITY = 24,
            /// A player drops an item ([`GameEvent`]: `entity`, `item` the stack as SNBT);
            /// cancellable: the item stays in the inventory. Since ABI 1.6.
            PLAYER_DROP_ITEM = 25,
            /// An entity (a player or a mob) picks up an item, arrow or trident
            /// ([`GameEvent`]: `entity` the picker, `other` the item entity, `item` the stack,
            /// `value` how many it takes); cancellable. Since ABI 1.6.
            ENTITY_PICKUP_ITEM = 26,
            /// A player starts (`flags` 1) or stops (0) sneaking ([`GameEvent`]: `entity`).
            /// Since ABI 1.6.
            PLAYER_TOGGLE_SNEAK = 27,
            /// A player starts (`flags` 1) or stops (0) sprinting ([`GameEvent`]: `entity`).
            /// Since ABI 1.6.
            PLAYER_TOGGLE_SPRINT = 28,
            /// A player starts (`flags` 1) or stops (0) flying ([`GameEvent`]: `entity`);
            /// cancellable: the client is corrected. Since ABI 1.6.
            PLAYER_TOGGLE_FLIGHT = 29,
            /// A player selects another hotbar slot ([`GameEvent`]: `entity`, `value` the new
            /// slot, `extra` the previous one); cancellable: the client is set back. Since
            /// ABI 1.6.
            PLAYER_ITEM_HELD = 30,
            /// A player finishes eating or drinking an item ([`GameEvent`]: `entity`, `item`,
            /// `cause` the [`hand`]); cancellable: nothing is consumed and nothing applies.
            /// Since ABI 1.6.
            PLAYER_ITEM_CONSUME = 31,
            /// A player is about to be kicked ([`GameEvent`]: `entity`, `text` the reason as
            /// JSON, `text_out` replaces it); cancellable (not for protocol errors and
            /// shutdown). Since ABI 1.6.
            PLAYER_KICK = 32,
            /// A player's statistic changes ([`GameEvent`]: `entity`, `key` the statistic as
            /// scoreboard criteria name it, e.g. `minecraft.custom:minecraft.jump`, `value` the
            /// new value, `extra` the old one); cancellable. Since ABI 1.6.
            PLAYER_STATISTIC = 33,
            /// A player's client reports a resource pack status ([`GameEvent`]: `entity`,
            /// `key` the pack's UUID, `cause` from [`resource_pack_status`]). Since ABI 1.6.
            PLAYER_RESOURCE_PACK = 34,
            /// A player swaps the items of its hands ([`GameEvent`]: `entity`, `item` the main
            /// hand's, `other_item` the off hand's); cancellable. Since ABI 1.6.
            PLAYER_SWAP_HANDS = 35,
            /// A player gains experience points from an orb ([`GameEvent`]: `entity`, `other`
            /// the orb, `value` the points, changeable). Since ABI 1.6.
            PLAYER_EXP_CHANGE = 36,
            /// A player is about to throw or shoot a projectile from an item it uses
            /// ([`GameEvent`]: `entity`, `key` the projectile's entity type, e.g.
            /// `minecraft:snowball`); cancellable: the item stays. [`PROJECTILE_LAUNCH`] follows
            /// once the projectile exists. Since ABI 1.6.
            PLAYER_LAUNCH_PROJECTILE = 37,
            /// A fishing rod action of a player ([`GameEvent`]: `entity`, `cause` from
            /// [`fish_state`]); cancellable: no cast, or the bobber stays without pulling or
            /// loot. Since ABI 1.6.
            PLAYER_FISH = 38,
            /// An entity is about to be added to a world ([`GameEvent`]: `entity`,
            /// `entity_type`, `cause` from [`spawn_reason`], `to` where, `other` the parent or
            /// spawner entity when there is one); cancellable: it is not added. Not for
            /// players and entities loaded from disk. Since ABI 1.6.
            ENTITY_SPAWN = 40,
            /// An entity died and is about to drop its loot ([`GameEvent`]: `entity`,
            /// `entity_type`, `other` the killer, `key` the damage type, `item` the drops as an
            /// SNBT list, `item_out` replaces them (`[]` for none), `value` the experience,
            /// changeable). Players too, before their inventory drops (`item` is then their
            /// inventory). Since ABI 1.6.
            ENTITY_DROPS = 41,
            /// A mob is about to change its target ([`GameEvent`]: `entity` the mob, `other`
            /// the new target or [`Uuid::NIL`] when it forgets it, `cause` from
            /// [`target_reason`]); cancellable: it keeps its target. Since ABI 1.6.
            ENTITY_TARGET = 42,
            /// An explosion is about to happen ([`GameEvent`]: `entity` its source or
            /// [`Uuid::NIL`] for a block (`pos`, `state`: a bed or respawn anchor), `to` its
            /// centre, `amount` its radius (changeable), `flags` from [`explode_flag`]
            /// (changeable)); cancellable. Since ABI 1.6. `text` names the blocks (not air) it
            /// would blow up as `x,y,z;x,y,z`, and `text_out` in the same form replaces that
            /// list: only the blocks named there are destroyed (servers before 0.1.0-beta.3 leave
            /// `text` empty and ignore `text_out`).
            ENTITY_EXPLODE = 43,
            /// A projectile is launched ([`GameEvent`]: `entity` the projectile,
            /// `entity_type`, `other` its shooter); cancellable: it is removed. Since ABI 1.6.
            PROJECTILE_LAUNCH = 44,
            /// A projectile hits something ([`GameEvent`]: `entity` the projectile, `other` the
            /// entity hit, or for a block `pos`, `state` and `cause` the face hit, `source` its
            /// shooter); cancellable: the hit is ignored. Since ABI 1.6.
            PROJECTILE_HIT = 45,
            /// An entity is about to mount a vehicle ([`GameEvent`]: `entity` the passenger,
            /// `other` the vehicle); cancellable. Since ABI 1.6.
            VEHICLE_ENTER = 46,
            /// An entity is about to leave its vehicle ([`GameEvent`]: `entity` the passenger,
            /// `other` the vehicle); cancellable where Vanilla lets it stay (not when the
            /// vehicle or passenger is removed). Since ABI 1.6.
            VEHICLE_EXIT = 47,
            /// An animal is about to be tamed ([`GameEvent`]: `entity`, `other` the owner);
            /// cancellable: the attempt fails. Since ABI 1.6.
            ENTITY_TAME = 48,
            /// Two animals are about to make a child ([`GameEvent`]: `entity` the child (not
            /// yet added), `other` and `source` the parents, `player` the breeder, `value` the
            /// experience, changeable); cancellable. Since ABI 1.6.
            ENTITY_BREED = 49,
            /// An entity is about to travel through a portal ([`GameEvent`]: `entity`,
            /// `cause` from [`portal_kind`], `from`, `to` the destination, changeable; since
            /// ABI 1.8 also its world, e.g. the nether of an extra world); cancellable.
            /// Players too. Since ABI 1.6.
            ENTITY_PORTAL = 50,
            /// An entity is about to turn into another ([`GameEvent`]: `entity`,
            /// `entity_type`, `key` the new type, `other` the new entity, `cause` from
            /// [`transform_reason`]); cancellable: it stays. Since ABI 1.6.
            ENTITY_TRANSFORM = 51,
            /// A death protection item (totem of undying) is about to save an entity from
            /// death ([`GameEvent`]: `entity`, `entity_type`); cancellable: it dies. Since
            /// ABI 1.6.
            ENTITY_RESURRECT = 52,
            /// A player is about to dye a sheep ([`GameEvent`]: `entity` the sheep, `player`,
            /// `key` the dye colour, e.g. `lime`); cancellable: the dye stays. Since ABI 1.6.
            SHEEP_DYE_WOOL = 53,
            /// An entity is about to drop an item on its own, e.g. a chicken's egg
            /// ([`GameEvent`]: `entity`, `entity_type`, `key` the item); cancellable: nothing
            /// drops. Since ABI 1.6.
            ENTITY_DROP_ITEM = 54,
            /// A command is about to change an attribute of a living entity ([`GameEvent`]:
            /// `entity`, `entity_type`, `key` the attribute, e.g. `minecraft:scale`, `cause`
            /// from [`attribute_change`], `text` the modifier id (empty for the base value),
            /// `amount` the new base value or the modifier's amount, `value` the modifier's
            /// [`attribute_op`]); cancellable: the command fails. Not for the transient
            /// modifiers of effects and equipment. Since ABI 1.13.
            ENTITY_ATTRIBUTE_CHANGE = 55,
            /// A block is broken and about to drop ([`GameEvent`]: `player` who broke it or
            /// [`Uuid::NIL`], `pos`, `state` the broken state, `item` the drops as an SNBT
            /// list, `item_out` replaces them, `value` the experience, changeable);
            /// cancellable: nothing drops. Since ABI 1.6.
            BLOCK_DROP = 60,
            /// A crop, stem, cactus, cane, sapling stage or similar grows ([`GameEvent`]:
            /// `pos`, `key` the old state, `state` the new one); cancellable. Since ABI 1.6.
            BLOCK_GROW = 61,
            /// A block forms (snow, ice, concrete, obsidian, cobblestone; [`GameEvent`] as
            /// [`event::BLOCK_GROW`]); cancellable. Since ABI 1.6.
            BLOCK_FORM = 62,
            /// A block spreads (grass, mycelium, vines, fire, sculk; [`GameEvent`] as
            /// [`event::BLOCK_GROW`], `extra` unused); cancellable. Since ABI 1.6.
            BLOCK_SPREAD = 63,
            /// A block fades (ice and snow melt, coral dies, fire goes out, farmland dries;
            /// [`GameEvent`] as [`event::BLOCK_GROW`]); cancellable. Since ABI 1.6.
            BLOCK_FADE = 64,
            /// Leaves decay ([`GameEvent`]: `pos`, `key` the leaves); cancellable. Since ABI 1.6.
            LEAVES_DECAY = 65,
            /// A fluid flows into a block ([`GameEvent`]: `from` the source block's centre,
            /// `pos` the block it flows into, `key` its old state, `state` the new one);
            /// cancellable. Since ABI 1.6.
            BLOCK_FLUID_FLOW = 66,
            /// Fire destroys a block ([`GameEvent`]: `pos`, `key` the burnt state);
            /// cancellable. Since ABI 1.6.
            BLOCK_BURN = 67,
            /// A block catches fire ([`GameEvent`]: `pos` the fire, `cause` from
            /// [`ignite_cause`], `entity` or `player` the cause when known); cancellable.
            /// Since ABI 1.6.
            BLOCK_IGNITE = 68,
            /// A player uses bone meal ([`GameEvent`]: `player`, `pos`); cancellable. Since
            /// ABI 1.6.
            BLOCK_FERTILIZE = 69,
            /// A sapling or mushroom grows into a tree or huge mushroom ([`GameEvent`]: `pos`,
            /// `key` the sapling state, `player` when bone meal did it); cancellable. Since
            /// ABI 1.6.
            STRUCTURE_GROW = 70,
            /// A piston extends (`cause` 1) or retracts (0) ([`GameEvent`]: `pos`, `state`,
            /// `value` its facing as `Direction` index); cancellable. Since ABI 1.6. `text`
            /// names the blocks it moves, then those it breaks, as `x,y,z;x,y,z` (empty when it
            /// moves none; servers before 0.1.0-beta.3 leave it empty).
            BLOCK_PISTON = 71,
            /// A player changes a sign's text ([`GameEvent`]: `player`, `pos`, `flags` 1 for
            /// the front side, `text` the lines as a JSON array of strings, `text_out`
            /// replaces them (same form)); cancellable. Since ABI 1.6.
            SIGN_CHANGE = 72,
            /// A dispenser or dropper is about to dispense ([`GameEvent`]: `pos`, `state`,
            /// `item` the stack); cancellable. Since ABI 1.6.
            BLOCK_DISPENSE = 73,
            /// A chunk was loaded into memory ([`GameEvent`]: `pos` the chunk's x and z in
            /// `x` and `z`, `flags` 1 when it was generated now). Since ABI 1.6.
            CHUNK_LOAD = 74,
            /// A chunk is about to be unloaded ([`GameEvent`]: `pos` as
            /// [`event::CHUNK_LOAD`]). Since ABI 1.6.
            CHUNK_UNLOAD = 75,
            /// A world is about to be saved ([`GameEvent`]: `world`). Since ABI 1.6.
            WORLD_SAVE = 76,
            /// A world's weather is about to change ([`GameEvent`]: `world`, `cause` the new
            /// [`weather`], `extra` the old one); cancellable. Since ABI 1.6.
            WEATHER_CHANGE = 77,
            /// A world's time is about to skip ahead by sleeping ([`GameEvent`]: `world`,
            /// `value` the ticks skipped, changeable); cancellable. Since ABI 1.6.
            TIME_SKIP = 78,
            /// A nether portal is about to be created ([`GameEvent`]: `pos` its lowest
            /// corner, `entity` the entity whose travel created it or [`Uuid::NIL`] when fire
            /// lit it); cancellable. Since ABI 1.6.
            PORTAL_CREATE = 79,
            /// A world was loaded: an extra world of `vanillite.toml` or `world_create` /
            /// `world_load` ([`GameEvent`]: `world`, `key` its dimension key, `text` its
            /// folder). Since ABI 1.8.
            WORLD_LOAD = 80,
            /// An extra world is about to be unloaded ([`GameEvent`] as
            /// [`event::WORLD_LOAD`], `flags` 1 when its chunks are saved); cancellable: it
            /// stays loaded. Since ABI 1.8.
            WORLD_UNLOAD = 81,
            /// A player's menu opens ([`GameEvent`]: `entity` the player, `key` the menu type,
            /// e.g. `minecraft:generic_9x3`, `value` the container id, `text` the title as JSON,
            /// `pos` the block when a block opened it, `flags` from [`menu_flag`]);
            /// cancellable: it stays closed. Since ABI 1.6.
            INVENTORY_OPEN = 90,
            /// A player clicks in its open menu ([`GameEvent`]: `entity`, `key` the menu type
            /// (`minecraft:inventory` for its own), `value` the slot (-999 outside), `extra`
            /// the button, `cause` from [`click`], `item` the slot's stack, `other_item` the
            /// cursor's, `flags` from [`menu_flag`]); cancellable: nothing changes and the
            /// client is corrected. Since ABI 1.6.
            INVENTORY_CLICK = 91,
            /// A player drags its cursor stack over slots ([`GameEvent`]: as
            /// [`event::INVENTORY_CLICK`], `text` the slots as a JSON array of numbers,
            /// `extra` the drag type: 0 split, 1 one each, 2 clone); cancellable. Since
            /// ABI 1.6.
            INVENTORY_DRAG = 92,
            /// A player's menu closed ([`GameEvent`]: as [`event::INVENTORY_OPEN`]). Since
            /// ABI 1.6.
            INVENTORY_CLOSE = 93,
            /// A crafting grid's result is computed ([`GameEvent`]: `entity` the player, `key`
            /// the recipe or empty, `item` the result, `item_out` replaces it). Since ABI 1.6.
            CRAFT_PREPARE = 94,
            /// A player takes a crafting result ([`GameEvent`]: as
            /// [`event::INVENTORY_CLICK`], `key` the recipe, `item` the result); cancellable.
            /// Since ABI 1.6.
            CRAFT_ITEM = 95,
            /// An anvil's result is computed ([`GameEvent`]: `entity` the player, `item` the
            /// result, `item_out` replaces it, `value` the level cost, changeable, `text` the
            /// new name). Since ABI 1.6.
            ANVIL_PREPARE = 96,
            /// A furnace, smoker or blast furnace finishes an item ([`GameEvent`]: `pos`,
            /// `state`, `other_item` the input, `item` the result, `item_out` replaces it);
            /// cancellable. Since ABI 1.6.
            FURNACE_SMELT = 97,
            /// A furnace starts burning fuel ([`GameEvent`]: `pos`, `state`, `item` the fuel,
            /// `value` the burn time in ticks, changeable); cancellable. Since ABI 1.6.
            FURNACE_BURN = 98,
            /// A player takes results out of a furnace ([`GameEvent`]: `entity`, `pos`,
            /// `item`, `value` the experience, changeable). Since ABI 1.6.
            FURNACE_EXTRACT = 99,
            /// A hopper or hopper minecart moves an item ([`GameEvent`]: `from` and `to` the
            /// containers' centres, `entity` the hopper minecart or [`Uuid::NIL`], `item` the
            /// stack moved); cancellable. Since ABI 1.6.
            INVENTORY_MOVE_ITEM = 100,
            /// A player enchants an item at an enchanting table ([`GameEvent`]: `entity`,
            /// `pos`, `item` the item, `value` the level cost, `text` the enchantments as SNBT);
            /// cancellable. Since ABI 1.6.
            ENCHANT_ITEM = 101,
            /// A brewing stand finishes brewing ([`GameEvent`]: `pos`, `item` the results as an
            /// SNBT list, `item_out` replaces them); cancellable. Since ABI 1.6.
            BREW = 102,
        }
        /// Flags of `subscribe_flags`. Since ABI 1.5.
        pub mod subscribe_flag: u32 {
            /// The handler is skipped while the event is cancelled (Bukkit's `ignoreCancelled`).
            IGNORE_CANCELLED = 1,
        }
        /// Event priorities: handlers run from the lowest to the highest value, then monitors.
        pub mod priority: i32 {
            /// First.
            LOWEST = -200,
            /// Early.
            LOW = -100,
            /// Default.
            NORMAL = 0,
            /// Late.
            HIGH = 100,
            /// Last that may change the event.
            HIGHEST = 200,
            /// Observes the outcome; must not change the event.
            MONITOR = 1000,
        }
        /// Owners a task runs on ([`Owner::kind`]).
        pub mod owner: u32 {
            /// The server thread between ticks (global pre-phase).
            GLOBAL = 0,
            /// A world, between the players' packets and the world's simulation.
            WORLD = 1,
            /// The owner of a block position (today its world; a region later).
            LOCATION = 2,
            /// The owner of an entity; the task ends when the entity is gone.
            ENTITY = 3,
            /// A background thread off the tick (I/O, databases). No world access.
            BACKGROUND = 4,
        }
        /// Command node kinds ([`CommandNode::kind`]).
        pub mod node: u32 {
            /// A literal word.
            LITERAL = 0,
            /// An argument.
            ARGUMENT = 1,
        }
        /// Command argument types ([`CommandNode::arg_type`], [`CommandArg::kind`]).
        pub mod arg: u32 {
            /// `true` or `false` ([`CommandArg::int`] 0 or 1).
            BOOL = 0,
            /// A 32-bit integer, bounds from `min`/`max` ([`CommandArg::int`]).
            INTEGER = 1,
            /// A 64-bit integer ([`CommandArg::int`]).
            LONG = 2,
            /// A 32-bit float ([`CommandArg::float`]).
            FLOAT = 3,
            /// A 64-bit float ([`CommandArg::float`]).
            DOUBLE = 4,
            /// One word ([`CommandArg::text`]).
            WORD = 5,
            /// A word or a quoted string ([`CommandArg::text`]).
            STRING = 6,
            /// The rest of the input ([`CommandArg::text`]).
            GREEDY_STRING = 7,
            /// One player: a name or a selector ([`CommandArg::players`], one entry).
            PLAYER = 8,
            /// Players: names or selectors ([`CommandArg::players`]).
            PLAYERS = 9,
            /// A block position, relative coordinates resolved ([`CommandArg::pos`]).
            BLOCK_POS = 10,
            /// One entity: a selector or a UUID ([`CommandArg::players`], one entry). Since
            /// ABI 1.5.
            ENTITY = 11,
            /// Entities: a selector or a UUID ([`CommandArg::players`]). Since ABI 1.5.
            ENTITIES = 12,
            /// A position, relative and local coordinates resolved ([`CommandArg::vec`]).
            /// Since ABI 1.5.
            VEC3 = 13,
            /// A rotation, relative values resolved ([`CommandArg::vec`]: `x` yaw, `y`
            /// pitch). Since ABI 1.5.
            ROTATION = 14,
            /// A resource location, e.g. `minecraft:stone` ([`CommandArg::text`], with its
            /// namespace). Since ABI 1.5.
            IDENTIFIER = 15,
            /// A block state with optional NBT, as typed ([`CommandArg::text`]). Since ABI 1.5.
            BLOCK_STATE = 16,
            /// An item with optional components, as typed ([`CommandArg::text`]), e.g.
            /// `diamond_sword[damage=5]`. Since ABI 1.5.
            ITEM_STACK = 17,
            /// A text component in SNBT or JSON, as typed ([`CommandArg::text`]). Since
            /// ABI 1.5.
            COMPONENT = 18,
            /// An SNBT compound, as typed ([`CommandArg::text`]). Since ABI 1.5.
            NBT_COMPOUND = 19,
            /// A game mode ([`CommandArg::int`], one of [`game_mode`]). Since ABI 1.5.
            GAME_MODE = 20,
            /// A duration like `5s` or `2d`, in ticks ([`CommandArg::int`]). Since ABI 1.5.
            TIME = 21,
            /// A UUID in its text form ([`CommandArg::text`]). Since ABI 1.5.
            UUID = 22,
        }
        /// Command senders ([`Sender::kind`]).
        pub mod sender: u32 {
            /// The server console.
            CONSOLE = 0,
            /// A player ([`Sender::player`]).
            PLAYER = 1,
            /// A command block or command block minecart. Since ABI 1.5.
            COMMAND_BLOCK = 2,
            /// An entity that is no player, e.g. through `execute as` ([`Sender::player`] holds
            /// its UUID). Since ABI 1.5.
            ENTITY = 3,
        }
        /// Answers of a permission provider.
        pub mod permission: u32 {
            /// Denied.
            DENY = 0,
            /// Granted.
            ALLOW = 1,
            /// Not decided: the server's default applies (operators and the console).
            UNDEFINED = 2,
        }
        /// Special values.
        pub mod limits: u32 {
            /// [`CommandNode::parent`] of top-level nodes.
            ROOT = u32::MAX,
            /// No world ([`Owner::world`], [`Sender::world`]).
            NO_WORLD = u32::MAX,
            /// A block state id that is no block: written by `blocks_get` for positions
            /// outside loaded chunks, skipped by `blocks_set`. Since ABI 1.2.
            NO_BLOCK = u32::MAX,
            /// The most blocks one `blocks_get` or `blocks_set` call covers. Since ABI 1.2.
            MAX_BOX_VOLUME = 1 << 24,
            /// The first event kind `event_register` hands out. Since ABI 1.5.
            FIRST_CUSTOM_EVENT = 192,
            /// The last event kind `event_register` hands out. Since ABI 1.5.
            LAST_CUSTOM_EVENT = 255,
            /// The longest plugin message in bytes (Vanilla's limit of client payloads).
            /// Since ABI 1.5.
            MAX_PLUGIN_MESSAGE = 32767,
            /// The most channels a plugin registers. Since ABI 1.5.
            MAX_CHANNELS = 128,
            /// The longest SNBT text the host parses (entity, item and block entity data).
            /// Since ABI 1.5.
            MAX_SNBT = 1 << 21,
            /// The most entries one `set_hosted_plugins` call reports. Since ABI 1.11.
            MAX_HOSTED_PLUGINS = 4096,
        }
        /// Why a login was refused ([`LoginEvent::reason`]). Since ABI 1.4.
        pub mod login_reason: u32 {
            /// Not refused.
            ALLOWED = 0,
            /// The player or its address is banned.
            BANNED = 1,
            /// The whitelist is on and the player is not on it.
            NOT_WHITELISTED = 2,
            /// The server is full.
            FULL = 3,
            /// Refused by a plugin.
            PLUGIN = 4,
        }
        /// What a player does with its item ([`InteractEvent::action`]). Since ABI 1.4.
        pub mod interact: u32 {
            /// Uses the item on a block (right click on a block).
            USE_ON_BLOCK = 0,
            /// Uses the item in the air (right click).
            USE_ITEM = 1,
            /// Hits a block (left click, start of breaking it; `pos`, `face`, `state` set).
            /// Since ABI 1.6.
            LEFT_CLICK_BLOCK = 2,
            /// Swings in the air (left click). Since ABI 1.6.
            LEFT_CLICK_AIR = 3,
            /// Steps on a pressure plate, tripwire or farmland (`pos`, `state` set; `item`
            /// empty). Since ABI 1.6.
            PHYSICAL = 4,
        }
        /// Hands ([`InteractEvent::hand`]). Since ABI 1.4.
        pub mod hand: u32 {
            /// The main hand.
            MAIN = 0,
            /// The off hand.
            OFF = 1,
        }
        /// Game modes ([`GameModeEvent`]). Since ABI 1.4.
        pub mod game_mode: u32 {
            /// Survival.
            SURVIVAL = 0,
            /// Creative.
            CREATIVE = 1,
            /// Adventure.
            ADVENTURE = 2,
            /// Spectator.
            SPECTATOR = 3,
        }
        /// Value types of persistent data, the NBT tag ids they are stored as. Numbers and
        /// arrays are little-endian, strings UTF-8. Since ABI 1.4.
        pub mod data_type: u32 {
            /// One byte (`i8`).
            BYTE = 1,
            /// `i16`.
            SHORT = 2,
            /// `i32`.
            INT = 3,
            /// `i64`.
            LONG = 4,
            /// `f32`.
            FLOAT = 5,
            /// `f64`.
            DOUBLE = 6,
            /// Bytes.
            BYTE_ARRAY = 7,
            /// A UTF-8 string.
            STRING = 8,
            /// `i32` values.
            INT_ARRAY = 11,
            /// `i64` values.
            LONG_ARRAY = 12,
        }
        /// Limits of persistent data. Since ABI 1.4.
        pub mod data_limits: u32 {
            /// The longest value in bytes.
            MAX_VALUE = 1 << 16,
            /// The most keys of one entity.
            MAX_KEYS = 256,
            /// The longest key in bytes.
            MAX_KEY = 256,
        }
        /// Sound categories ([`SoundSpec::source`], `player_stop_sound`), as `/playsound`'s
        /// sources. Since ABI 1.5.
        pub mod sound_source: u32 {
            /// `master`.
            MASTER = 0,
            /// `music`.
            MUSIC = 1,
            /// `record`.
            RECORD = 2,
            /// `weather`.
            WEATHER = 3,
            /// `block`.
            BLOCK = 4,
            /// `hostile`.
            HOSTILE = 5,
            /// `neutral`.
            NEUTRAL = 6,
            /// `player`.
            PLAYER = 7,
            /// `ambient`.
            AMBIENT = 8,
            /// `voice`.
            VOICE = 9,
            /// `ui`.
            UI = 10,
            /// Every category (`player_stop_sound` only).
            ANY = u32::MAX,
        }
        /// Properties of a player for `player_get` and `player_set`. Values are `f64`;
        /// flags are 0 or 1. Since ABI 1.5.
        pub mod player_prop: u32 {
            /// Health (0 kills the player).
            HEALTH = 1,
            /// Maximum health (the `max_health` attribute; read only).
            MAX_HEALTH = 2,
            /// Food level, 0 to 20.
            FOOD = 3,
            /// Saturation, 0 to the food level.
            SATURATION = 4,
            /// Exhaustion, 0 to 40.
            EXHAUSTION = 5,
            /// Experience level.
            XP_LEVEL = 6,
            /// Progress to the next level, 0 to 1.
            XP_PROGRESS = 7,
            /// Total experience points (read only).
            XP_TOTAL = 8,
            /// Game mode, one of [`game_mode`] (set: as `/gamemode`, with the change event).
            GAME_MODE = 9,
            /// Flying now.
            FLYING = 10,
            /// May fly.
            MAY_FLY = 11,
            /// Flying speed (Vanilla's default 0.05).
            FLY_SPEED = 12,
            /// Walking speed (Vanilla's default 0.1).
            WALK_SPEED = 13,
            /// The selected hotbar slot, 0 to 8.
            SELECTED_SLOT = 14,
            /// Air supply in ticks.
            AIR = 15,
            /// Ticks it still burns.
            FIRE_TICKS = 16,
            /// Ignored by the night skip: counts as sleeping while another player sleeps
            /// (Bukkit's `setSleepingIgnored`; not saved). Since ABI 1.12.
            SLEEPING_IGNORED = 17,
        }
        /// Flags of `entity_effect_add`. Since ABI 1.5.
        pub mod effect_flag: u32 {
            /// Ambient (as from a beacon).
            AMBIENT = 1,
            /// Without particles.
            HIDE_PARTICLES = 2,
            /// Without the icon in the inventory and HUD.
            HIDE_ICON = 4,
        }
        /// Scoreboard display slots (`objective_display`), Vanilla's `DisplaySlot` ids; the
        /// team sidebars follow as `SIDEBAR_TEAM_BLACK + colour index`. Since ABI 1.5.
        pub mod display_slot: u32 {
            /// The player list.
            LIST = 0,
            /// The sidebar.
            SIDEBAR = 1,
            /// Below the name.
            BELOW_NAME = 2,
            /// The sidebar of members of black teams (+ colour index up to 15, white).
            SIDEBAR_TEAM_BLACK = 3,
        }
        /// Properties of a boss bar for `bossbar_set`. Since ABI 1.5.
        pub mod bossbar_prop: u32 {
            /// The value, 0 to the maximum.
            VALUE = 0,
            /// The maximum, at least 1.
            MAX = 1,
            /// The colour, one of [`bossbar_color`].
            COLOR = 2,
            /// The style, one of [`bossbar_style`].
            STYLE = 3,
            /// Visible (0 or 1).
            VISIBLE = 4,
        }
        /// Boss bar colours. Since ABI 1.5.
        pub mod bossbar_color: u32 {
            /// Pink.
            PINK = 0,
            /// Blue.
            BLUE = 1,
            /// Red.
            RED = 2,
            /// Green.
            GREEN = 3,
            /// Yellow.
            YELLOW = 4,
            /// Purple.
            PURPLE = 5,
            /// White.
            WHITE = 6,
        }
        /// Boss bar styles. Since ABI 1.5.
        pub mod bossbar_style: u32 {
            /// One bar.
            PROGRESS = 0,
            /// 6 notches.
            NOTCHED_6 = 1,
            /// 10 notches.
            NOTCHED_10 = 2,
            /// 12 notches.
            NOTCHED_12 = 3,
            /// 20 notches.
            NOTCHED_20 = 4,
        }
        /// The server's access lists (`access_set`, `access_get`). Since ABI 1.5.
        pub mod access_list: u32 {
            /// Operators (`ops.json`, level 4 on add).
            OPERATOR = 0,
            /// The whitelist.
            WHITELIST = 1,
            /// Banned players.
            BAN = 2,
            /// Banned IP addresses (the address in `name`).
            IP_BAN = 3,
            /// Whether the whitelist is on (`add` switches it on, otherwise off).
            WHITELIST_ON = 4,
        }
        /// Who has a registered permission node when no provider decides. Since ABI 1.5.
        pub mod permission_default: u32 {
            /// Nobody.
            FALSE = 0,
            /// Everyone.
            TRUE = 1,
            /// Operators (the default of unregistered nodes).
            OP = 2,
            /// Everyone but operators.
            NOT_OP = 3,
        }
        /// Weather (`world_set_weather`). Since ABI 1.5.
        pub mod weather: u32 {
            /// Clear.
            CLEAR = 0,
            /// Rain.
            RAIN = 1,
            /// Thunder.
            THUNDER = 2,
        }
        /// Why a player is teleported ([`event::PLAYER_TELEPORT`]). Since ABI 1.6.
        pub mod teleport_cause: u32 {
            /// Anything else.
            UNKNOWN = 0,
            /// `/tp`, `/teleport`, `/spreadplayers` and similar commands.
            COMMAND = 1,
            /// A plugin (`entity_teleport`).
            PLUGIN = 2,
            /// An ender pearl.
            ENDER_PEARL = 3,
            /// A chorus fruit or another consumable that teleports.
            CONSUMABLE = 4,
            /// A spectator teleports to an entity.
            SPECTATE = 5,
            /// An end gateway.
            END_GATEWAY = 6,
            /// Dismounting a vehicle.
            DISMOUNT = 7,
        }
        /// Resource pack statuses ([`event::PLAYER_RESOURCE_PACK`]), Vanilla's action ids.
        /// Since ABI 1.6.
        pub mod resource_pack_status: u32 {
            /// Loaded.
            SUCCESSFULLY_LOADED = 0,
            /// The player declined it.
            DECLINED = 1,
            /// The download failed.
            FAILED_DOWNLOAD = 2,
            /// The player accepted it.
            ACCEPTED = 3,
            /// Downloaded.
            DOWNLOADED = 4,
            /// The URL is invalid.
            INVALID_URL = 5,
            /// Reloading failed.
            FAILED_RELOAD = 6,
            /// The client discarded it.
            DISCARDED = 7,
        }
        /// Why an entity spawns ([`event::ENTITY_SPAWN`]): Vanilla's `EntitySpawnReason` from
        /// 1 on. Since ABI 1.6.
        pub mod spawn_reason: u32 {
            /// Entities without a Vanilla reason (items, projectiles, experience orbs,
            /// lightning).
            DEFAULT = 0,
            /// Natural spawning.
            NATURAL = 1,
            /// World generation.
            CHUNK_GENERATION = 2,
            /// A monster spawner.
            SPAWNER = 3,
            /// A structure being placed.
            STRUCTURE = 4,
            /// Breeding.
            BREEDING = 5,
            /// Summoned by a mob (evoker vexes, ...).
            MOB_SUMMONED = 6,
            /// A jockey's rider.
            JOCKEY = 7,
            /// An event (cat and phantom spawning, wandering traders).
            EVENT = 8,
            /// A conversion (zombie villager cured, ...).
            CONVERSION = 9,
            /// Zombie reinforcements.
            REINFORCEMENT = 10,
            /// Triggered (golems built, ...).
            TRIGGERED = 11,
            /// Emptied from a bucket.
            BUCKET = 12,
            /// A spawn egg or another spawning item.
            SPAWN_ITEM_USE = 13,
            /// `/summon`.
            COMMAND = 14,
            /// A dispenser.
            DISPENSER = 15,
            /// A patrol.
            PATROL = 16,
            /// A trial spawner.
            TRIAL_SPAWNER = 17,
            /// Arrived from another world.
            DIMENSION_TRAVEL = 18,
            /// A plugin (`entity_spawn`).
            PLUGIN = 19,
        }
        /// Why a mob changes its target ([`event::ENTITY_TARGET`]). Since ABI 1.6.
        pub mod target_reason: u32 {
            /// Anything else.
            UNKNOWN = 0,
            /// The nearest player.
            CLOSEST_PLAYER = 1,
            /// The nearest entity of a kind it hunts.
            CLOSEST_ENTITY = 2,
            /// It was hurt by the target.
            RETALIATE = 3,
            /// Its owner was hurt by or hurt the target.
            OWNER = 4,
            /// It defends a village.
            DEFEND_VILLAGE = 5,
            /// It forgets its target (lost, dead or out of reach).
            FORGET = 6,
            /// Anger (memories, neutral mobs, raids).
            ANGER = 7,
            /// A plugin (`mob_set_target`).
            PLUGIN = 8,
        }
        /// Flags of an explosion ([`event::ENTITY_EXPLODE`], `explode`). Since ABI 1.6.
        pub mod explode_flag: u32 {
            /// It breaks blocks.
            BREAK_BLOCKS = 1,
            /// It sets fire.
            FIRE = 2,
        }
        /// Portals ([`event::ENTITY_PORTAL`]). Since ABI 1.6.
        pub mod portal_kind: u32 {
            /// A nether portal.
            NETHER = 0,
            /// An end portal.
            END = 1,
            /// An end gateway.
            END_GATEWAY = 2,
        }
        /// The environment of a world (Bukkit's `World.Environment`): its dimension type.
        /// Since ABI 1.8.
        pub mod environment: u32 {
            /// `minecraft:overworld`.
            NORMAL = 0,
            /// `minecraft:the_nether`.
            NETHER = 1,
            /// `minecraft:the_end`.
            THE_END = 2,
            /// Another dimension type (a data pack's).
            CUSTOM = 3,
        }
        /// Flags of [`WorldCreate`]. Since ABI 1.8.
        pub mod world_create_flag: u32 {
            /// `seed` is set; without it the seed is random.
            SEED = 1,
            /// No structures are generated.
            NO_STRUCTURES = 2,
        }
        /// Flags of [`WorldDetails`]. Since ABI 1.8.
        pub mod world_flag: u32 {
            /// The world is loaded.
            LOADED = 1,
            /// An extra world (own folder); otherwise a dimension of the default world.
            EXTRA = 2,
        }
        /// Flags of [`BlockTypeInfo`]. Since ABI 1.10.
        pub mod block_type_flag: u32 {
            /// An air block.
            AIR = 1,
            /// Blocks motion (`BlockState.blocksMotion`).
            SOLID = 2,
            /// A redstone conductor (a full, opaque block).
            OCCLUDING = 4,
            /// Ignited by lava.
            FLAMMABLE = 8,
            /// Fire can spread to it (ignite odds above 0).
            BURNABLE = 16,
            /// Falls when unsupported (`Fallable`).
            GRAVITY = 32,
            /// Has a block entity.
            BLOCK_ENTITY = 64,
            /// Replaceable when placing a block.
            REPLACEABLE = 128,
            /// A liquid.
            LIQUID = 256,
            /// Has an item.
            ITEM = 512,
        }
        /// Flags of [`ItemTypeInfo`]. Since ABI 1.10.
        pub mod item_type_flag: u32 {
            /// Has a default `food` component.
            EDIBLE = 1,
            /// Has a default `jukebox_playable` component (a music disc).
            RECORD = 2,
            /// Has a default `compostable` component.
            COMPOSTABLE = 4,
        }
        /// Why an entity turns into another ([`event::ENTITY_TRANSFORM`]). Since ABI 1.6.
        pub mod transform_reason: u32 {
            /// Anything else.
            UNKNOWN = 0,
            /// A zombie villager is cured.
            CURED = 1,
            /// A villager is killed by a zombie and becomes a zombie villager.
            INFECTION = 2,
            /// A zombie drowns, or a husk turns into a zombie.
            DROWNED = 3,
            /// A skeleton freezes into a stray.
            FROZEN = 4,
            /// Struck by lightning (pig, villager, mooshroom, creeper).
            LIGHTNING = 5,
            /// A piglin or hoglin zombifies in the overworld.
            PIGLIN_ZOMBIFIED = 6,
            /// A mooshroom is sheared.
            SHEARED = 7,
            /// A slime or magma cube splits.
            SPLIT = 8,
            /// A tadpole grows up.
            METAMORPHOSIS = 9,
        }
        /// What a player does with a fishing rod ([`event::PLAYER_FISH`]). Since ABI 1.6.
        pub mod fish_state: u32 {
            /// The rod is cast.
            FISHING = 0,
            /// The bobber pulls a hooked entity.
            CAUGHT_ENTITY = 1,
            /// A fish (or other fishing loot) is caught.
            CAUGHT_FISH = 2,
            /// The bobber is reeled in from the ground.
            IN_GROUND = 3,
            /// The bobber is reeled in with nothing.
            REEL_IN = 4,
        }
        /// What set a block on fire ([`event::BLOCK_IGNITE`]). Since ABI 1.6.
        pub mod ignite_cause: u32 {
            /// Anything else.
            UNKNOWN = 0,
            /// Flint and steel or a fire charge used by a player.
            FLINT_AND_STEEL = 1,
            /// Lava.
            LAVA = 2,
            /// Lightning.
            LIGHTNING = 3,
            /// Fire spreading.
            SPREAD = 4,
            /// A fireball.
            FIREBALL = 5,
            /// An explosion.
            EXPLOSION = 6,
            /// A burning projectile.
            ARROW = 7,
        }
        /// Click types of menus ([`event::INVENTORY_CLICK`]), Vanilla's ids. Since ABI 1.6.
        pub mod click: u32 {
            /// Picks up or puts down (left or right click).
            PICKUP = 0,
            /// Shift click.
            QUICK_MOVE = 1,
            /// A number key or the off-hand key (`extra` the hotbar slot, 40 the off hand).
            SWAP = 2,
            /// Middle click (creative).
            CLONE = 3,
            /// Drops (Q, with control the stack).
            THROW = 4,
            /// A drag step.
            QUICK_CRAFT = 5,
            /// Double click: collects the stack.
            PICKUP_ALL = 6,
        }
        /// Flags of menu events. Since ABI 1.6.
        pub mod menu_flag: u32 {
            /// The menu was opened by a plugin (`menu_open`).
            PLUGIN = 1,
        }
        /// Properties of any entity for `entity_get` and `entity_set` (players too). Values
        /// are `f64`; flags 0 or 1. Since ABI 1.6.
        pub mod entity_prop: u32 {
            /// Health of a living entity.
            HEALTH = 1,
            /// Its maximum health (read only; the `max_health` attribute).
            MAX_HEALTH = 2,
            /// Absorption hearts.
            ABSORPTION = 3,
            /// Ticks it still burns.
            FIRE_TICKS = 4,
            /// Ticks it froze in powder snow.
            FREEZE_TICKS = 5,
            /// Air supply in ticks.
            AIR = 6,
            /// Fall distance in blocks.
            FALL_DISTANCE = 7,
            /// Without gravity.
            NO_GRAVITY = 8,
            /// Silent.
            SILENT = 9,
            /// Glowing (the flag of its data, not the effect).
            GLOWING = 10,
            /// Invulnerable (except to creative players and the void).
            INVULNERABLE = 11,
            /// A mob without AI.
            NO_AI = 12,
            /// A mob that never despawns.
            PERSISTENT = 13,
            /// Its custom name is always shown.
            CUSTOM_NAME_VISIBLE = 14,
            /// Invisible (the shared flag, not the effect).
            INVISIBLE = 15,
            /// On the ground (read only).
            ON_GROUND = 16,
            /// The age of an animal or villager (negative: a baby).
            AGE = 17,
            /// Ticks it exists (read only for players).
            TICKS_LIVED = 18,
            /// Ticks before it may use a portal again.
            PORTAL_COOLDOWN = 19,
            /// Sneaking (read only).
            SNEAKING = 20,
            /// Sprinting (read only).
            SPRINTING = 21,
            /// The entity's network id in packets (`Entity.getId`, Bukkit's `getEntityId`;
            /// read only). Since ABI 1.12.
            NETWORK_ID = 22,
        }
        /// Attribute modifier operations (`entity_attribute_modifier`), as `/attribute`.
        /// Since ABI 1.6.
        pub mod attribute_op: u32 {
            /// `add_value`.
            ADD_VALUE = 0,
            /// `add_multiplied_base`.
            ADD_MULTIPLIED_BASE = 1,
            /// `add_multiplied_total`.
            ADD_MULTIPLIED_TOTAL = 2,
        }
        /// What [`event::ENTITY_ATTRIBUTE_CHANGE`] changes ([`GameEvent::cause`]). Since
        /// ABI 1.13.
        pub mod attribute_change: u32 {
            /// The base value is set (`base set`, `base reset`).
            BASE = 0,
            /// A modifier is added.
            MODIFIER_ADD = 1,
            /// A modifier is removed.
            MODIFIER_REMOVE = 2,
        }
        /// What a plugin goal controls ([`GoalSpec::flags`]), Vanilla's `Goal.Flag`. Since
        /// ABI 1.6.
        pub mod goal_flag: u32 {
            /// Movement.
            MOVE = 1,
            /// Looking.
            LOOK = 2,
            /// Jumping.
            JUMP = 4,
            /// The target (target selector goals).
            TARGET = 8,
        }
        /// What a plugin goal's tick does ([`GoalAction::actions`]). Since ABI 1.6.
        pub mod goal_action: u32 {
            /// Walk to `move_to` at `speed` (the navigation's path finding).
            MOVE = 1,
            /// Look at `look_at`.
            LOOK = 2,
            /// Jump.
            JUMP = 4,
            /// Target `target` ([`Uuid::NIL`] forgets the target).
            TARGET = 8,
            /// Stop moving.
            STOP = 16,
        }
        /// Which Vanilla goals `mob_goals_clear` removes. Since ABI 1.6.
        pub mod goal_clear: u32 {
            /// The goal selector's goals.
            GOALS = 1,
            /// The target selector's goals.
            TARGETS = 2,
        }
        /// What a ray stops at (`raytrace`). Since ABI 1.6.
        pub mod raytrace_flag: u32 {
            /// Block collision shapes.
            BLOCKS = 1,
            /// Fluids (sources and flowing).
            FLUIDS = 2,
            /// Entities that can be hit (not spectators).
            ENTITIES = 4,
        }
        /// What a ray hit ([`RayHit::kind`]). Since ABI 1.6.
        pub mod ray_hit: u32 {
            /// Nothing.
            MISS = 0,
            /// A block.
            BLOCK = 1,
            /// An entity.
            ENTITY = 2,
        }
        /// How far a chunk is loaded (`chunk_state`), Vanilla's `FullChunkStatus`. Since
        /// ABI 1.6.
        pub mod chunk_state: u32 {
            /// Not loaded.
            UNLOADED = 0,
            /// Loaded at the border: blocks readable, nothing ticks.
            FULL = 1,
            /// Blocks tick.
            BLOCK_TICKING = 2,
            /// Blocks and entities tick.
            ENTITY_TICKING = 3,
        }
        /// When a chunk populator runs ([`ChunkPopulatorProvider::stage`]). Since ABI 1.6.
        pub mod gen_stage: u32 {
            /// After the terrain, surface and carvers, before the features.
            AFTER_SURFACE = 1,
            /// After the features (trees, ores, structures' pieces), as Bukkit's
            /// `BlockPopulator`.
            AFTER_FEATURES = 2,
        }
        /// Protocol states of packets. Since ABI 1.7.
        pub mod packet_state: u32 {
            /// The first packet of a connection.
            HANDSHAKE = 0,
            /// The server list ping.
            STATUS = 1,
            /// Login, up to the client's acknowledgement.
            LOGIN = 2,
            /// Configuration (registries, resource packs).
            CONFIGURATION = 3,
            /// In a world.
            PLAY = 4,
        }
        /// Packet directions. Since ABI 1.7.
        pub mod packet_direction: u32 {
            /// From the server to a client.
            CLIENTBOUND = 0,
            /// From a client to the server.
            SERVERBOUND = 1,
        }
        /// Special packet ids. Since ABI 1.7.
        pub mod packet_id: i32 {
            /// [`PacketSpec::id`]: every packet of the state and direction.
            ANY = -1,
        }
        /// Flags of a packet listener ([`PacketSpec::flags`]). Since ABI 1.7.
        pub mod packet_listen: u32 {
            /// Clientbound only: the listener does not depend on the receiver. The host calls
            /// it once per encoded packet where the packet is encoded once for many receivers
            /// (then without a player, [`packet_event::BROADCAST`]); its decision holds for all
            /// of them, and a replacement is still one buffer shared by all. Broadcast
            /// listeners run before the listeners per receiver.
            BROADCAST = 1,
            /// The listener is skipped while the packet is cancelled.
            IGNORE_CANCELLED = 2,
        }
        /// Flags of a packet event ([`PacketEvent::flags`]). Since ABI 1.7.
        pub mod packet_event: u32 {
            /// The packet is encoded once for many receivers and this call decides for all of
            /// them: [`PacketEvent::player`] is nil and [`PacketEvent::connection`] 0.
            BROADCAST = 1,
            /// An earlier listener replaced the body.
            REPLACED = 2,
        }
        /// Flags of `packet_send` and `packet_receive`. Since ABI 1.7.
        pub mod packet_send: u32 {
            /// The packet passes no packet listener.
            SKIP_LISTENERS = 1,
        }
        /// Limits of the packet interface. Since ABI 1.7.
        pub mod packet_limits: u32 {
            /// The longest packet body in bytes (Vanilla's limit of an uncompressed packet).
            MAX_BODY = 1 << 23,
        }
        /// How `block_set_id` and `blocks_set` change blocks. Since ABI 1.2.
        pub mod set_mode: u32 {
            /// Like `block_set`: Vanilla's `setBlock` with neighbour and shape updates.
            UPDATE = 0,
            /// Without neighbour and shape updates (no physics); clients still see the change.
            NO_PHYSICS = 1,
        }
    }
    structs {
        /// A borrowed UTF-8 string, valid for the call.
        pub struct Str {
            /// First byte (may dangle when `len` is 0).
            pub ptr: *const u8,
            /// Length in bytes.
            pub len: usize,
        }
        /// Borrowed bytes, valid for the call.
        pub struct Bytes {
            /// First byte (may dangle when `len` is 0).
            pub ptr: *const u8,
            /// Length in bytes.
            pub len: usize,
        }
        /// A caller-owned output buffer for a string. The callee writes up to `cap` bytes and
        /// sets `len` to the full length; when `len > cap` the caller retries with more room.
        pub struct StrBuf {
            /// The buffer.
            pub ptr: *mut u8,
            /// Its capacity in bytes.
            pub cap: usize,
            /// Set by the callee: the full length of the string.
            pub len: usize,
        }
        /// A UUID as two big-endian halves (`hi` holds the first 8 bytes). Players and entities
        /// are identified by it; a UUID that is no longer present fails with
        /// [`status::NOT_FOUND`].
        pub struct Uuid {
            /// Most significant bits.
            pub hi: u64,
            /// Least significant bits.
            pub lo: u64,
        }
        /// A block position.
        pub struct BlockPos {
            /// X.
            pub x: i32,
            /// Y.
            pub y: i32,
            /// Z.
            pub z: i32,
        }
        /// A position.
        pub struct Vec3 {
            /// X.
            pub x: f64,
            /// Y.
            pub y: f64,
            /// Z.
            pub z: f64,
        }
        /// A box of block positions, both corners inclusive (`min` ≤ `max` on every axis).
        /// Block arrays over a box are indexed `((y - min.y) * size_z + (z - min.z)) * size_x +
        /// (x - min.x)` (as [`GenChunk`]). Since ABI 1.2.
        pub struct BlockBox {
            /// The lowest corner.
            pub min: BlockPos,
            /// The highest corner.
            pub max: BlockPos,
        }
        /// What a plugin library reports about itself before any other of its code runs.
        pub struct PluginInfo {
            /// Size of this record in bytes.
            pub size: u32,
            /// [`ABI_MAJOR`] the plugin was built against.
            pub abi_major: u16,
            /// [`ABI_MINOR`] the plugin was built against (the minimum it needs).
            pub abi_minor: u16,
            /// One of [`panic_strategy`]; only `UNWIND` is accepted.
            pub panic_strategy: u32,
            /// Data version of the Minecraft release a provider was built for; 0 when the
            /// plugin provides no system (providers work with numeric block state ids).
            pub data_version: i32,
            /// Version of the SDK that built the plugin.
            pub sdk_version: Str,
            /// Target triple the library was built for, e.g. `x86_64-unknown-linux-gnu`.
            pub target: Str,
            /// Registers commands, events and providers. Registries are writable only here.
            pub load: LoadFn,
            /// Starts the plugin after the worlds exist.
            pub enable: LifecycleFn,
            /// Stops the plugin at shutdown or before a development reload.
            pub disable: LifecycleFn,
            /// Development reload: writes the state the next build takes over.
            pub save_state: Option<SaveStateFn>,
            /// Development reload: takes over the state of the previous build.
            pub restore_state: Option<RestoreStateFn>,
        }
        /// A receiver of bytes.
        pub struct ByteSink {
            /// Size of this record in bytes.
            pub size: u32,
            /// Passed to `write`.
            pub state: *mut c_void,
            /// Receives a chunk of bytes; may be called several times.
            pub write: ByteSinkFn,
        }
        /// The start of every event record.
        pub struct EventHeader {
            /// Size of the whole event record in bytes.
            pub size: u32,
            /// One of [`event`].
            pub kind: u32,
            /// Non-zero when cancelled. Handlers of cancellable events may set or clear it.
            pub cancelled: u32,
        }
        /// [`event::PLAYER_JOIN`] and [`event::PLAYER_LEAVE`].
        pub struct PlayerEvent {
            /// Header.
            pub header: EventHeader,
            /// The player.
            pub player: Uuid,
            /// Its name.
            pub name: Str,
            /// Its world.
            pub world: u32,
            /// The join or leave message as a JSON text component (empty for none). Since
            /// ABI 1.5.
            pub message: Str,
            /// Replaces the join or leave message; an empty text shows none. Since ABI 1.5.
            pub message_out: *const TextOut,
        }
        /// [`event::PLAYER_CHAT`] and [`event::PLAYER_COMMAND`].
        pub struct PlayerTextEvent {
            /// Header.
            pub header: EventHeader,
            /// The player.
            pub player: Uuid,
            /// Its name.
            pub name: Str,
            /// The message, or the command line without `/`.
            pub text: Str,
            /// Replaces the text: for chat what the players see (format from
            /// [`text_format`]; the message stays signed, the new text is sent as its unsigned
            /// content as Vanilla's chat decoration does), for commands the command line that
            /// runs (plain). Since ABI 1.6.
            pub text_out: *const TextOut,
        }
        /// One move of [`PlayersMovedEvent`].
        pub struct PlayerMove {
            /// The player.
            pub player: Uuid,
            /// Position at the start of the tick.
            pub from: Vec3,
            /// Position after its movement packets.
            pub to: Vec3,
        }
        /// [`event::PLAYERS_MOVED`]: all moves of one world in one tick.
        pub struct PlayersMovedEvent {
            /// Header.
            pub header: EventHeader,
            /// The world.
            pub world: u32,
            /// The moves (`moves[i]` at `moves + i * stride` bytes).
            pub moves: *const PlayerMove,
            /// Number of moves.
            pub count: usize,
            /// Size of one move record in bytes.
            pub stride: usize,
        }
        /// [`event::ENTITY_DAMAGE`].
        pub struct EntityDamageEvent {
            /// Header.
            pub header: EventHeader,
            /// The world.
            pub world: u32,
            /// The entity.
            pub entity: Uuid,
            /// Its type, e.g. `minecraft:player`.
            pub entity_type: Str,
            /// The damage type, e.g. `minecraft:fall`.
            pub damage_type: Str,
            /// The damage after armour-independent scaling; handlers may change it.
            pub amount: f32,
            /// The entity responsible (the shooter of a projectile; [`Uuid::NIL`] for none).
            /// Since ABI 1.6.
            pub attacker: Uuid,
            /// The entity that hit directly (the projectile; [`Uuid::NIL`] for none). Since
            /// ABI 1.6.
            pub direct: Uuid,
        }
        /// [`event::BLOCK_BREAK`], [`event::BLOCK_PLACE`], [`event::PLAYER_BED_ENTER`],
        /// [`event::PLAYER_BED_LEAVE`] and [`event::PLAYER_SPAWN_CHANGE`].
        pub struct BlockEvent {
            /// Header.
            pub header: EventHeader,
            /// The world.
            pub world: u32,
            /// The player.
            pub player: Uuid,
            /// The block.
            pub pos: BlockPos,
            /// The block state broken, or the one to be placed, e.g. `minecraft:oak_log[axis=y]`.
            pub state: Str,
        }
        /// Where a handler writes a replacement text. Since ABI 1.4.
        pub struct TextOut {
            /// Size of this record in bytes.
            pub size: u32,
            /// Passed to `set`.
            pub state: *mut c_void,
            /// Replaces the text; the last call wins.
            pub set: TextSetFn,
        }
        /// [`event::PLAYER_LOGIN`]. Since ABI 1.4.
        pub struct LoginEvent {
            /// Header; `cancelled` is non-zero while the login is refused.
            pub header: EventHeader,
            /// The player.
            pub player: Uuid,
            /// Its name.
            pub name: Str,
            /// Its IP address.
            pub address: Str,
            /// Why the server refused it, one of [`login_reason`] (`ALLOWED` when not).
            pub reason: u32,
            /// The refusal message as a JSON text component; empty when not refused.
            pub message: Str,
            /// Replaces the refusal message.
            pub message_out: *const TextOut,
        }
        /// [`event::PLAYER_DEATH`]. Since ABI 1.4.
        pub struct PlayerDeathEvent {
            /// Header (not cancellable).
            pub header: EventHeader,
            /// The world.
            pub world: u32,
            /// The player.
            pub player: Uuid,
            /// The damage type that killed it, e.g. `minecraft:fall`.
            pub damage_type: Str,
            /// The entity responsible ([`Uuid::NIL`] for none).
            pub killer: Uuid,
            /// The death message as a JSON text component; empty when none is shown.
            pub message: Str,
            /// Replaces the death message; an empty text shows none.
            pub message_out: *const TextOut,
            /// Non-zero when the player keeps its inventory and experience; handlers may
            /// change it.
            pub keep_inventory: u32,
        }
        /// [`event::ENTITY_DEATH`]. Since ABI 1.4.
        pub struct EntityDeathEvent {
            /// Header (not cancellable).
            pub header: EventHeader,
            /// The world.
            pub world: u32,
            /// The entity.
            pub entity: Uuid,
            /// Its type, e.g. `minecraft:zombie`.
            pub entity_type: Str,
            /// The damage type that killed it.
            pub damage_type: Str,
            /// The entity responsible ([`Uuid::NIL`] for none).
            pub killer: Uuid,
        }
        /// [`event::PLAYER_RESPAWN`] and [`event::PLAYER_CHANGE_WORLD`]. Since ABI 1.4.
        pub struct PlayerWorldEvent {
            /// Header (not cancellable).
            pub header: EventHeader,
            /// The player.
            pub player: Uuid,
            /// The world it came from.
            pub from: u32,
            /// The world it is in now.
            pub world: u32,
            /// Its position there.
            pub pos: Vec3,
        }
        /// [`event::PLAYER_GAME_MODE_CHANGE`]. Since ABI 1.4.
        pub struct GameModeEvent {
            /// Header.
            pub header: EventHeader,
            /// The player's world.
            pub world: u32,
            /// The player.
            pub player: Uuid,
            /// Its game mode, one of [`game_mode`].
            pub from: u32,
            /// The new game mode.
            pub to: u32,
        }
        /// [`event::PLAYER_INTERACT`]. Since ABI 1.4.
        pub struct InteractEvent {
            /// Header.
            pub header: EventHeader,
            /// The world.
            pub world: u32,
            /// The player.
            pub player: Uuid,
            /// One of [`interact`].
            pub action: u32,
            /// One of [`hand`].
            pub hand: u32,
            /// The item in that hand, e.g. `minecraft:flint_and_steel` (`minecraft:air` for none).
            pub item: Str,
            /// The block used (`USE_ON_BLOCK`).
            pub pos: BlockPos,
            /// The clicked face (`USE_ON_BLOCK`): 0 down, 1 up, 2 north, 3 south, 4 west, 5 east.
            pub face: u32,
            /// The block state used (`USE_ON_BLOCK`; empty otherwise).
            pub state: Str,
        }
        /// A title for `player_send_title`. Since ABI 1.4.
        pub struct TitleSpec {
            /// Size of this record in bytes.
            pub size: u32,
            /// The title; empty for none.
            pub title: Str,
            /// The subtitle; empty for none.
            pub subtitle: Str,
            /// Format of both, from [`text_format`].
            pub format: u32,
            /// Ticks to fade in.
            pub fade_in: i32,
            /// Ticks to stay.
            pub stay: i32,
            /// Ticks to fade out.
            pub fade_out: i32,
        }
        /// [`event::PLAYER_PLUGIN_MESSAGE`]. Since ABI 1.5.
        pub struct PluginMessageEvent {
            /// Header (not cancellable).
            pub header: EventHeader,
            /// The player's world.
            pub world: u32,
            /// The player.
            pub player: Uuid,
            /// The channel, e.g. `myplugin:sync`.
            pub channel: Str,
            /// The payload.
            pub data: Bytes,
        }
        /// [`event::PLAYER_ADVANCEMENT_DONE`]. Since ABI 1.5.
        pub struct AdvancementEvent {
            /// Header (not cancellable).
            pub header: EventHeader,
            /// The player's world.
            pub world: u32,
            /// The player.
            pub player: Uuid,
            /// The advancement, e.g. `minecraft:story/mine_stone`.
            pub advancement: Str,
        }
        /// An event of a kind from `event_register`, fired by a plugin with `event_fire`.
        /// Since ABI 1.5.
        pub struct CustomEvent {
            /// Header; cancellable.
            pub header: EventHeader,
            /// The name the kind was registered with.
            pub name: Str,
            /// The id of the plugin that fired it.
            pub source: Str,
            /// The payload, in a format the plugins agree on.
            pub data: Bytes,
        }
        /// A sound for `player_play_sound`. Since ABI 1.5.
        pub struct SoundSpec {
            /// Size of this record in bytes.
            pub size: u32,
            /// The sound event, e.g. `minecraft:entity.experience_orb.pickup` (also sounds of
            /// resource packs).
            pub sound: Str,
            /// One of [`sound_source`].
            pub source: u32,
            /// Where it plays.
            pub pos: Vec3,
            /// Volume (1 is normal; above 1 widens its range).
            pub volume: f32,
            /// Pitch, 0.5 to 2.
            pub pitch: f32,
            /// Seed of the sound's variant choice.
            pub seed: i64,
        }
        /// Particles for `player_spawn_particle`. Since ABI 1.5.
        pub struct ParticleSpec {
            /// Size of this record in bytes.
            pub size: u32,
            /// The particle as `/particle` takes it: its id with options as SNBT, e.g.
            /// `minecraft:dust{color:[1.0,0.0,0.0],scale:1.0}`.
            pub particle: Str,
            /// Where they appear.
            pub pos: Vec3,
            /// The spread on each axis (with `count` 0: the direction of motion).
            pub delta: Vec3,
            /// Speed.
            pub speed: f32,
            /// Count (0: one particle moving along `delta`).
            pub count: i32,
            /// Non-zero to show them up to 512 blocks away (`force`), else 32.
            pub force: u32,
        }
        /// What `player_info` reports about an online player. Since ABI 1.5.
        pub struct PlayerInfo {
            /// Size of this record in bytes.
            pub size: u32,
            /// Its world.
            pub world: u32,
            /// Its game mode, one of [`game_mode`].
            pub game_mode: u32,
            /// Its operator level, 0 for none.
            pub op_level: u32,
            /// Its latency in milliseconds, as the player list shows it.
            pub ping: i32,
        }
        /// What `server_stats` reports. Since ABI 1.5.
        pub struct ServerStats {
            /// Size of this record in bytes.
            pub size: u32,
            /// The server's tick count.
            pub tick: u64,
            /// Average milliseconds per tick over the last 100 ticks.
            pub mspt: f64,
            /// Ticks per second over the last 100 ticks (at most the tick rate).
            pub tps: f64,
            /// The tick rate (20 unless changed with `/tick rate`).
            pub tick_rate: f64,
            /// Online players.
            pub online: u32,
            /// The player limit.
            pub max_players: u32,
        }
        /// A place in a world. Since ABI 1.5.
        pub struct Location {
            /// The world.
            pub world: u32,
            /// The position.
            pub pos: Vec3,
            /// Yaw in degrees.
            pub yaw: f32,
            /// Pitch in degrees.
            pub pitch: f32,
        }
        /// Where a task runs.
        pub struct Owner {
            /// One of [`owner`].
            pub kind: u32,
            /// The world (`WORLD`, `LOCATION`, `ENTITY`).
            pub world: u32,
            /// The position (`LOCATION`).
            pub pos: BlockPos,
            /// The entity (`ENTITY`).
            pub entity: Uuid,
        }
        /// A task to schedule.
        pub struct TaskSpec {
            /// Size of this record in bytes.
            pub size: u32,
            /// Where it runs.
            pub owner: Owner,
            /// Ticks before the first run (0 = the owner's next turn).
            pub delay: u64,
            /// Ticks between runs; 0 runs it once. Background tasks repeat on a background
            /// thread for plugins built against ABI 1.4 or later (before, it is ignored).
            pub period: u64,
            /// The task.
            pub run: TaskFn,
            /// Frees `user` when the task is finished or cancelled.
            pub drop: Option<DropFn>,
            /// Passed to `run` and `drop`.
            pub user: *mut c_void,
            /// `ENTITY` tasks: runs once instead of `run` when the task is due and its entity
            /// is gone (removed, dead or unloaded; Folia's "retired" callback), on the world
            /// it was in, then `drop`. Since ABI 1.6.
            pub retired: Option<TaskFn>,
        }
        /// One node of a command tree.
        pub struct CommandNode {
            /// Index of the parent in the node array, or [`limits::ROOT`]. Parents come first.
            pub parent: u32,
            /// One of [`node`].
            pub kind: u32,
            /// The literal, or the argument's name.
            pub name: Str,
            /// One of [`arg`] (arguments only).
            pub arg_type: u32,
            /// Non-zero when `min` applies.
            pub has_min: u32,
            /// Lower bound of numeric arguments.
            pub min: f64,
            /// Non-zero when `max` applies.
            pub has_max: u32,
            /// Upper bound of numeric arguments.
            pub max: f64,
            /// Permission level needed (0 = everyone .. 4 = owners).
            pub level: u32,
            /// Permission node needed besides the level; empty for none.
            pub permission: Str,
            /// The plugin's number of the command that runs when the input ends here; 0 = none.
            pub executes: u32,
            /// The plugin's number of the suggestion provider of this argument; 0 = none.
            pub suggests: u32,
        }
        /// A command tree to add to the server's command tree.
        pub struct CommandSpec {
            /// Size of this record in bytes.
            pub size: u32,
            /// The nodes (`nodes[i]` at `nodes + i * stride` bytes).
            pub nodes: *const CommandNode,
            /// Number of nodes.
            pub count: usize,
            /// Size of one node record in bytes.
            pub stride: usize,
            /// Runs the commands of this tree.
            pub execute: CommandFn,
            /// Answers the suggestion providers of this tree.
            pub suggest: Option<SuggestFn>,
            /// Passed to `execute` and `suggest`; never freed (commands live as long as the server).
            pub user: *mut c_void,
        }
        /// Who runs a command.
        pub struct Sender {
            /// One of [`sender`].
            pub kind: u32,
            /// The player (`PLAYER`).
            pub player: Uuid,
            /// The sender's world, or [`limits::NO_WORLD`].
            pub world: u32,
            /// The sender's position.
            pub pos: Vec3,
        }
        /// A parsed command argument.
        pub struct CommandArg {
            /// The argument's name.
            pub name: Str,
            /// One of [`arg`].
            pub kind: u32,
            /// Integer and boolean values.
            pub int: i64,
            /// Float values.
            pub float: f64,
            /// String values.
            pub text: Str,
            /// Block positions.
            pub pos: BlockPos,
            /// Players selected by `PLAYER` and `PLAYERS` arguments.
            pub players: *const Uuid,
            /// Number of players.
            pub player_count: usize,
            /// Positions and rotations (`VEC3`, `ROTATION`). Since ABI 1.5.
            pub vec: Vec3,
        }
        /// A command to run.
        pub struct CommandInvocation {
            /// Size of this record in bytes.
            pub size: u32,
            /// The plugin's number of the command ([`CommandNode::executes`]).
            pub command: u32,
            /// Who runs it.
            pub sender: Sender,
            /// The whole command line without `/`.
            pub line: Str,
            /// The arguments (`args[i]` at `args + i * stride` bytes).
            pub args: *const CommandArg,
            /// Number of arguments.
            pub arg_count: usize,
            /// Size of one argument record in bytes.
            pub arg_stride: usize,
        }
        /// A request for suggestions of a custom provider.
        pub struct SuggestRequest {
            /// Size of this record in bytes.
            pub size: u32,
            /// The plugin's number of the provider ([`CommandNode::suggests`]).
            pub provider: u32,
            /// Who asks.
            pub sender: Sender,
            /// The text of the argument typed so far.
            pub remaining: Str,
            /// The arguments parsed before this one.
            pub args: *const CommandArg,
            /// Number of arguments.
            pub arg_count: usize,
            /// Size of one argument record in bytes.
            pub arg_stride: usize,
        }
        /// Where suggestions go.
        pub struct SuggestSink {
            /// Size of this record in bytes.
            pub size: u32,
            /// Passed to `add`.
            pub state: *mut c_void,
            /// Adds one suggestion; the host filters by the typed prefix and the budget.
            pub add: SuggestAddFn,
        }
        /// The world a generator is created for.
        pub struct WorldInfo {
            /// Size of this record in bytes.
            pub size: u32,
            /// Dimension key, e.g. `minecraft:overworld`.
            pub dimension: Str,
            /// Lowest block Y.
            pub min_y: i32,
            /// Height in blocks.
            pub height: u32,
            /// World seed.
            pub seed: i64,
        }
        /// One chunk of a generator batch: a dense block array, pre-filled with air.
        pub struct GenChunk {
            /// Chunk X.
            pub x: i32,
            /// Chunk Z.
            pub z: i32,
            /// Numeric block state ids of the provider's data version, index
            /// `((y - min_y) * 16 + z) * 16 + x` with `x`, `z` in `0..16`.
            pub blocks: *mut u32,
            /// Number of entries (`16 * 16 * height`).
            pub len: usize,
        }
        /// A chunk generator provider (the `chunk-generator` slot). Called from many threads
        /// at once: instances must be thread-safe.
        pub struct ChunkGeneratorProvider {
            /// Size of this record in bytes.
            pub size: u32,
            /// Data version of the block state ids the provider writes.
            pub data_version: i32,
            /// Passed to `create`; never freed.
            pub user: *mut c_void,
            /// Creates the generator of a world.
            pub create: GeneratorCreateFn,
            /// Generates the terrain of a batch of chunks.
            pub generate: GeneratorRunFn,
            /// Destroys an instance.
            pub destroy: GeneratorDestroyFn,
        }
        /// The record of the event kinds added since ABI 1.6. Which fields a kind fills, and
        /// which its handlers may change, is documented at the kind; the others are zero,
        /// empty or [`Uuid::NIL`]. Handlers get a context that owns the event's world only
        /// where its kind says so; elsewhere they change the record or schedule a task.
        pub struct GameEvent {
            /// Header.
            pub header: EventHeader,
            /// The world.
            pub world: u32,
            /// The player or entity the event is about.
            pub entity: Uuid,
            /// Its type, e.g. `minecraft:zombie`.
            pub entity_type: Str,
            /// A second entity: the target, vehicle, owner, parent, killer or item entity.
            pub other: Uuid,
            /// Its type.
            pub other_type: Str,
            /// A third entity: the direct one (projectile), the other parent, the shooter.
            pub source: Uuid,
            /// The player behind it (who broke, bred, ignited).
            pub player: Uuid,
            /// A code of the kind: a reason, cause or action.
            pub cause: u32,
            /// An identifier of the kind: an item, statistic, menu type, recipe or old state.
            pub key: Str,
            /// A block.
            pub pos: BlockPos,
            /// A block state, e.g. `minecraft:wheat[age=7]`.
            pub state: Str,
            /// Where from.
            pub from: Location,
            /// Where to; changeable where the kind says so.
            pub to: Location,
            /// An item stack (`{id:"minecraft:stone",count:3}`) or a list of stacks, as SNBT.
            pub item: Str,
            /// A second item stack as SNBT (the cursor, the input, the off hand).
            pub other_item: Str,
            /// Replaces `item` (SNBT; the format argument is ignored).
            pub item_out: *const TextOut,
            /// A text (JSON text component unless the kind says otherwise).
            pub text: Str,
            /// Replaces `text`.
            pub text_out: *const TextOut,
            /// A number: a radius, a damage; changeable where the kind says so.
            pub amount: f64,
            /// An integer: a slot, a count, an experience; changeable where the kind says so.
            pub value: i64,
            /// A second integer: the previous value, a button.
            pub extra: i64,
            /// Flags of the kind.
            pub flags: u32,
        }
        /// A new extra world for `world_create` (Bukkit's `WorldCreator`). Since ABI 1.8.
        pub struct WorldCreate {
            /// Size of this record in bytes.
            pub size: u32,
            /// Its name and folder (letters, digits, `_`, `-`, `.`); the dimension key is
            /// `minecraft:<name in lower case>`.
            pub name: Str,
            /// One of [`environment`] (not `CUSTOM`).
            pub environment: u32,
            /// `normal`, `flat`, `void`, `amplified`, `large_biomes` or the key of a
            /// dimension whose generator it takes; empty for `normal`.
            pub generator: Str,
            /// The seed with [`world_create_flag::SEED`].
            pub seed: i64,
            /// Flags from [`world_create_flag`].
            pub flags: u32,
        }
        /// An entity as `entities_in_box` reports it. Since ABI 1.9.
        pub struct EntityInfo {
            /// Its UUID.
            pub uuid: Uuid,
            /// Its position.
            pub pos: Vec3,
            /// Yaw in degrees.
            pub yaw: f32,
            /// Pitch in degrees.
            pub pitch: f32,
            /// The numeric id of its type (`entity_type_name` names it).
            pub kind: u32,
        }
        /// Static facts of a block type, as `block_type_info` reports them. Since ABI 1.10.
        pub struct BlockTypeInfo {
            /// Size of this record in bytes.
            pub size: u32,
            /// Its default block state id.
            pub default_state: u32,
            /// The number of its block states.
            pub state_count: u32,
            /// Flags from [`block_type_flag`] (of the default state).
            pub flags: u32,
            /// Destroy time (`-1` for unbreakable blocks).
            pub hardness: f32,
            /// Explosion resistance.
            pub blast_resistance: f32,
            /// Friction (slipperiness).
            pub friction: f32,
            /// Speed factor of entities on it.
            pub speed_factor: f32,
            /// Jump factor of entities on it.
            pub jump_factor: f32,
        }
        /// Static facts of an item type, as `item_type_info` reports them. Since ABI 1.10.
        pub struct ItemTypeInfo {
            /// Size of this record in bytes.
            pub size: u32,
            /// The default maximum stack size.
            pub max_stack_size: u32,
            /// The default durability, 0 without one.
            pub max_damage: u32,
            /// The default block state of the block it places, `u32::MAX` for none.
            pub block_state: u32,
            /// Flags from [`item_type_flag`] (of its default components).
            pub flags: u32,
            /// The default rarity: 0 common, 1 uncommon, 2 rare, 3 epic.
            pub rarity: u32,
            /// The `EquipmentSlot` ordinal of the default `equippable` component (mainhand,
            /// offhand, feet, legs, chest, head, body, saddle), `u32::MAX` for none.
            pub equip_slot: u32,
        }
        /// A plugin another plugin hosts (the Paper/Spigot layer's Bukkit plugins), for
        /// `/plugins`, `/version <plugin>` and Query. The host copies every string; names,
        /// versions, groups and websites are at most 256 bytes, descriptions and authors 4 KiB.
        /// Since ABI 1.11.
        pub struct HostedPlugin {
            /// Size of this record in bytes.
            pub size: u32,
            /// 1 when the hosted plugin is enabled.
            pub enabled: u32,
            /// The section `/plugins` lists it under, e.g. `Bukkit` or `Paper`.
            pub group: Str,
            /// Its name.
            pub name: Str,
            /// Its version.
            pub version: Str,
            /// Its description; may be empty.
            pub description: Str,
            /// Its authors, separated by `, `; may be empty.
            pub authors: Str,
            /// Its website; may be empty.
            pub website: Str,
        }
        /// What `world_info` tells about a world. Since ABI 1.8.
        pub struct WorldDetails {
            /// Size of this record in bytes.
            pub size: u32,
            /// One of [`environment`].
            pub environment: u32,
            /// Flags from [`world_flag`].
            pub flags: u32,
            /// Its seed.
            pub seed: i64,
        }
        /// A world border for `border_get` and `border_set`, as `/worldborder` shows it.
        /// Since ABI 1.6.
        pub struct BorderInfo {
            /// Size of this record in bytes.
            pub size: u32,
            /// Centre X.
            pub center_x: f64,
            /// Centre Z.
            pub center_z: f64,
            /// The diameter now.
            pub diameter: f64,
            /// The diameter it grows or shrinks to (`set`: equal to `diameter` for none).
            pub target: f64,
            /// Ticks until it reaches `target`.
            pub lerp_ticks: i64,
            /// Damage per block outside the safe zone per second.
            pub damage_per_block: f64,
            /// Blocks outside the border without damage.
            pub safe_zone: f64,
            /// Warning distance in blocks.
            pub warning_blocks: i32,
            /// Warning time in seconds.
            pub warning_time: i32,
        }
        /// What a ray hit (`raytrace`). Since ABI 1.6.
        pub struct RayHit {
            /// Size of this record in bytes.
            pub size: u32,
            /// One of [`ray_hit`].
            pub kind: u32,
            /// The point hit.
            pub pos: Vec3,
            /// The block hit (`BLOCK`).
            pub block: BlockPos,
            /// The face hit (`BLOCK`): 0 down, 1 up, 2 north, 3 south, 4 west, 5 east.
            pub face: u32,
            /// The entity hit (`ENTITY`).
            pub entity: Uuid,
        }
        /// A menu a plugin opens for a player (`menu_open`). Since ABI 1.6.
        pub struct MenuSpec {
            /// Size of this record in bytes.
            pub size: u32,
            /// The menu type: `minecraft:generic_9x1` to `generic_9x6`, `generic_3x3`,
            /// `crafter_3x3`, `hopper`, `shulker_box`.
            pub menu_type: Str,
            /// The title.
            pub title: Str,
            /// Format of the title, from [`text_format`].
            pub format: u32,
            /// The items as an SNBT list of stacks with `Slot` (`[{Slot:0b,id:"minecraft:stone",
            /// count:1}]`); empty for none.
            pub items: Str,
        }
        /// What a plugin goal sees of its mob. Since ABI 1.6.
        pub struct GoalMob {
            /// Size of this record in bytes.
            pub size: u32,
            /// The mob.
            pub mob: Uuid,
            /// The world.
            pub world: u32,
            /// Its position.
            pub pos: Vec3,
            /// Its yaw.
            pub yaw: f32,
            /// Its pitch.
            pub pitch: f32,
            /// Its health.
            pub health: f32,
            /// Non-zero on the ground.
            pub on_ground: u32,
            /// Non-zero while its navigation follows a path.
            pub navigating: u32,
            /// Its target ([`Uuid::NIL`] for none).
            pub target: Uuid,
            /// The target's position.
            pub target_pos: Vec3,
            /// The world's game time.
            pub game_time: i64,
        }
        /// What a plugin goal's tick makes its mob do. Since ABI 1.6.
        pub struct GoalAction {
            /// Size of this record in bytes.
            pub size: u32,
            /// Bits of [`goal_action`].
            pub actions: u32,
            /// Where to walk (`MOVE`).
            pub move_to: Vec3,
            /// Speed modifier of the walk (1 is the mob's normal speed).
            pub speed: f64,
            /// Where to look (`LOOK`).
            pub look_at: Vec3,
            /// The new target (`TARGET`).
            pub target: Uuid,
        }
        /// A goal a plugin adds to one mob (`mob_goal_add`). Its functions run on the mob's
        /// owner during its AI tick, without a context. Since ABI 1.6.
        pub struct GoalSpec {
            /// Size of this record in bytes.
            pub size: u32,
            /// Priority as Vanilla's goals have it: lower runs first and may interrupt higher.
            pub priority: i32,
            /// Bits of [`goal_flag`] the goal controls.
            pub flags: u32,
            /// Non-zero for the target selector, zero for the goal selector.
            pub target: u32,
            /// Whether it should start.
            pub can_use: GoalCheckFn,
            /// Whether it should go on (none: `can_use`).
            pub can_continue: Option<GoalCheckFn>,
            /// Called when it starts.
            pub start: Option<GoalFn>,
            /// Called when it stops.
            pub stop: Option<GoalFn>,
            /// Called every tick while it runs.
            pub tick: GoalTickFn,
            /// Frees `user` when the goal or the mob is gone.
            pub drop: Option<DropFn>,
            /// Passed to the functions.
            pub user: *mut c_void,
        }
        /// A chunk populator (Bukkit's `BlockPopulator`): runs on generated chunks after a
        /// stage of the built-in generation, from many threads at once. Since ABI 1.6.
        pub struct ChunkPopulatorProvider {
            /// Size of this record in bytes.
            pub size: u32,
            /// Data version of the block state ids it reads and writes.
            pub data_version: i32,
            /// One of [`gen_stage`].
            pub stage: u32,
            /// Passed to `populate`; never freed.
            pub user: *mut c_void,
            /// Populates a batch of chunks.
            pub populate: PopulateFn,
        }
        /// A packet listener for `packet_subscribe`. Since ABI 1.7.
        pub struct PacketSpec {
            /// Size of this record in bytes.
            pub size: u32,
            /// One of [`packet_direction`].
            pub direction: u32,
            /// One of [`packet_state`].
            pub state: u32,
            /// The packet id in that state and direction (`packet_id` resolves a name), or
            /// [`packet_id::ANY`].
            pub id: i32,
            /// One of [`priority`], or any value between: listeners run from the lowest to the
            /// highest, then the monitors.
            pub priority: i32,
            /// [`packet_listen`] bits.
            pub flags: u32,
            /// The listener. It runs where packets are written and read: on network threads,
            /// for many connections at once (a broadcast listener also on the thread that
            /// encodes the packet), with a context that owns no world. The packets of one
            /// connection arrive in order.
            pub run: PacketFn,
            /// Frees `user` once the listener is removed and no call is running.
            pub drop: Option<DropFn>,
            /// Passed to `run` and `drop`.
            pub user: *mut c_void,
        }
        /// Where a packet listener writes a replacement body. Since ABI 1.7.
        pub struct PacketOut {
            /// Size of this record in bytes.
            pub size: u32,
            /// Passed to `set`.
            pub state: *mut c_void,
            /// Replaces the body (`id` + fields; the id may differ); the last call wins.
            pub set: PacketSetFn,
        }
        /// What a packet listener sees. Since ABI 1.7.
        pub struct PacketEvent {
            /// Size of this record in bytes.
            pub size: u32,
            /// One of [`packet_direction`].
            pub direction: u32,
            /// One of [`packet_state`].
            pub state: u32,
            /// The packet id the body starts with.
            pub id: i32,
            /// Non-zero when cancelled: a clientbound packet is not sent, a serverbound one
            /// not handled. Listeners may set or clear it.
            pub cancelled: u32,
            /// [`packet_event`] bits.
            pub flags: u32,
            /// The protocol version the body is encoded for (the server's own; clients of
            /// other versions are translated behind the listeners).
            pub protocol: i32,
            /// The connection, a number unique in this server run (0 in a broadcast call).
            pub connection: u64,
            /// The player the packet goes to or comes from; nil before its login finished and
            /// in a broadcast call.
            pub player: Uuid,
            /// The packet body: the id as a VarInt, then the fields.
            pub body: Bytes,
            /// Replaces the body. Monitors must not use it.
            pub out: *const PacketOut,
        }
        /// The host's functions for plugins.
        pub struct HostApi {
            /// Size of this record in bytes.
            pub size: u32,
            /// The host's [`ABI_MAJOR`].
            pub abi_major: u16,
            /// The host's [`ABI_MINOR`].
            pub abi_minor: u16,
            /// Logs a message under the plugin's name (`level` from [`log_level`]).
            pub log: unsafe extern "C" fn(PluginId, u32, Str),
            /// The plugin's data folder (`plugins/<id>/`), created by the host.
            pub data_dir: unsafe extern "C" fn(PluginId, *mut StrBuf) -> Status,
            /// Loads `config.toml` from the data folder; writes `defaults` there first when the
            /// file does not exist. Keys are read with the `config_*` functions.
            pub config_load: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// A string value (dotted key, e.g. `messages.join`).
            pub config_string: unsafe extern "C" fn(PluginId, Str, *mut StrBuf) -> Status,
            /// An integer value.
            pub config_int: unsafe extern "C" fn(PluginId, Str, *mut i64) -> Status,
            /// A float value (integers are converted).
            pub config_float: unsafe extern "C" fn(PluginId, Str, *mut f64) -> Status,
            /// A boolean value (0 or 1).
            pub config_bool: unsafe extern "C" fn(PluginId, Str, *mut u32) -> Status,
            /// The value of a `vanillite.toml` rule as TOML text (e.g. `chat.max-pending-messages`).
            pub server_rule: unsafe extern "C" fn(PluginId, Str, *mut StrBuf) -> Status,
            /// The online players: writes up to `cap` UUIDs and the total count.
            pub online_players: unsafe extern "C" fn(PluginId, *mut Uuid, usize, *mut usize) -> Status,
            /// A player's name.
            pub player_name: unsafe extern "C" fn(PluginId, Uuid, *mut StrBuf) -> Status,
            /// A player's world.
            pub player_world: unsafe extern "C" fn(PluginId, Uuid, *mut u32) -> Status,
            /// Sends a system message (format from [`text_format`]).
            pub player_send_message: unsafe extern "C" fn(PluginId, Uuid, Str, u32) -> Status,
            /// Disconnects a player with a reason (format from [`text_format`]).
            pub player_kick: unsafe extern "C" fn(PluginId, Uuid, Str, u32) -> Status,
            /// Whether a player has a permission node (0 or 1).
            pub player_has_permission: unsafe extern "C" fn(PluginId, Uuid, Str, *mut u32) -> Status,
            /// Sends a system message to every player and the log.
            pub broadcast_message: unsafe extern "C" fn(PluginId, Str, u32) -> Status,
            /// The dimension key of a world; [`status::NOT_FOUND`] past the last world.
            pub world_name: unsafe extern "C" fn(PluginId, u32, *mut StrBuf) -> Status,
            /// The numeric id of a block state (`minecraft:stone`, `minecraft:oak_log[axis=x]`)
            /// in the server's data version, for providers.
            pub block_state_id: unsafe extern "C" fn(PluginId, Str, *mut u32) -> Status,
            /// Runs a console command in the next global phase.
            pub run_command: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// Schedules a task; writes its id.
            pub schedule: unsafe extern "C" fn(PluginId, *const TaskSpec, *mut u64) -> Status,
            /// Cancels a task; its `drop` runs.
            pub cancel_task: unsafe extern "C" fn(PluginId, u64) -> Status,
            /// Subscribes to an event kind with a priority (`load` only).
            pub subscribe: unsafe extern "C" fn(PluginId, u32, i32, EventFn, *mut c_void) -> Status,
            /// Adds a command tree. Since ABI 1.12 also after `load` (Bukkit plugins register
            /// commands when they are enabled): the server adds the trees registered during a
            /// tick at its next tick and sends every player its command tree again. A top-level
            /// name that already exists is skipped; trees cannot be removed (hide them with a
            /// node permission the plugin's permission provider denies).
            pub register_command: unsafe extern "C" fn(PluginId, *const CommandSpec) -> Status,
            /// Becomes the permission provider (`load` only; one per server).
            pub set_permission_provider: unsafe extern "C" fn(PluginId, PermissionFn, *mut c_void) -> Status,
            /// Fills the `chunk-generator` slot (`load` only).
            pub provide_chunk_generator:
                unsafe extern "C" fn(PluginId, *const ChunkGeneratorProvider) -> Status,
            /// The world this context owns; [`status::NOT_FOUND`] for global and background
            /// contexts.
            pub owner_world: unsafe extern "C" fn(*mut Context, *mut u32) -> Status,
            /// The block state at a position of the owned world, as text.
            pub block_get: unsafe extern "C" fn(*mut Context, u32, BlockPos, *mut StrBuf) -> Status,
            /// Sets a block state (text) with Vanilla's updates in the owned world.
            pub block_set: unsafe extern "C" fn(*mut Context, u32, BlockPos, Str) -> Status,
            /// The position of a player of the owned world.
            pub player_position: unsafe extern "C" fn(*mut Context, Uuid, *mut Vec3) -> Status,
            /// Sets the header and footer of a player's player list (tab list); both in the
            /// format from [`text_format`], empty text removes them. Since ABI 1.1.
            pub player_set_tab_list: unsafe extern "C" fn(PluginId, Uuid, Str, Str, u32) -> Status,
            /// The text of a numeric block state id (the inverse of `block_state_id`), e.g.
            /// `minecraft:oak_log[axis=y]`. Since ABI 1.2.
            pub block_state_name: unsafe extern "C" fn(PluginId, u32, *mut StrBuf) -> Status,
            /// The numeric block state id at a position of the owned world;
            /// [`status::NOT_FOUND`] outside loaded chunks. Since ABI 1.2.
            pub block_get_id: unsafe extern "C" fn(*mut Context, u32, BlockPos, *mut u32) -> Status,
            /// Sets a block state by numeric id in the owned world (`mode` from [`set_mode`]).
            /// Since ABI 1.2.
            pub block_set_id: unsafe extern "C" fn(*mut Context, u32, BlockPos, u32, u32) -> Status,
            /// Reads the block state ids of a box of the owned world into `out` (`cap` entries,
            /// at least the box's volume, at most [`limits::MAX_BOX_VOLUME`]); positions outside
            /// loaded chunks read [`limits::NO_BLOCK`]. Since ABI 1.2.
            pub blocks_get:
                unsafe extern "C" fn(*mut Context, u32, *const BlockBox, *mut u32, usize) -> Status,
            /// Sets the block states of a box of the owned world from `states` (`len` entries,
            /// the box's volume), in array order with `mode` from [`set_mode`]; entries of
            /// [`limits::NO_BLOCK`] and positions outside loaded chunks are skipped. Writes the
            /// number of blocks that changed to the last argument. Since ABI 1.2.
            pub blocks_set: unsafe extern "C" fn(
                *mut Context,
                u32,
                *const BlockBox,
                *const u32,
                usize,
                u32,
                *mut usize,
            ) -> Status,
            /// Shows a text above a player's hotbar (format from [`text_format`]). Since ABI 1.4.
            pub player_send_action_bar: unsafe extern "C" fn(PluginId, Uuid, Str, u32) -> Status,
            /// Shows a title and subtitle to a player. Since ABI 1.4.
            pub player_send_title: unsafe extern "C" fn(PluginId, Uuid, *const TitleSpec) -> Status,
            /// Reads a persistent data value of an entity of the owned world (players too):
            /// writes its type (from [`data_type`]) and its bytes to `sink`.
            /// [`status::NOT_FOUND`] when the entity is not there or has no such key. Keys are
            /// `namespace:path`; without a namespace the plugin's id is used. The values are
            /// saved with the entity in its NBT (`BukkitValues`). Since ABI 1.4.
            pub entity_data_get: unsafe extern "C" fn(
                *mut Context,
                u32,
                Uuid,
                Str,
                *mut u32,
                *const ByteSink,
            ) -> Status,
            /// Sets a persistent data value of an entity of the owned world (type from
            /// [`data_type`], at most [`data_limits::MAX_VALUE`] bytes, at most
            /// [`data_limits::MAX_KEYS`] keys per entity). Since ABI 1.4.
            pub entity_data_set:
                unsafe extern "C" fn(*mut Context, u32, Uuid, Str, u32, Bytes) -> Status,
            /// Removes a persistent data value of an entity of the owned world; OK when it
            /// had none. Since ABI 1.4.
            pub entity_data_remove: unsafe extern "C" fn(*mut Context, u32, Uuid, Str) -> Status,
            /// The persistent data keys of an entity of the owned world, one per line, to
            /// `sink`. Since ABI 1.4.
            pub entity_data_keys:
                unsafe extern "C" fn(*mut Context, u32, Uuid, *const ByteSink) -> Status,
            /// Subscribes to an event kind with a priority and flags from [`subscribe_flag`]
            /// (`load` only). Since ABI 1.5.
            pub subscribe_flags:
                unsafe extern "C" fn(PluginId, u32, i32, u32, EventFn, *mut c_void) -> Status,
            /// Removes the plugin's handlers of an event kind, at any time. Since ABI 1.5.
            pub unsubscribe: unsafe extern "C" fn(PluginId, u32) -> Status,
            /// The event kind of a plugin event name (e.g. `economy:balance_change`): the same
            /// kind for the same name in every plugin, from [`limits::FIRST_CUSTOM_EVENT`] on.
            /// Since ABI 1.5.
            pub event_register: unsafe extern "C" fn(PluginId, Str, *mut u32) -> Status,
            /// Fires a plugin event ([`CustomEvent`]) to the handlers of its kind on the calling
            /// thread (not to the firing plugin's own: plugins are never re-entered); `ctx`
            /// (may be null) is the caller's context, whose world the handlers may touch.
            /// Writes whether a handler cancelled it. Since ABI 1.5.
            pub event_fire:
                unsafe extern "C" fn(PluginId, *mut Context, u32, Bytes, *mut u32) -> Status,
            /// Offers a service to other plugins: a table of functions (any `repr(C)` record,
            /// static or alive until unregistered) under a name and version; replaces the
            /// plugin's earlier offer of that name. Since ABI 1.5.
            pub service_register:
                unsafe extern "C" fn(PluginId, Str, u32, *const c_void) -> Status,
            /// Withdraws a service of the plugin. Since ABI 1.5.
            pub service_unregister: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// The newest table offered under a name and its version; [`status::NOT_FOUND`]
            /// when nobody offers it. Since ABI 1.5.
            pub service_get:
                unsafe extern "C" fn(PluginId, Str, *mut u32, *mut *const c_void) -> Status,
            /// The enabled plugins, one `id version` per line, in load order. Since ABI 1.5.
            pub plugin_list: unsafe extern "C" fn(PluginId, *const ByteSink) -> Status,
            /// Sets a key of the loaded `config.toml` to a value in TOML (`5`, `"text"`,
            /// `[1, 2]`); dotted keys create tables. Since ABI 1.5.
            pub config_set: unsafe extern "C" fn(PluginId, Str, Str) -> Status,
            /// Writes the loaded configuration to `config.toml`. Since ABI 1.5.
            pub config_save: unsafe extern "C" fn(PluginId) -> Status,
            /// A value of the configuration as TOML text (also lists and tables). Since
            /// ABI 1.5.
            pub config_toml: unsafe extern "C" fn(PluginId, Str, *mut StrBuf) -> Status,
            /// Registers a permission node with who has it when no provider decides (from
            /// [`permission_default`]); `node.*` patterns are not expanded. Since ABI 1.5.
            pub permission_register: unsafe extern "C" fn(PluginId, Str, u32) -> Status,
            /// What the server knows about an online player. Since ABI 1.5.
            pub player_info: unsafe extern "C" fn(PluginId, Uuid, *mut PlayerInfo) -> Status,
            /// An online player's IP address. Since ABI 1.5.
            pub player_address: unsafe extern "C" fn(PluginId, Uuid, *mut StrBuf) -> Status,
            /// Plays a sound to one player. Since ABI 1.5.
            pub player_play_sound: unsafe extern "C" fn(PluginId, Uuid, *const SoundSpec) -> Status,
            /// Stops a sound (empty: all) of a category ([`sound_source`], `ANY` for all) for
            /// a player. Since ABI 1.5.
            pub player_stop_sound: unsafe extern "C" fn(PluginId, Uuid, Str, u32) -> Status,
            /// Shows particles to one player. Since ABI 1.5.
            pub player_spawn_particle:
                unsafe extern "C" fn(PluginId, Uuid, *const ParticleSpec) -> Status,
            /// Sends a player to another server (host and port); the server must accept
            /// transfers. Since ABI 1.5.
            pub player_transfer: unsafe extern "C" fn(PluginId, Uuid, Str, u32) -> Status,
            /// Registers a channel the plugin receives plugin messages on, e.g. `myplugin:sync`
            /// (`load` only; at most [`limits::MAX_CHANNELS`]). Since ABI 1.5.
            pub register_channel: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// Sends a plugin message (custom payload) to a player (at most 1 MiB). Since
            /// ABI 1.5.
            pub player_send_plugin_message:
                unsafe extern "C" fn(PluginId, Uuid, Str, Bytes) -> Status,
            /// Adds a scoreboard objective: name, criterion (e.g. `dummy`), display name.
            /// [`status::INVALID_ARGUMENT`] when it exists. Since ABI 1.5.
            pub objective_add: unsafe extern "C" fn(PluginId, Str, Str, Str, u32) -> Status,
            /// Removes an objective. Since ABI 1.5.
            pub objective_remove: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// Shows an objective in a display slot ([`display_slot`]); an empty name clears
            /// the slot. Since ABI 1.5.
            pub objective_display: unsafe extern "C" fn(PluginId, u32, Str) -> Status,
            /// A score of a holder (a player name or an entity UUID) in an objective;
            /// [`status::NOT_FOUND`] when it has none. Since ABI 1.5.
            pub score_get: unsafe extern "C" fn(PluginId, Str, Str, *mut i32) -> Status,
            /// Sets a score. Since ABI 1.5.
            pub score_set: unsafe extern "C" fn(PluginId, Str, Str, i32) -> Status,
            /// Resets a holder's score in an objective (empty: in all). Since ABI 1.5.
            pub score_reset: unsafe extern "C" fn(PluginId, Str, Str) -> Status,
            /// Adds a team. Since ABI 1.5.
            pub team_add: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// Removes a team. Since ABI 1.5.
            pub team_remove: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// Puts a holder into a team (leaving its old one). Since ABI 1.5.
            pub team_join: unsafe extern "C" fn(PluginId, Str, Str) -> Status,
            /// Takes a holder out of its team. Since ABI 1.5.
            pub team_leave: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// Changes a team option as `/team modify` names it (`displayName`, `color`,
            /// `prefix`, `suffix` take JSON text or a colour name; `friendlyFire`,
            /// `seeFriendlyInvisibles` take `true`/`false`; `nametagVisibility`,
            /// `deathMessageVisibility`, `collisionRule` their Vanilla values). Since ABI 1.5.
            pub team_modify: unsafe extern "C" fn(PluginId, Str, Str, Str) -> Status,
            /// The team of a holder; [`status::NOT_FOUND`] for none. Since ABI 1.5.
            pub team_of: unsafe extern "C" fn(PluginId, Str, *mut StrBuf) -> Status,
            /// Creates a boss bar (a Vanilla custom boss bar, saved with the world) with a
            /// key (`namespace:path`; without a namespace the plugin's id) and a name.
            /// Since ABI 1.5.
            pub bossbar_create: unsafe extern "C" fn(PluginId, Str, Str, u32) -> Status,
            /// Removes a boss bar. Since ABI 1.5.
            pub bossbar_remove: unsafe extern "C" fn(PluginId, Str) -> Status,
            /// Changes a boss bar property ([`bossbar_prop`]). Since ABI 1.5.
            pub bossbar_set: unsafe extern "C" fn(PluginId, Str, u32, i64) -> Status,
            /// Renames a boss bar. Since ABI 1.5.
            pub bossbar_set_name: unsafe extern "C" fn(PluginId, Str, Str, u32) -> Status,
            /// Shows (non-zero) or hides a boss bar for a player. Since ABI 1.5.
            pub bossbar_player: unsafe extern "C" fn(PluginId, Str, Uuid, u32) -> Status,
            /// Tick statistics and player counts. Since ABI 1.5.
            pub server_stats: unsafe extern "C" fn(PluginId, *mut ServerStats) -> Status,
            /// Replaces the server list's message of the day. Since ABI 1.5.
            pub server_set_motd: unsafe extern "C" fn(PluginId, Str, u32) -> Status,
            /// Adds to (non-zero) or removes from an access list ([`access_list`]): player
            /// UUID and name (IP address for `IP_BAN`), reason for bans. Since ABI 1.5.
            pub access_set: unsafe extern "C" fn(PluginId, u32, Uuid, Str, u32, Str) -> Status,
            /// Whether a player (or address) is on an access list (0 or 1). Since ABI 1.5.
            pub access_get: unsafe extern "C" fn(PluginId, u32, Uuid, Str, *mut u32) -> Status,
            /// The time of day of a world in ticks (`/time query daytime` without the
            /// modulo). Since ABI 1.5.
            pub world_time: unsafe extern "C" fn(PluginId, u32, *mut i64) -> Status,
            /// Sets the time of day (`/time set`). Since ABI 1.5.
            pub world_set_time: unsafe extern "C" fn(PluginId, u32, i64) -> Status,
            /// Sets the weather ([`weather`]) for a duration in ticks (-1: Vanilla's random
            /// duration), as `/weather`. Since ABI 1.5.
            pub world_set_weather: unsafe extern "C" fn(PluginId, u32, i32) -> Status,
            /// A game rule's value as `/gamerule` shows it, by its name
            /// (e.g. `keep_inventory`). Since ABI 1.5.
            pub game_rule_get: unsafe extern "C" fn(PluginId, Str, *mut StrBuf) -> Status,
            /// Sets a game rule (`true`, `false` or a number). Since ABI 1.5.
            pub game_rule_set: unsafe extern "C" fn(PluginId, Str, Str) -> Status,
            /// A property ([`player_prop`]) of a player of the owned world. Since ABI 1.5.
            pub player_get: unsafe extern "C" fn(*mut Context, u32, Uuid, u32, *mut f64) -> Status,
            /// Changes a property of a player of the owned world. Since ABI 1.5.
            pub player_set: unsafe extern "C" fn(*mut Context, u32, Uuid, u32, f64) -> Status,
            /// Where an entity (or player) of the owned world is. Since ABI 1.5.
            pub entity_location:
                unsafe extern "C" fn(*mut Context, u32, Uuid, *mut Location) -> Status,
            /// The type of an entity of the owned world, e.g. `minecraft:zombie`. Since
            /// ABI 1.5.
            pub entity_type: unsafe extern "C" fn(*mut Context, u32, Uuid, *mut StrBuf) -> Status,
            /// Teleports an entity of the owned world; players may go to another world
            /// (`Location::world`), other entities only within theirs. Since ABI 1.5.
            pub entity_teleport:
                unsafe extern "C" fn(*mut Context, u32, Uuid, *const Location) -> Status,
            /// Sets an entity's motion in blocks per tick. Since ABI 1.5.
            pub entity_set_velocity: unsafe extern "C" fn(*mut Context, u32, Uuid, Vec3) -> Status,
            /// Spawns an entity as `/summon` does (type, position, SNBT data or empty); writes
            /// its UUID. Since ABI 1.5.
            pub entity_spawn:
                unsafe extern "C" fn(*mut Context, u32, Str, Vec3, Str, *mut Uuid) -> Status,
            /// Removes an entity (not a player) of the owned world. Since ABI 1.5.
            pub entity_remove: unsafe extern "C" fn(*mut Context, u32, Uuid) -> Status,
            /// The entities of the owned world within a radius of a position, nearest first,
            /// optionally of one type (empty: any): writes up to `cap` UUIDs and the total
            /// count. Since ABI 1.5.
            pub entities_near: unsafe extern "C" fn(
                *mut Context,
                u32,
                Vec3,
                f64,
                Str,
                *mut Uuid,
                usize,
                *mut usize,
            ) -> Status,
            /// An entity's data as `/data get entity` shows it, as SNBT to the sink. Since
            /// ABI 1.5.
            pub entity_nbt:
                unsafe extern "C" fn(*mut Context, u32, Uuid, *const ByteSink) -> Status,
            /// Merges SNBT into an entity's data as `/data merge entity` (not players).
            /// Since ABI 1.5.
            pub entity_nbt_merge: unsafe extern "C" fn(*mut Context, u32, Uuid, Str) -> Status,
            /// Gives a living entity an effect (e.g. `minecraft:speed`): duration in ticks
            /// (-1: infinite), amplifier, flags from [`effect_flag`]. Since ABI 1.5.
            pub entity_effect_add:
                unsafe extern "C" fn(*mut Context, u32, Uuid, Str, i32, i32, u32) -> Status,
            /// Removes an effect (empty: all) from a living entity. Since ABI 1.5.
            pub entity_effect_remove: unsafe extern "C" fn(*mut Context, u32, Uuid, Str) -> Status,
            /// Gives a player an item as `/give` does (what does not fit drops): the item as
            /// SNBT, e.g. `{id:"minecraft:diamond",count:3}`. Since ABI 1.5.
            pub player_give: unsafe extern "C" fn(*mut Context, u32, Uuid, Str) -> Status,
            /// The item in a slot of an entity, slots named as `/item` does
            /// (`container.0`, `weapon.mainhand`, `armor.head`, `enderchest.3`, ...), as SNBT
            /// to the sink (`{}` for an empty slot). Since ABI 1.5.
            pub entity_item_get:
                unsafe extern "C" fn(*mut Context, u32, Uuid, Str, *const ByteSink) -> Status,
            /// Puts an item (SNBT, `{}` for none) into a slot of an entity. Since ABI 1.5.
            pub entity_item_set: unsafe extern "C" fn(*mut Context, u32, Uuid, Str, Str) -> Status,
            /// The block entity at a position as `/data get block` shows it, as SNBT to the
            /// sink; [`status::NOT_FOUND`] without one. Since ABI 1.5.
            pub block_entity_nbt:
                unsafe extern "C" fn(*mut Context, u32, BlockPos, *const ByteSink) -> Status,
            /// Merges SNBT into a block entity as `/data merge block`. Since ABI 1.5.
            pub block_entity_merge: unsafe extern "C" fn(*mut Context, u32, BlockPos, Str) -> Status,
            /// Grants (non-zero) or revokes a player's advancement criterion (empty: all of the
            /// advancement), as `/advancement only`. Since ABI 1.5.
            pub advancement_set:
                unsafe extern "C" fn(*mut Context, u32, Uuid, Str, Str, u32) -> Status,
            /// Whether a player has completed an advancement (0 or 1). Since ABI 1.5.
            pub advancement_done:
                unsafe extern "C" fn(*mut Context, u32, Uuid, Str, *mut u32) -> Status,
            /// A property ([`entity_prop`]) of an entity (players too) of the owned world.
            /// Since ABI 1.6.
            pub entity_get: unsafe extern "C" fn(*mut Context, u32, Uuid, u32, *mut f64) -> Status,
            /// Changes a property of an entity of the owned world. Since ABI 1.6.
            pub entity_set: unsafe extern "C" fn(*mut Context, u32, Uuid, u32, f64) -> Status,
            /// An entity's custom name as JSON; [`status::NOT_FOUND`] without one. Since
            /// ABI 1.6.
            pub entity_name: unsafe extern "C" fn(*mut Context, u32, Uuid, *mut StrBuf) -> Status,
            /// Sets an entity's custom name (format from [`text_format`]; empty removes it).
            /// Since ABI 1.6.
            pub entity_set_name: unsafe extern "C" fn(*mut Context, u32, Uuid, Str, u32) -> Status,
            /// The passengers of an entity, first the controlling one: writes up to `cap`
            /// UUIDs and the total count. Since ABI 1.6.
            pub entity_passengers:
                unsafe extern "C" fn(*mut Context, u32, Uuid, *mut Uuid, usize, *mut usize) -> Status,
            /// The vehicle of an entity; [`status::NOT_FOUND`] when it rides nothing. Since
            /// ABI 1.6.
            pub entity_vehicle: unsafe extern "C" fn(*mut Context, u32, Uuid, *mut Uuid) -> Status,
            /// Mounts an entity (first) on a vehicle (second) as `/ride mount` (it leaves its
            /// old vehicle). Since ABI 1.6.
            pub entity_mount: unsafe extern "C" fn(*mut Context, u32, Uuid, Uuid) -> Status,
            /// Dismounts an entity as `/ride dismount`. Since ABI 1.6.
            pub entity_dismount: unsafe extern "C" fn(*mut Context, u32, Uuid) -> Status,
            /// An attribute (e.g. `minecraft:movement_speed`) of a living entity: writes its
            /// base and its value with modifiers. Since ABI 1.6.
            pub entity_attribute:
                unsafe extern "C" fn(*mut Context, u32, Uuid, Str, *mut f64, *mut f64) -> Status,
            /// Sets an attribute's base value as `/attribute ... base set`. Since ABI 1.6.
            pub entity_attribute_set_base:
                unsafe extern "C" fn(*mut Context, u32, Uuid, Str, f64) -> Status,
            /// Adds (non-zero) or removes an attribute modifier as `/attribute ... modifier`:
            /// attribute, modifier id (`namespace:path`; without a namespace the plugin's id),
            /// amount, operation from [`attribute_op`]. Since ABI 1.6.
            pub entity_attribute_modifier: unsafe extern "C" fn(
                *mut Context,
                u32,
                Uuid,
                Str,
                Str,
                f64,
                u32,
                u32,
            ) -> Status,
            /// The target of a mob; [`status::NOT_FOUND`] for none. Since ABI 1.6.
            pub mob_target: unsafe extern "C" fn(*mut Context, u32, Uuid, *mut Uuid) -> Status,
            /// Sets the target of a mob ([`Uuid::NIL`] forgets it), with the target event.
            /// Since ABI 1.6.
            pub mob_set_target: unsafe extern "C" fn(*mut Context, u32, Uuid, Uuid) -> Status,
            /// Lets a mob walk to a position with a speed modifier (1 normal) by its own path
            /// finding; a speed of 0 or less stops it. [`status::NOT_FOUND`] when no path is
            /// found. Since ABI 1.6.
            pub mob_navigate: unsafe extern "C" fn(*mut Context, u32, Uuid, Vec3, f64) -> Status,
            /// Adds a plugin goal to one mob; writes its id. Mobs without plugin goals keep
            /// Vanilla's goals and random sequence unchanged. Since ABI 1.6.
            pub mob_goal_add:
                unsafe extern "C" fn(*mut Context, u32, Uuid, *const GoalSpec, *mut u64) -> Status,
            /// Removes a plugin goal; its `drop` runs. Since ABI 1.6.
            pub mob_goal_remove: unsafe extern "C" fn(*mut Context, u32, Uuid, u64) -> Status,
            /// Removes a mob's Vanilla goals ([`goal_clear`] bits). Since ABI 1.6.
            pub mob_goals_clear: unsafe extern "C" fn(*mut Context, u32, Uuid, u32) -> Status,
            /// The biome at a position of the owned world, e.g. `minecraft:plains`. Since
            /// ABI 1.6.
            pub biome_get: unsafe extern "C" fn(*mut Context, u32, BlockPos, *mut StrBuf) -> Status,
            /// Sets the biome of a box of the owned world as `/fillbiome` (loaded chunks;
            /// clients get the chunks again). Since ABI 1.6.
            pub biome_set: unsafe extern "C" fn(*mut Context, u32, *const BlockBox, Str) -> Status,
            /// The sky and block light (0 to 15) at a position of the owned world. Since
            /// ABI 1.6.
            pub light_get:
                unsafe extern "C" fn(*mut Context, u32, BlockPos, *mut u32, *mut u32) -> Status,
            /// How far a chunk of the owned world is loaded ([`chunk_state`]). Since ABI 1.6.
            pub chunk_state: unsafe extern "C" fn(*mut Context, u32, i32, i32, *mut u32) -> Status,
            /// Force-loads (non-zero) a chunk or ends it, as `/forceload` (saved with the
            /// world). Since ABI 1.6.
            pub chunk_force_load: unsafe extern "C" fn(*mut Context, u32, i32, i32, u32) -> Status,
            /// Adds (non-zero) or removes the plugin's ticket on a chunk: it stays loaded and
            /// ticking while a plugin holds one (not saved; dropped when the plugin is
            /// disabled). Since ABI 1.6.
            pub chunk_ticket: unsafe extern "C" fn(*mut Context, u32, i32, i32, u32) -> Status,
            /// The world border of the owned world. Since ABI 1.6.
            pub border_get: unsafe extern "C" fn(*mut Context, u32, *mut BorderInfo) -> Status,
            /// Changes the world border as `/worldborder` (clients are told). Since ABI 1.6.
            pub border_set: unsafe extern "C" fn(*mut Context, u32, *const BorderInfo) -> Status,
            /// An explosion in the owned world as TNT makes it: centre, radius (power), flags
            /// from [`explode_flag`], the source entity or [`Uuid::NIL`]. Since ABI 1.6.
            pub explode: unsafe extern "C" fn(*mut Context, u32, Vec3, f32, u32, Uuid) -> Status,
            /// Traces a ray from `from` to `to` through the owned world (flags from
            /// [`raytrace_flag`], an entity to ignore or [`Uuid::NIL`]); writes the nearest
            /// hit (`kind` [`ray_hit::MISS`] for none). Since ABI 1.6.
            pub raytrace: unsafe extern "C" fn(
                *mut Context,
                u32,
                Vec3,
                Vec3,
                u32,
                Uuid,
                *mut RayHit,
            ) -> Status,
            /// Adds a chunk populator (`load` only; several per server, in load order). Since
            /// ABI 1.6.
            pub provide_chunk_populator:
                unsafe extern "C" fn(PluginId, *const ChunkPopulatorProvider) -> Status,
            /// Opens a plugin menu for a player of the owned world (its old menu closes);
            /// writes the container id. Clicks fire [`event::INVENTORY_CLICK`] with
            /// [`menu_flag::PLUGIN`]; uncancelled clicks move items as in a chest. Since
            /// ABI 1.6.
            pub menu_open:
                unsafe extern "C" fn(*mut Context, u32, Uuid, *const MenuSpec, *mut u32) -> Status,
            /// The item in a slot of a player's open menu (any menu, slots numbered as clicks
            /// report them), as SNBT to the sink (`{}` for none). Since ABI 1.6.
            pub menu_item:
                unsafe extern "C" fn(*mut Context, u32, Uuid, u32, *const ByteSink) -> Status,
            /// Puts an item (SNBT, `{}` for none) into a slot of a player's open menu. Since
            /// ABI 1.6.
            pub menu_set_item: unsafe extern "C" fn(*mut Context, u32, Uuid, u32, Str) -> Status,
            /// Closes a player's open menu. Since ABI 1.6.
            pub menu_close: unsafe extern "C" fn(*mut Context, u32, Uuid) -> Status,
            /// A component of an item (SNBT) as SNBT to the sink, e.g. `minecraft:custom_name`
            /// of `{id:"minecraft:stone",count:1,components:{...}}`, defaults of the item type
            /// included; [`status::NOT_FOUND`] when it has none. Since ABI 1.6.
            pub item_component:
                unsafe extern "C" fn(PluginId, Str, Str, *const ByteSink) -> Status,
            /// Sets a component of an item (SNBT value; empty removes it, also a default) and
            /// writes the item as SNBT to the sink; the value is checked as Vanilla's codec
            /// reads it. Since ABI 1.6.
            pub item_set_component:
                unsafe extern "C" fn(PluginId, Str, Str, Str, *const ByteSink) -> Status,
            /// Reads a persistent data value of an item (SNBT), stored as Paper stores it
            /// (`minecraft:custom_data` → `PublicBukkitValues`): writes its type ([`data_type`])
            /// and bytes. Keys as for entities. Since ABI 1.6.
            pub item_data_get:
                unsafe extern "C" fn(PluginId, Str, Str, *mut u32, *const ByteSink) -> Status,
            /// Sets a persistent data value of an item and writes the item as SNBT. Since
            /// ABI 1.6.
            pub item_data_set:
                unsafe extern "C" fn(PluginId, Str, Str, u32, Bytes, *const ByteSink) -> Status,
            /// Removes a persistent data value of an item and writes the item as SNBT. Since
            /// ABI 1.6.
            pub item_data_remove:
                unsafe extern "C" fn(PluginId, Str, Str, *const ByteSink) -> Status,
            /// The persistent data keys of an item, one per line. Since ABI 1.6.
            pub item_data_keys: unsafe extern "C" fn(PluginId, Str, *const ByteSink) -> Status,
            /// Adds a packet listener; writes its handle. Allowed at any time and from any
            /// thread. Packet types without a listener cost the server nothing. Since ABI 1.7.
            pub packet_subscribe:
                unsafe extern "C" fn(PluginId, *const PacketSpec, *mut u64) -> Status,
            /// Removes a packet listener by its handle; calls that are running finish first.
            /// Since ABI 1.7.
            pub packet_unsubscribe: unsafe extern "C" fn(PluginId, u64) -> Status,
            /// The id of a packet by its name in a state ([`packet_state`]) and direction
            /// ([`packet_direction`]), e.g. `set_entity_data`. Ids change between Minecraft
            /// versions, names rarely. Since ABI 1.7.
            pub packet_id: unsafe extern "C" fn(PluginId, u32, u32, Str, *mut i32) -> Status,
            /// The name of a packet by its id. Since ABI 1.7.
            pub packet_name: unsafe extern "C" fn(PluginId, u32, u32, i32, *mut StrBuf) -> Status,
            /// Sends a play packet body (`id` + fields, the server's protocol version) to a
            /// player, in order with the server's packets from this thread; flags from
            /// [`packet_send`]. Since ABI 1.7.
            pub packet_send: unsafe extern "C" fn(PluginId, Uuid, Bytes, u32) -> Status,
            /// The protocol version packet bodies are encoded for. Since ABI 1.7.
            pub protocol_version: unsafe extern "C" fn(PluginId, *mut i32) -> Status,
            /// Creates an extra world, or loads it when its folder holds one (which keeps its
            /// seed and generator), and writes its world index. It loads in the next global
            /// phase ([`event::WORLD_LOAD`]); until then it is not usable. Since ABI 1.8.
            pub world_create:
                unsafe extern "C" fn(PluginId, *const WorldCreate, *mut u32) -> Status,
            /// Loads the extra world of a folder (the name) and writes its world index;
            /// [`status::NOT_FOUND`] when the folder holds none. Loads as `world_create` does.
            /// Since ABI 1.8.
            pub world_load: unsafe extern "C" fn(PluginId, Str, *mut u32) -> Status,
            /// Unloads an extra world in the next global phase, its chunks saved when `save`
            /// is non-zero. [`status::WRONG_OWNER`] for the default world's dimensions,
            /// [`status::INVALID_ARGUMENT`] while players are in it. Since ABI 1.8.
            pub world_unload: unsafe extern "C" fn(PluginId, u32, u32) -> Status,
            /// The number of world indices (loaded or not); every world is below it. Since
            /// ABI 1.8.
            pub world_count: unsafe extern "C" fn(PluginId, *mut u32) -> Status,
            /// What a world is and its folder (the default world's `level-name` for its
            /// dimensions). Since ABI 1.8.
            pub world_info:
                unsafe extern "C" fn(PluginId, u32, *mut WorldDetails, *mut StrBuf) -> Status,
            /// Moves an online player to a place in any loaded world, from any thread
            /// (between the ticks of its world; to another world like `/tp`, with the
            /// teleport event). Since ABI 1.8.
            pub player_move: unsafe extern "C" fn(PluginId, Uuid, *const Location) -> Status,
            /// Feeds a serverbound play packet body (`id` + fields, the server's protocol
            /// version) into a player's connection as if the client sent it: it passes the
            /// packet listeners (unless [`packet_send::SKIP_LISTENERS`]), the validating
            /// decoder and the player's rate limits, and is handled in order with the
            /// client's packets. Fails with [`status::INVALID_ARGUMENT`] for a body that does
            /// not decode, [`status::NOT_FOUND`] for a player who is offline and
            /// [`status::ERROR`] while too many fed packets wait. Since ABI 1.8.
            pub packet_receive: unsafe extern "C" fn(PluginId, Uuid, Bytes, u32) -> Status,
            /// The entities of the owned world whose bounding box intersects the box from
            /// `min` to `max` (each side at most 2048 blocks), in the order of the world's
            /// entity sections, optionally of one type (empty: any): writes up to `cap`
            /// records and the total count, one call for Bukkit's `getNearbyEntities`.
            /// Since ABI 1.9.
            pub entities_in_box: unsafe extern "C" fn(
                *mut Context,
                u32,
                Vec3,
                Vec3,
                Str,
                *mut EntityInfo,
                usize,
                *mut usize,
            ) -> Status,
            /// The name of an entity type by its numeric id ([`EntityInfo::kind`]), e.g.
            /// `minecraft:zombie`. Since ABI 1.9.
            pub entity_type_name: unsafe extern "C" fn(PluginId, u32, *mut StrBuf) -> Status,
            /// Static facts of a block type by name (e.g. `minecraft:stone`); `NOT_FOUND` for
            /// an unknown block. Since ABI 1.10.
            pub block_type_info: unsafe extern "C" fn(PluginId, Str, *mut BlockTypeInfo) -> Status,
            /// Static facts of an item type by name (e.g. `minecraft:diamond_sword`);
            /// `NOT_FOUND` for an unknown item. Since ABI 1.10.
            pub item_type_info: unsafe extern "C" fn(PluginId, Str, *mut ItemTypeInfo) -> Status,
            /// The entry names of a registry (e.g. `minecraft:enchantment`), static or loaded
            /// from the data packs, one per line in protocol id order; `NOT_FOUND` for an
            /// unknown registry. Since ABI 1.10.
            pub registry_entries: unsafe extern "C" fn(PluginId, Str, *const ByteSink) -> Status,
            /// The tags of a registry loaded from the data packs: with an empty tag the tag
            /// names, else the entry names of that tag (resolved), one per line; `NOT_FOUND` for
            /// an unknown registry or tag. Since ABI 1.10.
            pub registry_tag: unsafe extern "C" fn(PluginId, Str, Str, *const ByteSink) -> Status,
            /// Sends an online player its command tree again (after its permissions changed),
            /// from any thread; `NOT_FOUND` for a player who is offline. Since ABI 1.10.
            pub player_update_commands: unsafe extern "C" fn(PluginId, Uuid) -> Status,
            /// Replaces the plugins this plugin hosts (`count` records `stride` bytes apart, at
            /// most [`limits::MAX_HOSTED_PLUGINS`]; 0 clears them), from any thread. On
            /// `INVALID_ARGUMENT` (bad UTF-8, too long, too many) the previous list stays.
            /// Since ABI 1.11.
            pub set_hosted_plugins:
                unsafe extern "C" fn(PluginId, *const HostedPlugin, usize, usize) -> Status,
        }
    }
}

impl Str {
    /// A string borrowed from `s`.
    pub const fn new(s: &str) -> Str {
        Str {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// The empty string.
    pub const EMPTY: Str = Str::new("");
}

impl Bytes {
    /// Bytes borrowed from `b`.
    pub const fn new(b: &[u8]) -> Bytes {
        Bytes {
            ptr: b.as_ptr(),
            len: b.len(),
        }
    }
}

impl Uuid {
    /// No UUID (all zero).
    pub const NIL: Uuid = Uuid { hi: 0, lo: 0 };

    /// From a 128-bit value.
    pub const fn from_u128(v: u128) -> Uuid {
        Uuid {
            hi: (v >> 64) as u64,
            lo: v as u64,
        }
    }

    /// As a 128-bit value.
    pub const fn to_u128(self) -> u128 {
        ((self.hi as u128) << 64) | self.lo as u128
    }
}

/// The size of a record as its `size` field holds it.
pub const fn size_of<T>() -> u32 {
    core::mem::size_of::<T>() as u32
}
