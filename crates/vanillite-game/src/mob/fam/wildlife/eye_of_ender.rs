//! Eyes of ender (`EnderEyeItem.use`, `EyeOfEnder`): thrown toward the nearest stronghold
//! (`#eye_of_ender_located`), they fly 12 blocks toward it and 8 up at most, and after 80 ticks
//! drop as an item (four in five) or shatter.
//!
//! Kind specific data: `vec` is the target, `ints[0]` is 1 while it has one, `ints[1]` is 1 when
//! it survives (`surviveAfterDeath`).

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{EntityId, NetworkIds, Rotation};
use vanillite_worldgen::behaviour::StateExt;

use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::mob::WorldAction;
use crate::mob::missile::{self, MissileData, MissileKind, MissileTick};
use crate::mob::physics::{self, Blocks};
use crate::player::item_use::Used;
use crate::survival::play_sound;
use crate::survival::player::Env;

/// `EnderEyeItem.use`: nothing when the player looks at an end portal frame (the frame is
/// filled by `useOn`), else the eye flies toward the nearest stronghold, if there is one.
pub fn use_item(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &NetworkIds,
    player: EntityId,
    stack: &mut ItemStack,
) -> Used {
    let Some(e) = ents.store.get(player) else {
        return Used::Pass;
    };
    let (pos, rot, height) = (e.pos(), e.rot(), e.dims().height);
    // `getPlayerPOVHitResult(level, player, Fluid.NONE)`.
    let eye = Vec3::new(
        pos.x,
        pos.y + crate::survival::player::eye_height(height),
        pos.z,
    );
    // `blockInteractionRange()` with the creative mode modifier.
    let range = ents.player(player).map_or(4.5, |p| {
        crate::player::play::interaction_ranges(&p.inventory, p.game_mode).0
    });
    let view = missile::direction_from_rotation(rot.x_rot, rot.y_rot, 0.0);
    if let Some((b, _)) = physics::clip(level as &dyn Blocks, eye, eye + view * range)
        && level
            .block(b)
            .is(vanillite_data::block::ids::END_PORTAL_FRAME)
    {
        return Used::Pass;
    }
    let Some(target) = nearest_stronghold(level, pos) else {
        return Used::Started;
    };
    let mut own = LegacyRandom::new(crate::mob::new_entity_seed());
    let uuid = crate::mob::new_uuid();
    let start = Vec3::new(pos.x, pos.y + f64::from(height) * 0.5, pos.z);
    let mut data = MissileData::new(MissileKind::EyeOfEnder);
    data.item = stack.with_count(1);
    // `signalTo(Vec3.atLowerCornerOf(target))`.
    let to = Vec3::new(
        f64::from(target.x),
        f64::from(target.y),
        f64::from(target.z),
    );
    let delta = to - start;
    let horizontal = (delta.x * delta.x + delta.z * delta.z).sqrt();
    data.vec = if horizontal > 12.0 {
        start
            + Vec3::new(
                delta.x / horizontal * 12.0,
                8.0,
                delta.z / horizontal * 12.0,
            )
    } else {
        to
    };
    data.ints[0] = 1;
    data.ints[1] = i32::from(own.next_int_bounded(5) > 0);
    data.random_state = own.state();
    // `gameEvent(PROJECTILE_SHOOT, eye.position(), Context.of(player))` before it is added.
    missile::shoot_event(env.out, start, Some(player));
    missile::spawn(
        ents,
        network_ids,
        uuid,
        start,
        Vec3::ZERO,
        Rotation::default(),
        data,
    );
    let pitch = 0.33 + env.random.next_float() * (0.5 - 0.33);
    play_sound(
        env.out,
        None,
        pos,
        "minecraft:entity.ender_eye.launch",
        SoundSource::Neutral,
        1.0,
        pitch,
        crate::survival::sound_seed(),
    );
    let creative = ents.player_mut(player).is_some_and(|p| {
        let distance_sqr = {
            let d = Vec3::new(
                f64::from(target.x),
                f64::from(target.y),
                f64::from(target.z),
            ) - Vec3::new(pos.x.floor(), pos.y.floor(), pos.z.floor());
            d.x * d.x + d.z * d.z
        };
        p.advancements
            .trigger(crate::advancements::trigger::Event::UsedEnderEye(
                distance_sqr,
            ));
        p.game_mode == GameMode::Creative
    });
    if !creative {
        stack.shrink(1);
    }
    if let Some(p) = ents.player_mut(player) {
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, stack.item),
            1,
        );
    }
    Used::Success
}

/// `findNearestMapStructure(#eye_of_ender_located, pos, 100, false)`.
fn nearest_stronghold(level: &Level, pos: Vec3) -> Option<BlockPos> {
    crate::level::locate::nearest_named(
        level,
        &["minecraft:stronghold"],
        BlockPos::containing(pos.x, pos.y, pos.z),
        crate::level::locate::MAX_STRUCTURE_SEARCH_RADIUS,
    )
}

/// `EyeOfEnder.tick`.
pub(super) fn tick(t: &mut MissileTick<'_, '_>) {
    let Some(d) = t.ents.missile(t.id).cloned() else {
        return;
    };
    let Some(e) = t.core() else {
        return;
    };
    let (pos, mut v) = (e.pos(), e.velocity());
    let new_pos = pos + v;
    if d.ints[0] != 0 {
        v = update_delta_movement(v, new_pos, d.vec);
    }
    if let Some(mut e) = t.ents.store.get_mut(t.id) {
        e.set_velocity(v);
        e.set_pos(new_pos);
    }
    if d.life <= 80 {
        return;
    }
    play_sound(
        t.env.out,
        None,
        new_pos,
        "minecraft:entity.ender_eye.death",
        SoundSource::Neutral,
        1.0,
        1.0,
        crate::survival::sound_seed(),
    );
    if let Some(d) = t.data() {
        d.removed = true;
    }
    if d.ints[1] != 0 {
        // `new ItemEntity(level, x, y, z, item)`: its own random motion, no pickup delay.
        let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
        t.env.out.push(crate::survival::Effect::DropItem {
            random: item_random,
            uuid,
            pos: new_pos,
            velocity,
            y_rot,
            pickup_delay: 0,
            stack: d.item.clone(),
        });
    } else {
        t.cx.actions.push(WorldAction::LevelEvent(
            2003,
            BlockPos::containing(new_pos.x, new_pos.y, new_pos.z),
            0,
        ));
    }
}

/// `EyeOfEnder.updateDeltaMovement`.
fn update_delta_movement(old: Vec3, position: Vec3, target: Vec3) -> Vec3 {
    let horizontal = Vec3::new(target.x - position.x, 0.0, target.z - position.z);
    let length = horizontal.length();
    let old_horizontal = (old.x * old.x + old.z * old.z).sqrt();
    let mut speed = old_horizontal + 0.0025 * (length - old_horizontal);
    let mut y = old.y;
    if length < 1.0 {
        speed *= 0.8;
        y *= 0.8;
    }
    let wanted_y = if position.y - old.y < target.y {
        1.0
    } else {
        -1.0
    };
    horizontal * (speed / length) + Vec3::new(0.0, y + (wanted_y - y) * 0.015, 0.0)
}
