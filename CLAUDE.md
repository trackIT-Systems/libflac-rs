# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A pure-Rust, **bit-exact** port of **libFLAC 1.5.0** (originally 1.4.3; see R1
under Milestones) — ultimately a *complete*
encoder **and** decoder whose output is byte-identical to the C reference.
`flacenc` (the existing pure-Rust encoder) is **not** byte-identical to libFLAC;
that is precisely why this exists: `chd-rs` and others need to *recreate* the exact
bytes MAME/libFLAC produce, not merely emit a valid FLAC stream.

**The first target is the configuration CHD/MAME uses** (level 8, 2ch/16-bit/
44.1 kHz, streamable subset off, MD5 off, fixed block size) — built first because
its float pipeline (windows → autocorrelation → Levinson → quantization) is the
linchpin every config shares, and finishing one config end-to-end de-risks the
float parity before fanning out. The project then **generalizes**: all bit depths
(RICE2 + wide paths), all compression levels 0–8, the full apodization-window set,
metadata blocks, MD5, the decoder, and (optionally) Ogg FLAC. See Milestones.

**Success criterion:** for every input in the corpus, `libflac-rs` output equals
the C libFLAC bytes for the same settings — verified live against the compiled C
oracle (the `oracle/` crate), which can be configured for *any* libFLAC settings.

This is bit-exactness-first work: a "working" codec that produces valid FLAC is
**not** the goal if it differs from the C output by a single byte.

## Commands

```sh
# Pure-Rust build/test (any platform); the whole repo is pure Rust, zero deps.
cargo build
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps        # with RUSTDOCFLAGS="-D warnings", as CI does
```

```sh
# Byte-exactness vs the C reference: a separate workspace that compiles libFLAC
# 1.5.0 + libogg 1.3.5 (CI job `oracle`, ubuntu-latest).
oracle/fetch-sources.sh    # once; clones the Xiph tags into oracle/vendor/
(cd oracle && cargo test)
```

Run the oracle after any change that can move an output byte. See **`ORACLE.md`**.

## Source of truth: the C libFLAC oracle

The differential oracle is **libFLAC 1.5.0** (Xiph.Org, BSD-3-Clause), compiled by
the `oracle/` crate from sources fetched into `oracle/vendor/` (not committed). Its
`build.rs` compiles the scalar reference path; `shim.c` drives the encoder for raw
frames (write-callback `samples > 0`, discarding the stream marker + metadata exactly
as CHD does), full streams, or Ogg FLAC. When porting or debugging a divergence,
**read the C** in `oracle/vendor/flac/src/libFLAC` — do not reconstruct logic from
memory. Module doc-comments cite specific `*.c:NNN` lines; keep those citations
accurate.

The port ↔ C-file correspondence (scalar reference path only — see the SIMD note
below):

| C file | Rust module | Role |
| --- | --- | --- |
| `bitwriter.c`, `crc.c` | `bitwriter` | bit packing, CRC-8/CRC-16 |
| `fixed.c` | `fixed` | fixed predictors (orders 0–4) + best-order |
| `stream_encoder_framing.c` | `frame`, `subframe` | frame/subframe headers + footer |
| `window.c` | `window` | apodization (`subdivide_tukey(3)`) |
| `lpc.c` | `lpc/*` | windowing, autocorrelation, Levinson, quantize, residual |
| `stream_encoder.c` | `encoder`, `rice` | orchestration, partition search, mid-side |
| `format.c` | (constants) | header/validation constants |
| `memory.c`, `md5.c` | — | replaced by Rust / skipped (MD5 off) |

**Out of scope:** every `*_intrin_*` / `*_asm_*` translation unit, and the
`metadata_object.c` helpers (we fill metadata structs directly). The decoder, the
full metadata blocks, and Ogg FLAC are all now **in** scope and done (see
Milestones): the oracle additionally compiles libFLAC's Ogg layer (`ogg_*.c`) with
`FLAC__HAS_OGG=1` plus vendored **libogg** (`cref/vendor/ogg`) for the Ogg-FLAC
differential tests. License is BSD-3-Clause; retain the Xiph copyrights for both
libFLAC and libogg (see LICENSE).

## The exact settings CHD uses

`flac.cpp:77` / `chdcodec.cpp`: `set_verify(false)` (MD5 off),
`set_compression_level(8)`, `set_channels(2)`, `set_bits_per_sample(16)`,
`set_sample_rate(44100)`, `set_total_samples_estimate(0)`,
`set_streamable_subset(false)`, `set_blocksize(block_size)` (typically **2048**).

