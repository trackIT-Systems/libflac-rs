# ROADMAP — `libflac-rs`

An implementation plan for a **complete, bit-exact** Rust port of FLAC (libFLAC
1.4.3): encoder **and** decoder, byte-identical to the C reference. This is an
ordered build plan — phases run top to bottom; each lists concrete tasks, the C
reference to port, the data shapes, the bit-exactness traps, and how to verify.

**Current status:** the encoder is byte-identical to libFLAC for the **CHD/MAME
config** and has generalized to **all levels (0–8)**, **bit depths 8/12/16/20/24**,
and **complete `.flac` files** at **every standard bit depth (8–32)** — and a
**decoder** that losslessly round-trips the encoder and reads real libFLAC output
(MD5-verified). Phases **0–11 are DONE** — full metadata (incl. SEEKTABLE +
CUESHEET), a complete decoder with `seek()` and variable-block-size support,
**byte-exact Ogg FLAC** (encode + decode) verified against libFLAC+libogg, and a
documented, zero-dependency **public API** (`Encoder`/`EncoderConfig` + `decode*`).
All that remains is dispatching the crates.io release (a maintainer action).

The C differential oracle that established byte-exactness is **no longer vendored** —
the library is 100% pure Rust (no C, no build script, no dependencies). Byte-exactness
is checked by the separate `oracle/` crate, which fetches and compiles libFLAC +
libogg and runs in CI; see **[`ORACLE.md`](ORACLE.md)**. (The phase notes below
describe the original in-tree `cref` rig, preserved at the `c-oracle` tag.)

```
DONE   Phase 0  Bitwriter + CRC                      █████████
DONE   Phase 1  Fixed/constant/verbatim + framing    █████████
DONE   Phase 2  LPC float pipeline (parity gate)     █████████
DONE   Phase 3  Mid-side  → CHD target complete      █████████
DONE   Phase 4  Compression levels 0–8               █████████
DONE   Phase 5  Metadata + MD5 (full streams)        █████████
DONE   Phase 6  Bit depths 8/12/16/20/24 (RICE2)     █████████
DONE   Phase 7  32-bit / wide-residual paths         █████████
DONE   Phase 8  Metadata: APPLICATION + PICTURE +     █████████
                 SEEKTABLE + CUESHEET (all byte-exact)
DONE   Phase 9  The decoder: round-trips, reads real  █████████
                 libFLAC, seek() + variable blocksize
DONE   Phase 10 Ogg FLAC (byte-exact vs libogg)       █████████
DONE*  Phase 11 Public API + docs (release dispatch   ████████░
                 is the only step left)
```

### How to verify any phase (the rule that governs all work)

Nothing is "done" until it is **byte-identical to the compiled C oracle** (the
`cref` feature) across the corpus — not merely "valid FLAC." Build/test in
**WSL/glibc** so Rust's `f32`/`f64` transcendentals and the oracle's libm agree:

```sh
wsl bash -lc 'cd /mnt/c/Temp/mistercore/libflac-rs && \
  export CARGO_TARGET_DIR=$HOME/.cache/libflac-rs-target && cargo test --features cref'
cargo fmt && cargo clippy --all-targets --features cref -- -D warnings
```

The rig (`build.rs` + `cref/shim.c` + `tests/cref.rs`) compiles the vendored
libFLAC and diffs Rust output **byte-for-byte**. Each new phase: add a C entry
point to the shim (full-encoder knob and/or leaf function), add a corpus, diff at
the first mismatching field. Commit per phase via Windows git with explicit path
staging (the vendored `cref/vendor` tree is CRLF noise under WSL git — never
`git add -A`).

---

# DONE — Phases 0–6 (the encoder, on `main`)

These are complete and differentially verified; details in [CLAUDE.md](CLAUDE.md).
Kept here so the plan is whole.

### ✅ Phase 0 — Bitwriter + CRC  ·  `bitwriter.rs`, `crc.rs`
MSB-first bit packing, CRC-8 (header) / CRC-16 (footer), byte-exact vs
`FLAC__crc8/16`.

### ✅ Phase 1 — Fixed / constant / verbatim + framing  ·  `fixed.rs`, `subframe.rs`, `frame.rs`, `rice.rs`, `encoder.rs`
CONSTANT/VERBATIM/FIXED subframes, frame header/footer (UTF-8 frame number,
block-size + sample-rate hint codes), wasted-bits, and the **integer Rice
partition-order search**. Byte-exact fixed-only across constant/short/sine+noise.

