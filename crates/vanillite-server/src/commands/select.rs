//! Resolving entity selectors (Vanilla's `EntitySelector.findEntities` and `findPlayers`).
//! Entities of the source's world are read directly; players of other worlds are known by
//! name, UUID and game mode from the player list. Other kinds in other worlds are only
//! reachable when the execution has those worlds at hand ([`Env::other_levels`], sequential
//! dimensions); otherwise (region mode) selectors without position options see the
//! non-player entities of the source's world only.

use vanillite_base::{Aabb, Vec3};
use vanillite_command::Source as _;
use vanillite_command::error::CommandError;
use vanillite_command::selector::{AdvancementCheck, EntitySelector, Filter, Order};
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_game::entities::Entities;
use vanillite_protocol::text::Component;
use vanillite_world::entity::EntityId;

use super::Env;
use super::source::{CommandSource, SourceEntity};
use crate::global::Global;
use crate::world::WorldId;

/// A selected entity.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    /// UUID.
    pub(crate) uuid: u128,
    /// `getName`: the player name, else the custom name or the type's name.
    pub(crate) name: String,
    /// The world the entity is in.
    pub(crate) world: WorldId,
    /// The entity, when it is in the world the command runs on.
    pub(crate) entity: Option<EntityId>,
    /// Position and rotation (`y_rot`, `x_rot`), when it is in the command's world.
    pub(crate) place: Option<(Vec3, f32, f32)>,
    /// Game mode (players).
    pub(crate) game_mode: vanillite_config::GameMode,
    /// Entity type.
    pub(crate) kind: EntityTypeId,
}

impl Target {
    /// Whether it is a player.
    pub(crate) fn is_player(&self) -> bool {
        self.kind == ids::PLAYER
    }

    /// `getScoreboardName`: the player name or the UUID.
    pub(crate) fn score_name(&self) -> String {
        if self.is_player() {
            self.name.clone()
        } else {
            vanillite_protocol::types::Uuid(self.uuid).to_string()
        }
    }

    /// `getDisplayName`.
    pub(crate) fn display(&self) -> Component {
        if self.is_player() {
            vanillite_game::player::join::display_name(&self.name, self.uuid)
        } else {
            Component::translatable(
                format!("entity.{}", self.kind.name().replace(':', ".")),
                vec![],
            )
        }
    }

    /// The entity as a command source entity.
    pub(crate) fn source_entity(&self) -> SourceEntity {
        SourceEntity {
            uuid: self.uuid,
            world: self.world,
            id: self.entity,
            kind: self.kind,
            name: self.name.clone(),
            display: self.display(),
            eye_height: f64::from(self.kind.info().eye_height),
        }
    }
}

/// The English name of a type without a custom name (`EntityType.getDescription`): the words
/// of its path, capitalised.
pub(crate) fn type_name(kind: EntityTypeId) -> String {
    let path = kind.name().rsplit(':').next().unwrap_or("");
    path.split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The target of an entity of this world.
pub(crate) fn local(entities: &Entities, world: WorldId, id: EntityId) -> Option<Target> {
    let e = entities.store.get(id)?;
    let kind = e.kind();
    let (name, game_mode) = match entities.player(id) {
        Some(p) => (p.profile.name.clone(), p.game_mode),
        None => (type_name(kind), vanillite_config::GameMode::Survival),
    };
    Some(Target {
        uuid: e.uuid(),
        name,
        world,
        entity: Some(id),
        place: Some((e.pos(), e.rot().y_rot, e.rot().x_rot)),
        game_mode,
        kind,
    })
}

/// All online players in player list order; those in this world with their entity.
pub(crate) fn online_players(entities: &Entities, global: &Global, world: WorldId) -> Vec<Target> {
    let local: Vec<(u128, EntityId, Vec3, f32, f32)> = entities
        .players
        .iter()
        .filter_map(|(id, _)| {
            let e = entities.store.get(id)?;
            Some((e.uuid(), id, e.pos(), e.rot().y_rot, e.rot().x_rot))
        })
        .collect();
    global
        .players
        .snapshot()
        .into_iter()
        .map(|p| {
            let here = local
                .iter()
                .find(|l| l.0 == p.uuid.0)
                .filter(|_| p.world == world);
            Target {
                uuid: p.uuid.0,
                name: p.name,
                world: p.world,
                entity: here.map(|l| l.1),
                place: here.map(|l| (l.2, l.3, l.4)),
                game_mode: p.info.game_mode,
                kind: ids::PLAYER,
            }
        })
        .collect()
}

/// `Mth.wrapDegrees`.
fn wrap_degrees(v: f64) -> f64 {
    let w = v % 360.0;
    if w >= 180.0 {
        w - 360.0
    } else if w < -180.0 {
        w + 360.0
    } else {
        w
    }
}

fn rotation_matches(range: &vanillite_command::bounds::FloatRange, rot: f32) -> bool {
    let min = wrap_degrees(f64::from(range.min.unwrap_or(0.0)));
    let max = wrap_degrees(f64::from(range.max.unwrap_or(359.0)));
    let rot = wrap_degrees(f64::from(rot));
    if min > max {
        rot >= min || rot <= max
    } else {
        rot >= min && rot <= max
    }
}

/// `EntitySelector.createAabb` moved to the origin.
fn volume(selector: &EntitySelector, origin: Vec3) -> Option<Aabb> {
    if selector.delta.iter().all(Option::is_none) {
        return None;
    }
    let [dx, dy, dz] = selector.delta.map(|d| d.unwrap_or(0.0));
    let span = |d: f64| if d < 0.0 { (d, 1.0) } else { (0.0, d + 1.0) };
    let (x0, x1) = span(dx);
    let (y0, y1) = span(dy);
    let (z0, z1) = span(dz);
    Some(Aabb::new(
        origin.x + x0,
        origin.y + y0,
        origin.z + z0,
        origin.x + x1,
        origin.y + y1,
        origin.z + z1,
    ))
}

/// What the filters read of the world.
struct Ctx<'a> {
    entities: &'a Entities,
    /// The chunks of the entities' level (`location_check` in predicates).
    chunks: &'a vanillite_world::chunks::Chunks,
    global: &'a Global,
    origin: Vec3,
    aabb: Option<Aabb>,
}

