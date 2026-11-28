//! Four `f32` lanes whose arithmetic is four scalar `f32` operations, lane by lane, with the
//! same IEEE results (SSE2 on x86-64, which every x86-64 CPU has; plain arrays elsewhere).

// SSE and SSE2 are part of every x86-64 target; the intrinsics below rely on it.
#[cfg(all(target_arch = "x86_64", not(target_feature = "sse2")))]
compile_error!("x86-64 without SSE2");

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{__m128, _mm_add_ps, _mm_mul_ps, _mm_set_ps, _mm_set1_ps, _mm_sub_ps};

/// Whether the CPU supports AVX2 and the AVX2 kernel is not switched off (checked once, then
/// cached). `VANILLITE_NOISE_LANES=sse` forces the SSE2 path every x86-64 CPU has, so the path
/// of CPUs without AVX2 can be tested and measured on any machine (#291).
#[cfg(target_arch = "x86_64")]
#[inline]
pub(crate) fn has_avx2() -> bool {
    static AVX2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVX2.get_or_init(|| {
        std::env::var_os("VANILLITE_NOISE_LANES").is_none_or(|lanes| lanes != "sse")
            && std::arch::is_x86_feature_detected!("avx2")
    })
}

/// Four `f32` values.
#[derive(Clone, Copy, Debug)]
pub(super) struct F32x4(
    #[cfg(target_arch = "x86_64")] __m128,
    #[cfg(not(target_arch = "x86_64"))] [f32; 4],
);

#[cfg(target_arch = "x86_64")]
impl F32x4 {
    #[inline(always)]
    pub fn new(values: [f32; 4]) -> Self {
        // SAFETY: SSE is enabled on every x86-64 target (checked above).
        Self(unsafe { _mm_set_ps(values[3], values[2], values[1], values[0]) })
    }

    #[inline(always)]
    pub fn splat(value: f32) -> Self {
        // SAFETY: SSE is enabled on every x86-64 target (checked above).
        Self(unsafe { _mm_set1_ps(value) })
    }

    #[inline(always)]
    pub fn to_array(self) -> [f32; 4] {
        // SAFETY: `__m128` and `[f32; 4]` have the same size, and every bit pattern is valid
        // for both.
        unsafe { std::mem::transmute::<__m128, [f32; 4]>(self.0) }
    }

    #[inline(always)]
    pub fn add(self, other: Self) -> Self {
        // SAFETY: SSE is enabled on every x86-64 target (checked above).
        Self(unsafe { _mm_add_ps(self.0, other.0) })
    }

    #[inline(always)]
    pub fn sub(self, other: Self) -> Self {
        // SAFETY: SSE is enabled on every x86-64 target (checked above).
        Self(unsafe { _mm_sub_ps(self.0, other.0) })
    }

    #[inline(always)]
    pub fn mul(self, other: Self) -> Self {
        // SAFETY: SSE is enabled on every x86-64 target (checked above).
        Self(unsafe { _mm_mul_ps(self.0, other.0) })
    }
}

#[cfg(not(target_arch = "x86_64"))]
impl F32x4 {
    #[inline(always)]
    pub fn new(values: [f32; 4]) -> Self {
        Self(values)
    }

    #[inline(always)]
    pub fn splat(value: f32) -> Self {
        Self([value; 4])
    }

    #[inline(always)]
    pub fn to_array(self) -> [f32; 4] {
        self.0
    }

    #[inline(always)]
    pub fn add(self, other: Self) -> Self {
        Self(std::array::from_fn(|i| self.0[i] + other.0[i]))
    }

    #[inline(always)]
    pub fn sub(self, other: Self) -> Self {
        Self(std::array::from_fn(|i| self.0[i] - other.0[i]))
    }

    #[inline(always)]
    pub fn mul(self, other: Self) -> Self {
        Self(std::array::from_fn(|i| self.0[i] * other.0[i]))
    }
}

impl F32x4 {
    /// `Mth.smoothstep(float)` per lane.
    #[inline(always)]
    pub fn smoothstep(self) -> Self {
        let x = self;
        x.mul(x).mul(x).mul(
            x.mul(x.mul(Self::splat(6.0)).sub(Self::splat(15.0)))
                .add(Self::splat(10.0)),
        )
    }

    /// `Mth.lerp(alpha, p0, p1)` per lane.
    #[inline(always)]
    pub fn lerp(alpha: Self, p0: Self, p1: Self) -> Self {
        p0.add(alpha.mul(p1.sub(p0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mth::lerp_f;

    #[test]
    fn lanes_match_scalar_arithmetic() {
        let a = [0.1f32, -3.75, 1.0e-30, 7.0e20];
        let b = [0.3f32, 2.5, -1.0e-30, f32::MAX];
        let c = [0.7f32, 1.0 / 3.0, 0.5, -2.0];
        let lanes = F32x4::lerp(F32x4::new(a), F32x4::new(b), F32x4::new(c)).to_array();
        for i in 0..4 {
            assert_eq!(lanes[i].to_bits(), lerp_f(a[i], b[i], c[i]).to_bits());
        }
        let mixed = F32x4::new(a)
            .mul(F32x4::splat(3.0))
            .sub(F32x4::new(c))
            .to_array();
        for i in 0..4 {
            assert_eq!(mixed[i].to_bits(), (a[i] * 3.0 - c[i]).to_bits());
        }
    }
}
