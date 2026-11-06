//! Block/frame orchestration, ported from `process_frame_` /
//! `process_subframes_` / `process_subframe_` (`stream_encoder.c`).
//! CONSTANT/VERBATIM/FIXED/LPC subframes and, for stereo, the per-frame mid-side
//! channel decision (L/R vs L/S vs R/S vs M/S by estimated bits, including the
//! `loose_mid_side` periodic-redecision mode). All compression levels 0–8 are
//! supported via [`Config`]/[`preset`] (the `tukey(0.5)` / `subdivide_tukey(2|3)`
//! apodizations and the per-level LPC-order / partition-order caps). All bit depths
//! 8/12/16/20/24/32 are supported — RICE2 entropy coding above 16 bps, and for
//! 32-bit input the 33-bit `i64` side channel plus the wide / overflow-limited
//! residual paths.

use crate::bitmath;
use crate::bitwriter::BitWriter;
use crate::format::{
    ENTROPY_CODING_METHOD_PARTITIONED_RICE_ESCAPE_PARAMETER,
    ENTROPY_CODING_METHOD_PARTITIONED_RICE2_ESCAPE_PARAMETER, MAX_FIXED_ORDER, MAX_LPC_ORDER,
    MAX_QLP_COEFF_PRECISION, MIN_QLP_COEFF_PRECISION,
};
use crate::frame::{ChannelAssignment, FrameHeader, write_frame_footer, write_frame_header};
use crate::{fixed, lpc, metadata, ogg, rice, subframe, window};

/// Minimum residual partition order (0 for every compression level).
const MIN_RESIDUAL_PARTITION_ORDER: u32 = 0;

/// The Rice parameter limit (escape value): RICE2 (31) above 16 bps, else RICE
/// (15) (`process_subframe_`, `stream_encoder.c:3471`). Based on the *stream* bps.
fn rice_parameter_limit(bits_per_sample: u32) -> u32 {
    if bits_per_sample > 16 {
        ENTROPY_CODING_METHOD_PARTITIONED_RICE2_ESCAPE_PARAMETER
    } else {
        ENTROPY_CODING_METHOD_PARTITIONED_RICE_ESCAPE_PARAMETER
    }
}

/// The apodization a compression level uses. Both variants ultimately window with
/// a Tukey window; `SubdivideTukey` additionally evaluates the per-subdivision
/// partial/punchout sub-windows via the a/b/c state machine.
#[derive(Clone, Copy)]
pub enum Apodization {
    /// A single Tukey window of the given `p` (e.g. `tukey(0.5)`).
    Tukey(f32),
    /// `subdivide_tukey(parts)`: a Tukey window of `p = 0.5/parts` plus its
    /// `parts`-deep partial/punchout subdivisions.
    SubdivideTukey(i32),
}

impl Apodization {
    /// The `p` of the underlying Tukey window.
    fn window_p(self) -> f32 {
        match self {
            Apodization::Tukey(p) => p,
            Apodization::SubdivideTukey(parts) => 0.5 / parts as f32,
        }
    }
    fn subdivide_parts(self) -> i32 {
        match self {
            Apodization::Tukey(_) => 1,
            Apodization::SubdivideTukey(parts) => parts,
        }
    }
    fn is_subdivide(self) -> bool {
        matches!(self, Apodization::SubdivideTukey(_))
    }
}

/// The compression settings of one libFLAC compression level (the fields of
/// `compression_levels_[]`, `stream_encoder.c:123`, that actually vary). The
/// constant fields — `min_residual_partition_order = 0`, `qlp_coeff_precision = 0`
/// (auto), and the escape/exhaustive/precision-search flags all `false` — are
/// implied.
#[derive(Clone, Copy)]
pub struct Config {
    pub do_mid_side: bool,
    pub loose_mid_side: bool,
    pub max_lpc_order: u32,
    pub max_residual_partition_order: u32,
    pub apodization: Apodization,
}

/// The preset for compression level `0..=8` (`compression_levels_[]`); levels
/// above 8 clamp to 8.
pub fn preset(level: u32) -> Config {
    use Apodization::{SubdivideTukey, Tukey};
    let (do_mid_side, loose_mid_side, max_lpc_order, max_residual_partition_order, apodization) =
        match level {
            0 => (false, false, 0, 3, Tukey(0.5)),
            1 => (true, true, 0, 3, Tukey(0.5)),
            2 => (true, false, 0, 3, Tukey(0.5)),
            3 => (false, false, 6, 4, Tukey(0.5)),
            4 => (true, true, 8, 4, Tukey(0.5)),
            5 => (true, false, 8, 5, Tukey(0.5)),
            6 => (true, false, 8, 6, SubdivideTukey(2)),
            7 => (true, false, 12, 6, SubdivideTukey(2)),
            _ => (true, false, 12, 6, SubdivideTukey(3)),
        };
    Config {
        do_mid_side,
        loose_mid_side,
        max_lpc_order,
        max_residual_partition_order,
        apodization,
    }
}

/// Detect and strip wasted (common trailing-zero) bits, mutating `signal` in
/// place and returning the shift (`get_wasted_bits_` / `get_wasted_bits_wide_`,
/// `stream_encoder.c:4469`/`4493`). `wide` selects the 33-bit-side variant, used
/// only for the side channel of 32-bit stereo: it differs solely in the all-zero
/// case, returning shift **1** (so a 33-bit side always fits `i32` after the
/// shift) where the narrow version returns 0.
pub(crate) fn get_wasted_bits(signal: &mut [i64], wide: bool) -> u32 {
    let mut x = 0i64;
    let mut i = 0;
    while i < signal.len() && x & 1 == 0 {
        x |= signal[i];
        i += 1;
    }
    let shift = if x == 0 {
        u32::from(wide)
    } else {
        x.trailing_zeros()
    };
    if shift > 0 {
        for s in signal.iter_mut() {
            *s >>= shift;
        }
    }
    shift
}

