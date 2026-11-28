//! Mob logic run from a block of the level (a dispenser shearing a sheep): the entities are
//! taken out of the level, the mob code runs with an [`Env`] built from the level, and what it
//! asks of the world is applied right away, as Vanilla does inside the block's tick.

use std::sync::OnceLock;

use vanillite_base::BlockPos;
use vanillite_data::block::BlockState;

use super::Level;
use crate::entities::Entities;
use crate::mob::{MobContext, WorldAction};
use crate::survival::Effect;
use crate::survival::blocks::{BlockAccess, BlockFacts};
use crate::survival::damage::DamageTypes;
use crate::survival::player::Env;
use crate::survival::rules::GameRules;

struct Blocks<'a>(&'a Level);

impl BlockAccess for Blocks<'_> {
    fn block_state(&self, pos: BlockPos) -> Option<BlockState> {
        self.0.is_loaded(pos).then(|| self.0.block(pos))
    }
}

/// Runs `f` with the level's entities, a mob environment over the level (its random, border,
/// difficulty and plugin hooks) and a mob context, then applies the effects, world actions
/// and removals it produced.
pub fn with_mob_env<R>(
    level: &mut Level,
    f: impl FnOnce(&Level, &mut Entities, &mut Env<'_>, &mut MobContext) -> R,
) -> R {
    static TYPES: OnceLock<DamageTypes> = OnceLock::new();
    static RULES: OnceLock<GameRules> = OnceLock::new();
    let types = TYPES.get_or_init(DamageTypes::built_in);
    let facts = BlockFacts::fluids_only();
    let rules = RULES.get_or_init(GameRules::default);
    let mut ents = std::mem::take(&mut level.entities);
    let mut random = level.lend_random();
    let mut out = Vec::new();
    let mut cx = MobContext::default();
    let r = {
        let level_ref: &Level = level;
        let blocks = Blocks(level_ref);
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
        let hooks = level_ref.hooks().cloned();
        let hooks: &dyn crate::hooks::GameHooks = match &hooks {
            Some(h) => &**h,
            None => &crate::hooks::NoHooks,
        };
        let mut env = Env {
            types,
            facts,
            blocks: &blocks,
            rules,
            difficulty: level_ref.spawner_env.difficulty,
            game_time: level_ref.game_time(),
            min_y: level_ref.min_y(),
            logical_height: level_ref.max_y() - level_ref.min_y() + 1,
            dimension: "",
            raining_at: &raining,
            border: Some(&level_ref.border),
            enter_gateway: &mut no_gateway,
            random: &mut random,
            out: &mut out,
            hooks,
            // The entities are lent to `f`: plugins read only the level's blocks here.
            hook_world: crate::hooks::HookWorld {
                level: Some(level_ref),
                entities: None,
            },
            display_name: &names,
            entity_team: &|_| None,
        };
        f(level_ref, &mut ents, &mut env, &mut cx)
    };
    level.restore_random(random);
    level.entities = ents;
    apply(level, out, cx);
    r
}

/// Applies what the mob code produced to the level.
fn apply(level: &mut Level, out: Vec<Effect>, mut cx: MobContext) {
    for effect in out {
        match effect {
            Effect::Tracking { entity, packet, .. } => {
                if let Some(pos) = level.entities.store.get(entity).map(|e| e.pos()) {
                    level.output().nearby_packets.push((pos, 160.0, packet));
                }
            }
            Effect::Nearby {
                pos, range, packet, ..
            } => level.output().nearby_packets.push((pos, range, packet)),
            Effect::DropItem {
                uuid,
                random,
                pos,
                velocity,
                y_rot,
                pickup_delay,
                stack,
            } => {
                let network_id = level.network_ids().next();
                let id = level.entities.spawn_item(
                    network_id,
                    uuid,
                    pos,
                    velocity,
                    y_rot,
                    crate::item_entity::ItemData::new(stack, pickup_delay, None, random),
                );
                level
                    .entities
                    .spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0);
            }
            Effect::Experience { pos, amount } => level.output().experience.push((pos, amount)),
            Effect::GameEvent {
                event,
                pos,
                context,
            } => level.game_event(event, pos, context),
            _ => {}
        }
    }
    for action in std::mem::take(&mut cx.actions) {
        if let WorldAction::Particles(p) = action {
            level.send_particles_packet(*p);
        } else {
            action.apply_to_level(level);
        }
    }
    for id in std::mem::take(&mut cx.discarded) {
        level.entities.despawn(id);
    }
}
