//! The Rust decoder against libFLAC 1.5.0's decoder: decoded PCM (including the
//! silence inserted for missing frames), reported gaps vs. `MISSING_FRAME` errors,
//! chained Ogg links, and frames whose samples exceed their bit depth.

use libflac_rs::{
    Encoder, EncoderConfig, Gap, LIBFLAC_VENDOR_STRING, MetadataBlock, SeekPoint, StreamDecoder,
    decode, decode_frames, decode_ogg, decode_ogg_chained, decode_seek,
};
use libflac_rs_oracle::{self as oracle, error};
use std::io::Read;

mod common;
use common::signal;

fn missing_frame_errors(d: &oracle::Decoded) -> usize {
    d.errors
        .iter()
        .filter(|&&e| e == error::MISSING_FRAME)
        .count()
}

/// Plain streams decode to the same PCM as libFLAC's decoder: native, raw frames
/// and Ogg, across levels, depths and channel counts.
#[test]
fn decodes_like_libflac() {
    for (channels, bps) in [(1, 8), (2, 16), (2, 24), (6, 20), (2, 32)] {
        let pcm = signal(channels, bps, 4096, 4, u64::from(channels * 100 + bps));
        for level in [0, 5, 8] {
            let cfg = EncoderConfig::new(channels, bps, 48_000).with_compression_level(level);
            let enc = Encoder::new(cfg);
            let label = format!("{channels}ch {bps}-bit level {level}");

            let flac = enc.encode(&pcm);
            let c = oracle::decode(&flac, false, false);
            assert_eq!(c.links.len(), 1, "{label}");
            assert!(
                c.errors.is_empty(),
                "{label}: libFLAC errors {:?}",
                c.errors
            );
            assert_eq!(
                decode(&flac).unwrap().interleaved,
                c.links[0].interleaved,
                "{label}"
            );

            let frames = enc.encode_frames(&pcm);
            let c = oracle::decode(&frames, false, false);
            assert_eq!(
                decode_frames(&frames).unwrap().interleaved,
                c.links[0].interleaved
            );

            let ogg = enc.encode_ogg(&pcm, 5);
            let c = oracle::decode(&ogg, true, false);
            assert_eq!(
                decode_ogg(&ogg).unwrap().interleaved,
                c.links[0].interleaved
            );
        }
    }
}

/// A stream and the byte range of each of its frames within the audio part.
struct Framed {
    header: Vec<u8>,
    frames: Vec<u8>,
    ranges: Vec<(usize, usize)>,
}

/// Encode `pcm` and find every frame's byte range, from a SEEKTABLE with one
/// point per block (written only to a scratch copy; `header` has none).
fn framed(cfg: EncoderConfig, pcm: &[i32]) -> Framed {
    let enc = Encoder::new(cfg);
    let block = u64::from(enc.config().block_size);
    let total = (pcm.len() / cfg.channels as usize) as u64;
    let targets: Vec<SeekPoint> = (0..total.div_ceil(block))
        .map(|k| SeekPoint {
            sample_number: k * block,
            stream_offset: 0,
            frame_samples: 0,
        })
        .collect();
    let indexed = enc.encode_with_metadata(
        pcm,
        &[
            MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING),
            MetadataBlock::Seektable(&targets),
        ],
    );
    let frames = enc.encode_frames(pcm);
    let offsets: Vec<usize> = decode(&indexed)
        .unwrap()
        .seek_points
        .iter()
        .map(|p| p.stream_offset as usize)
        .chain([frames.len()])
        .collect();
    let plain = enc.encode(pcm);
    Framed {
        header: plain[..plain.len() - frames.len()].to_vec(),
        ranges: offsets.windows(2).map(|w| (w[0], w[1])).collect(),
        frames,
    }
}

impl Framed {
    /// The frames without those in `cut`.
    fn without(&self, cut: std::ops::Range<usize>) -> Vec<u8> {
        self.ranges
            .iter()
            .enumerate()
            .filter(|(k, _)| !cut.contains(k))
            .flat_map(|(_, &(a, b))| self.frames[a..b].iter().copied())
            .collect()
    }
}

