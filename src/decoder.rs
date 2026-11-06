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

/// Decoded audio frames.
pub struct DecodedFrames {
    /// Interleaved PCM (channel-major within each sample).
    pub interleaved: Vec<i32>,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub sample_rate: u32,
}

/// Decode every audio frame in `data` (the raw frame stream produced by
/// [`Encoder::encode_frames`](crate::Encoder::encode_frames), i.e. no metadata) back
/// to interleaved PCM. Returns `None` on any malformed/truncated input or CRC
/// mismatch.
pub fn decode_frames(data: &[u8]) -> Option<DecodedFrames> {
    let mut br = BitReader::new(data);
    let mut interleaved = Vec::new();
    let mut channels = 0u32;
    let mut bits_per_sample = 0u32;
    let mut sample_rate = 0u32;

    while br.byte_pos() < data.len() {
        let frame = decode_frame(&mut br, data)?;
        channels = frame.channels;
        bits_per_sample = frame.bits_per_sample;
        sample_rate = frame.sample_rate;
        for i in 0..frame.blocksize {
            for ch in &frame.samples {
                interleaved.push(ch[i]);
            }
        }
    }

    Some(DecodedFrames {
        interleaved,
        channels,
        bits_per_sample,
        sample_rate,
    })
}

/// A fully decoded FLAC stream (`fLaC` marker + metadata + frames).
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

/// Decode a complete FLAC stream: the `fLaC` marker, the metadata blocks (only
/// STREAMINFO is interpreted; the rest are skipped), then the audio frames.
/// Verifies the audio MD5 against STREAMINFO when present. `None` on malformed
/// input or any CRC mismatch.
pub fn decode(data: &[u8]) -> Option<DecodedStream> {
    let mut br = BitReader::new(data);
    let (info, seek_points, vorbis_comment) = parse_header(&mut br)?;

    let mut interleaved = Vec::new();
    while br.byte_pos() < data.len() {
        let frame = decode_frame(&mut br, data)?;
        for i in 0..frame.blocksize {
            for ch in &frame.samples {
                interleaved.push(ch[i]);
            }
        }
    }
    // Trim any encoder padding beyond the declared length.
    if info.total_samples > 0 {
        interleaved.truncate(info.total_samples as usize * info.channels as usize);
    }

    let md5_ok = info.md5 == [0u8; 16] || {
        let computed =
            crate::md5::audio_md5(&interleaved, info.bits_per_sample.div_ceil(8) as usize);
        computed == info.md5
    };

    Some(DecodedStream {
        interleaved,
        channels: info.channels,
        bits_per_sample: info.bits_per_sample,
        sample_rate: info.sample_rate,
        total_samples: info.total_samples,
        md5: info.md5,
        md5_ok,
        seek_points,
        vorbis_comment,
    })
}

/// Parse the `fLaC` marker and the metadata blocks (interpreting STREAMINFO,
/// SEEKTABLE and VORBIS_COMMENT, skipping the rest), leaving `br` positioned at the first audio frame.
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
            // STREAMINFO (34 bytes).
            let _min_bs = br.read_raw_u32(16)?;
            let _max_bs = br.read_raw_u32(16)?;
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
            info = Some(StreamInfo {
                sample_rate,
                channels,
                bits_per_sample,
                total_samples,
                md5,
            });
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

