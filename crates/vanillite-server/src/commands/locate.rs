//! `locate` (`LocateCommand`) and `place` (`PlaceCommand`) on the world's terrain generator.
//!
//! `locate structure` searches like `ChunkGenerator.findNearestMapStructure` and `locate biome`
//! like `BiomeSource.findClosestBiome3d`. Points of interest are not tracked, so `locate poi`
//! finds nothing. `place` generates on a copy of the live chunks and replays the result on the
//! world (`vanillite_game::level::place`).

use std::time::Instant;

use vanillite_base::{BlockPos, ChunkPos};
use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, Args, GameValue, ResourceOrTag};
use vanillite_game::commands::{LocateOp, PlaceOp};
use vanillite_game::level::locate::{MAX_STRUCTURE_SEARCH_RADIUS, nearest_structure};
use vanillite_protocol::text::{Arg, ClickEvent, Color, Component, HoverEvent, NamedColor};
use vanillite_worldgen::biome::BiomeResolver;
use vanillite_worldgen::generator::TerrainGenerator;
use vanillite_worldgen::structure::{LevelHeight, StructureId};

use super::Env;
use super::blocks_cmd::{missing, not_loaded, this_world};
use super::execute::loaded_block_pos;
use super::source::CommandSource;

/// `MAX_BIOME_SEARCH_RADIUS`, `BIOME_SAMPLE_RESOLUTION_HORIZONTAL` and `_VERTICAL`.
const BIOME_SEARCH: (i32, i32, i32) = (6400, 32, 64);

fn source_block(source: &CommandSource) -> BlockPos {
    let p = source.pos;
    BlockPos::new(
        vanillite_base::math::floor(p.x),
        vanillite_base::math::floor(p.y),
        vanillite_base::math::floor(p.z),
    )
}

/// `asPrintable` of a resource or tag argument.
fn printable(arg: &ResourceOrTag) -> String {
    match arg {
        ResourceOrTag::Resource(id) => id.clone(),
        ResourceOrTag::Tag(tag) => format!("#{tag}"),
    }
}

fn resource_or_tag<'a>(args: &'a Args, name: &str) -> Result<&'a ResourceOrTag, CommandError> {
    match args.game(name) {
        Some(GameValue::ResourceOrTag(r)) => Ok(r),
        _ => Err(missing(name)),
    }
}

fn level_height(env: &Env<'_>) -> LevelHeight {
    LevelHeight {
        min_y: env.level.min_y(),
        height: env
            .level
            .max_y()
            .saturating_sub(env.level.min_y())
            .saturating_add(1),
    }
}

