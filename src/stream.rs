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
    Frame, Gap, StreamHeader, Timeline, decode_frame, parse_seektable, parse_streaminfo,
    parse_vorbis_comment,
};
use crate::md5::Md5;
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
        DecodeError::Io(e)
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

/// The FLAC logical stream being read from an Ogg file.
struct OggLink {
    serial: u32,
    /// The packet being reassembled across pages.
    partial: Vec<u8>,
    packets: VecDeque<Vec<u8>>,
    /// Its end-of-stream page has been read.
    ended: bool,
}

enum Container {
    Native,
    Raw,
    Ogg(OggLink),
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
    input: Input<R>,
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
}

impl<R: Read> StreamDecoder<R> {
    /// Detect the container and read the header (for raw frames, the first frame).
    pub fn new(reader: R) -> Result<Self, DecodeError> {
        let mut input = Input {
            reader,
            buf: Vec::new(),
            pos: 0,
            eof: false,
        };
        if !input.fill(4)? {
            return Err(DecodeError::Malformed);
        }
        let head = &input.available()[..4];
        let mut first_frame = None;
        let (container, header) = if head == b"fLaC" {
            input.consume(4);
            (Container::Native, read_native_header(&mut input)?)
        } else if head == b"OggS" {
            let (link, header) = find_ogg_link(&mut input)?.ok_or(DecodeError::Malformed)?;
            (Container::Ogg(link), header)
        } else if head[0] == 0xFF && head[1] & 0xFE == 0xF8 {
            let frame = read_frame(&mut input, None)?.ok_or(DecodeError::Malformed)?;
            let header = StreamHeader {
                channels: frame.channels,
                bits_per_sample: frame.bits_per_sample,
                sample_rate: frame.sample_rate,
                total_samples: 0,
                md5: [0; 16],
                min_blocksize: 0,
                max_blocksize: 0,
                seek_points: Vec::new(),
                vorbis_comment: None,
            };
            first_frame = Some(frame);
            (Container::Raw, header)
        } else {
            return Err(DecodeError::Malformed);
        };
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
        };
        decoder.start_link();
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
        let Container::Ogg(link) = &mut self.container else {
            return Ok(false);
        };
        // Skip to the end of the current link.
        while !link.ended {
            if !read_ogg_page(&mut self.input, link)? {
                break;
            }
        }
        match find_ogg_link(&mut self.input)? {
            Some((next, header)) => {
                self.container = Container::Ogg(next);
                self.header = header;
                self.start_link();
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
        let stream_bps = Some(self.header.bits_per_sample);
        match &mut self.container {
            Container::Native => read_frame(&mut self.input, stream_bps),
            Container::Raw => read_frame(&mut self.input, None),
            Container::Ogg(link) => loop {
                if let Some(packet) = link.packets.pop_front() {
                    let mut br = BitReader::new(&packet);
                    return decode_frame(&mut br, stream_bps)
                        .map(Some)
                        .ok_or(DecodeError::Malformed);
                }
                if link.ended || !read_ogg_page(&mut self.input, link)? {
                    return Ok(None);
                }
            },
        }
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

/// The metadata blocks after `fLaC`: STREAMINFO, SEEKTABLE and VORBIS_COMMENT are
/// parsed, the rest skipped without buffering them.
fn read_native_header<R: Read>(input: &mut Input<R>) -> Result<StreamHeader, DecodeError> {
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
                3 => seek_points = parse_seektable(body),
                _ => vorbis_comment = parse_vorbis_comment(body),
            }
            input.consume(length);
        } else if !input.skip(length)? {
            return Err(DecodeError::Malformed);
        }
        if is_last {
            break;
        }
    }
    let mut header = header.ok_or(DecodeError::Malformed)?;
    header.seek_points = seek_points;
    header.vorbis_comment = vorbis_comment;
    Ok(header)
}