/// Streams with frames cut out: the same PCM as libFLAC (silence where it fills,
/// none before the first or after the last frame, capped at 5 s and 50 blocks),
/// and one `Gap` per `MISSING_FRAME` error. Native streams and raw frames.
#[test]
fn missing_frames() {
    struct Case {
        label: &'static str,
        channels: u32,
        rate: u32,
        block: u32,
        blocks: u32,
        cut: std::ops::Range<usize>,
        expected: Option<Gap>,
    }
    let case = |label, channels, rate, block, blocks, cut, expected| Case {
        label,
        channels,
        rate,
        block,
        blocks,
        cut,
        expected,
    };
    let cases = [
        case(
            "one frame",
            2,
            44_100,
            4096,
            10,
            3..4,
            Some(gap(3 * 4096, 4096, 4096)),
        ),
        case(
            "three frames",
            2,
            44_100,
            4096,
            10,
            4..7,
            Some(gap(4 * 4096, 3 * 4096, 3 * 4096)),
        ),
        case("first frame", 2, 44_100, 4096, 10, 0..1, None),
        case("last frame", 1, 44_100, 4096, 10, 10..11, None),
        case(
            "5 s cap",
            1,
            8_000,
            4096,
            20,
            2..14,
            Some(gap(2 * 4096, 12 * 4096, 40_000)),
        ),
        case(
            "50 block cap",
            1,
            44_100,
            32,
            150,
            10..80,
            Some(gap(320, 70 * 32, 50 * 32)),
        ),
    ];
    for Case {
        label,
        channels,
        rate,
        block,
        blocks,
        cut,
        expected,
    } in cases
    {
        let pcm = signal(channels, 16, block, blocks, u64::from(block + rate));
        let cfg = EncoderConfig::new(channels, 16, rate).with_block_size(block);
        let f = framed(cfg, &pcm);
        assert!(
            cut.end <= f.ranges.len(),
            "{label}: only {} frames",
            f.ranges.len()
        );
        let damaged_frames = f.without(cut.clone());
        let damaged = [f.header.clone(), damaged_frames.clone()].concat();

        let c = oracle::decode(&damaged, false, false);
        let rust = decode(&damaged).unwrap();
        assert_eq!(rust.interleaved, c.links[0].interleaved, "{label}");
        assert_eq!(
            rust.gaps,
            expected.into_iter().collect::<Vec<_>>(),
            "{label}"
        );
        assert_eq!(rust.gaps.len(), missing_frame_errors(&c), "{label}");

        // Raw frames: positions from frame numbers without STREAMINFO.
        let c = oracle::decode(&damaged_frames, false, false);
        let rust = decode_frames(&damaged_frames).unwrap();
        assert_eq!(
            rust.interleaved, c.links[0].interleaved,
            "{label} (raw frames)"
        );
        assert_eq!(
            rust.gaps.len(),
            missing_frame_errors(&c),
            "{label} (raw frames)"
        );
    }
}

fn gap(first_sample: u64, samples: u64, filled: u64) -> Gap {
    Gap {
        first_sample,
        samples,
        filled,
    }
}

/// Seeking before a gap fills it like a full decode does; the result is the tail
/// of libFLAC's full decode.
#[test]
fn seek_across_missing_frames() {
    let pcm = signal(2, 16, 4096, 10, 7);
    let f = framed(
        EncoderConfig::new(2, 16, 44_100).with_block_size(4096),
        &pcm,
    );
    let damaged = [f.header.clone(), f.without(5..7)].concat();
    let full = &oracle::decode(&damaged, false, false).links[0].interleaved;
    for target in [0u64, 1000, 4096 * 4 + 17] {
        let seek = decode_seek(&damaged, target).unwrap();
        assert_eq!(seek.first_sample, target);
        assert_eq!(
            seek.interleaved,
            full[target as usize * 2..],
            "target {target}"
        );
        assert_eq!(
            seek.gaps,
            [gap(5 * 4096, 2 * 4096, 2 * 4096)],
            "target {target}"
        );
    }
}

/// Chained Ogg FLAC (links of different formats, one serial number reused):
/// `decode_ogg` returns the first link as libFLAC does by default,
/// `decode_ogg_chained` every link as libFLAC does with chaining on.
#[test]
fn chained_ogg() {
    let parts = [
        (
            EncoderConfig::new(1, 16, 44_100).with_compression_level(5),
            1,
            11,
        ),
        (EncoderConfig::new(2, 24, 96_000), 2, 12),
        (
            EncoderConfig::new(1, 8, 8_000).with_compression_level(0),
            1,
            13,
        ),
    ];
    let mut chained = Vec::new();
    let mut originals = Vec::new();
    for (cfg, serial, seed) in parts {
        let pcm = signal(cfg.channels, cfg.bits_per_sample, 4096, 3, seed);
        chained.extend(Encoder::new(cfg).encode_ogg(&pcm, serial));
        originals.push(pcm);
    }

    let c = oracle::decode(&chained, true, false);
    assert_eq!(c.links.len(), 1);
    let first = decode_ogg(&chained).unwrap();
    assert_eq!(first.interleaved, c.links[0].interleaved);
    assert_eq!(first.interleaved, originals[0]);
    assert!(first.md5_ok);

    let c = oracle::decode(&chained, true, true);
    let links = decode_ogg_chained(&chained).unwrap();
    assert_eq!(links.len(), 3);
    assert_eq!(c.links.len(), 3);
    for ((rust, c), original) in links.iter().zip(&c.links).zip(&originals) {
        assert_eq!(
            (rust.channels, rust.bits_per_sample, rust.sample_rate),
            (c.channels, c.bits_per_sample, c.sample_rate)
        );
        assert_eq!(rust.interleaved, c.interleaved);
        assert_eq!(&rust.interleaved, original);
        assert!(rust.md5_ok);
    }
}

