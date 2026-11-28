//! Sleeping and respawn points of players: using beds (`AbstractBedBlock.useWithoutItem`,
//! `ServerPlayer.startSleepInBed`), waking up (`stopSleepInBed`), the sleep part of the
//! player's tick, the world's sleep status and night skip (`SleepStatus`,
//! `ServerLevel.updateSleepingPlayerList`, `wakeUpAllPlayers`), and respawn anchors setting
//! the respawn point.
//!
//! Bed and anchor clicks reach this module after the player's packets of the tick
//! ([`run_block_uses`]): the block side ([`crate::block::bed`]) queues them because the
//! player's data is not in the level while its packets are handled.

use std::sync::Arc;

use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_data::block::{BlockState, ids};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{Animate, SystemChat};
use vanillite_protocol::text::Component;
use vanillite_world::entity::{EntityId, Pose};
use vanillite_worldgen::behaviour::StateExt;

use crate::advancements::trigger::{Event, TriggerKind};
use crate::block::bed::{self, BedRule, PlayerBlockUse};
use crate::hooks::GameHooks;
use crate::level::{Level, SOUND_BLOCKS};
use crate::stats::custom;
use crate::survival::player::RespawnConfig;

/// `ClientboundAnimatePacket.WAKE_UP`.
const WAKE_UP: u8 = 0;

/// `SleepStatus`: the players of a world that count and those asleep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SleepStatus {
    active: i32,
    sleeping: i32,
}

impl SleepStatus {
    /// `sleepersNeeded`: at least one, else the share of the active players.
    pub fn sleepers_needed(&self, percentage: i32) -> i32 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let needed = (self.active as f32 * percentage as f32 / 100.0).ceil() as i32;
        needed.max(1)
    }

    /// `areEnoughSleeping`.
    pub fn enough_sleeping(&self, percentage: i32) -> bool {
        self.sleeping >= self.sleepers_needed(percentage)
    }

    /// `amountSleeping`.
    pub fn amount_sleeping(&self) -> i32 {
        self.sleeping
    }

    /// `update(players)`: whether the counts changed while someone sleeps or slept.
    fn update(&mut self, level: &Level) -> bool {
        let (old_active, old_sleeping) = (self.active, self.sleeping);
        self.active = 0;
        self.sleeping = 0;
        let mut ignored = 0;
        for (_, p) in level.entities.players.iter() {
            if p.game_mode != GameMode::Spectator {
                self.active += 1;
                if p.survival.sleeping_pos.is_some() {
                    self.sleeping += 1;
                } else if p.survival.sleeping_ignored {
                    ignored += 1;
                }
            }
        }
        // Bukkit: players ignored as sleeping count once someone really sleeps.
        if self.sleeping > 0 {
            self.sleeping += ignored;
        }
        (old_sleeping > 0 || self.sleeping > 0)
            && (old_active != self.active || old_sleeping != self.sleeping)
    }
}

/// Whether the player sleeps (`isSleeping`).
pub fn is_sleeping(level: &Level, player: EntityId) -> bool {
    level
        .entities
        .player(player)
        .is_some_and(|p| p.survival.sleeping_pos.is_some())
}

/// `getSleepTimer` of a sleeping player (cats wait for 100 before their morning gift).
pub fn sleep_timer(level: &Level, player: EntityId) -> Option<i32> {
    let s = &level.entities.player(player)?.survival;
    s.sleeping_pos.map(|_| s.sleep_timer)
}

fn send_overlay(level: &Level, player: EntityId, message: Component) {
    if let Some(p) = level.entities.player(player) {
        p.connection.sink.send(&SystemChat {
            content: message,
            overlay: true,
        });
    }
}

fn translatable(key: &str) -> Component {
    Component::translatable(key, vec![])
}

/// `ServerLevel.canSleepThroughNights`.
fn can_sleep_through_nights(level: &Level) -> bool {
    level.settings.players_sleeping_percentage <= 100
}