### ✅ Phase 2 — LPC float pipeline (the parity gate)  ·  `window.rs`, `lpc/*`, `bitmath.rs`
`subdivide_tukey` windows → `double` autocorrelation → Levinson → order/precision
selection → quantization → residual, plus the `subdivide_tukey` a/b/c apodization
state machine. **This was the hard milestone** — every float stage matched
bit-for-bit (see *Float parity traps* below). Byte-exact LPC subframes.

### ✅ Phase 3 — Mid-side  ·  `encoder.rs`  →  **CHD target complete end-to-end**
Per-frame L/R vs L/S vs R/S vs M/S by summed estimated bits (side at +1 bps),
independent preferred on ties. The whole CHD/MAME slice (level 8, 2 ch/16-bit) is
byte-identical.

### ✅ Phase 4 — Compression levels 0–8  ·  `encoder.rs`
`Config`/`preset(0..=8)` reproduce `compression_levels_[]`: `tukey(p)` +
`subdivide_tukey(parts)` apodizations, per-level LPC-order / partition caps, and
`loose_mid_side` (the ~0.4 s periodic re-decision used by levels 1 & 4).

### ✅ Phase 5 — Metadata + MD5 (full streams)  ·  `md5.rs`, `metadata.rs`, `encoder::encode`
STREAMINFO, audio MD5 (RFC 1321 + the little-endian sample serialization),
VORBIS_COMMENT (vendor) + PADDING. The **entire** stream is byte-identical to
libFLAC's default output and round-trips through libFLAC's decoder.

### ✅ Phase 6 — Bit depths 8/12/16/20/24 (RICE2)  ·  `rice.rs`, `subframe.rs`, `encoder.rs`
`RicePartition::is_rice2` (set when any partition parameter reaches the escape
value 15), the `bps>16 ? 31 : 15` parameter limit, and the 5-bit parameter field.
Frames + full streams byte-exact for 8/12/16/20/24-bit.

### Float parity traps — RESOLVED (the reason this port exists)
The encoder decides in floating point; these had to match in IEEE-754:
`f32::cos == glibc cosf` (windows); `double` autocorrelation with plain `*`+`+`
(**no FMA** — MAME builds libFLAC without AVX, so the scalar `double` path is
canonical); Levinson `r/=err` in `double`; quantization `q=lround(error)` where
glibc `lround` is **half-away-from-zero** (C99) so `f64::round as i32` matches;
auto qlp precision (`min(.., 32-bps-ilog2(order))`, bps 17 on the side). All
verified leaf-by-leaf via `cref/shim.c`.

---

# ✅ DONE — Phase 7: 32-bit / wide-residual paths

**Status: implemented and verified** — 32-bit input is byte-identical at every
level (frames + full streams), so **all standard bit depths (8–32) are done.** The
channel signal is now carried as **`i64`** throughout, so `side = L−R` (33-bit) and
`mid = (L+R)>>1` never overflow; ≤24-bit values fit `i32` and stay byte-identical
to before. Beyond the overflow handling in the plan below, **five libFLAC quirks**
had to be matched exactly (and are precisely what the decoder, Phase 9, must
reverse):

- **Constant detection is disabled at `subframe_bps >= 28`** — the `_limit_residual`
  predictor's `CHECK_ORDER_IS_VALID` reports `rbps[1] = 34.0` (never `0.0`) for a
  constant signal, so a constant 32-bit signal becomes FIXED, not CONSTANT.
- **`get_wasted_bits_wide_` returns shift 1** for an all-zero 33-bit side (vs 0).
- **Fixed residuals wrap** (`i64`→`i32`) rather than bail; the order guess uses
  per-order validity (`|residual| > i32::MAX` ⇒ that order is invalid).
- **The fixed subframe is skipped when its estimated bits/sample ≥ `subframe_bps`**
  (`stream_encoder.c:3561`), leaving VERBATIM for an incompressible wide signal.
- **Rice selection short-circuits on `mean < 2`** (never forming `(mean-1)*divisor`)
  — which also fixed a latent panic on any all-zero partition.

The original implementation plan is kept below as the build record. **Overflow
context:** `side = L−R` is 33-bit and `mid = (L+R)>>1` plus the fixed/LPC residuals
exceed `i32`; the side is `i64` and the "wide"/"limit" residual routines handle it.

**Files:** `fixed.rs`, `lpc/residual.rs`, `lpc/mod.rs` (windowing), `subframe.rs`,
`encoder.rs`.

