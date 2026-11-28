//! Explosions (`ServerLevel.explode`, `ServerExplosion`).
//!
//! An explosion casts Vanilla's 1352 rays from its centre (the surface of a 16³ grid), each with
//! a random power that blast and fluid resistance wear down in 0.3 block steps; every block a ray
//! still has power at is blown up. Entities in range are pushed away by their exposure (the share
//! of rays from their box to the centre that no block stops), then the blown up blocks are
//! destroyed in random order and their drops spawn after all of them.
//!
//! The damage of the living entities hit is queued in [`super::LevelOutput::explosion_hurts`]
//! for the mob system, which applies it right after the explosion's tick step. As in Vanilla
//! the entities take their damage (and die and drop) before the blocks are destroyed: while
//! damage is queued, the block phase waits for [`Level::finish_explosion`], which the mob
//! system calls after the damage.
//!
//! Items lose health and are destroyed, boats take damage like living entities.
//!
//! Fire explosions (ghast fireballs) set fire to a third of the blown up air above solid
//! blocks. `TRIGGER_BLOCK` explosions (wind charges) leave the blocks and open doors,
//! trapdoors and fence gates, press buttons, pull levers, ring bells and blow out candles.
//!
//! Not ported: damage of end crystals and other non-living entities and the special damage
//! calculators (wither skulls, wind charges, TNT that used a portal).

use vanillite_base::hash::FxHashSet;
use vanillite_base::math::{floor, frac, lerp};
use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::{BlockKind, BlockState, ids};
use vanillite_data::entity::ids as entity_ids;
use vanillite_world::entity::{EntityFlags, EntityId};
use vanillite_worldgen::behaviour::StateExt;

use super::{Level, update};
use crate::block::{drops, tnt};

/// What an explosion does to blocks (`Explosion.BlockInteraction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockInteraction {
    /// `KEEP`: blocks stay.
    Keep,
    /// `DESTROY`: blocks break and drop everything.
    Destroy,
    /// `DESTROY_WITH_DECAY`: blocks break and drops decay with the radius.
    DestroyWithDecay,
    /// `TRIGGER_BLOCK`: blocks react (buttons, doors) instead of breaking.
    TriggerBlock,
}

/// The exploding entity's `getBlockExplosionResistance` override.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlockResistance {
    /// The block's and fluid's own resistance.
    #[default]
    Default,
    /// `WitherSkull` (dangerous): at most 0.8 for blocks the wither can destroy.
    DangerousWitherSkull,
    /// `MinecartTNT`: rails and the blocks under them have no resistance and do not break.
    TntMinecart,
    /// `AbstractWindCharge.EXPLOSION_DAMAGE_CALCULATOR`: `#blocks_wind_charge_explosions` are
    /// immune.
    WindCharge,
    /// `RespawnAnchorBlock.explode` in water: the anchor's position resists like water.
    WaterAt(BlockPos),
    /// `SimpleExplosionDamageCalculator` with `immune_blocks` (an enchantment's `explode`):
    /// only the blocks of [`CustomExplosion::immune`] resist.
    Immune,
}

/// The parts of an enchantment's `explode` effect beyond [`ExplosionStyle`]: the immune
/// blocks and what the clients show.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomExplosion {
    /// `immune_blocks` as sorted block ids (with [`BlockResistance::Immune`]).
    pub immune: Vec<u16>,
    pub look: std::sync::Arc<ExplosionLook>,
}

/// The particles and sound of `ClientboundExplodePacket`.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplosionLook {
    /// `smallParticle` (small explosions) and `largeParticle`.
    pub small: super::particles::Particle,
    pub large: super::particles::Particle,
    /// `blockParticles`: particle, scaling, speed and weight.
    pub block_particles: Vec<(super::particles::Particle, f32, f32, i32)>,
    pub sound: String,
}

/// The block phase of an explosion that waits for the damage of the entities it hit
/// (`interactWithBlocks` and `createFire` after `hurtEntities`).
#[derive(Debug)]
pub(super) struct PendingBlast {
    targets: Vec<BlockPos>,
    source: Option<EntityId>,
    indirect: Option<EntityId>,
    interaction: BlockInteraction,
    radius: f32,
    center: Vec3,
    fire: bool,
}

/// An explosion for the clients (`ClientboundExplodePacket`), sent to players within 64 blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplosionEvent {
    /// Centre.
    pub center: Vec3,
    /// Radius (power).
    pub radius: f32,
    /// Number of blocks blown up.
    pub block_count: i32,
    /// `isSmall()`: the small particle instead of the emitter.
    pub small: bool,
    /// Knockback of the players it hit (`getHitPlayers`).
    pub hit_players: Vec<(EntityId, Vec3)>,
    /// The burst sound of a wind charge (gust particles, no block particles); `None` for
    /// normal explosions.
    pub wind_sound: Option<&'static str>,
    /// The particles and sound of an enchantment's explosion; `None` for the default ones.
    pub look: Option<std::sync::Arc<ExplosionLook>>,
}

/// How an explosion treats entities (`ExplosionDamageCalculator`) and how clients show it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExplosionStyle {
    /// `shouldDamageEntity`.
    pub damages_entities: bool,
    /// `getKnockbackMultiplier`.
    pub knockback: f64,
    /// The wind charge burst sound, see [`ExplosionEvent::wind_sound`].
    pub wind_sound: Option<&'static str>,
    /// The source's block resistance override.
    pub resistance: BlockResistance,
    /// `fire`: sets fire to some of the blown up positions (`createFire`).
    pub fire: bool,
    /// The damage is `badRespawnPointExplosion` (beds and respawn anchors).
    pub bad_respawn_point: bool,
    /// The damage type of an enchantment's explosion and whether the damage is caused by the
    /// exploding entity (`attribute_to_user`) instead of coming from the centre.
    pub damage: Option<(crate::survival::damage::DamageKind, bool)>,
}

impl Default for ExplosionStyle {
    fn default() -> Self {
        ExplosionStyle {
            damages_entities: true,
            knockback: 1.0,
            wind_sound: None,
            resistance: BlockResistance::Default,
            fire: false,
            bad_respawn_point: false,
            damage: None,
        }
    }
}

