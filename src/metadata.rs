//! FLAC metadata blocks. Only STREAMINFO (the mandatory first block) is written;
//! the field layout is from `FLAC/format.h` and the metadata framing in
//! `stream_encoder.c` (`streaminfo` setup at `:1358`, framesize tracking at
//! `:3019`). All fields are big-endian, MSB-first, and the body is exactly 34
//! bytes (byte-aligned throughout).

use crate::bitwriter::BitWriter;

/// The STREAMINFO fields. `min_blocksize`/`max_blocksize` are the configured
/// block size (libFLAC reports the same value for both even with a short final
/// frame); `min_framesize`/`max_framesize` are the min/max frame size in bytes.
pub struct StreamInfo {
    pub min_blocksize: u32,
    pub max_blocksize: u32,
    pub min_framesize: u32,
    pub max_framesize: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub total_samples: u64,
    pub md5: [u8; 16],
}

/// Metadata block type codes.
const METADATA_TYPE_STREAMINFO: u32 = 0;
const METADATA_TYPE_PADDING: u32 = 1;
const METADATA_TYPE_APPLICATION: u32 = 2;
const METADATA_TYPE_SEEKTABLE: u32 = 3;
const METADATA_TYPE_VORBIS_COMMENT: u32 = 4;
const METADATA_TYPE_CUESHEET: u32 = 5;
const METADATA_TYPE_PICTURE: u32 = 6;
/// STREAMINFO body length in bytes.
const STREAMINFO_LENGTH: u32 = 34;
/// One serialized seek point: `sample_number` (u64) + `stream_offset` (u64) +
/// `frame_samples` (u16) = 18 bytes (`FLAC__STREAM_METADATA_SEEKPOINT_LENGTH`).
const SEEKPOINT_LENGTH: u32 = 18;
/// Sample number marking an unused seek point
/// (`FLAC__STREAM_METADATA_SEEKPOINT_PLACEHOLDER`, `format.c:81`).
pub const SEEKPOINT_PLACEHOLDER: u64 = 0xffff_ffff_ffff_ffff;

/// One SEEKTABLE seek point (`FLAC__StreamMetadata_SeekPoint`). In a *template*
/// (before encoding) `sample_number` is the target sample to make seekable and the
/// other two fields are 0; the encoder rewrites all three for the frame that holds
/// each target (see [`Encoder`](crate::Encoder)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekPoint {
    pub sample_number: u64,
    pub stream_offset: u64,
    pub frame_samples: u32,
}

/// CUESHEET bit-lengths (`format.h`) for the non-byte-aligned reserved runs.
/// Cuesheet-level reserved after `is_cd` is `7 + 258*8` bits; per-track reserved
/// after the two flag bits is `6 + 13*8`; per-index reserved is `3*8`.
const CUESHEET_RESERVED_BITS: u32 = 7 + 258 * 8;
const CUESHEET_TRACK_RESERVED_BITS: u32 = 6 + 13 * 8;
const CUESHEET_INDEX_RESERVED_BITS: u32 = 3 * 8;

/// One CUESHEET track index point (`FLAC__StreamMetadata_CueSheet_Index`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CueSheetIndex {
    /// Offset in samples relative to the track offset.
    pub offset: u64,
    pub number: u8,
}

/// One CUESHEET track (`FLAC__StreamMetadata_CueSheet_Track`).
pub struct CueSheetTrack<'a> {
    /// Offset in samples from the start of the stream.
    pub offset: u64,
    pub number: u8,
    /// 12-byte ISRC (zero-filled when unset).
    pub isrc: [u8; 12],
    /// The track type bit: `false` = audio, `true` = non-audio.
    pub non_audio: bool,
    pub pre_emphasis: bool,
    pub indices: &'a [CueSheetIndex],
}