**C reference:** `stream_encoder.c:3210-3219` (side construction),
`:3240-3249` (wasted-bits-wide + `subframe_bps`), `evaluate_*_subframe_`
residual dispatch (`:3493-3503`, `:3990-4006`); `fixed.c:470-563`
(`FLAC__fixed_compute_residual` / `_wide` / `_wide_33bit`) and `:301-468`
(`_compute_best_predictor_wide` / `_limit_residual` / `_limit_residual_33bit`);
`lpc.c:832-938` (`_limit_residual` / `_limit_residual_33bit`), `:75-80`
(`FLAC__lpc_window_data_wide`).

### Tasks

1. **Side channel as `i64`.** When stream `bps == 32`, build the side as
   `i64`: `side[i] = (L[i] as i64) − (R[i] as i64)` (33-bit), and
   `mid[i] = ((L[i] as i64) + (R[i] as i64)) >> 1` (fits `i32`, store `i32`).
   For `bps < 32` keep the existing `i32` path. (libFLAC over-reads one sample —
   its loop is `i <= blocksize` — a buffer-padding artifact; only `[0,blocksize)`
   is encoded, so ignore it.) `subframe_bps` for the side is `bps − wasted + 1`
   (so **33**); mid is `bps − wasted`.
2. **Wasted bits, wide.** Port `get_wasted_bits_wide_` for the 33-bit side
   (OR-reduce `i64` magnitudes; trailing-zero count). It writes the shifted side
   back; the shift can be up to `bps`.
3. **Fixed predictor, wide.** Add `i64`-input residual + order selection:
   `compute_residual_wide` (data `i32`, accumulate `i64`) for 27 < bps ≤ 32 and
   `compute_residual_wide_33bit` (data `i64`) for the side; the order guess uses
   the matching `_limit_residual` / `_limit_residual_33bit` predictor. A residual
   that exceeds `i32` makes that order unusable (the C's limit variants return
   false → fall back to a lower order / verbatim).
4. **LPC residual, wide.** Port `_limit_residual` (`i32` data, `i64` accumulate,
   bail if `residual ∉ (INT32_MIN, INT32_MAX]`) and `_limit_residual_33bit`
   (`i64` data). Selection (`evaluate_lpc_subframe_`): if
   `FLAC__lpc_max_residual_bps > 32` use the limit variant (bail → return
   `None`); else the normal path. **Windowing:** the side feeds
   `FLAC__lpc_window_data_wide` (`i64 → f32`); precision is lost exactly as in C,
   so match the `as f32` cast.
5. **Subframe writers for 33-bit.** Warmup and CONSTANT values already go through
   `write_raw_i64`, so they handle 33 bits; add an `i64` **VERBATIM** writer and
   `i64` constant-detection for the side. Residual is always `i32` (guaranteed by
   the bail), so the rice writer is unchanged.
6. **Plumb the `i64` side through `ChosenChannel`/the mid-side decision.** The
   side candidate is evaluated from the `i64` buffer; the other three channels
   (L, R, mid) stay `i32`. The bit-summed L/R vs L/S vs R/S vs M/S decision is
   unchanged.

**Verify:** extend `bit_depths_match_c` and `wider_depth_full_stream_matches_c`
to include `bps = 32` (the generator already scales to it); add crafted
identical/anti/scaled L−R cases that force each channel assignment at 32-bit.

---

# DONE — Phase 8: full metadata blocks + user metadata API

**Status: DONE — all standard blocks emit byte-identically to libFLAC.**
`metadata::MetadataBlock` (VorbisComment / Padding / Application / Seektable /
CueSheet / Picture) is the user-metadata API; `encode()` takes an ordered
`&[MetadataBlock]` after STREAMINFO. APPLICATION, PICTURE, SEEKTABLE, and CUESHEET
all serialize **byte-identically to libFLAC** (verified via manually-filled C
metadata structs in the shim — which also revealed libFLAC auto-prepends its
default VORBIS_COMMENT to any user metadata) and round-trip through the decoder.
SEEKTABLE is *generated* during encoding: a template of target sample numbers
(build evenly-spaced ones with `metadata::spaced_seek_points`) is filled per-frame
in `encode_frames_inner` (`fill_seekpoints`) then sorted/uniquified at finish
(`metadata::seektable_sort`); the decoder parses it into
`DecodedStream::seek_points` (groundwork for Phase 9 `seek()`). CUESHEET is fully
caller-supplied (`MetadataBlock::CueSheet` + `CueSheetTrack`/`CueSheetIndex`),
including the non-byte-aligned reserved runs and nested track/index lists; the
decoder still skips it (no use until a metadata-read API in Phase 11).