/// The damage an explosion deals to a living entity (`entity.hurtServer(level,
/// damageSources().explosion(source, indirect), amount)`), applied by the mob system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExplosionHurt {
    /// The entity hit.
    pub target: EntityId,
    /// `getEntityDamageAmount`.
    pub amount: f32,
    /// The exploding entity (`getDirectSourceEntity`).
    pub source: Option<EntityId>,
    /// The entity that caused it (`getIndirectSourceEntity`).
    pub indirect: Option<EntityId>,
    /// Centre (the damage position without a source entity).
    pub center: Vec3,
    /// `ServerExplosion.shouldAffectBlocklikeEntities` with mob griefing off: the explosion
    /// breaks blocks and is no wind charge (with mob griefing on only the wind charge part
    /// counts, see [`ExplosionHurt::wind`]).
    pub blocklike: bool,
    /// The source is a wind charge.
    pub wind: bool,
    /// `badRespawnPointExplosion` instead of `explosion`.
    pub bad_respawn_point: bool,
    /// The damage type of an enchantment's explosion, see [`ExplosionStyle::damage`].
    pub damage: Option<(crate::survival::damage::DamageKind, bool)>,
    /// The dragon part hit (`EnderDragonPart.hurtServer`), when `target` is a dragon hit
    /// through one of its parts.
    pub part: Option<usize>,
}

impl ExplosionHurt {
    /// `EnderDragonPart.hurtServer`: a hit through a part lands on that part of the dragon.
    pub fn aim(&self, ents: &mut crate::entities::Entities) {
        if let Some(i) = self.part
            && let Some(s) = ents.mob_mut(self.target).and_then(|m| m.boss.dragon_mut())
        {
            s.hit_part = Some(i);
        }
    }

    /// `shouldAffectBlocklikeEntities`.
    pub fn affects_blocklike(&self, mob_griefing: bool) -> bool {
        if mob_griefing {
            !self.wind
        } else {
            self.blocklike
        }
    }
}

impl Level {
    /// `ServerLevel.explode(source, ..., x, y, z, radius, fire = false, interaction)` with the
    /// default particles and sound. `source` is the exploding entity (already removed or not),
    /// `indirect` the entity that caused it (`getIndirectSourceEntity`). Returns the number of
    /// blocks blown up.
    pub fn explode(
        &mut self,
        source: Option<EntityId>,
        indirect: Option<EntityId>,
        center: Vec3,
        radius: f32,
        interaction: BlockInteraction,
    ) -> usize {
        self.explode_with(
            source,
            indirect,
            center,
            radius,
            interaction,
            ExplosionStyle::default(),
        )
    }

    /// [`Self::explode`] with a damage calculator and client style (wind charges: no damage,
    /// more knockback, gusts).
    pub fn explode_with(
        &mut self,
        source: Option<EntityId>,
        indirect: Option<EntityId>,
        center: Vec3,
        radius: f32,
        interaction: BlockInteraction,
        style: ExplosionStyle,
    ) -> usize {
        self.explode_full(source, indirect, center, radius, interaction, style, None)
    }

    /// [`Self::explode_with`] of an enchantment's `explode` effect with its immune blocks,
    /// particles and sound.
    #[allow(clippy::too_many_arguments)]
    pub fn explode_custom(
        &mut self,
        source: Option<EntityId>,
        center: Vec3,
        radius: f32,
        interaction: BlockInteraction,
        style: ExplosionStyle,
        custom: CustomExplosion,
    ) -> usize {
        self.custom_explosion = Some(custom);
        let count = self.explode_full(source, None, center, radius, interaction, style, None);
        self.custom_explosion = None;
        count
    }

    /// [`Self::explode_with`] of a block without a source entity (a bed or respawn anchor):
    /// `block` is the exploding block and its state before it went, for the plugins.
    pub fn explode_block(
        &mut self,
        block: (BlockPos, BlockState),
        center: Vec3,
        radius: f32,
        interaction: BlockInteraction,
        style: ExplosionStyle,
    ) -> usize {
        self.explode_full(None, None, center, radius, interaction, style, Some(block))
    }

    #[allow(clippy::too_many_arguments)]
    fn explode_full(
        &mut self,
        source: Option<EntityId>,
        indirect: Option<EntityId>,
        center: Vec3,
        radius: f32,
        interaction: BlockInteraction,
        style: ExplosionStyle,
        block: Option<(BlockPos, BlockState)>,
    ) -> usize {
        // The blocks of an earlier explosion nobody applied the damage of.
        self.finish_explosion();
        let (mut radius, mut interaction, mut style) = (radius, interaction, style);
        let mut planned = None;
        if let Some(hooks) = self.hooks_for(vanillite_abi::event::ENTITY_EXPLODE) {
            // Bukkit's `EntityExplodeEvent` names the blocks: they are known before the event.
            let mut targets = self.exploded_positions(center, radius, style.resistance);
            let Some((r, i, fire, kept)) = self.explode_event(
                &*hooks,
                source,
                center,
                radius,
                interaction,
                &style,
                block,
                &targets,
            ) else {
                return 0;
            };
            if r.to_bits() != radius.to_bits() {
                targets = self.exploded_positions(center, r, style.resistance);
            }
            if let Some(kept) = kept {
                targets.retain(|p| kept.contains(p) || self.block(*p).is_air());
            }
            (radius, interaction, style.fire) = (r, i, fire);
            planned = Some(targets);
        }
        let context = super::game_event::GameEventContext::of(source);
        self.game_event(super::game_event::events().explode, center, context);
        let targets = match planned {
            Some(targets) => targets,
            None => self.exploded_positions(center, radius, style.resistance),
        };
        let destroys = matches!(
            interaction,
            BlockInteraction::Destroy | BlockInteraction::DestroyWithDecay
        );
        let queued = self.output.explosion_hurts.len();
        let hit_players = self.hurt_entities(source, indirect, center, radius, style, destroys);
        let count = targets.len();
        self.output.explosions.push(ExplosionEvent {
            center,
            radius,
            block_count: i32::try_from(count).unwrap_or(i32::MAX),
            small: radius < 2.0 || interaction == BlockInteraction::Keep,
            hit_players,
            wind_sound: style.wind_sound,
            look: self.custom_explosion.as_ref().map(|c| c.look.clone()),
        });
        self.pending_blast = Some(PendingBlast {
            targets,
            source,
            indirect,
            interaction,
            radius,
            center,
            fire: style.fire,
        });
        // `hurtEntities` comes before `interactWithBlocks`: the blocks wait for the damage.
        if self.output.explosion_hurts.len() == queued {
            self.finish_explosion();
        }
        count
    }

