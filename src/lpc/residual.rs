//! LPC residual from quantized coefficients
//! (`FLAC__lpc_compute_residual_from_qlp_coefficients*`, `lpc.c:321`).
//!
//! The prediction sum is accumulated in `i64` and the residual formed as
//! `data[i] - (sum >> shift)`. libFLAC has several variants (32-bit accumulator,
//! 64-bit, and an overflow-checking `limit_residual`) and `evaluate_lpc_subframe_`
//! selects among them by the predictor/residual bit bounds; whenever a 32-bit
//! accumulator is chosen the encoder has already proven it cannot overflow, so the
//! `i64` accumulation reproduces the chosen variant's output exactly. The bound
//! check and the overflow *bail* live in the subframe evaluator (it is what
//! distinguishes "use this residual" from "can't LPC at this order").

use crate::bitmath::silog2;

/// Compute `blocksize - order` residuals for the given quantized predictor.
/// `signal` is the full subframe signal (warmup included); `shift` is the
/// quantization level (`>= 0`).
pub fn compute_residual(signal: &[i64], order: usize, qlp_coeff: &[i32], shift: i32) -> Vec<i32> {
    let data_len = signal.len() - order;
    let mut residual = vec![0i32; data_len];
    for i in 0..data_len {
        let mut sum = 0i64;
        for j in 0..order {
            sum += qlp_coeff[j] as i64 * signal[order + i - 1 - j];
        }
        residual[i] = (signal[order + i] - (sum >> shift)) as i32;
    }
    residual
}

/// Overflow-checked residual (`..._limit_residual`, `lpc.c:832`). Returns `None`
/// if any residual would be `<= INT32_MIN` or `> INT32_MAX` (the C bails to "can't
/// LPC at this order"); the encoder selects this path when `max_residual_bps > 32`.
pub fn compute_residual_limit(
    signal: &[i64],
    order: usize,
    qlp_coeff: &[i32],
    shift: i32,
) -> Option<Vec<i32>> {
    let data_len = signal.len() - order;
    let mut residual = vec![0i32; data_len];
    for i in 0..data_len {
        let mut sum = 0i64;
        for j in 0..order {
            sum += qlp_coeff[j] as i64 * signal[order + i - 1 - j];
        }
        let r = signal[order + i] - (sum >> shift);
        if r <= i32::MIN as i64 || r > i32::MAX as i64 {
            return None;
        }
        residual[i] = r as i32;
    }
    Some(residual)
}

/// Largest possible prediction magnitude before the shift
/// (`FLAC__lpc_max_prediction_value_before_shift`, `lpc.c:942`):
/// `2^(subframe_bps-1) · Σ|qlp_coeff|`, the coefficient sum in `u32` as in the C.
fn max_prediction_value_before_shift(subframe_bps: u32, qlp_coeff: &[i32], order: usize) -> u64 {
    let abs_sum = qlp_coeff[..order]
        .iter()
        .fold(0u32, |acc, &c| acc.wrapping_add(c.unsigned_abs()));
    (1u64 << (subframe_bps - 1)) * u64::from(abs_sum)
}

/// Max bits in a residual sample (`FLAC__lpc_max_residual_bps`, `lpc.c:962`): the
/// largest sample magnitude plus the largest prediction after the shift, rounded
/// up (`-((-v) >> shift)`). When this exceeds 32 the encoder must use the
/// overflow-checked residual path.
pub fn max_residual_bps(
    subframe_bps: u32,
    qlp_coeff: &[i32],
    order: usize,
    lp_quantization: i32,
) -> u32 {
    let max_abs_sample_value = 1u64 << (subframe_bps - 1);
    let prediction = max_prediction_value_before_shift(subframe_bps, qlp_coeff, order) as i64;
    let after_shift = (-((-prediction) >> lp_quantization)) as u64;
    silog2((max_abs_sample_value + after_shift) as i64)
}
