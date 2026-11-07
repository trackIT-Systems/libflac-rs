//! The streaming decoder (`StreamDecoder`), STREAMINFO's sample count as a decode
//! bound, and `decode_with`'s sample limit.

use libflac_rs::{
    DecodeError, DecodeOptions, Encoder, EncoderConfig, Gap, LIBFLAC_VENDOR_STRING, MetadataBlock,
    StreamDecoder, decode, decode_frames, decode_with,
};
use std::io::{self, Read};

/// Hands out at most `step` bytes per `read`, cycling through 1..=step, so frames
/// and pages arrive split at every possible position.
struct Trickle<'a> {
    data: &'a [u8],
    step: usize,
    n: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.n = self.n % self.step + 1;
        let k = self.n.min(buf.len()).min(self.data.len());
        buf[..k].copy_from_slice(&self.data[..k]);
        self.data = &self.data[k..];
        Ok(k)
    }
}

fn trickle(data: &[u8]) -> Trickle<'_> {
    Trickle {
        data,
        step: 7,
        n: 0,
    }
}

fn pcm(channels: u32, samples: usize, seed: i32) -> Vec<i32> {
    (0..samples * channels as usize)
        .map(|i| ((i as i32 * 37 + seed) % 2001) - 1000 + (i as i32 % 13) * 50)
        .collect()
}

/// Every block of the current link, checking positions are contiguous.
fn blocks<R: Read>(dec: &mut StreamDecoder<R>) -> Vec<i32> {
    let mut out = Vec::new();
    let mut next = 0;
    for block in dec.by_ref() {
        let block = block.unwrap();
        assert_eq!(block.first_sample, next);
        assert!(!block.silence);
        next += block.samples();
        out.extend(block.interleaved);
    }
    out
}

#[test]
fn streams_native_raw_and_ogg() {
    let input = pcm(2, 10_000, 1);
    let enc = Encoder::new(EncoderConfig::new(2, 16, 48_000).with_block_size(1024));

    let flac = enc.encode(&input);
    let mut dec = StreamDecoder::new(trickle(&flac)).unwrap();
    assert_eq!(dec.header().total_samples, 10_000);
    assert_eq!(dec.md5_ok(), None);
    assert_eq!(blocks(&mut dec), input);
    assert_eq!(dec.md5_ok(), Some(true));

    let frames = enc.encode_frames(&input);
    let mut dec = StreamDecoder::new(trickle(&frames)).unwrap();
    assert_eq!((dec.header().channels, dec.header().total_samples), (2, 0));
    assert_eq!(blocks(&mut dec), input);

    let ogg = enc.encode_ogg(&input, 3);
    let mut dec = StreamDecoder::new(trickle(&ogg)).unwrap();
    assert_eq!(blocks(&mut dec), input);
    assert_eq!(dec.md5_ok(), Some(true));
    assert!(!dec.next_link().unwrap());
}

#[test]
fn follows_chained_ogg_links_on_request() {
    let first = pcm(1, 5_000, 2);
    let second = pcm(2, 3_000, 3);
    let mut chained = Encoder::new(EncoderConfig::new(1, 16, 44_100)).encode_ogg(&first, 1);
    chained.extend(Encoder::new(EncoderConfig::new(2, 24, 96_000)).encode_ogg(&second, 2));

    let mut dec = StreamDecoder::new(trickle(&chained)).unwrap();
    assert_eq!(blocks(&mut dec), first);
    assert!(dec.next_link().unwrap());
    assert_eq!(
        (dec.header().channels, dec.header().sample_rate),
        (2, 96_000)
    );
    assert_eq!(blocks(&mut dec), second);
    assert_eq!(dec.md5_ok(), Some(true));
    assert!(!dec.next_link().unwrap());

    // Skipping the rest of a link.
    let mut dec = StreamDecoder::new(&chained[..]).unwrap();
    dec.next_block().unwrap();
    assert!(dec.next_link().unwrap());
    assert_eq!(blocks(&mut dec), second);
}

/// Frame `k`'s bytes in a raw frame stream, via `decode_frames` on prefixes.
fn frame_ranges(frames: &[u8], count: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for end in 1..=frames.len() {
        if ranges.len() < count && decode_frames(&frames[start..end]).is_some() {
            ranges.push((start, end));
            start = end;
        }
    }
    ranges
}