/// Auto QLP-coefficient precision (`stream_encoder.c:704`), derived once at init
/// from the *stream* bits-per-sample and the (full) block size; the short final
/// frame reuses the same value.
fn auto_qlp_coeff_precision(bits_per_sample: u32, blocksize: u32) -> u32 {
    if bits_per_sample < 16 {
        (2 + bits_per_sample / 2).max(MIN_QLP_COEFF_PRECISION)
    } else if bits_per_sample == 16 {
        match blocksize {
            0..=192 => 7,
            193..=384 => 8,
            385..=576 => 9,
            577..=1152 => 10,
            1153..=2304 => 11,
            2305..=4608 => 12,
            _ => 13,
        }
    } else {
        match blocksize {
            0..=384 => MAX_QLP_COEFF_PRECISION - 2,
            385..=1152 => MAX_QLP_COEFF_PRECISION - 1,
            _ => MAX_QLP_COEFF_PRECISION,
        }
    }
}

/// Settings the LPC path needs, plus the precomputed window for the current
/// frame's block size.
struct LpcCtx<'a> {
    window: &'a [f32],
    max_lpc_order: u32,
    qlp_coeff_precision: u32,
    apodization: Apodization,
}

/// One fully-evaluated LPC subframe candidate.
struct LpcChoice {
    order: u32,
    qlp_coeff: Vec<i32>,
    precision: u32,
    shift: i32,
    residual: Vec<i32>,
    rice: rice::RicePartition,
}

enum Choice {
    Constant,
    Verbatim,
    Fixed {
        order: u32,
        residual: Vec<i32>,
        rice: rice::RicePartition,
    },
    Lpc(LpcChoice),
}

/// A channel's chosen subframe plus everything needed to write it once the
/// channel assignment is decided.
struct ChosenChannel {
    signal: Vec<i64>,
    subframe_bps: u32,
    wasted_bits: u32,
    choice: Choice,
    bits: u32,
}

/// Advance the `subdivide_tukey` a/b/c state to the next sub-window
/// (`set_next_subdivide_tukey`, `stream_encoder.c:3686`). `a` = apodization index,
/// `depth` = subdivision denominator (b), `part` = interleaved partial/punchout
/// counter (c).
fn set_next_subdivide_tukey(parts: i32, a: &mut u32, depth: &mut u32, part: &mut u32) {
    if *depth == 2 {
        // Depth 2 does only partial, no (near-redundant) punchout.
        if *part == 0 {
            *part = 2;
        } else {
            *part = 0;
            *depth += 1;
        }
    } else if *part < 2 * *depth - 1 {
        *part += 1;
    } else {
        *part = 0;
        *depth += 1;
    }
    if *depth > parts as u32 {
        *a += 1;
        *depth = 1;
        *part = 0;
    }
}

/// One step of the apodization state machine (`apply_apodization_`,
/// `stream_encoder.c:3711`) for a `subdivide_tukey` apodization. Produces the LP
/// coefficients and a guess order for the current sub-window, advancing `a/b/c`,
/// or returns `None` to skip this sub-window (tiny block, or a constant signal).
#[allow(clippy::too_many_arguments)]
fn apply_apodization(
    signal: &[i64],
    window: &[f32],
    windowed: &mut [f32],
    autoc: &mut [f64],
    autoc_root: &mut [f64],
    blocksize: usize,
    max_order: usize,
    subframe_bps: u32,
    ctx: &LpcCtx,
    a: &mut u32,
    b: &mut u32,
    c: &mut u32,
) -> Option<(lpc::LpCoefficients, usize)> {
    let parts = ctx.apodization.subdivide_parts();
    let lag = max_order + 1;
    if *b == 1 {
        // Window the full block (the "root" autocorrelation).
        lpc::window_data(signal, window, windowed, blocksize);
        lpc::compute_autocorrelation(&windowed[..blocksize], lag, autoc);
        if ctx.apodization.is_subdivide() {
            autoc_root[..max_order].copy_from_slice(&autoc[..max_order]);
            *b += 1;
        } else {
            // A plain Tukey apodization is a single full-block window.
            *a += 1;
        }
    } else {
        let bb = *b as usize;
        if blocksize / bb <= MAX_LPC_ORDER as usize {
            // Windowing parts <= 32 samples is unsupported; skip and advance.
            set_next_subdivide_tukey(parts, a, b, c);
            return None;
        }
        if *c % 2 == 0 {
            // Even c: the (c/2)th partial window of size blocksize/b.
            let part_size = blocksize / bb / 2;
            let data_shift = (*c as usize / 2 * blocksize) / bb;
            lpc::window_data_partial(signal, window, windowed, blocksize, part_size, data_shift);
            lpc::compute_autocorrelation(&windowed[..blocksize / bb], lag, autoc);
        } else {
            // Odd c: the root window minus the previous partial (a punchout). Only
            // the first `max_order` lags are subtracted; autoc[max_order] is left
            // as the previous partial's value, exactly as libFLAC does.
            for i in 0..max_order {
                autoc[i] = autoc_root[i] - autoc[i];
            }
        }
        set_next_subdivide_tukey(parts, a, b, c);
    }

    if autoc[0] == 0.0 {
        // Signal is constant; can't do LP.
        return None;
    }
    let lp = lpc::compute_lp_coefficients(autoc, max_order);
    // do_qlp_coeff_prec_search is false at level 8, so the order-selection overhead
    // uses the actual qlp precision.
    let overhead = subframe_bps + ctx.qlp_coeff_precision;
    let guess_order = lpc::compute_best_order(&lp.error, lp.max_order, blocksize as u32, overhead);
    Some((lp, guess_order))
}

