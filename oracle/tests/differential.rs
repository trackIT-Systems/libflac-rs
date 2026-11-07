//! Byte-for-byte comparison of `libflac-rs` against the C libFLAC 1.5.0 + libogg
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

mod common;
use common::{Failures, signal};

const DEPTHS: [u32; 6] = [8, 12, 16, 20, 24, 32];
const LEVELS: std::ops::RangeInclusive<u32> = 0..=8;

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

/// Constant signals at 28–33-bit subframes, which libFLAC 1.5.0 codes as CONSTANT
/// (1.4.3: FIXED) unless the value is 0: an odd value leaves no wasted bits, a
/// multiple of 8 leaves 29 bits, and constant L/R give a constant 33-bit side.
#[test]
fn constant_wide_subframes() {
    let mut failures = Failures::default();
    let n = 4096 * 2 + 777;
    let values = [1, -1, 3 << 3, i32::MAX, i32::MIN + 1, 0x1234_5677, 0];
    for level in [0, 1, 4, 5, 8] {
        for &c in &values {
            let cfg = EncoderConfig::new(1, 32, 48_000).with_compression_level(level);
            let pcm = vec![c; n];
            failures.check(
                format_args!("mono 32-bit constant {c} level {level}"),
                &Encoder::new(cfg).encode(&pcm),
                &oracle::encode(&cfg, &pcm),
            );
        }
        for (l, r) in [(i32::MAX, i32::MIN), (7, -8), (i32::MIN + 1, 1), (5, 5)] {
            let cfg = EncoderConfig::new(2, 32, 48_000).with_compression_level(level);
            let pcm: Vec<i32> = (0..n).flat_map(|_| [l, r]).collect();
            failures.check(
                format_args!("stereo 32-bit constant ({l}, {r}) level {level}"),
                &Encoder::new(cfg).encode(&pcm),
                &oracle::encode(&cfg, &pcm),
            );
        }
    }
    failures.assert_none();
}

/// SEEKTABLE templates with targets at and past the end of the stream: libFLAC
/// 1.5.0 turns targets beyond the last sample into placeholders.
#[test]
fn seektable_targets_past_end() {
    let mut failures = Failures::default();
    let pcm = signal(2, 16, 4096, 3, 99);
    let total = (pcm.len() / 2) as u64;
    let point = |sample_number| libflac_rs::SeekPoint {
        sample_number,
        stream_offset: 0,
        frame_samples: 0,
    };
    let templates = [
        vec![point(0), point(total - 1), point(total), point(total + 1)],
        vec![point(5000), point(total + 4096), point(total * 3)],
        spaced_seek_points(12, total * 2),
    ];
    for (i, template) in templates.iter().enumerate() {
        for level in [0, 8] {
            let cfg = EncoderConfig::new(2, 16, 44_100).with_compression_level(level);
            let blocks = [
                MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING),
                MetadataBlock::Seektable(template),
            ];
            failures.check(
                format_args!("template {i} level {level}"),
                &Encoder::new(cfg).encode_with_metadata(&pcm, &blocks),
                &oracle::encode_with_metadata(&cfg, &pcm, &blocks),
            );
        }
    }
    failures.assert_none();
}

/// No block size set: libFLAC picks 1152 for levels without LPC (0–2) and 4096
/// otherwise (`stream_encoder.c:748`); `EncoderConfig::new` leaves it at 0 too.
#[test]
fn default_block_size() {
    let mut failures = Failures::default();
    for channels in [1, 2] {
        let pcm = signal(channels, 16, 1152, 12, u64::from(channels));
        for level in LEVELS {
            let cfg = EncoderConfig::new(channels, 16, 44_100).with_compression_level(level);
            assert_eq!(cfg.block_size, 0);
            let enc = Encoder::new(cfg);
            let expected = if level <= 2 { 1152 } else { 4096 };
            if enc.config().block_size != expected {
                failures.0.push(format!(
                    "{channels}ch level {level}: block size {} instead of {expected}",
                    enc.config().block_size
                ));
            }
            let label = format!("{channels}ch level {level}");
            failures.check(
                format_args!("{label} frames"),
                &enc.encode_frames(&pcm),
                &oracle::encode_frames(&cfg, &pcm),
            );
            failures.check(
                format_args!("{label} stream"),
                &enc.encode(&pcm),
                &oracle::encode(&cfg, &pcm),
            );
            failures.check(
                format_args!("{label} ogg"),
                &enc.encode_ogg(&pcm, 3),
                &oracle::encode_ogg(&cfg, &pcm, 3),
            );
        }
    }
    failures.assert_none();
}

/// Every bit depth the format allows (4–32), not just those with their own
/// frame-header code: frames, streams and Ogg, mono and stereo.
#[test]
fn every_bit_depth() {
    let mut failures = Failures::default();
    for bps in 4..=32 {
        for channels in [1, 2] {
            let pcm = signal(channels, bps, 4096, 3, u64::from(bps * 7 + channels));
            for level in [0, 5, 8] {
                let cfg = EncoderConfig::new(channels, bps, 44_100).with_compression_level(level);
                let enc = Encoder::new(cfg);
                let label = format!("{bps}-bit {channels}ch level {level}");
                failures.check(
                    format_args!("{label} frames"),
                    &enc.encode_frames(&pcm),
                    &oracle::encode_frames(&cfg, &pcm),
                );
                failures.check(
                    format_args!("{label} stream"),
                    &enc.encode(&pcm),
                    &oracle::encode(&cfg, &pcm),
                );
                failures.check(
                    format_args!("{label} ogg"),
                    &enc.encode_ogg(&pcm, 5),
                    &oracle::encode_ogg(&cfg, &pcm, 5),
                );
            }
        }
    }
    failures.assert_none();
}