/// `ServerLevel.updateSleepingPlayerList` with `announceSleepStatus`.
pub fn update_sleeping_player_list(level: &mut Level) {
    if level.entities.players.is_empty() {
        return;
    }
    let mut status = level.sleep_status;
    let changed = status.update(level);
    level.sleep_status = status;
    if !changed || !can_sleep_through_nights(level) {
        return;
    }
    let percentage = level.settings.players_sleeping_percentage;
    let message = if status.enough_sleeping(percentage) {
        translatable("sleep.skipping_night")
    } else {
        Component::translatable_args(
            "sleep.players_sleeping",
            vec![
                vanillite_protocol::text::Arg::Int(status.amount_sleeping()),
                vanillite_protocol::text::Arg::Int(status.sleepers_needed(percentage)),
            ],
        )
    };
    let packet = SharedPacket::of(&SystemChat {
        content: message,
        overlay: true,
    });
    for (_, p) in level.entities.players.iter() {
        p.connection.sink.send_shared(&packet);
    }
}

/// The bed and anchor clicks of the players' packets.
pub fn run_block_uses(level: &mut Level, hooks: &dyn GameHooks) {
    for (player, pos, kind) in level.take_player_block_uses() {
        match kind {
            PlayerBlockUse::Bed => use_bed(level, player, pos, hooks),
            PlayerBlockUse::AnchorSpawn => anchor_spawn(level, player, pos, hooks),
        }
    }
}

/// `AbstractBedBlock.useWithoutItem` (server side) on the bed whose head is at `pos`.
fn use_bed(level: &mut Level, player: EntityId, pos: BlockPos, hooks: &dyn GameHooks) {
    let state = level.block(pos);
    if !bed::is_bed(state) {
        return;
    }
    let rule = bed::bed_rule(level, state).clone();
    if rule.destroy_on_use {
        if let Some(message) = rule.error_message.clone() {
            send_overlay(level, player, message);
        }
        if bed::is_straw_bed(state) {
            destroy_straw_bed(level, pos);
        } else {
            bed::explode_bed(level, pos, state);
        }
        return;
    }
    if state.flag("occupied") {
        if !kick_villager_out_of_bed(level, pos) {
            send_overlay(level, player, translatable("block.minecraft.bed.occupied"));
        }
        return;
    }
    if let Err(Some(message)) = start_sleep_in_bed(level, player, state, &rule, pos, hooks) {
        send_overlay(level, player, *message);
    }
}

/// `StrawBedBlock.destroyBed`.
fn destroy_straw_bed(level: &mut Level, pos: BlockPos) {
    level.play_block_sound(
        None,
        pos,
        crate::block::sound_id("block.straw_bed.break_leave"),
        SOUND_BLOCKS,
        1.0,
        1.0,
    );
    level.set_block_and_update(pos, ids::AIR.default_state());
}

/// `kickVillagerOutOfBed`: the first sleeping villager in the bed's block wakes.
fn kick_villager_out_of_bed(level: &mut Level, pos: BlockPos) -> bool {
    let area = Aabb::of_block(pos);
    let mut found = None;
    level.entities.store.for_each_in(&area, false, |id| {
        if found.is_none()
            && level
                .entities
                .mob(id)
                .is_some_and(crate::mob::fam::village::villager::is_villager_sleeping)
        {
            found = Some(id);
        }
    });
    let Some(villager) = found else {
        return false;
    };
    if let Some(m) = level.entities.mob_mut(villager) {
        crate::mob::fam::village::villager::kick_out(m);
    }
    true
}

/// The entity types of class `Monster`.
const MONSTERS: [&str; 34] = [
    "minecraft:blaze",
    "minecraft:bogged",
    "minecraft:breeze",
    "minecraft:cave_spider",
    "minecraft:creaking",
    "minecraft:creeper",
    "minecraft:drowned",
    "minecraft:elder_guardian",
    "minecraft:enderman",
    "minecraft:endermite",
    "minecraft:evoker",
    "minecraft:giant",
    "minecraft:guardian",
    "minecraft:husk",
    "minecraft:illusioner",
    "minecraft:parched",
    "minecraft:piglin",
    "minecraft:piglin_brute",
    "minecraft:pillager",
    "minecraft:ravager",
    "minecraft:silverfish",
    "minecraft:skeleton",
    "minecraft:spider",
    "minecraft:stray",
    "minecraft:vex",
    "minecraft:vindicator",
    "minecraft:warden",
    "minecraft:witch",
    "minecraft:wither",
    "minecraft:wither_skeleton",
    "minecraft:zoglin",
    "minecraft:zombie",
    "minecraft:zombie_villager",
    "minecraft:zombified_piglin",
];