/// Whether a target passes the selector's predicates (`EntitySelector.getPredicate`).
fn matches(selector: &EntitySelector, target: &Target, ctx: &Ctx<'_>) -> bool {
    if let Some(range) = &selector.distance {
        let Some((pos, _, _)) = target.place else {
            return false;
        };
        let d = pos - ctx.origin;
        if !range.matches((d.x * d.x + d.y * d.y + d.z * d.z).sqrt()) {
            return false;
        }
    }
    if let Some(aabb) = &ctx.aabb {
        let Some(id) = target.entity else {
            return false;
        };
        if !ctx
            .entities
            .store
            .get(id)
            .is_some_and(|e| e.aabb().intersects(aabb))
        {
            return false;
        }
    }
    if let Some(range) = &selector.level {
        let level = target
            .entity
            .and_then(|id| ctx.entities.player(id))
            .map(|p| p.survival.xp.level);
        if !target.is_player() || !range.matches(level.unwrap_or(0)) {
            return false;
        }
    }
    if let Some(range) = &selector.x_rotation
        && !target
            .place
            .is_some_and(|(_, _, x)| rotation_matches(range, x))
    {
        return false;
    }
    if let Some(range) = &selector.y_rotation
        && !target
            .place
            .is_some_and(|(_, y, _)| rotation_matches(range, y))
    {
        return false;
    }
    selector
        .filters
        .iter()
        .all(|filter| filter_matches(filter, target, ctx))
}

