//! Byte-for-byte comparison of `libflac-rs` against the C libFLAC 1.4.3 + libogg
//! 1.3.5, through the public API only: raw frames, complete streams and Ogg FLAC
//! across compression levels, bit depths, channel counts, sample rates, block
//! sizes and every metadata block type.
//!
//! Each test collects all mismatching cases before failing, so one run shows the
//! whole extent of a divergence.

use libflac_rs::{
    CueSheetIndex, CueSheetTrack, Encoder, EncoderConfig, LIBFLAC_VENDOR_STRING, MetadataBlock,
    spaced_seek_points, vorbis_comment_entry,
};
use libflac_rs_oracle as oracle;

const DEPTHS: [u32; 6] = [8, 12, 16, 20, 24, 32];
const LEVELS: std::ops::RangeInclusive<u32> = 0..=8;

/// xorshift64*, so the corpus is identical on every run and platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [-1, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// Interleaved PCM of `blocks` full blocks plus a short final block. Each block
/// holds one kind of content, cycling through the cases that pick different
/// subframe types and code paths: partially correlated tones + noise (LPC,
/// mid/side), digital silence and DC (CONSTANT), full-scale square (extremes, the
/// 32-bit wide paths), low bits zeroed (wasted bits) and white noise (VERBATIM,
/// RICE2 escapes).
fn signal(channels: u32, bps: u32, block: u32, blocks: u32, seed: u64) -> Vec<i32> {
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
struct Failures(Vec<String>);

impl Failures {
    fn check(&mut self, label: impl std::fmt::Display, rust: &[u8], c: &[u8]) {
        if rust != c {
            self.0
                .push(format!("{label}: {}", first_difference(rust, c)));
        }
    }
    fn assert_none(self) {
        assert!(
            self.0.is_empty(),
            "{} case(s) differ from libFLAC:\n  {}",
            self.0.len(),
            self.0.join("\n  ")
        );
    }
}

#[test]
fn vendor_string_matches() {
    assert_eq!(oracle::vendor_string(), LIBFLAC_VENDOR_STRING);
}

/// Raw frames and complete streams (MD5 on) for every level × depth, mono and
/// stereo. Also decodes libFLAC's stream with the Rust decoder.
#[test]
fn levels_depths_channels() {
    let mut failures = Failures::default();
    for channels in [1, 2] {
        for bps in DEPTHS {
            let pcm = signal(channels, bps, 4096, 5, u64::from(bps * 10 + channels));
            for level in LEVELS {
                let cfg = EncoderConfig::new(channels, bps, 44_100).with_compression_level(level);
                let enc = Encoder::new(cfg);
                let label = format!("{channels}ch {bps}-bit level {level}");
                failures.check(
                    format_args!("{label} frames"),
                    &enc.encode_frames(&pcm),
                    &oracle::encode_frames(&cfg, &pcm),
                );
                let c = oracle::encode(&cfg, &pcm);
                failures.check(format_args!("{label} stream"), &enc.encode(&pcm), &c);
                match libflac_rs::decode(&c) {
                    Some(d) if d.interleaved == pcm && d.md5_ok => {}
                    _ => failures
                        .0
                        .push(format!("{label}: Rust decoder rejects libFLAC output")),
                }
            }
        }
    }
    failures.assert_none();
}

/// Ogg FLAC for every level × depth, mono and stereo.
#[test]
fn ogg_levels_depths_channels() {
    let mut failures = Failures::default();
    for channels in [1, 2] {
        for bps in DEPTHS {
            let pcm = signal(channels, bps, 4096, 5, u64::from(bps * 10 + channels));
            for level in LEVELS {
                let cfg = EncoderConfig::new(channels, bps, 44_100).with_compression_level(level);
                failures.check(
                    format_args!("{channels}ch {bps}-bit level {level} ogg"),
                    &Encoder::new(cfg).encode_ogg(&pcm, 0x1234_5678),
                    &oracle::encode_ogg(&cfg, &pcm, 0x1234_5678),
                );
            }
        }
    }
    failures.assert_none();
}

/// Sample rates through the STREAMINFO / frame-header rate codes, including the
/// high rates outside the streamable subset; MD5 on and off.
#[test]
fn sample_rates() {
    let mut failures = Failures::default();
    for rate in [
        8_000, 44_100, 48_000, 96_000, 192_000, 384_000, 500_000, 655_350,
    ] {
        for channels in [1, 2] {
            for bps in [16, 24] {
                let pcm = signal(channels, bps, 4096, 3, u64::from(rate + bps));
                for level in [0, 5, 8] {
                    for md5 in [true, false] {
                        let cfg = EncoderConfig::new(channels, bps, rate)
                            .with_compression_level(level)
                            .with_md5(md5);
                        let label =
                            format!("{rate} Hz {channels}ch {bps}-bit level {level} md5 {md5}");
                        let enc = Encoder::new(cfg);
                        failures.check(
                            format_args!("{label} stream"),
                            &enc.encode(&pcm),
                            &oracle::encode(&cfg, &pcm),
                        );
                        failures.check(
                            format_args!("{label} ogg"),
                            &enc.encode_ogg(&pcm, 7),
                            &oracle::encode_ogg(&cfg, &pcm, 7),
                        );
                    }
                }
            }
        }
    }
    failures.assert_none();
}

/// Block sizes, including CHD's configuration and sizes at the LPC-order limits.
#[test]
fn block_sizes() {
    let mut failures = Failures::default();
    for block in [16, 32, 192, 576, 1152, 2048, 4096, 4608, 16_384, 65_535] {
        let blocks = if block > 8192 { 2 } else { 5 };
        let pcm = signal(2, 16, block, blocks, u64::from(block));
        let chd = EncoderConfig::chd(block);
        failures.check(
            format_args!("CHD block {block}"),
            &Encoder::new(chd).encode_frames(&pcm),
            &oracle::encode_frames(&chd, &pcm),
        );
        for level in [0, 3, 6] {
            let cfg = EncoderConfig::new(2, 16, 44_100)
                .with_block_size(block)
                .with_compression_level(level);
            failures.check(
                format_args!("block {block} level {level}"),
                &Encoder::new(cfg).encode(&pcm),
                &oracle::encode(&cfg, &pcm),
            );
        }
    }
    failures.assert_none();
}

/// 3–8 channels (independent coding; mid/side is stereo-only).
#[test]
fn multichannel() {
    let mut failures = Failures::default();
    for channels in 3..=8 {
        for bps in [16, 24] {
            let pcm = signal(channels, bps, 4096, 3, u64::from(channels * 100 + bps));
            for level in [0, 5, 8] {
                let cfg = EncoderConfig::new(channels, bps, 48_000).with_compression_level(level);
                failures.check(
                    format_args!("{channels}ch {bps}-bit level {level}"),
                    &Encoder::new(cfg).encode(&pcm),
                    &oracle::encode(&cfg, &pcm),
                );
            }
        }
    }
    failures.assert_none();
}

/// Inputs shorter than one block, down to a single sample, and no samples at all.
#[test]
fn short_inputs() {
    let mut failures = Failures::default();
    for samples in [0usize, 1, 2, 15, 16, 17, 100, 4095] {
        for channels in [1, 2] {
            let pcm: Vec<i32> = signal(channels, 16, 4096, 1, samples as u64 + 3)
                [..samples * channels as usize]
                .to_vec();
            for level in [0, 8] {
                let cfg = EncoderConfig::new(channels, 16, 44_100).with_compression_level(level);
                let enc = Encoder::new(cfg);
                let label = format!("{samples} samples {channels}ch level {level}");
                failures.check(
                    format_args!("{label} stream"),
                    &enc.encode(&pcm),
                    &oracle::encode(&cfg, &pcm),
                );
                failures.check(
                    format_args!("{label} ogg"),
                    &enc.encode_ogg(&pcm, 1),
                    &oracle::encode_ogg(&cfg, &pcm, 1),
                );
            }
        }
    }
    failures.assert_none();
}

/// Every metadata block type, alone (after the VORBIS_COMMENT libFLAC always
/// writes) and all together, at the Echolo shape (mono, 384 kHz) and CD stereo.
#[test]
fn metadata_blocks() {
    let mut mcn = [0u8; 128];
    mcn[..13].copy_from_slice(b"CATALOG012345");
    let cue_indices = [
        CueSheetIndex {
            offset: 0,
            number: 0,
        },
        CueSheetIndex {
            offset: 2000,
            number: 1,
        },
    ];
    let cue_tracks = [
        CueSheetTrack {
            offset: 0,
            number: 1,
            isrc: *b"ABCDE1234567",
            non_audio: false,
            pre_emphasis: false,
            indices: &cue_indices,
        },
        CueSheetTrack {
            offset: 10_000,
            number: 255,
            isrc: [0; 12],
            non_audio: true,
            pre_emphasis: true,
            indices: &[],
        },
    ];
    let cd_index = [CueSheetIndex {
        offset: 0,
        number: 1,
    }];
    let cd_tracks = [
        CueSheetTrack {
            offset: 0,
            number: 1,
            isrc: *b"US1234567890",
            non_audio: false,
            pre_emphasis: false,
            indices: &cd_index,
        },
        CueSheetTrack {
            offset: 176_400,
            number: 170,
            isrc: [0; 12],
            non_audio: false,
            pre_emphasis: false,
            indices: &[],
        },
    ];
    let comments: Vec<String> = [
        ("TITLE", "Morning chorus"),
        ("ARTIST", "trackIT Systems"),
        ("DATE", "2026-11-06T05:30:00Z"),
        ("LOCATION", "Marburg"),
        ("COMMENT", "ünïcödé ✓"),
        ("EMPTY", ""),
    ]
    .iter()
    .map(|(n, v)| vorbis_comment_entry(n, v).unwrap())
    .collect();
    let comments: Vec<&str> = comments.iter().map(String::as_str).collect();
    let picture_data: Vec<u8> = (0..=255).cycle().take(3000).collect();
    let app_data = b"application payload";

    let mut failures = Failures::default();
    for (channels, rate) in [(1, 384_000), (2, 44_100)] {
        let pcm = signal(channels, 16, 4096, 6, u64::from(rate));
        let total = (pcm.len() / channels as usize) as u64;
        let seekpoints = spaced_seek_points(7, total);
        let vc = MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING);
        let tags = || MetadataBlock::VorbisCommentEntries {
            vendor: LIBFLAC_VENDOR_STRING,
            comments: &comments,
        };
        let picture = || MetadataBlock::Picture {
            picture_type: 3,
            mime_type: "image/png",
            description: "front cover",
            width: 640,
            height: 480,
            depth: 24,
            colors: 0,
            data: &picture_data,
        };
        let application = || MetadataBlock::Application {
            id: *b"tIT\x01",
            data: app_data,
        };
        let cuesheet = || MetadataBlock::CueSheet {
            media_catalog_number: &mcn,
            lead_in: 0,
            is_cd: false,
            tracks: &cue_tracks,
        };
        let mut cases: Vec<(&str, Vec<MetadataBlock>)> = vec![
            ("vorbis comment only", vec![vc]),
            ("tags", vec![tags()]),
            ("tags + padding", vec![tags(), MetadataBlock::Padding(8192)]),
            ("padding 0", vec![tags(), MetadataBlock::Padding(0)]),
            ("application", vec![tags(), application()]),
            ("picture", vec![tags(), picture()]),
            (
                "seektable",
                vec![tags(), MetadataBlock::Seektable(&seekpoints)],
            ),
            ("cuesheet", vec![tags(), cuesheet()]),
            (
                "all",
                vec![
                    tags(),
                    MetadataBlock::Seektable(&seekpoints),
                    cuesheet(),
                    application(),
                    picture(),
                    MetadataBlock::Padding(1000),
                ],
            ),
        ];
        if rate == 44_100 && channels == 2 {
            cases.push((
                "CD-DA cuesheet",
                vec![
                    tags(),
                    MetadataBlock::CueSheet {
                        media_catalog_number: &mcn,
                        lead_in: 88_200,
                        is_cd: true,
                        tracks: &cd_tracks,
                    },
                ],
            ));
        }
        for (name, blocks) in &cases {
            for level in [0, 8] {
                for md5 in [true, false] {
                    let cfg = EncoderConfig::new(channels, 16, rate)
                        .with_compression_level(level)
                        .with_md5(md5);
                    failures.check(
                        format_args!("{name} ({channels}ch {rate} Hz level {level} md5 {md5})"),
                        &Encoder::new(cfg).encode_with_metadata(&pcm, blocks),
                        &oracle::encode_with_metadata(&cfg, &pcm, blocks),
                    );
                }
            }
        }
    }
    failures.assert_none();
}
