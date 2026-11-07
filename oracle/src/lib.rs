//! The C reference encoder and decoder — libFLAC 1.5.0 with libogg 1.3.5 — behind
//! the same settings types as `libflac-rs`, for differential tests.
//!
//! Each function encodes with libFLAC configured from a [`libflac_rs::EncoderConfig`]
//! and mirrors the [`libflac_rs::Encoder`] method of the same name, so a test is
//! `assert_eq!(oracle::encode(&cfg, &pcm), Encoder::new(cfg).encode(&pcm))`.
//!
//! libFLAC always writes a VORBIS_COMMENT (it inserts an empty one after STREAMINFO
//! when the block list has none) and always writes its own vendor string, so a
//! block list that matches `libflac-rs` starts with
//! `MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING)` or a
//! `VorbisCommentEntries` with that vendor.
//!
//! Build requirements: the C sources (`fetch-sources.sh`) and a C compiler. Float
//! parity holds where Rust and C resolve `cosf`/`lround` to the same libm; CI runs
//! on glibc, the project's reference (see `ORACLE.md`).

use libflac_rs::{EncoderConfig, MetadataBlock};
use std::ffi::{CStr, CString, c_char};
use std::ptr;

#[repr(C)]
struct Config {
    channels: u32,
    bits_per_sample: u32,
    sample_rate: u32,
    blocksize: u32,
    compression_level: u32,
    do_md5: u32,
    mode: u32,
    ogg_serial: i32,
}

#[repr(C)]
struct Index {
    offset: u64,
    number: u8,
}

#[repr(C)]
struct Track {
    offset: u64,
    number: u8,
    isrc: [u8; 12],
    non_audio: u8,
    pre_emphasis: u8,
    num_indices: u32,
    indices: *const Index,
}

#[repr(C)]
struct Block {
    kind: u32,
    padding_length: u32,
    application_id: [u8; 4],
    data: *const u8,
    data_length: u32,
    vendor: *const u8,
    vendor_length: u32,
    comments: *const *const u8,
    comment_lengths: *const u32,
    num_comments: u32,
    seek_targets: *const u64,
    num_seek_targets: u32,
    media_catalog_number: *const u8,
    lead_in: u64,
    is_cd: u32,
    tracks: *const Track,
    num_tracks: u32,
    picture_type: u32,
    mime_type: *const c_char,
    description: *const c_char,
    width: u32,
    height: u32,
    depth: u32,
    colors: u32,
}

impl Block {
    fn new(kind: u32) -> Self {
        Self {
            kind,
            padding_length: 0,
            application_id: [0; 4],
            data: ptr::null(),
            data_length: 0,
            vendor: ptr::null(),
            vendor_length: 0,
            comments: ptr::null(),
            comment_lengths: ptr::null(),
            num_comments: 0,
            seek_targets: ptr::null(),
            num_seek_targets: 0,
            media_catalog_number: ptr::null(),
            lead_in: 0,
            is_cd: 0,
            tracks: ptr::null(),
            num_tracks: 0,
            picture_type: 0,
            mime_type: ptr::null(),
            description: ptr::null(),
            width: 0,
            height: 0,
            depth: 0,
            colors: 0,
        }
    }
}