/// Quantize + residual + rice + estimate for one (order, coefficient row)
/// (`evaluate_lpc_subframe_`, `stream_encoder.c:3954`). Returns `None` when the
/// coefficients can't be quantized or the residual overflows i32 (the C returns 0,
/// meaning "can't LPC at this order").
#[allow(clippy::too_many_arguments)]
fn evaluate_lpc_subframe(
    signal: &[i64],
    subframe_bps: u32,
    wasted_bits: u32,
    lp_coeff_row: &[f32],
    order: u32,
    qlp_coeff_precision: u32,
    rice_parameter_limit: u32,
    min_partition_order: u32,
    max_partition_order: u32,
) -> Option<(LpcChoice, u32)> {
    let order_us = order as usize;

    // Keep qlp precision low enough that decode of <=16bps(+1 for side) needs only
    // 32-bit math.
    let mut precision = qlp_coeff_precision;
    if subframe_bps <= 17 {
        precision = precision.min(32 - subframe_bps - bitmath::ilog2(order));
    }

    let q = lpc::quantize_coefficients(lp_coeff_row, order_us, precision).ok()?;

    let residual = if lpc::max_residual_bps(subframe_bps, &q.qlp_coeff, order_us, q.shift) > 32 {
        lpc::compute_residual_limit(signal, order_us, &q.qlp_coeff, q.shift)?
    } else {
        lpc::compute_residual(signal, order_us, &q.qlp_coeff, q.shift)
    };

    let (rice, residual_bits) = rice::find_best_partition_order(
        &residual,
        order,
        rice_parameter_limit,
        min_partition_order,
        max_partition_order,
    );
    let bits = subframe::lpc_bits(order, precision, subframe_bps, wasted_bits, residual_bits);

    Some((
        LpcChoice {
            order,
            qlp_coeff: q.qlp_coeff,
            precision,
            shift: q.shift,
            residual,
            rice,
        },
        bits,
    ))
}

/// Best LPC subframe over all `subdivide_tukey` sub-windows (the LPC arm of
/// `process_subframe_`, `stream_encoder.c:3594`). Each sub-window is evaluated at
/// its single guess order (no exhaustive/precision search at level 8). Returns the
/// lowest-bit candidate, or `None` if none could be produced.
#[allow(clippy::too_many_arguments)]
fn best_lpc_subframe(
    signal: &[i64],
    subframe_bps: u32,
    wasted_bits: u32,
    blocksize: u32,
    ctx: &LpcCtx,
    rice_parameter_limit: u32,
    min_partition_order: u32,
    max_partition_order: u32,
) -> Option<(LpcChoice, u32)> {
    let max_lpc_order = if ctx.max_lpc_order >= blocksize {
        blocksize - 1
    } else {
        ctx.max_lpc_order
    };
    if max_lpc_order == 0 {
        return None;
    }

    let bs = blocksize as usize;
    let mut windowed = vec![0f32; bs];
    let mut autoc = [0f64; MAX_LPC_ORDER as usize + 1];
    let mut autoc_root = [0f64; MAX_LPC_ORDER as usize + 1];

    let mut best: Option<(LpcChoice, u32)> = None;
    // num_apodizations == 1 (a single subdivide_tukey); the state machine drives
    // `a` to 1 once the apodization is exhausted.
    let (mut a, mut b, mut c) = (0u32, 1u32, 0u32);
    while a < 1 {
        let max_order_this = max_lpc_order as usize;
        if let Some((lp, guess_order)) = apply_apodization(
            signal,
            ctx.window,
            &mut windowed,
            &mut autoc,
            &mut autoc_root,
            bs,
            max_order_this,
            subframe_bps,
            ctx,
            &mut a,
            &mut b,
            &mut c,
        ) {
            // Non-exhaustive: only the guess order is tried.
            let lpc_residual_bps =
                lpc::expected_bits(lp.error[guess_order - 1], blocksize - guess_order as u32);
            if lpc_residual_bps < subframe_bps as f64 {
                if let Some((choice, bits)) = evaluate_lpc_subframe(
                    signal,
                    subframe_bps,
                    wasted_bits,
                    lp.row(guess_order),
                    guess_order as u32,
                    ctx.qlp_coeff_precision,
                    rice_parameter_limit,
                    min_partition_order,
                    max_partition_order,
                ) {
                    if best.as_ref().is_none_or(|(_, bb)| bits < *bb) {
                        best = Some((choice, bits));
                    }
                }
            }
        }
    }
    best
}

