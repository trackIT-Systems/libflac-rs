//! Frame header and footer, ported from `FLAC__frame_add_header`
//! (`stream_encoder_framing.c:245`) and the footer in `process_frame_`
//! (`stream_encoder.c:3453`).

use crate::bitwriter::BitWriter;
use crate::format::*;

/// Stereo channel decorrelation choice (and the mono/independent default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelAssignment {
    Independent,
    LeftSide,
    RightSide,
    MidSide,
}

/// Everything needed to emit a frame header. CHD streams are fixed-block-size, so
/// the frame is identified by a frame number (not a sample number).
pub struct FrameHeader {
    pub blocksize: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub channel_assignment: ChannelAssignment,
    pub bits_per_sample: u32,
    pub frame_number: u32,
}

/// Write the frame header including its trailing CRC-8 (`FLAC__frame_add_header`).
/// `bw` must be byte-aligned and empty/aligned so the CRC-8 covers exactly the
/// header bytes.
pub fn write_frame_header(bw: &mut BitWriter, h: &FrameHeader) {
    debug_assert!(bw.is_byte_aligned());

    bw.write_raw_u32(FRAME_HEADER_SYNC, FRAME_HEADER_SYNC_LEN);
    bw.write_raw_u32(0, FRAME_HEADER_RESERVED_LEN);
    // Fixed block size -> frame-number stream -> blocking strategy bit 0.
    bw.write_raw_u32(0, FRAME_HEADER_BLOCKING_STRATEGY_LEN);

    // Block-size code, plus an optional trailing hint byte/half-word.
    let (bs_code, bs_hint_bits) = match h.blocksize {
        192 => (1, 0),
        576 => (2, 0),
        1152 => (3, 0),
        2304 => (4, 0),
        4608 => (5, 0),
        256 => (8, 0),
        512 => (9, 0),
        1024 => (10, 0),
        2048 => (11, 0),
        4096 => (12, 0),
        8192 => (13, 0),
        16384 => (14, 0),
        32768 => (15, 0),
        n if n <= 0x100 => (6, 8),
        _ => (7, 16),
    };
    bw.write_raw_u32(bs_code, FRAME_HEADER_BLOCK_SIZE_LEN);

    // Sample-rate code, plus an optional trailing hint.
    let (sr_code, sr_hint) = match h.sample_rate {
        88200 => (1, SrHint::None),
        176400 => (2, SrHint::None),
        192000 => (3, SrHint::None),
        8000 => (4, SrHint::None),
        16000 => (5, SrHint::None),
        22050 => (6, SrHint::None),
        24000 => (7, SrHint::None),
        32000 => (8, SrHint::None),
        44100 => (9, SrHint::None),
        48000 => (10, SrHint::None),
        96000 => (11, SrHint::None),
        n if n <= 255000 && n % 1000 == 0 => (12, SrHint::Khz),
        n if n <= 655350 && n % 10 == 0 => (14, SrHint::TensHz),
        n if n <= 0xffff => (13, SrHint::Hz),
        _ => (0, SrHint::None),
    };
    bw.write_raw_u32(sr_code, FRAME_HEADER_SAMPLE_RATE_LEN);

    let ca = match h.channel_assignment {
        ChannelAssignment::Independent => h.channels - 1,
        ChannelAssignment::LeftSide => 8,
        ChannelAssignment::RightSide => 9,
        ChannelAssignment::MidSide => 10,
    };
    bw.write_raw_u32(ca, FRAME_HEADER_CHANNEL_ASSIGNMENT_LEN);

    let bps_code = match h.bits_per_sample {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        32 => 7,
        _ => 0,
    };
    bw.write_raw_u32(bps_code, FRAME_HEADER_BITS_PER_SAMPLE_LEN);

    bw.write_raw_u32(0, FRAME_HEADER_ZERO_PAD_LEN);

    bw.write_utf8_u32(h.frame_number);

    if bs_hint_bits != 0 {
        bw.write_raw_u32(h.blocksize - 1, bs_hint_bits);
    }
    match sr_hint {
        SrHint::None => {}
        SrHint::Khz => bw.write_raw_u32(h.sample_rate / 1000, 8),
        SrHint::Hz => bw.write_raw_u32(h.sample_rate, 16),
        SrHint::TensHz => bw.write_raw_u32(h.sample_rate / 10, 16),
    }

    let crc = bw.crc8();
    bw.write_raw_u32(crc as u32, FRAME_HEADER_CRC_LEN);
}

enum SrHint {
    None,
    Khz,
    Hz,
    TensHz,
}

/// Finish a frame: zero-pad to a byte boundary, then append the CRC-16 over the
/// whole frame (`process_frame_`, `stream_encoder.c:3453`).
pub fn write_frame_footer(bw: &mut BitWriter) {
    bw.zero_pad_to_byte_boundary();
    let crc = bw.crc16();
    bw.write_raw_u32(crc as u32, FRAME_FOOTER_CRC_LEN);
}
