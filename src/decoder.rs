//! FLAC frame decoder (`stream_decoder.c`): parse audio frames back into PCM.
//!
//! The decoder's contract is *losslessness* — it must reproduce the exact samples
//! the encoder consumed — not byte-parity with libFLAC's decoder internals, so the
//! code is idiomatic. Correctness is established by round-tripping every encoder
//! corpus (`decode_frames(encode_frames(pcm)) == pcm`) and by decoding real
//! libFLAC output. All prediction/restoration runs in `i64` so the 33-bit side
//! channel and any wide predictor sum are exact; the chosen subframes always have
//! `i32`-fitting residuals, so ≤32-bit channels round-trip exactly.

use crate::bitreader::BitReader;
use crate::crc::{crc8, crc16};
use crate::frame::ChannelAssignment;
use crate::metadata::{SEEKPOINT_PLACEHOLDER, SeekPoint};

/// Samples missing between two frames: the next frame starts later than the
/// previous one ended (`ERROR_STATUS_MISSING_FRAME`, `stream_decoder.c:2483`). As
/// libFLAC 1.5.0 does, the decoder fills the gap with silence if both frames have
/// the same sample rate, channel count and bit depth and the earlier frame holds
/// at least 16 samples, but at most 5 seconds and 50 of the earlier frame's
/// blocks. No gap is detected before the first frame or after the last one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    /// Sample number at which the missing samples begin.
    pub first_sample: u64,
    /// Number of missing samples (per channel).
    pub samples: u64,
    /// Number of those filled with silence (per channel); the output is that much
    /// longer.
    pub filled: u64,
}

/// Decoded audio frames.
pub struct DecodedFrames {
    /// Interleaved PCM (channel-major within each sample).
    pub interleaved: Vec<i32>,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub sample_rate: u32,
    /// Missing frames, in stream order (see [`Gap`]).
    pub gaps: Vec<Gap>,
}

/// Decode every audio frame in `data` (the raw frame stream produced by
/// [`Encoder::encode_frames`](crate::Encoder::encode_frames), i.e. no metadata) back
/// to interleaved PCM. Missing frames are reported and filled as described at
/// [`Gap`]. Returns `None` on any malformed/truncated input, a CRC mismatch, or a
/// frame whose samples exceed its bit depth.
pub fn decode_frames(data: &[u8]) -> Option<DecodedFrames> {
    let mut timeline = Timeline::new(None);
    let mut interleaved = Vec::new();
    let mut br = BitReader::new(data);
    let mut format = (0, 0, 0);
    while br.byte_pos() < data.len() {
        let frame = decode_frame(&mut br, data)?;
        format = (frame.channels, frame.bits_per_sample, frame.sample_rate);
        let start = timeline.place(&frame);
        timeline.fill_gap(start, &frame, &mut interleaved);
        push_samples(&frame, 0, &mut interleaved);
    }
    Some(DecodedFrames {
        interleaved,
        channels: format.0,
        bits_per_sample: format.1,
        sample_rate: format.2,
        gaps: timeline.gaps,
    })
}

/// A fully decoded FLAC stream (`fLaC` marker + metadata + frames), or one link of
/// an Ogg FLAC stream.
pub struct DecodedStream {
    pub interleaved: Vec<i32>,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub sample_rate: u32,
    pub total_samples: u64,
    /// The STREAMINFO MD5 (all-zero if the encoder had MD5 off).
    pub md5: [u8; 16],
    /// Whether the decoded audio's MD5 matches STREAMINFO (trivially `true` when
    /// STREAMINFO carries no MD5).
    pub md5_ok: bool,
    /// The seek points from the SEEKTABLE block, if any (empty otherwise). Trailing
    /// placeholder points (sample number `0xFFFF…`) are retained as stored.
    pub seek_points: Vec<SeekPoint>,
    /// The VORBIS_COMMENT block, if any.
    pub vorbis_comment: Option<DecodedVorbisComment>,
    /// Missing frames, in stream order (see [`Gap`]). Filled gaps make the audio
    /// differ from what was encoded, so `md5_ok` is then `false`.
    pub gaps: Vec<Gap>,
}

/// A decoded VORBIS_COMMENT block: the vendor string and the user comments
/// (`NAME=value` entries) in stored order. Non-UTF-8 bytes are replaced with
/// U+FFFD, since other encoders do not always validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedVorbisComment {
    pub vendor: String,
    pub comments: Vec<String>,
}

/// Parse a VORBIS_COMMENT block body: little-endian lengths, vendor string, comment
/// count, then length-prefixed comments. `None` if a length runs past the block.
fn parse_vorbis_comment(body: &[u8]) -> Option<DecodedVorbisComment> {
    fn take<'a>(rest: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, tail) = rest.split_at_checked(n)?;
        *rest = tail;
        Some(head)
    }
    fn u32_le(rest: &mut &[u8]) -> Option<u32> {
        Some(u32::from_le_bytes(take(rest, 4)?.try_into().ok()?))
    }
    fn string(rest: &mut &[u8]) -> Option<String> {
        let len = u32_le(rest)? as usize;
        Some(String::from_utf8_lossy(take(rest, len)?).into_owned())
    }
    let mut rest = body;
    let vendor = string(&mut rest)?;
    let count = u32_le(&mut rest)?;
    let comments = (0..count)
        .map(|_| string(&mut rest))
        .collect::<Option<Vec<_>>>()?;
    Some(DecodedVorbisComment { vendor, comments })
}

/// Decode a complete FLAC stream: the `fLaC` marker, the metadata blocks
/// (STREAMINFO, SEEKTABLE and VORBIS_COMMENT are interpreted, the rest skipped),
/// then the audio frames. Verifies the audio MD5 against STREAMINFO when present.
/// Missing frames are reported and filled as described at [`Gap`]. `None` on
/// malformed input, a CRC mismatch, or a frame whose samples exceed its bit depth.
pub fn decode(data: &[u8]) -> Option<DecodedStream> {
    let mut br = BitReader::new(data);
    let (info, seek_points, vorbis_comment) = parse_header(&mut br)?;
    let start = br.byte_pos();
    let (interleaved, gaps) = decode_audio(&data[start..], &info)?;
    Some(DecodedStream::new(
        info,
        interleaved,
        seek_points,
        vorbis_comment,
        gaps,
    ))
}