/// Choose the smallest-estimate subframe for one (already wasted-bits-shifted)
/// channel block, returning the choice and its estimated bit cost
/// (`process_subframe_`, `stream_encoder.c:3441`). VERBATIM is the baseline;
/// CONSTANT wins for a single repeated value; otherwise FIXED and LPC compete. On
/// ties the earlier candidate is kept (strict `<`). Writing is deferred so the
/// mid-side channel decision can pick among already-evaluated subframes.
#[allow(clippy::too_many_arguments)]
fn choose_subframe(
    signal: &[i64],
    subframe_bps: u32,
    wasted_bits: u32,
    blocksize: u32,
    rice_parameter_limit: u32,
    min_partition_order: u32,
    max_partition_order: u32,
    lpc_ctx: Option<&LpcCtx>,
) -> (Choice, u32) {
    let bs = signal.len() as u32;
    let mut best_bits = subframe::verbatim_bits(bs, subframe_bps, wasted_bits);
    let mut best = Choice::Verbatim;

    if bs > MAX_FIXED_ORDER {
        // libFLAC's constant detection keys off `fixed_residual_bits_per_sample[1]
        // == 0.0`, which the guess predictor only produces below 28 bps; at
        // `subframe_bps >= 28` the `_limit_residual` predictor reports 34.0 even for
        // a constant signal, so CONSTANT is never selected there (it becomes FIXED).
        if subframe_bps < 28 && signal.iter().all(|&s| s == signal[0]) {
            let cb = subframe::constant_bits(subframe_bps, wasted_bits);
            if cb < best_bits {
                best_bits = cb;
                best = Choice::Constant;
            }
        } else {
            let (order, fixed_rbps) = fixed::compute_best_predictor_order(signal, subframe_bps);
            // libFLAC skips a fixed order whose estimated bits/sample already meets
            // or exceeds the subframe bps (`process_subframe_`, stream_encoder.c:3561)
            // — e.g. an incompressible/overflowing wide signal — leaving VERBATIM.
            if (fixed_rbps) < subframe_bps as f32 {
                let residual = fixed::compute_residual(signal, order);
                let (rice_part, residual_bits) = rice::find_best_partition_order(
                    &residual,
                    order,
                    rice_parameter_limit,
                    min_partition_order,
                    max_partition_order,
                );
                let fb = subframe::fixed_bits(order, subframe_bps, wasted_bits, residual_bits);
                if fb < best_bits {
                    best_bits = fb;
                    best = Choice::Fixed {
                        order,
                        residual,
                        rice: rice_part,
                    };
                }
            }

            if let Some(ctx) = lpc_ctx {
                if let Some((lpc_choice, lpc_bits)) = best_lpc_subframe(
                    signal,
                    subframe_bps,
                    wasted_bits,
                    blocksize,
                    ctx,
                    rice_parameter_limit,
                    min_partition_order,
                    max_partition_order,
                ) {
                    if lpc_bits < best_bits {
                        best_bits = lpc_bits;
                        best = Choice::Lpc(lpc_choice);
                    }
                }
            }
        }
    }

    (best, best_bits)
}

/// Write a previously-chosen subframe (`add_subframe_`, `stream_encoder.c:3787`).
/// `signal` is the channel block the choice was made on (for warmup/raw samples).
fn write_choice(
    bw: &mut BitWriter,
    choice: &Choice,
    signal: &[i64],
    subframe_bps: u32,
    wasted_bits: u32,
) {
    match choice {
        Choice::Constant => subframe::write_constant(bw, signal[0], subframe_bps, wasted_bits),
        Choice::Verbatim => subframe::write_verbatim(bw, signal, subframe_bps, wasted_bits),
        Choice::Fixed {
            order,
            residual,
            rice,
        } => subframe::write_fixed(
            bw,
            *order,
            &signal[..*order as usize],
            subframe_bps,
            wasted_bits,
            residual,
            rice,
        ),
        Choice::Lpc(c) => subframe::write_lpc(
            bw,
            c.order,
            &signal[..c.order as usize],
            &c.qlp_coeff,
            c.precision,
            c.shift,
            subframe_bps,
            wasted_bits,
            &c.residual,
            &c.rice,
        ),
    }
}

/// Mark every seek point whose target sample falls in the frame starting at
/// `frame_first_sample` (`write_frame_`, `stream_encoder.c:2741`). The match range
/// and the recorded `frame_samples` use this frame's *actual* sample count
/// (`blocksize`): libFLAC reads `get_blocksize()`, which equals the configured block
/// size for every frame except the short final one, where `finish` lowers it to the
/// remaining sample count (`stream_encoder.c:1493`). `stream_offset` is the frame's
/// byte offset from the first audio frame. `first` is the persistent
/// `first_seekpoint_to_check` cursor; points are visited in (sorted) target order,
/// and a claimed point's `sample_number` is rewritten to the frame's first sample,
/// so several targets landing in one frame become duplicates (deduped at finish by
/// [`metadata::seektable_sort`]).
fn fill_seekpoints(
    points: &mut [metadata::SeekPoint],
    first: &mut usize,
    frame_first_sample: u64,
    blocksize: u64,
    stream_offset: u64,
) {
    let frame_last_sample = frame_first_sample + blocksize - 1;
    while *first < points.len() {
        let test = points[*first].sample_number;
        if test > frame_last_sample {
            break; // belongs to a later frame; resume here next time
        }
        if test >= frame_first_sample {
            points[*first] = metadata::SeekPoint {
                sample_number: frame_first_sample,
                stream_offset,
                frame_samples: blocksize as u32,
            };
        }
        // Either claimed (set above) or already passed (test < frame_first_sample);
        // advance the cursor in both cases, as the C does.
        *first += 1;
    }
}

/// Encode interleaved integer PCM into FLAC audio frames (no metadata) with the
/// given compression `config`. For stereo with `do_mid_side`, each frame picks the
/// channel assignment with the fewest bits (or, with `loose_mid_side`, re-decides
/// only every ~0.4 s and reuses it between). The block size is fixed except for a
/// possibly shorter final frame.
pub fn encode_frames(
    interleaved: &[i32],
    channels: u32,
    bits_per_sample: u32,
    sample_rate: u32,
    blocksize: u32,
    config: &Config,
) -> Vec<u8> {
    encode_frames_inner(
        interleaved,
        channels,
        bits_per_sample,
        sample_rate,
        blocksize,
        config,
        None,
        None,
    )
    .0
}