**Key finding (SEEKTABLE fill):** libFLAC fills each point from
`FLAC__stream_encoder_get_blocksize()`, which `finish` lowers to the *actual*
sample count for the short final frame (`stream_encoder.c:1493`). So the match
range and the stored `frame_samples` use the current frame's real length, not the
configured block size — a point landing in the short final frame records that
shorter count.

**Goal:** emit every standard metadata block and let callers supply them, in
libFLAC's canonical order (STREAMINFO first, user blocks next, PADDING last, with
the `is_last` flag on the final block).

**Files:** `metadata.rs` (+ a public metadata type once Phase 11 lands).

**C reference:** `format.h:498-519` (block-type enum), `:872`
(`STREAM_METADATA_HEADER_LENGTH = 4`), `:598`
(`SEEKPOINT_LENGTH = 18`); `stream_encoder_framing.c` block writers; the
decoder's per-type readers (`stream_decoder.c:1404-1491`) document each layout.

**Block header (every block):** 1 bit `is_last` · 7 bits `type` · 24 bits
`length` (big-endian, bytes of body) · then the body.

### Block bodies to add (type code → layout)

- **SEEKTABLE (3): DONE.** N × 18-byte seekpoints, each = `sample_number: u64` ·
  `stream_offset: u64` (from first audio frame) · `frame_samples: u16`. A
  placeholder point is `sample_number = 0xFFFFFFFFFFFFFFFF`. Built during encoding
  (`fill_seekpoints` + `seektable_sort`), byte-exact vs libFLAC.
- **APPLICATION (2):** `id: [u8;4]` · application data (rest of `length`).
- **CUESHEET (5): DONE.** `media_catalog_number: [u8;128]` · `lead_in: u64` ·
  `is_cd: 1 bit` · `7+258×8 reserved bits (0)` · `num_tracks: u8` · then per track:
  `offset: u64` · `number: u8` · `isrc: [u8;12]` · `type: 1 bit` (0=audio) ·
  `pre_emphasis: 1 bit` · `6+13×8 reserved bits` · `num_indices: u8` · indices
  (`offset: u64` · `number: u8` · `3 reserved bytes`). Byte-exact vs libFLAC
  (`write_cuesheet`); the input must be legal for its `is_cd` flag, which the C
  validates at init (`FLAC__format_cuesheet_is_legal`).
- **PICTURE (6):** `type: u32` · `mime_len: u32` + MIME bytes · `desc_len: u32` +
  UTF-8 description · `width,height,depth,colors: u32` each · `data_len: u32` +
  image bytes. (All big-endian.)

### Tasks
1. A `MetadataBlock` enum (Padding, Application, Seektable, VorbisComment,
   Cuesheet, Picture) + writers; reuse the existing STREAMINFO/VORBIS_COMMENT.
2. Ordering + `is_last` handling in `encode`; recompute STREAMINFO
   `min/max framesize`, `total_samples`, MD5 after the audio pass (already done).
3. SEEKTABLE generation: collect `(sample_number, byte_offset, frame_samples)`
   per frame; support a caller-requested seekpoint interval (e.g. one/sec).

**Verify:** teach the shim to set the same blocks on the C encoder
(`FLAC__metadata_object_*` + `set_metadata`) and diff full streams; round-trip
through libFLAC's decoder and compare parsed blocks.

---

# DONE — Phase 9: the decoder (seek + variable block size; streaming API in Phase 11)

**Status: the decode logic is implemented and verified.** A pure-Rust decoder
(`bitreader.rs` + `decoder.rs`) turns FLAC bytes back into the exact original PCM
with CRC-8/CRC-16 and MD5 verification. It decodes **complete streams the C
reference produced** (`decode_libflac_streams`) and **round-trips the whole
encoder corpus losslessly** (`decode(encode(pcm)) == pcm`) across 8/12/16/20/24/32-bit,
mono+stereo, levels 0–8, with the embedded MD5 verifying. All restore math is
`i64`, so the 33-bit side channel is exact. **SEEKTABLE-driven `seek()`** is done
(`decode_seek` — jump to the nearest seek point, then decode forward to the exact
target sample; verified by exact-sample round-trips with and without a seektable),
and so is **variable-block-size decoding** (`read_utf8_u64` for the sample-number
header field, selected by the blocking-strategy bit). The only Phase 9 item left is
an **incremental/streaming decode API**, which folds into the Phase 11 public API.
The implementation record follows.

