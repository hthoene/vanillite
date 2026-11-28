//! Thrown tridents (`ThrownTrident`, `AbstractArrow` flight) as the drowned throw them.
//!
//! The missile fields: `ints[0]` the ticks in the ground (despawn), `ints[1]` the block state
//! it sticks in plus one, `ints[2]` bit 0 dealt damage, bit 1 returning (no physics), the
//! bits above the ticks in the ground of `inGroundTime`, `ints[3]` the loyalty level.

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{EntityId, Rotation};

use crate::item::ItemStack;
use crate::mob::missile::{MissileData, MissileKind, MissileTick};
use crate::mob::physics::{self, Blocks};
use crate::mob::projectile::{lerp_rotation, segment_box, shoot};
use crate::mob::{Tick, atan2_deg, combat};
use crate::player::encode::DataWriter;
use crate::survival::play_sound;

const DEALT: i32 = 1;
const RETURNING: i32 = 2;

/// Throws a trident from `start` along `dir` (`spawnProjectileUsingShoot` with power 1.6).
pub fn throw(t: &mut Tick<'_, '_>, start: Vec3, dir: Vec3, item: ItemStack, uncertainty: f32) {
    let mut own = crate::mob::missile::new_projectile_random();
    let (velocity, rot) = shoot(&mut own, dir, 1.6, uncertainty);
    let mut d = MissileData::new(MissileKind::Trident);
    d.ints[3] = loyalty(&item);
    d.foil = has_foil(&item);
    d.item = item;
    d.random_state = own.state();
    crate::mob::missile::spawn_shot(t, start, velocity, rot, d);
}

/// `ItemStack.hasFoil`: the glint override, else whether it is enchanted.
pub fn has_foil(item: &ItemStack) -> bool {
    use vanillite_protocol::components::ids::ENCHANTMENT_GLINT_OVERRIDE;
    match crate::item_nbt::effective_component(item, ENCHANTMENT_GLINT_OVERRIDE) {
        Some(v) => v.first().is_some_and(|&b| b != 0),
        None => !crate::item::enchantments(item).is_empty(),
    }
}

/// The return acceleration of a trident (`getTridentReturnToOwnerAcceleration`: Loyalty).
pub fn loyalty(item: &ItemStack) -> i32 {
    let combat = crate::mob::attack::combat_data();
    let enchants = &combat.enchants;
    crate::experience_orb::with_entity_random(|r| enchants.trident_return_acceleration(item, r))
        .clamp(0, 127)
}

fn in_ground_time(d: &MissileData) -> i32 {
    d.ints[2] >> 2
}

fn set_in_ground_time(d: &mut MissileData, v: i32) {
    d.ints[2] = (d.ints[2] & 3) | (v.max(0) << 2);
}

fn sound(t: &mut MissileTick<'_, '_>, pos: Vec3, s: &str, volume: f32, pitch: f32) {
    let seed = crate::survival::sound_seed();
    play_sound(
        t.env.out,
        None,
        pos,
        s,
        SoundSource::Neutral,
        volume,
        pitch,
        seed,
    );
}

