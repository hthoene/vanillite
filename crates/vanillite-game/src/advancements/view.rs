//! Views of live entities for the trigger predicates (`EntityPredicate.createContext`) and the
//! way the game fires a player's triggers (`CriteriaTriggers.X.trigger(player, ...)`): the
//! event is only built when an unfinished criterion of the player listens for its type.

use std::borrow::Cow;

use vanillite_world::entity::{EntityFlags, EntityId, Pose};

use super::predicate::{DamageEvent, DamageView, EffectView, EntityView, PlayerFacts};
use super::trigger::{Event, Snapshot, TriggerKind};
use crate::entities::Entities;
use crate::item::{EquipmentSlot, ItemStack};
use crate::mob::Kind;
use crate::survival::damage::{DamageSource, DamageTypes};

/// Whether `player` has an unfinished criterion of `kind`.
pub fn listens(ents: &Entities, player: EntityId, kind: TriggerKind) -> bool {
    ents.player(player)
        .is_some_and(|p| p.advancements.listens(kind))
}

/// `CriteriaTriggers.X.trigger(player, ...)`: queues the event `make` builds for `player` if
/// anything of the player listens for `kind`.
pub fn fire(
    ents: &mut Entities,
    player: EntityId,
    kind: TriggerKind,
    make: impl FnOnce(&Entities) -> Event,
) {
    if !listens(ents, player, kind) {
        return;
    }
    let event = make(ents);
    if let Some(p) = ents.player_mut(player) {
        p.advancements.trigger(event);
    }
}

/// The view of an entity with its vehicle (and the vehicle's passengers).
pub fn snapshot(ents: &Entities, id: EntityId) -> Option<Snapshot> {
    let mut view = base(ents, id)?;
    if let Some(vehicle) = ents.riding.vehicle_of(id)
        && let Some(mut v) = base(ents, vehicle)
    {
        v.passengers = ents
            .riding
            .passengers_of(vehicle)
            .iter()
            .filter_map(|&p| base(ents, p))
            .collect();
        view.vehicle = Some(Box::new(v));
    }
    view.passengers = ents
        .riding
        .passengers_of(id)
        .iter()
        .filter_map(|&p| base(ents, p))
        .collect();
    Some(Box::new(view))
}

/// The views of entities (`createContext` of each).
pub fn snapshots(ents: &Entities, ids: &[EntityId]) -> Vec<EntityView<'static>> {
    ids.iter()
        .filter_map(|&id| snapshot(ents, id).map(|b| *b))
        .collect()
}