**Files (new):** `bitreader.rs`, `decoder.rs` (+ reuse `lpc::restore`,
`fixed::restore`, `crc`, `md5`, `metadata` parsing).

**C reference:** `bitreader.c` (bit input, `read_rice_signed_block`,
`read_utf8_uint32/64`), `stream_decoder.c` (`read_frame_header_`,
`read_subframe_{constant,fixed,lpc,verbatim}_`,
`read_residual_partitioned_rice_` with `is_extended` = RICE2,
`read_zero_padding_`), and `lpc.c:975+` / `fixed.c` restore functions.

### Sub-steps
1. **`bitreader.rs`** — MSB-first reader mirroring `bitwriter`: `read_raw_uN`,
   signed, unary, **rice signed block**, **UTF-8 u32/u64** (frame/sample number),
   running CRC-16. Unit-test against bytes the encoder produced.
2. **Stream + metadata parse** — the `fLaC` marker, then metadata blocks: parse
   STREAMINFO (min/max blocksize, min/max framesize, sample rate, channels, bps,
   total samples, MD5) into a header struct; parse or skip the rest (reuse
   Phase 8 layouts). Stop at the last block; frames follow.
3. **Frame header** — sync `0x3FFE` + reserved + blocking strategy; decode the
   blocksize / sample-rate / channel-assignment / bps codes and any hint bytes;
   read the frame/sample number (UTF-8); **verify CRC-8** over the header.
4. **Subframes** — per channel, read the 8-bit header (type + wasted-bits unary);
   dispatch:
   - CONSTANT → one value;  VERBATIM → `blocksize` raw samples;
   - FIXED(order) → `order` warmup + partitioned-rice residual →
     `fixed::restore_signal`;
   - LPC(order) → `order` warmup + `precision`/`shift` + `order` coeffs +
     residual → `lpc::restore_signal` (`data[i] = residual[i] + (Σ qlp·hist >> shift)`).
   - Residual: partitioned rice, RICE (4-bit) or **RICE2** (5-bit) per the method
     type; honor the escape parameter (raw bits) even though the encoder never
     emits it. Re-apply wasted-bits shift (`<< wasted`).