impl DecodedStream {
    /// Trim to the declared length and check the MD5.
    fn new(
        info: StreamInfo,
        mut interleaved: Vec<i32>,
        seek_points: Vec<SeekPoint>,
        vorbis_comment: Option<DecodedVorbisComment>,
        gaps: Vec<Gap>,
    ) -> Self {
        // Trim any encoder padding beyond the declared length.
        if info.total_samples > 0 {
            interleaved.truncate(info.total_samples as usize * info.channels as usize);
        }
        let md5_ok = info.md5 == [0u8; 16]
            || crate::md5::audio_md5(&interleaved, info.bits_per_sample.div_ceil(8) as usize)
                == info.md5;
        DecodedStream {
            interleaved,
            channels: info.channels,
            bits_per_sample: info.bits_per_sample,
            sample_rate: info.sample_rate,
            total_samples: info.total_samples,
            md5: info.md5,
            md5_ok,
            seek_points,
            vorbis_comment,
            gaps,
        }
    }
}

/// Decode the audio frames of a stream described by `info`, filling gaps.
fn decode_audio(frames: &[u8], info: &StreamInfo) -> Option<(Vec<i32>, Vec<Gap>)> {
    let mut timeline = Timeline::new(Some(info));
    let mut interleaved = Vec::new();
    let mut br = BitReader::new(frames);
    while br.byte_pos() < frames.len() {
        let frame = decode_frame(&mut br, frames)?;
        let start = timeline.place(&frame);
        timeline.fill_gap(start, &frame, &mut interleaved);
        push_samples(&frame, 0, &mut interleaved);
    }
    Some((interleaved, timeline.gaps))
}

/// Append a frame's samples from index `skip` on, interleaved.
fn push_samples(frame: &Frame, skip: usize, out: &mut Vec<i32>) {
    for i in skip..frame.blocksize {
        for ch in &frame.samples {
            out.push(ch[i]);
        }
    }
}

/// Where frames sit in the stream, and the silence that fills missing ones.
struct Timeline {
    /// Block size of fixed-block-size frames, once known (`fixed_block_size`).
    fixed_block_size: u64,
    /// STREAMINFO's block size when its minimum and maximum agree.
    stream_block_size: Option<u64>,
    /// STREAMINFO's sample rate, for frames whose header defers to it.
    stream_sample_rate: u32,
    /// The previous frame: first sample, block size, rate, channels, bit depth.
    last: Option<(u64, u64, u32, u32, u32)>,
    gaps: Vec<Gap>,
}

impl Timeline {
    fn new(info: Option<&StreamInfo>) -> Self {
        Timeline {
            fixed_block_size: 0,
            stream_block_size: info
                .filter(|i| i.min_blocksize == i.max_blocksize)
                .map(|i| u64::from(i.max_blocksize)),
            stream_sample_rate: info.map_or(0, |i| i.sample_rate),
            last: None,
            gaps: Vec::new(),
        }
    }

    /// The frame's first sample number (`read_frame_header_`,
    /// `stream_decoder.c:2915`): given directly for variable block sizes, else the
    /// frame number times the fixed block size, which comes from STREAMINFO or,
    /// without it, from frame 0. (libFLAC rejects frame numbers when STREAMINFO's
    /// minimum and maximum block sizes differ; this falls back to the frame's own
    /// block size.)
    fn place(&mut self, frame: &Frame) -> u64 {
        let blocksize = frame.blocksize as u64;
        let (sample, next_fixed) = match frame.number {
            FrameNumber::Sample(s) => (s, 0),
            FrameNumber::Frame(n) => {
                let n = u64::from(n);
                if self.fixed_block_size != 0 {
                    (self.fixed_block_size * n, 0)
                } else if let Some(bs) = self.stream_block_size {
                    (bs * n, bs)
                } else if n == 0 {
                    (0, blocksize)
                } else {
                    (blocksize * n, 0)
                }
            }
        };
        if next_fixed != 0 {
            self.fixed_block_size = next_fixed;
        }
        sample
    }

    /// Record a gap before the frame starting at `start` and append its silence
    /// (`stream_decoder.c:2483-2550`), then remember the frame as the previous one.
    fn fill_gap(&mut self, start: u64, frame: &Frame, out: &mut Vec<i32>) {
        let format = self.format(frame);
        if let Some((last_start, last_bs, last_rate, last_ch, last_bps)) = self.last {
            let end = last_start + last_bs;
            if end < start {
                let missing = start - end;
                let filled = if (last_rate, last_ch, last_bps) == format && last_bs >= 16 {
                    missing.min(5 * u64::from(last_rate)).min(50 * last_bs)
                } else {
                    0
                };
                out.resize(out.len() + (filled * u64::from(last_ch)) as usize, 0);
                self.gaps.push(Gap {
                    first_sample: end,
                    samples: missing,
                    filled,
                });
            }
        }
        self.remember(start, frame);
    }

    /// Remember a frame as the previous one without checking for a gap.
    fn remember(&mut self, start: u64, frame: &Frame) {
        let (rate, channels, bps) = self.format(frame);
        self.last = Some((start, frame.blocksize as u64, rate, channels, bps));
    }

    /// Sample rate (resolved against STREAMINFO), channels and bit depth.
    fn format(&self, frame: &Frame) -> (u32, u32, u32) {
        let rate = match frame.sample_rate {
            0 => self.stream_sample_rate,
            r => r,
        };
        (rate, frame.channels, frame.bits_per_sample)
    }
}

