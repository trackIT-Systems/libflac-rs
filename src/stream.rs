//! Streaming decoder: FLAC from any [`Read`], one block at a time, so memory stays
//! bounded however long the stream is — the counterpart of libFLAC's callback
//! decoder (`stream_decoder.c`). The whole-buffer functions ([`crate::decode`] and
//! friends) collect from it.
//!
//! Input is buffered until a frame (or Ogg page) is complete. A frame may not
//! exceed 16 MiB, the largest size STREAMINFO can declare, so a malformed stream
//! cannot make the decoder buffer without bound.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read};

use crate::bitreader::BitReader;
use crate::decoder::{
    DecodedVorbisComment, Frame, Gap, StreamHeader, Timeline, decode_frame, parse_seektable,
    parse_streaminfo, parse_vorbis_comment,
};
use crate::md5::Md5;
use crate::metadata::SeekPoint;
use crate::ogg::{self, PageParse};

/// Largest frame (or Ogg packet) accepted: the 24-bit STREAMINFO frame-size limit.
const MAX_FRAME_BYTES: usize = 1 << 24;
/// Minimum read size.
const READ_CHUNK: usize = 1 << 16;

/// Why decoding failed.
#[derive(Debug)]
pub enum DecodeError {
    /// Reading the input failed.
    Io(io::Error),
    /// The input is not FLAC, or is malformed or truncated: a bad marker, header or
    /// CRC, a frame libFLAC rejects, or data ending mid-frame.
    Malformed,
    /// The output would exceed [`DecodeOptions::max_samples`].
    LimitExceeded,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Io(e) => write!(f, "reading FLAC input failed: {e}"),
            DecodeError::Malformed => f.write_str("malformed or truncated FLAC input"),
            DecodeError::LimitExceeded => f.write_str("decoded audio exceeds the sample limit"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DecodeError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        if e.get_ref().is_some_and(|inner| inner.is::<OggMalformed>()) {
            DecodeError::Malformed
        } else {
            DecodeError::Io(e)
        }
    }
}

/// Options for [`crate::decode_with`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeOptions {
    /// Fail with [`DecodeError::LimitExceeded`] instead of producing more than this
    /// many samples per channel. A small FLAC file can legitimately decode to
    /// gigabytes (digital silence compresses extremely well), so set this for
    /// untrusted input when decoding into memory. `None`: no limit.
    pub max_samples: Option<u64>,
}

/// A block of decoded audio: one frame, or silence inserted for missing frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedBlock {
    /// Sample number (per channel) of the block's first sample.
    pub first_sample: u64,
    /// Interleaved PCM (channel-major within each sample).
    pub interleaved: Vec<i32>,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub sample_rate: u32,
    /// Silence inserted for missing frames (see [`Gap`]), not decoded audio.
    pub silence: bool,
}

impl DecodedBlock {
    /// Samples per channel in the block.
    pub fn samples(&self) -> u64 {
        match self.channels {
            0 => 0,
            c => (self.interleaved.len() / c as usize) as u64,
        }
    }
}

/// Buffered input.
struct Input<R> {
    reader: R,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<R: Read> Input<R> {
    fn available(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// Read at least `n` more bytes unless the input ends first.
    fn read_more(&mut self, n: usize) -> io::Result<()> {
        if self.pos > 0 && self.pos * 2 >= self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let start = self.buf.len();
        let target = start + n.max(READ_CHUNK);
        self.buf.resize(target, 0);
        let mut filled = start;
        while filled < start + n {
            match self.reader.read(&mut self.buf[filled..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(k) => filled += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.buf.truncate(filled);
                    return Err(e);
                }
            }
        }
        self.buf.truncate(filled);
        Ok(())
    }

    /// Buffer at least `n` bytes; `false` if the input ends first.
    fn fill(&mut self, n: usize) -> io::Result<bool> {
        while self.available().len() < n && !self.eof {
            let missing = n - self.available().len();
            self.read_more(missing)?;
        }
        Ok(self.available().len() >= n)
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }

    /// Discard `n` bytes; `false` if the input ends first.
    fn skip(&mut self, mut n: usize) -> io::Result<bool> {
        while n > 0 {
            if self.available().is_empty() {
                if self.eof {
                    return Ok(false);
                }
                self.read_more(1)?;
                continue;
            }
            let k = n.min(self.available().len());
            self.consume(k);
            n -= k;
        }
        Ok(true)
    }
}

enum Container {
    Native,
    /// Raw frames, or a native stream without STREAMINFO: no stream-level format.
    Raw,
}

/// Where the FLAC bytes come from: the input itself, or the FLAC logical stream
/// of an Ogg file.
enum Source<R> {
    Plain(R),
    Ogg(OggBytes<R>),
}

impl<R: Read> Read for Source<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Source::Plain(r) => r.read(buf),
            Source::Ogg(o) => o.read(buf),
        }
    }
}