/// `ThrownTrident.tick` with `AbstractArrow.tick`. The trident's own random (`random_state`)
/// draws the factors of a fall out of the ground, the turn after it hit an entity and the
/// pitch of the hit ground sound; everything else draws from the level random.
pub fn tick(t: &mut MissileTick<'_, '_>) {
    let id = t.id;
    let Some(mut d) = t.ents.missile(id).cloned() else {
        return;
    };
    if d.kind != MissileKind::Trident {
        return;
    }
    let was_in_ground = d.in_ground;
    let was_returning = d.ints[2] & RETURNING != 0;
    let mut own = vanillite_base::random::LegacyRandom::from_state(d.random_state);
    let Some(e) = t.ents.store.get(id) else {
        return;
    };
    let mut pos = e.pos();
    let start = pos;
    let mut velocity = e.velocity();
    let mut rot = e.rot();
    let aabb = e.aabb();
    if in_ground_time(&d) > 4 {
        d.ints[2] |= DEALT;
    }
    // Loyalty: return to the owner once it hit something.
    let loyalty = d.ints[3];
    let owner = d.owner.filter(|&o| t.ents.store.contains(o));
    if loyalty > 0
        && d.ints[2] & (DEALT | RETURNING) != 0
        && let Some(o) = owner
    {
        // `isAcceptibleReturnOwner`: alive, and no spectator.
        let alive = t.ents.mob(o).is_some_and(crate::mob::MobData::is_alive)
            || t.ents.player(o).is_some_and(|p| {
                !p.survival.is_dead_or_dying()
                    && p.game_mode != vanillite_config::GameMode::Spectator
            });
        if !alive {
            if d.pickup == 1 {
                // `spawnAtLocation(getPickupItem(), 0.1)`.
                let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
                t.env.out.push(crate::survival::Effect::DropItem {
                    random: item_random,
                    uuid,
                    pos: Vec3::new(pos.x, pos.y + 0.1, pos.z),
                    velocity,
                    y_rot,
                    pickup_delay: 10,
                    stack: d.item.clone(),
                });
            }
            d.removed = true;
        } else if let Some(oe) = t.ents.store.get(o) {
            let eye_height = crate::mob::missile::eye_height(t.ents, o);
            let eye = Vec3::new(oe.pos().x, oe.pos().y + eye_height, oe.pos().z);
            let width = f64::from(oe.dims().width);
            if t.ents.player(o).is_none() && pos.distance_to_sqr(eye).sqrt() < width + 1.0 {
                d.removed = true;
            } else {
                if d.ints[2] & RETURNING == 0 {
                    sound(t, pos, "minecraft:item.trident.return", 10.0, 1.0);
                }
                d.ints[2] |= RETURNING;
                let v = eye - pos;
                pos.y += v.y * 0.015 * f64::from(loyalty);
                velocity = velocity * 0.95 + v.normalize() * (0.05 * f64::from(loyalty));
            }
        }
        if d.removed {
            if let Some(m) = t.ents.missile_mut(id) {
                *m = d;
            }
            return;
        }
    }
    let no_physics = d.ints[2] & RETURNING != 0;
    let block_pos = BlockPos::containing(pos.x, pos.y, pos.z);
    let state = t.level.block(block_pos);
    if !no_physics && !state.is_air() && inside_shape(state, block_pos, pos) {
        velocity = Vec3::ZERO;
        d.in_ground = true;
    }
    d.shake = d.shake.saturating_sub(1);
    if d.in_ground && !no_physics {
        let last = u16::try_from(d.ints[1] - 1).ok().map(BlockState);
        let probe = Aabb::new(pos.x, pos.y, pos.z, pos.x, pos.y, pos.z).inflate(0.06, 0.06, 0.06);
        if last != Some(state) && physics::no_collision(&t.level.view(), &probe, &mut t.cx.boxes) {
            d.in_ground = false;
            let r = &mut own;
            velocity = velocity.multiply(
                f64::from(r.next_float() * 0.2),
                f64::from(r.next_float() * 0.2),
                f64::from(r.next_float() * 0.2),
            );
            d.ints[0] = 0;
        } else {
            let ticks = in_ground_time(&d) + 1;
            set_in_ground_time(&mut d, ticks);
            // `tickDespawn`: tridents that return to their owner stay.
            if d.pickup != 1 || loyalty <= 0 {
                d.ints[0] += 1;
                if d.ints[0] >= 1200 {
                    d.removed = true;
                }
            }
        }
    } else {
        set_in_ground_time(&mut d, 0);
        // A returning trident comes back handle first.
        #[allow(clippy::cast_possible_truncation)]
        let y_rot = if no_physics {
            atan2_deg(-velocity.x, -velocity.z)
        } else {
            atan2_deg(velocity.x, velocity.z)
        } as f32;
        let hd = (velocity.x * velocity.x + velocity.z * velocity.z).sqrt();
        #[allow(clippy::cast_possible_truncation)]
        let x_rot = atan2_deg(velocity.y, hd) as f32;
        rot.x_rot = lerp_rotation(rot.x_rot, x_rot);
        rot.y_rot = lerp_rotation(rot.y_rot, y_rot);
        if !d.left_owner {
            let grown = aabb
                .expand_towards(velocity.x, velocity.y, velocity.z)
                .inflate(1.0, 1.0, 1.0);
            let inside = owner.is_some_and(|o| {
                t.ents
                    .store
                    .get(o)
                    .is_some_and(|oe| oe.aabb().intersects(&grown))
            });
            d.left_owner = !inside;
        }
        let to = pos + velocity;
        if no_physics {
            pos = to;
        } else {
            let block_hit = physics::clip(t.level as &dyn Blocks, pos, to);
            let end = block_hit.map_or(to, |(_, p)| p);
            let entity_hit = if d.ints[2] & DEALT == 0 {
                find_hit(t, &d, aabb, pos, end, velocity)
            } else {
                None
            };
            // Bukkit's `ProjectileHitEvent`: a cancelled hit is ignored, the trident flies on.
            let hit_event =
                |what| crate::mob::missile::hit_event(t.level, t.ents, id, d.owner, what);
            let entity_hit = entity_hit
                .filter(|&(target, _)| hit_event(crate::mob::missile::HitWhat::Entity(target)));
            let block_hit = block_hit.filter(|&(hit_pos, at)| {
                entity_hit.is_some() || hit_event(crate::mob::missile::HitWhat::Block(hit_pos, at))
            });
            let deflector = entity_hit.map(|(target, _)| target).filter(|&target| {
                crate::mob::missile::deflects(t.ents, target, vanillite_data::entity::ids::TRIDENT)
            });
            if let Some(target) = deflector {
                // `hitTargetOrDeflectSelf`: a breeze turns the trident back, once in a row.
                if d.last_deflected != Some(target) {
                    (velocity, rot) = crate::mob::missile::reverse(
                        t.ents, t.env, target, velocity, rot, &mut own,
                    );
                    d.last_deflected = Some(target);
                }
            } else if let Some((target, hit)) = entity_hit {
                pos = hit;
                // `projectileReceivesSideEffectsOnHit(wasHurt)`: the hit sound and the bounce
                // (`deflect(REVERSE, .., (0.02, 0.2, 0.02))`), not for an enderman that dodged.
                if hit_entity(t, &mut d, target, pos) {
                    sound(t, pos, "minecraft:item.trident.hit", 1.0, 1.0);
                    velocity = velocity.multiply(-0.01, -0.1, -0.01);
                    let turn = 170.0 + own.next_float() * 20.0;
                    rot.y_rot += turn;
                }
                crate::mob::missile::land_event(t.env.out, t.level, id, d.owner, None, hit);
            } else if let Some((hit_pos, at)) = block_hit {
                let sign = |v: f64| {
                    if v > 0.0 {
                        1.0
                    } else if v < 0.0 {
                        -1.0
                    } else {
                        0.0
                    }
                };
                let offset = Vec3::new(sign(velocity.x), sign(velocity.y), sign(velocity.z))
                    * f64::from(0.05_f32);
                pos = at - offset;
                let speed = velocity.length();
                velocity = Vec3::ZERO;
                d.ints[1] = i32::from(t.level.block(hit_pos).0) + 1;
                crate::mob::missile::hit_block_at_speed(t.level, t.cx, id, hit_pos, at, speed);
                // `AbstractArrow.onHitBlock`: the enchantment effects, the hit ground sound
                // (`entity.arrow.hit` from the second landing on), the shake.
                hit_block_effects(t, &d, hit_pos, at);
                d.in_ground = true;
                let pitch = 1.2 / (own.next_float() * 0.2 + 0.9);
                let ground_sound = if d.arrow_hit_sound {
                    "minecraft:entity.arrow.hit"
                } else {
                    "minecraft:item.trident.hit_ground"
                };
                sound(t, pos, ground_sound, 1.0, pitch);
                d.shake = 7;
                d.arrow_hit_sound = true;
                crate::mob::missile::land_event(t.env.out, t.level, id, d.owner, Some(hit_pos), at);
            } else {
                pos = to;
            }
        }
        // `applyInertia`: tridents keep their speed in water (0.99) like in the air.
        velocity = velocity * f64::from(0.99_f32);
        if !no_physics && !d.in_ground {
            // `applyGravity`: `movement.add(0, -gravity, 0)` (turns -0.0 into 0.0).
            velocity = velocity + Vec3::new(0.0, -0.05, 0.0);
        }
    }
    // `applyEffectsFromBlocks` in flight, while returning and in the ground (buttons,
    // tripwires, pressure plates).
    if !d.removed {
        crate::mob::projectile::inside_blocks(t.level, t.cx, id, aabb, start, pos);
    }
    if let Some(mut e) = t.ents.store.get_mut(id) {
        e.set_pos(pos);
        e.set_velocity(velocity);
        e.set_rot(Rotation { ..rot });
        // Only `IN_GROUND` of the synched values changes here; resending it would restart the
        // client's shake.
        if d.in_ground != was_in_ground || (d.ints[2] & RETURNING != 0) != was_returning {
            e.mark_data_dirty();
        }
    }
    d.random_state = own.state();
    if let Some(m) = t.ents.missile_mut(id) {
        *m = d;
    }
}

