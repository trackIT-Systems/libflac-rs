//! Ogg encapsulation for FLAC-in-Ogg (`.oga`/`.ogg`), a faithful port of the
//! relevant libogg 1.3.5 paging (`framing.c`) plus the FLAC-in-Ogg mapping driven
//! by libFLAC's `ogg_encoder_aspect.c`. libFLAC delegates all Ogg paging to libogg,
//! so to be **byte-identical** to libFLAC+libogg output this reproduces libogg's
//! exact page-packing heuristics, lacing, granule selection, and CRC-32.
//!
//! Encode ([`OggStream`]): packets are buffered and emitted as pages by
//! `flush`/`pageout` exactly as `ogg_stream_flush`/`ogg_stream_pageout` do —
//! including the "nominal page ≈ 4096 bytes and ≥ 4 packets, else 255 segments"
//! rule. Decode ([`parse_page`]): pages are parsed and CRC-checked one at a time;
//! the stream decoder reassembles packets by lacing values (a `< 255` segment ends
//! a packet; `255` continues it, across pages), per FLAC logical stream.

/// The Ogg CRC-32 lookup table: polynomial `0x04c11db7`, **unreflected**, init/final
/// `0` (`framing.c` `crc_lookup[0]`).
const OGG_CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
            j += 1;
        }
        t[i] = crc;
        i += 1;
    }
    t
};

/// Fold `bytes` into a running Ogg CRC (`_os_update_crc`, byte-at-a-time form).
fn crc_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        crc = (crc << 8) ^ OGG_CRC_TABLE[(((crc >> 24) & 0xff) as u8 ^ b) as usize];
    }
    crc
}

/// The encode-side logical bitstream state (`ogg_stream_state`), holding buffered
/// packet bodies + per-segment lacing/granule values, and accumulating output pages.
pub struct OggStream {
    /// Buffered packet bodies; `body[body_returned..]` is not yet paged out.
    body: Vec<u8>,
    body_returned: usize,
    /// Per-segment lacing values; the low byte is the segment length, bit `0x100`
    /// flags the first segment of a packet.
    lacing: Vec<i32>,
    /// Per-segment granule positions (the running value at packet-in time).
    granule: Vec<i64>,
    /// The last granule position handed to [`Self::packetin`].
    granulepos: i64,
    /// Next page sequence number.
    pageno: i64,
    /// Set once the first page has been emitted (the BOS page).
    began: bool,
    e_o_s: bool,
    serialno: i32,
    out: Vec<u8>,
}

impl OggStream {
    pub fn new(serialno: i32) -> Self {
        Self {
            body: Vec::new(),
            body_returned: 0,
            lacing: Vec::new(),
            granule: Vec::new(),
            granulepos: 0,
            pageno: 0,
            began: false,
            e_o_s: false,
            serialno,
            out: Vec::new(),
        }
    }

    /// Submit one packet (`ogg_stream_packetin`/`iovecin`): append its body and
    /// `bytes/255 + 1` lacing values (the leading ones `255`, a final `bytes % 255`).
    pub fn packetin(&mut self, packet: &[u8], e_o_s: bool, granulepos: i64) {
        let bytes = packet.len();
        let lacing_count = bytes / 255 + 1;
        self.body.extend_from_slice(packet);
        let first = self.lacing.len();
        for _ in 0..lacing_count - 1 {
            self.lacing.push(255);
            self.granule.push(self.granulepos);
        }
        self.lacing.push((bytes % 255) as i32);
        self.granule.push(granulepos);
        self.granulepos = granulepos;
        // Flag the first segment as the beginning of the packet.
        self.lacing[first] |= 0x100;
        if e_o_s {
            self.e_o_s = true;
        }
    }