/// As [`encode_frames`], also returning the min/max frame size in bytes (for
/// STREAMINFO). `min_framesize` is `0xFFFFFF` if no frames were produced, as in
/// libFLAC. When `seektable`
/// is `Some`, it is a SEEKTABLE *template* (sorted target sample numbers) filled in
/// place as frames go by, then sorted/uniquified at finish — exactly as libFLAC
/// generates a seektable during encoding. When `frame_lengths` is `Some`, each
/// frame's byte length is appended (so the Ogg path can split the frame stream into
/// per-frame packets).
#[allow(clippy::too_many_arguments)]
fn encode_frames_inner(
    interleaved: &[i32],
    channels: u32,
    bits_per_sample: u32,
    sample_rate: u32,
    blocksize: u32,
    config: &Config,
    mut seektable: Option<&mut [metadata::SeekPoint]>,
    mut frame_lengths: Option<&mut Vec<usize>>,
) -> (Vec<u8>, u32, u32) {
    let ch = channels as usize;
    assert!(ch > 0 && interleaved.len() % ch == 0, "ragged interleave");
    let total = interleaved.len() / ch;
    let stereo_ms = config.do_mid_side && ch == 2;

    // Auto qlp precision is fixed once from the stream bps and the full block size.
    let qlp_coeff_precision = auto_qlp_coeff_precision(bits_per_sample, blocksize);
    // The apodization window, recomputed only when the frame block size changes
    // (i.e. once for the full frames, again for a short final frame).
    let window_p = config.apodization.window_p();
    let mut window_bs = 0usize;
    let mut window_buf: Vec<f32> = Vec::new();

    // Loose mid-side: re-decide the assignment only every `loose_frames` frames
    // (`stream_encoder.c:894`), reusing it in between.
    let loose_frames = if config.loose_mid_side {
        ((sample_rate as f64 * 0.4 / blocksize as f64 + 0.5) as u32).max(1)
    } else {
        0
    };
    let mut loose_count = 0u32;
    let mut last_assignment = ChannelAssignment::Independent;

    let mut out = Vec::new();
    let mut min_framesize = u32::MAX;
    let mut max_framesize = 0u32;
    let mut frame_number = 0u32;
    // Persistent SEEKTABLE cursor (`first_seekpoint_to_check`): the index of the
    // first not-yet-resolved seek point, only ever advanced.
    let mut first_seekpoint_to_check = 0usize;
    let mut start = 0usize;
    while start < total {
        let bs = (total - start).min(blocksize as usize);
        // Byte offset of this frame from the first audio frame (= bytes already
        // emitted, since `out` holds only frames here) — the seek-point offset.
        let stream_offset = out.len() as u64;
        if let Some(points) = seektable.as_deref_mut() {
            fill_seekpoints(
                points,
                &mut first_seekpoint_to_check,
                start as u64,
                bs as u64,
                stream_offset,
            );
        }

        // Per-frame Rice partition-order bounds (`process_subframes_:3163`). The
        // C clamps min to max (`flac_min`); min is 0 so that is a no-op, and
        // `find_best_partition_order` re-clamps min against the limited max.
        let max_partition_order = rice::max_partition_order_from_blocksize(bs as u32)
            .min(config.max_residual_partition_order);
        let min_partition_order = MIN_RESIDUAL_PARTITION_ORDER;

        // (Re)compute the window for this block size if LPC is enabled.
        if config.max_lpc_order > 0 && bs > 1 && bs != window_bs {
            window_buf = vec![0f32; bs];
            window::tukey(&mut window_buf, window_p);
            window_bs = bs;
        }
        let lpc_ctx = (config.max_lpc_order > 0 && bs > 1).then(|| LpcCtx {
            window: &window_buf,
            max_lpc_order: config.max_lpc_order,
            qlp_coeff_precision,
            apodization: config.apodization,
        });

        // Wasted-bits-shift a channel signal, then choose its best subframe.
        // `extra_bps` is 1 for the side channel (its values span one extra bit).
        let choose = |signal: Vec<i64>, extra_bps: u32| -> ChosenChannel {
            let mut sig = signal;
            // The 33-bit-side wasted-bits variant is used only for the side channel
            // (`extra_bps == 1`) of a 32-bit stream (`get_wasted_bits_wide_`).
            let wide = extra_bps == 1 && bits_per_sample == 32;
            let mut wasted = get_wasted_bits(&mut sig, wide);
            if wasted > bits_per_sample {
                wasted = bits_per_sample;
            }
            let subframe_bps = bits_per_sample - wasted + extra_bps;
            let (choice, bits) = choose_subframe(
                &sig,
                subframe_bps,
                wasted,
                bs as u32,
                rice_parameter_limit(bits_per_sample),
                min_partition_order,
                max_partition_order,
                lpc_ctx.as_ref(),
            );
            ChosenChannel {
                signal: sig,
                subframe_bps,
                wasted_bits: wasted,
                choice,
                bits,
            }
        };

        let mut frame = BitWriter::new();

        if stereo_ms {
            let left: Vec<i64> = (0..bs)
                .map(|i| interleaved[(start + i) * ch] as i64)
                .collect();
            let right: Vec<i64> = (0..bs)
                .map(|i| interleaved[(start + i) * ch + 1] as i64)
                .collect();
            // Mid/side from the *original* (pre-wasted-shift) L/R
            // (`process_subframes_:3210`): side = L - R, mid = (L + R) >> 1. The i64
            // arithmetic is exact for the 33-bit side / 32-bit mid of 32-bit input
            // and matches the i32 path for ≤24-bit.
            let mid_side = || {
                let mid: Vec<i64> = (0..bs).map(|i| (left[i] + right[i]) >> 1).collect();
                let side: Vec<i64> = (0..bs).map(|i| left[i] - right[i]).collect();
                (mid, side)
            };

            // Between loose decision points, reuse the last assignment and encode
            // only the channels it needs.
            let (assignment, pair): (ChannelAssignment, [ChosenChannel; 2]) =
                if config.loose_mid_side && loose_count != 0 {
                    if last_assignment == ChannelAssignment::Independent {
                        (
                            ChannelAssignment::Independent,
                            [choose(left, 0), choose(right, 0)],
                        )
                    } else {
                        let (mid, side) = mid_side();
                        (
                            ChannelAssignment::MidSide,
                            [choose(mid, 0), choose(side, 1)],
                        )
                    }
                } else {
                    let (mid, side) = mid_side();
                    let cl = choose(left, 0);
                    let cr = choose(right, 0);
                    let cm = choose(mid, 0);
                    let cs = choose(side, 1);
                    // Smallest total wins, independent preferred on ties. Loose
                    // decision frames consider only independent vs mid-side.
                    let mut assignment = ChannelAssignment::Independent;
                    let mut min_bits = cl.bits + cr.bits;
                    let all = !config.loose_mid_side;
                    for (enabled, bits, ca) in [
                        (all, cl.bits + cs.bits, ChannelAssignment::LeftSide),
                        (all, cr.bits + cs.bits, ChannelAssignment::RightSide),
                        (true, cm.bits + cs.bits, ChannelAssignment::MidSide),
                    ] {
                        if enabled && bits < min_bits {
                            min_bits = bits;
                            assignment = ca;
                        }
                    }
                    let pair = match assignment {
                        ChannelAssignment::Independent => [cl, cr],
                        ChannelAssignment::LeftSide => [cl, cs],
                        ChannelAssignment::RightSide => [cs, cr],
                        ChannelAssignment::MidSide => [cm, cs],
                    };
                    (assignment, pair)
                };

            write_frame_header(
                &mut frame,
                &FrameHeader {
                    blocksize: bs as u32,
                    sample_rate,
                    channels,
                    channel_assignment: assignment,
                    bits_per_sample,
                    frame_number,
                },
            );
            for cc in &pair {
                write_choice(
                    &mut frame,
                    &cc.choice,
                    &cc.signal,
                    cc.subframe_bps,
                    cc.wasted_bits,
                );
            }

            last_assignment = assignment;
            if config.loose_mid_side {
                loose_count += 1;
                if loose_count >= loose_frames {
                    loose_count = 0;
                }
            }
        } else {
            write_frame_header(
                &mut frame,
                &FrameHeader {
                    blocksize: bs as u32,
                    sample_rate,
                    channels,
                    channel_assignment: ChannelAssignment::Independent,
                    bits_per_sample,
                    frame_number,
                },
            );
            for c in 0..ch {
                let signal: Vec<i64> = (0..bs)
                    .map(|i| interleaved[(start + i) * ch + c] as i64)
                    .collect();
                let cc = choose(signal, 0);
                write_choice(
                    &mut frame,
                    &cc.choice,
                    &cc.signal,
                    cc.subframe_bps,
                    cc.wasted_bits,
                );
            }
        }

        write_frame_footer(&mut frame);
        let fsize = frame.as_bytes().len() as u32;
        min_framesize = min_framesize.min(fsize);
        max_framesize = max_framesize.max(fsize);
        if let Some(fl) = frame_lengths.as_deref_mut() {
            fl.push(fsize as usize);
        }
        out.extend_from_slice(frame.as_bytes());

        start += bs;
        frame_number += 1;
    }
    // With no frames, libFLAC finishes with the "absurdly-high" initial value it
    // sets once STREAMINFO is first written (`stream_encoder.c:1224`).
    if min_framesize == u32::MAX {
        min_framesize = (1 << 24) - 1;
    }
    // Finish: sort + uniquify the filled seektable, padding the tail with
    // placeholders (`stream_encoder.c:2918`).
    if let Some(points) = seektable {
        metadata::seektable_sort(points);
    }
    (out, min_framesize, max_framesize)
}

