# Changelog

Releases are GitHub releases of this repository (not crates.io). Versions follow
`0.<libFLAC digits>.<patch>`: the minor is the libFLAC release the encoder is
bit-exact with, the patch counts our own changes against it.

## Unreleased

### Added

- `StreamDecoder`: decodes native FLAC, raw frames or Ogg FLAC from any
  `std::io::Read`, one `DecodedBlock` (a frame, or gap silence) at a time, so
  memory stays bounded for long streams and untrusted input. Chained Ogg links
  via `next_link`; the header (`StreamHeader`), gaps and MD5 result are available
  as it goes. Frames over 16 MiB (the STREAMINFO limit) are rejected.
- `decode_with` with `DecodeOptions { max_samples }`: decode into memory, but fail
  with `DecodeError::LimitExceeded` before the output passes the limit. A small
  file can otherwise legitimately expand to gigabytes (measured: 36 bytes per
  2 MiB of silence, 72 bytes to 84 MB through a filled gap).

- Encoder input validation (`EncodeError`): `Encoder::try_new` and
  `try_encode`, `try_encode_frames`, `try_encode_with_metadata`, `try_encode_ogg`
  reject what libFLAC rejects — channels outside 1–8, bit depths outside 4–32,
  sample rates above 1 048 575 Hz, block sizes outside 16–65535, samples that do
  not fit the bit depth, input not a whole number of samples per channel, and
  metadata libFLAC's `init` refuses (duplicate SEEKTABLE or VORBIS_COMMENT,
  unsorted seek points, illegal cue sheets or pictures) or the format cannot hold
  (blocks of 16 MiB or more, more than 255 cue sheet tracks or index points,
  Vorbis comments that are not `NAME=value`).
- Every bit depth from 4 to 32, in the encoder (byte-identical to libFLAC) and the
  decoder.

### Changed

- `Encoder::new` and the `encode*` methods panic with the `EncodeError` on
  invalid settings or input. Before, they wrote a corrupt stream or decoded to
  different samples without notice.
- Decoding stops at STREAMINFO's sample count when it is set: data after the
  declared end is no longer read (before, it was decoded and then discarded, and
  trailing garbage made decoding fail).
- The whole-buffer decode functions run on `StreamDecoder`.

### Fixed

- Samples outside the bit depth were encoded without error and decoded to
  different values; metadata blocks of 16 MiB or more overflowed their length
  field and corrupted the stream. Both are now rejected.
- The decoder rejected valid FLAC at bit depths other than 8/12/16/20/24/32 (frame
  headers that defer the depth to STREAMINFO), e.g. libFLAC's 18- or 28-bit
  output.
- A total sample count beyond STREAMINFO's 36 bits is written as 0 ("unknown"),
  as in libFLAC.
- The decoder no longer panics on malformed input. It now rejects what libFLAC
  rejects (wasted bits not below the bit depth, residual partitions smaller than
  the predictor order or not dividing the block, reserved coding methods, FIXED
  orders and LPC precisions, negative LPC shifts) and uses wrapping arithmetic
  where garbage can overflow; such input makes `decode*` return `None`. Found by
  fuzzing; `tests/malformed_input.rs` decodes 30 000 mutated streams in CI.

### Development

- CI runs with a read-only token, actions are pinned to commit SHAs and kept
  current by Dependabot, and `oracle/fetch-sources.sh` refuses libFLAC/libogg
  checkouts that are not at the expected commits.

## 0.150.1

First release targeting **libFLAC 1.5.0** (`v0.150.0` is a tag only). Changes
since 0.143.1:

### Changed

- **Target is libFLAC 1.5.0.** The encoder output is byte-identical to libFLAC
  1.5.0 built on the same platform. It differs from 1.4.3 only where 1.5.0 changed:
  - loose mid-side (compression levels 1 and 4, stereo) picks L/R or M/S per frame
    by a first-difference heuristic instead of re-deciding every ~0.4 s;
  - constant non-zero signals at 28–33-bit subframes (32-bit input) are coded as
    CONSTANT instead of FIXED;
  - SEEKTABLE template targets past the end of the stream become placeholders;
  - the default vendor string is `reference libFLAC 1.5.0 20250211`
    (`LIBFLAC_VENDOR_STRING`).

  MAME/CHD's configuration (level 8, 16-bit stereo, raw frames) is unaffected.
- **Default block size** follows libFLAC: `EncoderConfig::new` leaves `block_size`
  at 0, which `Encoder::new` resolves to 1152 for levels 0–2 (no LPC) and 4096
  otherwise. Before, it was always 4096. Set it explicitly with `with_block_size`
  to keep the old behaviour.

### Added

- VORBIS_COMMENT user comments: `MetadataBlock::VorbisCommentEntries`, and
  `vorbis_comment_entry` to build `NAME=value` entries with libFLAC's checks. The
  decoder returns the block as `DecodedStream::vorbis_comment` (native and Ogg).
- The C differential oracle runs in CI: `oracle/` compiles libFLAC 1.5.0 + libogg
  1.3.5 and compares whole outputs through the public API, on glibc and musl. It
  also compares decoded samples with libFLAC's decoder.
- libFLAC 1.5.0's decoder changes:
  - `decode_ogg_chained` decodes every link of a chained Ogg FLAC stream, one
    `DecodedStream` each. `decode_ogg` decodes the first FLAC logical stream and
    ignores pages of other streams, as libFLAC does by default (chained files used
    to fail).
  - Missing frames: positions come from the frame headers, gaps are filled with
    silence under libFLAC's rules (same format, at most 5 s and 50 blocks) and
    listed in the new `gaps` field of `DecodedStream`, `DecodedFrames` and
    `SeekResult` (`Gap`). Before, the samples after a gap were shifted.
  - A frame whose samples exceed its bit depth makes decoding fail, as libFLAC
    rejects it (`OUT_OF_BOUNDS`); before, the samples wrapped.

### Fixed

- With no input samples, STREAMINFO's `min_framesize` is `0xFFFFFF` as in libFLAC
  (was 0).

### Notes

- Like libFLAC, the encoder uses the platform libm (`cosf`, `log`). Output can
  differ between glibc, musl and macOS in rare cases (14 of 2916 test streams
  between glibc and musl); see `ORACLE.md`.
- The decoder stops at a corrupt frame (CRC mismatch, out-of-range samples) where
  libFLAC resynchronizes and continues.