/// `locate ...` around the source in the world it runs on.
pub(super) fn locate(
    op: LocateOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    this_world(source, env)?;
    let origin = source_block(source);
    let started = Instant::now();
    match op {
        LocateOp::Structure => {
            let arg = resource_or_tag(args, "structure")?;
            let ids = match arg {
                ResourceOrTag::Resource(id) => vec![id.clone()],
                ResourceOrTag::Tag(tag) => {
                    let tags = env.global.systems.tags();
                    tags.get("minecraft:worldgen/structure")
                        .and_then(|t| t.get(tag))
                        .cloned()
                        .ok_or_else(|| {
                            CommandError::new(
                                "commands.locate.structure.invalid",
                                vec![ErrArg::Str(printable(arg))],
                            )
                        })?
                }
            };
            let found = env
                .level
                .generation
                .as_ref()
                .filter(|g| g.structures)
                .and_then(|g| {
                    let terrain = g.terrain();
                    let wanted: Vec<StructureId> = ids
                        .iter()
                        .filter_map(|id| terrain.structures().structure_id(id))
                        .collect();
                    nearest_structure(
                        &terrain,
                        &wanted,
                        origin,
                        level_height(env),
                        MAX_STRUCTURE_SEARCH_RADIUS,
                    )
                    .map(|(pos, id)| (pos, terrain.structures().structure(id).name.clone()))
                });
            let Some((pos, name)) = found else {
                return Err(CommandError::new(
                    "commands.locate.structure.not_found",
                    vec![ErrArg::Str(printable(arg))],
                ));
            };
            Ok(show_result(
                source,
                env,
                arg,
                origin,
                (pos, &name),
                "commands.locate.structure.success",
                false,
                started,
            ))
        }
        LocateOp::Biome => {
            let arg = resource_or_tag(args, "biome")?;
            let found = env.level.generation.as_ref().and_then(|g| {
                let terrain = &g.terrain();
                let accepted = |id: u16| {
                    let name = &terrain.biomes().get(id).name;
                    match arg {
                        ResourceOrTag::Resource(r) => r == name,
                        ResourceOrTag::Tag(tag) => {
                            env.global
                                .systems
                                .in_tag("minecraft:worldgen/biome", tag, name)
                        }
                    }
                };
                closest_biome(terrain, origin, env, &accepted)
                    .map(|(pos, id)| (pos, terrain.biomes().get(id).name.clone()))
            });
            let Some((pos, name)) = found else {
                return Err(CommandError::new(
                    "commands.locate.biome.not_found",
                    vec![ErrArg::Str(printable(arg))],
                ));
            };
            Ok(show_result(
                source,
                env,
                arg,
                origin,
                (pos, &name),
                "commands.locate.biome.success",
                true,
                started,
            ))
        }
        // `PoiManager.findClosestWithType`: no point of interest is tracked yet.
        LocateOp::Poi => {
            let arg = resource_or_tag(args, "poi")?;
            Err(CommandError::new(
                "commands.locate.poi.not_found",
                vec![ErrArg::Str(printable(arg))],
            ))
        }
    }
}

/// `LocateCommand.showLocateResult`.
#[allow(clippy::too_many_arguments)]
fn show_result(
    source: &CommandSource,
    env: &Env<'_>,
    arg: &ResourceOrTag,
    origin: BlockPos,
    (found, name): (BlockPos, &str),
    key: &str,
    include_y: bool,
    started: Instant,
) -> i32 {
    let found_name = match arg {
        ResourceOrTag::Resource(_) => printable(arg),
        ResourceOrTag::Tag(_) => format!("{} ({name})", printable(arg)),
    };
    let dx = found.x.wrapping_sub(origin.x);
    let dy = found.y.wrapping_sub(origin.y);
    let dz = found.z.wrapping_sub(origin.z);
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let distance = if include_y {
        let sqr = f64::from(dx) * f64::from(dx)
            + f64::from(dy) * f64::from(dy)
            + f64::from(dz) * f64::from(dz);
        vanillite_base::math::floor_f((sqr as f32).sqrt())
    } else {
        // `dist`: int arithmetic like Java.
        let sqr = dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz));
        vanillite_base::math::floor_f((sqr as f32).sqrt())
    };
    let y = if include_y {
        found.y.to_string()
    } else {
        "~".to_owned()
    };
    let coordinates = Component::translatable(
        "chat.square_brackets",
        vec![Component::translatable_args(
            "chat.coordinates",
            vec![Arg::Int(found.x), Arg::String(y.clone()), Arg::Int(found.z)],
        )],
    )
    .color(Color::Named(NamedColor::Green))
    .click(ClickEvent::SuggestCommand(format!(
        "/tp @s {} {y} {}",
        found.x, found.z
    )))
    .hover(HoverEvent::ShowText(Component::translatable(
        "chat.coordinates.tooltip",
        vec![],
    )));
    source.send_success(
        env.global,
        Component::translatable_args(
            key,
            vec![
                Arg::String(found_name.clone()),
                Arg::from(coordinates),
                Arg::Int(distance),
            ],
        ),
        false,
    );
    log::info!(
        "Locating element {found_name} took {} ms",
        started.elapsed().as_millis()
    );
    distance
}