/// A metadata block the caller can place after STREAMINFO (which the encoder
/// always writes first). libFLAC writes blocks in the order given (the OGG
/// reorder is compiled out for native FLAC), so this list maps 1:1 to the output.
pub enum MetadataBlock<'a> {
    /// A VORBIS_COMMENT with the given vendor string and no user comments.
    VorbisComment(&'a str),
    /// A VORBIS_COMMENT with the given vendor string and user comments, each a
    /// complete `NAME=value` entry written verbatim and in order
    /// (`stream_encoder_framing.c:145-152`). Build entries with
    /// [`vorbis_comment_entry`], which validates them as libFLAC does.
    VorbisCommentEntries {
        vendor: &'a str,
        comments: &'a [&'a str],
    },
    /// A PADDING block of N zero bytes.
    Padding(u32),
    /// An APPLICATION block: a 4-byte registered application id + opaque data.
    Application { id: [u8; 4], data: &'a [u8] },
    /// A SEEKTABLE block: the seek points to serialize, in order. The encoder fills
    /// a *template* (each point's `sample_number` a target, offsets 0) during
    /// encoding and writes the filled+sorted result; `write_seektable` serializes
    /// whatever points it is given verbatim.
    Seektable(&'a [SeekPoint]),
    /// A CUESHEET block: the 128-byte media catalog number, lead-in samples, the
    /// CD-DA flag, and the track list (each with its index points). Fully
    /// caller-supplied — unlike SEEKTABLE, nothing is generated during encoding.
    CueSheet {
        media_catalog_number: &'a [u8; 128],
        lead_in: u64,
        is_cd: bool,
        tracks: &'a [CueSheetTrack<'a>],
    },
    /// A PICTURE block (e.g. cover art). `mime_type`/`description` are stored with
    /// 32-bit length prefixes; `picture_type` is the FLAC picture-type code.
    Picture {
        picture_type: u32,
        mime_type: &'a str,
        description: &'a str,
        width: u32,
        height: u32,
        depth: u32,
        colors: u32,
        data: &'a [u8],
    },
}

/// Write one [`MetadataBlock`] with its `is_last` flag.
pub fn write_block(bw: &mut BitWriter, block: &MetadataBlock, is_last: bool) {
    match block {
        MetadataBlock::VorbisComment(vendor) => write_vorbis_comment(bw, vendor, &[], is_last),
        MetadataBlock::VorbisCommentEntries { vendor, comments } => {
            write_vorbis_comment(bw, vendor, comments, is_last)
        }
        MetadataBlock::Padding(len) => write_padding(bw, *len, is_last),
        MetadataBlock::Application { id, data } => write_application(bw, id, data, is_last),
        MetadataBlock::Seektable(points) => write_seektable(bw, points, is_last),
        MetadataBlock::CueSheet {
            media_catalog_number,
            lead_in,
            is_cd,
            tracks,
        } => write_cuesheet(bw, media_catalog_number, *lead_in, *is_cd, tracks, is_last),
        MetadataBlock::Picture {
            picture_type,
            mime_type,
            description,
            width,
            height,
            depth,
            colors,
            data,
        } => {
            let mime = mime_type.as_bytes();
            let desc = description.as_bytes();
            // body = type + mime_len + mime + desc_len + desc + w/h/d/colors + data_len + data
            let length = 4 + 4 + mime.len() + 4 + desc.len() + 16 + 4 + data.len();
            write_block_header(bw, is_last, METADATA_TYPE_PICTURE, length as u32);
            bw.write_raw_u32(*picture_type, 32);
            bw.write_raw_u32(mime.len() as u32, 32);
            bw.write_byte_block(mime);
            bw.write_raw_u32(desc.len() as u32, 32);
            bw.write_byte_block(desc);
            bw.write_raw_u32(*width, 32);
            bw.write_raw_u32(*height, 32);
            bw.write_raw_u32(*depth, 32);
            bw.write_raw_u32(*colors, 32);
            bw.write_raw_u32(data.len() as u32, 32);
            bw.write_byte_block(data);
        }
    }
}

/// Write an APPLICATION block: 4-byte id then the application data
/// (`FLAC__metadata_object_application`; body length = 4 + data length).
pub fn write_application(bw: &mut BitWriter, id: &[u8; 4], data: &[u8], is_last: bool) {
    write_block_header(
        bw,
        is_last,
        METADATA_TYPE_APPLICATION,
        4 + data.len() as u32,
    );
    bw.write_byte_block(id);
    bw.write_byte_block(data);
}

