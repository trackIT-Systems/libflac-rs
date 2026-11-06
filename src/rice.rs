//! Partitioned-Rice parameter/partition-order search (integer), ported from
//! `find_best_partition_order_` / `precompute_partition_info_sums_` /
//! `set_partitioned_rice_` / `count_rice_bits_in_partition_` (`stream_encoder.c`).
//!
//! At CHD's level-8 / 16-bit config: escape coding is off (always plain
//! `PARTITIONED_RICE`, never RICE2), `rice_parameter_search_dist` is 0 (one
//! estimate per partition), and the bit count is the *estimate* formula
//! (`EXACT_RICE_BITS_CALCULATION` is not defined in MAME's build).

use crate::format::*;

/// Max partition order representable in the 4-bit order field.
pub const MAX_RICE_PARTITION_ORDER: u32 = 15;

/// The chosen partition order and its per-partition Rice parameters.
pub struct RicePartition {
    pub order: u32,
    pub parameters: Vec<u32>,
    /// True if any parameter reaches the RICE escape value (15), so the subframe
    /// must use PARTITIONED_RICE2 (5-bit parameters) — only possible above 16 bps
    /// (`find_best_partition_order_`, `stream_encoder.c:4774`).
    pub is_rice2: bool,
}

#[inline]
fn ilog2_u64(v: u64) -> u32 {
    debug_assert!(v > 0);
    63 - v.leading_zeros()
}

/// Largest partition order dividing `blocksize` (`...from_blocksize`,
/// `format.c:540`): the count of trailing zero bits, capped.
pub fn max_partition_order_from_blocksize(blocksize: u32) -> u32 {
    blocksize.trailing_zeros().min(MAX_RICE_PARTITION_ORDER)
}

/// Reduce `limit` until the first partition holds more than `predictor_order`
/// samples (`..._limited_max_and_predictor_order`, `format.c:550`).
fn max_partition_order_limited(limit: u32, blocksize: u32, predictor_order: u32) -> u32 {
    let mut o = limit;
    while o > 0 && (blocksize >> o) <= predictor_order {
        o -= 1;
    }
    o
}

/// Per-partition |residual| sums, laid out highest-order-first:
/// `[2^max .. 2^min]` (`precompute_partition_info_sums_`, `stream_encoder.c:4785`).
/// Always accumulates in 64 bits (the C's 32-bit fast path yields the same sums).
fn precompute_partition_info_sums(
    residual: &[i32],
    predictor_order: u32,
    min_partition_order: u32,
    max_partition_order: u32,
) -> Vec<u64> {
    let residual_samples = residual.len() as u32;
    let blocksize = residual_samples + predictor_order;
    let default_partition_samples = blocksize >> max_partition_order;

    let total = (1usize << (max_partition_order + 1)) - (1usize << min_partition_order);
    let mut sums = vec![0u64; total];

    // Highest order: partition 0 omits the warmup samples, the rest are full.
    let partitions_max = 1usize << max_partition_order;
    let mut rs = 0usize;
    for (partition, slot) in sums.iter_mut().enumerate().take(partitions_max) {
        let psamps = if partition == 0 {
            default_partition_samples - predictor_order
        } else {
            default_partition_samples
        } as usize;
        let mut s = 0u64;
        for _ in 0..psamps {
            s += (residual[rs] as i64).unsigned_abs();
            rs += 1;
        }
        *slot = s;
    }

    // Merge adjacent pairs down to lower orders.
    let mut from = 0usize;
    let mut to = partitions_max;
    let mut partitions = partitions_max;
    let mut po = max_partition_order;
    while po > min_partition_order {
        po -= 1;
        partitions >>= 1;
        for _ in 0..partitions {
            sums[to] = sums[from] + sums[from + 1];
            to += 1;
            from += 2;
        }
    }
    sums
}

/// Estimated bits to Rice-code a partition (`count_rice_bits_in_partition_`,
/// non-exact form). `abs_sum` is the sum of magnitudes in the partition.
fn count_rice_bits(rice_parameter: u32, partition_samples: u32, abs_sum: u64) -> u32 {
    let extra = if rice_parameter > 0 {
        abs_sum >> (rice_parameter - 1)
    } else {
        abs_sum << 1
    };
    let bits = u64::from(ENTROPY_CODING_METHOD_PARTITIONED_RICE_PARAMETER_LEN)
        + u64::from(1 + rice_parameter) * u64::from(partition_samples)
        + extra
        - u64::from(partition_samples >> 1);
    bits.min(u64::from(u32::MAX)) as u32
}