Level-8 preset (`stream_encoder.c:140`, last row): `do_mid_side=true`,
`loose_mid_side=false`, `max_lpc_order=12`, `qlp_coeff_precision=0` (auto),
`do_qlp_coeff_prec_search=false`, `do_escape_coding=false`,
`do_exhaustive_model_search=false`, `min_residual_partition_order=0`,
`max_residual_partition_order=6`, `rice_parameter_search_dist=0`,
apodization `"subdivide_tukey(3)"`.

## SIMD vs scalar — RESOLVED: scalar is the correct target

The encoder makes its float decisions (windows, autocorrelation, LPC quant) where
rounding must match the C reference. libFLAC ships SIMD autocorrelation variants
that *can* diverge from the scalar `double` path (notably FMA: one rounding vs
two). **But MAME compiles libFLAC without `FLAC__USE_AVX`** (verified:
`scripts/src/3rdparty.lua` defines `HAVE_LROUND=1`, `ENABLE_64_BIT_WORDS=1`,
`FLAC__HAS_OGG=0`, `NDEBUG`, `HAVE_CONFIG_H`, … but **never `FLAC__USE_AVX`**), and
`cpu.h` gates `FLAC__AVX2_SUPPORTED`/`FLAC__FMA_SUPPORTED` behind `FLAC__USE_AVX`.
So the FMA/AVX2 autocorrelation is **compiled out** of chdman.

At level 8 (`max_lpc_order = 12`, so `lag = 13`) the dispatch
(`stream_encoder.c:1034-1044`) therefore selects **`..._intrin_sse2_lag_14`**,
which is plain `double`, no fused multiply. The scalar function itself
(`lpc.c:133`) uses the same `deduplication/lpc_compute_autocorrelation_intrin.c`
template (`autoc[j] += (double)data[i]*(double)data[i-j]`, MAX_LAG 8/12/16) for
`lag ≤ 16`. Empirically, **scalar == MAME's exact SSE2 config, byte-identical
across 60 randomized multi-partial+noise signals** (and structurally: same double
math, no FMA).

**Implication:** port the **scalar** autocorrelation template (plain `*` then `+`,
no `mul_add`). The oracle is built with `FLAC__NO_ASM`, which is deterministic and
platform-independent and matches x86 chdman. *Caveat:* CHDs created on ARM (NEON
has FMA) could differ; the x86 path is the canonical target. To re-verify, build
libFLAC two ways (scalar `-DFLAC__NO_ASM` vs MAME's full define set + the
`*_intrin_*.c` files) and diff frame output — they must stay identical.

## Bit-exactness hazards (FLOAT is the enemy)

1. **Apodization windows** (`window.c`): coefficients via `cosf`/`fabsf` (f32).
   Reproduce the exact float expressions and evaluation order. Build/run under
   glibc so Rust's `f32::cos` and the oracle's `cosf` are the same libm; if any
   per-coefficient drift appears, embed reference-computed tables.
2. **Autocorrelation** (`lpc.c` template): accumulate in **`double`**, operands
   promoted from `f32`. Plain `*` then `+` (NO `mul_add` — see the SIMD note).
   Keep the exact loop/accumulation order.
3. **Levinson-Durbin** (`lpc.c:176`): reflection coeff `r /= err` in `double`,
   early `if(err == 0.0)` exit, round-to-nearest-even.
4. **LPC quantization** (`lpc.c:~265`): `q = lround(error); error -= q;` with error
   feedback. The oracle is built `HAVE_LROUND=1` → glibc `lround` (round half away
   from zero, per C99 — *not* half-to-even). Match that explicitly (`f64::round`'s
   half-away semantics + cast), verified against the oracle.
5. **Auto qlp precision** (`lpc.c`): `min(15, 32 - subframe_bps - ilog2(order))`;
   subframe_bps is 17 after mid-side. Get `ilog2` and bps right.
6. **Rice partition search** (`stream_encoder.c:~4689`): integer, but tie-breaks
   (first-best wins) must match. `do_escape_coding` is false at level 8.
7. **Mid-side decision** (`stream_encoder.c:~3925`, `loose_mid_side=false`):
   per-frame L/R vs M/S vs L/S vs R/S by estimated bits — replicate the estimate.
   With `loose_mid_side` (levels 1, 4) 1.5.0 instead picks L/R or M/S *before*
   evaluating subframes, by summed absolute first differences
   (`stream_encoder.c:3766-3794`, ties → M/S).
8. **`f32` (`FLAC__real`) vs `f64`**: libFLAC stores LPC coeffs as `float` but
   computes autocorrelation in `double`. Match each variable's type precisely.

## Differential testing