unsafe extern "C" {
    fn libflac_rs_oracle_encode(
        cfg: *const Config,
        interleaved: *const i32,
        nsamples: u32,
        blocks: *const Block,
        num_blocks: i32,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> i32;
    fn libflac_rs_oracle_free(buf: *mut u8);
    fn libflac_rs_oracle_vendor_string() -> *const c_char;
    #[allow(clippy::too_many_arguments)]
    fn libflac_rs_oracle_decode(
        data: *const u8,
        len: usize,
        is_ogg: u32,
        chained: u32,
        pcm: *mut *mut i32,
        pcm_len: *mut usize,
        links: *mut *mut LinkC,
        num_links: *mut usize,
        errors: *mut *mut i32,
        num_errors: *mut usize,
    ) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LinkC {
    channels: u32,
    bits_per_sample: u32,
    sample_rate: u32,
    first: u64,
}

const MODE_FRAMES: u32 = 0;
const MODE_STREAM: u32 = 1;
const MODE_OGG: u32 = 2;

/// Heap storage the C block descriptors point into; must outlive the FFI call.
/// Moving a `Vec` or `CString` into here does not move its heap buffer.
#[derive(Default)]
struct Keep {
    ptr_lists: Vec<Vec<*const u8>>,
    u32_lists: Vec<Vec<u32>>,
    u64_lists: Vec<Vec<u64>>,
    strings: Vec<CString>,
    tracks: Vec<Vec<Track>>,
    indices: Vec<Vec<Index>>,
}

fn c_string(keep: &mut Keep, s: &str) -> *const c_char {
    let c = CString::new(s).expect("metadata string contains NUL");
    let p = c.as_ptr();
    keep.strings.push(c);
    p
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).expect("length exceeds u32")
}

fn to_c(block: &MetadataBlock, keep: &mut Keep) -> Block {
    match block {
        MetadataBlock::Padding(n) => {
            let mut b = Block::new(1);
            b.padding_length = *n;
            b
        }
        MetadataBlock::Application { id, data } => {
            let mut b = Block::new(2);
            b.application_id = *id;
            b.data = data.as_ptr();
            b.data_length = len32(data.len());
            b
        }
        MetadataBlock::Seektable(points) => {
            let targets: Vec<u64> = points.iter().map(|p| p.sample_number).collect();
            let mut b = Block::new(3);
            b.seek_targets = targets.as_ptr();
            b.num_seek_targets = len32(targets.len());
            keep.u64_lists.push(targets);
            b
        }
        MetadataBlock::VorbisComment(vendor) => vorbis_comment(vendor, &[], keep),
        MetadataBlock::VorbisCommentEntries { vendor, comments } => {
            vorbis_comment(vendor, comments, keep)
        }
        MetadataBlock::CueSheet {
            media_catalog_number,
            lead_in,
            is_cd,
            tracks,
        } => {
            let c_tracks: Vec<Track> = tracks
                .iter()
                .map(|t| {
                    let indices: Vec<Index> = t
                        .indices
                        .iter()
                        .map(|i| Index {
                            offset: i.offset,
                            number: i.number,
                        })
                        .collect();
                    let track = Track {
                        offset: t.offset,
                        number: t.number,
                        isrc: t.isrc,
                        non_audio: t.non_audio.into(),
                        pre_emphasis: t.pre_emphasis.into(),
                        num_indices: len32(indices.len()),
                        indices: indices.as_ptr(),
                    };
                    keep.indices.push(indices);
                    track
                })
                .collect();
            let mut b = Block::new(5);
            b.media_catalog_number = media_catalog_number.as_ptr();
            b.lead_in = *lead_in;
            b.is_cd = (*is_cd).into();
            b.tracks = c_tracks.as_ptr();
            b.num_tracks = len32(c_tracks.len());
            keep.tracks.push(c_tracks);
            b
        }
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
            let mut b = Block::new(6);
            b.picture_type = *picture_type;
            b.mime_type = c_string(keep, mime_type);
            b.description = c_string(keep, description);
            b.width = *width;
            b.height = *height;
            b.depth = *depth;
            b.colors = *colors;
            b.data = data.as_ptr();
            b.data_length = len32(data.len());
            b
        }
    }
}

fn vorbis_comment(vendor: &str, comments: &[&str], keep: &mut Keep) -> Block {
    let ptrs: Vec<*const u8> = comments.iter().map(|c| c.as_ptr()).collect();
    let lens: Vec<u32> = comments.iter().map(|c| len32(c.len())).collect();
    let mut b = Block::new(4);
    b.vendor = vendor.as_ptr();
    b.vendor_length = len32(vendor.len());
    b.comments = ptrs.as_ptr();
    b.comment_lengths = lens.as_ptr();
    b.num_comments = len32(comments.len());
    keep.ptr_lists.push(ptrs);
    keep.u32_lists.push(lens);
    b
}

fn run(
    config: &EncoderConfig,
    mode: u32,
    serial: i32,
    interleaved: &[i32],
    blocks: Option<&[MetadataBlock]>,
) -> Vec<u8> {
    try_run(config, mode, serial, interleaved, blocks)
        .unwrap_or_else(|rc| panic!("libFLAC encode failed (shim code {rc})"))
}

