//! Encoder input validation. Rejects what libFLAC's encoder rejects — the settings
//! and metadata checks of `init_stream_internal_` (`stream_encoder.c:726-916`) and
//! the sample range check of `FLAC__stream_encoder_process_interleaved` — plus
//! values the format's fields cannot hold, so invalid input fails instead of
//! producing a corrupt stream. Everything the format allows is accepted.

use std::fmt;

use crate::encoder::EncoderConfig;
use crate::metadata::{MetadataBlock, SEEKPOINT_PLACEHOLDER};

/// Largest metadata block body: the block header's 24-bit length field.
const MAX_BLOCK_LENGTH: u64 = (1 << 24) - 1;

/// Why the encoder rejected its settings or input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The channel count is not 1–8.
    InvalidChannels(u32),
    /// The bit depth is not 4–32.
    InvalidBitsPerSample(u32),
    /// The sample rate exceeds 1 048 575 Hz.
    InvalidSampleRate(u32),
    /// The block size is not 16–65535 (or 0 for libFLAC's default).
    InvalidBlockSize(u32),
    /// The interleaved input's length is not a multiple of the channel count.
    RaggedInput { len: usize, channels: u32 },
    /// A sample does not fit the bit depth; `index` is its position in the
    /// interleaved input.
    SampleOutOfRange { index: usize, value: i32 },
    /// The metadata block at `index` is one libFLAC rejects or the format cannot
    /// represent.
    InvalidMetadata { index: usize, reason: &'static str },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::InvalidChannels(c) => write!(f, "{c} channels (FLAC allows 1-8)"),
            EncodeError::InvalidBitsPerSample(b) => {
                write!(f, "{b} bits per sample (FLAC allows 4-32)")
            }
            EncodeError::InvalidSampleRate(r) => {
                write!(f, "sample rate {r} Hz (FLAC allows up to 1048575)")
            }
            EncodeError::InvalidBlockSize(b) => {
                write!(
                    f,
                    "block size {b} (FLAC allows 16-65535, or 0 for the default)"
                )
            }
            EncodeError::RaggedInput { len, channels } => {
                write!(
                    f,
                    "{len} interleaved values are not a multiple of {channels} channels"
                )
            }
            EncodeError::SampleOutOfRange { index, value } => {
                write!(
                    f,
                    "sample {value} at index {index} does not fit the bit depth"
                )
            }
            EncodeError::InvalidMetadata { index, reason } => {
                write!(f, "metadata block {index}: {reason}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// The settings checks of libFLAC's `init_stream_internal_`. A block size of 0 is
/// libFLAC's default; compression levels above 8 clamp to 8, as in libFLAC.
pub fn check_config(config: &EncoderConfig) -> Result<(), EncodeError> {
    if !(1..=8).contains(&config.channels) {
        return Err(EncodeError::InvalidChannels(config.channels));
    }
    if !(4..=32).contains(&config.bits_per_sample) {
        return Err(EncodeError::InvalidBitsPerSample(config.bits_per_sample));
    }
    if config.sample_rate > 1_048_575 {
        return Err(EncodeError::InvalidSampleRate(config.sample_rate));
    }
    if config.block_size != 0 && !(16..=65_535).contains(&config.block_size) {
        return Err(EncodeError::InvalidBlockSize(config.block_size));
    }
    Ok(())
}

/// Whole samples for `channels`, each within the signed `bits_per_sample` range
/// (libFLAC's `process_interleaved` check, `sample_min`/`sample_max`).
pub fn check_samples(
    interleaved: &[i32],
    channels: u32,
    bits_per_sample: u32,
) -> Result<(), EncodeError> {
    if interleaved.len() % channels as usize != 0 {
        return Err(EncodeError::RaggedInput {
            len: interleaved.len(),
            channels,
        });
    }
    let (min, max) = (
        i32::MIN >> (32 - bits_per_sample),
        i32::MAX >> (32 - bits_per_sample),
    );
    match interleaved.iter().position(|&s| s < min || s > max) {
        Some(index) => Err(EncodeError::SampleOutOfRange {
            index,
            value: interleaved[index],
        }),
        None => Ok(()),
    }
}

/// The metadata checks of libFLAC's `init_stream_internal_` (one SEEKTABLE and one
/// VORBIS_COMMENT at most; legal seek tables, cue sheets and pictures; at most one
/// standard and one other file icon, the standard one a 32x32 PNG) and of the
/// format's field sizes (block lengths below 16 MiB, at most 255 cue sheet tracks
/// and index points per track, legal Vorbis comment entries).
pub fn check_metadata(blocks: &[MetadataBlock]) -> Result<(), EncodeError> {
    let (mut seektable, mut comments, mut icon, mut other_icon) = (false, false, false, false);
    for (index, block) in blocks.iter().enumerate() {
        let fail = |reason| Err(EncodeError::InvalidMetadata { index, reason });
        if body_length(block) > MAX_BLOCK_LENGTH {
            return fail("longer than 16 MiB, the block length field's maximum");
        }
        match block {
            MetadataBlock::Seektable(points) => {
                if std::mem::replace(&mut seektable, true) {
                    return fail("only one SEEKTABLE is allowed");
                }
                // FLAC__format_seektable_is_legal: ascending, no duplicates except
                // placeholders.
                let ordered = points.windows(2).all(|w| {
                    w[1].sample_number == SEEKPOINT_PLACEHOLDER
                        || w[1].sample_number > w[0].sample_number
                });
                if !ordered {
                    return fail("seek points must be in ascending order without duplicates");
                }
            }
            MetadataBlock::VorbisComment(_) | MetadataBlock::VorbisCommentEntries { .. } => {
                if std::mem::replace(&mut comments, true) {
                    return fail("only one VORBIS_COMMENT is allowed");
                }
                if let MetadataBlock::VorbisCommentEntries { comments, .. } = block {
                    if !comments.iter().all(|c| comment_entry_is_legal(c)) {
                        return fail(
                            "a comment is not NAME=value with a name of ASCII 0x20-0x7D except '='",
                        );
                    }
                }
            }
            MetadataBlock::CueSheet {
                lead_in,
                is_cd,
                tracks,
                ..
            } => {
                if let Err(reason) = cuesheet_is_legal(*lead_in, *is_cd, tracks) {
                    return fail(reason);
                }
            }
            MetadataBlock::Picture {
                picture_type,
                mime_type,
                description,
                width,
                height,
                ..
            } => {
                // FLAC__format_picture_is_legal.
                if !mime_type.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
                    return fail("the MIME type must be printable ASCII");
                }
                if description.contains(['\u{FFFE}', '\u{FFFF}']) {
                    return fail("the description contains U+FFFE or U+FFFF");
                }
                match picture_type {
                    1 => {
                        if std::mem::replace(&mut icon, true) {
                            return fail("only one 32x32 file icon (picture type 1) is allowed");
                        }
                        if !matches!(*mime_type, "image/png" | "-->")
                            || *width != 32
                            || *height != 32
                        {
                            return fail("a file icon (picture type 1) must be a 32x32 PNG");
                        }
                    }
                    2 if std::mem::replace(&mut other_icon, true) => {
                        return fail("only one other file icon (picture type 2) is allowed");
                    }
                    _ => {}
                }
            }
            MetadataBlock::Padding(_) | MetadataBlock::Application { .. } => {}
        }
    }
    Ok(())
}

/// The body length the block would have in bytes.
fn body_length(block: &MetadataBlock) -> u64 {
    let len = |n: usize| n as u64;
    match block {
        MetadataBlock::VorbisComment(vendor) => 8 + len(vendor.len()),
        MetadataBlock::VorbisCommentEntries { vendor, comments } => {
            8 + len(vendor.len()) + comments.iter().map(|c| 4 + len(c.len())).sum::<u64>()
        }
        MetadataBlock::Padding(n) => u64::from(*n),
        MetadataBlock::Application { data, .. } => 4 + len(data.len()),
        MetadataBlock::Seektable(points) => 18 * len(points.len()),
        MetadataBlock::CueSheet { tracks, .. } => {
            396 + tracks
                .iter()
                .map(|t| 36 + 12 * len(t.indices.len()))
                .sum::<u64>()
        }
        MetadataBlock::Picture {
            mime_type,
            description,
            data,
            ..
        } => 32 + len(mime_type.len()) + len(description.len()) + len(data.len()),
    }
}

/// FLAC__format_vorbiscomment_entry_is_legal (`format.c`): a name of ASCII
/// 0x20-0x7D other than `=`, then `=`, then UTF-8 (a `str` always is, except that
/// libFLAC's `utf8len_` also rejects U+FFFE and U+FFFF).
fn comment_entry_is_legal(entry: &str) -> bool {
    match entry.split_once('=') {
        Some((name, value)) => {
            name.bytes().all(|b| (0x20..=0x7D).contains(&b))
                && !value.contains(['\u{FFFE}', '\u{FFFF}'])
        }
        None => false,
    }
}

/// FLAC__format_cuesheet_is_legal (`format.c`), with the CD-DA rules when `is_cd`,
/// plus the 8-bit track and index-point count fields.
fn cuesheet_is_legal(
    lead_in: u64,
    is_cd: bool,
    tracks: &[crate::metadata::CueSheetTrack],
) -> Result<(), &'static str> {
    if is_cd && lead_in < 2 * 44_100 {
        return Err("a CD-DA cue sheet needs a lead-in of at least 2 seconds");
    }
    if is_cd && lead_in % 588 != 0 {
        return Err("a CD-DA cue sheet's lead-in must be a multiple of 588 samples");
    }
    let Some(lead_out) = tracks.last() else {
        return Err("a cue sheet needs at least one track (the lead-out)");
    };
    if tracks.len() > 255 {
        return Err("a cue sheet holds at most 255 tracks");
    }
    if is_cd && lead_out.number != 170 {
        return Err("a CD-DA cue sheet's lead-out track must be number 170");
    }
    for (i, track) in tracks.iter().enumerate() {
        if track.number == 0 {
            return Err("cue sheet track number 0 is not allowed");
        }
        if is_cd && !((1..=99).contains(&track.number) || track.number == 170) {
            return Err("CD-DA cue sheet track numbers must be 1-99 or 170");
        }
        if is_cd && track.offset % 588 != 0 {
            return Err("CD-DA cue sheet track offsets must be multiples of 588 samples");
        }
        if track.indices.len() > 255 {
            return Err("a cue sheet track holds at most 255 index points");
        }
        if i + 1 < tracks.len() {
            match track.indices.first() {
                None => return Err("each cue sheet track needs at least one index point"),
                Some(first) if first.number > 1 => {
                    return Err("a cue sheet track's first index number must be 0 or 1");
                }
                Some(_) => {}
            }
        }
        for (j, index) in track.indices.iter().enumerate() {
            if is_cd && index.offset % 588 != 0 {
                return Err("CD-DA cue sheet index offsets must be multiples of 588 samples");
            }
            if j > 0 && u32::from(index.number) != u32::from(track.indices[j - 1].number) + 1 {
                return Err("cue sheet index numbers must increase by 1");
            }
        }
    }
    Ok(())
}
