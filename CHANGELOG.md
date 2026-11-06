# Changelog

Releases are GitHub releases of this repository (not crates.io). Versions follow
`0.<libFLAC digits>.<patch>`: the minor is the libFLAC release the encoder is
bit-exact with, the patch counts our own changes against it.

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