/// Encode with libFLAC; `Err` with the shim's negative code when libFLAC rejects
/// the settings (`-100 - init status`) or the samples (`-200 - state`).
fn try_run(
    config: &EncoderConfig,
    mode: u32,
    serial: i32,
    interleaved: &[i32],
    blocks: Option<&[MetadataBlock]>,
) -> Result<Vec<u8>, i32> {
    let channels = config.channels as usize;
    assert!(
        channels == 0 || interleaved.len() % channels == 0,
        "ragged interleave"
    );
    let cfg = Config {
        channels: config.channels,
        bits_per_sample: config.bits_per_sample,
        sample_rate: config.sample_rate,
        blocksize: config.block_size,
        compression_level: config.compression_level,
        do_md5: config.md5.into(),
        mode,
        ogg_serial: serial,
    };
    let mut keep = Keep::default();
    let c_blocks: Vec<Block> = blocks
        .unwrap_or(&[])
        .iter()
        .map(|b| to_c(b, &mut keep))
        .collect();
    let num_blocks = blocks.map_or(-1, |b| i32::try_from(b.len()).unwrap());
    let mut out: *mut u8 = ptr::null_mut();
    let mut out_len = 0usize;
    // SAFETY: every pointer refers to `interleaved`, `cfg`, `c_blocks`, the caller's
    // `blocks`, or `keep`, all alive until the call returns. The C side only reads
    // them and hands back a malloc'd buffer, copied and then freed below.
    let rc = unsafe {
        libflac_rs_oracle_encode(
            &cfg,
            interleaved.as_ptr(),
            len32(interleaved.len().checked_div(channels).unwrap_or(0)),
            c_blocks.as_ptr(),
            num_blocks,
            &mut out,
            &mut out_len,
        )
    };
    if rc != 0 {
        return Err(rc);
    }
    if out.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: on success `out` holds `out_len` initialized bytes owned by us.
    let bytes = unsafe { std::slice::from_raw_parts(out, out_len) }.to_vec();
    unsafe { libflac_rs_oracle_free(out) };
    drop(keep);
    Ok(bytes)
}

/// libFLAC's raw audio frames (marker and metadata dropped, as CHD stores them).
/// Mirrors [`libflac_rs::Encoder::encode_frames`].
pub fn encode_frames(config: &EncoderConfig, interleaved: &[i32]) -> Vec<u8> {
    run(config, MODE_FRAMES, 0, interleaved, None)
}

/// A complete `.flac` stream with libFLAC's default metadata. Mirrors
/// [`libflac_rs::Encoder::encode`].
pub fn encode(config: &EncoderConfig, interleaved: &[i32]) -> Vec<u8> {
    run(config, MODE_STREAM, 0, interleaved, None)
}

/// A complete `.flac` stream with `blocks` set via
/// `FLAC__stream_encoder_set_metadata`. Mirrors
/// [`libflac_rs::Encoder::encode_with_metadata`].
pub fn encode_with_metadata(
    config: &EncoderConfig,
    interleaved: &[i32],
    blocks: &[MetadataBlock],
) -> Vec<u8> {
    run(config, MODE_STREAM, 0, interleaved, Some(blocks))
}

/// [`encode_with_metadata`], returning libFLAC's rejection (the shim's negative
/// code) instead of panicking: `-100 - FLAC__StreamEncoderInitStatus` for
/// settings or metadata `init` refuses, `-200 - state` for samples `process`
/// refuses.
pub fn try_encode_with_metadata(
    config: &EncoderConfig,
    interleaved: &[i32],
    blocks: &[MetadataBlock],
) -> Result<Vec<u8>, i32> {
    try_run(config, MODE_STREAM, 0, interleaved, Some(blocks))
}

/// A complete Ogg FLAC stream with libFLAC's default metadata and the given Ogg
/// `serial`. Mirrors [`libflac_rs::Encoder::encode_ogg`].
pub fn encode_ogg(config: &EncoderConfig, interleaved: &[i32], serial: i32) -> Vec<u8> {
    run(config, MODE_OGG, serial, interleaved, None)
}