    /// The plugins' explosion event (`vanillite_abi::event::ENTITY_EXPLODE`): `None` when a
    /// plugin cancelled it, else the radius, block interaction and fire it may have changed,
    /// and the blocks it left to blow up when it changed the list (`targets` without air).
    #[allow(clippy::too_many_arguments)]
    fn explode_event(
        &self,
        hooks: &dyn crate::hooks::GameHooks,
        source: Option<EntityId>,
        center: Vec3,
        radius: f32,
        interaction: BlockInteraction,
        style: &ExplosionStyle,
        block: Option<(BlockPos, BlockState)>,
        targets: &[BlockPos],
    ) -> Option<(f32, BlockInteraction, bool, Option<FxHashSet<BlockPos>>)> {
        use vanillite_abi::explode_flag;
        let destroys = matches!(
            interaction,
            BlockInteraction::Destroy | BlockInteraction::DestroyWithDecay
        );
        let mut flags = 0;
        if destroys {
            flags |= explode_flag::BREAK_BLOCKS;
        }
        if style.fire {
            flags |= explode_flag::FIRE;
        }
        let entity = source.and_then(|s| self.entities.store.get(s));
        let mut ev = crate::hooks::Event {
            entity: entity.as_ref().map_or(0, |e| e.uuid()),
            entity_type: entity
                .as_ref()
                .map_or_else(String::new, |e| e.kind().name().to_owned()),
            pos: block.map_or(BlockPos::containing(center.x, center.y, center.z), |b| b.0),
            state: block.map(|b| b.1),
            to: crate::hooks::Place::at(center),
            amount: f64::from(radius),
            flags,
            text: crate::hooks::positions_text(
                targets.iter().filter(|p| !self.block(**p).is_air()),
            ),
            ..crate::hooks::Event::new(vanillite_abi::event::ENTITY_EXPLODE)
        };
        hooks.event(crate::hooks::HookWorld::of(self), &mut ev);
        if ev.cancelled {
            return None;
        }
        let kept = ev
            .text_out
            .as_ref()
            .map(|t| crate::hooks::parse_positions(t.as_str()).collect());
        #[allow(clippy::cast_possible_truncation)]
        let radius = if ev.amount.is_finite() {
            ev.amount.clamp(0.0, crate::hooks::MAX_EXPLOSION_RADIUS) as f32
        } else {
            radius
        };
        let breaks = ev.flags & explode_flag::BREAK_BLOCKS != 0;
        let interaction = match (destroys, breaks) {
            (true, false) => BlockInteraction::Keep,
            (false, true) => BlockInteraction::Destroy,
            _ => interaction,
        };
        Some((
            radius,
            interaction,
            ev.flags & explode_flag::FIRE != 0,
            kept,
        ))
    }

    /// The block phase of the last explosion (`interactWithBlocks`, `createFire`), once the
    /// entities it hit took their damage. Does nothing when no explosion waits.
    pub fn finish_explosion(&mut self) {
        let Some(mut blast) = self.pending_blast.take() else {
            return;
        };
        if blast.interaction != BlockInteraction::Keep {
            self.interact_with_blocks(
                &mut blast.targets,
                blast.source,
                blast.indirect,
                blast.interaction,
                blast.radius,
                blast.center,
            );
        }
        if blast.fire {
            self.create_fire(&blast.targets);
        }
    }

    /// `calculateExplodedPositions`: the blocks the rays reach, in the iteration order of
    /// Vanilla's `HashSet<BlockPos>`.
    fn exploded_positions(
        &mut self,
        center: Vec3,
        radius: f32,
        override_resistance: BlockResistance,
    ) -> Vec<BlockPos> {
        let mut positions = Vec::new();
        match Cube::new(center, radius, override_resistance) {
            Some(mut cube) => self.cube_rays(
                &mut cube,
                center,
                radius,
                override_resistance,
                &mut positions,
            ),
            None => self.set_rays(center, radius, override_resistance, &mut positions),
        }
        java_hash_set_order(&mut positions);
        positions
    }

    /// The rays of [`Self::exploded_positions`] with the reached blocks and their losses in a
    /// [`Cube`] around the centre.
    fn cube_rays(
        &mut self,
        cube: &mut Cube,
        center: Vec3,
        radius: f32,
        override_resistance: BlockResistance,
        positions: &mut Vec<BlockPos>,
    ) {
        let (min, side) = (cube.min, cube.side);
        let max = min.offset(side - 1, side - 1, side - 1);
        // Rays only leave the world through the cube's sides.
        let checked = !(self.in_height(min.y)
            && self.in_height(max.y)
            && in_world_bounds_horizontal(min)
            && in_world_bounds_horizontal(max));
        let step = f64::from(0.3_f32);
        for &(xd, yd, zd) in ray_directions() {
            let mut power = radius * (0.7 + self.random.next_float() * 0.6);
            let (mut xp, mut yp, mut zp) = (center.x, center.y, center.z);
            while power > 0.0 {
                let pos = BlockPos::containing(xp, yp, zp);
                if checked && (!self.in_height(pos.y) || !in_world_bounds_horizontal(pos)) {
                    break;
                }
                let (x, y, z) = (pos.x - min.x, pos.y - min.y, pos.z - min.z);
                if (x | y | z) < 0 || x >= side || y >= side || z >= side {
                    // Outside the cube (the cube holds every ray): as without one.
                    let (loss, _) = self.ray_loss(pos, override_resistance);
                    if let Some(loss) = loss {
                        power -= loss;
                    }
                    if power > 0.0 && cube.outside.insert(pos) {
                        positions.push(pos);
                    }
                } else {
                    let cell = ((y * side + z) * side + x) as usize;
                    let mut loss = cube.losses[cell];
                    if loss == Cube::UNKNOWN {
                        loss = self
                            .ray_loss(pos, override_resistance)
                            .0
                            .unwrap_or(f32::NAN);
                        cube.losses[cell] = loss;
                    }
                    if !loss.is_nan() {
                        power -= loss;
                    }
                    if power > 0.0 {
                        let (word, bit) = (cell >> 6, 1u64 << (cell & 63));
                        if cube.reached[word] & bit == 0 {
                            cube.reached[word] |= bit;
                            positions.push(pos);
                        }
                    }
                }
                xp += xd * step;
                yp += yd * step;
                zp += zd * step;
                power -= 0.225_000_01;
            }
        }
    }

