//! Encoder input validation: settings, samples and metadata are rejected exactly
//! where libFLAC or the format's fields draw the line, and accepted up to it.

use libflac_rs::{
    CueSheetIndex, CueSheetTrack, EncodeError, Encoder, EncoderConfig, LIBFLAC_VENDOR_STRING,
    MetadataBlock, SeekPoint, decode,
};

const VC: MetadataBlock = MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING);

fn config(channels: u32, bps: u32, rate: u32, block: u32) -> EncoderConfig {
    EncoderConfig::new(channels, bps, rate).with_block_size(block)
}

#[test]
fn settings_are_checked_at_the_format_limits() {
    let ok = |c| Encoder::try_new(c).is_ok();
    assert!(ok(config(1, 4, 0, 16)));
    assert!(ok(config(8, 32, 1_048_575, 65_535)));
    assert!(ok(config(2, 17, 44_100, 0)));
    assert_eq!(
        Encoder::try_new(config(0, 16, 44_100, 0)).err(),
        Some(EncodeError::InvalidChannels(0))
    );
    assert_eq!(
        Encoder::try_new(config(9, 16, 44_100, 0)).err(),
        Some(EncodeError::InvalidChannels(9))
    );
    assert_eq!(
        Encoder::try_new(config(1, 3, 44_100, 0)).err(),
        Some(EncodeError::InvalidBitsPerSample(3))
    );
    assert_eq!(
        Encoder::try_new(config(1, 33, 44_100, 0)).err(),
        Some(EncodeError::InvalidBitsPerSample(33))
    );
    assert_eq!(
        Encoder::try_new(config(1, 16, 1_048_576, 0)).err(),
        Some(EncodeError::InvalidSampleRate(1_048_576))
    );
    assert_eq!(
        Encoder::try_new(config(1, 16, 44_100, 15)).err(),
        Some(EncodeError::InvalidBlockSize(15))
    );
    assert_eq!(
        Encoder::try_new(config(1, 16, 44_100, 65_536)).err(),
        Some(EncodeError::InvalidBlockSize(65_536))
    );
}

#[test]
#[should_panic(expected = "invalid FLAC encoder settings: 9 channels")]
fn new_panics_on_invalid_settings() {
    Encoder::new(config(9, 16, 44_100, 0));
}

#[test]
fn samples_must_fit_the_bit_depth() {
    for bps in [4u32, 8, 16, 17, 24, 31, 32] {
        let (min, max) = (i32::MIN >> (32 - bps), i32::MAX >> (32 - bps));
        let enc = Encoder::new(config(2, bps, 44_100, 0));
        assert!(enc.try_encode(&[min, max]).is_ok(), "{bps}-bit");
        if bps < 32 {
            assert_eq!(
                enc.try_encode_frames(&[0, 0, max + 1, 0]),
                Err(EncodeError::SampleOutOfRange {
                    index: 2,
                    value: max + 1
                }),
                "{bps}-bit"
            );
            assert!(enc.try_encode_ogg(&[min - 1, 0], 1).is_err(), "{bps}-bit");
        }
    }
    let enc = Encoder::new(config(2, 16, 44_100, 0));
    assert_eq!(
        enc.try_encode(&[1, 2, 3]),
        Err(EncodeError::RaggedInput {
            len: 3,
            channels: 2
        })
    );
}

#[test]
#[should_panic(expected = "invalid FLAC encoder input: sample 40000 at index 0")]
fn encode_panics_on_out_of_range_samples() {
    Encoder::new(config(1, 16, 44_100, 0)).encode(&[40_000]);
}

/// Every depth the format allows round-trips (byte-exactness against libFLAC at
/// each depth is checked in the oracle).
#[test]
fn every_bit_depth_round_trips() {
    for bps in 4..=32u32 {
        let max = ((1i64 << (bps - 1)) - 1) as f64;
        let pcm: Vec<i32> = (0..5000)
            .map(|i| ((i as f64 * 0.03).sin() * max).round() as i32)
            .collect();
        let flac = Encoder::new(config(2, bps, 44_100, 1024)).encode(&pcm);
        let decoded = decode(&flac).unwrap_or_else(|| panic!("{bps}-bit"));
        assert_eq!((decoded.bits_per_sample, &decoded.interleaved), (bps, &pcm));
        assert!(decoded.md5_ok);
    }
}

fn metadata_error(blocks: &[MetadataBlock]) -> Option<&'static str> {
    match Encoder::new(config(1, 16, 44_100, 0)).try_encode_with_metadata(&[0; 100], blocks) {
        Ok(_) => None,
        Err(EncodeError::InvalidMetadata { reason, .. }) => Some(reason),
        Err(e) => panic!("unexpected {e}"),
    }
}