/// Encode interleaved integer PCM into a complete FLAC stream: the `fLaC` marker,
/// STREAMINFO, the given metadata `blocks` (in order), then the audio frames.
/// `do_md5` controls whether STREAMINFO carries the audio MD5 (libFLAC's
/// `--no-md5-sum` writes zeros). `blocks` are written after STREAMINFO exactly as
/// supplied — pass a single [`metadata::MetadataBlock::VorbisComment`] with
/// [`metadata::LIBFLAC_VENDOR_STRING`] to match libFLAC's default output. The last
/// block written (STREAMINFO if `blocks` is empty) carries the `is_last` flag.
#[allow(clippy::too_many_arguments)]
pub fn encode(
    interleaved: &[i32],
    channels: u32,
    bits_per_sample: u32,
    sample_rate: u32,
    blocksize: u32,
    config: &Config,
    do_md5: bool,
    blocks: &[metadata::MetadataBlock],
) -> Vec<u8> {
    let ch = channels as usize;
    assert!(ch > 0 && interleaved.len() % ch == 0, "ragged interleave");
    let total_samples = (interleaved.len() / ch) as u64;

    // libFLAC fills the (first) SEEKTABLE during encoding, then rewrites it; we
    // clone the template, fill it as frames are produced, and serialize the filled
    // copy below in place of the original block.
    let mut filled_seektable: Option<Vec<metadata::SeekPoint>> = blocks.iter().find_map(|b| {
        if let metadata::MetadataBlock::Seektable(pts) = b {
            Some(pts.to_vec())
        } else {
            None
        }
    });

    let (frames, min_framesize, max_framesize) = encode_frames_inner(
        interleaved,
        channels,
        bits_per_sample,
        sample_rate,
        blocksize,
        config,
        filled_seektable.as_deref_mut(),
        None,
    );

    let md5 = if do_md5 {
        crate::md5::audio_md5(interleaved, bits_per_sample.div_ceil(8) as usize)
    } else {
        [0u8; 16]
    };

    let si = metadata::StreamInfo {
        min_blocksize: blocksize,
        max_blocksize: blocksize,
        min_framesize,
        max_framesize,
        sample_rate,
        channels,
        bits_per_sample,
        total_samples,
        md5,
    };

    let mut bw = BitWriter::new();
    bw.write_byte_block(b"fLaC");
    metadata::write_streaminfo(&mut bw, &si, blocks.is_empty());
    for (i, block) in blocks.iter().enumerate() {
        let is_last = i + 1 == blocks.len();
        match block {
            // Substitute the filled+sorted seek points for the supplied template.
            metadata::MetadataBlock::Seektable(_) => metadata::write_seektable(
                &mut bw,
                filled_seektable.as_deref().unwrap_or(&[]),
                is_last,
            ),
            _ => metadata::write_block(&mut bw, block, is_last),
        }
    }
    let mut out = bw.as_bytes().to_vec();
    out.extend_from_slice(&frames);
    out
}

