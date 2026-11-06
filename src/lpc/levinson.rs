//! Levinson-Durbin recursion and order selection (`lpc.c`).
//!
//! **Bit-exactness:** the recursion runs entirely in `f64` (reflection coeff
//! `r /= err`, the symmetric coefficient update, `err *= 1 - r*r`), with the
//! early `if err == 0.0` exit (SF bug #234). Only the saved predictor
//! coefficients are narrowed to `f32` (`(FLAC__real)(-lpc[j])`). Order selection
//! uses `log` (natural, `f64`) and `M_LN2`; built under glibc so Rust's `f64::ln`
//! resolves to the same libm `log`.

use super::MAX_LPC_ORDER;

/// Per-order LP coefficients and error from `FLAC__lpc_compute_lp_coefficients`
/// (`lpc.c:176`). `coeff` is the flat `[MAX_LPC_ORDER][MAX_LPC_ORDER]` array the C
/// fills: row `order-1` holds the `order` coefficients for that order.
pub struct LpCoefficients {
    coeff: Vec<f32>,
    /// `error[order-1]` is the modeled residual variance × sample count.
    pub error: [f64; MAX_LPC_ORDER],
    /// Possibly reduced below the requested max if the error hit exactly 0.
    pub max_order: usize,
}

impl LpCoefficients {
    /// The `order` predictor coefficients for the given order (`1..=max_order`).
    pub fn row(&self, order: usize) -> &[f32] {
        let base = (order - 1) * MAX_LPC_ORDER;
        &self.coeff[base..base + order]
    }
}

/// Levinson-Durbin (`FLAC__lpc_compute_lp_coefficients`, `lpc.c:176`). Computes LP
/// coefficients for orders `1..=max_order` from the autocorrelation. The caller
/// must ensure `autoc[0] != 0.0` (the C asserts it; `apply_apodization_` checks it
/// and bails otherwise). `autoc` must hold at least `max_order + 1` values.
pub fn compute_lp_coefficients(autoc: &[f64], max_order: usize) -> LpCoefficients {
    debug_assert!(max_order > 0 && max_order <= MAX_LPC_ORDER);
    let mut coeff = vec![0f32; MAX_LPC_ORDER * MAX_LPC_ORDER];
    let mut error = [0f64; MAX_LPC_ORDER];
    let mut lpc = [0f64; MAX_LPC_ORDER];
    let mut err = autoc[0];
    let mut out_max_order = max_order;

    let mut i = 0;
    while i < max_order {
        // Sum up this iteration's reflection coefficient.
        let mut r = -autoc[i + 1];
        for j in 0..i {
            r -= lpc[j] * autoc[i - j];
        }
        r /= err;

        // Update LPC coefficients and total error.
        lpc[i] = r;
        let mut j = 0;
        while j < i >> 1 {
            let tmp = lpc[j];
            lpc[j] += r * lpc[i - 1 - j];
            lpc[i - 1 - j] += r * tmp;
            j += 1;
        }
        if i & 1 != 0 {
            lpc[j] += lpc[j] * r;
        }

        err *= 1.0 - r * r;

        // Save this order (negate FIR coeff to get the predictor coeff).
        for j in 0..=i {
            coeff[i * MAX_LPC_ORDER + j] = (-lpc[j]) as f32;
        }
        error[i] = err;

        // See SF bug https://sourceforge.net/p/flac/bugs/234/
        if err == 0.0 {
            out_max_order = i + 1;
            break;
        }
        i += 1;
    }

    LpCoefficients {
        coeff,
        error,
        max_order: out_max_order,
    }
}

/// Expected bits per residual sample for a given error scale
/// (`FLAC__lpc_compute_expected_bits_per_residual_sample_with_error_scale`,
/// `lpc.c:1591`). `f64` natural log; negative error returns a large sentinel.
pub fn expected_bits_with_error_scale(lpc_error: f64, error_scale: f64) -> f64 {
    if lpc_error > 0.0 {
        let bps = 0.5 * (error_scale * lpc_error).ln() / std::f64::consts::LN_2;
        if bps >= 0.0 { bps } else { 0.0 }
    } else if lpc_error < 0.0 {
        // error should not be negative but can happen due to fp resolution
        1e32
    } else {
        0.0
    }
}

/// Expected bits per residual sample (`...expected_bits_per_residual_sample`,
/// `lpc.c:1580`): `error_scale = 0.5 / total_samples`. Used for the per-order
/// "don't even try" short-circuit.
pub fn expected_bits(lpc_error: f64, total_samples: u32) -> f64 {
    let error_scale = 0.5 / total_samples as f64;
    expected_bits_with_error_scale(lpc_error, error_scale)
}

/// Pick the order in `1..=max_order` minimizing the estimated total subframe bits
/// (`FLAC__lpc_compute_best_order`, `lpc.c:1608`). `best_bits` starts at
/// `(uint32_t)-1` cast to `double`; ties keep the lower order (strict `<`).
pub fn compute_best_order(
    error: &[f64],
    max_order: usize,
    total_samples: u32,
    overhead_bits_per_order: u32,
) -> usize {
    let error_scale = 0.5 / total_samples as f64;
    let mut best_index = 0usize;
    let mut best_bits = u32::MAX as f64;
    for (indx, &err) in error.iter().enumerate().take(max_order) {
        let order = (indx + 1) as u32;
        let bits = expected_bits_with_error_scale(err, error_scale)
            * (total_samples - order) as f64
            + (order * overhead_bits_per_order) as f64;
        if bits < best_bits {
            best_index = indx;
            best_bits = bits;
        }
    }
    best_index + 1
}