/// Whether `pos` is inside the collision shape of the block at `bp`.
fn inside_shape(state: BlockState, bp: BlockPos, pos: Vec3) -> bool {
    state.collision_shape().iter().any(|b| {
        Aabb::new(
            f64::from(bp.x) + b[0],
            f64::from(bp.y) + b[1],
            f64::from(bp.z) + b[2],
            f64::from(bp.x) + b[3],
            f64::from(bp.y) + b[4],
            f64::from(bp.z) + b[5],
        )
        .contains(pos.x, pos.y, pos.z)
    })
}

/// `findHitEntity`: the closest living entity the flight crosses.
fn find_hit(
    t: &MissileTick<'_, '_>,
    d: &MissileData,
    aabb: Aabb,
    from: Vec3,
    to: Vec3,
    velocity: Vec3,
) -> Option<(EntityId, Vec3)> {
    let area = aabb
        .expand_towards(velocity.x, velocity.y, velocity.z)
        .inflate(1.0, 1.0, 1.0);
    let mut best: Option<(EntityId, f64, Vec3)> = None;
    t.ents.store.for_each_in(&area, false, |other| {
        if other == t.id || (!d.left_owner && d.owner == Some(other)) {
            return;
        }
        if !crate::mob::projectile::can_be_hit_by_projectile(t.ents, other) {
            return;
        }
        // `AbstractArrow.canHitEntity`: a player's trident flies through the players it may
        // not hurt (`canHarmPlayer`).
        if t.ents.player(other).is_some()
            && let Some(owner) = d.owner.filter(|&o| t.ents.player(o).is_some())
            && !crate::mob::attack::can_harm_player(t.ents, t.env, owner, other)
        {
            return;
        }
        let Some(oe) = t.ents.store.get(other) else {
            return;
        };
        let m = crate::mob::missile::margin(d.life);
        let grown = oe.aabb().inflate(m, m, m);
        if let Some(f) = segment_box(from, to, &grown) {
            let hit = from + (to - from) * f;
            // `ProjectileUtil.getEntityHitResult`: the nearest clip point.
            let dist = from.distance_to_sqr(hit);
            if best.is_none_or(|(_, b, _)| dist < b) {
                best = Some((other, dist, hit));
            }
        }
    });
    best.map(|(id, _, hit)| (id, hit))
}