fn filter_matches(filter: &Filter, target: &Target, ctx: &Ctx<'_>) -> bool {
    let systems = &ctx.global.systems;
    match filter {
        Filter::Alive => match target.entity.and_then(|id| ctx.entities.player(id)) {
            Some(p) => !p.survival.is_dead_or_dying(),
            None => true,
        },
        Filter::Name { name, inverted } => (target.name == *name) != *inverted,
        Filter::GameMode { mode, inverted } => {
            target.is_player()
                && ((super::exec::config_game_mode(*mode) == target.game_mode) != *inverted)
        }
        Filter::Team { team, inverted } => {
            let board = systems.scoreboard();
            let own = board.team_of(&target.score_name()).map(|t| t.name.as_str());
            let is = if team.is_empty() {
                own.is_none()
            } else {
                own == Some(team.as_str())
            };
            is != *inverted
        }
        Filter::Type { id, inverted } => (target.kind.name() == id) != *inverted,
        Filter::TypeTag { tag, inverted } => {
            systems.in_tag("minecraft:entity_type", tag, target.kind.name()) != *inverted
        }
        Filter::Tag { tag, inverted } => {
            let tags = target.entity.map_or(&[][..], |id| ctx.entities.tags_of(id));
            let has = if tag.is_empty() {
                tags.is_empty()
            } else {
                tags.iter().any(|t| t == tag)
            };
            has != *inverted
        }
        Filter::Nbt { nbt, inverted } => {
            let data = target
                .entity
                .and_then(|id| super::data::entity_nbt(ctx.entities, id));
            let found = data.is_some_and(|d| super::nbt_ops::compound_matches(nbt, &d));
            found != *inverted
        }
        Filter::Scores(scores) => {
            let board = systems.scoreboard();
            let holder = target.score_name();
            scores.iter().all(|(objective, range)| {
                board.objective(objective).is_some()
                    && board
                        .score(&holder, objective)
                        .is_some_and(|s| range.matches(s.value))
            })
        }
        // Players of other worlds are seen without progress.
        Filter::Advancements(checks) => {
            let progress = target
                .entity
                .and_then(|id| ctx.entities.player(id))
                .map(|p| &p.advancements);
            let set = systems.advancements();
            target.is_player()
                && checks.iter().all(|(id, check)| {
                    let Some(index) = set.index(id) else {
                        return false;
                    };
                    match check {
                        AdvancementCheck::Done(done) => {
                            progress.is_some_and(|p| p.is_done(index)) == *done
                        }
                        AdvancementCheck::Criteria(criteria) => {
                            criteria.iter().all(|(name, done)| {
                                set.get(index)
                                    .and_then(|a| a.criterion(name))
                                    .is_some_and(|c| {
                                        progress.is_some_and(|p| p.criterion_done(index, c))
                                            == *done
                                    })
                            })
                        }
                    }
                })
        }
        // `LootContextParamSets.SELECTOR`: the entity at its position. The level's random is
        // not reachable here; a random seeded by the game time and the entity stands in.
        Filter::Predicate { id, inverted } => {
            let origin = target.place.map_or(ctx.origin, |p| p.0);
            #[allow(clippy::cast_possible_truncation)]
            let mut rng = vanillite_base::random::LegacyRandom::new(
                ctx.global.level.game_time() ^ (target.uuid as i64),
            );
            let weather = {
                let w = ctx.global.level.weather();
                (w.raining, w.thundering)
            };
            let passes = super::loot_cmd::test_predicate(
                ctx.entities,
                Some(ctx.chunks),
                ctx.global,
                target.entity,
                origin,
                weather,
                &(Some(id.clone()), None),
                &mut rng,
            )
            .unwrap_or(false);
            passes != *inverted
        }
    }
}