#[test]
fn yields_gap_silence_in_previous_block_sizes() {
    let input = pcm(1, 1000 * 6, 4);
    let frames =
        Encoder::new(EncoderConfig::new(1, 16, 44_100).with_block_size(1000)).encode_frames(&input);
    let ranges = frame_ranges(&frames, 6);
    let cut: Vec<u8> = [&frames[..ranges[1].1], &frames[ranges[4].0..]].concat();

    let mut dec = StreamDecoder::new(trickle(&cut)).unwrap();
    let all: Vec<_> = dec.by_ref().map(Result::unwrap).collect();
    let silence: Vec<_> = all.iter().filter(|b| b.silence).collect();
    assert_eq!(silence.len(), 2);
    assert_eq!(
        silence
            .iter()
            .map(|b| (b.first_sample, b.samples()))
            .collect::<Vec<_>>(),
        [(2000, 1000), (3000, 1000)]
    );
    assert!(
        silence
            .iter()
            .all(|b| b.interleaved.iter().all(|&s| s == 0))
    );
    assert_eq!(
        dec.gaps(),
        [Gap {
            first_sample: 2000,
            samples: 2000,
            filled: 2000,
        }]
    );
    // Frames 0 and 1, the silence, then frames 4 and 5.
    assert_eq!(
        all.iter().map(|b| b.first_sample).collect::<Vec<_>>(),
        [0, 1000, 2000, 3000, 4000, 5000]
    );
}

/// Set STREAMINFO's total sample count (36 bits at bytes 21..26 of the stream).
fn set_total_samples(flac: &mut [u8], total: u64) {
    flac[21] = (flac[21] & 0xF0) | ((total >> 32) as u8 & 0x0F);
    flac[22..26].copy_from_slice(&(total as u32).to_be_bytes());
}

/// STREAMINFO's sample count bounds decoding: nothing after it is read, so data
/// beyond the declared end (here garbage) neither costs memory nor fails.
#[test]
fn stops_at_declared_total_samples() {
    let input = pcm(2, 8_000, 5);
    let enc = Encoder::new(
        EncoderConfig::new(2, 16, 44_100)
            .with_block_size(1024)
            .with_md5(false),
    );
    let mut flac = enc.encode_with_metadata(
        &input,
        &[MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING)],
    );
    set_total_samples(&mut flac, 2_500);
    flac.extend_from_slice(&[0x5A; 1000]);

    let decoded = decode(&flac).unwrap();
    assert_eq!(decoded.interleaved, input[..2_500 * 2]);

    let mut dec = StreamDecoder::new(trickle(&flac)).unwrap();
    assert_eq!(blocks(&mut dec), input[..2_500 * 2]);
}

/// `decode_with` stops before exceeding the limit; at exactly the limit it
/// succeeds.
#[test]
fn decode_with_enforces_the_sample_limit() {
    let input = pcm(2, 10_000, 6);
    let flac = Encoder::new(EncoderConfig::new(2, 16, 44_100)).encode(&input);
    let limit = |max| DecodeOptions {
        max_samples: Some(max),
    };
    assert_eq!(
        decode_with(&flac, &limit(10_000)).unwrap().interleaved,
        input
    );
    assert!(matches!(
        decode_with(&flac, &limit(9_999)),
        Err(DecodeError::LimitExceeded)
    ));
    assert_eq!(
        decode_with(&flac, &DecodeOptions::default())
            .unwrap()
            .interleaved,
        input
    );
}

/// A 72-byte stream claiming no length that fills a gap to millions of samples:
/// rejected by the limit rather than allocated.
#[test]
fn decode_with_rejects_a_gap_bomb() {
    let silence = vec![0i32; 65_535 * 40 * 8];
    let frames = Encoder::new(EncoderConfig::new(8, 16, 655_350).with_block_size(65_535))
        .encode_frames(&silence);
    let per_frame = frames.len() / 40;
    let bomb = [&frames[..per_frame], &frames[frames.len() - per_frame..]].concat();
    assert_eq!(bomb.len(), 72);
    let limit = DecodeOptions {
        max_samples: Some(10 * 65_535),
    };
    assert!(matches!(
        decode_with(&bomb, &limit),
        Err(DecodeError::LimitExceeded)
    ));

    // Streamed, the same input comes out in blocks of at most one frame each.
    let sizes: Vec<u64> = StreamDecoder::new(&bomb[..])
        .unwrap()
        .map(|b| b.unwrap().samples())
        .collect();
    assert_eq!(sizes.len(), 40);
    assert!(sizes.iter().all(|&n| n == 65_535));
}

struct Failing;

impl Read for Failing {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("disk on fire"))
    }
}