/// Encode interleaved integer PCM into a complete **Ogg FLAC** stream, byte-identical
/// to libFLAC+libogg. The native FLAC stream (STREAMINFO + `blocks` + frames) is
/// mapped into Ogg packets and paged exactly as libFLAC's `ogg_encoder_aspect` drives
/// libogg:
/// - The first packet (BOS page) is `0x7F` + `"FLAC"` + mapping version `1.0` +
///   a 2-byte header count (always 0 = "unknown") + `"fLaC"` + STREAMINFO, flushed.
/// - Each remaining metadata block is its own flushed packet/page. **A SEEKTABLE is
///   dropped** (libFLAC removes it for Ogg).
/// - Each audio frame is its own packet, paged out (accumulated) with the last frame
///   carrying EOS. Granule positions are cumulative sample counts.
///
/// `serial` is the Ogg logical-bitstream serial number. As with [`encode`], pass a
/// [`metadata::MetadataBlock::VorbisComment`] with [`metadata::LIBFLAC_VENDOR_STRING`]
/// first to match libFLAC's default output.
#[allow(clippy::too_many_arguments)]
pub fn encode_ogg(
    interleaved: &[i32],
    channels: u32,
    bits_per_sample: u32,
    sample_rate: u32,
    blocksize: u32,
    config: &Config,
    do_md5: bool,
    blocks: &[metadata::MetadataBlock],
    serial: i32,
) -> Vec<u8> {
    let ch = channels as usize;
    assert!(ch > 0 && interleaved.len() % ch == 0, "ragged interleave");
    let total_samples = (interleaved.len() / ch) as u64;

    // Encode the audio frames, capturing each frame's byte length so the stream can
    // be split into per-frame Ogg packets. (Ogg drops the seektable, so none here.)
    let mut frame_lengths: Vec<usize> = Vec::new();
    let (frames, min_framesize, max_framesize) = encode_frames_inner(
        interleaved,
        channels,
        bits_per_sample,
        sample_rate,
        blocksize,
        config,
        None,
        Some(&mut frame_lengths),
    );

    let md5 = if do_md5 {
        crate::md5::audio_md5(interleaved, bits_per_sample.div_ceil(8) as usize)
    } else {
        [0u8; 16]
    };
    let si = metadata::StreamInfo {
        min_blocksize: blocksize,
        max_blocksize: blocksize,
        min_framesize,
        max_framesize,
        sample_rate,
        channels,
        bits_per_sample,
        total_samples,
        md5,
    };

    // Metadata blocks to write after STREAMINFO, with the seektable removed.
    let meta: Vec<&metadata::MetadataBlock> = blocks
        .iter()
        .filter(|b| !matches!(b, metadata::MetadataBlock::Seektable(_)))
        .collect();

    // The BOS packet: mapping header + native "fLaC" + STREAMINFO (is_last is false
    // when metadata blocks follow, matching libFLAC's native ordering).
    let mut si_bw = BitWriter::new();
    metadata::write_streaminfo(&mut si_bw, &si, meta.is_empty());
    let mut bos = Vec::with_capacity(13 + 38);
    bos.push(0x7F);
    bos.extend_from_slice(b"FLAC");
    bos.push(1); // mapping version major
    bos.push(0); // mapping version minor
    bos.extend_from_slice(&[0, 0]); // header packet count: 0 = unknown
    bos.extend_from_slice(b"fLaC");
    bos.extend_from_slice(si_bw.as_bytes());

    let mut og = ogg::OggStream::new(serial);
    og.packetin(&bos, false, 0);
    og.flush();

    for (i, block) in meta.iter().enumerate() {
        let is_last = i + 1 == meta.len();
        let mut bw = BitWriter::new();
        metadata::write_block(&mut bw, block, is_last);
        og.packetin(bw.as_bytes(), false, 0);
        og.flush();
    }

    let nframes = frame_lengths.len();
    let mut byte_off = 0usize;
    let mut sample_off = 0u64;
    for (i, &flen) in frame_lengths.iter().enumerate() {
        let fsamples = (blocksize as u64).min(total_samples - sample_off);
        sample_off += fsamples;
        let is_last = i + 1 == nframes;
        og.packetin(
            &frames[byte_off..byte_off + flen],
            is_last,
            sample_off as i64,
        );
        byte_off += flen;
        og.pageout();
    }

    og.into_bytes()
}