/// Parse the `fLaC` marker and the metadata blocks (interpreting STREAMINFO,
/// SEEKTABLE and VORBIS_COMMENT, skipping the rest), leaving `br` positioned at
/// the first audio frame.
fn parse_header(
    br: &mut BitReader,
) -> Option<(StreamInfo, Vec<SeekPoint>, Option<DecodedVorbisComment>)> {
    if br.read_raw_u32(32)? != 0x664c_6143 {
        return None; // "fLaC"
    }
    let mut info: Option<StreamInfo> = None;
    let mut seek_points: Vec<SeekPoint> = Vec::new();
    let mut vorbis_comment = None;
    loop {
        let is_last = br.read_raw_u32(1)? == 1;
        let block_type = br.read_raw_u32(7)?;
        let length = br.read_raw_u32(24)? as usize;
        if block_type == 0 {
            info = Some(parse_streaminfo(br)?);
        } else if block_type == 3 {
            // SEEKTABLE: length / 18 points of (sample u64, offset u64, samples u16).
            let n = length / 18;
            seek_points = Vec::with_capacity(n);
            for _ in 0..n {
                let sample_number = br.read_raw_u64(64)?;
                let stream_offset = br.read_raw_u64(64)?;
                let frame_samples = br.read_raw_u32(16)?;
                seek_points.push(SeekPoint {
                    sample_number,
                    stream_offset,
                    frame_samples,
                });
            }
            // Tolerate a length not divisible by 18 (non-conforming writers).
            br.skip_bytes(length - n * 18)?;
        } else if block_type == 4 {
            // VORBIS_COMMENT
            let start = br.byte_pos();
            br.skip_bytes(length)?;
            vorbis_comment = parse_vorbis_comment(br.bytes_since(start));
        } else {
            br.skip_bytes(length)?;
        }
        if is_last {
            break;
        }
    }
    Some((info?, seek_points, vorbis_comment))
}

/// The 34-byte STREAMINFO body.
fn parse_streaminfo(br: &mut BitReader) -> Option<StreamInfo> {
    let min_blocksize = br.read_raw_u32(16)?;
    let max_blocksize = br.read_raw_u32(16)?;
    let _min_fs = br.read_raw_u32(24)?;
    let _max_fs = br.read_raw_u32(24)?;
    let sample_rate = br.read_raw_u32(20)?;
    let channels = br.read_raw_u32(3)? + 1;
    let bits_per_sample = br.read_raw_u32(5)? + 1;
    let total_samples = br.read_raw_u64(36)?;
    let mut md5 = [0u8; 16];
    for b in &mut md5 {
        *b = br.read_raw_u32(8)? as u8;
    }
    Some(StreamInfo {
        min_blocksize,
        max_blocksize,
        sample_rate,
        channels,
        bits_per_sample,
        total_samples,
        md5,
    })
}

/// The result of a [`decode_seek`]: PCM from `first_sample` to the end of the
/// stream.
pub struct SeekResult {
    /// Interleaved PCM starting at `first_sample`.
    pub interleaved: Vec<i32>,
    /// The sample number the PCM begins at: the requested target, or the first
    /// sample after it when the target lies in a missing frame.
    pub first_sample: u64,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub sample_rate: u32,
    /// Missing frames after `first_sample`, in stream order (see [`Gap`]).
    pub gaps: Vec<Gap>,
}

/// The seek point to start decoding from for `target`: the non-placeholder point
/// with the largest `sample_number` `<= target`, as `(sample_number, stream_offset)`
/// relative to the first audio frame. Falls back to `(0, 0)` (the first frame) when
/// no usable point exists. A linear scan — seektables are small.
fn seek_start(seek_points: &[SeekPoint], target: u64) -> (u64, u64) {
    let mut best = (0u64, 0u64);
    for p in seek_points {
        if p.sample_number != SEEKPOINT_PLACEHOLDER
            && p.sample_number <= target
            && p.sample_number >= best.0
        {
            best = (p.sample_number, p.stream_offset);
        }
    }
    best
}

/// Decode a stream starting at `target_sample`, using the SEEKTABLE (if any) to jump
/// near it before decoding forward. Returns the interleaved PCM from `target_sample`
/// to the end of the stream; frames are placed by their header numbers. As in
/// libFLAC, gaps are filled only after the target is reached. `None` on malformed
/// input, a CRC mismatch, an out-of-range sample, or `target_sample >=
/// total_samples` (when the total is known). With no seektable it still works by
/// decoding from the first frame.
pub fn decode_seek(data: &[u8], target_sample: u64) -> Option<SeekResult> {
    let mut br = BitReader::new(data);
    let (info, seek_points, _) = parse_header(&mut br)?;
    if info.total_samples != 0 && target_sample >= info.total_samples {
        return None;
    }
    let audio_offset = br.byte_pos();

    let (_, start_offset) = seek_start(&seek_points, target_sample);
    let start_byte = audio_offset.checked_add(start_offset as usize)?;
    let frames = data.get(start_byte..)?;

    let mut timeline = Timeline::new(Some(&info));
    let mut fbr = BitReader::new(frames);
    let mut first_sample = None;
    let mut interleaved = Vec::new();
    while fbr.byte_pos() < frames.len() {
        let frame = decode_frame(&mut fbr, frames)?;
        let start = timeline.place(&frame);
        if first_sample.is_none() {
            // Skip frames before the target; emit the part at or after it.
            timeline.remember(start, &frame);
            if start + frame.blocksize as u64 > target_sample {
                let skip = target_sample.saturating_sub(start);
                first_sample = Some(start + skip);
                push_samples(&frame, skip as usize, &mut interleaved);
            }
            continue;
        }
        timeline.fill_gap(start, &frame, &mut interleaved);
        push_samples(&frame, 0, &mut interleaved);
    }
    let first_sample = first_sample.unwrap_or(target_sample);
    if info.total_samples != 0 {
        let want =
            info.total_samples.saturating_sub(first_sample) as usize * info.channels as usize;
        interleaved.truncate(want);
    }

    Some(SeekResult {
        interleaved,
        first_sample,
        channels: info.channels,
        bits_per_sample: info.bits_per_sample,
        sample_rate: info.sample_rate,
        gaps: timeline.gaps,
    })
}

