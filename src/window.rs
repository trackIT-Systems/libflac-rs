//! Apodization windows, ported from `window.c`. Only the windows the level-8
//! `subdivide_tukey(3)` apodization needs are implemented here — `tukey` (which
//! falls back to `rectangle`/`hann` at the parameter extremes); the full
//! apodization set is a later generalization milestone.
//!
//! All coefficients are `f32` (`FLAC__real`). The transcendental is `cosf`: the
//! argument is formed in `f64` (`M_PI * n / Np`, matching C's `double`-precision
//! `M_PI`) and narrowed to `f32` before the cosine, exactly as the C does.
//! `f32::cos` resolves to the platform libm's `cosf`, as libFLAC's call does, so
//! the coefficients match libFLAC built on the same platform (they differ in the
//! last bit between glibc, musl and macOS; see `ORACLE.md`).

use std::f64::consts::PI;

/// `FLAC__window_rectangle` (`window.c:173`): every coefficient 1.0.
pub fn rectangle(window: &mut [f32]) {
    window.fill(1.0);
}

/// `FLAC__window_hann` (`window.c:146`): `0.5 - 0.5*cos(2*pi*n/N)`, N = L-1.
pub fn hann(window: &mut [f32]) {
    let l = window.len() as i32;
    let n_den = (l - 1) as f64;
    for (n, w) in window.iter_mut().enumerate() {
        let arg = (2.0 * PI * n as f64 / n_den) as f32;
        *w = 0.5f32 - 0.5f32 * arg.cos();
    }
}

/// `FLAC__window_tukey` (`window.c:199`): a rectangle whose two ends are replaced
/// by a Hann taper of half-width `Np`. `p` is the fraction of the window that
/// tapers; `p <= 0` degenerates to a rectangle and `p >= 1` to a full Hann (and a
/// NaN `p` defaults to 0.5, as in the C). The taper boundary `Np` and the
/// per-coefficient cosine match the C's float evaluation order exactly.
pub fn tukey(window: &mut [f32], p: f32) {
    if p <= 0.0 {
        rectangle(window);
    } else if p >= 1.0 {
        hann(window);
    } else if !(p > 0.0 && p < 1.0) {
        // p is NaN: default to 0.5, as the C does.
        tukey(window, 0.5);
    } else {
        let l = window.len() as i32;
        // Np = (int)(p/2 * L) - 1, the float product truncated toward zero.
        let np = (p / 2.0f32 * l as f32) as i32 - 1;
        rectangle(window);
        if np > 0 {
            let npf = np as f64;
            for n in 0..=np {
                let rise = (PI * n as f64 / npf) as f32;
                window[n as usize] = 0.5f32 - 0.5f32 * rise.cos();
                let fall = (PI * (n + np) as f64 / npf) as f32;
                window[(l - np - 1 + n) as usize] = 0.5f32 - 0.5f32 * fall.cos();
            }
        }
    }
}
