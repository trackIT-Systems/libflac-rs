//! Malformed input never panics: valid streams with random byte flips,
//! overwrites and truncations go through every `decode*` function, which may fail
//! (`None`) but must not panic. Runs with overflow checks in debug builds.

use libflac_rs::{Encoder, EncoderConfig, decode, decode_frames, decode_ogg_chained, decode_seek};
use std::panic::catch_unwind;

/// xorshift64, so every run mutates identically.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn mutated_streams_do_not_panic() {
    let mut seeds = Vec::new();
    for (channels, bps, level, block) in [
        (1u32, 16u32, 8u32, 192u32),
        (2, 16, 5, 256),
        (2, 24, 8, 512),
        (1, 8, 0, 64),
        (2, 32, 8, 256),
        (3, 20, 3, 128),
    ] {
        let len = block as usize * 4 + 37;
        let max = ((1i64 << (bps - 1)) - 1) as f64;
        let pcm: Vec<i32> = (0..len * channels as usize)
            .map(|i| {
                let t = i as f64;
                ((0.6 * (t * 0.01).sin() + 0.3 * (t * 0.37).sin()) * max) as i32
            })
            .collect();
        let enc = Encoder::new(
            EncoderConfig::new(channels, bps, 44_100)
                .with_compression_level(level)
                .with_block_size(block),
        );
        seeds.push((0, enc.encode(&pcm)));
        seeds.push((1, enc.encode_frames(&pcm)));
        seeds.push((2, enc.encode_ogg(&pcm, 9)));
    }

    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for iteration in 0..30_000 {
        let (kind, seed) = &seeds[rng.below(seeds.len())];
        let mut data = seed.clone();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(data.len());
            match rng.below(4) {
                0 | 1 => data[i] ^= 1 << rng.below(8),
                2 => data[i] = rng.next() as u8,
                _ => data.truncate(i.max(1)),
            }
        }
        let kind = *kind;
        let input = data.clone();
        let result = catch_unwind(move || match kind {
            0 => {
                let _ = decode(&input);
                let _ = decode_seek(&input, 300);
            }
            1 => {
                let _ = decode_frames(&input);
            }
            _ => {
                let _ = decode_ogg_chained(&input);
            }
        });
        assert!(
            result.is_ok(),
            "iteration {iteration}: decoder panicked on {} bytes: {:02x?}",
            data.len(),
            &data[..data.len().min(64)]
        );
    }
}