    /// Emit one page if warranted (`ogg_stream_flush_i`). `force` emits regardless of
    /// size (down to the last whole packet / 255 segments); otherwise a page is only
    /// emitted once it reaches the nominal size. Returns `true` if a page was written.
    fn flush_i(&mut self, mut force: bool, nfill: i64) -> bool {
        let lacing_fill = self.lacing.len();
        let maxvals = lacing_fill.min(255);
        if maxvals == 0 {
            return false;
        }

        let mut vals = 0usize;
        let granule_pos: i64;
        if !self.began {
            // Initial header page: include exactly the first packet.
            granule_pos = 0;
            while vals < maxvals {
                let end = (self.lacing[vals] & 0xff) < 255;
                vals += 1;
                if end {
                    break;
                }
            }
        } else {
            // Accumulate packets up to the nominal fill, but don't span pages
            // unnecessarily and prefer ≥ 4 packets per page (the libogg heuristic).
            let mut acc = 0i64;
            let mut packets_done = 0;
            let mut packet_just_done = 0;
            let mut gp = -1i64;
            while vals < maxvals {
                if acc > nfill && packet_just_done >= 4 {
                    force = true;
                    break;
                }
                acc += (self.lacing[vals] & 0xff) as i64;
                if (self.lacing[vals] & 0xff) < 255 {
                    gp = self.granule[vals];
                    packets_done += 1;
                    packet_just_done = packets_done;
                } else {
                    packet_just_done = 0;
                }
                vals += 1;
            }
            if vals == 255 {
                force = true;
            }
            granule_pos = gp;
        }

        if !force {
            return false;
        }

        let mut header = Vec::with_capacity(27 + vals);
        header.extend_from_slice(b"OggS");
        header.push(0x00); // stream structure version
        let mut flags = 0u8;
        if self.lacing[0] & 0x100 == 0 {
            flags |= 0x01; // continued packet
        }
        if !self.began {
            flags |= 0x02; // first page (BOS)
        }
        if self.e_o_s && lacing_fill == vals {
            flags |= 0x04; // last page (EOS)
        }
        header.push(flags);
        self.began = true;

        let mut g = granule_pos as u64;
        for _ in 0..8 {
            header.push((g & 0xff) as u8);
            g >>= 8;
        }
        let mut s = self.serialno as u32;
        for _ in 0..4 {
            header.push((s & 0xff) as u8);
            s >>= 8;
        }
        let mut p = self.pageno as u64;
        self.pageno += 1;
        for _ in 0..4 {
            header.push((p & 0xff) as u8);
            p >>= 8;
        }
        header.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder

        header.push(vals as u8);
        let mut body_len = 0usize;
        for i in 0..vals {
            let lv = (self.lacing[i] & 0xff) as u8;
            header.push(lv);
            body_len += lv as usize;
        }

        let body = &self.body[self.body_returned..self.body_returned + body_len];
        let crc = crc_update(crc_update(0, &header), body);
        header[22] = (crc & 0xff) as u8;
        header[23] = ((crc >> 8) & 0xff) as u8;
        header[24] = ((crc >> 16) & 0xff) as u8;
        header[25] = ((crc >> 24) & 0xff) as u8;

        self.out.extend_from_slice(&header);
        self.out.extend_from_slice(body);

        self.body_returned += body_len;
        self.lacing.drain(0..vals);
        self.granule.drain(0..vals);
        true
    }

    /// Flush all buffered packets into pages (`ogg_stream_flush` loop) — used after
    /// the BOS and each metadata packet (the mapping requires a flush after metadata).
    pub fn flush(&mut self) {
        while self.flush_i(true, 4096) {}
    }

    /// Emit any complete pages (`ogg_stream_pageout` loop) — used after each audio
    /// packet. Forces the final page(s) once EOS has been submitted.
    pub fn pageout(&mut self) {
        loop {
            let force =
                (self.e_o_s && !self.lacing.is_empty()) || (!self.lacing.is_empty() && !self.began);
            if !self.flush_i(force, 4096) {
                break;
            }
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

/// One Ogg page, borrowed from the input.
pub struct Page<'a> {
    /// Stream structure version; libogg accepts only 0.
    pub version: u8,
    /// Header-type flags: 0x01 continued packet, 0x02 BOS, 0x04 EOS.
    pub flags: u8,
    pub serial: u32,
    /// Page sequence number.
    pub sequence: u32,
    /// Lacing values: a value `< 255` ends a packet, `255` continues it.
    pub lacing: &'a [u8],
    pub body: &'a [u8],
}

/// The result of [`parse_page`].
pub enum PageParse<'a> {
    /// A complete page and its length in bytes.
    Page(Page<'a>, usize),
    /// At least this many bytes are needed to parse the page.
    NeedMore(usize),
}

/// Parse the page at the start of `data` (`framing.c` decode side), checking its
/// CRC. `None` on a missing `OggS` capture pattern or a CRC mismatch. Pages are at
/// most 65 307 bytes, so callers can buffer incrementally.
pub fn parse_page(data: &[u8]) -> Option<PageParse<'_>> {
    if data.len() < 27 {
        return Some(PageParse::NeedMore(27));
    }
    if &data[..4] != b"OggS" {
        return None;
    }
    let nsegs = data[26] as usize;
    let body_start = 27 + nsegs;
    if data.len() < body_start {
        return Some(PageParse::NeedMore(body_start));
    }
    let lacing = &data[27..body_start];
    let body_end = body_start + lacing.iter().map(|&b| b as usize).sum::<usize>();
    if data.len() < body_end {
        return Some(PageParse::NeedMore(body_end));
    }

    // Verify the page CRC over the header (CRC field zeroed) + body.
    let stored = u32::from_le_bytes(data[22..26].try_into().ok()?);
    let mut header = data[..body_start].to_vec();
    header[22..26].fill(0);
    if crc_update(crc_update(0, &header), &data[body_start..body_end]) != stored {
        return None;
    }
    Some(PageParse::Page(
        Page {
            version: data[4],
            flags: data[5],
            serial: u32::from_le_bytes(data[14..18].try_into().ok()?),
            sequence: u32::from_le_bytes(data[18..22].try_into().ok()?),
            lacing,
            body: &data[body_start..body_end],
        },
        body_end,
    ))
}
