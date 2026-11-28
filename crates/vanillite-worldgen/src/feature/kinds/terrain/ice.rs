//! Ice and snow features: `BlueIceFeature`, `IcebergFeature`, `SpikeFeature` and
//! `SnowAndFreezeFeature` (`freeze_top_layer`).

use std::f64::consts::PI;

use serde_json::Value;
use vanillite_base::math::ceil_f;
use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Direction};
use vanillite_chunk::HeightmapKind;
use vanillite_data::block::{BlockState, ids};

use super::{Climates, is, state_field};
use crate::Error;
use crate::feature::region::{WorldGenRegion, flags};
use crate::feature::{BlockPredicate, FeatureContext, FeatureParser, FeatureType, field};
use crate::mth::{clamp_f, sin_cos_ulp_sensitive};

/// `blue_ice`: a cluster of blue ice next to packed ice below sea level.
#[derive(Debug)]
pub struct BlueIceFeature;

impl FeatureType for BlueIceFeature {
    fn parse(_json: &Value, _parser: &mut FeatureParser<'_>) -> Result<Self, Error> {
        Ok(Self)
    }

    fn place(&self, ctx: &mut FeatureContext<'_, '_>, origin: BlockPos) -> bool {
        let level = &mut *ctx.level;
        if origin.y > level.sea_level() - 1 {
            return false;
        }
        if !is(level.block(origin), ids::WATER)
            && !is(level.block(origin.relative(Direction::Down)), ids::WATER)
        {
            return false;
        }
        let found_packed_ice = Direction::ALL
            .iter()
            .any(|&d| d != Direction::Down && is(level.block(origin.relative(d)), ids::PACKED_ICE));
        if !found_packed_ice {
            return false;
        }
        let blue_ice = ids::BLUE_ICE.default_state();
        level.set_block(origin, blue_ice, flags::CLIENTS);
        for _ in 0..200 {
            let y_off = ctx.random.next_int_bounded(5) - ctx.random.next_int_bounded(6);
            let mut xz_diff = 3;
            if y_off < 2 {
                xz_diff += y_off / 2;
            }
            if xz_diff < 1 {
                continue;
            }
            let dx = ctx.random.next_int_bounded(xz_diff) - ctx.random.next_int_bounded(xz_diff);
            let dz = ctx.random.next_int_bounded(xz_diff) - ctx.random.next_int_bounded(xz_diff);
            let pos = origin.offset(dx, y_off, dz);
            let state = level.block(pos);
            if state.is_air()
                || is(state, ids::WATER)
                || is(state, ids::PACKED_ICE)
                || is(state, ids::ICE)
            {
                let next_to_blue_ice = Direction::ALL
                    .iter()
                    .any(|&d| is(level.block(pos.relative(d)), ids::BLUE_ICE));
                if next_to_blue_ice {
                    level.set_block(pos, blue_ice, flags::CLIENTS);
                }
            }
        }
        true
    }
}

/// `iceberg`: a round or elliptic iceberg at sea level.
#[derive(Debug)]
pub struct IcebergFeature {
    state: BlockState,
}

/// The shape parameters an iceberg rolls once.
struct Shape {
    angle: f64,
    ellipse_a: i32,
    ellipse_c: i32,
    is_ellipse: bool,
    snow_on_top: bool,
}

impl FeatureType for IcebergFeature {
    fn parse(json: &Value, _parser: &mut FeatureParser<'_>) -> Result<Self, Error> {
        Ok(Self {
            state: state_field(json, "state")?,
        })
    }