/// Audio format and compression settings for an [`Encoder`].
///
/// Build one with [`EncoderConfig::new`] (libFLAC's defaults: compression level 8,
/// 4096-sample blocks, MD5 on) or [`EncoderConfig::chd`] (MAME/CHD's exact
/// configuration), then adjust with the `with_*` methods if needed.
#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    /// Number of channels, 1–8. Stereo (2) enables mid/side decorrelation.
    pub channels: u32,
    /// Bits per sample: 8, 12, 16, 20, 24, or 32.
    pub bits_per_sample: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Fixed block size in samples (the final frame may be shorter). libFLAC's
    /// default is 4096; CHD uses 2048.
    pub block_size: u32,
    /// Compression preset, 0 (fastest) … 8 (smallest); values above 8 clamp to 8.
    pub compression_level: u32,
    /// Whether STREAMINFO carries the audio MD5 signature (libFLAC's `--no-md5-sum`,
    /// and CHD, set this `false`).
    pub md5: bool,
}

impl EncoderConfig {
    /// A config for the given format at libFLAC's defaults: compression level 8,
    /// 4096-sample blocks, MD5 enabled.
    pub fn new(channels: u32, bits_per_sample: u32, sample_rate: u32) -> Self {
        Self {
            channels,
            bits_per_sample,
            sample_rate,
            block_size: 4096,
            compression_level: 8,
            md5: true,
        }
    }

    /// The exact configuration MAME/CHD uses: 2-channel / 16-bit / 44.1 kHz,
    /// compression level 8, MD5 **off**, and the given fixed `block_size` (CHD
    /// typically uses 2048). Pair with [`Encoder::encode_frames`] for the raw frame
    /// bytes CHD embeds.
    pub fn chd(block_size: u32) -> Self {
        Self {
            channels: 2,
            bits_per_sample: 16,
            sample_rate: 44_100,
            block_size,
            compression_level: 8,
            md5: false,
        }
    }

    /// Set the compression level (0–8).
    pub fn with_compression_level(mut self, level: u32) -> Self {
        self.compression_level = level;
        self
    }
    /// Set the fixed block size in samples.
    pub fn with_block_size(mut self, block_size: u32) -> Self {
        self.block_size = block_size;
        self
    }
    /// Set whether STREAMINFO carries the audio MD5.
    pub fn with_md5(mut self, md5: bool) -> Self {
        self.md5 = md5;
        self
    }
}

/// A one-shot FLAC encoder, byte-identical to libFLAC 1.4.3 at the same settings.
///
/// Each `encode*` method takes the **entire** interleaved signal (channel-major
/// within a sample — `L R L R …` for stereo, each value a sign-extended `i32`) and
/// returns the encoded bytes.
///
/// ```
/// use libflac_rs::{Encoder, EncoderConfig};
///
/// let enc = Encoder::new(EncoderConfig::new(2, 16, 44_100));
/// let pcm = vec![0i32; 4096 * 2]; // one block of stereo silence, interleaved
/// let flac = enc.encode(&pcm);    // a complete .flac file
/// assert_eq!(&flac[..4], b"fLaC");
/// ```
pub struct Encoder {
    config: EncoderConfig,
    inner: Config,
}

impl Encoder {
    /// Create an encoder from `config`.
    pub fn new(config: EncoderConfig) -> Self {
        let inner = preset(config.compression_level);
        Self { config, inner }
    }

    /// The configuration this encoder was built with.
    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Encode to **raw FLAC frames** — no `fLaC` marker or metadata — the byte stream
    /// MAME/CHD embeds. (The `md5` setting has no effect here: MD5 lives in
    /// STREAMINFO, which raw frames omit.)
    pub fn encode_frames(&self, interleaved: &[i32]) -> Vec<u8> {
        encode_frames(
            interleaved,
            self.config.channels,
            self.config.bits_per_sample,
            self.config.sample_rate,
            self.config.block_size,
            &self.inner,
        )
    }

    /// Encode to a complete `.flac` file (the `fLaC` marker, STREAMINFO, libFLAC's
    /// default VORBIS_COMMENT, then the frames) — byte-identical to libFLAC's default
    /// output. Use [`Encoder::encode_with_metadata`] to control the metadata.
    pub fn encode(&self, interleaved: &[i32]) -> Vec<u8> {
        self.encode_with_metadata(
            interleaved,
            &[metadata::MetadataBlock::VorbisComment(
                metadata::LIBFLAC_VENDOR_STRING,
            )],
        )
    }

    /// Encode to a complete `.flac` file with the given metadata `blocks`, written
    /// after STREAMINFO in order (the last automatically carries the
    /// end-of-metadata flag). To match libFLAC's output, put a
    /// [`MetadataBlock::VorbisComment`](crate::MetadataBlock::VorbisComment) with
    /// [`LIBFLAC_VENDOR_STRING`](crate::LIBFLAC_VENDOR_STRING) first. A
    /// [`MetadataBlock::Seektable`](crate::MetadataBlock::Seektable) is filled in
    /// during encoding.
    pub fn encode_with_metadata(
        &self,
        interleaved: &[i32],
        blocks: &[metadata::MetadataBlock],
    ) -> Vec<u8> {
        encode(
            interleaved,
            self.config.channels,
            self.config.bits_per_sample,
            self.config.sample_rate,
            self.config.block_size,
            &self.inner,
            self.config.md5,
            blocks,
        )
    }

    /// Encode to a complete **Ogg FLAC** stream with the given Ogg logical-bitstream
    /// `serial` number. libFLAC's default VORBIS_COMMENT is included; any SEEKTABLE is
    /// dropped (as libFLAC does for Ogg).
    pub fn encode_ogg(&self, interleaved: &[i32], serial: i32) -> Vec<u8> {
        encode_ogg(
            interleaved,
            self.config.channels,
            self.config.bits_per_sample,
            self.config.sample_rate,
            self.config.block_size,
            &self.inner,
            self.config.md5,
            &[metadata::MetadataBlock::VorbisComment(
                metadata::LIBFLAC_VENDOR_STRING,
            )],
            serial,
        )
    }
}