    /// The rays of [`Self::exploded_positions`] with a hash set of the reached blocks (large
    /// radii and rail protection).
    fn set_rays(
        &mut self,
        center: Vec3,
        radius: f32,
        override_resistance: BlockResistance,
        positions: &mut Vec<BlockPos>,
    ) {
        let mut seen = FxHashSet::default();
        let step = f64::from(0.3_f32);
        for &(xd, yd, zd) in ray_directions() {
            let mut power = radius * (0.7 + self.random.next_float() * 0.6);
            let (mut xp, mut yp, mut zp) = (center.x, center.y, center.z);
            while power > 0.0 {
                let pos = BlockPos::containing(xp, yp, zp);
                if !self.in_height(pos.y) || !in_world_bounds_horizontal(pos) {
                    break;
                }
                let (loss, rail_protected) = self.ray_loss(pos, override_resistance);
                if let Some(loss) = loss {
                    power -= loss;
                }
                if power > 0.0 && !rail_protected && seen.insert(pos) {
                    positions.push(pos);
                }
                xp += xd * step;
                yp += yd * step;
                zp += zd * step;
                power -= 0.225_000_01;
            }
        }
    }

    /// What a ray loses in the block at `pos` (`None`: nothing, for air without fluid) and
    /// whether the block is rail protected (`ExplosionDamageCalculator`).
    fn ray_loss(&self, pos: BlockPos, override_resistance: BlockResistance) -> (Option<f32>, bool) {
        let state = self.block(pos);
        let fluid = state.fluid_state();
        // `ExplosionDamageCalculator.getBlockExplosionResistance`.
        let rail_protected = override_resistance == BlockResistance::TntMinecart
            && (crate::block::is_rail(state.block())
                || crate::block::is_rail(self.block(pos.offset(0, 1, 0)).block()));
        let loss = if override_resistance == BlockResistance::Immune {
            self.custom_explosion
                .as_ref()
                .is_some_and(|c| c.immune.binary_search(&state.block().0).is_ok())
                .then_some((3_600_000.0 + 0.3) * 0.3)
        } else if override_resistance == BlockResistance::WindCharge {
            // `SimpleExplosionDamageCalculator` with immune blocks: only those resist,
            // everything else lets the ray through.
            crate::block::block_tags()
                .blocks_wind_charge_explosions
                .contains(state)
                .then_some((3_600_000.0 + 0.3) * 0.3)
        } else if override_resistance == BlockResistance::WaterAt(pos) {
            let water = vanillite_data::block::ids::WATER.default_state();
            Some((water.block().info().blast_resistance + 0.3) * 0.3)
        } else if !(state.is_air() && fluid.is_empty()) {
            let mut resistance = if rail_protected {
                0.0
            } else {
                state
                    .block()
                    .info()
                    .blast_resistance
                    .max(fluid.info().explosion_resistance)
            };
            if override_resistance == BlockResistance::DangerousWitherSkull {
                resistance =
                    crate::mob::boss::wither::skull_block_resistance(true, state, resistance);
            }
            Some((resistance + 0.3) * 0.3)
        } else {
            None
        };
        (loss, rail_protected)
    }

