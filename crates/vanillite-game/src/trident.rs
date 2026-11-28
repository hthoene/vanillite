//! Tridents in players' hands (`TridentItem`): charged like a bow for at least 10 ticks, they
//! are thrown (`ThrownTrident`, see [`crate::mob::fam::undead::trident`]) or, with Riptide in
//! water or rain, launch the player into a spin attack (`startAutoSpinAttack`) that hits the
//! first living entity it touches (`checkAutoSpinAttack`).

use vanillite_base::{Aabb, Vec3};
use vanillite_config::GameMode;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{EntityFlags, EntityId, NetworkIds};

use crate::entities::Entities;
use crate::item::{self, ItemStack};
use crate::level::Level;
use crate::mob::MobContext;
use crate::mob::missile::{MissileData, MissileKind};
use crate::survival::player::Env;

/// `TridentItem.THROW_THRESHOLD_TIME`.
const THROW_THRESHOLD: i32 = 10;

/// A riptide spin (`autoSpinAttackTicks`, `autoSpinAttackDmg`, `autoSpinAttackItemStack`).
#[derive(Clone, Debug, PartialEq)]
pub struct SpinAttack {
    pub ticks: i32,
    pub damage: f32,
    pub item: ItemStack,
    /// The inventory slot of the trident (`autoSpinAttackItemStack` is that very stack).
    pub index: usize,
    /// The box the player swept through in its last travel.
    pub swept: Aabb,
    /// `horizontalCollision` of that travel.
    pub horizontal_collision: bool,
}

/// `AABB.minmax`.
pub fn minmax(a: Aabb, b: Aabb) -> Aabb {
    Aabb::new(
        a.min_x.min(b.min_x),
        a.min_y.min(b.min_y),
        a.min_z.min(b.min_z),
        a.max_x.max(b.max_x),
        a.max_y.max(b.max_y),
        a.max_z.max(b.max_z),
    )
}

/// Whether the stack is a trident.
pub fn is_trident(stack: &ItemStack) -> bool {
    !stack.is_empty() && item::item_id(stack).name() == "minecraft:trident"
}

/// `nextDamageWillBreak`.
fn next_damage_breaks(stack: &ItemStack) -> bool {
    item::is_damageable(stack) && item::damage(stack) >= item::max_damage(stack) - 1
}

/// `EnchantmentHelper.getTridentSpinAttackStrength`.
pub fn riptide_strength(stack: &ItemStack) -> f32 {
    let combat = crate::mob::attack::combat_data();
    let enchants = &combat.enchants;
    crate::experience_orb::with_entity_random(|r| enchants.trident_spin_attack_strength(stack, r))
}

/// `isInWaterOrRain` of a player.
fn in_water_or_rain(ents: &Entities, env: &Env<'_>, player: EntityId) -> bool {
    let Some(p) = ents.player(player) else {
        return false;
    };
    if p.survival.fluids.in_water() {
        return true;
    }
    let Some(e) = ents.store.get(player) else {
        return false;
    };
    let pos = e.pos();
    let feet = vanillite_base::BlockPos::containing(pos.x, pos.y, pos.z);
    let head = vanillite_base::BlockPos::containing(pos.x, e.aabb().max_y, pos.z);
    (env.raining_at)(feet) || (env.raining_at)(head)
}

/// `TridentItem.use`: no use when the next damage breaks it, or with Riptide out of water and
/// rain; else it charges.
pub fn can_start(ents: &Entities, env: &Env<'_>, player: EntityId, stack: &ItemStack) -> bool {
    if next_damage_breaks(stack) {
        return false;
    }
    !(riptide_strength(stack) > 0.0 && !in_water_or_rain(ents, env, player))
}

