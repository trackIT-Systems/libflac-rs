//! Subframe writers, ported from `FLAC__subframe_add_*`
//! (`stream_encoder_framing.c:393+`). FIXED/LPC bodies are added in later
//! milestones; CONSTANT and VERBATIM are here.

use crate::bitwriter::BitWriter;
use crate::format::*;
use crate::rice::RicePartition;

/// Write the 8-bit byte-aligned subframe header (zero-pad + 6-bit type + wasted
/// flag), then the wasted-bits unary count if any. `type_bits` is the pre-shifted
/// type mask (already including the predictor order for FIXED/LPC).
fn write_subframe_header(bw: &mut BitWriter, type_bits: u32, wasted_bits: u32) {
    bw.write_raw_u32(type_bits | u32::from(wasted_bits > 0), SUBFRAME_HEADER_LEN);
    if wasted_bits > 0 {
        bw.write_unary_unsigned(wasted_bits - 1);
    }
}

/// CONSTANT subframe: the single repeated value (`FLAC__subframe_add_constant`).
pub fn write_constant(bw: &mut BitWriter, value: i64, subframe_bps: u32, wasted_bits: u32) {
    write_subframe_header(bw, SUBFRAME_TYPE_CONSTANT_BYTE_ALIGNED_MASK, wasted_bits);
    bw.write_raw_i64(value, subframe_bps);
}

/// VERBATIM subframe: every sample stored raw (`FLAC__subframe_add_verbatim`).
/// `signal` is `i64` (the unified channel type; up to 33 bps for the side channel).
pub fn write_verbatim(bw: &mut BitWriter, signal: &[i64], subframe_bps: u32, wasted_bits: u32) {
    write_subframe_header(bw, SUBFRAME_TYPE_VERBATIM_BYTE_ALIGNED_MASK, wasted_bits);
    for &s in signal {
        bw.write_raw_i64(s, subframe_bps);
    }
}

/// Bit cost of a CONSTANT subframe (`evaluate_constant_subframe_`,
/// `stream_encoder.c:4466`): header + wasted-bits unary + one sample.
pub fn constant_bits(subframe_bps: u32, wasted_bits: u32) -> u32 {
    SUBFRAME_HEADER_LEN + wasted_bits + subframe_bps
}

/// Bit cost of a VERBATIM subframe (`evaluate_verbatim_subframe_`,
/// `stream_encoder.c:4678`): header + wasted-bits unary + every sample.
pub fn verbatim_bits(blocksize: u32, subframe_bps: u32, wasted_bits: u32) -> u32 {
    SUBFRAME_HEADER_LEN + wasted_bits + blocksize * subframe_bps
}

/// FIXED subframe: header, `order` warmup samples, then the partitioned-rice
/// residual (`FLAC__subframe_add_fixed`, `stream_encoder_framing.c:406`).
pub fn write_fixed(
    bw: &mut BitWriter,
    order: u32,
    warmup: &[i64],
    subframe_bps: u32,
    wasted_bits: u32,
    residual: &[i32],
    rice: &RicePartition,
) {
    write_subframe_header(
        bw,
        SUBFRAME_TYPE_FIXED_BYTE_ALIGNED_MASK | (order << 1),
        wasted_bits,
    );
    for &w in warmup {
        bw.write_raw_i64(w, subframe_bps);
    }
    write_entropy_and_residual(bw, residual, order, rice);
}

/// Bit cost of a FIXED subframe (`evaluate_fixed_subframe_`,
/// `stream_encoder.c:4538`): header + wasted + `order` warmup + residual estimate.
pub fn fixed_bits(order: u32, subframe_bps: u32, wasted_bits: u32, residual_bits: u32) -> u32 {
    let header = SUBFRAME_HEADER_LEN + wasted_bits + order * subframe_bps;
    if residual_bits < u32::MAX - header {
        header + residual_bits
    } else {
        u32::MAX
    }
}