/// The encoder rejects what libFLAC rejects (settings, samples, metadata) and
/// accepts what it accepts, with identical bytes. libFLAC's field-width limits
/// (a 16 MiB block, 256 cue sheet tracks) fail in its bit writer, which refuses a
/// value wider than its field (`bitwriter.c:357`). One rule is the format's
/// rather than libFLAC's: Vorbis comments that are not `NAME=value`, which
/// libFLAC does not check; only the Rust encoder rejects those.
#[test]
fn validation_matches_libflac() {
    use libflac_rs::{CueSheetIndex, CueSheetTrack, SeekPoint};
    const VC: MetadataBlock = MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING);
    let point = |sample_number| SeekPoint {
        sample_number,
        stream_offset: 0,
        frame_samples: 0,
    };
    let unsorted = [point(4096), point(0)];
    let indices = [CueSheetIndex {
        offset: 0,
        number: 1,
    }];
    let tracks: Vec<CueSheetTrack> = (0..256)
        .map(|i| CueSheetTrack {
            offset: i,
            number: (i % 250 + 1) as u8,
            isrc: [0; 12],
            non_audio: false,
            pre_emphasis: false,
            indices: &indices,
        })
        .collect();
    let mcn = [0u8; 128];
    let big = vec![0u8; 1 << 24];
    let bad_entry = ["not a comment"];

    let pcm16 =
        |n: usize| -> Vec<i32> { (0..n as i32).map(|i| (i * 37) % 30_000 - 15_000).collect() };
    let cfg = |ch, bps, rate, block| EncoderConfig::new(ch, bps, rate).with_block_size(block);
    // (label, settings, samples, metadata, rejected, libFLAC has the rule)
    #[allow(clippy::type_complexity)]
    let cases: Vec<(
        &str,
        EncoderConfig,
        Vec<i32>,
        Vec<MetadataBlock>,
        bool,
        bool,
    )> = vec![
        (
            "1 channel, 4-bit, rate 0, block 16",
            cfg(1, 4, 0, 16),
            [-8, 7, 0, 3].repeat(40),
            vec![VC],
            false,
            true,
        ),
        (
            "8 channels, 32-bit, max rate, block 65535",
            cfg(8, 32, 1_048_575, 65_535),
            [i32::MIN, i32::MAX, 0, 1, -1, 5, 6, 7].repeat(9),
            vec![VC],
            false,
            true,
        ),
        (
            "9 channels",
            cfg(9, 16, 44_100, 0),
            vec![0; 9],
            vec![VC],
            true,
            true,
        ),
        (
            "3 bits",
            cfg(1, 3, 44_100, 0),
            vec![0],
            vec![VC],
            true,
            true,
        ),
        (
            "33 bits",
            cfg(1, 33, 44_100, 0),
            vec![0],
            vec![VC],
            true,
            true,
        ),
        (
            "rate 1048576",
            cfg(1, 16, 1_048_576, 0),
            vec![0],
            vec![VC],
            true,
            true,
        ),
        (
            "block 15",
            cfg(1, 16, 44_100, 15),
            vec![0],
            vec![VC],
            true,
            true,
        ),
        (
            "16-bit sample 32768",
            cfg(1, 16, 44_100, 0),
            [pcm16(500), vec![32_768]].concat(),
            vec![VC],
            true,
            true,
        ),
        (
            "16-bit sample -32769",
            cfg(1, 16, 44_100, 0),
            vec![-32_769],
            vec![VC],
            true,
            true,
        ),
        (
            "4-bit sample 8",
            cfg(1, 4, 44_100, 0),
            vec![8],
            vec![VC],
            true,
            true,
        ),
        (
            "unsorted seek table",
            cfg(1, 16, 44_100, 0),
            pcm16(9000),
            vec![VC, MetadataBlock::Seektable(&unsorted)],
            true,
            true,
        ),
        (
            "two VORBIS_COMMENTs",
            cfg(1, 16, 44_100, 0),
            pcm16(100),
            vec![VC, VC],
            true,
            true,
        ),
        (
            "16 MiB APPLICATION",
            cfg(1, 16, 44_100, 0),
            pcm16(100),
            vec![
                VC,
                MetadataBlock::Application {
                    id: *b"test",
                    data: &big,
                },
            ],
            true,
            true,
        ),
        (
            "256 cue sheet tracks",
            cfg(1, 16, 44_100, 0),
            pcm16(100),
            vec![
                VC,
                MetadataBlock::CueSheet {
                    media_catalog_number: &mcn,
                    lead_in: 0,
                    is_cd: false,
                    tracks: &tracks,
                },
            ],
            true,
            true,
        ),
        (
            "comment without '='",
            cfg(1, 16, 44_100, 0),
            pcm16(100),
            vec![MetadataBlock::VorbisCommentEntries {
                vendor: LIBFLAC_VENDOR_STRING,
                comments: &bad_entry,
            }],
            true,
            false,
        ),
    ];
    for (label, config, pcm, blocks, rejected, libflac_rule) in &cases {
        let rust = Encoder::try_new(*config).and_then(|e| e.try_encode_with_metadata(pcm, blocks));
        let c = oracle::try_encode_with_metadata(config, pcm, blocks);
        assert_eq!(rust.is_err(), *rejected, "{label}: Rust {rust:?}");
        if *libflac_rule {
            assert_eq!(c.is_err(), *rejected, "{label}: libFLAC {c:?}");
        } else {
            assert!(c.is_ok(), "{label}: libFLAC was expected to accept it");
        }
        if !rejected {
            assert_eq!(rust.unwrap(), c.unwrap(), "{label}: bytes differ");
        }
    }
}