/// Decode an **Ogg FLAC** stream: the first FLAC logical stream in `data`, up to
/// its end-of-stream page, as libFLAC does by default. Pages of other logical
/// streams (other codecs, or later links of a chained file) are ignored; use
/// [`decode_ogg_chained`] for all links. Parses the mapping packet with STREAMINFO
/// and the metadata packets, decodes the frame packets and verifies the audio MD5.
/// `None` on malformed Ogg (a bad page CRC or missing mapping), a frame CRC
/// mismatch, or an out-of-range sample. (Ogg FLAC carries no seektable, so
/// `seek_points` is empty; the VORBIS_COMMENT packet is parsed.)
pub fn decode_ogg(data: &[u8]) -> Option<DecodedStream> {
    let streams = crate::ogg::read_flac_streams(data, 1)?;
    decode_ogg_link(streams.first()?)
}

/// Decode a **chained** Ogg FLAC stream: every FLAC logical stream in `data`, in
/// order, one [`DecodedStream`] per link (libFLAC 1.5.0's
/// `FLAC__stream_decoder_set_decode_chained_stream`). Links may differ in format;
/// a single, unchained stream gives one link. `None` if any link fails as in
/// [`decode_ogg`], or if `data` holds no FLAC stream.
pub fn decode_ogg_chained(data: &[u8]) -> Option<Vec<DecodedStream>> {
    let streams = crate::ogg::read_flac_streams(data, usize::MAX)?;
    if streams.is_empty() {
        return None;
    }
    streams.iter().map(|s| decode_ogg_link(s)).collect()
}

/// Decode the packets of one Ogg FLAC logical stream.
fn decode_ogg_link(packets: &[Vec<u8>]) -> Option<DecodedStream> {
    let bos = packets.first()?;
    // BOS layout: 0x7F | "FLAC" | maj | min | num_headers(2) | "fLaC" | STREAMINFO(38)
    if bos.len() < 13 + 38 || bos[0] != 0x7F || &bos[1..5] != b"FLAC" || &bos[9..13] != b"fLaC" {
        return None;
    }
    let mut sbr = BitReader::new(&bos[13..]);
    let _is_last = sbr.read_raw_u32(1)?;
    if sbr.read_raw_u32(7)? != 0 {
        return None; // first metadata block must be STREAMINFO
    }
    let _len = sbr.read_raw_u32(24)?;
    let info = parse_streaminfo(&mut sbr)?;

    // Audio frames are the packets from the first one with a 0x3FFE sync (high byte
    // 0xFF); the metadata packets (VORBIS_COMMENT, etc.) precede them.
    let first_frame = packets[1..]
        .iter()
        .position(|p| p.first() == Some(&0xFF))
        .map_or(packets.len(), |i| 1 + i);
    // each metadata packet is a block header (type in the low 7 bits) + body
    let vorbis_comment = packets[1..first_frame]
        .iter()
        .find(|p| p.first().is_some_and(|h| h & 0x7F == 4))
        .and_then(|p| parse_vorbis_comment(p.get(4..)?));
    let frame_bytes = packets[first_frame..].concat();
    let (interleaved, gaps) = decode_audio(&frame_bytes, &info)?;
    Some(DecodedStream::new(
        info,
        interleaved,
        Vec::new(),
        vorbis_comment,
        gaps,
    ))
}

struct StreamInfo {
    min_blocksize: u32,
    max_blocksize: u32,
    sample_rate: u32,
    channels: u32,
    bits_per_sample: u32,
    total_samples: u64,
    md5: [u8; 16],
}

/// The number in a frame header: the frame number (fixed block size) or the first
/// sample number (variable block size).
#[derive(Clone, Copy)]
enum FrameNumber {
    Frame(u32),
    Sample(u64),
}

struct Frame {
    number: FrameNumber,
    blocksize: usize,
    channels: u32,
    bits_per_sample: u32,
    /// From the header; 0 means "see STREAMINFO".
    sample_rate: u32,
    /// Per-channel decoded PCM (already un-decorrelated to independent channels).
    samples: Vec<Vec<i32>>,
}

fn decode_frame(br: &mut BitReader, data: &[u8]) -> Option<Frame> {
    let frame_start = br.byte_pos();

    // --- header ---
    if br.read_raw_u32(14)? != 0x3ffe {
        return None; // sync
    }
    let _reserved = br.read_raw_u32(1)?;
    let blocking_strategy = br.read_raw_u32(1)?; // 0 = fixed, 1 = variable block size
    let bs_code = br.read_raw_u32(4)?;
    let sr_code = br.read_raw_u32(4)?;
    let ca_code = br.read_raw_u32(4)?;
    let bps_code = br.read_raw_u32(3)?;
    let _reserved2 = br.read_raw_u32(1)?;
    // Fixed block size carries the frame number (UTF-8 u32); variable carries the
    // first sample number (UTF-8 u64). `Timeline::place` turns either into the
    // frame's position in the stream.
    let number = if blocking_strategy == 0 {
        FrameNumber::Frame(br.read_utf8_u32()?)
    } else {
        FrameNumber::Sample(br.read_utf8_u64()?)
    };

    let blocksize = match bs_code {
        1 => 192,
        2 => 576,
        3 => 1152,
        4 => 2304,
        5 => 4608,
        6 => br.read_raw_u32(8)? as usize + 1,
        7 => br.read_raw_u32(16)? as usize + 1,
        8 => 256,
        9 => 512,
        10 => 1024,
        11 => 2048,
        12 => 4096,
        13 => 8192,
        14 => 16384,
        15 => 32768,
        _ => return None, // 0 reserved
    };
    let sample_rate = match sr_code {
        1 => 88200,
        2 => 176400,
        3 => 192000,
        4 => 8000,
        5 => 16000,
        6 => 22050,
        7 => 24000,
        8 => 32000,
        9 => 44100,
        10 => 48000,
        11 => 96000,
        12 => br.read_raw_u32(8)? * 1000,
        13 => br.read_raw_u32(16)?,
        14 => br.read_raw_u32(16)? * 10,
        _ => 0, // 0 = from STREAMINFO, 15 invalid
    };
    let bits_per_sample = match bps_code {
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return None, // 0 from STREAMINFO, 3 reserved
    };
    let (assignment, channels) = match ca_code {
        0..=7 => (ChannelAssignment::Independent, ca_code + 1),
        8 => (ChannelAssignment::LeftSide, 2),
        9 => (ChannelAssignment::RightSide, 2),
        10 => (ChannelAssignment::MidSide, 2),
        _ => return None,
    };

    // CRC-8 over the header bytes just read (the reader is byte-aligned here).
    let computed_crc8 = crc8(br.bytes_since(frame_start));
    if br.read_raw_u32(8)? as u8 != computed_crc8 {
        return None;
    }

    // --- subframes (i64 channel data) ---
    let mut chans: Vec<Vec<i64>> = Vec::with_capacity(channels as usize);
    for ch in 0..channels {
        let side = matches!(
            (assignment, ch),
            (ChannelAssignment::LeftSide, 1)
                | (ChannelAssignment::RightSide, 0)
                | (ChannelAssignment::MidSide, 1)
        );
        let channel_bps = bits_per_sample + u32::from(side);
        chans.push(decode_subframe(br, blocksize, channel_bps)?);
    }

    // --- footer: CRC-16 over the whole frame so far ---
    br.align_to_byte();
    let computed_crc16 = crc16(br.bytes_since(frame_start));
    if br.read_raw_u32(16)? as u16 != computed_crc16 {
        return None;
    }
    let _ = data; // backing slice is reached through the reader

    // libFLAC 1.5.0 rejects a frame whose samples do not fit its bit depth
    // (`ERROR_STATUS_OUT_OF_BOUNDS`, stream_decoder.c:2455), e.g. a side channel
    // that reconstructs a channel out of range.
    let limit = 1i64 << (bits_per_sample - 1);
    let samples = undecorrelate(assignment, &chans, blocksize);
    if samples.iter().flatten().any(|&v| v < -limit || v >= limit) {
        return None;
    }

    Some(Frame {
        number,
        blocksize,
        channels,
        bits_per_sample,
        sample_rate,
        samples: samples
            .into_iter()
            .map(|c| c.into_iter().map(|v| v as i32).collect())
            .collect(),
    })
}

