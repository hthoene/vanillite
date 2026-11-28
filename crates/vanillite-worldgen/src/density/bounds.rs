//! Bounds of a function over the cells of its interpolation lattice, to find the height above
//! which a chunk is provably empty without sampling the function there.
//!
//! [`Program::not_positive_from`] runs the instructions with intervals instead of values: a 3D
//! noise counts with its whole range, everything that depends on the column only (the 2D
//! terrain shape) is sampled exactly at the lattice points, and an interpolation is bounded by
//! the corners of each cell. Every rule widens its result a little, so the bounds hold for the
//! rounded `float` values the interpreter computes; an instruction without a rule counts with
//! its static range or as unbounded. A function the rules cannot bound simply proves nothing.

use std::rc::Rc;

use super::exec::{SamplerContext, gradient};
use super::function::{Axis, leaky_relu, squeeze};
use super::program::{GradientTiling, Op, OpIndex, Program};
use super::{FnId, spline::SplineProgram};
use crate::mth::{clamp_f, lerp_f, max_f, min_f};
use crate::volume::DensityVolume;

/// Lower and upper bounds, per lattice point or per cell (indexed by its lowest corner).
#[derive(Clone, Debug)]
struct Bounds {
    lo: Vec<f32>,
    hi: Vec<f32>,
}

impl Bounds {
    fn constant(len: usize, value: f32) -> Self {
        Self {
            lo: vec![value; len],
            hi: vec![value; len],
        }
    }

    fn unknown(len: usize) -> Self {
        Self {
            lo: vec![f32::NEG_INFINITY; len],
            hi: vec![f32::INFINITY; len],
        }
    }

    /// Each pair of bounds mapped by `f` (a rule that selects or orders values without
    /// rounding; rules that compute use [`Self::widened`] on the result).
    fn map(&self, f: impl Fn(f32, f32) -> (f32, f32)) -> Self {
        let mut out = Self::constant(self.lo.len(), 0.0);
        let pairs = self.lo.iter().zip(&self.hi);
        for ((&lo, &hi), (out_lo, out_hi)) in pairs.zip(out.lo.iter_mut().zip(&mut out.hi)) {
            (*out_lo, *out_hi) = sane(f(lo, hi));
        }
        out
    }

    /// Each pair of bounds of `self` and `other` combined by `f` (see [`Self::map`]).
    fn zip(&self, other: &Self, f: impl Fn((f32, f32), (f32, f32)) -> (f32, f32)) -> Self {
        let mut out = Self::constant(self.lo.len(), 0.0);
        let left = self.lo.iter().zip(&self.hi);
        let right = other.lo.iter().zip(&other.hi);
        let pairs = left.zip(right);
        for (((&a_lo, &a_hi), (&b_lo, &b_hi)), (out_lo, out_hi)) in
            pairs.zip(out.lo.iter_mut().zip(&mut out.hi))
        {
            (*out_lo, *out_hi) = sane(f((a_lo, a_hi), (b_lo, b_hi)));
        }
        out
    }

    /// The bounds moved apart a little (see [`widen`]).
    fn widened(mut self) -> Self {
        for (lo, hi) in self.lo.iter_mut().zip(&mut self.hi) {
            (*lo, *hi) = widen((*lo, *hi));
        }
        self
    }
}

/// Unordered or NaN bounds become unbounded.
#[inline]
fn sane((lo, hi): (f32, f32)) -> (f32, f32) {
    // (False for a NaN on either side.)
    if lo <= hi {
        (lo, hi)
    } else {
        (f32::NEG_INFINITY, f32::INFINITY)
    }
}

/// Bounds moved apart by a few units in the last place, so that they also hold for values
/// rounded differently than the bounds were; unordered or NaN bounds become unbounded.
#[inline]
fn widen((lo, hi): (f32, f32)) -> (f32, f32) {
    const RELATIVE: f32 = 1.0e-6;
    // (An infinite bound stays as it is; a NaN on either side fails the comparison.)
    if lo <= hi {
        (
            lo - (lo.abs() * RELATIVE + f32::MIN_POSITIVE),
            hi + (hi.abs() * RELATIVE + f32::MIN_POSITIVE),
        )
    } else {
        (f32::NEG_INFINITY, f32::INFINITY)
    }
}

/// The hull of two intervals.
#[inline]
fn hull(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    (a.0.min(b.0), a.1.max(b.1))
}

/// Whether bounds are evaluated at the lattice points or over whole cells.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Points,
    Cells,
}