    fn place(&self, ctx: &mut FeatureContext<'_, '_>, origin: BlockPos) -> bool {
        let level = &mut *ctx.level;
        let random = &mut *ctx.random;
        let origin = BlockPos::new(origin.x, level.sea_level(), origin.z);
        let snow_on_top = random.next_double() > 0.7;
        let angle = random.next_double() * 2.0 * PI;
        let ellipse_a = 11 - random.next_int_bounded(5);
        let ellipse_c = 3 + random.next_int_bounded(3);
        let is_ellipse = random.next_double() > 0.7;
        let mut over_water_height = if is_ellipse {
            random.next_int_bounded(6) + 6
        } else {
            random.next_int_bounded(15) + 3
        };
        if !is_ellipse && random.next_double() > 0.9 {
            over_water_height += random.next_int_bounded(19) + 7;
        }
        let under_water_height = (over_water_height + random.next_int_bounded(11)).min(18);
        let width =
            (over_water_height + random.next_int_bounded(7) - random.next_int_bounded(5)).min(11);
        let a = if is_ellipse { ellipse_a } else { 11 };
        let shape = Shape {
            angle,
            ellipse_a,
            ellipse_c,
            is_ellipse,
            snow_on_top,
        };

        for xo in -a..a {
            for zo in -a..a {
                for y_off in 0..over_water_height {
                    let radius = if is_ellipse {
                        radius_ellipse(y_off, over_water_height, width)
                    } else {
                        radius_round(random, y_off, over_water_height, width)
                    };
                    if is_ellipse || xo < radius {
                        self.generate_block(
                            level,
                            random,
                            &shape,
                            origin,
                            over_water_height,
                            [xo, y_off, zo],
                            radius,
                            a,
                        );
                    }
                }
            }
        }

        smooth(
            level,
            origin,
            width,
            over_water_height,
            is_ellipse,
            ellipse_a,
        );

        for xo in -a..a {
            for zo in -a..a {
                let mut y_off = -1;
                while y_off > -under_water_height {
                    let new_a = if is_ellipse {
                        ceil_f(
                            a as f32
                                * (1.0
                                    - (y_off * y_off) as f32 / (under_water_height as f32 * 8.0)),
                        )
                    } else {
                        a
                    };
                    let radius = radius_steep(random, -y_off, under_water_height, width);
                    if xo < radius {
                        self.generate_block(
                            level,
                            random,
                            &shape,
                            origin,
                            under_water_height,
                            [xo, y_off, zo],
                            radius,
                            new_a,
                        );
                    }
                    y_off -= 1;
                }
            }
        }

        let do_cut_out = if is_ellipse {
            random.next_double() > 0.1
        } else {
            random.next_double() > 0.7
        };
        if do_cut_out {
            generate_cut_out(random, level, width, over_water_height, origin, &shape);
        }
        true
    }
}

impl IcebergFeature {
    /// `generateIcebergBlock`; `offset` is `[xo, yOff, zo]`.
    #[allow(clippy::too_many_arguments)]
    fn generate_block<R: RandomSource>(
        &self,
        level: &mut WorldGenRegion<'_>,
        random: &mut R,
        shape: &Shape,
        origin: BlockPos,
        height: i32,
        offset: [i32; 3],
        radius: i32,
        a: i32,
    ) {
        let [xo, y_off, zo] = offset;
        let signed_dist = if shape.is_ellipse {
            signed_distance_ellipse(
                xo,
                zo,
                (0, 0),
                a,
                ellipse_c(y_off, height, shape.ellipse_c),
                shape.angle,
            )
        } else {
            signed_distance_circle(xo, zo, radius, random)
        };
        if signed_dist < 0.0 {
            let pos = origin.offset(xo, y_off, zo);
            let compare = if shape.is_ellipse {
                -0.5
            } else {
                f64::from(-6 - random.next_int_bounded(3))
            };
            if signed_dist > compare && random.next_double() > 0.9 {
                return;
            }
            self.set_iceberg_block(level, random, shape, pos, height - y_off, height);
        }
    }

    /// `setIcebergBlock`.
    fn set_iceberg_block<R: RandomSource>(
        &self,
        level: &mut WorldGenRegion<'_>,
        random: &mut R,
        shape: &Shape,
        pos: BlockPos,
        h_diff: i32,
        height: i32,
    ) {
        let state = level.block(pos);
        if state.is_air()
            || is(state, ids::SNOW_BLOCK)
            || is(state, ids::ICE)
            || is(state, ids::WATER)
        {
            let randomness = !shape.is_ellipse || random.next_double() > 0.05;
            let divisor = if shape.is_ellipse { 3 } else { 2 };
            let snow = shape.snow_on_top
                && !is(state, ids::WATER)
                && f64::from(h_diff)
                    <= f64::from(random.next_int_bounded((height / divisor).max(1)))
                        + f64::from(height) * 0.6
                && randomness;
            let new_state = if snow {
                ids::SNOW_BLOCK.default_state()
            } else {
                self.state
            };
            level.set_block_and_update(pos, new_state);
        }
    }
}