#[test]
fn reports_io_and_format_errors() {
    assert!(matches!(
        StreamDecoder::new(Failing),
        Err(DecodeError::Io(_))
    ));
    assert!(matches!(
        StreamDecoder::new(&b"RIFF....WAVE"[..]),
        Err(DecodeError::Malformed)
    ));
    assert!(matches!(
        StreamDecoder::new(&[][..]),
        Err(DecodeError::Malformed)
    ));

    // Garbage that is not a truncation still fails.
    let input = pcm(1, 9_000, 7);
    let mut flac =
        Encoder::new(EncoderConfig::new(1, 16, 44_100).with_block_size(1000)).encode(&input);
    let end = flac.len();
    flac[end - 20] ^= 0xFF;
    let mut dec = StreamDecoder::new(&flac[..]).unwrap();
    let results: Vec<_> = dec.by_ref().collect();
    assert!(matches!(results.last(), Some(Err(DecodeError::Malformed))));
    assert!(dec.next().is_none());
}

/// Input that ends mid-frame ends the stream after the last complete frame, as
/// libFLAC does; `truncated` tells, and the MD5 check fails.
#[test]
fn truncated_input_ends_after_the_last_complete_frame() {
    let input = pcm(1, 9_000, 7);
    let enc = Encoder::new(EncoderConfig::new(1, 16, 44_100).with_block_size(1000));
    let flac = enc.encode(&input);
    let cut = &flac[..flac.len() - 10]; // into the last (short) frame
    let mut dec = StreamDecoder::new(trickle(cut)).unwrap();
    assert_eq!(blocks(&mut dec), input[..8_000]);
    assert!(dec.truncated());
    assert_eq!(dec.md5_ok(), Some(false));

    let decoded = decode(cut).unwrap();
    assert_eq!(decoded.interleaved, input[..8_000]);
    assert!(decoded.truncated && !decoded.md5_ok);
    let seek = libflac_rs::decode_seek(cut, 2_500).unwrap();
    assert_eq!(seek.interleaved, input[2_500..8_000]);
    assert!(seek.truncated);
    let raw = enc.encode_frames(&input);
    let frames = decode_frames(&raw[..raw.len() - 3]).unwrap();
    assert!(frames.truncated && frames.interleaved == input[..8_000]);

    // Complete input is not truncated.
    let mut dec = StreamDecoder::new(&flac[..]).unwrap();
    blocks(&mut dec);
    assert!(!dec.truncated());
}

/// Ogg: a page cut off at the end ends the stream; junk or a damaged page
/// between pages is skipped up to the next page, as libogg does.
#[test]
fn ogg_truncation_and_junk_are_tolerated() {
    let input = pcm(2, 6_000, 8);
    let ogg =
        Encoder::new(EncoderConfig::new(2, 16, 44_100).with_block_size(512)).encode_ogg(&input, 4);

    let cut = &ogg[..ogg.len() - 7];
    let mut dec = StreamDecoder::new(trickle(cut)).unwrap();
    let decoded = blocks(&mut dec);
    assert!(dec.truncated());
    assert!(input.starts_with(&decoded) && decoded.len() < input.len());

    // Junk between pages (after the first) and a page with a bad CRC are skipped.
    let first = 27
        + ogg[26] as usize
        + ogg[27..27 + ogg[26] as usize]
            .iter()
            .map(|&b| b as usize)
            .sum::<usize>();
    let mut bad_page = ogg[..first].to_vec();
    bad_page[first - 1] ^= 1; // a copy of the first page with a bad CRC
    let wrapped = [&ogg[..first], b"junk OggX", &bad_page, &ogg[first..]].concat();
    let mut dec = StreamDecoder::new(&wrapped[..]).unwrap();
    assert_eq!(blocks(&mut dec), input);
    assert!(!dec.truncated());
}