/// Write a SEEKTABLE block: N seek points × 18 bytes
/// (`stream_encoder_framing.c:122`; the finish-time rewrite at
/// `stream_encoder.c:3257` produces the same layout). `sample_number` and
/// `stream_offset` are 64-bit, `frame_samples` 16-bit, all big-endian. The body
/// length is `num_points * 18` even after sorting (unused trailing points are
/// placeholders), matching the header written once at metadata time.
pub fn write_seektable(bw: &mut BitWriter, points: &[SeekPoint], is_last: bool) {
    write_block_header(
        bw,
        is_last,
        METADATA_TYPE_SEEKTABLE,
        points.len() as u32 * SEEKPOINT_LENGTH,
    );
    for p in points {
        bw.write_raw_u64(p.sample_number, 64);
        bw.write_raw_u64(p.stream_offset, 64);
        bw.write_raw_u32(p.frame_samples, 16);
    }
}

/// Build a SEEKTABLE *template* of `num` evenly-spaced placeholder points for a
/// stream of `total_samples`
/// (`FLAC__metadata_object_seektable_template_append_spaced_points`): point `i`
/// targets sample `total_samples * i / num`, with zero offset/frame_samples, to be
/// filled during encoding. Returns empty if `num` or `total_samples` is 0. (For a
/// legal table — at most ~932k points so the block fits the 24-bit length field —
/// the `total_samples * i` product cannot overflow `u64`.)
pub fn spaced_seek_points(num: u32, total_samples: u64) -> Vec<SeekPoint> {
    if num == 0 || total_samples == 0 {
        return Vec::new();
    }
    (0..num as u64)
        .map(|i| SeekPoint {
            sample_number: total_samples * i / num as u64,
            stream_offset: 0,
            frame_samples: 0,
        })
        .collect()
}

/// Sort + uniquify a (filled) seektable in place, exactly as the encoder does at
/// finish (`FLAC__format_seektable_sort`, `format.c:281`): sort by `sample_number`
/// (placeholders, `u64::MAX`, sort last), drop any non-placeholder point whose
/// `sample_number` duplicates the previous kept point, and overwrite the freed tail
/// slots with placeholders. The point count is preserved, so the serialized length
/// is unchanged.
pub fn seektable_sort(points: &mut [SeekPoint]) {
    if points.is_empty() {
        return;
    }
    // qsort in C is unstable, but post-fill duplicates share all three fields, so
    // the kept representative is identical regardless of order.
    points.sort_by_key(|p| p.sample_number);
    let mut j = 0usize;
    let mut first = true;
    for i in 0..points.len() {
        let sn = points[i].sample_number;
        if !first && sn != SEEKPOINT_PLACEHOLDER && sn == points[j - 1].sample_number {
            continue; // duplicate of the previous kept point
        }
        first = false;
        points[j] = points[i];
        j += 1;
    }
    for p in &mut points[j..] {
        *p = SeekPoint {
            sample_number: SEEKPOINT_PLACEHOLDER,
            stream_offset: 0,
            frame_samples: 0,
        };
    }
}

/// The serialized body length of a CUESHEET (`stream_encoder_framing.c:154`): a
/// 396-byte fixed header (128-byte catalog + 8-byte lead-in + 259 bytes of
/// `is_cd`+reserved + 1-byte track count), then 36 bytes per track plus 12 bytes
/// per index. Every field run lands on a byte boundary, so this is exact.
fn cuesheet_length(tracks: &[CueSheetTrack]) -> u32 {
    let mut len = 396u32;
    for t in tracks {
        len += 36 + t.indices.len() as u32 * 12;
    }
    len
}