/// `generateCutOut`.
fn generate_cut_out<R: RandomSource>(
    random: &mut R,
    level: &mut WorldGenRegion<'_>,
    width: i32,
    height: i32,
    origin: BlockPos,
    shape: &Shape,
) {
    let sign_x = if random.next_boolean() { -1 } else { 1 };
    let sign_z = if random.next_boolean() { -1 } else { 1 };
    let mut x_off = random.next_int_bounded((width / 2 - 2).max(1));
    if random.next_boolean() {
        x_off = width / 2 + 1 - random.next_int_bounded((width - width / 2 - 1).max(1));
    }
    let mut z_off = random.next_int_bounded((width / 2 - 2).max(1));
    if random.next_boolean() {
        z_off = width / 2 + 1 - random.next_int_bounded((width - width / 2 - 1).max(1));
    }
    if shape.is_ellipse {
        x_off = random.next_int_bounded((shape.ellipse_a - 5).max(1));
        z_off = x_off;
    }
    let local_origin = (sign_x * x_off, sign_z * z_off);
    let angle = if shape.is_ellipse {
        shape.angle + PI / 2.0
    } else {
        random.next_double() * 2.0 * PI
    };
    for y_off in 0..height - 3 {
        let radius = radius_round(random, y_off, height, width);
        carve(
            level,
            radius,
            y_off,
            origin,
            false,
            angle,
            local_origin,
            shape,
        );
    }
    let mut y_off = -1;
    while y_off > -height + random.next_int_bounded(5) {
        let radius = radius_steep(random, -y_off, height, width);
        carve(
            level,
            radius,
            y_off,
            origin,
            true,
            angle,
            local_origin,
            shape,
        );
        y_off -= 1;
    }
}

/// `carve`.
#[allow(clippy::too_many_arguments)]
fn carve(
    level: &mut WorldGenRegion<'_>,
    radius: i32,
    y_off: i32,
    origin: BlockPos,
    under_water: bool,
    angle: f64,
    local_origin: (i32, i32),
    shape: &Shape,
) {
    let a = radius + 1 + shape.ellipse_a / 3;
    let c = (radius - 3).min(3) + shape.ellipse_c / 2 - 1;
    for xo in -a..a {
        for zo in -a..a {
            // `signedDist < 0.0`: a NaN distance (`c` of 0 at the local origin) carves nothing.
            let signed_dist = signed_distance_ellipse(xo, zo, local_origin, a, c, angle);
            if signed_dist.is_nan() || signed_dist >= 0.0 {
                continue;
            }
            let pos = origin.offset(xo, y_off, zo);
            let state = level.block(pos);
            if is_iceberg_state(state) || is(state, ids::SNOW_BLOCK) {
                if under_water {
                    level.set_block_and_update(pos, ids::WATER.default_state());
                } else {
                    level.set_block_and_update(pos, BlockState::AIR);
                    // `removeFloatingSnowLayer`
                    let above = pos.relative(Direction::Up);
                    if is(level.block(above), ids::SNOW) {
                        level.set_block_and_update(above, BlockState::AIR);
                    }
                }
            }
        }
    }
}

/// `getEllipseC`.
fn ellipse_c(y_off: i32, height: i32, shape_ellipse_c: i32) -> i32 {
    if y_off > 0 && height - y_off <= 3 {
        shape_ellipse_c - (4 - (height - y_off))
    } else {
        shape_ellipse_c
    }
}

/// `signedDistanceCircle` around the origin.
fn signed_distance_circle<R: RandomSource>(xo: i32, zo: i32, radius: i32, random: &mut R) -> f64 {
    let off = 10.0 * clamp_f(random.next_float(), 0.2, 0.8) / radius as f32;
    let (xo, zo, radius) = (f64::from(xo), f64::from(zo), f64::from(radius));
    f64::from(off) + xo * xo + zo * zo - radius * radius
}

