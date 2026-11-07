//! Write a seed corpus of valid streams for the byte-level targets:
//! `cargo run --example seed -- corpus/decode corpus/diff_decode`.

use libflac_rs::{
    CueSheetIndex, CueSheetTrack, Encoder, EncoderConfig, LIBFLAC_VENDOR_STRING, MetadataBlock,
    spaced_seek_points, vorbis_comment_entry,
};
use std::{env, fs, path::Path};

fn signal(channels: u32, bps: u32, frames: usize, seed: u32) -> Vec<i32> {
    let max = ((1i64 << (bps - 1)) - 1) as f64;
    let mut state = seed | 1;
    (0..frames * channels as usize)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let t = i as f64;
            let v = 0.5 * (t * 0.011).sin()
                + 0.2 * (t * 0.13 + seed as f64).sin()
                + 0.05 * (state as f64 / u32::MAX as f64 - 0.5);
            (v * max).round() as i32
        })
        .collect()
}

fn main() {
    let dirs: Vec<String> = env::args().skip(1).collect();
    let mut seeds: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |name: String, bytes: Vec<u8>| seeds.push((name, bytes));

    for (channels, bps, level, block) in [
        (1, 16, 8, 256),
        (2, 16, 5, 512),
        (2, 24, 8, 1152),
        (1, 8, 0, 64),
        (2, 32, 8, 256),
        (6, 20, 3, 128),
        (2, 4, 1, 32),
        (1, 12, 4, 576),
    ] {
        let enc = Encoder::new(
            EncoderConfig::new(channels, bps, 44_100)
                .with_compression_level(level)
                .with_block_size(block),
        );
        let pcm = signal(channels, bps, block as usize * 3 + 17, bps * 7 + channels);
        let tag = format!("{channels}ch{bps}b-l{level}-b{block}");
        add(format!("{tag}.flac"), enc.encode(&pcm));
        add(format!("{tag}.raw"), enc.encode_frames(&pcm));
        add(format!("{tag}.oga"), enc.encode_ogg(&pcm, 3));
    }

    // Every metadata block type.
    let pcm = signal(2, 16, 2000, 9);
    let enc = Encoder::new(EncoderConfig::new(2, 16, 44_100).with_block_size(512));
    let comments = [vorbis_comment_entry("TITLE", "seed").unwrap()];
    let comments: Vec<&str> = comments.iter().map(String::as_str).collect();
    let seek = spaced_seek_points(4, 2000);
    let indices = [CueSheetIndex {
        offset: 0,
        number: 1,
    }];
    let tracks = [
        CueSheetTrack {
            offset: 0,
            number: 1,
            isrc: [0; 12],
            non_audio: false,
            pre_emphasis: false,
            indices: &indices,
        },
        CueSheetTrack {
            offset: 2000,
            number: 255,
            isrc: [0; 12],
            non_audio: false,
            pre_emphasis: false,
            indices: &[],
        },
    ];
    add(
        "metadata.flac".into(),
        enc.encode_with_metadata(
            &pcm,
            &[
                MetadataBlock::VorbisCommentEntries {
                    vendor: LIBFLAC_VENDOR_STRING,
                    comments: &comments,
                },
                MetadataBlock::Seektable(&seek),
                MetadataBlock::CueSheet {
                    media_catalog_number: &[0; 128],
                    lead_in: 0,
                    is_cd: false,
                    tracks: &tracks,
                },
                MetadataBlock::Application {
                    id: *b"seed",
                    data: b"data",
                },
                MetadataBlock::Picture {
                    picture_type: 3,
                    mime_type: "image/png",
                    description: "x",
                    width: 1,
                    height: 1,
                    depth: 24,
                    colors: 0,
                    data: &[1, 2, 3],
                },
                MetadataBlock::Padding(16),
            ],
        ),
    );

    // Chained Ogg, two formats.
    let mut chained = Encoder::new(EncoderConfig::new(1, 16, 44_100).with_block_size(256))
        .encode_ogg(&signal(1, 16, 700, 1), 1);
    chained.extend(
        Encoder::new(EncoderConfig::new(2, 24, 96_000).with_block_size(256))
            .encode_ogg(&signal(2, 24, 700, 2), 2),
    );
    add("chained.oga".into(), chained);

    // A stream with frames cut out (a gap): keep the first and last frame, found
    // by decoding growing prefixes.
    let whole = Encoder::new(
        EncoderConfig::new(1, 16, 44_100)
            .with_block_size(128)
            .with_md5(false),
    )
    .encode_frames(&signal(1, 16, 128 * 4, 5));
    let mut ends = Vec::new();
    let mut start = 0;
    for end in 1..=whole.len() {
        if libflac_rs::decode_frames(&whole[start..end]).is_some() {
            ends.push(end);
            start = end;
        }
    }
    add(
        "gap.raw".into(),
        [&whole[..ends[0]], &whole[ends[2]..]].concat(),
    );

    // A non-standard depth (18 bits): frame headers defer to STREAMINFO.
    add(
        "18bit.flac".into(),
        Encoder::new(EncoderConfig::new(2, 18, 48_000)).encode(&signal(2, 18, 3000, 4)),
    );

    for dir in &dirs {
        fs::create_dir_all(dir).unwrap();
        for (name, bytes) in &seeds {
            fs::write(Path::new(dir).join(name), bytes).unwrap();
        }
    }
    println!("wrote {} seeds to {}", seeds.len(), dirs.join(", "));
}