/// `Mth.outFromOrigin(origin, lower, upper, step)`.
fn out_from_origin(origin: i32, lower: i32, upper: i32, step: i32) -> Vec<i32> {
    let mut out = Vec::new();
    if lower > upper || step < 1 {
        return out;
    }
    let center = origin.clamp(lower, upper);
    let mut cursor = center;
    loop {
        let distance = center.abs_diff(cursor);
        let distance = i32::try_from(distance).unwrap_or(i32::MAX);
        if !(center.saturating_sub(distance) >= lower || center.saturating_add(distance) <= upper) {
            break;
        }
        out.push(cursor);
        let previous_negative = cursor <= center;
        let can_move_positive = center.saturating_add(distance).saturating_add(step) <= upper;
        cursor = 'next: {
            if !previous_negative || !can_move_positive {
                let attempt = center
                    .saturating_sub(distance)
                    .saturating_sub(if previous_negative { step } else { 0 });
                if attempt >= lower {
                    break 'next attempt;
                }
            }
            center.saturating_add(distance).saturating_add(step)
        };
    }
    out
}

/// `BlockPos.spiralAround(ZERO, radius, EAST, SOUTH)`: calls `f` with each `(x, z)` until it
/// returns a value.
fn spiral<T>(radius: i32, mut f: impl FnMut(i32, i32) -> Option<T>) -> Option<T> {
    const DIRECTIONS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];
    let (mut x, mut z) = (0i32, 1i32);
    let legs = radius.saturating_mul(4);
    let (mut leg, mut leg_size, mut leg_index) = (-1i32, 0i32, 0i32);
    loop {
        let index = usize::try_from(leg.saturating_add(4) % 4).unwrap_or(0);
        let (dx, dz) = DIRECTIONS.get(index).copied().unwrap_or((0, 0));
        x = x.saturating_add(dx);
        z = z.saturating_add(dz);
        if leg_index >= leg_size {
            if leg >= legs {
                return None;
            }
            leg = leg.saturating_add(1);
            leg_index = 0;
            leg_size = leg / 2 + 1;
        }
        leg_index = leg_index.saturating_add(1);
        if let Some(found) = f(x, z) {
            return Some(found);
        }
    }
}

/// `BiomeSource.findClosestBiome3d(origin, 6400, 32, 64, allowed, ...)`.
fn closest_biome(
    terrain: &TerrainGenerator,
    origin: BlockPos,
    env: &Env<'_>,
    allowed: &dyn Fn(u16) -> bool,
) -> Option<(BlockPos, u16)> {
    let source = terrain.biome_source();
    let candidates: Vec<u16> = source
        .possible_biomes()
        .into_iter()
        .filter(|&b| allowed(b))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let (radius, horizontal, vertical) = BIOME_SEARCH;
    let mut resolver = source.create_caching_resolver(terrain.program(), terrain.climate());
    let ys = out_from_origin(
        origin.y,
        env.level.min_y().saturating_add(1),
        env.level.max_y().saturating_add(1),
        vertical,
    );
    spiral(radius.div_euclid(horizontal), |sx, sz| {
        let x = origin.x.saturating_add(sx.saturating_mul(horizontal));
        let z = origin.z.saturating_add(sz.saturating_mul(horizontal));
        ys.iter().find_map(|&y| {
            let biome = resolver.noise_biome(x >> 2, y >> 2, z >> 2);
            candidates
                .contains(&biome)
                .then(|| (BlockPos::new(x, y, z), biome))
        })
    })
}

/// The packs and the terrain generator `place` generates with.
fn generation(
    env: &Env<'_>,
) -> Option<(
    std::sync::Arc<vanillite_datapack::LoadedPacks>,
    std::sync::Arc<TerrainGenerator>,
)> {
    let packs = env.global.packs.loaded()?;
    let generator = env.level.generation.as_ref()?;
    Some((std::sync::Arc::clone(packs), generator.terrain()))
}