/// LPC subframe (`FLAC__subframe_add_lpc`, `stream_encoder_framing.c:444`): header,
/// `order` warmup samples, the `qlp_coeff_precision-1` field, the 5-bit
/// quantization level (shift), the `order` quantized coefficients, then the
/// partitioned-rice residual.
#[allow(clippy::too_many_arguments)]
pub fn write_lpc(
    bw: &mut BitWriter,
    order: u32,
    warmup: &[i64],
    qlp_coeff: &[i32],
    precision: u32,
    shift: i32,
    subframe_bps: u32,
    wasted_bits: u32,
    residual: &[i32],
    rice: &RicePartition,
) {
    write_subframe_header(
        bw,
        SUBFRAME_TYPE_LPC_BYTE_ALIGNED_MASK | ((order - 1) << 1),
        wasted_bits,
    );
    for &w in warmup {
        bw.write_raw_i64(w, subframe_bps);
    }
    bw.write_raw_u32(precision - 1, SUBFRAME_LPC_QLP_COEFF_PRECISION_LEN);
    bw.write_raw_i32(shift, SUBFRAME_LPC_QLP_SHIFT_LEN);
    for &c in qlp_coeff {
        bw.write_raw_i32(c, precision);
    }
    write_entropy_and_residual(bw, residual, order, rice);
}

/// Write the entropy-coding-method header (RICE or RICE2 by `rice.is_rice2`) and
/// the partitioned-rice residual (`add_entropy_coding_method_` +
/// `add_residual_partitioned_rice_`).
fn write_entropy_and_residual(
    bw: &mut BitWriter,
    residual: &[i32],
    predictor_order: u32,
    rice: &RicePartition,
) {
    let method_type = if rice.is_rice2 {
        ENTROPY_CODING_METHOD_PARTITIONED_RICE2
    } else {
        ENTROPY_CODING_METHOD_PARTITIONED_RICE
    };
    bw.write_raw_u32(method_type, ENTROPY_CODING_METHOD_TYPE_LEN);
    bw.write_raw_u32(rice.order, ENTROPY_CODING_METHOD_PARTITIONED_RICE_ORDER_LEN);
    write_residual_partitioned_rice(
        bw,
        residual,
        predictor_order,
        &rice.parameters,
        rice.order,
        rice.is_rice2,
    );
}

/// Bit cost of an LPC subframe (`evaluate_lpc_subframe_`, `stream_encoder.c:4643`):
/// header + wasted + the precision/shift fields + `order*(precision+bps)` (warmup
/// + coeffs) + residual estimate.
pub fn lpc_bits(
    order: u32,
    precision: u32,
    subframe_bps: u32,
    wasted_bits: u32,
    residual_bits: u32,
) -> u32 {
    let header = SUBFRAME_HEADER_LEN
        + wasted_bits
        + SUBFRAME_LPC_QLP_COEFF_PRECISION_LEN
        + SUBFRAME_LPC_QLP_SHIFT_LEN
        + order * (precision + subframe_bps);
    if residual_bits < u32::MAX - header {
        header + residual_bits
    } else {
        u32::MAX
    }
}

/// Partitioned-rice residual (`add_residual_partitioned_rice_`,
/// `stream_encoder_framing.c:538`). Escape coding is off, so every partition uses
/// the plain-rice path with a 4-bit parameter.
fn write_residual_partitioned_rice(
    bw: &mut BitWriter,
    residual: &[i32],
    predictor_order: u32,
    parameters: &[u32],
    partition_order: u32,
    is_rice2: bool,
) {
    let plen = if is_rice2 {
        ENTROPY_CODING_METHOD_PARTITIONED_RICE2_PARAMETER_LEN
    } else {
        ENTROPY_CODING_METHOD_PARTITIONED_RICE_PARAMETER_LEN
    };
    if partition_order == 0 {
        bw.write_raw_u32(parameters[0], plen);
        bw.write_rice_signed_block(residual, parameters[0]);
        return;
    }
    let residual_samples = residual.len() as u32;
    let blocksize = residual_samples + predictor_order;
    let default_partition_samples = blocksize >> partition_order;
    let mut k = 0usize;
    let mut k_last = 0usize;
    for (i, &param) in parameters
        .iter()
        .enumerate()
        .take(1usize << partition_order)
    {
        let psamps = if i == 0 {
            default_partition_samples - predictor_order
        } else {
            default_partition_samples
        } as usize;
        k += psamps;
        bw.write_raw_u32(param, plen);
        bw.write_rice_signed_block(&residual[k_last..k], param);
        k_last = k;
    }
}