    /// `hurtEntities`: queues the damage of the living entities in range and pushes the
    /// entities away; players get their knockback with the explosion packet instead.
    fn hurt_entities(
        &mut self,
        source: Option<EntityId>,
        indirect: Option<EntityId>,
        center: Vec3,
        radius: f32,
        style: ExplosionStyle,
        destroys: bool,
    ) -> Vec<(EntityId, Vec3)> {
        let mut hit_players = Vec::new();
        let source_kind = source
            .and_then(|s| self.entities.store.get(s))
            .map(|e| e.kind());
        let player_wind_charge = source_kind == Some(entity_ids::WIND_CHARGE);
        let wind = player_wind_charge || source_kind == Some(entity_ids::BREEZE_WIND_CHARGE);
        if radius < 1.0e-5 {
            return hit_players;
        }
        let double_radius = radius * 2.0;
        let r = f64::from(double_radius) + 1.0;
        let area = Aabb::new(
            f64::from(floor(center.x - r)),
            f64::from(floor(center.y - r)),
            f64::from(floor(center.z - r)),
            f64::from(floor(center.x + r)),
            f64::from(floor(center.y + r)),
            f64::from(floor(center.z + r)),
        );
        let mut found = Vec::new();
        self.entities.store.for_each_in(&area, false, |id| {
            if Some(id) != source {
                found.push(id);
            }
        });
        // `Level.getEntities` adds the dragon parts in the area last (`dragonParts()`).
        let mut parts = Vec::new();
        if style.damages_entities {
            let ents = &self.entities;
            ents.store
                .for_each_in(&area.inflate(12.0, 12.0, 12.0), false, |id| {
                    if Some(id) != source
                        && let Some(s) = ents.mob(id).and_then(|m| m.boss.dragon())
                    {
                        for (i, p) in s.parts.iter().enumerate() {
                            let bb = s.part_box(i);
                            if bb.intersects(&area) {
                                parts.push((id, i, *p, bb));
                            }
                        }
                    }
                });
        }
        for id in found {
            let Some(entity) = self.entities.store.get(id) else {
                continue;
            };
            let kind = entity.kind();
            let (pos, aabb) = (entity.pos(), entity.aabb());
            // `getEntities(..., NO_SPECTATORS)`.
            if self
                .entities
                .player(id)
                .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Spectator)
            {
                continue;
            }
            let dist = pos.distance_to_sqr(center).sqrt() / f64::from(double_radius);
            if dist > 1.0 {
                continue;
            }
            // `ExplosionDamageCalculator.getEntityDamageAmount`: living entities and boats
            // through the mob system, items here (`ItemEntity.hurtServer`: health 5).
            let is_boat = crate::vehicle::BoatKind::of(kind).is_some()
                || crate::minecart::MinecartKind::of(kind).is_some();
            // `ItemEntity.ignoreExplosion`: an explosion that does not affect block-like
            // entities (wind charges; with `mob_griefing` off every explosion that keeps the
            // blocks) neither hurts nor pushes items.
            let affects_blocklike = if self.settings.mob_griefing {
                !wind
            } else {
                destroys && !wind
            };
            if kind == entity_ids::ITEM && !affects_blocklike {
                continue;
            }
            // `ItemEntity.hurtServer`: a mob's explosion spares items without `mob_griefing`,
            // and `Invulnerable` items take none.
            let mob_spared = !self.settings.mob_griefing
                && indirect.is_some_and(|i| self.entities.mob(i).is_some());
            let invulnerable = crate::mob::entity_nbt::is_invulnerable(&self.entities, id)
                && !indirect
                    .and_then(|i| self.entities.player(i))
                    .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Creative);
            if kind == entity_ids::ITEM && style.damages_entities && !mob_spared && !invulnerable {
                let double_radius = f64::from(double_radius);
                let pow = (1.0 - dist) * f64::from(self.seen_percent(center, aabb));
                #[allow(clippy::cast_possible_truncation)]
                let amount = ((pow * pow + pow) / 2.0 * 7.0 * double_radius + 1.0) as f32;
                let destroyed = self.entities.item_mut(id).is_some_and(|d| {
                    // `ItemStack.canBeHurtBy`: the nether star resists explosions
                    // (`damage_resistant` with `#is_explosion`).
                    if crate::item::damage_resistant(&d.stack) == Some("minecraft:is_explosion") {
                        return false;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let health = (f32::from(d.health) - amount) as i16;
                    d.health = health;
                    health <= 0
                });
                if self.entities.item(id).is_some_and(|d| {
                    crate::item::damage_resistant(&d.stack) != Some("minecraft:is_explosion")
                }) {
                    // `gameEvent(ENTITY_DAMAGE, source.getEntity())`.
                    self.game_event(
                        crate::level::game_event::events().entity_damage,
                        pos,
                        crate::level::game_event::GameEventContext::of(indirect),
                    );
                }
                if destroyed {
                    // `Item.onDestroyed`: a bundle's or a container item's contents drop.
                    let contents = self
                        .entities
                        .item_mut(id)
                        .map(|d| crate::item_entity::take_destroyed_contents(&mut d.stack))
                        .unwrap_or_default();
                    for stack in contents {
                        crate::block::drops::spawn_item_moving(self, pos, stack, None, 0, None);
                    }
                    self.entities.despawn(id);
                    continue;
                }
            }
            if kind == entity_ids::EXPERIENCE_ORB && style.damages_entities {
                // `ExperienceOrb.hurtServer`: the damage wears its health (5) down.
                let double_radius = f64::from(double_radius);
                let pow = (1.0 - dist) * f64::from(self.seen_percent(center, aabb));
                #[allow(clippy::cast_possible_truncation)]
                let amount = ((pow * pow + pow) / 2.0 * 7.0 * double_radius + 1.0) as f32;
                let destroyed = self.entities.experience_orb_mut(id).is_some_and(|d| {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                    let health = (d.health as f32 - amount) as i32;
                    d.health = health;
                    health <= 0
                });
                if destroyed {
                    self.entities.despawn(id);
                    continue;
                }
            }
            let player = self.entities.player(id);
            let mob = self.entities.mob(id);
            let eye = match mob {
                Some(mob) => crate::mob::eye_height(mob),
                None => f64::from(kind.info().eye_height),
            };
            let origin = if kind == entity_ids::TNT {
                pos
            } else {
                Vec3::new(pos.x, pos.y + eye, pos.z)
            };
            let direction = (origin - center).normalize();
            let seen = self.seen_percent(center, aabb);
            let exposure = f64::from(seen);
            // `ExplosionDamageCalculator.getEntityDamageAmount` for the living entities.
            let crystal = kind == entity_ids::END_CRYSTAL;
            let decor = crate::decoration::ticks(kind);
            if style.damages_entities
                && (player.is_some()
                    || crystal
                    || mob.is_some_and(crate::mob::MobData::is_alive)
                    || is_boat
                    || decor)
            {
                let double_radius = f64::from(double_radius);
                let pow = (1.0 - dist) * exposure;
                #[allow(clippy::cast_possible_truncation)]
                let amount = ((pow * pow + pow) / 2.0 * 7.0 * double_radius + 1.0) as f32;
                self.output.explosion_hurts.push(ExplosionHurt {
                    target: id,
                    amount,
                    source,
                    indirect,
                    center,
                    blocklike: destroys && !wind,
                    wind,
                    bad_respawn_point: style.bad_respawn_point,
                    damage: style.damage,
                    part: None,
                });
            }
            if decor || crate::decoration::is_decor_kind(kind) {
                // Block-attached entities break when pushed; displays and markers do not move.
                continue;
            }
            // `EXPLOSION_KNOCKBACK_RESISTANCE` of any living entity, with its equipment (Blast
            // Protection).
            let name = "minecraft:explosion_knockback_resistance";
            let resistance = match mob {
                Some(m) => crate::mob::equipment::attribute(
                    m,
                    name,
                    m.attrs.explosion_knockback_resistance,
                    0.0,
                    1.0,
                ),
                None => {
                    crate::mob::attack::player_attribute(&self.entities, id, name, 0.0, 0.0, 1.0)
                }
            };
            let knockback =
                direction * ((1.0 - dist) * exposure * style.knockback * (1.0 - resistance));
            // `onExplosionHit`: where the explosion hit the player and what caused it; a
            // player's wind charge spares the fall back to that height
            // (`setIgnoreFallDamageFromCurrentImpulse`), other explosions end the grace.
            let at = self.entities.store.get(id).map_or(pos, |e| e.pos());
            if self.entities.player(id).is_some() {
                let cause =
                    source.and_then(|s| crate::advancements::view::snapshot(&self.entities, s));
                if let Some(p) = self.entities.player_mut(id) {
                    crate::advancements::tracking::explosion_hit(p, pos, cause);
                    let s = &mut p.survival;
                    if player_wind_charge {
                        s.impulse_grace = s.impulse_grace.max(40);
                        s.impulse_impact = Some(at);
                    } else {
                        s.impulse_grace = 0;
                    }
                }
            }
            let player = self.entities.player(id);
            if let Some(p) = player {
                // `hitPlayers`: not a creative player who flies.
                if !(p.game_mode == vanillite_config::GameMode::Creative && p.survival.flying) {
                    hit_players.push((id, knockback));
                }
                // A client moves the player by the packet, unless the server moves it itself.
                if !p.survival.simulate_movement {
                    continue;
                }
            }
            // `pushFromExplosion`; `AbstractArrow.push` does nothing in the ground (arrows and
            // tridents).
            let stuck = self.entities.arrow(id).is_some_and(|a| a.in_ground)
                || self.entities.missile(id).is_some_and(|d| {
                    d.kind == crate::mob::missile::MissileKind::Trident && d.in_ground
                });
            if !stuck
                && let Some(mut entity) = self.entities.store.get_mut(id)
                && knockback.x.is_finite()
                && knockback.y.is_finite()
                && knockback.z.is_finite()
            {
                let velocity = entity.as_ref().velocity() + knockback;
                entity.set_velocity(velocity);
                entity.set_flags(EntityFlags::HAS_IMPULSE, true);
            }
        }
        for (dragon, i, at, bb) in parts {
            let dist = at.distance_to_sqr(center).sqrt() / f64::from(double_radius);
            if dist > 1.0 {
                continue;
            }
            let pow = (1.0 - dist) * f64::from(self.seen_percent(center, bb));
            #[allow(clippy::cast_possible_truncation)]
            let amount = ((pow * pow + pow) / 2.0 * 7.0 * f64::from(double_radius) + 1.0) as f32;
            self.output.explosion_hurts.push(ExplosionHurt {
                target: dragon,
                amount,
                source,
                indirect,
                center,
                blocklike: destroys && !wind,
                wind,
                bad_respawn_point: style.bad_respawn_point,
                damage: style.damage,
                part: Some(i),
            });
        }
        hit_players
    }

    /// `interactWithBlocks`: the blocks in random order, then their drops.
    fn interact_with_blocks(
        &mut self,
        targets: &mut [BlockPos],
        source: Option<EntityId>,
        indirect: Option<EntityId>,
        interaction: BlockInteraction,
        radius: f32,
        center: Vec3,
    ) {
        // `Util.shuffle(targetBlocks, level.random)`.
        for i in (2..=targets.len()).rev() {
            let swap = self.random.next_int_bounded(i as i32) as usize;
            targets.swap(i - 1, swap);
        }
        // `StackCollector`s: the drops merged into stacks of up to 16 (`addOrAppendStack`).
        let mut stacks: Vec<(BlockPos, crate::item::ItemStack)> = Vec::new();
        let mut loot = Vec::new();
        let decay = (interaction == BlockInteraction::DestroyWithDecay).then_some(radius);
        let trigger =
            interaction == BlockInteraction::TriggerBlock && self.can_trigger_blocks(source);
        for &pos in targets.iter() {
            // `Block.onExplosionHit`.
            let state = self.block(pos);
            if trigger {
                self.trigger_block(state, pos);
            }
            let hive = crate::block::beehive::is_beehive(state.block());
            if state.is_air() || interaction == BlockInteraction::TriggerBlock {
                // `BeehiveBlock.onExplosionHit` angers the bees around in any explosion.
                if hive {
                    crate::block::beehive::anger_nearby_bees(self, pos);
                }
                continue;
            }
            if state.is(ids::CREAKING_HEART) {
                crate::block::creaking_heart::on_explosion_hit(
                    self, pos, state, source, indirect, center,
                );
            }
            // `dropFromExplosion`: everything but TNT; the loot is rolled block by block.
            if !state.is(ids::TNT) {
                if hive {
                    crate::block::beehive::exploded(self, pos, state, source);
                }
                loot.clear();
                // `spawnAfterBreak` with experience only for a player's explosion.
                let by_player = indirect.is_some_and(|i| self.entities.player(i).is_some());
                crate::block::break_hooks::spawn_after_break(
                    self,
                    state,
                    pos,
                    &crate::item::ItemStack::EMPTY,
                    by_player,
                );
                drops::explosion_loot(self, pos, state, decay, source, &mut loot);
                // `addOrAppendStack`: an empty stack only starts a collector when there is none.
                'stacks: for mut stack in loot.drain(..) {
                    for (_, collected) in &mut stacks {
                        if crate::item_entity::are_mergable(collected, &stack) {
                            *collected =
                                crate::item_entity::merge_stacks_max(collected, &mut stack, 16);
                        }
                        if stack.is_empty() {
                            continue 'stacks;
                        }
                    }
                    stacks.push((pos, stack));
                }
            }
            self.set_block(pos, BlockState::AIR, update::ALL);
            // `wasExploded`.
            if state.is(ids::TNT) {
                tnt::was_exploded(self, pos, indirect);
            }
            if hive {
                crate::block::beehive::anger_nearby_bees(self, pos);
            }
        }
        // `Block.popResource` of the merged stacks (it draws the positions even without
        // `block_drops`).
        for (pos, stack) in stacks {
            drops::pop_resource(self, pos, stack);
        }
    }

    /// `canTriggerBlocks`: a breeze's wind charge only with `mobGriefing`.
    fn can_trigger_blocks(&self, source: Option<EntityId>) -> bool {
        let breeze = source
            .and_then(|s| self.entities.store.get(s))
            .is_some_and(|e| e.kind() == entity_ids::BREEZE_WIND_CHARGE);
        !breeze || self.settings.mob_griefing
    }

    /// The `onExplosionHit` overrides of the blocks a triggering explosion reaches.
    fn trigger_block(&mut self, state: BlockState, pos: BlockPos) {
        use crate::block::redstone;
        match state.block().info().kind {
            BlockKind::DoorBlock | BlockKind::WeatheringCopperDoorBlock => {
                redstone::door_explosion_hit(self, state, pos);
            }
            BlockKind::TrapDoorBlock | BlockKind::WeatheringCopperTrapDoorBlock => {
                redstone::trapdoor_explosion_hit(self, state, pos);
            }
            BlockKind::FenceGateBlock => redstone::fence_gate_explosion_hit(self, state, pos),
            BlockKind::ButtonBlock => redstone::button_explosion_hit(self, state, pos),
            BlockKind::LeverBlock => redstone::lever_pull(self, state, pos),
            BlockKind::BellBlock => {
                crate::block::bell::attempt_to_ring(self, None, pos, None);
            }
            BlockKind::CandleBlock | BlockKind::CandleCakeBlock if state.flag("lit") => {
                // `AbstractCandleBlock.extinguish(null, ...)`; the smoke is the client's.
                self.set_block(
                    pos,
                    state.set_flag("lit", false),
                    update::ALL | update::IMMEDIATE,
                );
                let sound = crate::block::sound_id("block.candle.extinguish");
                self.play_block_sound(None, pos, sound, crate::level::SOUND_BLOCKS, 1.0, 1.0);
                let event = super::game_event::events().block_change;
                self.game_event_at(None, event, pos);
            }
            _ => {}
        }
    }

    /// `createFire`: a third of the blown up air positions above solid blocks catch fire.
    fn create_fire(&mut self, targets: &[BlockPos]) {
        for &pos in targets {
            if self.random.next_int_bounded(3) == 0
                && self.block(pos).is_air()
                && vanillite_worldgen::behaviour::is_solid_render(self.block(pos.offset(0, -1, 0)))
            {
                let fire = crate::block::place::fire_state(&*self, pos);
                self.set_block_and_update(pos, fire);
            }
        }
    }

    /// `ServerExplosion.getSeenPercent(center, entity)`: the share of points on the entity's box
    /// from which the line to the centre hits no block.
    fn seen_percent(&self, center: Vec3, bb: Aabb) -> f32 {
        let xs = 1.0 / ((bb.max_x - bb.min_x) * 2.0 + 1.0);
        let ys = 1.0 / ((bb.max_y - bb.min_y) * 2.0 + 1.0);
        let zs = 1.0 / ((bb.max_z - bb.min_z) * 2.0 + 1.0);
        let x_offset = (1.0 - (1.0 / xs).floor() * xs) / 2.0;
        let z_offset = (1.0 - (1.0 / zs).floor() * zs) / 2.0;
        if xs < 0.0 || ys < 0.0 || zs < 0.0 {
            return 0.0;
        }
        let mut hits = 0;
        let mut count = 0;
        let mut xx = 0.0;
        while xx <= 1.0 {
            let mut yy = 0.0;
            while yy <= 1.0 {
                let mut zz = 0.0;
                while zz <= 1.0 {
                    let x = lerp(xx, bb.min_x, bb.max_x);
                    let y = lerp(yy, bb.min_y, bb.max_y);
                    let z = lerp(zz, bb.min_z, bb.max_z);
                    let from = Vec3::new(x + x_offset, y, z + z_offset);
                    if !self.clip_hits(from, center) {
                        hits += 1;
                    }
                    count += 1;
                    zz += zs;
                }
                yy += ys;
            }
            xx += xs;
        }
        hits as f32 / count as f32
    }

    /// `clip(new ClipContext(from, to, COLLIDER, NONE, entity)).getType() != MISS`: whether a
    /// collision shape lies on the line (`BlockGetter.traverseBlocks`).
    fn clip_hits(&self, from: Vec3, to: Vec3) -> bool {
        if from == to {
            return false;
        }
        let to_x = lerp(-1.0e-7, to.x, from.x);
        let to_y = lerp(-1.0e-7, to.y, from.y);
        let to_z = lerp(-1.0e-7, to.z, from.z);
        let from_x = lerp(-1.0e-7, from.x, to.x);
        let from_y = lerp(-1.0e-7, from.y, to.y);
        let from_z = lerp(-1.0e-7, from.z, to.z);
        let mut bx = floor(from_x);
        let mut by = floor(from_y);
        let mut bz = floor(from_z);
        if self.shape_clips(BlockPos::new(bx, by, bz), from, to) {
            return true;
        }
        let dx = to_x - from_x;
        let dy = to_y - from_y;
        let dz = to_z - from_z;
        let sign = |d: f64| -> i32 {
            if d == 0.0 {
                0
            } else if d > 0.0 {
                1
            } else {
                -1
            }
        };
        let (sx, sy, sz) = (sign(dx), sign(dy), sign(dz));
        let t_delta = |s: i32, d: f64| {
            if s == 0 { f64::MAX } else { f64::from(s) / d }
        };
        let (tdx, tdy, tdz) = (t_delta(sx, dx), t_delta(sy, dy), t_delta(sz, dz));
        let start = |s: i32, f: f64| if s > 0 { 1.0 - frac(f) } else { frac(f) };
        let mut tx = tdx * start(sx, from_x);
        let mut ty = tdy * start(sy, from_y);
        let mut tz = tdz * start(sz, from_z);
        while tx <= 1.0 || ty <= 1.0 || tz <= 1.0 {
            if tx < ty {
                if tx < tz {
                    bx += sx;
                    tx += tdx;
                } else {
                    bz += sz;
                    tz += tdz;
                }
            } else if ty < tz {
                by += sy;
                ty += tdy;
            } else {
                bz += sz;
                tz += tdz;
            }
            if self.shape_clips(BlockPos::new(bx, by, bz), from, to) {
                return true;
            }
        }
        false
    }

    /// `VoxelShape.clip(from, to, pos) != null` for the collision shape at `pos`.
    fn shape_clips(&self, pos: BlockPos, from: Vec3, to: Vec3) -> bool {
        let shape = self.block(pos).collision_shape();
        if shape.is_empty() {
            return false;
        }
        let diff = to - from;
        if diff.length_sqr() < 1.0e-7 {
            return false;
        }
        let origin = [f64::from(pos.x), f64::from(pos.y), f64::from(pos.z)];
        let test = from + diff * 0.001;
        let test = [test.x - origin[0], test.y - origin[1], test.z - origin[2]];
        if shape
            .iter()
            .any(|b| (0..3).all(|a| b[a] <= test[a] && test[a] < b[a + 3]))
        {
            return true;
        }
        // `AABB.clip(boxes, from, to, pos)`.
        let from = [from.x, from.y, from.z];
        let d = [diff.x, diff.y, diff.z];
        let mut scale = 1.0;
        let mut hit = false;
        for b in shape {
            let min = [b[0] + origin[0], b[1] + origin[1], b[2] + origin[2]];
            let max = [b[3] + origin[0], b[4] + origin[1], b[5] + origin[2]];
            // X, then Y, then Z, as `getDirection`.
            for (a, (b1, c1)) in [(0, (1, 2)), (1, (2, 0)), (2, (0, 1))] {
                let plane = if d[a] > 1.0e-7 {
                    min[a]
                } else if d[a] < -1.0e-7 {
                    max[a]
                } else {
                    continue;
                };
                let s = (plane - from[a]) / d[a];
                let pb = from[b1] + s * d[b1];
                let pc = from[c1] + s * d[c1];
                if 0.0 < s
                    && s < scale
                    && min[b1] - 1.0e-7 < pb
                    && pb < max[b1] + 1.0e-7
                    && min[c1] - 1.0e-7 < pc
                    && pc < max[c1] + 1.0e-7
                {
                    scale = s;
                    hit = true;
                }
            }
        }
        hit
    }
}