/// `ThrownTrident.onHitEntity`: 8 damage (with the weapon's enchantments), then it bounces.
/// Returns `projectileReceivesSideEffectsOnHit(wasHurt)`: false for an enderman it missed.
fn hit_entity(
    t: &mut MissileTick<'_, '_>,
    d: &mut MissileData,
    target: EntityId,
    pos: Vec3,
) -> bool {
    let enderman = t
        .ents
        .mob(target)
        .is_some_and(|m| m.kind == crate::mob::Kind::Enderman);
    let owner = d
        .owner
        .filter(|o| t.ents.store.contains(*o))
        .unwrap_or(t.id);
    let kind = t.env.types.named("minecraft:trident");
    let mut source = combat::entity_source(t.ents, kind, owner, t.id);
    source.position = Some(pos);
    let mut damage = 8.0_f32;
    if !d.item.is_empty() {
        let combat = crate::mob::attack::combat_data();
        let (enchants, tags): (
            &crate::enchant::Enchantments,
            &dyn crate::predicate::EntityTags,
        ) = (&combat.enchants, &*combat);
        let cx = crate::enchant::EnchantContext {
            types: t.env.types,
            tags,
            this: crate::mob::attack::facts(t.ents, target),
            attacker: crate::mob::attack::facts(t.ents, owner),
            direct_attacker: None,
            attacking_player: None,
            source: None,
            raining: false,
            thundering: false,
            location: None,
        };
        damage = enchants.modify_damage(&d.item, &cx, t.env.random, damage);
    }
    d.ints[2] |= DEALT;
    let hurt = combat::hurt(t.level, t.ents, t.env, t.cx, target, source, damage);
    if hurt && !d.item.is_empty() {
        // `doPostAttackEffectsWithItemSource` with the trident (Channeling).
        crate::mob::attack::post_attack(
            t.level,
            t.ents,
            t.env,
            t.cx,
            target,
            &source,
            Some(&d.item),
        );
    }
    hurt || !enderman
}