fn effects_of(ents: &Entities, id: EntityId) -> Vec<EffectView> {
    ents.effects(id)
        .map(|fx| {
            fx.iter()
                .map(|e| EffectView {
                    effect: e.effect,
                    amplifier: i32::from(e.amplifier),
                    duration: e.duration,
                    ambient: e.ambient,
                    visible: e.visible,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The equipment in view order (head, chest, legs, feet, body, main hand, off hand).
const VIEW_SLOTS: [EquipmentSlot; 7] = [
    EquipmentSlot::Head,
    EquipmentSlot::Chest,
    EquipmentSlot::Legs,
    EquipmentSlot::Feet,
    EquipmentSlot::Body,
    EquipmentSlot::MainHand,
    EquipmentSlot::OffHand,
];

/// Mobs whose `variant` save field is an exact entity component (`<type>/variant`).
fn has_variant_component(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Cat
            | Kind::Wolf
            | Kind::Frog
            | Kind::Pig
            | Kind::Cow
            | Kind::Chicken
            | Kind::Nautilus
            | Kind::ZombieNautilus
    )
}

/// The view of a mob's target (without its own target, vehicle or passengers).
fn target_view(ents: &Entities, id: EntityId) -> Option<Box<EntityView<'static>>> {
    base_inner(ents, id, false).map(Box::new)
}

/// The view of one entity without vehicle and passengers.
fn base(ents: &Entities, id: EntityId) -> Option<EntityView<'static>> {
    base_inner(ents, id, true)
}

/// [`base`]; `with_target` follows a mob's target one level.
fn base_inner(ents: &Entities, id: EntityId, with_target: bool) -> Option<EntityView<'static>> {
    let e = ents.store.get(id)?;
    let flags = e.flags();
    let mut v = EntityView::new(e.kind().name(), e.pos());
    v.on_ground = flags.contains(EntityFlags::ON_GROUND);
    v.on_fire = flags.contains(EntityFlags::ON_FIRE);
    v.crouching = e.pose() == Pose::Crouching;
    v.sprinting = flags.contains(EntityFlags::SPRINTING);
    v.swimming = flags.contains(EntityFlags::SWIMMING);
    v.fall_flying = flags.contains(EntityFlags::FALL_FLYING);
    if let Some(bolt) = ents.bolt(id) {
        v.lightning_fires = Some(bolt.blocks_set_on_fire);
        if with_target {
            v.struck = bolt
                .hit_entities
                .iter()
                .filter(|&&h| crate::lightning::alive(ents, h))
                .filter_map(|&h| base_inner(ents, h, false))
                .collect();
        }
    }
    v.tags = ents.tags_of(id).to_vec();
    let nbt = super::predicate::entity_nbt_wanted()
        .then(|| crate::mob::entity_nbt::save_full(ents, id))
        .flatten();
    if let Some(villager) = nbt
        .as_ref()
        .and_then(|n| n.get("VillagerData"))
        .and_then(|d| match d {
            vanillite_nbt::Tag::Compound(c) => c.get("type").and_then(vanillite_nbt::Tag::as_str),
            _ => None,
        })
    {
        v.components
            .push(("minecraft:villager/variant", villager.to_owned()));
    }
    v.nbt = nbt.map(Box::new);
    let m = crate::spear::known_movement(ents, id);
    v.movement = vanillite_base::Vec3::new(m.x * 20.0, m.y * 20.0, m.z * 20.0);
    if let Some(b) = ents.missile(id).and_then(|d| d.bobber.as_deref()) {
        v.open_water = Some(b.open_water);
    }
    v.living = matches!(v.kind, "minecraft:armor_stand" | "minecraft:mannequin");
    if let Some(m) = ents.mob(id) {
        v.living = true;
        v.in_water = m.fluids.in_water();
        v.baby = m.baby;
        v.fall_distance = m.fall_distance;
        v.tick_count = m.tick_count;
        if m.kind == Kind::Sheep {
            v.sheared = Some(m.sheared);
        }
        if matches!(m.kind, Kind::Slime | Kind::MagmaCube | Kind::SulfurCube) {
            v.cube_size = Some(crate::mob::fam::undead::cubes::size(m));
        }
        if m.kind.is_raider() {
            use crate::mob::fam::illager::{raider, st};
            let s = st(m);
            let captain =
                s.patrol_leader && raider::is_ominous_banner(m.equipment.get(EquipmentSlot::Head));
            v.raider = Some((s.raid.is_some(), captain));
        }
        if with_target {
            v.target = m.target.and_then(|t| target_view(ents, t));
        }
        v.on_fire = m.is_on_fire();
        for (slot, s) in v.equipment.iter_mut().zip(VIEW_SLOTS) {
            *slot = Cow::Owned(m.equipment.get(s).clone());
        }
        v.effects = effects_of(ents, id);
        if has_variant_component(m.kind) {
            let tag = crate::mob::nbt::save_mob(&e, m);
            if let Some(variant) = tag.get("variant").and_then(vanillite_nbt::Tag::as_str) {
                let name: &'static str = match m.kind {
                    Kind::Cat => "minecraft:cat/variant",
                    Kind::Wolf => "minecraft:wolf/variant",
                    Kind::Frog => "minecraft:frog/variant",
                    Kind::Pig => "minecraft:pig/variant",
                    Kind::Cow => "minecraft:cow/variant",
                    Kind::Chicken => "minecraft:chicken/variant",
                    _ => "minecraft:zombie_nautilus/variant",
                };
                v.components.push((name, variant.to_owned()));
            }
        }
    } else if let Some(p) = ents.player(id) {
        let inv = &p.inventory;
        let [feet, legs, chest, head] = &inv.armor;
        v.equipment = [
            Cow::Owned(head.clone()),
            Cow::Owned(chest.clone()),
            Cow::Owned(legs.clone()),
            Cow::Owned(feet.clone()),
            Cow::Owned(ItemStack::EMPTY),
            Cow::Owned(inv.selected_item().clone()),
            Cow::Owned(inv.offhand.clone()),
        ];
        v.living = true;
        v.flying = p.survival.flying;
        v.on_fire = p.survival.fire_ticks > 0;
        v.in_water = p.survival.fluids.in_water();
        v.fall_distance = p.survival.fall_distance;
        v.tick_count = p.survival.tick_count;
        v.player = Some(PlayerFacts {
            game_mode: p.game_mode,
            level: p.survival.xp.level,
            stats: None,
            food: (p.survival.food.food_level, p.survival.food.saturation),
            recipes: None,
            input: p.connection.vehicle.input,
            own: false,
        });
        v.effects = effects_of(ents, id);
    }
    Some(v)
}