/// `Monster.isPreventingPlayerRest` of a monster within 8 blocks around (5 up and down) the
/// bed: every monster, a zombified piglin only when angry at the player.
fn monsters_near(level: &Level, player: EntityId, pos: BlockPos) -> bool {
    let center = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    );
    let area = Aabb::new(
        center.x - 8.0,
        center.y - 5.0,
        center.z - 8.0,
        center.x + 8.0,
        center.y + 5.0,
        center.z + 8.0,
    );
    let uuid = level.entities.store.get(player).map(|e| e.uuid());
    let mut found = false;
    level.entities.store.for_each_in(&area, false, |id| {
        if found {
            return;
        }
        let Some(e) = level.entities.store.get(id) else {
            return;
        };
        let name = e.kind().name();
        if !MONSTERS.contains(&name) {
            return;
        }
        found = name != "minecraft:zombified_piglin"
            || level
                .entities
                .mob(id)
                .is_some_and(|m| m.anger_target.is_some() && m.anger_target == uuid);
    });
    found
}

/// `Player.freeAt`: the block does not suffocate.
fn free_at(level: &Level, pos: BlockPos) -> bool {
    let state = level.block(pos);
    let block = state.block();
    if [ids::FARMLAND, ids::SOUL_SAND, ids::DIRT_PATH, ids::MUD].contains(&block) {
        return false;
    }
    if block == ids::PISTON || block == ids::STICKY_PISTON {
        return state.value("extended") != Some("false");
    }
    let full = state.has(vanillite_data::block::flags::COLLISION_FULL_BLOCK);
    !(full
        && !crate::survival::blocks::never_suffocating(block)
        && crate::player::play::game_data()
            .is_none_or(|d| d.block_in_tag(block.0, "minecraft:causes_suffocation")))
}

/// `ServerPlayer.startSleepInBed`: `Err` with the message of the problem.
fn start_sleep_in_bed(
    level: &mut Level,
    player: EntityId,
    state: BlockState,
    rule: &BedRule,
    pos: BlockPos,
    hooks: &dyn GameHooks,
) -> Result<(), Option<Box<Component>>> {
    let (Some(e), Some(p)) = (
        level.entities.store.get(player),
        level.entities.player(player),
    ) else {
        return Err(None);
    };
    if p.survival.sleeping_pos.is_some() || p.survival.health <= 0.0 {
        return Err(None);
    }
    let (at, rot, uuid) = (e.pos(), e.rot(), e.uuid());
    let creative = p.game_mode == GameMode::Creative;
    let dark = bed::is_dark_outside(level);
    let can_sleep = rule.can_sleep.test(dark);
    let can_set_spawn = rule.can_set_spawn.test(dark);
    let facing = bed::facing(state);
    // `bedInRange`: the head or the foot within 3 blocks (2 up and down).
    let reachable = |b: BlockPos| {
        (at.x - (f64::from(b.x) + 0.5)).abs() <= 3.0
            && (at.y - f64::from(b.y)).abs() <= 2.0
            && (at.z - (f64::from(b.z) + 0.5)).abs() <= 3.0
    };
    let [fx, _, fz] = facing.opposite().normal();
    if !reachable(pos) && !reachable(pos.offset(fx, 0, fz)) {
        return Err(Some(Box::new(translatable(
            "block.minecraft.bed.too_far_away",
        ))));
    }
    // `bedBlocked`.
    let above = pos.offset(0, 1, 0);
    if !free_at(level, above) || !free_at(level, above.offset(fx, 0, fz)) {
        return Err(Some(Box::new(translatable(
            "block.minecraft.bed.obstructed",
        ))));
    }
    if can_set_spawn {
        let config = RespawnConfig {
            dimension: level.settings.dimension.clone(),
            pos,
            yaw: rot.y_rot,
            pitch: rot.x_rot,
            forced: false,
        };
        set_respawn_position(level, player, config, true, state, hooks);
    }
    if !can_sleep {
        return Err(rule.error_message.clone().map(Box::new));
    }
    if !creative && monsters_near(level, player, pos) {
        return Err(Some(Box::new(translatable("block.minecraft.bed.not_safe"))));
    }
    if hooks.bed_enter(uuid, pos, state) && start_sleeping(level, player, pos) {
        let stat = if bed::is_straw_bed(state) {
            custom::SLEEP_IN_STRAW_BED
        } else {
            custom::SLEEP_IN_BED
        };
        if let Some(p) = level.entities.player_mut(player) {
            p.survival.sleep_timer = 0;
            p.stats.award(stat, 1);
        }
        if can_set_spawn {
            crate::advancements::view::fire(
                &mut level.entities,
                player,
                TriggerKind::SLEPT_IN_BED,
                |_| Event::Player(TriggerKind::SLEPT_IN_BED),
            );
        }
    }
    if !can_sleep_through_nights(level) {
        send_overlay(level, player, translatable("sleep.not_possible"));
    }
    update_sleeping_player_list(level);
    Ok(())
}