/// An Ogg FLAC stream that is not valid (a bad mapping header); raised through
/// `io::Error` from [`OggBytes`] and turned back into [`DecodeError::Malformed`].
#[derive(Debug)]
struct OggMalformed;

impl fmt::Display for OggMalformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed Ogg FLAC mapping")
    }
}

impl std::error::Error for OggMalformed {}

fn ogg_malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, OggMalformed)
}

/// The bytes libFLAC's Ogg layer feeds its FLAC parser (`ogg_decoder_aspect.c`
/// over libogg's `framing.c`): the packets of one FLAC logical stream,
/// concatenated, with the 9-byte mapping header (`0x7F "FLAC"` version, header
/// count) stripped from any packet that starts with `0x7F`. The rest is the native
/// format (`fLaC`, metadata blocks, frames), so a lost page has the same effect as
/// in libFLAC.
///
/// - The stream is the first page whose body starts with `0x7F` and is longer than
///   5 bytes (libFLAC's serial-number selection); its EOS page ends it.
/// - Pages of other streams, and pages whose version is not 0, are ignored
///   (`ogg_stream_pagein` refuses them).
/// - A gap in page sequence numbers drops the unfinished packet; a page marked
///   "continued" whose predecessor packet is complete (or lost) has its leading
///   continuation segments skipped, as libogg does.
struct OggBytes<R> {
    pages: Input<R>,
    serial: Option<u32>,
    next_sequence: Option<u32>,
    partial: Vec<u8>,
    /// The last segment was 255 bytes: a packet continues on the next page.
    open: bool,
    out: Vec<u8>,
    out_pos: usize,
    /// The stream's EOS page has been read.
    ended: bool,
    /// The input ended in the middle of a page.
    truncated: bool,
}

impl<R: Read> OggBytes<R> {
    fn new(pages: Input<R>) -> Self {
        OggBytes {
            pages,
            serial: None,
            next_sequence: None,
            partial: Vec::new(),
            open: false,
            out: Vec::new(),
            out_pos: 0,
            ended: false,
            truncated: false,
        }
    }

    /// Start over with the next FLAC logical stream (a chain link).
    fn next_link(&mut self) {
        self.serial = None;
        self.next_sequence = None;
        self.partial.clear();
        self.open = false;
        self.out.clear();
        self.out_pos = 0;
        self.ended = false;
        self.truncated = false;
    }