/// The random of a placement (`level.getRandom()`).
fn placement_random(env: &mut Env<'_>) -> vanillite_base::random::DecorationRandom {
    use vanillite_base::random::RandomSource;
    vanillite_base::random::DecorationRandom::new(env.level.random().next_long())
}

/// `PlaceCommand`: generates on a copy of the live chunks around the placement and replays it
/// on the world.
pub(super) fn place(
    op: PlaceOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    use vanillite_game::level::place::PlaceArea;
    use vanillite_worldgen::place as wgp;
    this_world(source, env)?;
    let pos_arg = match op {
        PlaceOp::Jigsaw => "position",
        _ => "pos",
    };
    let pos = if args.has(pos_arg) {
        loaded_block_pos(args, pos_arg, source, env)?
    } else {
        source_block(source)
    };
    // `checkLoaded` over the chunks the placement starts in.
    let check = |env: &Env<'_>, min: ChunkPos, max: ChunkPos| {
        let loaded = (min.x..=max.x).all(|x| {
            (min.z..=max.z).all(|z| env.level.chunks.chunk(ChunkPos::new(x, z)).is_some())
        });
        if loaded { Ok(()) } else { Err(not_loaded()) }
    };
    let chunk = pos.chunk();
    let coords = || vec![Arg::Int(pos.x), Arg::Int(pos.y), Arg::Int(pos.z)];
    let height = LevelHeight {
        min_y: env.level.min_y(),
        height: env.level.max_y() - env.level.min_y() + 1,
    };
    match op {
        PlaceOp::Feature => {
            let (id, inline) = super::loot_cmd::resource_or_inline(args, "feature")?;
            let around = (
                ChunkPos::new(chunk.x.saturating_sub(1), chunk.z.saturating_sub(1)),
                ChunkPos::new(chunk.x.saturating_add(1), chunk.z.saturating_add(1)),
            );
            check(env, around.0, around.1)?;
            let failed = || CommandError::new("commands.place.feature.failed", vec![]);
            let (packs, generator) = generation(env).ok_or_else(failed)?;
            let definition = match (&id, inline) {
                (Some(id), _) => serde_json::Value::String(id.clone()),
                (None, Some(json)) => json,
                (None, None) => return Err(missing("feature")),
            };
            let feature =
                wgp::CommandFeature::load(&generator, &packs, &definition).map_err(|e| {
                    CommandError::new(
                        "argument.resource_or_id.failed_to_parse",
                        vec![ErrArg::Str(e.to_string())],
                    )
                })?;
            let mut random = placement_random(env);
            let mut area = PlaceArea::copy(env.level, &generator, around.0, around.1);
            let placed = area.run(&generator, |region| feature.place(region, &mut random, pos));
            area.apply(env.level);
            if !placed {
                return Err(failed());
            }
            let message = match id {
                Some(id) => {
                    let mut a = vec![Arg::String(id)];
                    a.extend(coords());
                    Component::translatable_args("commands.place.feature.success", a)
                }
                None => {
                    Component::translatable_args("commands.place.feature.success.inline", coords())
                }
            };
            source.send_success(env.global, message, true);
            Ok(1)
        }
        PlaceOp::Jigsaw => {
            let pool = args.identifier("pool").ok_or_else(|| missing("pool"))?;
            let target = args.identifier("target").ok_or_else(|| missing("target"))?;
            let max_depth = args.int("max_depth").copied().unwrap_or(1);
            let failed = || CommandError::new("commands.place.jigsaw.failed", vec![]);
            let (packs, generator) = generation(env).ok_or_else(failed)?;
            if !wgp::has_entry(&packs, "minecraft:worldgen/template_pool", pool) {
                return Err(CommandError::new(
                    "commands.place.jigsaw.invalid",
                    vec![ErrArg::Str(pool.clone())],
                ));
            }
            check(env, chunk, chunk)?;
            let pieces =
                wgp::jigsaw_pieces(&generator, &packs, pool, target, max_depth, pos, height)
                    .map_err(|e| {
                        log::warn!("place jigsaw {pool}: {e}");
                        failed()
                    })?;
            let Some(mut pieces) = pieces else {
                return Err(failed());
            };
            let (min, max) = pieces_chunks(&pieces).unwrap_or((chunk, chunk));
            let mut random = placement_random(env);
            let mut area = PlaceArea::copy(env.level, &generator, min, max);
            area.run(&generator, |region| {
                wgp::place_jigsaw(&generator, &mut pieces, region, &mut random, pos, false);
            });
            area.apply(env.level);
            source.send_success(
                env.global,
                Component::translatable_args("commands.place.jigsaw.success", coords()),
                true,
            );
            Ok(1)
        }
        PlaceOp::Structure => {
            let id = args
                .identifier("structure")
                .ok_or_else(|| missing("structure"))?;
            let invalid = || {
                CommandError::new(
                    "commands.place.structure.invalid",
                    vec![ErrArg::Str(id.clone())],
                )
            };
            let (_, generator) = generation(env).ok_or_else(invalid)?;
            let structure = generator
                .structures()
                .structure_id(id)
                .ok_or_else(invalid)?;
            let start = wgp::structure_start(&generator, structure, chunk, height)
                .ok_or_else(|| CommandError::new("commands.place.structure.failed", vec![]))?;
            let bb = start.bounding_box();
            let min = ChunkPos::new(bb.min_x() >> 4, bb.min_z() >> 4);
            let max = ChunkPos::new(bb.max_x() >> 4, bb.max_z() >> 4);
            check(env, min, max)?;
            let mut random = placement_random(env);
            let mut area = PlaceArea::copy(env.level, &generator, min, max);
            area.run(&generator, |region| {
                wgp::place_structure(&generator, &start, region, &mut random, (min, max));
            });
            area.apply(env.level);
            let mut a = vec![Arg::String(id.clone())];
            a.extend(coords());
            source.send_success(
                env.global,
                Component::translatable_args("commands.place.structure.success", a),
                true,
            );
            Ok(1)
        }
        PlaceOp::Template => place_template(args, source, env, pos, false),
        PlaceOp::TemplateStrict => place_template(args, source, env, pos, true),
    }
}

