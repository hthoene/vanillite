//! Registration in `load`: events, commands, the permission provider, providers and config.

use std::ffi::c_void;
use std::marker::PhantomData;

use vanillite_abi::{self as abi, Status, status};

use crate::Plugin;
use crate::command::{self, Command};
use crate::events::{
    AdvancementDone, BlockChange, CustomEvent, EntityDamage, EntityDeath, GameModeChange, Interact,
    Login, PlayerDeath, PlayerJoin, PlayerText, PlayerWorld, PlayersMoved, PluginMessage,
};
use crate::ffi::{Host, guard, str, with_plugin};
use crate::game::{ChunkPopulator, GameEvent};
use crate::generator::{self, ChunkGeneratorProvider};
use crate::server::{Ctx, Server};
use crate::types::{Error, Uuid};

type Handler<P> = Box<dyn FnMut(&mut P, &mut Ctx<'_>, *mut abi::EventHeader) + Send>;
type PermissionCheck = Box<dyn Fn(Uuid, &str) -> Permission + Send + Sync>;

/// The answer of a permission provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    /// Granted.
    Allow,
    /// Denied.
    Deny,
    /// Not decided: operators and the console have it, other players not.
    Undefined,
}

/// Who has a registered permission node when no permission provider decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDefault {
    /// Nobody.
    False,
    /// Everyone.
    True,
    /// Operators (the default of unregistered nodes).
    Op,
    /// Everyone but operators.
    NotOp,
}

/// Registers what a plugin provides. Only valid in [`Plugin::load`].
pub struct Registrar<P> {
    host: Host,
    error: Option<Error>,
    /// [`abi::subscribe_flag`] bits for the next subscription.
    next_flags: u32,
    _plugin: PhantomData<fn(P)>,
}

impl<P> std::fmt::Debug for Registrar<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registrar")
            .field("plugin", &self.host.id)
            .finish()
    }
}

impl<P: Plugin> Registrar<P> {
    pub(crate) fn new(host: Host) -> Self {
        Registrar {
            host,
            error: None,
            next_flags: 0,
            _plugin: PhantomData,
        }
    }

    /// The first registration error, as the status `load` returns.
    pub(crate) fn finish(self) -> Status {
        self.error.map_or(status::OK, |e| e.status())
    }

    fn record(&mut self, s: Status) {
        if let Err(e) = Error::check(s)
            && self.error.is_none()
        {
            self.error = Some(e);
        }
    }

    /// Thread-safe server access (config, logging, data folder already work in `load`).
    pub fn server(&self) -> Server {
        Server::from_host(self.host)
    }

    /// Loads the plugin's `config.toml`, writing `defaults` first when it does not exist. Read
    /// values with [`Server::config_string`] and its siblings.
    pub fn config(&mut self, defaults: &str) -> Result<(), Error> {
        // SAFETY: valid host function and string.
        Error::check(unsafe { (self.host.api.config_load)(self.host.id, abi::Str::new(defaults)) })
    }

    fn subscribe(&mut self, kind: u32, priority: i32, handler: Handler<P>) -> &mut Self {
        let user = Box::into_raw(Box::new(handler)).cast::<c_void>();
        let flags = std::mem::take(&mut self.next_flags);
        // SAFETY: valid host functions; `user` stays alive for the life of the library.
        let s = unsafe {
            if flags == 0 {
                (self.host.api.subscribe)(self.host.id, kind, priority, on_event::<P>, user)
            } else {
                (self.host.api.subscribe_flags)(
                    self.host.id,
                    kind,
                    priority,
                    flags,
                    on_event::<P>,
                    user,
                )
            }
        };
        self.record(s);
        self
    }