/// The ray directions of `calculateExplodedPositions` (the 16 × 16 × 16 grid's surface), in
/// Vanilla's order: float arithmetic, widened to double, normalised.
fn ray_directions() -> &'static [(f64, f64, f64)] {
    static RAYS: std::sync::OnceLock<Vec<(f64, f64, f64)>> = std::sync::OnceLock::new();
    RAYS.get_or_init(|| {
        let mut rays = Vec::with_capacity(1352);
        for xx in 0..16 {
            for yy in 0..16 {
                for zz in 0..16 {
                    if !(xx == 0 || xx == 15 || yy == 0 || yy == 15 || zz == 0 || zz == 15) {
                        continue;
                    }
                    let xd = f64::from(xx as f32 / 15.0 * 2.0 - 1.0);
                    let yd = f64::from(yy as f32 / 15.0 * 2.0 - 1.0);
                    let zd = f64::from(zz as f32 / 15.0 * 2.0 - 1.0);
                    let d = (xd * xd + yd * yd + zd * zd).sqrt();
                    rays.push((xd / d, yd / d, zd / d));
                }
            }
        }
        rays
    })
}

/// The blocks an explosion's rays can reach (rays cross the blocks near the centre many
/// times): which of them a ray reached and what a ray loses in each, looked up once.
struct Cube {
    min: BlockPos,
    side: i32,
    /// A bit per block.
    reached: Vec<u64>,
    /// Per block: [`Cube::UNKNOWN`], `NaN` for no loss, else the loss.
    losses: Vec<f32>,
    /// Reached positions outside the cube (cannot happen; kept for safety).
    outside: FxHashSet<BlockPos>,
}