/// Write a CUESHEET block (`stream_encoder_framing.c:154`). All fields are
/// big-endian/MSB-first; the reserved runs (`7+258*8`, `6+13*8`, `3*8` bits) are
/// **not** byte-aligned individually but each track/index boundary is. `is_cd` is a
/// single bit; the track `type` bit is `non_audio`.
pub fn write_cuesheet(
    bw: &mut BitWriter,
    media_catalog_number: &[u8; 128],
    lead_in: u64,
    is_cd: bool,
    tracks: &[CueSheetTrack],
    is_last: bool,
) {
    write_block_header(bw, is_last, METADATA_TYPE_CUESHEET, cuesheet_length(tracks));
    bw.write_byte_block(media_catalog_number);
    bw.write_raw_u64(lead_in, 64);
    bw.write_raw_u32(is_cd as u32, 1);
    bw.write_zeroes(CUESHEET_RESERVED_BITS);
    bw.write_raw_u32(tracks.len() as u32, 8);
    for t in tracks {
        bw.write_raw_u64(t.offset, 64);
        bw.write_raw_u32(t.number as u32, 8);
        bw.write_byte_block(&t.isrc);
        bw.write_raw_u32(t.non_audio as u32, 1);
        bw.write_raw_u32(t.pre_emphasis as u32, 1);
        bw.write_zeroes(CUESHEET_TRACK_RESERVED_BITS);
        bw.write_raw_u32(t.indices.len() as u32, 8);
        for idx in t.indices {
            bw.write_raw_u64(idx.offset, 64);
            bw.write_raw_u32(idx.number as u32, 8);
            bw.write_zeroes(CUESHEET_INDEX_RESERVED_BITS);
        }
    }
}

/// The vendor string libFLAC 1.5.0 writes into its auto VORBIS_COMMENT
/// (`FLAC__VENDOR_STRING` = `"reference libFLAC " PACKAGE_VERSION " 20250211"`, `format.c:58`
/// with no git tag/hash defined). Used to byte-match libFLAC's default output.
pub const LIBFLAC_VENDOR_STRING: &str = "reference libFLAC 1.5.0 20250211";

/// Write a metadata block header (1-bit last flag, 7-bit type, 24-bit length).
fn write_block_header(bw: &mut BitWriter, is_last: bool, block_type: u32, length: u32) {
    bw.write_raw_u32(is_last as u32, 1);
    bw.write_raw_u32(block_type, 7);
    bw.write_raw_u32(length, 24);
}

/// Write the STREAMINFO metadata block (4-byte block header + 34-byte body).
/// `is_last` sets the last-metadata-block flag.
pub fn write_streaminfo(bw: &mut BitWriter, si: &StreamInfo, is_last: bool) {
    write_block_header(bw, is_last, METADATA_TYPE_STREAMINFO, STREAMINFO_LENGTH);

    bw.write_raw_u32(si.min_blocksize, 16);
    bw.write_raw_u32(si.max_blocksize, 16);
    bw.write_raw_u32(si.min_framesize, 24);
    bw.write_raw_u32(si.max_framesize, 24);
    bw.write_raw_u32(si.sample_rate, 20);
    bw.write_raw_u32(si.channels - 1, 3);
    bw.write_raw_u32(si.bits_per_sample - 1, 5);
    // A count that does not fit 36 bits is written as 0, "unknown"
    // (`stream_encoder.c:3189`).
    let total_samples = if si.total_samples >> 36 != 0 {
        0
    } else {
        si.total_samples
    };
    bw.write_raw_u64(total_samples, 36);
    bw.write_byte_block(&si.md5);
}

/// Write a VORBIS_COMMENT block: the vendor string, then the user comments
/// (`stream_encoder_framing.c:132-152`). With no comments this is the empty block
/// libFLAC auto-writes (`add_metadata_block`). The vendor length, comment count and
/// comment lengths are little-endian per the Vorbis comment spec.
pub fn write_vorbis_comment(bw: &mut BitWriter, vendor: &str, comments: &[&str], is_last: bool) {
    let vendor = vendor.as_bytes();
    // vendor-length + vendor + num-comments + (length + entry) per comment
    let length =
        4 + vendor.len() as u32 + 4 + comments.iter().map(|c| 4 + c.len() as u32).sum::<u32>();
    write_block_header(bw, is_last, METADATA_TYPE_VORBIS_COMMENT, length);
    bw.write_raw_u32_little_endian(vendor.len() as u32);
    bw.write_byte_block(vendor);
    bw.write_raw_u32_little_endian(comments.len() as u32);
    for comment in comments {
        bw.write_raw_u32_little_endian(comment.len() as u32);
        bw.write_byte_block(comment.as_bytes());
    }
}