/// The next frame from a native/raw stream, `None` at the end of the input.
fn read_frame<R: Read>(
    input: &mut Input<R>,
    stream_bps: Option<u32>,
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
        if let Some(frame) = decode_frame(&mut br, stream_bps) {
            let used = br.byte_pos();
            input.consume(used);
            return Ok(Some(frame));
        }
        // Truncated rather than invalid: read more and retry, doubling the buffer.
        if !br.ran_out() || input.eof || avail.len() >= MAX_FRAME_BYTES {
            return Err(DecodeError::Malformed);
        }
        let more = avail.len();
        input.read_more(more)?;
    }
}

/// Read one Ogg page; packets of `link`'s logical stream are queued, other pages
/// skipped. `false` at the end of the input.
fn read_ogg_page<R: Read>(input: &mut Input<R>, link: &mut OggLink) -> Result<bool, DecodeError> {
    loop {
        if input.available().is_empty() && input.fill(1).map(|got| !got)? {
            return Ok(false);
        }
        match ogg::parse_page(input.available()).ok_or(DecodeError::Malformed)? {
            PageParse::NeedMore(n) => {
                if !input.fill(n)? {
                    return Err(DecodeError::Malformed);
                }
            }
            PageParse::Page(page, len) => {
                if page.serial == link.serial && !link.ended {
                    let mut off = 0;
                    for &lv in page.lacing {
                        let lv = lv as usize;
                        link.partial.extend_from_slice(&page.body[off..off + lv]);
                        off += lv;
                        if link.partial.len() > MAX_FRAME_BYTES {
                            return Err(DecodeError::Malformed);
                        }
                        if lv < 255 {
                            link.packets.push_back(std::mem::take(&mut link.partial));
                        }
                    }
                    link.ended = page.flags & 0x04 != 0;
                }
                input.consume(len);
                return Ok(true);
            }
        }
    }
}

/// Find the next FLAC logical stream (a BOS page with the `0x7F"FLAC"` mapping
/// header) and read its header packets: the mapping packet with STREAMINFO, then
/// the metadata packets up to the first frame. `None` if the input ends first.
fn find_ogg_link<R: Read>(
    input: &mut Input<R>,
) -> Result<Option<(OggLink, StreamHeader)>, DecodeError> {
    let serial = loop {
        if input.available().is_empty() && !input.fill(1)? {
            return Ok(None);
        }
        let page = ogg::parse_page(input.available()).ok_or(DecodeError::Malformed)?;
        match page {
            PageParse::NeedMore(n) => {
                if !input.fill(n)? {
                    return Err(DecodeError::Malformed);
                }
            }
            PageParse::Page(page, len) => {
                if page.flags & 0x02 != 0 && page.body.starts_with(b"\x7FFLAC") {
                    break page.serial; // parsed again below, as part of the link
                }
                input.consume(len);
            }
        }
    };
    let mut link = OggLink {
        serial,
        partial: Vec::new(),
        packets: VecDeque::new(),
        ended: false,
    };
    // The mapping packet: 0x7F "FLAC" major minor header-count(2) "fLaC"
    // STREAMINFO-block-header(4) STREAMINFO(34).
    let mapping = loop {
        if let Some(p) = link.packets.pop_front() {
            break p;
        }
        if !read_ogg_page(input, &mut link)? {
            return Err(DecodeError::Malformed);
        }
    };
    if mapping.len() < 13 + 4 + 34 || &mapping[9..13] != b"fLaC" || mapping[13] & 0x7F != 0 {
        return Err(DecodeError::Malformed);
    }
    let mut header = parse_streaminfo(&mapping[17..]).ok_or(DecodeError::Malformed)?;
    // Metadata packets (block header + body) until the first frame packet.
    loop {
        match link.packets.front() {
            Some(p) if p.first() == Some(&0xFF) => break,
            Some(_) => {
                let p = link.packets.pop_front().unwrap_or_default();
                if let Some(body) = p.get(4..) {
                    match p[0] & 0x7F {
                        3 => header.seek_points = parse_seektable(body),
                        4 => header.vorbis_comment = parse_vorbis_comment(body),
                        _ => {}
                    }
                }
            }
            None => {
                if link.ended || !read_ogg_page(input, &mut link)? {
                    break; // a link without audio
                }
            }
        }
    }
    Ok(Some((link, header)))
}