/// The structure search and biome tags of exploration maps
/// ([`vanillite_game::map::Explorer`]), with the registry tags of the loaded data packs.
/// Starts are found from the terrain generator; the references maps add are kept in the
/// world's [`StructureReferences`](vanillite_game::level::structure_refs::StructureReferences).
pub(crate) struct MapExplorer {
    pub(crate) tags: std::sync::Arc<crate::systems::RegistryTags>,
}

/// `ExplorationMapFunction.claimStructureAt`: references the start of one of `wanted` whose
/// box holds `pos` (`StructureManager.getStructureAt`: the starts within reach, 8 chunks),
/// if it can be referenced.
fn claim_structure_at(
    terrain: &TerrainGenerator,
    wanted: &[StructureId],
    pos: BlockPos,
    level: LevelHeight,
    refs: &vanillite_game::level::structure_refs::StructureReferences,
) {
    use vanillite_worldgen::structure::state::StructureState;
    let state = StructureState::new(terrain);
    let registry = terrain.structures();
    let sets: Vec<_> = state
        .possible_structure_sets()
        .iter()
        .copied()
        .filter(|&id| {
            registry
                .set(id)
                .structures
                .iter()
                .any(|(s, _)| wanted.contains(s))
        })
        .collect();
    let center = ChunkPos::new(pos.x >> 4, pos.z >> 4);
    for dx in -8..=8 {
        for dz in -8..=8 {
            let chunk = ChunkPos::new(center.x + dx, center.z + dz);
            for &set in &sets {
                let Some(id) = terrain.structure_start_in(&state, set, chunk, level) else {
                    continue;
                };
                if !wanted.contains(&id) {
                    continue;
                }
                let inside = vanillite_worldgen::place::structure_start(terrain, id, chunk, level)
                    .is_some_and(|start| start.bounding_box().is_inside(pos));
                if inside {
                    refs.try_add_reference(chunk, &registry.structure(id).name);
                    return;
                }
            }
        }
    }
}