struct Evaluator<'a> {
    program: &'a Program,
    scratch: &'a mut SamplerContext,
    /// The corner points of the cells.
    lattice: DensityVolume,
    beardifier_bound: bool,
    /// How many instructions that read a single noise may still be sampled exactly at the
    /// lattice points (those reading none always are).
    exact_noises: u32,
    /// Whether a range choice looks at both branches where the choice is open (else it counts
    /// with its static range there, which keeps large functions cheap).
    open_branches: bool,
    /// Bounds at the lattice points by instruction.
    points: Vec<Option<Rc<Bounds>>>,
}

impl Evaluator<'_> {
    fn len(&self) -> usize {
        self.lattice.len()
    }

    /// The static range of an instruction as bounds.
    fn static_range(&self, op: OpIndex) -> Bounds {
        let range = self.program.ranges[op as usize];
        if range.is_nai() {
            Bounds::unknown(self.len())
        } else {
            let (lo, hi) = widen((range.min(), range.max()));
            Bounds {
                lo: vec![lo; self.len()],
                hi: vec![hi; self.len()],
            }
        }
    }

    /// Bounds of `op` at the lattice points.
    fn points(&mut self, op: OpIndex) -> Rc<Bounds> {
        if let Some(bounds) = &self.points[op as usize] {
            return Rc::clone(bounds);
        }
        let mut cheap = !self.program.maskable[op as usize];
        if !cheap && self.exact_noises > 0 && self.program.noise_ops[op as usize] <= 1 {
            self.exact_noises -= 1;
            cheap = true;
        }
        let bounds = if !cheap {
            self.eval(op, Mode::Points)
        } else {
            // Cheap (no 3D noise): the exact values, in a context of their own so the
            // chunk's cache cells stay as they are.
            let mut values = vec![0.0; self.len()];
            let lattice = self.lattice;
            self.program
                .sample_volume(self.scratch, FnId(op), &mut values, &lattice);
            Bounds {
                lo: values.clone(),
                hi: values,
            }
            .map(|lo, hi| (lo, hi))
        };
        let bounds = Rc::new(bounds);
        self.points[op as usize] = Some(Rc::clone(&bounds));
        bounds
    }

    /// Bounds of `op` in `mode` (cells are not memoised: few instructions are evaluated on
    /// them, each once per reader).
    fn get(&mut self, op: OpIndex, mode: Mode) -> Rc<Bounds> {
        match mode {
            Mode::Points => self.points(op),
            Mode::Cells => Rc::new(self.eval(op, Mode::Cells)),
        }
    }

    /// The range of a Y gradient over each cell (`Mode::Cells`) for a clamped tiling, which is
    /// monotone: its values at the cell's lowest and highest block.
    fn gradient_cells(
        &self,
        axis: Axis,
        tiling: GradientTiling,
        from_coordinate: i32,
        from_value: f32,
        factor: f32,
    ) -> Option<Bounds> {
        if !matches!(tiling, GradientTiling::Clamped { .. }) {
            return None;
        }
        let l = &self.lattice;
        let at = |c: i32| gradient(tiling, from_coordinate, from_value, factor, c);
        let mut out = Bounds::unknown(self.len());
        for z in 0..l.size_z {
            for x in 0..l.size_x {
                for y in 0..l.size_y {
                    let (low, high) = match axis {
                        Axis::X => (l.block_x(x), l.block_x(x) + l.step_x - 1),
                        Axis::Y => (l.block_y(y), l.block_y(y) + l.step_y - 1),
                        Axis::Z => (l.block_z(z), l.block_z(z) + l.step_z - 1),
                    };
                    let (a, b) = (at(low), at(high));
                    let i = l.index(x, y, z);
                    (out.lo[i], out.hi[i]) = sane((a.min(b), a.max(b)));
                }
            }
        }
        Some(out)
    }

    /// Bounds of an interpolation over each cell: the hull of the cell's eight corners.
    fn interpolated_cells(&mut self, input: OpIndex) -> Bounds {
        let corners = self.points(input);
        let l = self.lattice;
        let mut out = Bounds::unknown(self.len());
        for z in 0..l.size_z - 1 {
            for x in 0..l.size_x - 1 {
                for y in 0..l.size_y - 1 {
                    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
                    for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        for dy in 0..2 {
                            let i = l.index(x + dx, y + dy, z + dz);
                            lo = lo.min(corners.lo[i]);
                            hi = hi.max(corners.hi[i]);
                        }
                    }
                    // The interpolation rounds at every step, relative to its largest
                    // corner.
                    let slack = lo.abs().max(hi.abs()) * 1.0e-5;
                    let i = l.index(x, y, z);
                    (out.lo[i], out.hi[i]) = widen((lo - slack, hi + slack));
                }
            }
        }
        out
    }

    /// Bounds of `op`: by its rule, within its static range where the compiler knows one.
    fn eval(&mut self, op: OpIndex, mode: Mode) -> Bounds {
        let range = self.program.ranges[op as usize];
        if !range.is_nai() && !self.program.positional[op as usize] {
            // Noises and constants only: the rules would find nothing tighter.
            return self.static_range(op);
        }
        let mut bounds = self.rule(op, mode);
        if !range.is_nai() && (range.min().is_finite() || range.max().is_finite()) {
            let (min, max) = widen((range.min(), range.max()));
            for (lo, hi) in bounds.lo.iter_mut().zip(&mut bounds.hi) {
                // (An empty intersection would mean a value outside the static range: keep
                // the rule's bounds then.)
                if lo.max(min) <= hi.min(max) {
                    (*lo, *hi) = (lo.max(min), hi.min(max));
                }
            }
        }
        bounds
    }

    /// Bounds of `op` by its rule.
    fn rule(&mut self, op: OpIndex, mode: Mode) -> Bounds {
        let len = self.len();
        let program = self.program;
        match program.ops[op as usize] {
            Op::Constant { value } => Bounds::constant(len, value),
            // Nothing is bound (`not_positive_from` gives up with a bound beardifier).
            Op::ContextBound { .. } if self.beardifier_bound => Bounds::unknown(len),
            Op::ContextBound { fallback, .. } => Bounds::constant(len, fallback),
            Op::Gradient {
                axis,
                tiling,
                from_coordinate,
                from_value,
                factor,
            } => match mode {
                // Not maskable: `points` samples it exactly.
                Mode::Points => self.static_range(op),
                Mode::Cells => self
                    .gradient_cells(axis, tiling, from_coordinate, from_value, factor)
                    .unwrap_or_else(|| self.static_range(op)),
            },
            Op::Interpolated {
                input,
                cell_size_xz,
                cell_size_y,
                ..
            } => {
                let l = self.lattice;
                if (cell_size_xz, cell_size_y, cell_size_xz) != (l.step_x, l.step_y, l.step_z) {
                    return self.static_range(op);
                }
                match mode {
                    // A lattice point is a cell corner: the input's value there.
                    Mode::Points => self.points(input).as_ref().clone().widened(),
                    Mode::Cells => self.interpolated_cells(input),
                }
            }
            Op::Cached { input, .. } | Op::BlendDensity { input } => {
                self.get(input, mode).as_ref().clone()
            }
            Op::Add { left, right } => {
                let (l, r) = (self.get(left, mode), self.get(right, mode));
                l.zip(&r, |a, b| (a.0 + b.0, a.1 + b.1)).widened()
            }
            Op::Sub { left, right } => {
                let (l, r) = (self.get(left, mode), self.get(right, mode));
                l.zip(&r, |a, b| (a.0 - b.1, a.1 - b.0)).widened()
            }
            Op::Mul { left, right } => {
                let (l, r) = (self.get(left, mode), self.get(right, mode));
                l.zip(&r, |a, b| {
                    let products = [a.0 * b.0, a.0 * b.1, a.1 * b.0, a.1 * b.1];
                    // `0 * inf` is NaN: `widen` then gives up.
                    products
                        .into_iter()
                        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), p| {
                            if p.is_nan() {
                                (f32::NAN, f32::NAN)
                            } else {
                                (lo.min(p), hi.max(p))
                            }
                        })
                })
                .widened()
            }
            Op::Min { left, right, .. } => {
                // The left side decides how low the minimum is; the right side (often a
                // large function of its own) counts with its static range if it has one.
                let l = self.get(left, mode);
                let r = if program.ranges[right as usize].is_nai() {
                    self.get(right, mode)
                } else {
                    Rc::new(self.static_range(right))
                };
                l.zip(&r, |a, b| (a.0.min(b.0), a.1.min(b.1)))
            }
            Op::Max { left, right, .. } => {
                let (l, r) = (self.get(left, mode), self.get(right, mode));
                l.zip(&r, |a, b| (a.0.max(b.0), a.1.max(b.1)))
            }
            Op::ConstAdd { input, value } => self
                .get(input, mode)
                .map(|lo, hi| (lo + value, hi + value))
                .widened(),
            Op::ConstSub { value, input } => self
                .get(input, mode)
                .map(|lo, hi| (value - hi, value - lo))
                .widened(),
            Op::ConstMul { input, value } => self
                .get(input, mode)
                .map(|lo, hi| {
                    if value >= 0.0 {
                        (lo * value, hi * value)
                    } else {
                        (hi * value, lo * value)
                    }
                })
                .widened(),
            Op::ConstMin { input, value } => self
                .get(input, mode)
                .map(|lo, hi| (min_f(lo, value), min_f(hi, value))),
            Op::ConstMax { input, value } => self
                .get(input, mode)
                .map(|lo, hi| (max_f(lo, value), max_f(hi, value))),
            Op::Abs { input } => self.get(input, mode).map(|lo, hi| {
                if lo >= 0.0 {
                    (lo, hi)
                } else if hi <= 0.0 {
                    (-hi, -lo)
                } else {
                    (0.0, (-lo).max(hi))
                }
            }),
            Op::Square { input } => self
                .get(input, mode)
                .map(|lo, hi| {
                    if lo >= 0.0 {
                        (lo * lo, hi * hi)
                    } else if hi <= 0.0 {
                        (hi * hi, lo * lo)
                    } else {
                        (0.0, (lo * lo).max(hi * hi))
                    }
                })
                .widened(),
            Op::Cube { input } => self
                .get(input, mode)
                .map(|lo, hi| (lo * lo * lo, hi * hi * hi))
                .widened(),
            Op::LeakyRelu {
                input,
                negative_factor,
            } if negative_factor >= 0.0 => self
                .get(input, mode)
                .map(|lo, hi| {
                    (
                        leaky_relu(negative_factor, lo),
                        leaky_relu(negative_factor, hi),
                    )
                })
                .widened(),
            Op::Negate { input } => self.get(input, mode).map(|lo, hi| (-hi, -lo)),
            Op::Squeeze { input } => self
                .get(input, mode)
                .map(|lo, hi| (squeeze(lo), squeeze(hi)))
                .widened(),
            Op::Clamp { input, min, max } => self
                .get(input, mode)
                .map(|lo, hi| (clamp_f(lo, min, max), clamp_f(hi, min, max))),
            Op::Lerp {
                alpha,
                first,
                second,
            } => {
                let alpha = self.get(alpha, mode);
                let (f, s) = (self.get(first, mode), self.get(second, mode));
                let mut out = f.zip(&s, hull).widened();
                for i in 0..len {
                    if !(alpha.lo[i] >= 0.0 && alpha.hi[i] <= 1.0) {
                        (out.lo[i], out.hi[i]) = (f32::NEG_INFINITY, f32::INFINITY);
                    }
                }
                out
            }
            Op::LerpConstFirst {
                alpha,
                first,
                second,
            } => {
                let alpha = self.get(alpha, mode);
                let second = self.get(second, mode);
                let mut out = Bounds::unknown(len);
                for i in 0..len {
                    let (a_lo, a_hi) = (alpha.lo[i], alpha.hi[i]);
                    let s = (second.lo[i], second.hi[i]);
                    let bounds = if a_lo == 0.0 && a_hi == 0.0 {
                        (first, first)
                    } else if a_lo == 1.0 && a_hi == 1.0 {
                        s
                    } else if a_lo == a_hi && a_lo > 0.0 && a_lo < 1.0 {
                        // `first + alpha * (second - first)` grows with `second`.
                        (lerp_f(a_lo, first, s.0), lerp_f(a_lo, first, s.1))
                    } else if a_lo >= 0.0 && a_hi <= 1.0 {
                        hull((first, first), s)
                    } else {
                        (f32::NEG_INFINITY, f32::INFINITY)
                    };
                    (out.lo[i], out.hi[i]) = widen(bounds);
                }
                out
            }
            Op::LerpConstSecond {
                alpha,
                first,
                second,
            } => {
                let alpha = self.get(alpha, mode);
                let first = self.get(first, mode);
                let mut out = Bounds::unknown(len);
                for i in 0..len {
                    if alpha.lo[i] >= 0.0 && alpha.hi[i] <= 1.0 {
                        (out.lo[i], out.hi[i]) =
                            widen(hull((first.lo[i], first.hi[i]), (second, second)));
                    }
                }
                out
            }
            Op::RangeChoice {
                input,
                min_inclusive,
                max_exclusive,
                when_in_range,
                when_out_of_range,
            } => {
                let input = self.get(input, mode);
                let is_in = |i: usize| input.lo[i] >= min_inclusive && input.hi[i] < max_exclusive;
                let is_out = |i: usize| input.hi[i] < min_inclusive || input.lo[i] >= max_exclusive;
                // A branch is only looked at where it is certainly chosen; where the choice
                // is open the instruction counts with its static range.
                let open = self.open_branches;
                let inside = (0..len)
                    .any(|i| is_in(i) || (open && !is_out(i)))
                    .then(|| self.get(when_in_range, mode));
                let outside = (0..len)
                    .any(|i| is_out(i) || (open && !is_in(i)))
                    .then(|| self.get(when_out_of_range, mode));
                let mut out = self.static_range(op);
                for i in 0..len {
                    let a = inside.as_ref().map(|b| (b.lo[i], b.hi[i]));
                    let b = outside.as_ref().map(|b| (b.lo[i], b.hi[i]));
                    let bounds = match (is_in(i), is_out(i), a, b) {
                        (true, _, Some(a), _) => Some(a),
                        (_, true, _, Some(b)) => Some(b),
                        (false, false, Some(a), Some(b)) if open => Some(hull(a, b)),
                        _ => None,
                    };
                    if let Some(bounds) = bounds {
                        (out.lo[i], out.hi[i]) = bounds;
                    }
                }
                out
            }
            Op::RangeChoiceConst {
                when_in_range,
                when_out_of_range,
                ..
            } => Bounds {
                lo: vec![when_in_range.min(when_out_of_range); len],
                hi: vec![when_in_range.max(when_out_of_range); len],
            },
            Op::Spline { spline } => self.spline_range(op, &program.splines[spline as usize]),
            _ => self.static_range(op),
        }
    }

    /// A spline has no rule: its static range.
    fn spline_range(&self, op: OpIndex, _spline: &SplineProgram) -> Bounds {
        self.static_range(op)
    }
}