#[test]
fn metadata_follows_libflac_and_the_format() {
    let point = |sample_number| SeekPoint {
        sample_number,
        stream_offset: 0,
        frame_samples: 0,
    };
    let placeholder = point(u64::MAX);

    // Accepted.
    assert_eq!(
        metadata_error(&[VC, MetadataBlock::Padding((1 << 24) - 1)]),
        None
    );
    assert_eq!(
        metadata_error(&[
            VC,
            MetadataBlock::Seektable(&[point(0), point(50), placeholder, placeholder])
        ]),
        None
    );
    // Block length field.
    assert!(metadata_error(&[VC, MetadataBlock::Padding(1 << 24)]).is_some());
    let big = vec![0u8; (1 << 24) - 4];
    assert!(
        metadata_error(&[MetadataBlock::Application {
            id: *b"test",
            data: &big
        }])
        .is_some()
    );
    // One SEEKTABLE, ascending.
    let table = [point(0), point(10)];
    assert!(
        metadata_error(&[
            MetadataBlock::Seektable(&table),
            MetadataBlock::Seektable(&table)
        ])
        .is_some()
    );
    assert!(metadata_error(&[MetadataBlock::Seektable(&[point(10), point(5)])]).is_some());
    assert!(metadata_error(&[MetadataBlock::Seektable(&[point(5), point(5)])]).is_some());
    // One VORBIS_COMMENT, legal entries.
    let entries = |comments| MetadataBlock::VorbisCommentEntries {
        vendor: LIBFLAC_VENDOR_STRING,
        comments,
    };
    assert_eq!(metadata_error(&[entries(&["TITLE=x", "EMPTY="])]), None);
    assert!(metadata_error(&[VC, entries(&["A=b"])]).is_some());
    assert!(metadata_error(&[entries(&["no equals sign"])]).is_some());
    assert!(metadata_error(&[entries(&["TI~TLE=x"])]).is_some());
}

#[test]
fn cuesheets_and_pictures_follow_libflac() {
    let index = |number| CueSheetIndex { offset: 0, number };
    fn track<'a>(number: u8, indices: &'a [CueSheetIndex]) -> CueSheetTrack<'a> {
        CueSheetTrack {
            offset: 0,
            number,
            isrc: [0; 12],
            non_audio: false,
            pre_emphasis: false,
            indices,
        }
    }
    fn cue<'a>(tracks: &'a [CueSheetTrack<'a>], is_cd: bool, lead_in: u64) -> MetadataBlock<'a> {
        MetadataBlock::CueSheet {
            media_catalog_number: &[0; 128],
            lead_in,
            is_cd,
            tracks,
        }
    }
    let one = [index(1)];
    assert_eq!(
        metadata_error(&[cue(&[track(1, &one), track(255, &[])], false, 0)]),
        None
    );
    assert!(metadata_error(&[cue(&[], false, 0)]).is_some());
    assert!(metadata_error(&[cue(&[track(0, &one), track(255, &[])], false, 0)]).is_some());
    assert!(metadata_error(&[cue(&[track(1, &[]), track(255, &[])], false, 0)]).is_some());
    assert!(metadata_error(&[cue(&[track(1, &[index(2)]), track(255, &[])], false, 0)]).is_some());
    assert!(
        metadata_error(&[cue(
            &[track(1, &[index(0), index(2)]), track(255, &[])],
            false,
            0
        )])
        .is_some()
    );
    let many: Vec<CueSheetTrack> = (0..256).map(|i| track(1 + (i % 250) as u8, &one)).collect();
    assert!(metadata_error(&[cue(&many, false, 0)]).is_some());
    // CD-DA: lead-in of 2 s, lead-out 170.
    assert_eq!(
        metadata_error(&[cue(&[track(1, &one), track(170, &[])], true, 88_200)]),
        None
    );
    assert!(metadata_error(&[cue(&[track(1, &one), track(170, &[])], true, 44_100)]).is_some());
    assert!(metadata_error(&[cue(&[track(1, &one), track(99, &[])], true, 88_200)]).is_some());

    let picture = |picture_type, mime_type, width| MetadataBlock::Picture {
        picture_type,
        mime_type,
        description: "",
        width,
        height: 32,
        depth: 24,
        colors: 0,
        data: &[1, 2, 3],
    };
    assert_eq!(
        metadata_error(&[picture(3, "image/jpeg", 640), picture(1, "image/png", 32)]),
        None
    );
    assert!(metadata_error(&[picture(3, "image/jp\u{e9}g", 640)]).is_some());
    assert!(metadata_error(&[picture(1, "image/jpeg", 32)]).is_some());
    assert!(metadata_error(&[picture(1, "image/png", 64)]).is_some());
    assert!(metadata_error(&[picture(1, "image/png", 32), picture(1, "image/png", 32)]).is_some());
    assert!(metadata_error(&[picture(2, "image/png", 9), picture(2, "image/png", 9)]).is_some());
}
