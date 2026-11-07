//! Bit-level reader, the decode-side mirror of [`crate::bitwriter`]
//! (`bitreader.c`). Reads big-endian / MSB-first from a byte slice, tracking an
//! absolute bit position so the decoder can CRC byte-aligned regions (frame
//! header, whole frame) directly against the backing slice.
//!
//! Unlike the encoder's bit *writer*, the reader does not need to reproduce
//! libFLAC's internal word machinery byte-for-byte — only to read back the exact
//! values the format encodes. Correctness is established by round-tripping against
//! [`crate::bitwriter::BitWriter`] and, at the stream level, by decoding real
//! libFLAC output back to the original PCM.

/// Reads MSB-first from `data`. `None` from any read means the stream ended
/// (truncated/!malformed); the decoder turns that into an error.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute position in bits from the start of `data`.
    pos: usize,
    /// A read failed because `data` ended (rather than because a value was
    /// invalid), so more input might complete it.
    ran_out: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            ran_out: false,
        }
    }

    /// Whether a read failed for lack of data (see the field).
    pub fn ran_out(&self) -> bool {
        self.ran_out
    }

    /// Current byte offset (only meaningful when byte-aligned).
    pub fn byte_pos(&self) -> usize {
        self.pos / 8
    }

    /// Current position in bits.
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    pub fn is_byte_aligned(&self) -> bool {
        self.pos % 8 == 0
    }

    fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// Read the low `bits` bits (`<= 32`) MSB-first into a `u32`.
    pub fn read_raw_u32(&mut self, bits: u32) -> Option<u32> {
        debug_assert!(bits <= 32);
        if bits == 0 {
            return Some(0);
        }
        if bits as usize > self.bits_left() {
            self.ran_out = true;
            return None;
        }
        let mut v = 0u32;
        for _ in 0..bits {
            let byte = self.data[self.pos >> 3];
            let bit = (byte >> (7 - (self.pos & 7))) & 1;
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        Some(v)
    }

    /// Read the low `bits` bits (`<= 64`) MSB-first into a `u64`.
    pub fn read_raw_u64(&mut self, bits: u32) -> Option<u64> {
        debug_assert!(bits <= 64);
        if bits > 32 {
            let hi = self.read_raw_u32(bits - 32)? as u64;
            let lo = self.read_raw_u32(32)? as u64;
            Some((hi << 32) | lo)
        } else {
            Some(self.read_raw_u32(bits)? as u64)
        }
    }

    /// Read `bits` bits as a two's-complement signed value (sign-extended).
    pub fn read_signed(&mut self, bits: u32) -> Option<i64> {
        let v = self.read_raw_u64(bits)?;
        Some(sign_extend(v, bits))
    }

    /// Count zero bits up to and consuming the terminating `1`
    /// (`FLAC__bitreader_read_unary_unsigned`); `None` past `u32::MAX`.
    pub fn read_unary(&mut self) -> Option<u32> {
        let mut count = 0u32;
        loop {
            if self.read_raw_u32(1)? == 1 {
                return Some(count);
            }
            count = count.checked_add(1)?;
        }
    }

    /// Read one Rice-coded signed value with the given parameter
    /// (`FLAC__bitreader_read_rice_signed`): unary MSBs, `parameter` LSBs, then
    /// zigzag-unfold.
    /// A quotient that cannot belong to a 32-bit residual (`msbs > u32::MAX >>
    /// parameter`) is invalid, as in libFLAC's `read_rice_signed_block`.
    pub fn read_rice_signed(&mut self, parameter: u32) -> Option<i32> {
        let msbs = self.read_unary()?;
        if msbs > u32::MAX >> parameter {
            return None;
        }
        let lsbs = self.read_raw_u32(parameter)?;
        let uval = (msbs << parameter) | lsbs;
        // zigzag: odd -> -(uval>>1)-1, even -> uval>>1.
        Some(((uval >> 1) as i32) ^ -((uval & 1) as i32))
    }

    /// Read `n` Rice-coded signed values into `out`.
    pub fn read_rice_signed_block(&mut self, out: &mut [i32], parameter: u32) -> Option<()> {
        for o in out.iter_mut() {
            *o = self.read_rice_signed(parameter)?;
        }
        Some(())
    }

    /// Decode a UTF-8-style coded integer (`FLAC__bitreader_read_utf8_uint32`),
    /// used for the frame/sample number. Returns `0xFFFF_FFFF` on a malformed
    /// sequence (matching the C sentinel), `None` only on truncation.
    pub fn read_utf8_u32(&mut self) -> Option<u32> {
        let x = self.read_raw_u32(8)?;
        let (mut v, n) = if x & 0x80 == 0 {
            (x, 0)
        } else if x & 0xC0 != 0 && x & 0x20 == 0 {
            (x & 0x1F, 1)
        } else if x & 0xE0 != 0 && x & 0x10 == 0 {
            (x & 0x0F, 2)
        } else if x & 0xF0 != 0 && x & 0x08 == 0 {
            (x & 0x07, 3)
        } else if x & 0xF8 != 0 && x & 0x04 == 0 {
            (x & 0x03, 4)
        } else if x & 0xFC != 0 && x & 0x02 == 0 {
            (x & 0x01, 5)
        } else {
            return Some(0xFFFF_FFFF);
        };
        for _ in 0..n {
            let x = self.read_raw_u32(8)?;
            if x & 0x80 == 0 || x & 0x40 != 0 {
                return Some(0xFFFF_FFFF);
            }
            v = (v << 6) | (x & 0x3F);
        }
        Some(v)
    }

    /// Decode a UTF-8-style coded 36-bit value (`FLAC__bitreader_read_utf8_uint64`),
    /// used for the sample number of a variable-block-size frame. Like
    /// [`Self::read_utf8_u32`] with one extra (7-byte, `0xFE` lead) length. Returns
    /// `0xFFFF_FFFF_FFFF_FFFF` on a malformed sequence, `None` only on truncation.
    pub fn read_utf8_u64(&mut self) -> Option<u64> {
        let x = self.read_raw_u32(8)? as u64;
        let (mut v, n) = if x & 0x80 == 0 {
            (x, 0)
        } else if x & 0xC0 != 0 && x & 0x20 == 0 {
            (x & 0x1F, 1)
        } else if x & 0xE0 != 0 && x & 0x10 == 0 {
            (x & 0x0F, 2)
        } else if x & 0xF0 != 0 && x & 0x08 == 0 {
            (x & 0x07, 3)
        } else if x & 0xF8 != 0 && x & 0x04 == 0 {
            (x & 0x03, 4)
        } else if x & 0xFC != 0 && x & 0x02 == 0 {
            (x & 0x01, 5)
        } else if x & 0xFE != 0 && x & 0x01 == 0 {
            (0, 6) // 7-byte form: the 0xFE lead carries no value bits
        } else {
            return Some(0xFFFF_FFFF_FFFF_FFFF);
        };
        for _ in 0..n {
            let x = self.read_raw_u32(8)? as u64;
            if x & 0x80 == 0 || x & 0x40 != 0 {
                return Some(0xFFFF_FFFF_FFFF_FFFF);
            }
            v = (v << 6) | (x & 0x3F);
        }
        Some(v)
    }

    /// Skip `n` bytes (must be byte-aligned); used to step over metadata blocks.
    /// Returns `None` if that runs past the end.
    pub fn skip_bytes(&mut self, n: usize) -> Option<()> {
        debug_assert!(self.is_byte_aligned());
        if n * 8 > self.bits_left() {
            self.ran_out = true;
            return None;
        }
        self.pos += n * 8;
        Some(())
    }

    /// The backing bytes in `[start, self.byte_pos())` — for CRC over a
    /// byte-aligned region just read.
    pub fn bytes_since(&self, start: usize) -> &'a [u8] {
        &self.data[start..self.pos / 8]
    }
}