5. **Un-decorrelate channels** — reverse L/S, R/S, M/S to L/R (the M/S inverse
   recovers the dropped LSB from the side's parity); handle the 33-bit side for
   32-bit.
6. **Verify** — frame **CRC-16**; accumulate decoded PCM and check **MD5** vs
   STREAMINFO at end-of-stream.
7. **API + seeking** — SEEKTABLE-driven `seek(sample)` is **done** (`decode_seek`:
   find the nearest seek point ≤ target, decode forward to the exact sample). A
   streaming decode (feed bytes → pull samples) folds into Phase 11.

**Verify (decisive):** (a) **round-trip** — `decode(encode(pcm)) == pcm` over the
whole corpus and all configs; (b) decode files libFLAC produced and compare to
libFLAC's own decode, sample-for-sample, including MD5.

---

# DONE — Phase 10: Ogg FLAC (byte-exact vs libFLAC+libogg)

**Status: DONE — byte-identical to libFLAC+libogg.** `ogg.rs` is a faithful port of
the libogg 1.3.5 paging (`framing.c`) plus the FLAC-in-Ogg mapping that libFLAC's
`ogg_encoder_aspect.c` drives. `encoder::encode_ogg` produces an Ogg FLAC stream
byte-for-byte identical to libFLAC built with `FLAC__HAS_OGG=1`+libogg, and
`decoder::decode_ogg` reads pages back to PCM (round-trips + decodes real libFLAC
Ogg). Still pure-Rust / zero-dependency — libogg is **not** a runtime dependency,
only a dev-only oracle vendored under `cref/vendor/ogg`.

**The byte-exact crux** was reproducing libogg's paging exactly: the CRC-32
(polynomial `0x04c11db7`, **unreflected**, init/final 0); lacing (`bytes/255 + 1`
segments, trailing `bytes % 255`); and `ogg_stream_flush_i`'s page-decision logic —
the BOS page holds exactly the first packet, metadata packets are each flushed, and
audio packets accumulate via `pageout` until a *nominal* page (`> 4096` body bytes
**and** `≥ 4` complete packets) or 255 segments, with the final page forced at EOS.

**Mapping details that mattered:**
- BOS packet (51 bytes): `0x7F` · `"FLAC"` · `1` · `0` · header count **`0`**
  (= "unknown"; libFLAC never back-patches it) · `"fLaC"` · STREAMINFO (with
  `is_last = 0`, since the auto VORBIS_COMMENT follows).
- libFLAC **drops any SEEKTABLE** for Ogg (`stream_encoder.c:773`).
- Granule position = cumulative samples through the page's last completed packet
  (0 for the BOS/metadata pages); the per-frame packet granule is the running
  sample count *after* that frame.
- STREAMINFO is back-patched at finish in C (seek + rewrite + page-CRC); we build
  it correct in one pass, which yields the identical bytes.

**Oracle:** `build.rs` now compiles the oracle with `FLAC__HAS_OGG=1`, the four
libFLAC Ogg-layer TUs (`ogg_encoder_aspect.c`, `ogg_decoder_aspect.c`,
`ogg_helper.c`, `ogg_mapping.c`), and vendored libogg (`framing.c`, `bitwise.c`).
The Ogg paths are runtime-gated on `is_ogg`, so native output stays byte-identical
(the pre-existing differential tests still pass). The shim's
`libflac_rs_cref_encode_ogg` uses `FLAC__stream_encoder_init_ogg_stream` with a
fixed serial number; `ogg_stream_matches_c` diffs full streams and
`decode_ogg_libflac_streams` round-trips real libFLAC Ogg.

---

# DONE* — Phase 11: public API, docs, publish (code ready; release dispatch left)

**Status: the public API, docs, and packaging are done; only the actual crates.io
release dispatch remains** (a maintainer action — tag + run the publish workflow).

1. **Public API — DONE.** A crate-root surface, no more `#![allow(dead_code)]`
   blanket (only two targeted allows on faithful bit-writer mirror methods):
   ```rust
   pub struct EncoderConfig { channels, bits_per_sample, sample_rate, block_size,
                              compression_level, md5 }
   impl EncoderConfig { fn new(ch, bps, rate); fn chd(block_size); with_* builders }
   pub struct Encoder(EncoderConfig);
   impl Encoder {
       fn encode(&self, &[i32]) -> Vec<u8>;                  // full .flac (default VC)
       fn encode_with_metadata(&self, &[i32], &[MetadataBlock]) -> Vec<u8>;
       fn encode_frames(&self, &[i32]) -> Vec<u8>;           // raw frames (CHD)
       fn encode_ogg(&self, &[i32], serial: i32) -> Vec<u8>; // Ogg FLAC
   }
   // free fns: decode, decode_frames, decode_ogg, decode_seek -> Decoded{Stream,Frames}/SeekResult
   // metadata: MetadataBlock, SeekPoint, CueSheetTrack/Index, LIBFLAC_VENDOR_STRING, spaced_seek_points
   ```
   (One-shot, not a streaming push/pull — sufficient for the CHD use case; a
   streaming API can be added later, additively.)
2. **Docs — DONE.** Crate-level guide with worked encode/decode examples (3
   doctests), per-type docs; module `*.c:NNN` citations kept accurate.
3. **CI / packaging — DONE.** Pure-Rust matrix (Linux/Windows/macOS) + Linux/glibc
   differential + lint (fmt/clippy/doc); `#![forbid(unsafe_code)]`, edition 2024,
   MSRV 1.85, **zero runtime deps**; `cref/` + `build.rs` + vendored C excluded from
   the package (verified: `cargo publish --dry-run` builds the slim tarball — 30
   files, no leaks). The `publish-crates-io.yml` workflow (tag + dispatch, needs
   `CARGO_REGISTRY_TOKEN`) does the release; **not yet dispatched.**
4. **CHD glue stays in `chd-rs`** — the `'L'`/`'B'` endian-flag byte + both-endian
   trial (raw `flac` codec) and the audio→FLAC / subcode→deflate split (`cdfl`)
   wrap this crate; out of scope here.

---

## Definition of "fully implemented"

FLAC support is complete when, across the corpus and **all** standard configs
(levels 0–8, 8/12/16/20/24/32-bit, mono/stereo, every metadata block):

1. **Encode** is byte-identical to libFLAC (frames **and** full files). — *done
   except 32-bit + extra metadata (Phases 7–8).*
2. **Decode** reproduces the exact input PCM and verifies CRC-16 + MD5; round-trip
   is lossless for every corpus item. — *Phase 9.*
3. Optional **Ogg** encapsulation matches the reference. — *Phase 10.*
4. A documented, stable, zero-dependency public API ships on crates.io. — *Phase 11.*