/// A Vorbis comment entry `NAME=value`, or `None` if libFLAC would reject it — the
/// equivalent of `FLAC__metadata_object_vorbiscomment_entry_from_name_value_pair`.
///
/// The name must be printable ASCII `0x20..=0x7D` without `=`
/// (`FLAC__format_vorbiscomment_entry_name_is_legal`, `format.c:363`). The value
/// must be legal UTF-8 for `FLAC__format_vorbiscomment_entry_value_is_legal`
/// (`format.c:372`); a Rust `str` always is, except that libFLAC also rejects
/// U+FFFE and U+FFFF (`utf8len_`, `format.c:339`). A NUL in the value is rejected
/// too, since the C API would end the value there. The name is kept as given (field
/// names compare case-insensitively; by convention they are upper case).
///
/// ```
/// use libflac_rs::vorbis_comment_entry;
/// assert_eq!(vorbis_comment_entry("TITLE", "Night 1").as_deref(), Some("TITLE=Night 1"));
/// assert_eq!(vorbis_comment_entry("A=B", "x"), None);
/// ```
pub fn vorbis_comment_entry(name: &str, value: &str) -> Option<String> {
    let name_ok = name
        .bytes()
        .all(|c| (0x20..=0x7D).contains(&c) && c != b'=');
    let value_ok = !value.contains(['\0', '\u{FFFE}', '\u{FFFF}']);
    (name_ok && value_ok).then(|| format!("{name}={value}"))
}