    fn add_page(&mut self, page: OwnedPage) -> io::Result<()> {
        if self.serial.is_none() {
            if page.body.len() > 5 && page.body[0] == 0x7F {
                self.serial = Some(page.serial);
            } else {
                return Ok(());
            }
        }
        if Some(page.serial) != self.serial || page.version != 0 {
            return Ok(());
        }
        if self.next_sequence.is_some_and(|s| s != page.sequence) {
            self.partial.clear();
            self.open = false;
        }
        self.next_sequence = Some(page.sequence.wrapping_add(1));
        let (mut seg, mut off) = (0, 0);
        if page.flags & 0x01 != 0 && !self.open {
            while seg < page.lacing.len() {
                let lv = page.lacing[seg] as usize;
                seg += 1;
                off += lv;
                if lv < 255 {
                    break;
                }
            }
        }
        for &lv in &page.lacing[seg..] {
            let lv = lv as usize;
            self.partial.extend_from_slice(&page.body[off..off + lv]);
            off += lv;
            self.open = lv == 255;
            if self.partial.len() > MAX_FRAME_BYTES {
                return Err(ogg_malformed());
            }
            if lv < 255 {
                let packet = std::mem::take(&mut self.partial);
                match packet.first() {
                    Some(0x7F) => {
                        if packet.len() < 9 || &packet[1..5] != b"FLAC" || packet[5] != 1 {
                            return Err(ogg_malformed());
                        }
                        self.out.extend_from_slice(&packet[9..]);
                    }
                    _ => self.out.extend_from_slice(&packet),
                }
            }
        }
        if page.flags & 0x04 != 0 {
            self.ended = true;
        }
        Ok(())
    }
}

impl<R: Read> Read for OggBytes<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.out_pos < self.out.len() {
                let n = buf.len().min(self.out.len() - self.out_pos);
                buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            self.out.clear();
            self.out_pos = 0;
            if self.ended {
                return Ok(0);
            }
            let page = match next_page(&mut self.pages, &mut self.truncated) {
                Ok(page) => page,
                Err(DecodeError::Io(e)) => return Err(e),
                Err(_) => return Err(ogg_malformed()),
            };
            match page {
                Some(page) => self.add_page(page)?,
                None => return Ok(0),
            }
        }
    }
}

/// A streaming FLAC decoder over any [`Read`]: native FLAC (`fLaC` + metadata +
/// frames), raw frames as [`Encoder::encode_frames`](crate::Encoder::encode_frames)
/// writes them, or Ogg FLAC, detected from the first bytes.
///
/// [`next_block`](Self::next_block) (or the [`Iterator`] impl) yields the audio
/// block by block, so memory use is bounded by one frame regardless of the
/// stream's length; nothing is collected. It behaves like the whole-buffer
/// functions: frames are placed by their header numbers, missing frames are filled
/// with silence ([`Gap`]), decoding stops at STREAMINFO's sample count when set,
/// and the audio MD5 is checked at the end ([`md5_ok`](Self::md5_ok)). A malformed
/// frame ends decoding with [`DecodeError::Malformed`].
///
/// An Ogg file may chain several FLAC streams ("links"). Decoding stops at the end
/// of the first, as libFLAC does by default; [`next_link`](Self::next_link) moves on
/// to the next (libFLAC's `FLAC__stream_decoder_finish_link`).
///
/// ```
/// use libflac_rs::{Encoder, EncoderConfig, StreamDecoder};
///
/// let pcm: Vec<i32> = (0..44_100 * 2).map(|i| (i % 200) - 100).collect();
/// let flac = Encoder::new(EncoderConfig::new(2, 16, 44_100)).encode(&pcm);
///
/// let mut decoder = StreamDecoder::new(&flac[..])?; // any `std::io::Read`
/// assert_eq!(decoder.header().sample_rate, 44_100);
/// let mut samples = 0;
/// for block in &mut decoder {
///     samples += block?.samples(); // process each block, then drop it
/// }
/// assert_eq!(samples, 44_100);
/// assert_eq!(decoder.md5_ok(), Some(true));
/// # Ok::<(), libflac_rs::DecodeError>(())
/// ```
pub struct StreamDecoder<R> {
    input: Input<Source<R>>,
    container: Container,
    header: StreamHeader,
    timeline: Timeline,
    md5: Option<Md5>,
    md5_ok: Option<bool>,
    /// Samples per channel emitted in the current link.
    emitted: u64,
    /// Blocks decoded but not yet returned (gap silence before a frame).
    pending: VecDeque<DecodedBlock>,
    /// A raw-frames stream's first frame, decoded to learn the format.
    first_frame: Option<Frame>,
    /// The current link has ended.
    done: bool,
    /// The input ended in the middle of a frame or Ogg page.
    truncated: bool,
}