impl vanillite_game::map::Explorer for MapExplorer {
    fn nearest_structure(
        &self,
        terrain: &TerrainGenerator,
        tag: &str,
        origin: BlockPos,
        radius: i32,
        min_y: i32,
        height: i32,
        known: Option<&vanillite_game::level::structure_refs::StructureReferences>,
    ) -> Option<BlockPos> {
        let names = self.tags.get("minecraft:worldgen/structure")?.get(tag)?;
        let wanted: Vec<StructureId> = names
            .iter()
            .filter_map(|n| terrain.structures().structure_id(n))
            .collect();
        if wanted.is_empty() {
            return None;
        }
        let level = LevelHeight { min_y, height };
        let Some(refs) = known else {
            return nearest_structure(terrain, &wanted, origin, level, radius).map(|(pos, _)| pos);
        };
        claim_structure_at(terrain, &wanted, origin, level, refs);
        let registry = terrain.structures();
        vanillite_game::level::locate::nearest_structure_claiming(
            terrain,
            &wanted,
            origin,
            level,
            radius,
            &mut |chunk, id| refs.try_add_reference(chunk, &registry.structure(id).name),
        )
        .map(|(pos, _)| pos)
    }

    fn water_on_map_outlines<'a>(
        &'a self,
        terrain: &'a TerrainGenerator,
    ) -> Box<dyn FnMut(BlockPos) -> bool + 'a> {
        let watery = self
            .tags
            .get("minecraft:worldgen/biome")
            .and_then(|t| t.get("minecraft:water_on_map_outlines"));
        let mut resolver = terrain
            .biome_source()
            .create_caching_resolver(terrain.program(), terrain.climate());
        Box::new(move |pos: BlockPos| {
            let biome = resolver.noise_biome(pos.x >> 2, pos.y >> 2, pos.z >> 2);
            let name = &terrain.biomes().get(biome).name;
            watery.is_some_and(|w| w.iter().any(|n| n == name))
        })
    }
}

/// The chunks (min and max corner) the pieces' boxes cover.
fn pieces_chunks(
    pieces: &[Box<dyn vanillite_worldgen::structure::piece::StructurePiece>],
) -> Option<(ChunkPos, ChunkPos)> {
    let mut boxes = pieces.iter().map(|p| *p.bounding_box());
    let first = boxes.next()?;
    let (mut min, mut max) = (
        (first.min_x(), first.min_z()),
        (first.max_x(), first.max_z()),
    );
    for b in boxes {
        min = (min.0.min(b.min_x()), min.1.min(b.min_z()));
        max = (max.0.max(b.max_x()), max.1.max(b.max_z()));
    }
    Some((
        ChunkPos::new(min.0 >> 4, min.1 >> 4),
        ChunkPos::new(max.0 >> 4, max.1 >> 4),
    ))
}

