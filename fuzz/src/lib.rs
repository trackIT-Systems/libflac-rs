//! Shared input generation for the fuzz targets.

use libflac_rs::{DecodeError, EncoderConfig, StreamDecoder};
use libfuzzer_sys::arbitrary::{self, Arbitrary};

/// Decoded output above this many samples (all channels) is not collected: the
/// format legitimately expands tiny inputs enormously, and the in-memory decode
/// functions have no limit by design.
pub const MAX_OUTPUT: u64 = 1 << 22;

/// Encoder settings plus a signal built from segments, so that LPC, fixed and
/// constant subframes all occur, not just the verbatim ones random noise gives.
#[derive(Arbitrary, Debug)]
pub struct EncodeInput {
    channels: u8,
    bits_per_sample: u8,
    sample_rate: u32,
    block_size: u16,
    level: u8,
    md5: bool,
    /// 0: stream, 1: raw frames, 2: Ogg.
    pub mode: u8,
    /// Keep samples as generated instead of clamping them into the bit depth, so
    /// some fall outside it.
    raw: bool,
    segments: Vec<Segment>,
}

#[derive(Arbitrary, Debug)]
struct Segment {
    kind: u8,
    len: u16,
    a: i32,
    b: i32,
}

impl EncodeInput {
    /// Settings within the format's limits.
    pub fn config(&self) -> EncoderConfig {
        EncoderConfig::new(
            1 + u32::from(self.channels % 8),
            4 + u32::from(self.bits_per_sample % 29),
            self.sample_rate % 1_048_576,
        )
        .with_block_size(match self.block_size {
            0..16 => 0,
            b => u32::from(b),
        })
        .with_compression_level(u32::from(self.level % 9))
        .with_md5(self.md5)
    }

    /// Interleaved PCM, at most 16384 samples per channel.
    pub fn pcm(&self) -> Vec<i32> {
        let config = self.config();
        let (channels, bps) = (config.channels as usize, config.bits_per_sample);
        let (min, max) = (
            i64::from(i32::MIN >> (32 - bps)),
            i64::from(i32::MAX >> (32 - bps)),
        );
        let mut out = Vec::new();
        let mut frames = 0usize;
        for seg in &self.segments {
            let len = usize::from(seg.len % 4096) + 1;
            let len = len.min(16_384 - frames);
            let mut state = (seg.a as u64) << 32 | seg.b as u32 as u64 | 1;
            for i in 0..len {
                for c in 0..channels {
                    let (a, b, t) = (
                        i64::from(seg.a),
                        i64::from(seg.b),
                        (i * channels + c) as i64,
                    );
                    let v = match seg.kind % 6 {
                        0 => a + c as i64,
                        1 => a.wrapping_add(b.wrapping_mul(i as i64)) >> 8,
                        2 => {
                            let freq = (b.unsigned_abs() % 10_000) as f64 / 20_000.0;
                            ((t as f64 * freq).sin() * a as f64) as i64
                        }
                        3 => {
                            state ^= state << 13;
                            state ^= state >> 7;
                            state ^= state << 17;
                            (state as i64) >> (64 - bps)
                        }
                        4 => {
                            if (i / (1 + (b.unsigned_abs() % 100) as usize)).is_multiple_of(2) {
                                a
                            } else {
                                -a
                            }
                        }
                        _ => a.wrapping_add(t.wrapping_mul(b)),
                    };
                    out.push(if self.raw {
                        v as i32
                    } else {
                        v.clamp(min, max) as i32
                    });
                }
            }
            frames += len;
            if frames == 16_384 {
                break;
            }
        }
        out
    }
}

/// The first stream (or Ogg link) decoded with [`StreamDecoder`], or `None` if it
/// produces more than [`MAX_OUTPUT`] samples.
pub fn stream_decode(data: &[u8]) -> Option<Result<Vec<i32>, DecodeError>> {
    let mut decoder = match StreamDecoder::new(data) {
        Ok(d) => d,
        Err(e) => return Some(Err(e)),
    };
    let mut pcm = Vec::new();
    for block in decoder.by_ref() {
        match block {
            Ok(block) => {
                pcm.extend_from_slice(&block.interleaved);
                if pcm.len() as u64 > MAX_OUTPUT {
                    return None;
                }
            }
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Ok(pcm))
}