/// Choose Rice parameters for a fixed partition order (`set_partitioned_rice_`,
/// escapes off, no parameter search). Returns the parameters and the total
/// estimated bits (including the 6-bit method header), or `None` if the order is
/// invalid for this predictor order.
fn set_partitioned_rice(
    abs_sums: &[u64],
    residual_samples: u32,
    predictor_order: u32,
    rice_parameter_limit: u32,
    partition_order: u32,
) -> Option<(Vec<u32>, u32)> {
    let partitions = 1usize << partition_order;
    let mut params = vec![0u32; partitions];
    let mut bits = u64::from(
        ENTROPY_CODING_METHOD_TYPE_LEN + ENTROPY_CODING_METHOD_PARTITIONED_RICE_ORDER_LEN,
    );
    let partition_samples_base = (residual_samples + predictor_order) >> partition_order;
    let divisor_base = 0x40000u32 / partition_samples_base;

    for (partition, slot) in params.iter_mut().enumerate() {
        let (partition_samples, divisor) = if partition > 0 {
            (partition_samples_base, divisor_base)
        } else {
            if partition_samples_base <= predictor_order {
                return None;
            }
            let ps = partition_samples_base - predictor_order;
            (ps, 0x40000u32 / ps)
        };

        let mean = abs_sums[partition];
        // Match libFLAC's short-circuit (`set_partitioned_rice_`,
        // stream_encoder.c:5005): the `(mean-1)*divisor` product is only formed when
        // `mean >= 2`, so an all-zero partition (`mean == 0`) never underflows. The
        // `wrapping_mul` matches the C's well-defined `u64` wrap for huge blocksizes.
        let mut rice_parameter = if mean < 2 {
            0
        } else {
            let scaled = mean.wrapping_sub(1).wrapping_mul(u64::from(divisor)) >> 18;
            if scaled == 0 {
                0
            } else {
                ilog2_u64(scaled) + 1
            }
        };
        if rice_parameter >= rice_parameter_limit {
            rice_parameter = rice_parameter_limit - 1;
        }

        *slot = rice_parameter;
        let partition_bits = count_rice_bits(rice_parameter, partition_samples, mean);
        bits = if u64::from(partition_bits) < u64::from(u32::MAX) - bits {
            bits + u64::from(partition_bits)
        } else {
            u64::from(u32::MAX)
        };
    }
    Some((params, bits.min(u64::from(u32::MAX)) as u32))
}

/// Search partition orders `[min, max]` for the fewest estimated residual bits
/// (`find_best_partition_order_`). On ties the higher order wins (it is evaluated
/// first and replacement requires a strict improvement). Returns the chosen
/// partition + parameters and the best estimated bit count.
pub fn find_best_partition_order(
    residual: &[i32],
    predictor_order: u32,
    rice_parameter_limit: u32,
    min_partition_order: u32,
    max_partition_order: u32,
) -> (RicePartition, u32) {
    let residual_samples = residual.len() as u32;
    let blocksize = residual_samples + predictor_order;
    let max_po = max_partition_order_limited(max_partition_order, blocksize, predictor_order);
    let min_po = min_partition_order.min(max_po);

    let abs_sums = precompute_partition_info_sums(residual, predictor_order, min_po, max_po);

    let mut best_bits = 0u32;
    let mut best_params: Vec<u32> = Vec::new();
    let mut best_order = 0u32;
    let mut found = false;

    let mut offset = 0usize;
    let mut po = max_po;
    loop {
        let slice = &abs_sums[offset..offset + (1usize << po)];
        match set_partitioned_rice(
            slice,
            residual_samples,
            predictor_order,
            rice_parameter_limit,
            po,
        ) {
            Some((params, bits)) => {
                if !found || bits < best_bits {
                    best_bits = bits;
                    best_params = params;
                    best_order = po;
                    found = true;
                }
            }
            None => break,
        }
        offset += 1usize << po;
        if po == min_po {
            break;
        }
        po -= 1;
    }

    // PARTITIONED_RICE2 is used iff any parameter reached the RICE escape value.
    let is_rice2 = best_params
        .iter()
        .any(|&p| p >= ENTROPY_CODING_METHOD_PARTITIONED_RICE_ESCAPE_PARAMETER);

    (
        RicePartition {
            order: best_order,
            parameters: best_params,
            is_rice2,
        },
        best_bits,
    )
}