impl<R: Read> StreamDecoder<R> {
    /// Detect the container and read the header (for raw frames, the first frame).
    pub fn new(reader: R) -> Result<Self, DecodeError> {
        let mut plain = Input {
            reader,
            buf: Vec::new(),
            pos: 0,
            eof: false,
        };
        let ogg = plain.fill(4)? && &plain.available()[..4] == b"OggS";
        let mut input = if ogg {
            Input {
                reader: Source::Ogg(OggBytes::new(plain)),
                buf: Vec::new(),
                pos: 0,
                eof: false,
            }
        } else {
            Input {
                reader: Source::Plain(plain.reader),
                buf: plain.buf,
                pos: plain.pos,
                eof: plain.eof,
            }
        };
        let mut truncated = false;
        let (container, header, first_frame) =
            read_stream_start(&mut input, &mut truncated)?.ok_or(DecodeError::Malformed)?;
        let mut decoder = StreamDecoder {
            input,
            container,
            timeline: Timeline::new(None),
            header,
            md5: None,
            md5_ok: None,
            emitted: 0,
            pending: VecDeque::new(),
            first_frame,
            done: false,
            truncated,
        };
        decoder.start_link();
        decoder.truncated = truncated;
        Ok(decoder)
    }

    /// The current stream's (or link's) header.
    pub fn header(&self) -> &StreamHeader {
        &self.header
    }

    /// Missing frames found so far in the current stream or link.
    pub fn gaps(&self) -> &[Gap] {
        &self.timeline.gaps
    }

    /// Whether the audio matched STREAMINFO's MD5, once the stream or link has
    /// ended: `Some(true)` also when there is no MD5 to check. `None` before.
    pub fn md5_ok(&self) -> Option<bool> {
        self.md5_ok
    }

    /// Whether the input ended in the middle of a frame or Ogg page. Like libFLAC,
    /// the decoder then ends the stream after the last complete frame instead of
    /// failing; this tells the two apart (the MD5 check also fails then).
    pub fn truncated(&self) -> bool {
        self.truncated || matches!(&self.input.reader, Source::Ogg(o) if o.truncated)
    }

    /// The next block of audio, `Ok(None)` at the end of the stream (or of the
    /// current Ogg link).
    pub fn next_block(&mut self) -> Result<Option<DecodedBlock>, DecodeError> {
        loop {
            if let Some(block) = self.pending.pop_front() {
                return Ok(Some(block));
            }
            if self.done {
                return Ok(None);
            }
            let frame = match self.first_frame.take() {
                Some(frame) => frame,
                None => match self.next_frame()? {
                    Some(frame) => frame,
                    None => {
                        self.finish();
                        continue;
                    }
                },
            };
            let start = self.timeline.place(&frame);
            if let Some(fill) = self.timeline.check_gap(start, &frame) {
                // libFLAC writes the silence in blocks of the previous frame's size.
                let mut first = fill.gap.first_sample;
                let mut left = fill.gap.filled;
                while left > 0 && !self.done {
                    let n = left.min(fill.blocksize);
                    self.queue(DecodedBlock {
                        first_sample: first,
                        interleaved: vec![0; (n * u64::from(fill.channels)) as usize],
                        channels: fill.channels,
                        bits_per_sample: fill.bits_per_sample,
                        sample_rate: fill.sample_rate,
                        silence: true,
                    });
                    first += n;
                    left -= n;
                }
            }
            let (sample_rate, channels, bits_per_sample) = self.timeline.format(&frame);
            let mut interleaved = Vec::with_capacity(frame.blocksize * channels as usize);
            for i in 0..frame.blocksize {
                for ch in &frame.samples {
                    interleaved.push(ch[i]);
                }
            }
            self.queue(DecodedBlock {
                first_sample: start,
                interleaved,
                channels,
                bits_per_sample,
                sample_rate,
                silence: false,
            });
        }
    }