/// `signedDistanceEllipse`.
fn signed_distance_ellipse(
    xo: i32,
    zo: i32,
    origin: (i32, i32),
    a: i32,
    c: i32,
    angle: f64,
) -> f64 {
    let (dx, dz) = (f64::from(xo - origin.0), f64::from(zo - origin.1));
    // Known limit (#300): `Math.sin/cos` can differ from ours by 1 ulp, and with `a == c` the
    // lattice points on the circle sit on that ulp, so now and then a block flips.
    let (sin, cos) = sin_cos_ulp_sensitive(angle);
    let u = (dx * cos - dz * sin) / f64::from(a);
    let v = (dx * sin + dz * cos) / f64::from(c);
    u * u + v * v - 1.0
}

/// `heightDependentRadiusRound`.
fn radius_round<R: RandomSource>(random: &mut R, y_off: i32, height: i32, width: i32) -> i32 {
    let k = 3.5 - random.next_float();
    let mut scale = (1.0 - (y_off * y_off) as f32 / (height as f32 * k)) * width as f32;
    if height > 15 + random.next_int_bounded(5) {
        let temp_y_off = if y_off < 3 + random.next_int_bounded(6) {
            y_off / 2
        } else {
            y_off
        };
        scale = (1.0 - temp_y_off as f32 / (height as f32 * k * 0.4)) * width as f32;
    }
    ceil_f(scale / 2.0)
}

/// `heightDependentRadiusEllipse`.
fn radius_ellipse(y_off: i32, height: i32, width: i32) -> i32 {
    let scale = (1.0 - (y_off * y_off) as f32 / (height as f32 * 1.0)) * width as f32;
    ceil_f(scale / 2.0)
}

/// `heightDependentRadiusSteep`.
fn radius_steep<R: RandomSource>(random: &mut R, y_off: i32, height: i32, width: i32) -> i32 {
    let k = 1.0 + random.next_float() / 2.0;
    let scale = (1.0 - y_off as f32 / (height as f32 * k)) * width as f32;
    ceil_f(scale / 2.0)
}

/// `isIcebergState`.
fn is_iceberg_state(state: BlockState) -> bool {
    is(state, ids::PACKED_ICE) || is(state, ids::SNOW_BLOCK) || is(state, ids::BLUE_ICE)
}

/// `smooth`: removes floating and lonely iceberg blocks.
fn smooth(
    level: &mut WorldGenRegion<'_>,
    origin: BlockPos,
    width: i32,
    height: i32,
    is_ellipse: bool,
    ellipse_a: i32,
) {
    let a = if is_ellipse { ellipse_a } else { width / 2 };
    for x in -a..=a {
        for z in -a..=a {
            for y_off in 0..=height {
                let pos = origin.offset(x, y_off, z);
                let state = level.block(pos);
                if !is_iceberg_state(state) && !is(state, ids::SNOW) {
                    continue;
                }
                if level.block(pos.relative(Direction::Down)).is_air() {
                    level.set_block_and_update(pos, BlockState::AIR);
                    level.set_block_and_update(pos.relative(Direction::Up), BlockState::AIR);
                } else if is_iceberg_state(state) {
                    let open_sides = [
                        Direction::West,
                        Direction::East,
                        Direction::North,
                        Direction::South,
                    ]
                    .iter()
                    .filter(|&&d| !is_iceberg_state(level.block(pos.relative(d))))
                    .count();
                    if open_sides >= 3 {
                        level.set_block_and_update(pos, BlockState::AIR);
                    }
                }
            }
        }
    }
}

/// `spike`: an ice spike with a pillar below it.
#[derive(Debug)]
pub struct SpikeFeature {
    state: BlockState,
    can_place_on: BlockPredicate,
    can_replace: BlockPredicate,
}

impl FeatureType for SpikeFeature {
    fn parse(json: &Value, parser: &mut FeatureParser<'_>) -> Result<Self, Error> {
        Ok(Self {
            state: state_field(json, "state")?,
            can_place_on: parser.block_predicate(field(json, "can_place_on")?)?,
            can_replace: parser.block_predicate(field(json, "can_replace")?)?,
        })
    }