/// `DamageSource` as the damage source predicates see it.
pub fn damage_view(ents: &Entities, types: &DamageTypes, source: &DamageSource) -> DamageView {
    let t = types.get(source.kind);
    DamageView {
        kind: t.id.clone(),
        tags: t.tags,
        direct: source.direct.and_then(|d| snapshot(ents, d)),
        cause: source.cause.and_then(|c| snapshot(ents, c)),
        is_direct: source.cause == source.direct,
        position: source.position,
    }
}

/// A damage event (`DamagePredicate.matches` arguments).
pub fn damage_event(
    ents: &Entities,
    types: &DamageTypes,
    source: &DamageSource,
    dealt: f32,
    taken: f32,
    blocked: bool,
) -> Box<DamageEvent> {
    Box::new(DamageEvent {
        source: damage_view(ents, types, source),
        dealt,
        taken,
        blocked,
    })
}

/// Damage or the death of a player or an effect applied by another entity, kept until the
/// world's entities are at hand (`LivingEntity.hurtServer`'s triggers, `awardKillScore`,
/// `EFFECTS_CHANGED`).
#[derive(Clone, Copy, Debug)]
pub enum PendingCombat {
    /// `ENTITY_HURT_PLAYER` and the causing player's `PLAYER_HURT_ENTITY`.
    Hurt {
        source: DamageSource,
        damage: f32,
        blocked: bool,
    },
    /// `ENTITY_KILLED_PLAYER` and the killing player's `PLAYER_KILLED_ENTITY`.
    Died {
        source: DamageSource,
        killer: EntityId,
    },
    /// `EFFECTS_CHANGED` of an effect another entity applied (`onEffectAdded`,
    /// `onEffectUpdated` with a source).
    EffectsChanged { source: EntityId },
}

/// Fires the triggers of a player's [`PendingCombat`] with the entities as they are now.
pub fn resolve_combat(ents: &mut Entities, types: &DamageTypes, player: EntityId) {
    let pending = match ents.player_mut(player) {
        Some(p) if !p.advancements.combat.is_empty() => std::mem::take(&mut p.advancements.combat),
        _ => return,
    };
    let is_player = |ents: &Entities, id: EntityId| ents.player(id).is_some();
    for p in pending {
        match p {
            PendingCombat::EffectsChanged { source } => {
                fire(ents, player, TriggerKind::EFFECTS_CHANGED, |ents| {
                    Event::EffectsChanged(snapshot(ents, source))
                });
            }
            PendingCombat::Hurt {
                source,
                damage,
                blocked,
            } => {
                fire(ents, player, TriggerKind::ENTITY_HURT_PLAYER, |ents| {
                    Event::EntityHurtPlayer(damage_event(
                        ents, types, &source, damage, damage, blocked,
                    ))
                });
                if let Some(cause) = source.cause.filter(|&c| is_player(ents, c))
                    && listens(ents, cause, TriggerKind::PLAYER_HURT_ENTITY)
                    && let Some(victim) = snapshot(ents, player)
                {
                    let event = Event::PlayerHurtEntity(
                        victim,
                        damage_event(ents, types, &source, damage, damage, blocked),
                    );
                    if let Some(p) = ents.player_mut(cause) {
                        p.advancements.trigger(event);
                    }
                }
            }
            PendingCombat::Died { source, killer } => {
                // `Entity.awardKillScore`: `ENTITY_KILLED_PLAYER` of the victim.
                if listens(ents, player, TriggerKind::ENTITY_KILLED_PLAYER)
                    && let Some(k) = snapshot(ents, killer)
                {
                    let event = Event::Killed(
                        TriggerKind::ENTITY_KILLED_PLAYER,
                        k,
                        Box::new(damage_view(ents, types, &source)),
                    );
                    if let Some(p) = ents.player_mut(player) {
                        p.advancements.trigger(event);
                    }
                }
                // `ServerPlayer.awardKillScore`: `PLAYER_KILLED_ENTITY` of the killer.
                if killer != player
                    && is_player(ents, killer)
                    && listens(ents, killer, TriggerKind::PLAYER_KILLED_ENTITY)
                    && let Some(victim) = snapshot(ents, player)
                {
                    let event = Event::Killed(
                        TriggerKind::PLAYER_KILLED_ENTITY,
                        victim,
                        Box::new(damage_view(ents, types, &source)),
                    );
                    if let Some(p) = ents.player_mut(killer) {
                        p.advancements.trigger(event);
                    }
                }
            }
        }
    }
}

