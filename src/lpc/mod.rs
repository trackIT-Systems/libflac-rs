//! The LPC float pipeline (`lpc.c`): windowing → autocorrelation → Levinson →
//! quantization → residual. This is the F2 float-parity gate; every stage must
//! reproduce the C reference's exact float evaluation, so the code is a faithful
//! transcription rather than idiomatic Rust where bytes are at stake.
//!
//! `FLAC__real` is `f32`; the windowed signal is `f32`, autocorrelation and
//! Levinson accumulate in `f64`. See the module doc-comments for the specific
//! `*.c` lines and the bit-exactness hazards each stage carries.

mod autocorr;
mod levinson;
mod quantize;
mod residual;

pub use autocorr::compute_autocorrelation;
pub use levinson::{LpCoefficients, compute_best_order, compute_lp_coefficients, expected_bits};
pub use quantize::quantize_coefficients;
pub use residual::{
    compute_residual, compute_residual_limit, max_prediction_before_shift_bps, max_residual_bps,
};

/// Maximum LPC order (`FLAC__MAX_LPC_ORDER`); the Levinson coefficient rows and
/// the predictor history are sized to this.
pub const MAX_LPC_ORDER: usize = 32;

/// Apply a window to integer signal data (`FLAC__lpc_window_data[_wide]`,
/// `lpc.c:68`): `out[i] = in[i] * window[i]`, the integer promoted to `f32` and
/// multiplied in `f32`. `in` is `i64` (the unified channel type — the 33-bit side
/// loses precision in the `f32` cast exactly as the C's `_wide` variant does; for
/// ≤24-bit values the cast is identical to the narrow path). `in`/`window`/`out`
/// are all at least `data_len` long.
pub fn window_data(input: &[i64], window: &[f32], out: &mut [f32], data_len: usize) {
    for i in 0..data_len {
        out[i] = input[i] as f32 * window[i];
    }
}

/// Window a contiguous sub-block (`FLAC__lpc_window_data_partial`, `lpc.c:82`):
/// the rising half of `window` is applied to `in[data_shift..]` and the falling
/// half to the tail, forming a smaller Tukey-like window over `2*part_size`
/// samples. Faithful transcription including the `flac_min` clamp and the single
/// trailing zero; only `out[0..2*part_size)` is subsequently read by the
/// autocorrelation, but the exact indexing is preserved.
pub fn window_data_partial(
    input: &[i64],
    window: &[f32],
    out: &mut [f32],
    data_len: usize,
    part_size: usize,
    data_shift: usize,
) {
    if part_size + data_shift < data_len {
        let mut i = 0usize;
        while i < part_size {
            out[i] = input[data_shift + i] as f32 * window[i];
            i += 1;
        }
        i = i.min(data_len - part_size - data_shift);
        let mut j = data_len - part_size;
        while j < data_len {
            out[i] = input[data_shift + i] as f32 * window[j];
            i += 1;
            j += 1;
        }
        if i < data_len {
            out[i] = 0.0f32;
        }
    }
}