    fn place(&self, ctx: &mut FeatureContext<'_, '_>, origin: BlockPos) -> bool {
        let level = &mut *ctx.level;
        let random = &mut *ctx.random;
        let mut origin = origin;
        while level.is_empty_block(origin) && origin.y > level.min_y() + 2 {
            origin = origin.relative(Direction::Down);
        }
        if !self.can_place_on.test(level, origin) {
            return false;
        }
        origin = origin.offset(0, random.next_int_bounded(4), 0);
        let height = random.next_int_bounded(4) + 7;
        let width = height / 4 + random.next_int_bounded(2);
        if width > 1 && random.next_int_bounded(60) == 0 {
            origin = origin.offset(0, 10 + random.next_int_bounded(30), 0);
        }

        for y_off in 0..height {
            let scale = (1.0 - y_off as f32 / height as f32) * width as f32;
            let new_width = ceil_f(scale);
            for xo in -new_width..=new_width {
                let dx = xo.abs() as f32 - 0.25;
                for zo in -new_width..=new_width {
                    let dz = zo.abs() as f32 - 0.25;
                    let inside = xo == 0 && zo == 0 || dx * dx + dz * dz <= scale * scale;
                    if !inside {
                        continue;
                    }
                    let border =
                        xo == -new_width || xo == new_width || zo == -new_width || zo == new_width;
                    if border && random.next_float() > 0.75 {
                        continue;
                    }
                    let pos = origin.offset(xo, y_off, zo);
                    if level.block(pos).is_air() || self.can_replace.test(level, pos) {
                        level.set_block_and_update(pos, self.state);
                    }
                    if y_off != 0 && new_width > 1 {
                        let pos = origin.offset(xo, -y_off, zo);
                        if level.block(pos).is_air() || self.can_replace.test(level, pos) {
                            level.set_block_and_update(pos, self.state);
                        }
                    }
                }
            }
        }

        let pillar_width = (width - 1).clamp(0, 1);
        for xo in -pillar_width..=pillar_width {
            for zo in -pillar_width..=pillar_width {
                let mut cursor = origin.offset(xo, -1, zo);
                let mut run_length = 50;
                if xo.abs() == 1 && zo.abs() == 1 {
                    run_length = random.next_int_bounded(5);
                }
                while cursor.y > 50 {
                    let state = level.block(cursor);
                    if !state.is_air()
                        && !self.can_replace.test(level, cursor)
                        && state != self.state
                    {
                        break;
                    }
                    level.set_block_and_update(cursor, self.state);
                    cursor = cursor.relative(Direction::Down);
                    run_length -= 1;
                    if run_length <= 0 {
                        cursor = cursor.offset(0, -(random.next_int_bounded(5) + 1), 0);
                        run_length = random.next_int_bounded(5);
                    }
                }
            }
        }
        true
    }
}

/// `freeze_top_layer`: freezes water and places snow on the top layer of the chunk.
#[derive(Debug)]
pub struct SnowAndFreezeFeature {
    climates: Climates,
}

impl FeatureType for SnowAndFreezeFeature {
    fn parse(_json: &Value, parser: &mut FeatureParser<'_>) -> Result<Self, Error> {
        Ok(Self {
            climates: Climates::new(parser),
        })
    }

    fn place(&self, ctx: &mut FeatureContext<'_, '_>, origin: BlockPos) -> bool {
        let level = &mut *ctx.level;
        for dx in 0..16 {
            for dz in 0..16 {
                let (x, z) = (origin.x + dx, origin.z + dz);
                let y = level.height_at(HeightmapKind::MotionBlocking, x, z);
                let top = BlockPos::new(x, y, z);
                let below = top.relative(Direction::Down);
                // Both read the biome at `top`; placing blocks keeps the biomes.
                let biome = self.climates.at(level, top);
                if Climates::freezes(biome, level, below) {
                    level.set_block(below, ids::ICE.default_state(), flags::CLIENTS);
                }
                if Climates::snows(biome, level, top) {
                    level.set_block(top, ids::SNOW.default_state(), flags::CLIENTS);
                    let below_state = level.block(below);
                    if let Some(snowy) = below_state.with("snowy", "true") {
                        level.set_block(below, snowy, flags::CLIENTS);
                    }
                }
            }
        }
        true
    }
}