/// A frame whose samples exceed its bit depth: an 8-bit left/side frame with
/// left = 127 and side = -128 reconstructs right = 255. libFLAC reports
/// `OUT_OF_BOUNDS` and drops the frame; the Rust decoder rejects the stream. With
/// side = 0 the same frame is valid in both.
#[test]
fn out_of_bounds_frame() {
    for (side, valid) in [(0i32, true), (-128, false)] {
        let stream = [streaminfo_8bit_stereo(16), left_side_frame(127, side, 16)].concat();
        let c = oracle::decode(&stream, false, false);
        let rust = decode(&stream);
        if valid {
            assert!(c.errors.is_empty(), "{:?}", c.errors);
            assert_eq!(c.links[0].interleaved, [127, 127].repeat(16));
            assert_eq!(rust.unwrap().interleaved, c.links[0].interleaved);
        } else {
            // After dropping the frame libFLAC searches on and loses sync at the end.
            assert_eq!(c.errors.first(), Some(&error::OUT_OF_BOUNDS));
            assert!(c.links.iter().all(|l| l.interleaved.is_empty()));
            assert!(rust.is_none());
        }
    }
}

/// MSB-first bit packing for hand-built streams.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u64, bits: u32) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                self.bytes.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }
    fn align(&mut self) {
        while self.n != 0 {
            self.put(0, 1);
        }
    }
}

/// `fLaC` + a final STREAMINFO for 2-channel 8-bit 44.1 kHz audio, MD5 unset.
fn streaminfo_8bit_stereo(samples: u64) -> Vec<u8> {
    let mut b = Bits::default();
    b.bytes.extend_from_slice(b"fLaC");
    b.put(1, 1); // last metadata block
    b.put(0, 7); // STREAMINFO
    b.put(34, 24);
    b.put(16, 16); // min block size
    b.put(16, 16); // max block size
    b.put(0, 24);
    b.put(0, 24);
    b.put(44_100, 20);
    b.put(1, 3); // channels - 1
    b.put(7, 5); // bits per sample - 1
    b.put(samples, 36);
    b.put(0, 64);
    b.put(0, 64);
    b.bytes
}

/// Frame 0: 8-bit, left/side, CONSTANT subframes `left` (8 bits) and `side`
/// (9 bits), `blocksize` samples (`<= 256`), with valid CRC-8 and CRC-16.
fn left_side_frame(left: i32, side: i32, blocksize: u32) -> Vec<u8> {
    let mut b = Bits::default();
    b.put(0x3FFE, 14); // sync
    b.put(0, 1);
    b.put(0, 1); // fixed block size
    b.put(6, 4); // block size: 8-bit field at the end of the header
    b.put(9, 4); // 44.1 kHz
    b.put(8, 4); // left/side
    b.put(1, 3); // 8 bits per sample
    b.put(0, 1);
    b.put(0, 8); // frame number 0 (UTF-8)
    b.put(u64::from(blocksize - 1), 8);
    let crc = crc8(&b.bytes);
    b.put(u64::from(crc), 8);
    for (value, bits) in [(left, 8), (side, 9)] {
        b.put(0, 8); // CONSTANT, no wasted bits
        b.put(value as u64 & ((1 << bits) - 1), bits);
    }
    b.align();
    let crc = crc16(&b.bytes);
    b.put(u64::from(crc), 16);
    b.bytes
}

fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |mut crc, &byte| {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
        crc
    })
}

fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |mut crc, &byte| {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
        crc
    })
}

/// Hands out at most 5 bytes per `read`, so every frame and page arrives split.
struct Trickle<'a>(&'a [u8]);

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = buf.len().min(5).min(self.0.len());
        buf[..k].copy_from_slice(&self.0[..k]);
        self.0 = &self.0[k..];
        Ok(k)
    }
}

/// All links of `data`, decoded block by block through a trickling reader.
fn stream_links(data: &[u8]) -> Vec<Vec<i32>> {
    let mut dec = StreamDecoder::new(Trickle(data)).unwrap();
    let mut links = Vec::new();
    loop {
        let mut pcm = Vec::new();
        for block in dec.by_ref() {
            pcm.extend(block.unwrap().interleaved);
        }
        links.push(pcm);
        if !dec.next_link().unwrap() {
            return links;
        }
    }
}

/// The streaming decoder produces what libFLAC's decoder does: damaged native
/// streams (silence included) and every link of a chained Ogg stream.
#[test]
fn stream_decoder_matches_libflac() {
    let pcm = signal(2, 16, 4096, 8, 21);
    let f = framed(
        EncoderConfig::new(2, 16, 44_100).with_block_size(4096),
        &pcm,
    );
    let damaged = [f.header.clone(), f.without(2..5)].concat();
    let c = oracle::decode(&damaged, false, false);
    assert_eq!(stream_links(&damaged), [c.links[0].interleaved.clone()]);

    let mut chained = Vec::new();
    for (channels, bps, rate, serial) in [(1, 16, 44_100, 1), (2, 24, 96_000, 2)] {
        let pcm = signal(channels, bps, 4096, 3, u64::from(rate));
        chained
            .extend(Encoder::new(EncoderConfig::new(channels, bps, rate)).encode_ogg(&pcm, serial));
    }
    let c = oracle::decode(&chained, true, true);
    let expected: Vec<Vec<i32>> = c.links.into_iter().map(|l| l.interleaved).collect();
    assert_eq!(stream_links(&chained), expected);
}