/// `LivingEntity.startSleeping` with `ServerPlayer`'s statistic reset and position sync.
fn start_sleeping(level: &mut Level, player: EntityId, pos: BlockPos) -> bool {
    let state = level.block(pos);
    let Some(height) = bed::sleep_height(level, state, pos) else {
        return false;
    };
    if level.entities.riding.vehicle_of(player).is_some() {
        crate::riding::stop_riding_in_level(level, player);
    }
    level.set_block_and_update(pos, state.set_flag("occupied", true));
    if let Some(mut e) = level.entities.store.get_mut(player) {
        e.set_pos(Vec3::new(
            f64::from(pos.x) + 0.5,
            f64::from(pos.y) + height + 0.125,
            f64::from(pos.z) + 0.5,
        ));
        e.set_pose(Pose::Sleeping);
        e.set_dims(crate::player::dimensions(Pose::Sleeping));
        e.set_velocity(Vec3::ZERO);
        e.mark_data_dirty();
    }
    if let Some(p) = level.entities.player_mut(player) {
        p.survival.sleeping_pos = Some(pos);
        p.stats.reset(custom::TIME_SINCE_REST);
    }
    sync_position(level, player);
    true
}

/// `ServerGamePacketListenerImpl.teleport` to where the player is.
fn sync_position(level: &mut Level, player: EntityId) {
    let Some(e) = level.entities.store.get(player) else {
        return;
    };
    let flags = e.flags();
    let mut body = crate::player::Body {
        pos: e.pos(),
        velocity: e.velocity(),
        y_rot: e.rot().y_rot,
        x_rot: e.rot().x_rot,
        on_ground: flags.contains(vanillite_world::entity::EntityFlags::ON_GROUND),
        horizontal_collision: false,
        fall_flying: flags.contains(vanillite_world::entity::EntityFlags::FALL_FLYING),
    };
    let (pos, y_rot, x_rot) = (body.pos, body.y_rot, body.x_rot);
    let Some(p) = level.entities.player_mut(player) else {
        return;
    };
    let sink = Arc::clone(&p.connection.sink);
    p.connection
        .movement
        .teleport(&mut body, pos, y_rot, x_rot, sink.as_ref());
    p.connection.movement.reset_position(body.pos);
    if let Some(mut e) = level.entities.store.get_mut(player) {
        e.set_velocity(body.velocity);
    }
}

