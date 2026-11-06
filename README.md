# libflac-rs

[![CI](https://github.com/trackIT-Systems/libflac-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/trackIT-Systems/libflac-rs/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/trackIT-Systems/libflac-rs)](https://github.com/trackIT-Systems/libflac-rs/releases)
[![MSRV](https://img.shields.io/badge/rustc-1.85+-blue)](Cargo.toml)
[![License](https://img.shields.io/badge/license-BSD--3--Clause-blue)](LICENSE)

A FLAC encoder and decoder in pure Rust whose encoder output is **byte-identical to
[libFLAC](https://xiph.org/flac/) 1.5.0** at the same settings.

Most FLAC encoders produce valid FLAC; this one produces *libFLAC's* FLAC, down to
the last byte. That matters when bytes must be reproduced rather than just decoded:
recreating the audio in MAME CHD files, verifying archives against reference
encodes, or replacing libFLAC in a pipeline without changing its output. It has no
dependencies, no build script and no `unsafe`.

## Features

- **Encoder**
  - compression levels 0–8 with libFLAC's presets
  - 8, 12, 16, 20, 24 and 32 bits per sample
  - 1–8 channels, with mid/side stereo decorrelation
  - output as a complete `.flac` file, as raw frames (what MAME/CHD stores), or as
    Ogg FLAC
  - every metadata block: STREAMINFO (with the audio MD5), VORBIS_COMMENT with
    tags, PADDING, APPLICATION, SEEKTABLE (filled in during encoding), CUESHEET,
    PICTURE
- **Decoder**
  - streaming from any `std::io::Read`, block by block with bounded memory, or
    into memory with an optional sample limit
  - native and Ogg FLAC, including chained Ogg streams; fixed and variable block
    sizes
  - MD5 verification, SEEKTABLE-based seeking, tags
  - missing frames are reported and filled with silence as libFLAC 1.5.0 does
  - lossless: it decodes everything the encoder produces and libFLAC's output
- Pure Rust, `#![forbid(unsafe_code)]`, zero dependencies, Rust 1.85+

## Installation

Releases are published on GitHub, not on crates.io:

```toml
[dependencies]
libflac-rs = { git = "https://github.com/trackIT-Systems/libflac-rs", tag = "v0.150.1" }
```

What changed between versions is in [`CHANGELOG.md`](CHANGELOG.md).

## Usage

```rust
use libflac_rs::{Encoder, EncoderConfig};

// 2 channels, 16 bits, 44.1 kHz at libFLAC's defaults (level 8, MD5 on).
let enc = Encoder::new(EncoderConfig::new(2, 16, 44_100));
let pcm: Vec<i32> = vec![0; 4096 * 2]; // interleaved: L R L R …

let flac: Vec<u8> = enc.encode(&pcm);          // a complete .flac file
let frames: Vec<u8> = enc.encode_frames(&pcm); // raw frames, as MAME/CHD stores them
let ogg: Vec<u8> = enc.encode_ogg(&pcm, 0);    // Ogg FLAC, logical stream serial 0

let decoded = libflac_rs::decode(&flac).unwrap();
assert_eq!(decoded.interleaved, pcm);
assert!(decoded.md5_ok);
```

Settings are adjusted with `with_compression_level`, `with_block_size` and
`with_md5`. For MAME's CHD audio codec (level 8, 2 channels, 16 bits, 44.1 kHz, MD5
off), use `EncoderConfig::chd(block_size)` with `encode_frames`.

Tags go into a VORBIS_COMMENT block; `vorbis_comment_entry` checks names and values
the way libFLAC does:

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

For long streams, files read from disk or the network, or untrusted input, decode
block by block instead of into memory. A few bytes of FLAC can legitimately decode
to gigabytes (digital silence compresses extremely well):

```rust
use libflac_rs::{DecodeError, StreamDecoder};
use std::{fs::File, io::BufReader};

fn play(path: &str) -> Result<(), DecodeError> {
    let mut decoder = StreamDecoder::new(BufReader::new(File::open(path)?))?;
    let header = decoder.header();
    println!("{} Hz, {} channels", header.sample_rate, header.channels);
    for block in &mut decoder {
        let block = block?; // one frame (or gap silence), interleaved
        // ... process block.interleaved ...
    }
    assert_eq!(decoder.md5_ok(), Some(true));
    Ok(())
}
```

`decode_with(data, &DecodeOptions { max_samples: Some(n) })` decodes into memory
but fails with `DecodeError::LimitExceeded` instead of growing past `n` samples
per channel. All decoders stop at the sample count STREAMINFO declares, when it
does.

The full API is documented in the crate docs (`cargo doc --open`).

## Compatibility

- **libFLAC version.** 0.150.x matches libFLAC 1.5.0; 0.143.x matched 1.4.3. The two
  differ only where 1.5.0 changed the encoder: loose mid-side at levels 1 and 4,
  constant 32-bit signals, seektable targets past the end, and the vendor string
  (details in [`CHANGELOG.md`](CHANGELOG.md)).
- **MAME / CHD.** The CHD configuration is unaffected by those changes, so CHD audio
  stays byte-identical to the libFLAC 1.4.3 that MAME ships.
- **Platforms.** Like libFLAC, the encoder takes `cos` and `log` from the platform's
  math library, whose last-bit rounding differs between glibc, musl and macOS. The
  output matches libFLAC built on the same platform and can differ between
  platforms in rare cases. Measurements are in [`ORACLE.md`](ORACLE.md).
- **Decoder.** Byte-identical output is an encoder property; the decoder is
  checked for producing the same samples as libFLAC 1.5.0's decoder, including
  chained Ogg links and the silence it inserts for missing frames. Unlike libFLAC,
  it does not resynchronize after a corrupt frame (bad CRC, or samples beyond the
  bit depth): decoding then fails.

## How it is verified

The separate `oracle/` crate compiles the C libFLAC 1.5.0 and libogg 1.3.5 from the
Xiph release tags and compares complete encoder outputs byte for byte, through the
public API. It covers all levels, bit depths, channel counts, block sizes, sample
rates and metadata blocks, and compares decoded samples with libFLAC's decoder. It
runs in CI on glibc and musl. The library itself never builds C.

```sh
cargo test                                            # the library: unit and round-trip tests
oracle/fetch-sources.sh && (cd oracle && cargo test)  # against the C reference
```

[`ORACLE.md`](ORACLE.md) describes the oracle; [`ROADMAP.md`](ROADMAP.md) records how
the port was built.

## License

BSD-3-Clause, retaining the Xiph.Org copyrights of the ported libFLAC and libogg
code. See [`LICENSE`](LICENSE).