    /// Adds a packet listener ([`Server::on_packet`], which also works after `load`); see
    /// [`packet`](crate::packet). Since ABI 1.7.
    pub fn on_packet(
        &mut self,
        filter: crate::packet::PacketFilter,
        listener: impl Fn(&mut crate::packet::PacketEvent<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        if let Err(e) = self.server().on_packet(filter, listener)
            && self.error.is_none()
        {
            self.error = Some(e);
        }
        self
    }

    /// The next `on_*` handler is skipped while its event is cancelled (Bukkit's
    /// `ignoreCancelled = true`), e.g. `registrar.ignore_cancelled().on_chat(...)`. Since
    /// ABI 1.5.
    pub fn ignore_cancelled(&mut self) -> &mut Self {
        self.next_flags |= abi::subscribe_flag::IGNORE_CANCELLED;
        self
    }

    /// Registers a permission node with who has it when no permission provider decides.
    /// Since ABI 1.5.
    pub fn permission(&mut self, node: &str, default: PermissionDefault) -> &mut Self {
        let default = match default {
            PermissionDefault::False => abi::permission_default::FALSE,
            PermissionDefault::True => abi::permission_default::TRUE,
            PermissionDefault::Op => abi::permission_default::OP,
            PermissionDefault::NotOp => abi::permission_default::NOT_OP,
        };
        // SAFETY: valid host function and string.
        let s = unsafe {
            (self.host.api.permission_register)(self.host.id, abi::Str::new(node), default)
        };
        self.record(s);
        self
    }

    /// Receives the plugin messages players send on `channel` (e.g. `myplugin:sync`). No
    /// world access. Since ABI 1.5.
    pub fn on_plugin_message(
        &mut self,
        channel: &str,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &PluginMessage<'_>) + Send + 'static,
    ) -> &mut Self {
        // SAFETY: valid host function and string.
        let s = unsafe { (self.host.api.register_channel)(self.host.id, abi::Str::new(channel)) };
        self.record(s);
        let channel = channel.to_owned();
        self.subscribe(
            abi::event::PLAYER_PLUGIN_MESSAGE,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `PluginMessageEvent` for this kind.
                let raw = unsafe { &*e.cast::<abi::PluginMessageEvent>() };
                let message = PluginMessage { raw };
                if message.channel() == channel {
                    f(p, ctx, &message);
                }
            }),
        )
    }

    /// A player completed an advancement. No world access. Since ABI 1.5.
    pub fn on_advancement(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &AdvancementDone<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_ADVANCEMENT_DONE,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes an `AdvancementEvent` for this kind.
                let raw = unsafe { &*e.cast::<abi::AdvancementEvent>() };
                f(p, ctx, &AdvancementDone { raw });
            }),
        )
    }

    /// A plugin event named `name` (any plugin may fire it with [`Server::fire_event`]); the
    /// context is the firing plugin's. Since ABI 1.5.
    pub fn on_plugin_event(
        &mut self,
        name: &str,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut CustomEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        let Some(kind) = self.server().event_kind(name) else {
            self.record(status::UNSUPPORTED);
            return self;
        };
        self.subscribe(
            kind,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `CustomEvent` for plugin event kinds.
                let raw = unsafe { &mut *e.cast::<abi::CustomEvent>() };
                f(p, ctx, &mut CustomEvent { raw });
            }),
        )
    }

    /// A player joined (after the join message). The context owns the player's world.
    pub fn on_join(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &PlayerJoin<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_JOIN,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `PlayerEvent` for this kind.
                let raw = unsafe { &*e.cast::<abi::PlayerEvent>() };
                f(p, ctx, &PlayerJoin { raw });
            }),
        )
    }

    /// A player left.
    pub fn on_leave(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &PlayerJoin<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_LEAVE,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `PlayerEvent` for this kind.
                let raw = unsafe { &*e.cast::<abi::PlayerEvent>() };
                f(p, ctx, &PlayerJoin { raw });
            }),
        )
    }

    /// A player sends a chat message; cancellable.
    pub fn on_chat(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut PlayerText<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_CHAT,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `PlayerTextEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::PlayerTextEvent>() };
                f(p, ctx, &mut PlayerText { raw });
            }),
        )
    }

    /// A player runs a command (any command, before it is parsed); cancellable.
    pub fn on_command(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut PlayerText<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_COMMAND,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `PlayerTextEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::PlayerTextEvent>() };
                f(p, ctx, &mut PlayerText { raw });
            }),
        )
    }

    /// The players of a world that moved this tick, in one batch per world and tick. The
    /// context owns that world.
    pub fn on_players_moved(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &PlayersMoved<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYERS_MOVED,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `PlayersMovedEvent` for this kind.
                let raw = unsafe { &*e.cast::<abi::PlayersMovedEvent>() };
                f(p, ctx, &PlayersMoved { raw });
            }),
        )
    }

    /// An entity is about to take damage; cancellable, the amount can be changed.
    pub fn on_entity_damage(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut EntityDamage<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::ENTITY_DAMAGE,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes an `EntityDamageEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::EntityDamageEvent>() };
                f(p, ctx, &mut EntityDamage { raw });
            }),
        )
    }

    /// A player is about to break a block; cancellable.
    pub fn on_block_break(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut BlockChange<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::BLOCK_BREAK,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `BlockEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::BlockEvent>() };
                f(p, ctx, &mut BlockChange { raw });
            }),
        )
    }

    /// A player is about to place a block; cancellable.
    pub fn on_block_place(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut BlockChange<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::BLOCK_PLACE,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `BlockEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::BlockEvent>() };
                f(p, ctx, &mut BlockChange { raw });
            }),
        )
    }

    /// A player is about to fall asleep in a bed (the position is the bed's head);
    /// cancellable. Since ABI 1.3.
    pub fn on_bed_enter(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut BlockChange<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_player_block(abi::event::PLAYER_BED_ENTER, priority, f)
    }

    /// A player left its bed (the position is the bed's head). Since ABI 1.3.
    pub fn on_bed_leave(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut BlockChange<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_player_block(abi::event::PLAYER_BED_LEAVE, priority, f)
    }

    /// A bed or respawn anchor is about to become a player's respawn point; cancellable.
    /// Since ABI 1.3.
    pub fn on_spawn_change(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut BlockChange<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_player_block(abi::event::PLAYER_SPAWN_CHANGE, priority, f)
    }

    fn on_player_block(
        &mut self,
        kind: u32,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut BlockChange<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            kind,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `BlockEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::BlockEvent>() };
                f(p, ctx, &mut BlockChange { raw });
            }),
        )
    }

    /// A player logs in, after the server's checks (bans, whitelist, player limit) and
    /// before it joins a world: refuse it or let a refused player in. No world access.
    /// Since ABI 1.4.
    pub fn on_login(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut Login<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_LOGIN,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `LoginEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::LoginEvent>() };
                f(p, ctx, &mut Login { raw });
            }),
        )
    }

    /// A player died: change its death message or let it keep its inventory. No world
    /// access (the world is in the middle of its tick). Since ABI 1.4.
    pub fn on_death(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut PlayerDeath<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_DEATH,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `PlayerDeathEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::PlayerDeathEvent>() };
                f(p, ctx, &mut PlayerDeath { raw });
            }),
        )
    }

    /// A mob died. No world access (the world is in the middle of its tick). Since ABI 1.4.
    pub fn on_entity_death(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &EntityDeath<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::ENTITY_DEATH,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes an `EntityDeathEvent` for this kind.
                let raw = unsafe { &*e.cast::<abi::EntityDeathEvent>() };
                f(p, ctx, &EntityDeath { raw });
            }),
        )
    }

    /// A player respawned (after death or the end credits). The context owns its new world.
    /// Since ABI 1.4.
    pub fn on_respawn(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &PlayerWorld<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_player_world(abi::event::PLAYER_RESPAWN, priority, f)
    }

    /// A player arrived in another world (portal or teleport). The context owns the new
    /// world. Since ABI 1.4.
    pub fn on_change_world(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &PlayerWorld<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_player_world(abi::event::PLAYER_CHANGE_WORLD, priority, f)
    }

    fn on_player_world(
        &mut self,
        kind: u32,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &PlayerWorld<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            kind,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `PlayerWorldEvent` for this kind.
                let raw = unsafe { &*e.cast::<abi::PlayerWorldEvent>() };
                f(p, ctx, &PlayerWorld { raw });
            }),
        )
    }

    /// A player's game mode is about to change; cancellable. No world access. Since ABI 1.4.
    pub fn on_game_mode_change(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameModeChange<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_GAME_MODE_CHANGE,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `GameModeEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::GameModeEvent>() };
                f(p, ctx, &mut GameModeChange { raw });
            }),
        )
    }

    /// A player uses its item on a block or in the air; cancellable. No world access (the
    /// player's world is busy with its packets). Since ABI 1.4.
    pub fn on_interact(
        &mut self,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut Interact<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            abi::event::PLAYER_INTERACT,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes an `InteractEvent` for this kind.
                let raw = unsafe { &mut *e.cast::<abi::InteractEvent>() };
                f(p, ctx, &mut Interact { raw });
            }),
        )
    }

    /// Adds a command tree to the server's command tree. Its commands keep their place in the
    /// tree for the life of the server; a development reload replaces their callbacks.
    pub fn command(&mut self, command: Command<P>) -> &mut Self {
        let (nodes, tree) = command::flatten(command);
        let raw: Vec<abi::CommandNode> = nodes
            .iter()
            .map(|(name, node, permission)| abi::CommandNode {
                // SAFETY: the bytes come from a `String`.
                name: abi::Str::new(unsafe { std::str::from_utf8_unchecked(name) }),
                permission: permission.as_deref().map_or(abi::Str::EMPTY, abi::Str::new),
                ..*node
            })
            .collect();
        let user = Box::into_raw(Box::new(tree)).cast::<c_void>();
        let spec = abi::CommandSpec {
            size: abi::size_of::<abi::CommandSpec>(),
            nodes: raw.as_ptr(),
            count: raw.len(),
            stride: size_of::<abi::CommandNode>(),
            execute: command::execute::<P>,
            suggest: Some(command::suggest::<P>),
            user,
        };
        // SAFETY: valid host function; the spec and its strings live for the call (the host
        // copies them), `user` lives for the life of the library.
        let s = unsafe { (self.host.api.register_command)(self.host.id, &spec) };
        self.record(s);
        self
    }

    /// Becomes the server's permission provider. `f` is called from several threads at once.
    pub fn permission_provider(
        &mut self,
        f: impl Fn(Uuid, &str) -> Permission + Send + Sync + 'static,
    ) -> &mut Self {
        let provider: Box<PermissionCheck> = Box::new(Box::new(f));
        let user = Box::into_raw(provider).cast::<c_void>();
        // SAFETY: valid host function; `user` lives for the life of the library.
        let s = unsafe { (self.host.api.set_permission_provider)(self.host.id, permission, user) };
        self.record(s);
        self
    }

    /// Handles an event of the kinds added with ABI 1.6 ([`abi::event::PLAYER_MOVE`] and
    /// later; [`GameEvent`] documents the fields per kind). The `on_*` methods below name each
    /// kind. Since ABI 1.6.
    pub fn on_event(
        &mut self,
        kind: u32,
        priority: i32,
        mut f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.subscribe(
            kind,
            priority,
            Box::new(move |p, ctx, e| {
                // SAFETY: the host passes a `GameEvent` for the kinds since ABI 1.6.
                let raw = unsafe { &mut *e.cast::<abi::GameEvent>() };
                f(p, ctx, &mut GameEvent { raw });
            }),
        )
    }

    /// A player moved or turned; cancellable, the target can be changed. See [`abi::event::PLAYER_MOVE`]. Since ABI 1.6.
    pub fn on_player_move(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_MOVE, priority, f)
    }

    /// A player is about to be teleported; cancellable, the target can be changed. See [`abi::event::PLAYER_TELEPORT`]. Since ABI 1.6.
    pub fn on_player_teleport(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_TELEPORT, priority, f)
    }

    /// A player uses its item on an entity; cancellable. See [`abi::event::PLAYER_INTERACT_ENTITY`]. Since ABI 1.6.
    pub fn on_player_interact_entity(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_INTERACT_ENTITY, priority, f)
    }

    /// A player is about to attack an entity; cancellable. See [`abi::event::PLAYER_ATTACK_ENTITY`]. Since ABI 1.6.
    pub fn on_player_attack_entity(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_ATTACK_ENTITY, priority, f)
    }

    /// A player drops an item; cancellable. See [`abi::event::PLAYER_DROP_ITEM`]. Since ABI 1.6.
    pub fn on_player_drop_item(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_DROP_ITEM, priority, f)
    }

    /// An entity picks up an item; cancellable. See [`abi::event::ENTITY_PICKUP_ITEM`]. Since ABI 1.6.
    pub fn on_entity_pickup_item(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_PICKUP_ITEM, priority, f)
    }

    /// A player starts or stops sneaking. See [`abi::event::PLAYER_TOGGLE_SNEAK`]. Since ABI 1.6.
    pub fn on_player_toggle_sneak(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_TOGGLE_SNEAK, priority, f)
    }

    /// A player starts or stops sprinting. See [`abi::event::PLAYER_TOGGLE_SPRINT`]. Since ABI 1.6.
    pub fn on_player_toggle_sprint(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_TOGGLE_SPRINT, priority, f)
    }

    /// A player starts or stops flying; cancellable. See [`abi::event::PLAYER_TOGGLE_FLIGHT`]. Since ABI 1.6.
    pub fn on_player_toggle_flight(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_TOGGLE_FLIGHT, priority, f)
    }

    /// A player selects another hotbar slot; cancellable. See [`abi::event::PLAYER_ITEM_HELD`]. Since ABI 1.6.
    pub fn on_player_item_held(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_ITEM_HELD, priority, f)
    }

    /// A player finishes eating or drinking; cancellable. See [`abi::event::PLAYER_ITEM_CONSUME`]. Since ABI 1.6.
    pub fn on_player_item_consume(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_ITEM_CONSUME, priority, f)
    }

    /// A player is about to be kicked; cancellable, the reason can be changed. See [`abi::event::PLAYER_KICK`]. Since ABI 1.6.
    pub fn on_player_kick(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_KICK, priority, f)
    }

    /// A player's statistic changes; cancellable. See [`abi::event::PLAYER_STATISTIC`]. Since ABI 1.6.
    pub fn on_player_statistic(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_STATISTIC, priority, f)
    }

    /// A player's client reports a resource pack status. See [`abi::event::PLAYER_RESOURCE_PACK`]. Since ABI 1.6.
    pub fn on_player_resource_pack(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_RESOURCE_PACK, priority, f)
    }

    /// A player swaps the items of its hands; cancellable. See [`abi::event::PLAYER_SWAP_HANDS`]. Since ABI 1.6.
    pub fn on_player_swap_hands(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_SWAP_HANDS, priority, f)
    }

    /// A player gains experience from an orb; the amount can be changed. See [`abi::event::PLAYER_EXP_CHANGE`]. Since ABI 1.6.
    pub fn on_player_exp_change(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_EXP_CHANGE, priority, f)
    }

    /// An entity is about to be added to a world; cancellable. See [`abi::event::ENTITY_SPAWN`]. Since ABI 1.6.
    pub fn on_entity_spawn(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_SPAWN, priority, f)
    }

    /// An entity died and is about to drop loot and experience; both can be changed. See [`abi::event::ENTITY_DROPS`]. Since ABI 1.6.
    pub fn on_entity_drops(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_DROPS, priority, f)
    }

    /// A mob is about to change its target; cancellable. See [`abi::event::ENTITY_TARGET`]. Since ABI 1.6.
    pub fn on_entity_target(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_TARGET, priority, f)
    }

    /// An explosion is about to happen; cancellable, radius and flags can be changed. See [`abi::event::ENTITY_EXPLODE`]. Since ABI 1.6.
    pub fn on_entity_explode(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_EXPLODE, priority, f)
    }

    /// A projectile is launched; cancellable. See [`abi::event::PROJECTILE_LAUNCH`]. Since ABI 1.6.
    pub fn on_projectile_launch(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PROJECTILE_LAUNCH, priority, f)
    }

    /// A projectile hits something; cancellable. See [`abi::event::PROJECTILE_HIT`]. Since ABI 1.6.
    pub fn on_projectile_hit(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PROJECTILE_HIT, priority, f)
    }

    /// An entity is about to mount a vehicle; cancellable. See [`abi::event::VEHICLE_ENTER`]. Since ABI 1.6.
    pub fn on_vehicle_enter(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::VEHICLE_ENTER, priority, f)
    }

    /// An entity is about to leave its vehicle; cancellable. See [`abi::event::VEHICLE_EXIT`]. Since ABI 1.6.
    pub fn on_vehicle_exit(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::VEHICLE_EXIT, priority, f)
    }

    /// An animal is about to be tamed; cancellable. See [`abi::event::ENTITY_TAME`]. Since ABI 1.6.
    pub fn on_entity_tame(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_TAME, priority, f)
    }

    /// Two animals are about to make a child; cancellable. See [`abi::event::ENTITY_BREED`]. Since ABI 1.6.
    pub fn on_entity_breed(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_BREED, priority, f)
    }

    /// An entity is about to travel through a portal; cancellable. See [`abi::event::ENTITY_PORTAL`]. Since ABI 1.6.
    pub fn on_entity_portal(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_PORTAL, priority, f)
    }

    /// An entity is about to turn into another; cancellable. See [`abi::event::ENTITY_TRANSFORM`]. Since ABI 1.6.
    pub fn on_entity_transform(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_TRANSFORM, priority, f)
    }

    /// A broken block is about to drop items and experience; both can be changed. See [`abi::event::BLOCK_DROP`]. Since ABI 1.6.
    pub fn on_block_drop(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_DROP, priority, f)
    }

    /// A block grows; cancellable. See [`abi::event::BLOCK_GROW`]. Since ABI 1.6.
    pub fn on_block_grow(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_GROW, priority, f)
    }

    /// A block forms; cancellable. See [`abi::event::BLOCK_FORM`]. Since ABI 1.6.
    pub fn on_block_form(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_FORM, priority, f)
    }

    /// A block spreads; cancellable. See [`abi::event::BLOCK_SPREAD`]. Since ABI 1.6.
    pub fn on_block_spread(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_SPREAD, priority, f)
    }

    /// A block fades; cancellable. See [`abi::event::BLOCK_FADE`]. Since ABI 1.6.
    pub fn on_block_fade(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_FADE, priority, f)
    }

    /// Leaves decay; cancellable. See [`abi::event::LEAVES_DECAY`]. Since ABI 1.6.
    pub fn on_leaves_decay(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::LEAVES_DECAY, priority, f)
    }

    /// A fluid flows into a block; cancellable. See [`abi::event::BLOCK_FLUID_FLOW`]. Since ABI 1.6.
    pub fn on_block_fluid_flow(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_FLUID_FLOW, priority, f)
    }

    /// Fire destroys a block; cancellable. See [`abi::event::BLOCK_BURN`]. Since ABI 1.6.
    pub fn on_block_burn(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_BURN, priority, f)
    }

    /// A block catches fire; cancellable. See [`abi::event::BLOCK_IGNITE`]. Since ABI 1.6.
    pub fn on_block_ignite(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_IGNITE, priority, f)
    }

    /// A player uses bone meal; cancellable. See [`abi::event::BLOCK_FERTILIZE`]. Since ABI 1.6.
    pub fn on_block_fertilize(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_FERTILIZE, priority, f)
    }

    /// A sapling or mushroom grows into a tree; cancellable. See [`abi::event::STRUCTURE_GROW`]. Since ABI 1.6.
    pub fn on_structure_grow(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::STRUCTURE_GROW, priority, f)
    }

    /// A piston extends or retracts; cancellable. See [`abi::event::BLOCK_PISTON`]. Since ABI 1.6.
    pub fn on_block_piston(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_PISTON, priority, f)
    }

    /// A player changes a sign's text; cancellable, the lines can be changed. See [`abi::event::SIGN_CHANGE`]. Since ABI 1.6.
    pub fn on_sign_change(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::SIGN_CHANGE, priority, f)
    }

    /// A dispenser or dropper is about to dispense; cancellable. See [`abi::event::BLOCK_DISPENSE`]. Since ABI 1.6.
    pub fn on_block_dispense(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BLOCK_DISPENSE, priority, f)
    }

    /// A chunk was loaded. See [`abi::event::CHUNK_LOAD`]. Since ABI 1.6.
    pub fn on_chunk_load(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::CHUNK_LOAD, priority, f)
    }

    /// A chunk is about to be unloaded. See [`abi::event::CHUNK_UNLOAD`]. Since ABI 1.6.
    pub fn on_chunk_unload(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::CHUNK_UNLOAD, priority, f)
    }

    /// A world is about to be saved. See [`abi::event::WORLD_SAVE`]. Since ABI 1.6.
    pub fn on_world_save(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::WORLD_SAVE, priority, f)
    }

    /// A world was loaded (an extra world). See [`abi::event::WORLD_LOAD`]. Since ABI 1.8.
    pub fn on_world_load(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::WORLD_LOAD, priority, f)
    }

    /// An extra world is about to be unloaded; cancellable. See [`abi::event::WORLD_UNLOAD`].
    /// Since ABI 1.8.
    pub fn on_world_unload(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::WORLD_UNLOAD, priority, f)
    }

    /// A world's weather is about to change; cancellable. See [`abi::event::WEATHER_CHANGE`]. Since ABI 1.6.
    pub fn on_weather_change(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::WEATHER_CHANGE, priority, f)
    }

    /// A world's time is about to skip by sleeping; cancellable. See [`abi::event::TIME_SKIP`]. Since ABI 1.6.
    pub fn on_time_skip(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::TIME_SKIP, priority, f)
    }

    /// A nether portal is about to be created; cancellable. See [`abi::event::PORTAL_CREATE`]. Since ABI 1.6.
    pub fn on_portal_create(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PORTAL_CREATE, priority, f)
    }

    /// A player's menu opens; cancellable. See [`abi::event::INVENTORY_OPEN`]. Since ABI 1.6.
    pub fn on_inventory_open(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::INVENTORY_OPEN, priority, f)
    }

    /// A player clicks in its menu; cancellable. See [`abi::event::INVENTORY_CLICK`]. Since ABI 1.6.
    pub fn on_inventory_click(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::INVENTORY_CLICK, priority, f)
    }

    /// A player drags its cursor stack over slots; cancellable. See [`abi::event::INVENTORY_DRAG`]. Since ABI 1.6.
    pub fn on_inventory_drag(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::INVENTORY_DRAG, priority, f)
    }

    /// A player's menu closed. See [`abi::event::INVENTORY_CLOSE`]. Since ABI 1.6.
    pub fn on_inventory_close(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::INVENTORY_CLOSE, priority, f)
    }

    /// A crafting result is computed; the result can be changed. See [`abi::event::CRAFT_PREPARE`]. Since ABI 1.6.
    pub fn on_craft_prepare(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::CRAFT_PREPARE, priority, f)
    }

    /// A player takes a crafting result; cancellable. See [`abi::event::CRAFT_ITEM`]. Since ABI 1.6.
    pub fn on_craft_item(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::CRAFT_ITEM, priority, f)
    }

    /// An anvil result is computed; result and cost can be changed. See [`abi::event::ANVIL_PREPARE`]. Since ABI 1.6.
    pub fn on_anvil_prepare(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ANVIL_PREPARE, priority, f)
    }

    /// A furnace finishes an item; cancellable, the result can be changed. See [`abi::event::FURNACE_SMELT`]. Since ABI 1.6.
    pub fn on_furnace_smelt(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::FURNACE_SMELT, priority, f)
    }

    /// A furnace starts burning fuel; cancellable, the burn time can be changed. See [`abi::event::FURNACE_BURN`]. Since ABI 1.6.
    pub fn on_furnace_burn(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::FURNACE_BURN, priority, f)
    }

    /// A player takes furnace results; the experience can be changed. See [`abi::event::FURNACE_EXTRACT`]. Since ABI 1.6.
    pub fn on_furnace_extract(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::FURNACE_EXTRACT, priority, f)
    }

    /// A hopper moves an item; cancellable. See [`abi::event::INVENTORY_MOVE_ITEM`]. Since ABI 1.6.
    pub fn on_inventory_move_item(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::INVENTORY_MOVE_ITEM, priority, f)
    }

    /// A player enchants an item; cancellable. See [`abi::event::ENCHANT_ITEM`]. Since ABI 1.6.
    pub fn on_enchant_item(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENCHANT_ITEM, priority, f)
    }

    /// A brewing stand finishes; cancellable, the results can be changed. See [`abi::event::BREW`]. Since ABI 1.6.
    pub fn on_brew(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::BREW, priority, f)
    }

    /// A player is about to throw or shoot a projectile; cancellable. See [`abi::event::PLAYER_LAUNCH_PROJECTILE`]. Since ABI 1.6.
    pub fn on_player_launch_projectile(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_LAUNCH_PROJECTILE, priority, f)
    }

    /// A fishing rod action of a player; cancellable. See [`abi::event::PLAYER_FISH`]. Since ABI 1.6.
    pub fn on_player_fish(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::PLAYER_FISH, priority, f)
    }

    /// A totem is about to save an entity from death; cancellable. See [`abi::event::ENTITY_RESURRECT`]. Since ABI 1.6.
    pub fn on_entity_resurrect(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_RESURRECT, priority, f)
    }

    /// A player is about to dye a sheep; cancellable. See [`abi::event::SHEEP_DYE_WOOL`]. Since ABI 1.6.
    pub fn on_sheep_dye_wool(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::SHEEP_DYE_WOOL, priority, f)
    }

    /// An entity is about to drop an item on its own; cancellable. See [`abi::event::ENTITY_DROP_ITEM`]. Since ABI 1.6.
    pub fn on_entity_drop_item(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_DROP_ITEM, priority, f)
    }

    /// A command is about to change an attribute of a living entity; cancellable. See [`abi::event::ENTITY_ATTRIBUTE_CHANGE`]. Since ABI 1.13.
    pub fn on_entity_attribute_change(
        &mut self,
        priority: i32,
        f: impl FnMut(&mut P, &mut Ctx<'_>, &mut GameEvent<'_>) + Send + 'static,
    ) -> &mut Self {
        self.on_event(abi::event::ENTITY_ATTRIBUTE_CHANGE, priority, f)
    }

    /// Adds a chunk populator after a stage of the generation ([`abi::gen_stage`]). Since
    /// ABI 1.6.
    pub fn chunk_populator(&mut self, stage: u32, populator: impl ChunkPopulator) -> &mut Self {
        let populator: Box<Box<dyn ChunkPopulator>> = Box::new(Box::new(populator));
        let table = abi::ChunkPopulatorProvider {
            size: abi::size_of::<abi::ChunkPopulatorProvider>(),
            data_version: 0,
            stage,
            user: Box::into_raw(populator).cast(),
            populate: crate::game::populate,
        };
        // SAFETY: valid host function; the host copies the table, `user` lives for the life
        // of the library.
        let s = unsafe { (self.host.api.provide_chunk_populator)(self.host.id, &table) };
        self.record(s);
        self
    }

    /// Fills the `chunk-generator` slot with `provider`. Its generators work with numeric block
    /// state ids resolved through [`Server::block_state_id`].
    pub fn chunk_generator(&mut self, provider: impl ChunkGeneratorProvider) -> &mut Self {
        let provider: Box<Box<dyn ChunkGeneratorProvider>> = Box::new(Box::new(provider));
        let table = abi::ChunkGeneratorProvider {
            size: abi::size_of::<abi::ChunkGeneratorProvider>(),
            data_version: 0,
            user: Box::into_raw(provider).cast(),
            create: generator::create,
            generate: generator::generate,
            destroy: generator::destroy,
        };
        // SAFETY: valid host function; the host copies the table, `user` lives for the life
        // of the library.
        let s = unsafe { (self.host.api.provide_chunk_generator)(self.host.id, &table) };
        self.record(s);
        self
    }
}

unsafe extern "C" fn on_event<P: Plugin>(
    user: *mut c_void,
    ctx: *mut abi::Context,
    event: *mut abi::EventHeader,
) -> Status {
    guard(|| {
        if event.is_null() {
            return status::INVALID_ARGUMENT;
        }
        let mut ctx = Ctx::new(ctx);
        with_plugin::<P, _>(|p| {
            // SAFETY: `user` is the leaked handler of `subscribe`; calls are serialized by the
            // plugin lock held here.
            let handler = unsafe { &mut *user.cast::<Handler<P>>() };
            handler(p, &mut ctx, event);
        })
        .map_or(status::WRONG_PHASE, |()| status::OK)
    })
}

unsafe extern "C" fn permission(
    user: *mut c_void,
    player: abi::Uuid,
    node: abi::Str,
    out: *mut u32,
) -> Status {
    guard(|| {
        if out.is_null() {
            return status::INVALID_ARGUMENT;
        }
        // SAFETY: `user` is the leaked provider of `permission_provider`; the node is valid
        // for the call.
        let (f, node) = unsafe { (&*user.cast::<PermissionCheck>(), str(node)) };
        let answer = match f(Uuid::from_abi(player), node) {
            Permission::Allow => abi::permission::ALLOW,
            Permission::Deny => abi::permission::DENY,
            Permission::Undefined => abi::permission::UNDEFINED,
        };
        // SAFETY: checked above.
        unsafe { *out = answer };
        status::OK
    })
}