/// `ServerPlayer.stopSleepInBed(forcefulWakeUp, updateLevelList)`: out of the bed to where the
/// player can stand; `forceful` restarts the sleep timer, else it runs on from 100 (the
/// screen fades back).
pub fn stop_sleep_in_bed(
    level: &mut Level,
    player: EntityId,
    forceful: bool,
    update_list: bool,
    hooks: &dyn GameHooks,
) {
    let Some(bed_pos) = level
        .entities
        .player(player)
        .and_then(|p| p.survival.sleeping_pos)
    else {
        return;
    };
    let Some(e) = level.entities.store.get(player) else {
        return;
    };
    let (network_id, uuid, at, yaw) = (e.network_id().0, e.uuid(), e.pos(), e.rot().y_rot);
    level.output().nearby_packets.push((
        at,
        64.0,
        SharedPacket::of(&Animate {
            entity_id: network_id,
            action: WAKE_UP,
        }),
    ));
    // `LivingEntity.stopSleeping`.
    let state = level.block(bed_pos);
    if level.is_loaded(bed_pos) && bed::is_bed(state) {
        level.set_block_and_update(bed_pos, state.set_flag("occupied", false));
        if bed::bed_rule(level, state).destroy_on_leave && bed::is_straw_bed(state) {
            destroy_straw_bed(level, bed_pos);
        }
        let dims = vanillite_world::entity::Dimensions::of(vanillite_data::entity::ids::PLAYER);
        let stand = bed::stand_up_position(level, bed_pos, bed::facing(state), yaw, dims, true)
            .unwrap_or(Vec3::new(
                f64::from(bed_pos.x) + 0.5,
                f64::from(bed_pos.y) + 1.1,
                f64::from(bed_pos.z) + 0.5,
            ));
        let look = Vec3::new(
            f64::from(bed_pos.x) + 0.5 - stand.x,
            f64::from(bed_pos.y) - stand.y,
            f64::from(bed_pos.z) + 0.5 - stand.z,
        )
        .normalize();
        #[allow(clippy::cast_possible_truncation)]
        let yaw = vanillite_base::math::wrap_degrees_f(
            (crate::mob::atan2_deg(look.z, look.x) - 90.0) as f32,
        );
        if let Some(mut e) = level.entities.store.get_mut(player) {
            e.set_pos(stand);
            let mut rot = e.as_ref().rot();
            rot.y_rot = yaw;
            rot.x_rot = 0.0;
            // `Player.aiStep`: the head follows the body's yaw.
            rot.y_head_rot = yaw;
            e.set_rot(rot);
        }
    }
    let dead = level
        .entities
        .player(player)
        .is_some_and(|p| p.survival.health <= 0.0);
    if let Some(mut e) = level.entities.store.get_mut(player) {
        if !dead {
            e.set_pose(Pose::Standing);
            e.set_dims(crate::player::dimensions(Pose::Standing));
        }
        e.mark_data_dirty();
    }
    if let Some(p) = level.entities.player_mut(player) {
        p.survival.sleeping_pos = None;
        p.survival.wake = false;
        p.survival.sleep_timer = if forceful { 0 } else { 100 };
    }
    if update_list {
        update_sleeping_player_list(level);
    }
    sync_position(level, player);
    hooks.bed_leave(uuid, bed_pos, state);
}

/// The sleep part of the players' ticks (`Player.tick`, `LivingEntity.tick`): the sleep
/// timer, waking when the bed is gone or does not allow sleeping any more, and players woken
/// by damage or death.
pub fn tick_players(level: &mut Level, hooks: &dyn GameHooks) {
    if level.entities.players.is_empty() {
        return;
    }
    let ids: Vec<EntityId> = level.entities.players.iter().map(|(id, _)| id).collect();
    for id in ids {
        let Some(p) = level.entities.player_mut(id) else {
            continue;
        };
        let s = &mut p.survival;
        let Some(bed_pos) = s.sleeping_pos else {
            if s.sleep_timer > 0 {
                s.sleep_timer += 1;
                if s.sleep_timer >= 110 {
                    s.sleep_timer = 0;
                }
            }
            continue;
        };
        s.sleep_timer = (s.sleep_timer + 1).min(100);
        let woken = std::mem::take(&mut s.wake);
        if woken {
            stop_sleep_in_bed(level, id, true, true, hooks);
            continue;
        }
        // `Player.tick`: the bed the player is in no longer allows sleeping.
        let Some(at) = level.entities.store.get(id).map(|e| e.pos()) else {
            continue;
        };
        let inside = level.block(BlockPos::containing(at.x, at.y, at.z));
        if bed::is_bed(inside)
            && !bed::bed_rule(level, inside)
                .can_sleep
                .test(bed::is_dark_outside(level))
        {
            stop_sleep_in_bed(level, id, false, true, hooks);
            continue;
        }
        // `LivingEntity.tick`: the bed is gone.
        let state = level.block(bed_pos);
        if bed::sleep_height(level, state, bed_pos).is_none() {
            stop_sleep_in_bed(level, id, true, true, hooks);
            continue;
        }
        // `LivingEntity.aiStep`: sleepers look straight.
        if let Some(mut e) = level.entities.store.get_mut(id) {
            let mut rot = e.as_ref().rot();
            rot.x_rot = 0.0;
            e.set_rot(rot);
        }
    }
}