/// `TridentItem.releaseUsing` after `held` ticks: throws the trident or launches the riptide.
pub fn release(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &NetworkIds,
    player: EntityId,
    hand_index: usize,
    held: i32,
) {
    if held < THROW_THRESHOLD {
        return;
    }
    let Some(stack) = ents
        .player(player)
        .map(|p| p.inventory.get(hand_index).clone())
    else {
        return;
    };
    let riptide = riptide_strength(&stack);
    let riding = ents.vehicle_of(player).is_some();
    if riptide > 0.0 && !(in_water_or_rain(ents, env, player) && !riding) {
        return;
    }
    if next_damage_breaks(&stack) {
        return;
    }
    if riptide == 0.0
        && let Some(uuid) = ents.store.get(player).map(|e| e.uuid())
        && !env.hooks.projectile_launch(
            crate::hooks::HookWorld::entities(ents),
            uuid,
            "minecraft:trident",
        )
    {
        return;
    }
    let combat = crate::mob::attack::combat_data();
    let enchants = &combat.enchants;
    let sound = enchants
        .trident_sound(&stack)
        .unwrap_or("minecraft:item.trident.throw")
        .to_owned();
    let creative = ents.player_mut(player).is_some_and(|p| {
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, stack.item),
            1,
        );
        p.game_mode == GameMode::Creative
    });
    // `hurtWithoutBreaking(1, player)`.
    if !creative {
        hurt_without_breaking(ents, env, player, hand_index);
    }
    if riptide == 0.0 {
        let Some(held_stack) = ents
            .player(player)
            .map(|p| p.inventory.get(hand_index).clone())
        else {
            return;
        };
        // `consumeAndReturn(1, player)`: creative players keep theirs.
        let thrown = held_stack.with_count(1);
        if !creative
            && let Some(p) = ents.player_mut(player)
            && let Some(s) = p.inventory.get_mut(hand_index)
        {
            s.shrink(1);
        }
        let Some(pos) = ents.store.get(player).map(|e| e.pos()) else {
            return;
        };
        let eye = crate::mob::missile::eye_height(ents, player);
        let start = Vec3::new(pos.x, pos.y + eye - f64::from(0.1_f32), pos.z);
        let mut data = MissileData::new(MissileKind::Trident);
        data.ints[3] = crate::mob::fam::undead::trident::loyalty(&thrown);
        data.foil = crate::mob::fam::undead::trident::has_foil(&thrown);
        // `AbstractArrow`: an `intangible_projectile` stack makes it creative only, like a
        // creative player's throw.
        let intangible = crate::crossbow::is_intangible(&thrown);
        data.item = thrown;
        data.pickup = if creative || intangible { 2 } else { 1 };
        let id = crate::mob::missile::spawn_from_rotation(
            ents,
            network_ids,
            player,
            start,
            0.0,
            2.5,
            1.0,
            data,
        );
        if let Some(id) = id {
            crate::survival::play_entity_sound(
                env.out,
                ents,
                None,
                id,
                &sound,
                SoundSource::Players,
                1.0,
                1.0,
            );
        }
        return;
    }
    // The riptide launch along the view.
    let Some(e) = ents.store.get(player) else {
        return;
    };
    let rot = e.rot();
    let on_ground = e.flags().contains(EntityFlags::ON_GROUND);
    let rad = crate::mob::DEG_TO_RAD;
    let sin = |a: f32| vanillite_base::math::sin(f64::from(a));
    let cos = |a: f32| vanillite_base::math::cos(f64::from(a));
    let mut xd = -sin(rot.y_rot * rad) * cos(rot.x_rot * rad);
    let mut yd = -sin(rot.x_rot * rad);
    let mut zd = cos(rot.y_rot * rad) * cos(rot.x_rot * rad);
    let dist = (xd * xd + yd * yd + zd * zd).sqrt();
    xd *= riptide / dist;
    yd *= riptide / dist;
    zd *= riptide / dist;
    let push = Vec3::new(f64::from(xd), f64::from(yd), f64::from(zd));
    let simulated = ents
        .player(player)
        .is_some_and(|p| p.survival.simulate_movement);
    if let Some(mut e) = ents.store.get_mut(player) {
        // `push`: the motion grows and the players tracking it hear of it (`needsSync`); the
        // riptiding player's client launches itself.
        let v = e.as_ref().velocity();
        e.set_velocity(v + push);
        e.set_flags(EntityFlags::HAS_IMPULSE, true);
    }
    start_spin_attack(ents, env, player, 20, 8.0, stack.clone(), hand_index);
    if on_ground && simulated {
        // `move(SELF, (0, 1.2, 0))`.
        crate::survival::player::lift_player(ents, env, player, f64::from(1.199_999_9_f32));
    }
    crate::survival::play_entity_sound(
        env.out,
        ents,
        None,
        player,
        &sound,
        SoundSource::Players,
        1.0,
        1.0,
    );
}