/// Sign-extend the low `bits` of `v` to `i64`.
fn sign_extend(v: u64, bits: u32) -> i64 {
    if bits == 0 || bits >= 64 {
        return v as i64;
    }
    let shift = 64 - bits;
    ((v << shift) as i64) >> shift
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitwriter::BitWriter;

    #[test]
    fn raw_round_trips() {
        let mut bw = BitWriter::new();
        bw.write_raw_u32(0xABCD, 16);
        bw.write_raw_u32(0b101, 3);
        bw.write_raw_u32(0x1FFFFF, 21);
        bw.write_raw_u64(0x1_2345_6789, 33);
        bw.zero_pad_to_byte_boundary();
        let bytes = bw.as_bytes().to_vec();

        let mut br = BitReader::new(&bytes);
        assert_eq!(br.read_raw_u32(16), Some(0xABCD));
        assert_eq!(br.read_raw_u32(3), Some(0b101));
        assert_eq!(br.read_raw_u32(21), Some(0x1FFFFF));
        assert_eq!(br.read_raw_u64(33), Some(0x1_2345_6789));
    }

    #[test]
    fn signed_round_trips() {
        let mut bw = BitWriter::new();
        for &(v, bits) in &[
            (-1i64, 4),
            (-2, 4),
            (32767, 16),
            (-32768, 16),
            (-(1 << 32), 33),
        ] {
            bw.write_raw_i64(v, bits);
        }
        bw.zero_pad_to_byte_boundary();
        let bytes = bw.as_bytes().to_vec();
        let mut br = BitReader::new(&bytes);
        for &(v, bits) in &[
            (-1i64, 4),
            (-2, 4),
            (32767, 16),
            (-32768, 16),
            (-(1 << 32), 33),
        ] {
            assert_eq!(br.read_signed(bits), Some(v), "{v} @ {bits} bits");
        }
    }

    #[test]
    fn unary_round_trips() {
        let mut bw = BitWriter::new();
        for v in [0u32, 1, 3, 10, 0, 200] {
            bw.write_unary_unsigned(v);
        }
        bw.zero_pad_to_byte_boundary();
        let bytes = bw.as_bytes().to_vec();
        let mut br = BitReader::new(&bytes);
        for v in [0u32, 1, 3, 10, 0, 200] {
            assert_eq!(br.read_unary(), Some(v));
        }
    }

    #[test]
    fn rice_block_round_trips() {
        let vals: Vec<i32> = vec![0, -1, 1, -2, 2, 100, -100, 5000, -5000, 7, -3];
        for param in [0u32, 1, 4, 8, 14, 30] {
            let mut bw = BitWriter::new();
            bw.write_rice_signed_block(&vals, param);
            bw.zero_pad_to_byte_boundary();
            let bytes = bw.as_bytes().to_vec();
            let mut br = BitReader::new(&bytes);
            let mut out = vec![0i32; vals.len()];
            br.read_rice_signed_block(&mut out, param).unwrap();
            assert_eq!(out, vals, "param {param}");
        }
    }

    #[test]
    fn utf8_round_trips() {
        for v in [
            0u32, 0x7F, 0x80, 0x7FF, 0x800, 0xFFFF, 0x10_FFFF, 0x1F_FFFF, 0x20_0000,
        ] {
            let mut bw = BitWriter::new();
            bw.write_utf8_u32(v);
            bw.zero_pad_to_byte_boundary();
            let bytes = bw.as_bytes().to_vec();
            let mut br = BitReader::new(&bytes);
            assert_eq!(br.read_utf8_u32(), Some(v), "utf8 {v:#x}");
        }
    }

    #[test]
    fn utf8_u64_round_trips() {
        // Through the 7-byte form: 0x8000_0000 first needs it; 0xF_FFFF_FFFF is the
        // largest 36-bit value (a variable-block-size sample number).
        for v in [
            0u64,
            0x7F,
            0x80,
            0x7FF,
            0x800,
            0xFFFF,
            0x1F_FFFF,
            0x20_0000,
            0x7FFF_FFFF,
            0x8000_0000,
            0xF_FFFF_FFFF,
        ] {
            let mut bw = BitWriter::new();
            bw.write_utf8_u64(v);
            bw.zero_pad_to_byte_boundary();
            let bytes = bw.as_bytes().to_vec();
            let mut br = BitReader::new(&bytes);
            assert_eq!(br.read_utf8_u64(), Some(v), "utf8_u64 {v:#x}");
        }
    }
}
