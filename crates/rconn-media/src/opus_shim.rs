//! libopus 1.6.1 built with `no-simd` on Windows ARM64 still calls `neon_fcvtnss32` from
//! its float-to-int helper (float_cast.h keys on `__ARM_NEON`, which clang defines for every
//! AArch64 target), but the file defining it is compiled out. Provide it: AArch64 FCVTNS is
//! round-to-nearest, ties-to-even, saturating, which is exactly `round_ties_even` + `as`.
#![allow(unsafe_code)] // exporting an unmangled C symbol is `unsafe` in edition 2024

#[unsafe(no_mangle)]
pub extern "C" fn neon_fcvtnss32(x: f32) -> i32 {
    x.round_ties_even() as i32
}

#[cfg(test)]
mod tests {
    use super::neon_fcvtnss32;

    #[test]
    fn matches_fcvtns() {
        assert_eq!(neon_fcvtnss32(2.5), 2);
        assert_eq!(neon_fcvtnss32(3.5), 4);
        assert_eq!(neon_fcvtnss32(-2.5), -2);
        assert_eq!(neon_fcvtnss32(1e20), i32::MAX);
        assert_eq!(neon_fcvtnss32(f32::NAN), 0);
    }
}