/// `ItemStack.applyDamage` of a player's item (`hurtAndBreak(amount, owner, slot)` after
/// Unbreaking, when the change is not 0): `ITEM_DURABILITY_CHANGED` with the stack as it was,
/// the new damage, and when it breaks the stack shrinks and `ITEM_BROKEN` counts
/// (`onEquippedItemBroken`). Returns whether it broke.
pub fn apply_damage(
    advancements: Option<&mut super::PlayerAdvancements>,
    stats: Option<&mut crate::stats::StatsCounter>,
    stack: &mut ItemStack,
    new_damage: i32,
) -> bool {
    if let Some(a) = advancements {
        a.trigger_with(TriggerKind::ITEM_DURABILITY_CHANGED, || {
            Event::ItemDurabilityChanged(stack.clone(), new_damage)
        });
    }
    // `applyDamage`: the damage is set before a broken stack shrinks, so the rest of a stack
    // of several keeps it.
    crate::item::set_damage(stack, new_damage);
    if new_damage >= crate::item::max_damage(stack) {
        let item = stack.item;
        stack.shrink(1);
        if let Some(stats) = stats {
            stats.award(
                crate::stats::Stat::of(crate::stats::StatType::Broken, item),
                1,
            );
        }
        true
    } else {
        false
    }
}

/// The height of an entity's eyes above its feet (`getEyeHeight`) in its current pose.
fn eye_height(e: &vanillite_world::entity::EntityRef<'_>) -> f64 {
    let dims = e.dims();
    if e.kind() == vanillite_data::entity::ids::PLAYER {
        return crate::survival::player::eye_height(dims.height);
    }
    let info = e.kind().info();
    f64::from(info.eye_height) * f64::from(dims.height / info.height.max(0.01))
}

/// The entity of `PlayerPredicate.lookingAt`: the nearest entity whose box the player's view
/// enters within 100 blocks (`ProjectileUtil.getEntityHitResult` from its eyes without a
/// margin, spectators left out), if the player has a line of sight to it
/// (`LivingEntity.hasLineOfSight`: no block collision between the eyes within 128 blocks).
pub fn looked_at(
    level: &crate::level::Level,
    player: EntityId,
    spectator: &dyn Fn(EntityId) -> bool,
) -> Option<EntityId> {
    use vanillite_base::Vec3;
    let store = &level.entities.store;
    let e = store.get(player)?;
    let p = e.pos();
    let eye = Vec3::new(p.x, p.y + eye_height(&e), p.z);
    let rot = e.rot();
    let view = crate::mob::view_vector(rot.x_rot, rot.y_rot);
    let to = Vec3::new(
        eye.x + view.x * 100.0,
        eye.y + view.y * 100.0,
        eye.z + view.z * 100.0,
    );
    let area =
        vanillite_base::Aabb::new(eye.x, eye.y, eye.z, to.x, to.y, to.z).inflate(1.0, 1.0, 1.0);
    let mut nearest: Option<(EntityId, f64)> = None;
    store.for_each_in(&area, false, |other| {
        if other == player || spectator(other) {
            return;
        }
        let Some(o) = store.get(other) else {
            return;
        };
        if let Some(hit) = crate::mob::projectile::aabb_clip(&o.aabb(), eye, to) {
            let d = eye.distance_to_sqr(hit);
            if nearest.is_none_or(|(_, n)| d < n) {
                nearest = Some((other, d));
            }
        }
    });
    let (target, _) = nearest?;
    let t = store.get(target)?;
    let tp = t.pos();
    let target_eye = Vec3::new(tp.x, tp.y + eye_height(&t), tp.z);
    let sees = target_eye.distance_to_sqr(eye).sqrt() <= 128.0
        && crate::mob::physics::clip(level, eye, target_eye).is_none();
    sees.then_some(target)
}