/// `ItemStack.hurtWithoutBreaking(1, player)`: one point, never the last one (Unbreaking may
/// save it).
fn hurt_without_breaking(ents: &mut Entities, env: &mut Env<'_>, player: EntityId, index: usize) {
    let Some(stack) = ents.player(player).map(|p| p.inventory.get(index).clone()) else {
        return;
    };
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Creative);
    let change = crate::durability::change(&stack, 1, creative, env.random);
    if change == 0 {
        return;
    }
    if let Some(p) = ents.player_mut(player)
        && let Some(s) = p.inventory.get_mut(index)
    {
        let d = (item::damage(s) + change).min(item::max_damage(s) - 1);
        crate::advancements::view::apply_damage(
            Some(&mut p.advancements),
            Some(&mut p.stats),
            s,
            d,
        );
    }
}

/// `Player.startAutoSpinAttack`: the spin flag (living flag 4), the parrots leave the
/// shoulders.
pub fn start_spin_attack(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    ticks: i32,
    damage: f32,
    item: ItemStack,
    index: usize,
) {
    let Entities { store, players, .. } = ents;
    let Some(mut entity) = store.get_mut(player) else {
        return;
    };
    let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
        return;
    };
    let swept = entity.as_ref().aabb();
    data.survival.spin_attack = Some(SpinAttack {
        ticks,
        damage,
        item,
        index,
        swept,
        horizontal_collision: false,
    });
    crate::survival::shoulder::remove_entities(&mut entity, data, env);
    entity.mark_data_dirty();
}

/// The `checkAutoSpinAttack` of the spinning players after their tick: the first living
/// entity in the swept box takes the player's attack and the spin ends with the player bounced
/// back; a wall ends it too.
pub fn tick_spin_attacks(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
) {
    let spinning: Vec<EntityId> = ents
        .players
        .iter()
        .filter(|(_, p)| p.survival.spin_attack.is_some())
        .map(|(id, _)| id)
        .collect();
    for player in spinning {
        let Some(spin) = ents
            .player(player)
            .and_then(|p| p.survival.spin_attack.clone())
        else {
            continue;
        };
        // `getEntities(this, box)`: no spectators.
        let mut found = Vec::new();
        ents.store.for_each_in(&spin.swept, false, |e| {
            let spectator = ents
                .player(e)
                .is_some_and(|p| p.game_mode == GameMode::Spectator);
            if e != player && !spectator {
                found.push(e);
            }
        });
        found.sort_unstable();
        let mut ticks = spin.ticks;
        // A wall ends the spin only when the box holds no entity at all.
        let any = !found.is_empty();
        let hit = found
            .into_iter()
            .find(|&e| ents.mob(e).is_some() || ents.player(e).is_some());
        if let Some(target) = hit {
            // `doAutoAttackOnTouch`: `Player.attack(target)` with the riptide trident.
            crate::player::item_use::attack_entity(level, ents, env, cx, player, target);
            ticks = 0;
            if let Some(mut e) = ents.store.get_mut(player) {
                let v = e.as_ref().velocity();
                e.set_velocity(v * -0.2);
            }
        } else if !any && spin.horizontal_collision {
            ticks = 0;
        }
        let Entities { store, players, .. } = &mut *ents;
        let Some(mut entity) = store.get_mut(player) else {
            continue;
        };
        let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
            continue;
        };
        if ticks <= 0 {
            data.survival.spin_attack = None;
            entity.mark_data_dirty();
        } else if let Some(s) = data.survival.spin_attack.as_mut() {
            s.ticks = ticks;
        }
    }
}

/// `getWeaponItem` of a spinning player: the riptide trident.
pub fn spin_weapon(ents: &Entities, player: EntityId) -> Option<ItemStack> {
    ents.player(player)?
        .survival
        .spin_attack
        .as_ref()
        .map(|s| s.item.clone())
}