/// Metadata handling as in libFLAC: a SEEKTABLE whose length is not a multiple of
/// 18 stops decoding; a stream without STREAMINFO decodes like raw frames; ID3v2
/// tags before `fLaC` are skipped.
#[test]
fn metadata_quirks_match_libflac() {
    let input = pcm(2, 5_000, 9);
    let enc = Encoder::new(EncoderConfig::new(2, 16, 44_100).with_block_size(1024));
    let flac = enc.encode_with_metadata(
        &input,
        &[
            MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING),
            MetadataBlock::Padding(19),
        ],
    );
    // STREAMINFO (4+34) + VORBIS_COMMENT header and body, then PADDING.
    let vc_len = u32::from_be_bytes([0, flac[43], flac[44], flac[45]]) as usize;
    let padding = 4 + 4 + 34 + 4 + vc_len;
    assert_eq!(flac[padding] & 0x7F, 1);

    // The 19-byte PADDING turned into a SEEKTABLE: libFLAC gives up.
    let mut bad_table = flac.clone();
    bad_table[padding] = (bad_table[padding] & 0x80) | 3;
    assert!(decode(&bad_table).is_none());

    // STREAMINFO's type changed to APPLICATION: no STREAMINFO, so the format comes
    // from the frames and nothing bounds the length.
    let mut no_info = flac.clone();
    no_info[4] = (no_info[4] & 0x80) | 2;
    let decoded = decode(&no_info).unwrap();
    assert_eq!(decoded.interleaved, input);
    assert_eq!((decoded.channels, decoded.total_samples), (2, 0));

    // ID3v2 tags (10-byte header with a syncsafe size of 300) in front.
    let id3 = [
        b"ID3\x04\x00\x00\x00\x00\x02\x2C".as_slice(),
        &[0u8; 300],
        &flac,
    ]
    .concat();
    assert_eq!(decode(&id3).unwrap().interleaved, input);
    let mut dec = StreamDecoder::new(trickle(&id3)).unwrap();
    assert_eq!(blocks(&mut dec), input);
}

/// The scan for the stream start, short APPLICATION blocks, and Ogg handled as a
/// byte stream, all as libFLAC does.
#[test]
fn stream_start_and_ogg_rules_match_libflac() {
    let input = pcm(1, 3_000, 10);
    let enc = Encoder::new(EncoderConfig::new(1, 16, 44_100).with_block_size(512));

    // Junk before a frame sync is skipped (libFLAC's find_metadata_).
    let frames = enc.encode_frames(&input);
    let junk = [&b"f\x00junk"[..], &frames].concat();
    assert_eq!(decode_frames(&frames).unwrap().interleaved, input);
    let mut dec = StreamDecoder::new(&junk[..]).unwrap();
    assert_eq!(blocks(&mut dec), input);

    // An ID3v2 tag must be followed by fLaC.
    let flac = enc.encode(&input);
    let id3 = |after: &[u8]| [&b"ID3\x04\x00\x00\x00\x00\x00\x02xy"[..], after].concat();
    assert!(decode(&id3(&flac)).is_some());
    assert!(decode(&id3(&[b"x", &flac[..]].concat())).is_none());

    // An APPLICATION block shorter than its 4-byte id stops decoding.
    let short_app = [
        &flac[..4],
        &[0x02, 0, 0, 0][..], // APPLICATION, not last, length 0
        &flac[4..],
    ]
    .concat();
    assert!(decode(&short_app).is_none());

    // Ogg: losing the page with the VORBIS_COMMENT packet loses the end of the
    // metadata, so (as in libFLAC) the frames are read as metadata: no audio.
    let ogg = enc.encode_ogg(&input, 6);
    let page_len = |at: usize| {
        let n = ogg[at + 26] as usize;
        27 + n
            + ogg[at + 27..at + 27 + n]
                .iter()
                .map(|&b| b as usize)
                .sum::<usize>()
    };
    let first = page_len(0);
    let second = page_len(first);
    let lost = [&ogg[..first], &ogg[first + second..]].concat();
    assert!(libflac_rs::decode_ogg(&ogg).is_some());
    assert!(libflac_rs::decode_ogg(&lost).is_none());

    // A packet starting with 0x7F must carry a valid mapping header.
    let mut bad_mapping = ogg.clone();
    bad_mapping[27 + ogg[26] as usize + 5] = 2; // mapping major version 2
    let crc_fixed = fix_page_crc(&bad_mapping, 0);
    assert!(libflac_rs::decode_ogg(&crc_fixed).is_none());
}

/// Recompute the CRC of the page at `at` (libogg: polynomial 0x04c11db7, MSB
/// first, initial 0, CRC field zeroed).
fn fix_page_crc(ogg: &[u8], at: usize) -> Vec<u8> {
    let mut out = ogg.to_vec();
    let n = out[at + 26] as usize;
    let end = at
        + 27
        + n
        + out[at + 27..at + 27 + n]
            .iter()
            .map(|&b| b as usize)
            .sum::<usize>();
    out[at + 22..at + 26].fill(0);
    let mut crc = 0u32;
    for &byte in &out[at..end] {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    out[at + 22..at + 26].copy_from_slice(&crc.to_le_bytes());
    out
}