/// `hitBlockEnchantmentEffects`: the `hit_block` effects of the trident (Channeling on a
/// lightning rod in a thunderstorm).
fn hit_block_effects(t: &mut MissileTick<'_, '_>, d: &MissileData, block: BlockPos, at: Vec3) {
    if d.item.is_empty() {
        return;
    }
    let combat = crate::mob::attack::combat_data();
    let (enchants, tags): (
        &crate::enchant::Enchantments,
        &dyn crate::predicate::EntityTags,
    ) = (&combat.enchants, &*combat);
    let state = t.level.block(block);
    let sees_sky = crate::mob::can_see_sky(t.level, block);
    let effects = {
        let cx = crate::enchant::EnchantContext {
            types: t.env.types,
            tags,
            this: crate::mob::attack::facts(t.ents, t.id),
            attacker: d.owner.and_then(|o| crate::mob::attack::facts(t.ents, o)),
            direct_attacker: None,
            attacking_player: None,
            source: None,
            raining: t.level.is_raining(),
            thundering: t.level.is_thundering(),
            location: Some((state, sees_sky)),
        };
        enchants.hit_block_effects(&d.item, &cx, t.env.random)
    };
    // The position clamped into the block.
    let clamp = |v: f64, b: i32| v.clamp(f64::from(b), f64::from(b) + 1.0);
    let pos = Vec3::new(
        clamp(at.x, block.x),
        clamp(at.y, block.y),
        clamp(at.z, block.z),
    );
    for (effect, _) in effects {
        run_block_effect(t, d, &effect, pos);
    }
}

fn run_block_effect(
    t: &mut MissileTick<'_, '_>,
    d: &MissileData,
    effect: &crate::enchant::EntityEffect,
    pos: Vec3,
) {
    use crate::enchant::EntityEffect;
    match effect {
        EntityEffect::SummonLightning => {
            let cause = d.owner.filter(|&o| t.ents.player(o).is_some());
            t.cx.actions.push(crate::mob::WorldAction::Level(
                crate::mob::boss::warden::LevelAction::Lightning { pos, cause },
            ));
        }
        EntityEffect::PlaySound {
            sounds,
            volume,
            pitch,
        } => {
            let Some(s) = sounds.first() else {
                return;
            };
            play_sound(
                t.env.out,
                None,
                pos,
                s,
                SoundSource::Neutral,
                *volume,
                *pitch,
                crate::survival::sound_seed(),
            );
        }
        EntityEffect::AllOf(list) => {
            for e in list {
                run_block_effect(t, d, e, pos);
            }
        }
        _ => {}
    }
}

/// `AbstractArrow.playerTouch` of a trident (`ThrownTrident.playerTouch` and `tryPickup`):
/// its owner (or anybody, for one without owner) takes it once it is in the ground or
/// returning. Returns whether it was taken.
pub fn try_pickup(
    ents: &mut crate::entities::Entities,
    trident: EntityId,
    player: &mut crate::player::PlayerData,
    player_id: EntityId,
) -> bool {
    let Some(d) = ents.missile(trident) else {
        return false;
    };
    if d.kind != MissileKind::Trident || d.removed {
        return false;
    }
    // `ownedBy` compares the UUID; `getOwner() == null` for an owner that is not here.
    let player_uuid = ents.store.get(player_id).map(|e| e.uuid());
    let owned = d.owner_uuid.is_some() && d.owner_uuid == player_uuid;
    let ownerless = crate::mob::missile::resolve_owner(ents, d.owner, d.owner_uuid).is_none();
    if !(owned || ownerless) {
        return false;
    }
    let returning = d.ints[2] & RETURNING != 0;
    if !(d.in_ground || returning) || d.shake > 0 {
        return false;
    }
    let infinite = player.game_mode == vanillite_config::GameMode::Creative;
    let mut item = d.item.clone();
    let taken = match d.pickup {
        1 => player.inventory.add(None, &mut item, infinite),
        2 => infinite,
        _ => false,
    } || (returning && owned && player.inventory.add(None, &mut item, infinite));
    if taken && let Some(m) = ents.missile_mut(trident) {
        m.removed = true;
    }
    taken
}

