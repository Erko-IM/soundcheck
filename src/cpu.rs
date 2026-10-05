//! A second copy of the hottest code, compiled for x86-64-v3, the AVX2
//! generation of x86 processors: Intel's from Haswell in 2013 on, with
//! AVX2, FMA, BMI1, BMI2, LZCNT and MOVBE. Rust otherwise compiles x86 code
//! for processors far older, Penryn on macOS and the first x86-64 on Linux
//! and Windows, so a newer one would never use them. Each hot function
//! picks its copy at run time, the second wherever [`has_v3`] finds those
//! features.

/// Whether the processor runs the code [`v3!`] compiles.
#[cfg(target_arch = "x86_64")]
pub fn has_v3() -> bool {
    is_x86_feature_detected!("avx2")
        && is_x86_feature_detected!("bmi1")
        && is_x86_feature_detected!("bmi2")
        && is_x86_feature_detected!("fma")
        && is_x86_feature_detected!("lzcnt")
        && is_x86_feature_detected!("movbe")
        && is_x86_feature_detected!("popcnt")
}

/// Compiles a function, on x86 only, for the processors [`has_v3`] finds.
/// Nothing else can run it, so it is called only once `has_v3` says so.
macro_rules! v3 {
    ($($function:tt)*) => {
        #[cfg(target_arch = "x86_64")]
        #[target_feature(enable = "avx2,bmi1,bmi2,fma,lzcnt,movbe,popcnt")]
        $($function)*
    };
}
pub(crate) use v3;

#[cfg(test)]

mod tests {
    /// Run with SOUNDCHECK_V3 set, the tests are on a processor meant to run
    /// the second copies, as `make ci.test.mac` asks of Rosetta: finding it
    /// lacks what they need, they'd all pass without running them.
    #[test]
    fn the_second_copies_run_where_they_are_meant_to() {
        if std::env::var_os("SOUNDCHECK_V3").is_none() {
            return;
        }
        #[cfg(target_arch = "x86_64")]
        assert!(
            super::has_v3(),
            "SOUNDCHECK_V3 is set, but this processor lacks AVX2, BMI1, BMI2, FMA, LZCNT, MOVBE or POPCNT"
        );
        #[cfg(not(target_arch = "x86_64"))]
        panic!("SOUNDCHECK_V3 is set on a processor that isn't x86");
    }
}