    /// Move to the next link of a chained Ogg FLAC stream: `Ok(true)` with
    /// [`header`](Self::header) updated, `Ok(false)` if there is none (always for
    /// native FLAC and raw frames). The rest of the current link is skipped.
    pub fn next_link(&mut self) -> Result<bool, DecodeError> {
        if !matches!(self.input.reader, Source::Ogg(_)) {
            return Ok(false);
        }
        // Skip to the end of the current link, then start reading the next one.
        while !self.input.eof {
            let n = self.input.available().len();
            self.input.consume(n);
            self.input.read_more(1)?;
        }
        if let Source::Ogg(ogg) = &mut self.input.reader {
            ogg.next_link();
        }
        self.input.buf.clear();
        self.input.pos = 0;
        self.input.eof = false;
        let mut truncated = false;
        match read_stream_start(&mut self.input, &mut truncated)? {
            Some((container, header, first_frame)) => {
                self.container = container;
                self.header = header;
                self.first_frame = first_frame;
                self.start_link();
                self.truncated = truncated;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Reset the per-link state.
    fn start_link(&mut self) {
        let has_info = !matches!(self.container, Container::Raw);
        self.timeline = Timeline::new(has_info.then_some(&self.header));
        self.md5 = (self.header.md5 != [0; 16]).then(Md5::new);
        self.md5_ok = None;
        self.emitted = 0;
        self.pending.clear();
        self.done = false;
        self.truncated = false;
    }

    /// Queue a block, truncated at STREAMINFO's sample count (when set), feeding
    /// the MD5; ends the link once the count is reached.
    fn queue(&mut self, mut block: DecodedBlock) {
        let total = self.header.total_samples;
        if total > 0 {
            let left = total.saturating_sub(self.emitted);
            if block.samples() >= left {
                block
                    .interleaved
                    .truncate((left * u64::from(block.channels)) as usize);
                self.done = true;
            }
        }
        self.emitted += block.samples();
        if let Some(md5) = &mut self.md5 {
            let bytes = self.header.bits_per_sample.div_ceil(8) as usize;
            let mut buf = Vec::with_capacity(block.interleaved.len() * bytes);
            for &s in &block.interleaved {
                buf.extend_from_slice(&(s as u32).to_le_bytes()[..bytes]);
            }
            md5.update(&buf);
        }
        if !block.interleaved.is_empty() {
            self.pending.push_back(block);
        }
        if self.done {
            self.finish();
        }
    }

    /// End the link and check the MD5.
    fn finish(&mut self) {
        self.done = true;
        if self.md5_ok.is_none() {
            self.md5_ok = Some(match self.md5.take() {
                Some(md5) => md5.finalize() == self.header.md5,
                None => true,
            });
        }
    }

    /// The next frame of the current stream or link, `None` at its end.
    fn next_frame(&mut self) -> Result<Option<Frame>, DecodeError> {
        let header = match self.container {
            Container::Native => Some(&self.header),
            Container::Raw => None,
        };
        read_frame(&mut self.input, header, &mut self.truncated)
    }
}

impl<R: Read> Iterator for StreamDecoder<R> {
    type Item = Result<DecodedBlock, DecodeError>;

    /// [`StreamDecoder::next_block`] as an iterator; an error ends the iteration.
    fn next(&mut self) -> Option<Self::Item> {
        match self.next_block() {
            Ok(block) => block.map(Ok),
            Err(e) => {
                self.done = true;
                self.pending.clear();
                Some(Err(e))
            }
        }
    }
}

/// Where a stream's data begins, as found by [`find_start`].
enum Start {
    /// `fLaC` was read; metadata follows.
    Native,
    /// A frame sync is next in the input (raw frames).
    Frame,
}

/// libFLAC's `find_metadata_` (`stream_decoder.c`): scan for `fLaC`, an ID3v2 tag
/// (which must be followed directly by `fLaC`), or a frame sync, skipping anything
/// else (libFLAC reports lost sync and continues). `None` if the input ends first.
fn find_start<R: Read>(input: &mut Input<R>) -> Result<Option<Start>, DecodeError> {
    const SYNC: &[u8; 4] = b"fLaC";
    const ID3: &[u8; 3] = b"ID3";
    let (mut i, mut id) = (0usize, 0usize);
    loop {
        if !input.fill(1)? {
            return Ok(None);
        }
        let x = input.available()[0];
        if x == SYNC[i] {
            input.consume(1);
            i += 1;
            id = 0;
            if i == 4 {
                return Ok(Some(Start::Native));
            }
            continue;
        }
        if id >= 3 {
            return Err(DecodeError::Malformed); // an ID3v2 tag not followed by fLaC
        }
        if x == ID3[id] {
            input.consume(1);
            id += 1;
            i = 0;
            if id == 3 {
                // Version and flags (3 bytes), a 28-bit syncsafe size, the tag.
                if !input.fill(7)? {
                    return Err(DecodeError::Malformed);
                }
                let size = input.available()[3..7]
                    .iter()
                    .fold(0usize, |acc, &b| acc << 7 | usize::from(b & 0x7F));
                input.consume(7);
                if !input.skip(size)? {
                    return Err(DecodeError::Malformed);
                }
            }
            continue;
        }
        id = 0;
        if x == 0xFF {
            if !input.fill(2)? {
                return Ok(None);
            }
            let y = input.available()[1];
            if y >> 1 == 0x7C {
                return Ok(Some(Start::Frame));
            }
            // A second 0xFF may start the sync; anything else is dropped with it.
            input.consume(if y == 0xFF { 1 } else { 2 });
        } else {
            input.consume(1);
        }
        i = 0;
    }
}

/// Read the start of a stream (or Ogg link) from `input`: native FLAC (`fLaC` and
/// metadata) or raw frames (the first frame, for the format). A native stream
/// without STREAMINFO is decoded like raw frames, keeping its other metadata.
/// `None` if the input holds no stream.
fn read_stream_start<R: Read>(
    input: &mut Input<R>,
    truncated: &mut bool,
) -> Result<Option<(Container, StreamHeader, Option<Frame>)>, DecodeError> {
    match find_start(input)? {
        None => Ok(None),
        Some(Start::Native) => Ok(Some(match read_native_header(input)? {
            (Some(mut header), seek_points, vorbis_comment) => {
                header.seek_points = seek_points;
                header.vorbis_comment = vorbis_comment;
                (Container::Native, header, None)
            }
            (None, seek_points, vorbis_comment) => {
                let frame = read_frame(input, None, truncated)?.ok_or(DecodeError::Malformed)?;
                let mut header = frame_header(&frame);
                header.seek_points = seek_points;
                header.vorbis_comment = vorbis_comment;
                (Container::Raw, header, Some(frame))
            }
        })),
        Some(Start::Frame) => {
            let frame = read_frame(input, None, truncated)?.ok_or(DecodeError::Malformed)?;
            let header = frame_header(&frame);
            Ok(Some((Container::Raw, header, Some(frame))))
        }
    }
}

/// STREAMINFO (if any), seek points and Vorbis comments of a native stream.
type NativeMetadata = (
    Option<StreamHeader>,
    Vec<SeekPoint>,
    Option<DecodedVorbisComment>,
);

/// The metadata blocks after `fLaC`: STREAMINFO, SEEKTABLE and VORBIS_COMMENT are
/// parsed, the rest skipped without buffering them. The STREAMINFO part of the
/// header is `None` if the stream has none (libFLAC then takes the format from
/// the frames).
fn read_native_header<R: Read>(input: &mut Input<R>) -> Result<NativeMetadata, DecodeError> {
    let mut header = None;
    let mut seek_points = Vec::new();
    let mut vorbis_comment = None;
    loop {
        if !input.fill(4)? {
            return Err(DecodeError::Malformed);
        }
        let h = &input.available()[..4];
        let is_last = h[0] & 0x80 != 0;
        let block_type = h[0] & 0x7F;
        let length = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        input.consume(4);
        if matches!(block_type, 0 | 3 | 4) {
            if !input.fill(length)? {
                return Err(DecodeError::Malformed);
            }
            let body = &input.available()[..length];
            match block_type {
                0 => header = Some(parse_streaminfo(body).ok_or(DecodeError::Malformed)?),
                3 => seek_points = parse_seektable(body).ok_or(DecodeError::Malformed)?,
                _ => vorbis_comment = parse_vorbis_comment(body),
            }
            input.consume(length);
        } else if (block_type == 2 && length < 4) || !input.skip(length)? {
            // libFLAC reads an APPLICATION block's 4-byte id before anything else
            // and gives up if the block is shorter (`read_metadata_`).
            return Err(DecodeError::Malformed);
        }
        if is_last {
            break;
        }
    }
    Ok((header, seek_points, vorbis_comment))
}

/// The next frame from a native/raw stream, `None` at the end of the input. A
/// frame cut off by the end of the input also ends it, as in libFLAC (which
/// treats running out of data mid-frame as the end of the stream); `truncated` is
/// then set.
fn read_frame<R: Read>(
    input: &mut Input<R>,
    stream: Option<&StreamHeader>,
    truncated: &mut bool,
) -> Result<Option<Frame>, DecodeError> {
    loop {
        let avail = input.available();
        if avail.is_empty() {
            if input.eof {
                return Ok(None);
            }
            input.read_more(1)?;
            continue;
        }
        let mut br = BitReader::new(avail);
        if let Some(frame) = decode_frame(&mut br, stream) {
            let used = br.byte_pos();
            input.consume(used);
            return Ok(Some(frame));
        }
        // Truncated rather than invalid: read more and retry, doubling the buffer.
        if !br.ran_out() || avail.len() >= MAX_FRAME_BYTES {
            return Err(DecodeError::Malformed);
        }
        if input.eof {
            let rest = avail.len();
            input.consume(rest);
            *truncated = true;
            return Ok(None);
        }
        let more = avail.len();
        input.read_more(more)?;
    }
}

/// An Ogg page copied out of the input.
struct OwnedPage {
    version: u8,
    flags: u8,
    serial: u32,
    sequence: u32,
    lacing: Vec<u8>,
    body: Vec<u8>,
}

/// The next valid Ogg page, `None` at the end of the input. Like libogg's
/// `ogg_sync_pageseek`, bytes that do not start a valid page (junk, or a page with
/// a bad CRC) are skipped up to the next `OggS` capture pattern; a page cut off by
/// the end of the input ends it and sets `truncated`.
fn next_page<R: Read>(
    input: &mut Input<R>,
    truncated: &mut bool,
) -> Result<Option<OwnedPage>, DecodeError> {
    loop {
        if input.available().is_empty() && !input.fill(1)? {
            return Ok(None);
        }
        match ogg::parse_page(input.available()) {
            Some(PageParse::Page(page, len)) => {
                let owned = OwnedPage {
                    version: page.version,
                    flags: page.flags,
                    serial: page.serial,
                    sequence: page.sequence,
                    lacing: page.lacing.to_vec(),
                    body: page.body.to_vec(),
                };
                input.consume(len);
                return Ok(Some(owned));
            }
            Some(PageParse::NeedMore(n)) => {
                if !input.fill(n)? {
                    let rest = input.available().len();
                    input.consume(rest);
                    *truncated = true;
                    return Ok(None);
                }
            }
            None => {
                // Resynchronize after the current byte.
                let avail = input.available();
                match avail[1..].windows(4).position(|w| w == b"OggS") {
                    Some(i) => input.consume(1 + i),
                    None => {
                        let skip = avail.len().saturating_sub(3).max(1);
                        input.consume(skip);
                        if !input.eof {
                            input.read_more(1)?;
                        }
                    }
                }
            }
        }
    }
}

/// A header for a stream without STREAMINFO: the first frame's format.
fn frame_header(frame: &Frame) -> StreamHeader {
    StreamHeader {
        channels: frame.channels,
        bits_per_sample: frame.bits_per_sample,
        sample_rate: frame.sample_rate,
        total_samples: 0,
        md5: [0; 16],
        min_blocksize: 0,
        max_blocksize: 0,
        seek_points: Vec::new(),
        vorbis_comment: None,
    }
}
