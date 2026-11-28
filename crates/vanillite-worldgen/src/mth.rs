//! Java-exact `float` maths for world generation (the `float` overloads of Vanilla's `Mth` and
//! `java.lang.Math`). The `double` helpers live in `vanillite_base::math`.
//!
//! Keep Vanilla's operation order, never use `mul_add`, and convert with the helpers below.

use std::cell::{Cell, RefCell};

/// Java `Math.min(float, float)`: NaN if either is NaN, and `-0.0 < 0.0`.
#[inline]
pub fn min_f(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && a.is_sign_negative() {
        return a;
    }
    if a <= b { a } else { b }
}

/// Java `Math.max(float, float)`: NaN if either is NaN, and `0.0 > -0.0`.
#[inline]
pub fn max_f(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && a.is_sign_positive() {
        return a;
    }
    if a >= b { a } else { b }
}

/// `Mth.clamp(float, float, float)`: `value < min ? min : Math.min(value, max)`.
#[inline]
pub fn clamp_f(value: f32, min: f32, max: f32) -> f32 {
    if value < min { min } else { min_f(value, max) }
}

/// `Mth.lerp(float, float, float)`: `p0 + alpha * (p1 - p0)`.
#[inline]
pub fn lerp_f(alpha: f32, p0: f32, p1: f32) -> f32 {
    p0 + alpha * (p1 - p0)
}

/// `Mth.lerp2(float…)`.
#[inline]
pub fn lerp2_f(a1: f32, a2: f32, x00: f32, x10: f32, x01: f32, x11: f32) -> f32 {
    lerp_f(a2, lerp_f(a1, x00, x10), lerp_f(a1, x01, x11))
}

/// `Mth.lerp3(float…)`.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn lerp3_f(
    a1: f32,
    a2: f32,
    a3: f32,
    x000: f32,
    x100: f32,
    x010: f32,
    x110: f32,
    x001: f32,
    x101: f32,
    x011: f32,
    x111: f32,
) -> f32 {
    lerp_f(
        a3,
        lerp2_f(a1, a2, x000, x100, x010, x110),
        lerp2_f(a1, a2, x001, x101, x011, x111),
    )
}

/// `Mth.square(float)`.
#[inline]
pub fn square_f(x: f32) -> f32 {
    x * x
}

/// `Mth.cube(float)`.
#[inline]
pub fn cube_f(x: f32) -> f32 {
    x * x * x
}

/// `Mth.sqrt(float)`: `(float) Math.sqrt(x)`.
#[inline]
pub fn sqrt_f(x: f32) -> f32 {
    f64::from(x).sqrt() as f32
}

/// `Mth.floor(float)`: `(int) Math.floor(v)`.
#[inline]
pub fn floor_f(v: f32) -> i32 {
    f64::from(v).floor() as i32
}

/// `Mth.length(float, float)`: `(float) Math.sqrt(lengthSquared)`.
#[inline]
pub fn length2_f(x: f32, z: f32) -> f32 {
    sqrt_f(x * x + z * z)
}

/// `Mth.length(float, float, float)`.
#[inline]
pub fn length3_f(x: f32, y: f32, z: f32) -> f32 {
    sqrt_f(x * x + y * y + z * z)
}

/// Java `Math.signum(float)`.
#[inline]
pub fn signum_f(x: f32) -> f32 {
    if x == 0.0 || x.is_nan() {
        x
    } else {
        1.0_f32.copysign(x)
    }
}

/// Java `Math.round(float)`: the closest `int`, ties towards positive infinity; NaN is 0.
#[inline]
pub fn round_f(x: f32) -> i32 {
    if x.is_nan() {
        return 0;
    }
    // Exact for every float: floor(x + 0.5) computed without the double-rounding trap.
    let floor = x.floor();
    let diff = x - floor;
    let r = if diff >= 0.5 { floor + 1.0 } else { floor };
    r as i32
}

/// `Mth.roundToward(input, multiple)`: rounds `input` up to a multiple of `multiple`.
#[inline]
pub fn round_toward(input: i32, multiple: i32) -> i32 {
    vanillite_base::math::floor_div(input.wrapping_neg(), multiple).wrapping_neg() * multiple
}

/// A 1-ulp probe for the known sin/cos limit (#300): [`math_sin`] and [`math_cos`] add `sin`
/// and `cos` ulps (away from zero for positive values) to their results for exactly `input`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrigShift {
    pub input: f64,
    pub sin: i8,
    pub cos: i8,
}

thread_local! {
    /// The probe of this thread; `None` except in the world generation sweep (#300).
    static TRIG_SHIFT: Cell<Option<TrigShift>> = const { Cell::new(None) };
    /// Whether [`sin_cos_ulp_sensitive`] records its inputs on this thread.
    static TRIG_RECORDING: Cell<bool> = const { Cell::new(false) };
    static TRIG_RECORDED: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
}

/// Java `Math.sin(double)`.
///
/// Known limit (#300): HotSpot computes `Math.sin`/`Math.cos` with an intrinsic based on Intel's
/// LIBM, Vanillite with the platform libm (glibc). For about 0.23 % of the angles the two
/// results differ by 1 ulp. Where a world generation decision sits exactly on that ulp (seen:
/// iceberg ellipses, see [`sin_cos_ulp_sensitive`]) a feature comes out differently. HotSpot's
/// code is GPLv2 with Classpath Exception, so it is not ported; the sweep reports such areas as
/// a known limit when they match Vanilla with one result shifted by 1 ulp ([`TrigShift`]).
#[inline]
pub fn math_sin(x: f64) -> f64 {
    match TRIG_SHIFT.get() {
        Some(s) if s.input.to_bits() == x.to_bits() => shift_ulps(x.sin(), s.sin),
        _ => x.sin(),
    }
}

/// Java `Math.cos(double)`; see [`math_sin`] for the known 1-ulp limit (#300).
#[inline]
pub fn math_cos(x: f64) -> f64 {
    match TRIG_SHIFT.get() {
        Some(s) if s.input.to_bits() == x.to_bits() => shift_ulps(x.cos(), s.cos),
        _ => x.cos(),
    }
}

/// `(Math.sin(x), Math.cos(x))` for a call site whose decisions are known to sit on the
/// 1-ulp limit (#300) now and then; records `x` for the sweep's probe while recording is on.
#[inline]
pub fn sin_cos_ulp_sensitive(x: f64) -> (f64, f64) {
    if TRIG_RECORDING.get() {
        TRIG_RECORDED.with_borrow_mut(|r| {
            if r.last().is_none_or(|l| l.to_bits() != x.to_bits()) {
                r.push(x);
            }
        });
    }
    (math_sin(x), math_cos(x))
}

/// The 1-ulp probe of this thread (#300).
pub fn trig_shift() -> Option<TrigShift> {
    TRIG_SHIFT.get()
}

/// Sets the 1-ulp probe of this thread; only for the world generation sweep (#300).
pub fn set_trig_shift(shift: Option<TrigShift>) {
    TRIG_SHIFT.set(shift);
}

/// Starts (`true`) or stops recording the inputs of [`sin_cos_ulp_sensitive`] on this thread
/// and returns those recorded so far; only for the world generation sweep (#300).
pub fn record_ulp_sensitive_inputs(on: bool) -> Vec<f64> {
    TRIG_RECORDING.set(on);
    TRIG_RECORDED.take()
}

#[inline]
fn shift_ulps(value: f64, ulps: i8) -> f64 {
    if ulps == 0 || value == 0.0 || !value.is_finite() {
        return value;
    }
    f64::from_bits(value.to_bits().wrapping_add_signed(i64::from(ulps)))
}