/// The result of a [`decode_seek`]: PCM from `first_sample` to the end of the
/// stream.
pub struct SeekResult {
    /// Interleaved PCM starting at `first_sample`.
    pub interleaved: Vec<i32>,
    /// The first sample number the PCM begins at (the requested target).
    pub first_sample: u64,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub sample_rate: u32,
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
/// to the end of the stream. `None` on malformed input, a CRC mismatch, or
/// `target_sample >= total_samples` (when the total is known). With no seektable it
/// still works by decoding from the first frame.
pub fn decode_seek(data: &[u8], target_sample: u64) -> Option<SeekResult> {
    let mut br = BitReader::new(data);
    let (info, seek_points, _) = parse_header(&mut br)?;
    if info.total_samples != 0 && target_sample >= info.total_samples {
        return None;
    }
    let audio_offset = br.byte_pos();

    let (start_sample, start_offset) = seek_start(&seek_points, target_sample);
    let start_byte = audio_offset.checked_add(start_offset as usize)?;
    let frames = data.get(start_byte..)?;

    let mut fbr = BitReader::new(frames);
    let mut current = start_sample;
    let mut interleaved = Vec::new();
    while fbr.byte_pos() < frames.len() {
        let frame = decode_frame(&mut fbr, frames)?;
        let frame_end = current + frame.blocksize as u64;
        // Emit only the part of this frame at or after the target sample.
        if frame_end > target_sample {
            let skip = target_sample.saturating_sub(current) as usize;
            for i in skip..frame.blocksize {
                for ch in &frame.samples {
                    interleaved.push(ch[i]);
                }
            }
        }
        current = frame_end;
    }
    if info.total_samples != 0 {
        let want = (info.total_samples - target_sample) as usize * info.channels as usize;
        interleaved.truncate(want);
    }

    Some(SeekResult {
        interleaved,
        first_sample: target_sample,
        channels: info.channels,
        bits_per_sample: info.bits_per_sample,
        sample_rate: info.sample_rate,
    })
}

/// Decode a complete **Ogg FLAC** stream: `OggS` pages → FLAC packets → PCM. Parses
/// the BOS mapping packet (`0x7F"FLAC"` + version + header count + `"fLaC"` +
/// STREAMINFO), concatenates the audio-frame packets, and decodes them, verifying the
/// audio MD5 against STREAMINFO. `None` on malformed Ogg (a bad page CRC or missing
/// mapping) or any frame CRC mismatch. (Ogg FLAC carries no seektable, so
/// `seek_points` is empty; the VORBIS_COMMENT packet is parsed.)
pub fn decode_ogg(data: &[u8]) -> Option<DecodedStream> {
    let packets = crate::ogg::read_packets(data)?;
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
    let _min_bs = sbr.read_raw_u32(16)?;
    let _max_bs = sbr.read_raw_u32(16)?;
    let _min_fs = sbr.read_raw_u32(24)?;
    let _max_fs = sbr.read_raw_u32(24)?;
    let sample_rate = sbr.read_raw_u32(20)?;
    let channels = sbr.read_raw_u32(3)? + 1;
    let bits_per_sample = sbr.read_raw_u32(5)? + 1;
    let total_samples = sbr.read_raw_u64(36)?;
    let mut md5 = [0u8; 16];
    for b in &mut md5 {
        *b = sbr.read_raw_u32(8)? as u8;
    }

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
    let mut frame_bytes = Vec::new();
    if let Some(idx) = packets[1..].iter().position(|p| p.first() == Some(&0xFF)) {
        for p in &packets[1 + idx..] {
            frame_bytes.extend_from_slice(p);
        }
    }
    let decoded = decode_frames(&frame_bytes)?;
    let mut interleaved = decoded.interleaved;
    if total_samples > 0 {
        interleaved.truncate(total_samples as usize * channels as usize);
    }
    let md5_ok = md5 == [0u8; 16] || {
        crate::md5::audio_md5(&interleaved, bits_per_sample.div_ceil(8) as usize) == md5
    };

    Some(DecodedStream {
        interleaved,
        channels,
        bits_per_sample,
        sample_rate,
        total_samples,
        md5,
        md5_ok,
        seek_points: Vec::new(),
        vorbis_comment,
    })
}

struct StreamInfo {
    sample_rate: u32,
    channels: u32,
    bits_per_sample: u32,
    total_samples: u64,
    md5: [u8; 16],
}

struct Frame {
    blocksize: usize,
    channels: u32,
    bits_per_sample: u32,
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
    // first sample number (UTF-8 u64). We track samples by summing block sizes, so
    // the value is discarded — but the two forms consume different byte counts, so
    // the right one must be read for the rest of the header (and CRC-8) to align.
    if blocking_strategy == 0 {
        let _frame_number = br.read_utf8_u32()?;
    } else {
        let _sample_number = br.read_utf8_u64()?;
    }

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

    Some(Frame {
        blocksize,
        channels,
        bits_per_sample,
        sample_rate,
        samples: undecorrelate(assignment, &chans, blocksize),
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

/// Reverse the stereo decorrelation back to independent `i32` channels.
fn undecorrelate(
    assignment: ChannelAssignment,
    chans: &[Vec<i64>],
    blocksize: usize,
) -> Vec<Vec<i32>> {
    match assignment {
        ChannelAssignment::Independent => chans
            .iter()
            .map(|c| c.iter().map(|&s| s as i32).collect())
            .collect(),
        ChannelAssignment::LeftSide => {
            // ch0 = left, ch1 = side = left - right -> right = left - side.
            let (l, s) = (&chans[0], &chans[1]);
            let left: Vec<i32> = l.iter().map(|&v| v as i32).collect();
            let right: Vec<i32> = (0..blocksize).map(|i| (l[i] - s[i]) as i32).collect();
            vec![left, right]
        }
        ChannelAssignment::RightSide => {
            // ch0 = side = left - right, ch1 = right -> left = right + side.
            let (s, r) = (&chans[0], &chans[1]);
            let left: Vec<i32> = (0..blocksize).map(|i| (r[i] + s[i]) as i32).collect();
            let right: Vec<i32> = r.iter().map(|&v| v as i32).collect();
            vec![left, right]
        }
        ChannelAssignment::MidSide => {
            // ch0 = mid = (L+R)>>1 (LSB dropped), ch1 = side = L-R. The dropped LSB
            // equals side&1, so mid2 = (mid<<1)|(side&1) = L+R; L=(mid2+side)/2.
            let (m, s) = (&chans[0], &chans[1]);
            let mut left = vec![0i32; blocksize];
            let mut right = vec![0i32; blocksize];
            for i in 0..blocksize {
                let side = s[i];
                let mid2 = (m[i] << 1) | (side & 1);
                left[i] = ((mid2 + side) >> 1) as i32;
                right[i] = ((mid2 - side) >> 1) as i32;
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