/// `AbstractArrow` and `ThrownTrident` NBT.
pub fn save(d: &MissileData, out: &mut Compound) {
    if d.kind != MissileKind::Trident {
        return;
    }
    out.insert(
        "life",
        i16::try_from(d.ints[0].clamp(0, 32767)).unwrap_or(0),
    );
    out.insert("inGround", d.in_ground);
    if let Some(state) = d.ints[1].checked_sub(1).and_then(|s| u16::try_from(s).ok()) {
        out.insert(
            "inBlockState",
            crate::level::block_entity::write_state_codec(BlockState(state)),
        );
    }
    out.insert("shake", i8::try_from(d.shake).unwrap_or(0));
    out.insert("pickup", i8::try_from(d.pickup).unwrap_or(0));
    out.insert("damage", 2.0_f64);
    out.insert("crit", false);
    out.insert("PierceLevel", 0_i8);
    out.insert(
        "SoundEvent",
        if d.arrow_hit_sound {
            "minecraft:entity.arrow.hit"
        } else {
            "minecraft:item.trident.hit_ground"
        },
    );
    out.insert("DealtDamage", d.ints[2] & DEALT != 0);
    if !d.item.is_empty() {
        out.insert("item", crate::inventory::stack_to_nbt(&d.item));
    }
}

/// Reads `ThrownTrident` NBT.
pub fn load(d: &mut MissileData, tag: &Compound) {
    if d.kind != MissileKind::Trident {
        return;
    }
    let boolean = |k: &str| crate::mob::entity_nbt::get_bool(tag, k).unwrap_or(false);
    d.in_ground = boolean("inGround");
    d.ints[0] = crate::mob::entity_nbt::get_int(tag, "life").unwrap_or(0);
    d.shake = crate::mob::entity_nbt::get_int(tag, "shake")
        .map_or(0, |v| u8::try_from(v & 255).unwrap_or(0));
    d.arrow_hit_sound =
        tag.get("SoundEvent").and_then(Tag::as_str) == Some("minecraft:entity.arrow.hit");
    if let Some(state) = tag
        .get("inBlockState")
        .and_then(crate::level::block_entity::read_state_codec)
    {
        d.ints[1] = i32::from(state.0) + 1;
    }
    if boolean("DealtDamage") {
        d.ints[2] |= DEALT;
    }
    d.pickup = crate::mob::entity_nbt::get_int(tag, "pickup")
        .map_or(0, |v| u8::try_from(v).unwrap_or(0).min(2));
    if d.item.is_empty()
        && let Some(Tag::Compound(c)) = tag.get("item")
    {
        d.item = crate::inventory::stack_from_nbt(c);
    }
    d.ints[3] = loyalty(&d.item);
}

/// `AbstractArrow` (flags, pierce level, in ground) and `ThrownTrident` (loyalty, foil) data.
pub fn encode(w: &mut DataWriter<'_>, d: &MissileData) {
    if d.kind != MissileKind::Trident {
        return;
    }
    // `ID_FLAGS`: bit 2 no physics (returning).
    w.byte(8, if d.ints[2] & RETURNING != 0 { 2 } else { 0 }, 0);
    w.byte(9, 0, 0);
    w.boolean(10, d.in_ground, false);
    w.byte(11, u8::try_from(d.ints[3].clamp(0, 127)).unwrap_or(0), 0);
    w.boolean(12, d.foil, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AbstractArrow.addAdditionalSaveData` of a trident: base damage 2, the shake, the hit
    /// ground sound after its first landing and the block it is in; read back the same
    /// (#272, I3-28).
    #[test]
    fn trident_nbt_keeps_arrow_state() {
        let mut d = MissileData::new(MissileKind::Trident);
        d.shake = 5;
        d.arrow_hit_sound = true;
        d.in_ground = true;
        d.ints[1] = i32::from(vanillite_data::block::ids::STONE.default_state().0) + 1;
        let mut out = Compound::new();
        save(&d, &mut out);
        assert_eq!(out.get("damage"), Some(&Tag::Double(2.0)));
        assert_eq!(out.get("shake"), Some(&Tag::Byte(5)));
        assert_eq!(
            out.get("SoundEvent").and_then(Tag::as_str),
            Some("minecraft:entity.arrow.hit")
        );
        let mut back = MissileData::new(MissileKind::Trident);
        load(&mut back, &out);
        assert_eq!(back.shake, 5);
        assert!(back.arrow_hit_sound);
        assert_eq!(back.ints[1], d.ints[1]);
    }
}