/// `EntitySelector.findEntities`. The caller checks counts and the selector permission.
pub(crate) fn select(
    selector: &EntitySelector,
    source: &CommandSource,
    env: &Env<'_>,
) -> Vec<Target> {
    let (entities, global) = (&env.level.entities, env.global);
    let here = source.world;
    // An entity of another world at hand: known like a player there, without entity and place.
    let remote = |entities: &Entities, world: WorldId, id: EntityId| {
        local(entities, world, id)
            .filter(|t| !t.is_player())
            .map(|t| Target {
                entity: None,
                place: None,
                ..t
            })
    };
    let players = || online_players(entities, global, here);
    if let Some(name) = &selector.player_name {
        return players()
            .into_iter()
            .filter(|p| p.name.eq_ignore_ascii_case(name))
            .take(1)
            .collect();
    }
    if let Some(uuid) = selector.uuid {
        if let Some(id) = find_uuid(entities, uuid) {
            return local(entities, here, id).into_iter().collect();
        }
        let other = env.other_levels().find_map(|(world, level)| {
            remote(&level.entities, world, find_uuid(&level.entities, uuid)?)
        });
        if let Some(other) = other {
            return vec![other];
        }
        return players()
            .into_iter()
            .filter(|p| p.uuid == uuid)
            .take(1)
            .collect();
    }
    let origin = Vec3::new(
        selector.position[0].unwrap_or(source.pos.x),
        selector.position[1].unwrap_or(source.pos.y),
        selector.position[2].unwrap_or(source.pos.z),
    );
    let ctx = Ctx {
        entities,
        chunks: &env.level.chunks,
        global,
        origin,
        aabb: volume(selector, origin),
    };
    let only_players = !selector.includes_entities;
    if selector.current_entity {
        let Some(own) = &source.entity else {
            return Vec::new();
        };
        if only_players && !own.is_player() {
            return Vec::new();
        }
        let target = match own.id.filter(|_| own.world == here) {
            Some(id) => local(entities, here, id),
            None => players().into_iter().find(|p| p.uuid == own.uuid),
        };
        return target
            .filter(|t| matches(selector, t, &ctx))
            .into_iter()
            .collect();
    }
    let limit = if selector.order == Order::Arbitrary {
        usize::try_from(selector.max_results).unwrap_or(usize::MAX)
    } else {
        usize::MAX
    };
    let mut found: Vec<Target> = Vec::new();
    if only_players {
        for p in players() {
            if found.len() >= limit {
                break;
            }
            if (!selector.world_limited || p.world == here) && matches(selector, &p, &ctx) {
                found.push(p);
            }
        }
    } else {
        let type_id = selector
            .limit_to_type
            .as_deref()
            .and_then(EntityTypeId::by_name);
        // The source's world first, then the players of the other worlds.
        let columns = entities.store.columns();
        for (row, &id) in columns.id.iter().enumerate() {
            if found.len() >= limit {
                break;
            }
            if type_id.is_some_and(|t| columns.kind.get(row) != Some(&t)) {
                continue;
            }
            if let Some(t) = local(entities, here, id)
                && matches(selector, &t, &ctx)
            {
                found.push(t);
            }
        }
        if !selector.world_limited {
            for (world, level) in env.other_levels() {
                let ctx = Ctx {
                    entities: &level.entities,
                    chunks: &level.chunks,
                    global,
                    origin,
                    aabb: ctx.aabb,
                };
                let columns = level.entities.store.columns();
                for (row, &id) in columns.id.iter().enumerate() {
                    if found.len() >= limit {
                        break;
                    }
                    if type_id.is_some_and(|t| columns.kind.get(row) != Some(&t)) {
                        continue;
                    }
                    if let Some(t) = local(&level.entities, world, id)
                        && !t.is_player()
                        && matches(selector, &t, &ctx)
                        && let Some(t) = remote(&level.entities, world, id)
                    {
                        found.push(t);
                    }
                }
            }
        }
        if !selector.world_limited && type_id.is_none_or(|t| t == ids::PLAYER) {
            for p in players() {
                if found.len() >= limit {
                    break;
                }
                if p.world != here && matches(selector, &p, &ctx) {
                    found.push(p);
                }
            }
        }
    }
    let distance = |t: &Target| {
        t.place.map_or(f64::INFINITY, |(pos, _, _)| {
            let d = pos - origin;
            d.x * d.x + d.y * d.y + d.z * d.z
        })
    };
    match selector.order {
        Order::Arbitrary => {}
        Order::Nearest => found.sort_by(|a, b| distance(a).total_cmp(&distance(b))),
        Order::Furthest => found.sort_by(|a, b| distance(b).total_cmp(&distance(a))),
        Order::Random => shuffle(&mut found),
    }
    found.truncate(usize::try_from(selector.max_results).unwrap_or(usize::MAX));
    found
}

/// The entity of this world with a UUID.
pub(crate) fn find_uuid(entities: &Entities, uuid: u128) -> Option<EntityId> {
    entities.store.by_uuid(uuid)
}

/// `EntityArgument.getEntities`/`getPlayers`: at least one, with the selector permission.
pub(crate) fn many(
    env: &Env<'_>,
    source: &CommandSource,
    selector: &EntitySelector,
) -> Result<Vec<Target>, CommandError> {
    let found = optional(env, source, selector)?;
    if found.is_empty() {
        let key = if selector.includes_entities {
            "argument.entity.notfound.entity"
        } else {
            "argument.entity.notfound.player"
        };
        return Err(CommandError::new(key, vec![]));
    }
    Ok(found)
}

/// `EntityArgument.getOptionalEntities`: possibly none.
pub(crate) fn optional(
    env: &Env<'_>,
    source: &CommandSource,
    selector: &EntitySelector,
) -> Result<Vec<Target>, CommandError> {
    if selector.uses_selector && !source.allows_selectors() {
        return Err(CommandError::new(
            "argument.entity.selector.not_allowed",
            vec![],
        ));
    }
    Ok(select(selector, source, env))
}

/// `EntityArgument.getEntity`: exactly one.
pub(crate) fn single(
    env: &Env<'_>,
    source: &CommandSource,
    selector: &EntitySelector,
) -> Result<Target, CommandError> {
    let mut found = many(env, source, selector)?;
    if found.len() > 1 {
        let key = if selector.includes_entities {
            "argument.entity.toomany"
        } else {
            "argument.player.toomany"
        };
        return Err(CommandError::new(key, vec![]));
    }
    found
        .pop()
        .ok_or_else(|| CommandError::new("argument.entity.notfound.entity", vec![]))
}

/// A Fisher-Yates shuffle with a time-seeded xorshift (`@r` needs no cryptographic quality).
fn shuffle<T>(items: &mut [T]) {
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9E37_79B9, |d| d.subsec_nanos() as u64 | 1);
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}