`oracle/` (its own workspace, `publish = false`, excluded from the package) depends
on `libflac-rs` by path and tests through the **public API only**:
`oracle::{encode_frames, encode, encode_with_metadata, encode_ogg}` take the same
`EncoderConfig` / `MetadataBlock`s as `Encoder`, so each test is
`assert_eq!(Encoder::new(cfg).encode(&pcm), oracle::encode(&cfg, &pcm))`.
`tests/differential.rs` covers levels 0–8 × depths 8–32 × mono/stereo (frames,
streams, Ogg), sample rates, block sizes (incl. CHD), 3–8 channels, short and
empty inputs, and every metadata block type. libFLAC always writes a VORBIS_COMMENT
with its own vendor string, so matching block lists start with
`VorbisComment(LIBFLAC_VENDOR_STRING)`.

To localize float drift, the original in-tree rig at the **`c-oracle`** tag also
wraps the individual stages (window, autocorrelation, Levinson, best order,
quantization, residual) in C; restore it into a scratch branch per **`ORACLE.md`**
§4 and compare in stages — frame header → subframe header → qlp_coeff integers →
residual/rice → CRC-16 — diffing at the first mismatching field.

## Milestones

- **F0 — DONE.** Bitwriter (`bitwriter.rs`) + CRC-8/16 (`crc.rs`); CRC byte-exact
  vs `FLAC__crc8/16`.
- **F1 — DONE.** CONSTANT/VERBATIM/FIXED subframes (`subframe.rs`, `fixed.rs`),
  frame header/footer + UTF-8 frame number + block-size hints (`frame.rs`), the
  integer Rice partition search (`rice.rs`), wasted-bits, and block orchestration
  (`encoder.rs`). Byte-exact vs the oracle with `max_lpc_order = 0`, independent
  stereo, across constant/short/sine+noise corpora. (The Rice partition search the
  handoff slotted in F3 lives here, since FIXED needs it; F3 is now just mid-side.)
- **F2 — DONE.** The LPC float pipeline: `subdivide_tukey(3)` windows
  (`window.rs`), windowing + `double` autocorrelation + Levinson + order/precision
  selection + quantization + residual (`lpc/*`), `silog2`/`ilog2` (`bitmath.rs`),
  the LPC subframe writer (`subframe.rs`), and the apodization a/b/c state machine
  + `evaluate_lpc_subframe_` wired into `process_subframe` (`encoder.rs`). Each
  stage is diffed against an exposed C leaf function (`cref/shim.c`), and full LPC
  subframes are byte-exact vs the oracle (`max_lpc_order = 12`, `do_mid_side = 0`)
  across block-multiple, short-final-frame, multi-blocksize (precision 7/9/10/12),
  wasted-bits, and pure-sine-sweep corpora. Confirmed: `f32::cos`==glibc `cosf`,
  `f64::round`==glibc `lround` (half-away), plain `*` autocorrelation (no FMA), and
  the `frexp` exponent + `log2cmax--` shift derivation.
- **F3 — DONE.** Per-frame stereo channel decision (`encoder.rs`): mid/side built
  from the original L/R (`side = L-R`, `mid = (L+R)>>1`), each of L/R/M/S
  independently wasted-bits-shifted and subframe-evaluated (side at +1 bps), then
  the assignment with the fewest summed bits chosen (L/R vs L/S vs R/S vs M/S,
  independent preferred on ties). `process_subframe` was split into a choose pass
  + a deferred writer so the decision picks among already-evaluated subframes.
  Byte-exact vs the oracle at the **real CHD level-8 preset** (`max_lpc_order = -1`,
  `do_mid_side = -1`) across the decorrelated-noise corpus and crafted
  identical/anti/scaled/independent L-R cases. **The first target (CHD/MAME
  config) is now complete end-to-end.**
- **F4 — DONE (public API + docs).** The crate-root surface
  (`Encoder`/`EncoderConfig`, the `decode*` free fns + `Decoded*`/`SeekResult`, and
  the `metadata` types) with crate-level + per-type docs and doctests; the blanket
  `#![allow(dead_code)]` is gone (only two targeted allows on faithful bit-writer
  mirrors). CI vendors libFLAC + libogg for a self-contained differential build;
  `cargo package` produces a slim, leak-free pure-Rust tarball. Releases are
  GitHub releases (`release.yml`, run by pushing a `v*` tag, notes from
  `CHANGELOG.md`); this fork has `publish = false`, as the crates.io name belongs
  to upstream. The
  CD subcode-split + `'L'`/`'B'` endian-trial wrapper stays in chd-rs.

### Generalization (beyond the CHD slice)