/// `PlaceCommand.placeTemplate`.
fn place_template(
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
    pos: BlockPos,
    strict: bool,
) -> Result<i32, CommandError> {
    use vanillite_game::level::place::PlaceArea;
    use vanillite_worldgen::template::{
        Mirror, Rotation, StructurePlaceSettings, StructureProcessor,
    };
    let id = args
        .identifier("template")
        .ok_or_else(|| missing("template"))?;
    let invalid = || {
        CommandError::new(
            "commands.place.template.invalid",
            vec![ErrArg::Str(id.clone())],
        )
    };
    // Without a generator (a world of fixed chunks) the template goes into the loaded chunks.
    let generator = generation(env).map(|(_, g)| g);
    // `getStructureTemplateManager().get(id)`: the world's `generated/` templates first.
    let template = env.level.structure_template(id).ok_or_else(invalid)?;
    let rotation = args
        .string("rotation")
        .and_then(|r| Rotation::from_name(r).ok())
        .unwrap_or_default();
    let mirror = args
        .string("mirror")
        .and_then(|m| Mirror::from_name(m).ok())
        .unwrap_or_default();
    let integrity = args.float("integrity").copied().unwrap_or(1.0);
    let seed = args.int("seed").copied().unwrap_or(0);
    let size = template.size();
    let end = BlockPos::new(
        pos.x.saturating_add(size.x),
        pos.y.saturating_add(size.y),
        pos.z.saturating_add(size.z),
    );
    let (min, max) = (pos.chunk(), end.chunk());
    let loaded = (min.x..=max.x)
        .all(|x| (min.z..=max.z).all(|z| env.level.chunks.chunk(ChunkPos::new(x, z)).is_some()));
    if !loaded {
        return Err(not_loaded());
    }
    let mut settings = StructurePlaceSettings::new()
        .with_mirror(mirror)
        .with_rotation(rotation);
    settings.known_shape = strict;
    let rot = [StructureProcessor::BlockRot {
        rottable_blocks: None,
        integrity,
    }];
    if integrity < 1.0 {
        settings.clear_processors();
        settings.add_processors(&rot);
        settings = settings.with_placement_random();
    }
    // `StructureBlockEntity.createRandom(seed)`: the time for seed 0.
    let seed = if seed == 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
    } else {
        i64::from(seed)
    };
    let mut random = vanillite_base::random::LegacyRandom::new(seed);
    let flags = 2 | if strict { 816 } else { 0 };
    // The rotated and mirrored template and its neighbour updates may reach past
    // `pos..pos + size` (Vanilla places into the level itself).
    let reach = |v: i32| v.saturating_add(1);
    let (min, max) = (
        BlockPos::new(
            pos.x.saturating_sub(reach(size.x)),
            pos.y,
            pos.z.saturating_sub(reach(size.z)),
        )
        .chunk(),
        BlockPos::new(
            pos.x.saturating_add(reach(size.x)),
            pos.y,
            pos.z.saturating_add(reach(size.z)),
        )
        .chunk(),
    );
    let (area, placed) = match &generator {
        Some(generator) => {
            let mut area = PlaceArea::copy(env.level, generator, min, max);
            let placed = area.run(generator, |region| {
                template.place_in_world(region, pos, pos, &settings, &mut random, flags)
            });
            (area, placed)
        }
        None => {
            let mut area = PlaceArea::copy_plain(env.level, min, max);
            let placed = area.run_plain(env.level.seed(), |region| {
                template.place_in_world(region, pos, pos, &settings, &mut random, flags)
            });
            (area, placed)
        }
    };
    area.apply(env.level);
    if !placed {
        return Err(CommandError::new("commands.place.template.failed", vec![]));
    }
    let mut a = vec![Arg::from(Component::translatable(id.clone(), vec![]))];
    a.extend([Arg::Int(pos.x), Arg::Int(pos.y), Arg::Int(pos.z)]);
    source.send_success(
        env.global,
        Component::translatable_args("commands.place.template.success", a),
        true,
    );
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::{out_from_origin, spiral};

    #[test]
    fn samples_out_from_origin_like_vanilla() {
        assert_eq!(out_from_origin(5, 0, 10, 2), vec![5, 7, 3, 9, 1]);
        assert_eq!(out_from_origin(0, 0, 4, 1), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn spirals_from_the_centre() {
        let mut seen = Vec::new();
        let none: Option<()> = spiral(1, |x, z| {
            seen.push((x, z));
            None
        });
        assert!(none.is_none());
        assert_eq!(seen.first(), Some(&(0, 0)));
        assert_eq!(seen.len(), 9);
    }
}
