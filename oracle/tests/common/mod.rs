//! Helpers shared by the oracle test files.
#![allow(dead_code)]

/// xorshift64*, so the corpus is identical on every run and platform.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [-1, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// Interleaved PCM of `blocks` full blocks plus a short final block. Each block
/// holds one kind of content, cycling through the cases that pick different
/// subframe types and code paths: partially correlated tones + noise (LPC,
/// mid/side), digital silence and DC (CONSTANT), full-scale square (extremes, the
/// 32-bit wide paths), low bits zeroed (wasted bits) and white noise (VERBATIM,
/// RICE2 escapes).
pub fn signal(channels: u32, bps: u32, block: u32, blocks: u32, seed: u64) -> Vec<i32> {
    let ch = channels as usize;
    let block = block as usize;
    let len = block * blocks as usize + block / 3 + 1;
    let max = ((1i64 << (bps - 1)) - 1) as f64;
    let min = -(1i64 << (bps - 1)) as f64;
    let mut rng = Rng(seed | 1);
    let mut out = Vec::with_capacity(len * ch);
    for i in 0..len {
        let kind = (i / block) % 5;
        let t = i as f64;
        let base = 0.45 * (t * 0.013).sin() + 0.2 * (t * 0.171 + 1.0).sin();
        for c in 0..ch {
            let own = 0.1 * (t * (0.05 + 0.031 * c as f64)).sin();
            let v = match kind {
                0 => (base * (1.0 - 0.15 * c as f64) + own + 0.02 * rng.unit()) * max,
                1 => {
                    if c == 1 {
                        0.25 * max
                    } else {
                        0.0
                    }
                }
                2 => {
                    if (i / (37 + c)) % 2 == 0 {
                        max
                    } else {
                        min
                    }
                }
                3 => {
                    let v = ((base + own) * max) as i64;
                    let shift = (bps / 4).min(4);
                    (v >> shift << shift) as f64
                }
                _ => rng.unit() * max,
            };
            out.push(v.round().clamp(min, max) as i32);
        }
    }
    out
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(at) => format!("first difference at byte {at}"),
        None => format!("lengths {} (Rust) vs {} (C)", a.len(), b.len()),
    }
}

#[derive(Default)]
pub struct Failures(pub Vec<String>);

impl Failures {
    pub fn check(&mut self, label: impl std::fmt::Display, rust: &[u8], c: &[u8]) {
        if rust != c {
            self.0
                .push(format!("{label}: {}", first_difference(rust, c)));
        }
    }
    pub fn assert_none(self) {
        assert!(
            self.0.is_empty(),
            "{} case(s) differ from libFLAC:\n  {}",
            self.0.len(),
            self.0.join("\n  ")
        );
    }
}