impl Program {
    /// The lowest block Y `top >= from_y` such that the masked-cache root `f` is proven to be
    /// `<= 0` at every block of `volume` from `top` up; `top` is a whole number of cell rows
    /// above the volume's bottom, and `volume.max_y() + 1` if nothing is proven (always for
    /// volumes that are not made of whole cells, as single columns).
    ///
    /// `scratch` is a context of the caller's other than the chunk's (its contents are
    /// irrelevant); `beardifier_bound` says whether the chunk's context binds the beardifier.
    pub fn not_positive_from(
        &self,
        scratch: &mut SamplerContext,
        f: FnId,
        volume: &DensityVolume,
        from_y: i32,
        beardifier_bound: bool,
    ) -> i32 {
        let top = volume.max_y() + 1;
        let Some((root, xz, cy)) = self.root_lattice else {
            return top;
        };
        if beardifier_bound {
            // Structures add density wherever their pieces are.
            return top;
        }
        let aligned =
            |min: i32, size: i32, cell: i32| min.rem_euclid(cell) == 0 && size % cell == 0;
        if root != f
            || (volume.step_x, volume.step_y, volume.step_z) != (1, 1, 1)
            || !aligned(volume.min_x, volume.size_x, xz)
            || !aligned(volume.min_y, volume.size_y, cy)
            || !aligned(volume.min_z, volume.size_z, xz)
        {
            return top;
        }
        // The first cell row whose blocks are all at or above `from_y`.
        let rows = volume.size_y / cy;
        let first = (i64::from(from_y) - i64::from(volume.min_y) + i64::from(cy) - 1)
            .div_euclid(i64::from(cy))
            .clamp(0, i64::from(rows)) as i32;
        if first >= rows {
            return top;
        }
        let lattice = DensityVolume::with_step(
            volume.size_x / xz + 1,
            rows - first + 1,
            volume.size_z / xz + 1,
            volume.min_x,
            volume.min_y + first * cy,
            volume.min_z,
            xz,
            cy,
            xz,
        );
        scratch.reset();
        let mut evaluator = Evaluator {
            program: self,
            scratch,
            lattice,
            beardifier_bound,
            exact_noises: 0,
            open_branches: false,
            points: vec![None; self.ops.len()],
        };
        let bounds = evaluator.eval(f.0, Mode::Cells);
        // From the top down: the rows whose cells are all proven.
        let mut proven = lattice.size_y - 1;
        'rows: while proven > 0 {
            let y = proven - 1;
            for z in 0..lattice.size_z - 1 {
                for x in 0..lattice.size_x - 1 {
                    // (A NaN bound proves nothing.)
                    let hi = bounds.hi[lattice.index(x, y, z)];
                    if hi > 0.0 || hi.is_nan() {
                        break 'rows;
                    }
                }
            }
            proven = y;
        }
        lattice.block_y(proven)
    }

    /// The smallest box of whole cells of `volume` outside of which `f` is proven to be
    /// `<= 0`, as a volume: `Some(None)` when it is proven everywhere, `None` when nothing can
    /// be proven (as [`Self::not_positive_in`], whose bounds it uses per cell).
    ///
    /// `scratch` is a context of the caller's other than the one `f` is sampled in.
    pub fn positive_box(
        &self,
        scratch: &mut SamplerContext,
        f: FnId,
        volume: &DensityVolume,
    ) -> Option<Option<DensityVolume>> {
        let (lattice, bounds) = self.vein_bounds(scratch, f, volume)?;
        let (mut lo, mut hi) = ([i32::MAX; 3], [i32::MIN; 3]);
        for z in 0..lattice.size_z - 1 {
            for x in 0..lattice.size_x - 1 {
                for y in 0..lattice.size_y - 1 {
                    // (A NaN bound proves nothing.)
                    let bound = bounds.hi[lattice.index(x, y, z)];
                    if bound > 0.0 || bound.is_nan() {
                        for (axis, v) in [x, y, z].into_iter().enumerate() {
                            lo[axis] = lo[axis].min(v);
                            hi[axis] = hi[axis].max(v);
                        }
                    }
                }
            }
        }
        if lo[0] > hi[0] {
            return Some(None);
        }
        let (min_x, min_y, min_z) = (
            lattice.block_x(lo[0]),
            lattice.block_y(lo[1]),
            lattice.block_z(lo[2]),
        );
        Some(Some(DensityVolume::new(
            lattice.block_x(hi[0] + 1) - min_x,
            lattice.block_y(hi[1] + 1) - min_y,
            lattice.block_z(hi[2] + 1) - min_z,
            min_x,
            min_y,
            min_z,
        )))
    }

    /// Whether `f` is proven to be `<= 0` at every block of `volume`. For small functions
    /// (the ore veins): the first single noise the bounds meet is sampled exactly at the
    /// corners of the interpolation cells, which costs a fraction of sampling the volume. `false` if the
    /// volume is not made of whole cells or nothing can be proven.
    ///
    /// `scratch` is a context of the caller's other than the one `f` is sampled in.
    pub fn not_positive_in(
        &self,
        scratch: &mut SamplerContext,
        f: FnId,
        volume: &DensityVolume,
    ) -> bool {
        let Some((lattice, bounds)) = self.vein_bounds(scratch, f, volume) else {
            return false;
        };
        (0..lattice.size_z - 1).all(|z| {
            (0..lattice.size_x - 1)
                .all(|x| (0..lattice.size_y - 1).all(|y| bounds.hi[lattice.index(x, y, z)] <= 0.0))
        })
    }

    /// The interpolation lattice of `volume` for `f` and the bounds of `f` per cell (by the
    /// cell's lowest corner), for the small functions of [`Self::not_positive_in`]; `None` if
    /// the volume is not made of whole cells.
    fn vein_bounds(
        &self,
        scratch: &mut SamplerContext,
        f: FnId,
        volume: &DensityVolume,
    ) -> Option<(DensityVolume, Bounds)> {
        let (xz, cy) = self.interpolation_lattice(f)?;
        let aligned =
            |min: i32, size: i32, cell: i32| min.rem_euclid(cell) == 0 && size % cell == 0;
        if (volume.step_x, volume.step_y, volume.step_z) != (1, 1, 1)
            || !aligned(volume.min_x, volume.size_x, xz)
            || !aligned(volume.min_y, volume.size_y, cy)
            || !aligned(volume.min_z, volume.size_z, xz)
        {
            return None;
        }
        let lattice = DensityVolume::with_step(
            volume.size_x / xz + 1,
            volume.size_y / cy + 1,
            volume.size_z / xz + 1,
            volume.min_x,
            volume.min_y,
            volume.min_z,
            xz,
            cy,
            xz,
        );
        scratch.reset();
        let mut evaluator = Evaluator {
            program: self,
            scratch,
            lattice,
            beardifier_bound: true,
            // The first noise met decides for most chunks (the one that says whether there
            // is a vein at all); more would cost as much as they save.
            exact_noises: 1,
            open_branches: true,
            points: vec![None; self.ops.len()],
        };
        Some((lattice, evaluator.eval(f.0, Mode::Cells)))
    }
}