/// Decode one subframe to its (wasted-bits-restored) `i64` channel signal.
fn decode_subframe(br: &mut BitReader, blocksize: usize, channel_bps: u32) -> Option<Vec<i64>> {
    let header = br.read_raw_u32(8)?;
    let wasted = if header & 1 == 1 {
        br.read_unary()? + 1
    } else {
        0
    };
    let type6 = (header >> 1) & 0x3f;
    let subframe_bps = channel_bps - wasted;

    let mut data = vec![0i64; blocksize];
    if type6 == 0 {
        // CONSTANT
        let v = br.read_signed(subframe_bps)?;
        data.fill(v);
    } else if type6 == 1 {
        // VERBATIM
        for d in data.iter_mut() {
            *d = br.read_signed(subframe_bps)?;
        }
    } else if type6 & 0x20 != 0 {
        // LPC, order = (type6 & 0x1f) + 1
        let order = ((type6 & 0x1f) + 1) as usize;
        for d in data.iter_mut().take(order) {
            *d = br.read_signed(subframe_bps)?;
        }
        let precision = br.read_raw_u32(4)? + 1;
        let shift = sign_extend5(br.read_raw_u32(5)?);
        let mut qlp = vec![0i32; order];
        for c in qlp.iter_mut() {
            *c = br.read_signed(precision)? as i32;
        }
        let residual = decode_residual(br, blocksize, order)?;
        for i in order..blocksize {
            let mut sum = 0i64;
            for (j, &c) in qlp.iter().enumerate() {
                sum += c as i64 * data[i - 1 - j];
            }
            data[i] = residual[i - order] as i64 + (sum >> shift);
        }
    } else if type6 & 0x08 != 0 {
        // FIXED, order = type6 & 0x07
        let order = (type6 & 0x07) as usize;
        for d in data.iter_mut().take(order) {
            *d = br.read_signed(subframe_bps)?;
        }
        let residual = decode_residual(br, blocksize, order)?;
        for i in order..blocksize {
            let r = residual[i - order] as i64;
            data[i] = match order {
                0 => r,
                1 => r + data[i - 1],
                2 => r + 2 * data[i - 1] - data[i - 2],
                3 => r + 3 * data[i - 1] - 3 * data[i - 2] + data[i - 3],
                _ => r + 4 * data[i - 1] - 6 * data[i - 2] + 4 * data[i - 3] - data[i - 4],
            };
        }
    } else {
        return None; // reserved subframe type
    }

    if wasted > 0 {
        for d in data.iter_mut() {
            *d <<= wasted;
        }
    }
    Some(data)
}

/// Decode the partitioned-Rice residual (`blocksize - order` values).
fn decode_residual(br: &mut BitReader, blocksize: usize, order: usize) -> Option<Vec<i32>> {
    let is_rice2 = br.read_raw_u32(2)? == 1;
    let param_len = if is_rice2 { 5 } else { 4 };
    let escape = if is_rice2 { 31 } else { 15 };
    let partition_order = br.read_raw_u32(4)?;
    let partitions = 1usize << partition_order;

    let mut residual = vec![0i32; blocksize - order];
    let mut idx = 0usize;
    for p in 0..partitions {
        let count = if p == 0 {
            (blocksize >> partition_order) - order
        } else {
            blocksize >> partition_order
        };
        let param = br.read_raw_u32(param_len)?;
        if param == escape {
            // Escaped partition: raw `raw_bits`-wide samples (the encoder never
            // emits these, but other encoders may).
            let raw_bits = br.read_raw_u32(5)?;
            for r in &mut residual[idx..idx + count] {
                *r = br.read_signed(raw_bits)? as i32;
            }
        } else {
            br.read_rice_signed_block(&mut residual[idx..idx + count], param)?;
        }
        idx += count;
    }
    Some(residual)
}

