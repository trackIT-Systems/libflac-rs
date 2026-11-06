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

    let flac = Encoder::new(EncoderConfig::new(1, 16, 44_100)).encode(&pcm(1, 9_000, 7));
    let truncated = &flac[..flac.len() - 10];
    let mut dec = StreamDecoder::new(trickle(truncated)).unwrap();
    let results: Vec<_> = dec.by_ref().collect();
    assert!(matches!(results.last(), Some(Err(DecodeError::Malformed))));
    assert!(dec.next().is_none());
}