/// Write a PADDING block of `length` zero bytes (`add_metadata_block`,
/// `stream_encoder_framing.c:112`).
pub fn write_padding(bw: &mut BitWriter, length: u32, is_last: bool) {
    write_block_header(bw, is_last, METADATA_TYPE_PADDING, length);
    bw.write_zeroes(length * 8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vorbis_comment_layout() {
        let mut bw = BitWriter::new();
        write_vorbis_comment(&mut bw, "v", &["A=1", "BC=xy"], true);
        let bytes = bw.as_bytes();
        #[rustfmt::skip]
        let expected = [
            0x84, 0, 0, 25,                 // last, type 4, length 4+1+4+(4+3)+(4+5)
            1, 0, 0, 0, b'v',               // vendor
            2, 0, 0, 0,                     // comment count
            3, 0, 0, 0, b'A', b'=', b'1',
            5, 0, 0, 0, b'B', b'C', b'=', b'x', b'y',
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn vorbis_comment_entries_are_validated_like_libflac() {
        assert_eq!(
            vorbis_comment_entry("TITLE", "x").as_deref(),
            Some("TITLE=x")
        );
        assert_eq!(
            vorbis_comment_entry("title", "x").as_deref(),
            Some("title=x")
        );
        assert_eq!(vorbis_comment_entry(" }", "").as_deref(), Some(" }="));
        assert_eq!(vorbis_comment_entry("A=B", "x"), None); // '='
        assert_eq!(vorbis_comment_entry("A~", "x"), None); // 0x7E
        assert_eq!(vorbis_comment_entry("A\tB", "x"), None); // control character
        assert_eq!(vorbis_comment_entry("NÄME", "x"), None); // non-ASCII
        assert_eq!(vorbis_comment_entry("A", "a\u{FFFE}"), None);
        assert_eq!(vorbis_comment_entry("A", "a\u{FFFF}"), None);
        assert_eq!(vorbis_comment_entry("A", "a\0b"), None);
        assert_eq!(
            vorbis_comment_entry("A", "a=b ü 🦇").as_deref(),
            Some("A=a=b ü 🦇")
        );
    }

    fn pt(sample_number: u64, stream_offset: u64, frame_samples: u32) -> SeekPoint {
        SeekPoint {
            sample_number,
            stream_offset,
            frame_samples,
        }
    }

    fn placeholder() -> SeekPoint {
        pt(SEEKPOINT_PLACEHOLDER, 0, 0)
    }

    #[test]
    fn spaced_points_formula() {
        // sample_number = total * i / num (matches libFLAC's append_spaced_points).
        assert_eq!(
            spaced_seek_points(4, 8000),
            vec![pt(0, 0, 0), pt(2000, 0, 0), pt(4000, 0, 0), pt(6000, 0, 0)],
        );
        assert_eq!(
            spaced_seek_points(3, 10),
            vec![pt(0, 0, 0), pt(3, 0, 0), pt(6, 0, 0)], // 10*1/3=3, 10*2/3=6
        );
        assert!(spaced_seek_points(0, 8000).is_empty());
        assert!(spaced_seek_points(4, 0).is_empty());
    }

    #[test]
    fn sort_dedups_and_pads_with_placeholders() {
        // Multiple targets that resolved to the same frame become identical points;
        // the sort keeps one and pushes the freed slots to the tail as placeholders,
        // preserving the count. (Mirrors FLAC__format_seektable_sort.)
        let mut points = vec![
            pt(30, 300, 2048),
            pt(10, 100, 2048),
            pt(10, 100, 2048), // duplicate of the previous (same frame)
            placeholder(),
            pt(20, 200, 2048),
        ];
        seektable_sort(&mut points);
        assert_eq!(
            points,
            vec![
                pt(10, 100, 2048),
                pt(20, 200, 2048),
                pt(30, 300, 2048),
                placeholder(),
                placeholder(),
            ],
        );
    }

    #[test]
    fn sort_keeps_existing_placeholders_at_tail() {
        let mut points = vec![placeholder(), pt(5, 50, 2048), placeholder()];
        seektable_sort(&mut points);
        assert_eq!(points, vec![pt(5, 50, 2048), placeholder(), placeholder()]);
    }

    #[test]
    fn seektable_byte_layout() {
        let mut bw = BitWriter::new();
        write_seektable(
            &mut bw,
            &[pt(0x0102_0304_0506_0708, 0x1112_1314_1516_1718, 2048)],
            true,
        );
        assert_eq!(
            bw.as_bytes(),
            &[
                0x83, 0x00, 0x00, 0x12, // is_last=1 | type=3, length=18
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // sample_number
                0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, // stream_offset
                0x08, 0x00, // frame_samples = 2048
            ],
        );
    }

    #[test]
    fn cuesheet_byte_layout() {
        let mcn = [0u8; 128];
        let indices = [CueSheetIndex {
            offset: 0x1122_3344_5566_7788,
            number: 1,
        }];
        let tracks = [CueSheetTrack {
            offset: 0x0102_0304_0506_0708,
            number: 7,
            isrc: *b"ABCDEFGHIJKL",
            non_audio: true,
            pre_emphasis: false,
            indices: &indices,
        }];
        let mut bw = BitWriter::new();
        write_cuesheet(&mut bw, &mcn, 0x00FF_00FF_00FF_00FF, true, &tracks, true);
        let out = bw.as_bytes();

        // 4-byte block header + 396 fixed + 36 (one track) + 12 (one index) = 448.
        assert_eq!(out.len(), 448);
        // is_last=1 | type=5 (0x85); body length = 444 = 0x0001BC.
        assert_eq!(&out[0..4], &[0x85, 0x00, 0x01, 0xBC]);
        assert_eq!(&out[4..132], &[0u8; 128]); // media catalog number
        assert_eq!(&out[132..140], &0x00FF_00FF_00FF_00FFu64.to_be_bytes()); // lead_in
        assert_eq!(out[140], 0x80); // is_cd bit set, then reserved zeros
        assert_eq!(&out[141..399], &[0u8; 258]); // rest of the cuesheet reserved
        assert_eq!(out[399], 1); // num_tracks
        assert_eq!(&out[400..408], &0x0102_0304_0506_0708u64.to_be_bytes()); // track offset
        assert_eq!(out[408], 7); // track number
        assert_eq!(&out[409..421], b"ABCDEFGHIJKL"); // isrc
        assert_eq!(out[421], 0b1000_0000); // type=1 (non_audio), pre_emphasis=0
        assert_eq!(&out[422..435], &[0u8; 13]); // rest of the track reserved
        assert_eq!(out[435], 1); // num_indices
        assert_eq!(&out[436..444], &0x1122_3344_5566_7788u64.to_be_bytes()); // index offset
        assert_eq!(out[444], 1); // index number
        assert_eq!(&out[445..448], &[0u8; 3]); // index reserved
    }
}