/// Reverse the stereo decorrelation back to independent channels, in `i64` so a
/// result outside the bit depth can be detected rather than wrapped.
fn undecorrelate(
    assignment: ChannelAssignment,
    chans: &[Vec<i64>],
    blocksize: usize,
) -> Vec<Vec<i64>> {
    match assignment {
        ChannelAssignment::Independent => chans.to_vec(),
        ChannelAssignment::LeftSide => {
            // ch0 = left, ch1 = side = left - right -> right = left - side.
            let (l, s) = (&chans[0], &chans[1]);
            let right = (0..blocksize).map(|i| l[i] - s[i]).collect();
            vec![l.clone(), right]
        }
        ChannelAssignment::RightSide => {
            // ch0 = side = left - right, ch1 = right -> left = right + side.
            let (s, r) = (&chans[0], &chans[1]);
            let left = (0..blocksize).map(|i| r[i] + s[i]).collect();
            vec![left, r.clone()]
        }
        ChannelAssignment::MidSide => {
            // ch0 = mid = (L+R)>>1 (LSB dropped), ch1 = side = L-R. The dropped LSB
            // equals side&1, so mid2 = (mid<<1)|(side&1) = L+R; L=(mid2+side)/2.
            let (m, s) = (&chans[0], &chans[1]);
            let mut left = vec![0i64; blocksize];
            let mut right = vec![0i64; blocksize];
            for i in 0..blocksize {
                let side = s[i];
                let mid2 = (m[i] << 1) | (side & 1);
                left[i] = (mid2 + side) >> 1;
                right[i] = (mid2 - side) >> 1;
            }
            vec![left, right]
        }
    }
}

/// Sign-extend a 5-bit quantization shift (always non-negative in practice).
fn sign_extend5(v: u32) -> i32 {
    ((v << 27) as i32) >> 27
}

#[cfg(test)]
mod tests {
    use super::{decode, decode_frames, decode_ogg, decode_seek};
    use crate::bitwriter::BitWriter;
    use crate::encoder::{encode, encode_frames, encode_ogg, preset};
    use crate::metadata::{LIBFLAC_VENDOR_STRING, MetadataBlock, spaced_seek_points};

    /// Multi-partial + noise PCM scaled to a `bps`-bit range, `channels` wide.
    fn gen_pcm(seed: u32, n: usize, bps: u32, channels: u32) -> Vec<i32> {
        let mut st = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        let mut rng = || {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            st
        };
        let maxv = ((1i64 << (bps - 1)) - 1) as f64;
        let minv = -(1i64 << (bps - 1)) as f64;
        let scale = (1u64 << (bps - 1)) as f64 / 32768.0;
        let f1 = 0.008 + (rng() >> 22) as f64 / 2.0e6;
        let f2 = 0.05 + (rng() >> 22) as f64 / 1.0e6;
        let a1 = (1500.0 + (rng() >> 19) as f64 / 8.0e3) * scale;
        let a2 = (400.0 + (rng() >> 20) as f64 / 1.0e4) * scale;
        let noise = 250.0 * scale;
        let mut out = Vec::with_capacity(n * channels as usize);
        for i in 0..n {
            for c in 0..channels {
                let p = c as f64 * 0.35;
                let v = a1 * (f1 * i as f64 + p).sin()
                    + a2 * (f2 * i as f64 + p).sin()
                    + noise * ((rng() >> 16) as u16 as i16 as f64 / 32768.0);
                out.push(v.round().clamp(minv, maxv) as i32);
            }
        }
        out
    }

