# libflac-rs

A pure-Rust, **bit-exact** port of [libFLAC](https://xiph.org/flac/) 1.5.0 — a
complete FLAC **encoder and decoder** whose output is **byte-identical** to the C
reference. Unlike a "produces valid FLAC" library, the goal is to *recreate the
exact bytes* libFLAC/MAME produce, so tools like `chd-rs` can reproduce and verify
CHD files losslessly. (`flacenc`, the existing pure-Rust encoder, is **not**
byte-identical to libFLAC — which is precisely why this exists.)

## Status — complete and byte-exact

Every byte is verified **against the real libFLAC** (and libogg for Ogg), compiled
from source by the separate `oracle/` test crate — the library itself is 100% pure
Rust. See [`ORACLE.md`](ORACLE.md):

- ✅ **Encoder**, byte-identical to libFLAC: compression levels **0–8**, bit depths
  **8/12/16/20/24/32**, mono / stereo (mid-side) / multichannel, the audio MD5, and
  every metadata block (STREAMINFO, VORBIS_COMMENT, PADDING, APPLICATION, SEEKTABLE,
  PICTURE, CUESHEET).
- ✅ **Decoder**: lossless and MD5-verified, with `seek()` and variable-block-size
  support.
- ✅ **Ogg FLAC** (encode + decode), byte-identical to libFLAC + libogg.
- ✅ Pure Rust, `#![forbid(unsafe_code)]`, **zero runtime dependencies**.

### libFLAC 1.5.0 vs. 1.4.3

Since 0.150.0 the target is libFLAC **1.5.0** (0.143.x targeted 1.4.3). The encoder
output differs from 1.4.3 only where 1.5.0 changed it:

- **Loose mid-side** (compression levels 1 and 4, stereo): a per-frame heuristic on
  the first differences of L/R vs. M/S replaces the re-decision every ~0.4 s.
- **Constant signals at 28–33-bit subframes** (32-bit input, non-zero value) are
  coded as CONSTANT instead of FIXED.
- **SEEKTABLE templates**: targets past the end of the stream become placeholders.
- The default **vendor string** is `reference libFLAC 1.5.0 20250211`.

None of these touch MAME/CHD's configuration (level 8, 16-bit stereo, raw frames),
so CHD audio stays byte-identical to the libFLAC 1.4.3 that MAME ships.

## Usage

```rust
use libflac_rs::{Encoder, EncoderConfig};

// 2-channel, 16-bit, 44.1 kHz, compression level 8 (libFLAC's defaults).
let enc = Encoder::new(EncoderConfig::new(2, 16, 44_100));
let pcm: Vec<i32> = vec![0; 4096 * 2]; // interleaved: L R L R …

let flac: Vec<u8> = enc.encode(&pcm);      // a complete .flac file
let frames: Vec<u8> = enc.encode_frames(&pcm); // raw frames (what MAME/CHD embeds)
let ogg: Vec<u8> = enc.encode_ogg(&pcm, 0);    // an Ogg FLAC stream

let decoded = libflac_rs::decode(&flac).unwrap();
assert_eq!(decoded.interleaved, pcm);
assert!(decoded.md5_ok);
```

To tag a file, add a VORBIS_COMMENT block with user comments; entries are checked
the way libFLAC checks them:

```rust
use libflac_rs::{Encoder, EncoderConfig, LIBFLAC_VENDOR_STRING, MetadataBlock, vorbis_comment_entry};

let enc = Encoder::new(EncoderConfig::new(1, 16, 384_000).with_compression_level(5));
let entries = [vorbis_comment_entry("TITLE", "Night 1").unwrap()];
let comments: Vec<&str> = entries.iter().map(String::as_str).collect();
let flac = enc.encode_with_metadata(
    &[0i32; 4096],
    &[MetadataBlock::VorbisCommentEntries { vendor: LIBFLAC_VENDOR_STRING, comments: &comments }],
);
let decoded = libflac_rs::decode(&flac).unwrap();
assert_eq!(decoded.vorbis_comment.unwrap().comments, ["TITLE=Night 1"]);
```

For the exact configuration MAME's CHD codec uses (level 8, 2ch/16-bit/44.1 kHz,
MD5 off), construct with `EncoderConfig::chd(block_size)` and call `encode_frames`.

## Pure Rust, zero dependencies

The crate is `#![forbid(unsafe_code)]`, edition 2024, with **no dependencies at all**
— not even a build script. There is no C in the repository.

```sh
cargo test   # pure-Rust unit + lossless round-trip tests (any platform)
```

Byte-exactness against the C reference is checked by the `oracle/` crate, which
compiles libFLAC 1.5.0 + libogg 1.3.5 and diffs complete outputs (CI runs it under
glibc, the libm parity target MAME/chdman use):

```sh
oracle/fetch-sources.sh && (cd oracle && cargo test)
```

See [`ORACLE.md`](ORACLE.md).

## License

BSD-3-Clause, retaining the Xiph.Org copyrights on the ported libFLAC and libogg
sources. See [`LICENSE`](LICENSE).