/// `FLAC__VENDOR_STRING` of the compiled libFLAC.
pub fn vendor_string() -> String {
    // SAFETY: returns a pointer to a static NUL-terminated string.
    unsafe { CStr::from_ptr(libflac_rs_oracle_vendor_string()) }
        .to_string_lossy()
        .into_owned()
}

/// One decoded stream (or Ogg chain link) as libFLAC's decoder delivers it,
/// including any silence it inserted for missing frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedLink {
    pub channels: u32,
    pub bits_per_sample: u32,
    pub sample_rate: u32,
    pub interleaved: Vec<i32>,
}

/// `FLAC__StreamDecoderErrorStatus` values the decoder can report.
pub mod error {
    pub const LOST_SYNC: i32 = 0;
    pub const BAD_HEADER: i32 = 1;
    pub const FRAME_CRC_MISMATCH: i32 = 2;
    pub const UNPARSEABLE_STREAM: i32 = 3;
    pub const BAD_METADATA: i32 = 4;
    pub const OUT_OF_BOUNDS: i32 = 5;
    pub const MISSING_FRAME: i32 = 6;
}

/// What libFLAC's decoder produced: the links in order, and the error statuses it
/// reported (see [`error`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub links: Vec<DecodedLink>,
    pub errors: Vec<i32>,
}

/// Decode with libFLAC: native FLAC or raw frames when `ogg` is false, Ogg FLAC
/// otherwise, following all chain links when `chained` is set
/// (`FLAC__stream_decoder_set_decode_chained_stream`). MD5 checking is off.
pub fn decode(data: &[u8], ogg: bool, chained: bool) -> Decoded {
    let (mut pcm, mut pcm_len) = (ptr::null_mut::<i32>(), 0usize);
    let (mut links, mut num_links) = (ptr::null_mut::<LinkC>(), 0usize);
    let (mut errors, mut num_errors) = (ptr::null_mut::<i32>(), 0usize);
    // SAFETY: `data` outlives the call; the C side hands back malloc'd buffers of
    // the stated lengths (or null when empty), copied and then freed below.
    let rc = unsafe {
        libflac_rs_oracle_decode(
            data.as_ptr(),
            data.len(),
            ogg.into(),
            chained.into(),
            &mut pcm,
            &mut pcm_len,
            &mut links,
            &mut num_links,
            &mut errors,
            &mut num_errors,
        )
    };
    assert!(rc >= 0, "libFLAC decode failed (shim code {rc})");
    // SAFETY: as above; a null pointer only comes with a zero length.
    let copy = |p: *const u8, n: usize| -> Vec<u8> {
        if p.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(p, n) }.to_vec()
        }
    };
    let pcm_vec: Vec<i32> = copy(pcm.cast(), pcm_len * 4)
        .chunks_exact(4)
        .map(|b| i32::from_ne_bytes(b.try_into().unwrap()))
        .collect();
    let link_vec: Vec<LinkC> = (0..num_links)
        // SAFETY: `links` holds `num_links` initialized entries.
        .map(|i| unsafe { *links.add(i) })
        .collect();
    let error_vec: Vec<i32> = copy(errors.cast(), num_errors * 4)
        .chunks_exact(4)
        .map(|b| i32::from_ne_bytes(b.try_into().unwrap()))
        .collect();
    // SAFETY: each buffer came from malloc (or is null).
    unsafe {
        libflac_rs_oracle_free(pcm.cast());
        libflac_rs_oracle_free(links.cast());
        libflac_rs_oracle_free(errors.cast());
    }
    let ends: Vec<usize> = link_vec
        .iter()
        .skip(1)
        .map(|l| l.first as usize)
        .chain([pcm_vec.len()])
        .collect();
    Decoded {
        links: link_vec
            .iter()
            .zip(ends)
            .map(|(l, end)| DecodedLink {
                channels: l.channels,
                bits_per_sample: l.bits_per_sample,
                sample_rate: l.sample_rate,
                interleaved: pcm_vec[l.first as usize..end].to_vec(),
            })
            .collect(),
        errors: error_vec,
    }
}