- **G1 — DONE (compression levels).** `encode_frames` takes a `Config`
  (`encoder.rs`); `preset(0..=8)` reproduces `compression_levels_[]`. Adds the
  `tukey(p)` apodization (single full window) alongside `subdivide_tukey(parts)`,
  the per-level LPC-order / max-partition-order caps, and `loose_mid_side` (the
  every-~0.4 s redecision used by levels 1 & 4 in 1.4.3; replaced in R1). Byte-exact vs the oracle for all
  levels 0–8 at 16-bit (stereo + mono), incl. the loose redecision boundary. The
  shim gained `libflac_rs_cref_encode_cfg` (a `compression_level` knob).
- **G2 — DONE (metadata + MD5, full streams).** `md5.rs` (RFC 1321 + the
  `format_input_` little-endian sample serialization), `metadata.rs` (STREAMINFO),
  and `encoder::encode` (the `fLaC` marker + STREAMINFO + frames). `audio_md5`
  byte-exact vs `FLAC__MD5`; the STREAMINFO body (min/max framesize, total samples,
  MD5) byte-exact vs libFLAC's finalized block; and the full stream round-trips
  through the real libFLAC **decoder** back to the original PCM. The shim gained a
  seekable in-memory full-stream encoder (`libflac_rs_cref_encode_full`) and a
  decode round-trip (`libflac_rs_cref_decode`). `encode` also writes the optional
  VORBIS_COMMENT (vendor) + PADDING blocks: with `metadata::LIBFLAC_VENDOR_STRING`
  and no padding the **entire** stream is byte-identical to libFLAC's default
  output (`libflac_rs_cref_vendor_string` confirms the version string). ROADMAP
  Phase 8 finished the remaining metadata blocks — APPLICATION, PICTURE, SEEKTABLE,
  and CUESHEET — all byte-exact vs libFLAC; SEEKTABLE is *generated* during
  encoding (`fill_seekpoints` + `metadata::seektable_sort`).
  VORBIS_COMMENT user comments (`MetadataBlock::VorbisCommentEntries`,
  `stream_encoder_framing.c:145-152`) and `vorbis_comment_entry` (libFLAC's
  `entry_from_name_value_pair` with `format.c`'s name/value checks) were added in the
  trackIT-Systems fork; tagged streams at 192/384/500 kHz, levels 0/5/8, are
  byte-identical to libFLAC 1.5.0 apart from the vendor string. The decoder returns
  the block as `DecodedStream::vorbis_comment` (native and Ogg).
- **G3 — DONE (all bit depths 8/12/16/20/24/32; later every depth 4–32, whose
  frame headers defer to STREAMINFO, byte-exact without further changes).** RICE2 (5-bit params, escape 31)
  is selected per partition when any rice parameter reaches 15
  (`RicePartition::is_rice2`), driven by the `bps>16 ? 31 : 15` rice-parameter
  limit. **32-bit** carries the channel signal as `i64` throughout (so the 33-bit
  `side = L-R` and `mid = (L+R)>>1` don't overflow); the fixed/LPC residuals are
  computed wide and either wrap to `i32` (fixed) or bail (LPC `_limit_residual`).
  Five libFLAC quirks were matched: constant detection at **`subframe_bps >= 28`**
  goes through the `_limit_residual` predictor's `rbps[1]`, which stays `34.0` for
  an **all-zero** signal (so that one is FIXED, not CONSTANT — in 1.4.3 every
  constant 32-bit signal was); `get_wasted_bits_wide_` returns **shift 1**
  for an all-zero side; the **fixed subframe is skipped when `rbps >= subframe_bps`**
  (`stream_encoder.c:4154`); and rice selection **short-circuits on `mean < 2`**.
  Frames + full streams byte-exact for all depths.
- **D1 — decoder core DONE.** `bitreader.rs` (MSB-first reads, the decode mirror of
  `bitwriter`) + `decoder.rs`: `decode_frames` (raw frames) and `decode` (full
  stream — `fLaC` marker, STREAMINFO, skip other metadata, frames, MD5 verify).
  Frame header (+CRC-8), all four subframe types, RICE/RICE2 residual (incl.
  escape), `i64` fixed/LPC restore, L/S·R/S·M/S un-decorrelation, CRC-16. The
  decoder is *not* byte-parity work — it's verified by **lossless round-trip**
  (`decode(encode(pcm)) == pcm`, all depths/levels, MD5 ok) and by **decoding real
  libFLAC output** (`decode_libflac_streams`). Phase 9 then added **SEEKTABLE-driven
  `seek()`** (`decode_seek`) and **variable-block-size** decoding (`read_utf8_u64`,
  selected by the blocking-strategy bit). **Streaming:** `stream.rs`'s
  `StreamDecoder` (any `Read`; native, raw frames, chained Ogg) is the decode core;
  `decode`, `decode_frames`, `decode_ogg[_chained]` and `decode_with` (sample
  limit) collect from it. Decoding stops at STREAMINFO's total when set.
  **D2 (libFLAC 1.5.0 decoder, issue #2):** frames are placed by their header
  numbers (`Timeline::place`, `read_frame_header_`'s fixed-block-size logic) and
  missing frames are filled with silence under libFLAC's rules (`Timeline::fill_gap`,
  `stream_decoder.c:2483`, reported as `Gap`s); out-of-range samples reject the
  frame (`OUT_OF_BOUNDS`, `:2455`); `decode_ogg` takes the first FLAC logical
  stream and `decode_ogg_chained` every link (`ogg::read_flac_streams`). Checked
  against libFLAC's decoder in `oracle/tests/decoder.rs`. Corrupt frames still fail
  the decode (libFLAC resynchronizes). Sample reconstruction mirrors libFLAC's
  integer widths (`Channel::Narrow` = its `int32` paths, wrapping; `Wide` = the
  33-bit side channel) so damaged-but-CRC-valid frames match it; truncated input
  ends the stream (`truncated`), as in libFLAC. The `fuzz/` crate's `diff_decode`
  target finds remaining differences.
- **O1 — Ogg FLAC DONE (byte-exact).** `ogg.rs` ports libogg 1.3.5 paging
  (`framing.c`) + the FLAC-in-Ogg mapping (`ogg_encoder_aspect.c`).
  `encoder::encode_ogg` is **byte-identical to libFLAC+libogg** and
  `decoder::decode_ogg` round-trips. Crux: libogg's CRC-32 (`0x04c11db7`,
  unreflected), lacing, and the `flush_i` page heuristic (BOS = 1 packet; audio
  pages at `>4096` bytes & `≥4` packets, else 255 segs, forced at EOS); the BOS
  header-packet count is `0` (unknown) and the SEEKTABLE is dropped for Ogg. The
  oracle is built `FLAC__HAS_OGG=1` with the `ogg_*.c` layer + vendored libogg
  (`cref/vendor/ogg`); still zero **runtime** deps.
- **R1 — DONE (retarget to libFLAC 1.5.0, `0.150.0`).** The output-relevant
  1.4.3 → 1.5.0 changes, found by diffing every compiled `.c` file: the loose
  mid-side heuristic (`encoder::loose_prefers_independent`); the
  `CHECK_ORDER_IS_VALID` fix in `fixed.c:356` (each order's estimate from its own
  error sum, which makes constant non-zero ≥28-bit subframes CONSTANT); seektable
  targets past the end become placeholders (`stream_encoder.c:3239`); the vendor
  string. The new `FLAC__lpc_max_residual_bps` bound (`lpc.c:962`) is ported too
  but cannot change output (where it avoids the `_limit_residual` path, the residual
  provably fits). Multithreading, bitwriter growth and the Ogg write-callback sample
  counts don't affect bytes. The CHD configuration is unaffected: its output is
  identical under 1.4.3 and 1.5.0 (checked against both oracles).

## Conventions

- Faithful transcription over "improved" Rust when bit-exactness is at stake;
  idiomatic refactors are fine only for plumbing that cannot affect output bytes.
- Keep `*.c:NNN` citations in module docs accurate when you touch the code.
- Input validation lives in `validate.rs`: settings (`init_stream_internal_`),
  samples (`process_interleaved`'s range check) and metadata (`init`'s checks plus
  field widths) are rejected where libFLAC rejects them, nothing stricter except
  Vorbis comment entries (the format's rule); `oracle/tests/differential.rs`
  `validation_matches_libflac` pins this down.
- Transcendentals (`f32::cos`, `f64::ln`) go through the platform libm, like
  libFLAC's: output matches libFLAC built on the same platform and may differ
  between glibc, musl and macOS (decided 2026-11-06; measurements in `ORACLE.md`).
  Don't replace them with a bundled implementation without revisiting that.
- Library is `#![forbid(unsafe_code)]`, edition 2024, MSRV 1.85, **zero
  dependencies** (pure `std`, not even a build script) — keep it that way. C and
  `cc` live only in the separate `oracle/` workspace; see `ORACLE.md`.
- Version scheme `0.<flac-digits>.<patch>` (1.5.0 → `0.150.x`); git tag `v0.150.1`.
  Every release gets a `CHANGELOG.md` section, which `release.yml` uses as notes.
