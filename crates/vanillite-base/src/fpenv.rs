//! The floating point environment Java's arithmetic assumes: round to nearest, no flush of
//! subnormal results or inputs to zero. A native library (for example a plugin built with
//! `-ffast-math`) can change it for the thread it runs on, which would silently change world
//! generation and light; jobs that need Java's results restore it first (#291, risk R7).

/// Restores the default floating point environment of the calling thread if anything changed
/// it, and returns the control register as it was found then (`MXCSR` on x86-64, `FPCR` on
/// aarch64), else `None`. One register read when nothing changed.
#[inline]
pub fn ensure_default() -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        // Rounding control (bits 13–14), flush to zero (15) and denormals are zero (6).
        const MODE: u32 = 0x6000 | 0x8000 | 0x0040;
        let mut csr: u32 = 0;
        // SAFETY: `stmxcsr` stores the 32-bit MXCSR register into the given location (SSE,
        // which every x86-64 CPU has).
        unsafe {
            std::arch::asm!("stmxcsr [{}]", in(reg) &raw mut csr, options(nostack, preserves_flags));
        }
        if csr & MODE == 0 {
            return None;
        }
        let fixed = csr & !MODE;
        // SAFETY: `ldmxcsr` loads MXCSR from the given location; the value only clears the
        // rounding and flush bits of the current one, which is a valid MXCSR value.
        unsafe {
            std::arch::asm!("ldmxcsr [{}]", in(reg) &raw const fixed, options(nostack, preserves_flags));
        }
        Some(u64::from(csr))
    }
    #[cfg(target_arch = "aarch64")]
    {
        // Rounding mode (bits 22–23), flush to zero (24) and flush to zero for half precision (19).
        const MODE: u64 = (3 << 22) | (1 << 24) | (1 << 19);
        let fpcr: u64;
        // SAFETY: reading FPCR has no side effects.
        unsafe {
            std::arch::asm!("mrs {}, fpcr", out(reg) fpcr, options(nomem, nostack, preserves_flags));
        }
        if fpcr & MODE == 0 {
            return None;
        }
        // SAFETY: writing FPCR with only the rounding and flush bits cleared keeps it valid.
        unsafe {
            std::arch::asm!("msr fpcr, {}", in(reg) fpcr & !MODE, options(nomem, nostack, preserves_flags));
        }
        Some(fpcr)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_environment_is_kept() {
        assert_eq!(super::ensure_default(), None);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn flush_to_zero_is_cleared() {
        let ftz: u32 = 0x1F80 | 0x8000;
        // SAFETY: a valid MXCSR value (default plus flush to zero).
        unsafe {
            std::arch::asm!("ldmxcsr [{}]", in(reg) &raw const ftz, options(nostack, preserves_flags));
        }
        assert_eq!(super::ensure_default(), Some(u64::from(ftz)));
        assert_eq!(super::ensure_default(), None);
    }
}