/// The night skip of `ServerLevel.tick`: enough players sleep and enough of them long
/// enough (100 ticks). Wakes them all and returns `true`; the caller moves the clock to its
/// `wake_up_from_sleep` marker and clears the weather.
pub fn skip_night(level: &mut Level, hooks: &dyn GameHooks) -> bool {
    let percentage = level.settings.players_sleeping_percentage;
    let status = level.sleep_status;
    if !status.enough_sleeping(percentage) {
        return false;
    }
    let sleepers: Vec<EntityId> = level
        .entities
        .players
        .iter()
        .filter(|(_, p)| p.survival.sleeping_pos.is_some())
        .map(|(id, _)| id)
        .collect();
    let deep = level
        .entities
        .players
        .iter()
        .filter(|(_, p)| {
            (p.survival.sleeping_pos.is_some() && p.survival.sleep_timer >= 100)
                || (p.survival.sleeping_ignored && p.survival.sleeping_pos.is_none())
        })
        .count();
    if i32::try_from(deep).unwrap_or(i32::MAX) < status.sleepers_needed(percentage) {
        return false;
    }
    // `wakeUpAllPlayers`.
    level.sleep_status.sleeping = 0;
    for id in sleepers {
        stop_sleep_in_bed(level, id, false, false, hooks);
    }
    true
}

/// `ServerPlayer.setRespawnPosition(config, showMessage)` for a bed or anchor (`state`):
/// plugins may refuse it. Returns whether it was set.
pub fn set_respawn_position(
    level: &mut Level,
    player: EntityId,
    config: RespawnConfig,
    show_message: bool,
    state: BlockState,
    hooks: &dyn GameHooks,
) -> bool {
    let Some(uuid) = level.entities.store.get(player).map(|e| e.uuid()) else {
        return false;
    };
    let Some(p) = level.entities.player(player) else {
        return false;
    };
    let same = p
        .survival
        .respawn
        .as_ref()
        .is_some_and(|r| r.dimension == config.dimension && r.pos == config.pos);
    if !same && !hooks.spawn_change(uuid, config.pos, state) {
        return false;
    }
    let Some(p) = level.entities.player_mut(player) else {
        return false;
    };
    if show_message && !same {
        p.connection.sink.send(&SystemChat {
            content: translatable("block.minecraft.set_spawn"),
            overlay: false,
        });
    }
    p.survival.respawn = Some(config);
    true
}

/// `RespawnAnchorBlock.useWithoutItem` for a player whose respawn point is elsewhere.
fn anchor_spawn(level: &mut Level, player: EntityId, pos: BlockPos, hooks: &dyn GameHooks) {
    let state = level.block(pos);
    if state.block() != ids::RESPAWN_ANCHOR {
        return;
    }
    let config = RespawnConfig {
        dimension: level.settings.dimension.clone(),
        pos,
        yaw: 0.0,
        pitch: 0.0,
        forced: false,
    };
    if set_respawn_position(level, player, config, true, state, hooks) {
        level.play_block_sound(
            None,
            pos,
            crate::block::sound_id("block.respawn_anchor.set_spawn"),
            SOUND_BLOCKS,
            1.0,
            1.0,
        );
    }
}