impl Cube {
    /// Cubes up to this side are used.
    const MAX_SIDE: i32 = 64;
    const UNKNOWN: f32 = f32::NEG_INFINITY;

    /// The cube around `center`; `None` for radii too large for one and for rail protection
    /// (a block's loss depends on the one above it there, and it hides blocks).
    fn new(center: Vec3, radius: f32, override_resistance: BlockResistance) -> Option<Self> {
        if override_resistance == BlockResistance::TntMinecart {
            return None;
        }
        // The longest ray: power `radius * 1.3`, 0.225 per 0.3-block step, plus the block
        // around the start and rounding.
        let reach = f64::from(radius) * 1.3 / 0.225 * 0.3 + 2.0;
        if !(reach.is_finite() && reach * 2.0 + 1.0 <= f64::from(Self::MAX_SIDE)) {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        let reach = reach.ceil() as i32;
        let side = reach * 2 + 1;
        let cells = (side * side * side) as usize;
        Some(Self {
            min: BlockPos::containing(center.x, center.y, center.z).offset(-reach, -reach, -reach),
            side,
            reached: vec![0; (cells >> 6) + 1],
            losses: vec![Self::UNKNOWN; cells],
            outside: FxHashSet::default(),
        })
    }
}

/// `Level.isInWorldBoundsHorizontal(pos)`.
fn in_world_bounds_horizontal(pos: BlockPos) -> bool {
    pos.x >= -30_000_000 && pos.z >= -30_000_000 && pos.x < 30_000_000 && pos.z < 30_000_000
}

/// Orders unique positions, given in insertion order, as Java's `HashSet<BlockPos>` iterates
/// them: by bucket of the final table (`Vec3i.hashCode`, spread by `HashMap.hash`), insertion
/// order within a bucket. Buckets of eight or more entries, which Java turns into trees, do not
/// occur for the positions of one explosion.
fn java_hash_set_order(positions: &mut [BlockPos]) {
    let mut capacity: usize = 16;
    while positions.len() * 4 > capacity * 3 {
        capacity *= 2;
    }
    let mask = capacity - 1;
    // Bucket, then insertion order: unique keys, so an unstable sort keeps insertion order.
    let mut keys: Vec<u64> = positions
        .iter()
        .enumerate()
        .map(|(i, pos)| {
            let h = pos
                .y
                .wrapping_add(pos.z.wrapping_mul(31))
                .wrapping_mul(31)
                .wrapping_add(pos.x);
            let spread = h ^ ((h as u32) >> 16) as i32;
            ((spread as u32 as usize & mask) as u64) << 32 | i as u64
        })
        .collect();
    keys.sort_unstable();
    let unordered = positions.to_vec();
    for (slot, key) in positions.iter_mut().zip(keys) {
        *slot = unordered[(key & u64::from(u32::MAX)) as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_set_order_matches_java() {
        // Hashes 0, 1, 961 and 31 fall into buckets 0, 1, 1 and 15: Java iterates (0,0,0),
        // then (1,0,0) and (0,0,1) in insertion order, then (0,1,0).
        let mut positions = vec![
            BlockPos::new(0, 0, 0),
            BlockPos::new(1, 0, 0),
            BlockPos::new(0, 0, 1),
            BlockPos::new(0, 1, 0),
        ];
        java_hash_set_order(&mut positions);
        assert_eq!(
            positions,
            vec![
                BlockPos::new(0, 0, 0),
                BlockPos::new(1, 0, 0),
                BlockPos::new(0, 0, 1),
                BlockPos::new(0, 1, 0),
            ]
        );
    }
}