    /// No samples: no frames, and libFLAC's untouched `min_framesize` sentinel
    /// (0xFFFFFF) in STREAMINFO.
    #[test]
    fn empty_stream() {
        let flac = encode(
            &[],
            2,
            16,
            44_100,
            4096,
            &preset(8),
            true,
            &[MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING)],
        );
        assert_eq!(&flac[12..15], &[0xff, 0xff, 0xff]);
        let dec = decode(&flac).expect("empty stream decodes");
        assert!(dec.interleaved.is_empty());
        assert!(dec.md5_ok);
    }

    #[test]
    fn round_trip_all_depths_levels() {
        let bs = 2048u32;
        for &bps in &[8u32, 12, 16, 20, 24, 32] {
            for &ch in &[1u32, 2] {
                for level in [0u32, 2, 5, 8] {
                    for seed in 1..=3u32 {
                        // Non-block-multiple lengths exercise a short final frame.
                        let n = bs as usize + (seed as usize * 173) % 1500;
                        let pcm = gen_pcm(seed, n, bps, ch);
                        let frames = encode_frames(&pcm, ch, bps, 44_100, bs, &preset(level));
                        let dec = decode_frames(&frames).expect("decode failed (CRC/format)");
                        assert_eq!(dec.channels, ch);
                        assert_eq!(dec.bits_per_sample, bps);
                        assert_eq!(dec.sample_rate, 44_100);
                        assert_eq!(
                            dec.interleaved, pcm,
                            "round-trip mismatch [bps {bps} ch {ch} level {level} seed {seed}]"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn vorbis_comments_round_trip() {
        use crate::metadata::{LIBFLAC_VENDOR_STRING, MetadataBlock, vorbis_comment_entry};
        let pcm = gen_pcm(3, 5000, 16, 1);
        let entries = [
            vorbis_comment_entry("TITLE", "2026-11-05 22:14:03").unwrap(),
            vorbis_comment_entry("COMMENT", "Fledermäuse, ünïcödé").unwrap(),
            vorbis_comment_entry("EMPTY", "").unwrap(),
        ];
        let comments: Vec<&str> = entries.iter().map(String::as_str).collect();
        let blocks = [MetadataBlock::VorbisCommentEntries {
            vendor: LIBFLAC_VENDOR_STRING,
            comments: &comments,
        }];
        let expected = Some(super::DecodedVorbisComment {
            vendor: LIBFLAC_VENDOR_STRING.to_string(),
            comments: entries.to_vec(),
        });

        // 384 kHz mono: the bat-recording case this was added for
        let stream = encode(&pcm, 1, 16, 384_000, 4096, &preset(5), true, &blocks);
        let dec = decode(&stream).expect("decode");
        assert_eq!(dec.interleaved, pcm);
        assert_eq!(dec.sample_rate, 384_000);
        assert_eq!(dec.vorbis_comment, expected);

        let ogg = encode_ogg(&pcm, 1, 16, 384_000, 4096, &preset(5), true, &blocks, 7);
        let dec = decode_ogg(&ogg).expect("decode ogg");
        assert_eq!(dec.interleaved, pcm);
        assert_eq!(dec.vorbis_comment, expected);

        // the vendor-only block still decodes, with no comments
        let stream = encode(
            &pcm,
            1,
            16,
            44_100,
            4096,
            &preset(5),
            true,
            &[MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING)],
        );
        assert_eq!(
            decode(&stream).unwrap().vorbis_comment.unwrap().comments,
            Vec::<String>::new()
        );
    }

    #[test]
    fn full_stream_round_trip_with_md5() {
        use crate::encoder::encode;
        use crate::metadata::{LIBFLAC_VENDOR_STRING, MetadataBlock};
        let bs = 2048u32;
        for &bps in &[8u32, 16, 24, 32] {
            for level in [0u32, 8] {
                let pcm = gen_pcm(9, bs as usize + 555, bps, 2);
                // do_md5 = true, the libFLAC vendor string, no padding.
                let stream = encode(
                    &pcm,
                    2,
                    bps,
                    44_100,
                    bs,
                    &preset(level),
                    true,
                    &[MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING)],
                );
                let dec = decode(&stream).expect("decode full stream");
                assert_eq!(dec.interleaved, pcm, "[bps {bps} level {level}] PCM");
                assert_eq!(dec.total_samples, (pcm.len() / 2) as u64);
                assert!(dec.md5_ok, "[bps {bps} level {level}] MD5 mismatch");
            }
        }
    }

    #[test]
    fn round_trip_edge_signals() {
        let bs = 2048u32;
        for &bps in &[16u32, 24, 32] {
            let maxv = ((1i64 << (bps - 1)) - 1) as i32;
            let minv = -(1i64 << (bps - 1)) as i32;
            let cases: &[(&str, Vec<i32>)] = &[
                ("silence", vec![0i32; bs as usize * 2 + 50]), // CONSTANT
                ("dc", vec![1234 << 3; bs as usize + 10]),     // CONSTANT + wasted bits
                ("full_pos", vec![maxv; bs as usize + 7]),
                ("full_neg", vec![minv; bs as usize + 7]),
                ("tiny", vec![5, -5, 5]), // < MAX_FIXED_ORDER -> VERBATIM, short frame
            ];
            for &(name, ref mono) in cases {
                // Duplicate into stereo so mid-side paths run too.
                let stereo: Vec<i32> = mono.iter().flat_map(|&v| [v, v]).collect();
                for level in [0u32, 8] {
                    let frames = encode_frames(&stereo, 2, bps, 44_100, bs, &preset(level));
                    let dec = decode_frames(&frames).expect("decode failed");
                    assert_eq!(dec.interleaved, stereo, "[{name} bps {bps} level {level}]");
                }
            }
        }
    }

    /// SEEKTABLE-driven `seek()`: a full stream with a spaced seektable, sought to a
    /// range of targets (frame starts, mid-frame, the very end), must return PCM
    /// that exactly matches the original from that sample onward. Also covers a
    /// stream with *no* seektable (decode-from-start fallback).
    #[test]
    fn seek_lands_on_exact_sample() {
        let bs = 2048u32;
        let ch = 2u32;
        for &bps in &[16u32, 24] {
            let n = bs as usize * 5 + 777; // ~6 frames, short final
            let total = n as u64;
            let pcm = gen_pcm(5, n, bps, ch);
            let with_seektable = spaced_seek_points(10, total);
            let targets = [
                0u64,
                1,
                bs as u64 - 1,
                bs as u64,
                bs as u64 + 5,
                bs as u64 * 3 + 100,
                total - bs as u64,
                total - 10,
                total - 1,
            ];
            // With and without a seektable (the latter exercises decode-from-start).
            for seektable in [with_seektable.as_slice(), &[]] {
                let blocks = [
                    MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING),
                    MetadataBlock::Seektable(seektable),
                ];
                let nblocks = if seektable.is_empty() {
                    &blocks[..1]
                } else {
                    &blocks[..]
                };
                let stream = encode(&pcm, ch, bps, 44_100, bs, &preset(8), true, nblocks);
                for &target in &targets {
                    let r = decode_seek(&stream, target).expect("seek");
                    assert_eq!(r.first_sample, target);
                    assert_eq!(r.channels, ch);
                    assert_eq!(r.bits_per_sample, bps);
                    let expected = &pcm[target as usize * ch as usize..];
                    assert_eq!(
                        r.interleaved,
                        expected,
                        "[bps {bps} seektable {}] seek to {target}",
                        !seektable.is_empty()
                    );
                }
                // Seeking at/after the end is rejected.
                assert!(decode_seek(&stream, total).is_none());
            }
        }
    }

    /// The encoder only writes fixed-block-size frames, so hand-build a
    /// variable-block-size frame (blocking-strategy bit = 1, a UTF-8 u64 *sample*
    /// number) and confirm the decoder reads the wider header field, keeps CRC-8
    /// alignment, and decodes the subframe. A mono 16-bit CONSTANT frame, block
    /// size 4, sample number `0x123456` (a 4-byte UTF-8 code).
    /// The byte range of every frame in a raw frame stream.
    fn frame_ranges(data: &[u8]) -> Vec<(usize, usize)> {
        let mut br = crate::bitreader::BitReader::new(data);
        let mut ranges = Vec::new();
        while br.byte_pos() < data.len() {
            let start = br.byte_pos();
            super::decode_frame(&mut br, data).expect("valid frame");
            ranges.push((start, br.byte_pos()));
        }
        ranges
    }

    /// A frame cut out of the middle is reported as a gap and filled with silence
    /// (libFLAC 1.5.0's missing-frame handling); one cut first is not filled.
    #[test]
    fn fills_missing_frames() {
        let pcm = gen_pcm(3, 4096 * 6 + 100, 16, 2);
        let frames = encode_frames(&pcm, 2, 16, 44_100, 4096, &preset(5));
        let ranges = frame_ranges(&frames);
        let without = |cut: usize| -> Vec<u8> {
            ranges
                .iter()
                .enumerate()
                .filter(|&(k, _)| k != cut)
                .flat_map(|(_, &(a, b))| frames[a..b].iter().copied())
                .collect()
        };

        let dec = decode_frames(&without(2)).unwrap();
        assert_eq!(
            dec.gaps,
            [super::Gap {
                first_sample: 2 * 4096,
                samples: 4096,
                filled: 4096,
            }]
        );
        let mut expected = pcm.clone();
        expected[2 * 4096 * 2..3 * 4096 * 2].fill(0);
        assert_eq!(dec.interleaved, expected);

        let dec = decode_frames(&without(0)).unwrap();
        assert!(dec.gaps.is_empty());
        assert_eq!(dec.interleaved, pcm[4096 * 2..]);
    }

    /// An 8-bit left/side frame whose right channel (left - side) exceeds 8 bits is
    /// rejected (libFLAC 1.5.0: `OUT_OF_BOUNDS`) instead of wrapping.
    #[test]
    fn rejects_out_of_bounds_samples() {
        let frame = |side: u32| {
            let mut bw = BitWriter::new();
            bw.write_raw_u32(0x3FFE, 14); // sync
            bw.write_raw_u32(0, 2); // reserved, fixed block size
            bw.write_raw_u32(6, 4); // block size: 8-bit field follows
            bw.write_raw_u32(9, 4); // 44.1 kHz
            bw.write_raw_u32(8, 4); // left/side
            bw.write_raw_u32(1, 3); // 8 bits per sample
            bw.write_raw_u32(0, 1); // reserved
            bw.write_raw_u32(0, 8); // frame number 0
            bw.write_raw_u32(16 - 1, 8); // block size 16
            let crc = bw.crc8();
            bw.write_raw_u32(crc as u32, 8);
            bw.write_raw_u32(0x00, 8); // left: CONSTANT 127 (8 bits)
            bw.write_raw_u32(127, 8);
            bw.write_raw_u32(0x00, 8); // side: CONSTANT (9 bits)
            bw.write_raw_u32(side, 9);
            bw.zero_pad_to_byte_boundary();
            let crc = bw.crc16();
            bw.write_raw_u32(crc as u32, 16);
            bw.as_bytes().to_vec()
        };
        assert_eq!(decode_frames(&frame(0)).unwrap().interleaved, [127; 32]);
        // side = -128 (9-bit two's complement): right = 127 + 128 = 255.
        assert!(decode_frames(&frame(0x180)).is_none());
    }

    /// Chained Ogg FLAC: `decode_ogg` takes the first link, `decode_ogg_chained`
    /// all of them, each with its own format.
    #[test]
    fn decodes_chained_ogg() {
        let first = gen_pcm(1, 5000, 16, 1);
        let second = gen_pcm(2, 7000, 24, 2);
        let meta = [MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING)];
        let mut chained = encode_ogg(&first, 1, 16, 44_100, 4096, &preset(5), true, &meta, 7);
        chained.extend(encode_ogg(
            &second,
            2,
            24,
            96_000,
            4096,
            &preset(8),
            true,
            &meta,
            7,
        ));

        let dec = super::decode_ogg(&chained).unwrap();
        assert_eq!((dec.channels, dec.interleaved), (1, first.clone()));

        let links = super::decode_ogg_chained(&chained).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].interleaved, first);
        assert_eq!((links[1].channels, links[1].sample_rate), (2, 96_000));
        assert_eq!(links[1].interleaved, second);
        assert!(links.iter().all(|l| l.md5_ok && l.gaps.is_empty()));
    }

    #[test]
    fn decodes_variable_block_size_frame() {
        let mut bw = BitWriter::new();
        bw.write_raw_u32(0x3FFE, 14); // sync
        bw.write_raw_u32(0, 1); // reserved
        bw.write_raw_u32(1, 1); // blocking strategy = variable
        bw.write_raw_u32(6, 4); // block-size code 6 => 8-bit (blocksize-1) follows
        bw.write_raw_u32(9, 4); // sample-rate code 9 = 44100
        bw.write_raw_u32(0, 4); // channel assignment 0 = mono
        bw.write_raw_u32(4, 3); // sample-size code 4 = 16 bps
        bw.write_raw_u32(0, 1); // reserved
        bw.write_utf8_u64(0x123456); // sample number (4-byte UTF-8)
        bw.write_raw_u32(4 - 1, 8); // block size hint: blocksize - 1 = 3
        let header_crc = bw.crc8(); // CRC-8 over the (byte-aligned) header
        bw.write_raw_u32(header_crc as u32, 8);
        // CONSTANT subframe (header 0x00) holding the value 1000.
        bw.write_raw_u32(0x00, 8);
        bw.write_raw_u32(1000, 16);
        let frame_crc = bw.crc16(); // CRC-16 over the whole frame so far
        bw.write_raw_u32(frame_crc as u32, 16);
        let frame = bw.as_bytes().to_vec();

        let dec = decode_frames(&frame).expect("decode variable-block-size frame");
        assert_eq!(dec.channels, 1);
        assert_eq!(dec.bits_per_sample, 16);
        assert_eq!(dec.sample_rate, 44_100);
        assert_eq!(dec.interleaved, vec![1000i32; 4]);
    }

    /// Ogg FLAC round-trip: `encode_ogg` → `decode_ogg` reproduces the PCM exactly
    /// (with the embedded MD5 verifying), across all depths incl. 32-bit. The
    /// byte-exactness of `encode_ogg` vs libFLAC+libogg is covered by the differential
    /// harness (see `ORACLE.md`); this exercises the pure-Rust page demux + FLAC demap.
    #[test]
    fn ogg_round_trip() {
        for &bps in &[8u32, 16, 20, 24, 32] {
            for level in [0u32, 5, 8] {
                // Non-block-multiple length → a short final frame.
                let pcm = gen_pcm(7, 12_000 + bps as usize * 13, bps, 2);
                let ogg = encode_ogg(
                    &pcm,
                    2,
                    bps,
                    44_100,
                    4096,
                    &preset(level),
                    true,
                    &[MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING)],
                    0x55AA_1234,
                );
                let dec = decode_ogg(&ogg).expect("decode_ogg");
                assert_eq!(
                    dec.interleaved, pcm,
                    "[bps {bps} level {level}] ogg round-trip"
                );
                assert!(dec.md5_ok, "[bps {bps} level {level}] ogg MD5");
                assert_eq!(dec.total_samples, (pcm.len() / 2) as u64);
            }
        }
    }
}
