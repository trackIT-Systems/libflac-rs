# Differential testing against the C oracle

This crate is a **bit-exact** port of libFLAC 1.5.0: its output is byte-identical to
the C reference. That is checked by a **differential test crate**, `oracle/`, which
compiles the real libFLAC (and libogg, for Ogg FLAC) from source and compares the
Rust output against the C output byte-for-byte.

The library itself stays **100% pure Rust**: no C, no build script, no
dependencies. `oracle/` is a separate Cargo workspace (`publish = false`, excluded
from the package) that depends on `libflac-rs` by path. Only it uses `cc`.

---

## 1. What the oracle is

| Component | Upstream | Version / tag | Role |
| --- | --- | --- | --- |
| libFLAC | [xiph/flac](https://github.com/xiph/flac) | `1.5.0` | the encoder reference (scalar path only) |
| libogg | [xiph/ogg](https://github.com/xiph/ogg) | `v1.3.5` | Ogg page framing (libFLAC delegates all Ogg paging to it) |

Both are BSD-3-Clause (Xiph.Org). The sources are **not vendored**:
`oracle/fetch-sources.sh` shallow-clones the release tags into `oracle/vendor/`
(gitignored).

## 2. Running it

```sh
oracle/fetch-sources.sh      # once; clones libFLAC 1.5.0 + libogg 1.3.5
cd oracle && cargo test
```

`LIBFLAC_SRC` / `LIBOGG_SRC` point the build at other checkouts. CI runs the same
steps in the `oracle` job (`.github/workflows/ci.yml`), on `ubuntu-latest` only.

**Run it after any change that can move an output byte** (the LPC float pipeline,
framing, rice coding, metadata, Ogg paging, …). A failure lists every mismatching
case with the offset of its first differing byte.

### The output depends on the platform libm — on purpose

Two libm functions feed encoder decisions: `cosf` (the apodization windows) and
`log` (the fixed-predictor and LPC-order bit estimates). Their last-bit rounding
differs between C libraries, so **libFLAC itself produces slightly different bytes
on glibc, musl and macOS**. The Rust port calls the same functions through `std`
(`f32::cos`, `f64::ln`), which resolve to the platform libm, so it reproduces
libFLAC **built on the same platform**. The oracle compares like with like and
passes on each of them. `f64::round() as i32` matches C99 `lround` (half away from
zero, `HAVE_LROUND=1`) everywhere.

Measured on 2026-11-06 (glibc 2.36, musl 1.2.5, macOS 27):

| vs. glibc amd64 | glibc arm64 | musl amd64/arm64 | macOS arm64 |
| --- | --- | --- | --- |
| `cosf`, every `f32` in [0, 7) | identical | ~1.5% of 4096-value chunks differ | ~2.5% differ |
| `log`, 2·10⁸ samples | identical | 96 of 3051 chunks differ | every chunk differs |
| encoder output, 2916 streams | — | 14 differ | 8 differ |

CI runs the oracle on glibc (`oracle`) and musl (`oracle-musl`). glibc stays the
reference for the documented float hazards (see [`CLAUDE.md`](CLAUDE.md)). The C
side is built `FLAC__NO_ASM` (deterministic, matches x86 chdman — see "SIMD vs
scalar") and with `-ffp-contract=off`, so clang/gcc never fuse a multiply-add the
Rust port keeps separate (they would on arm64).

## 3. Layout of `oracle/`

| File | Role |
| --- | --- |
| `fetch-sources.sh` | clones the Xiph tags into `vendor/` |
| `build.rs` | compiles libFLAC (`stream_encoder*.c`, `lpc.c`, `fixed.c`, `window.c`, `bitwriter.c`, `bitmath.c`, `crc.c`, `format.c`, `memory.c`, `float.c`, `md5.c`, `cpu.c`, `stream_decoder.c`, `bitreader.c`, the `ogg_*.c` layer), libogg (`framing.c`, `bitwise.c`) and `shim.c`, with MAME's define set (`scripts/src/3rdparty.lua`) plus `FLAC__HAS_OGG=1` and `HAVE_FSEEKO`. It writes the two headers the upstream builds generate into `OUT_DIR`: libFLAC's `config.h` (MAME's CPU-detection header, `PACKAGE_VERSION "1.5.0"`) and libogg's `ogg/config_types.h`. |
| `shim.c` | one entry point, `libflac_rs_oracle_encode`: raw frames (metadata dropped, as CHD does), a complete stream (seekable sink, so STREAMINFO is rewritten at finish), or Ogg FLAC; plus an optional list of metadata blocks of every type |
| `src/lib.rs` | safe wrappers mirroring `Encoder`: `encode_frames`, `encode`, `encode_with_metadata`, `encode_ogg`, taking the same `EncoderConfig` and `MetadataBlock`s |
| `tests/differential.rs` | the matrix: levels 0–8 × depths 8/12/16/20/24/32 × mono/stereo (frames, streams, Ogg); sample rates 8 kHz–655.35 kHz; block sizes 16–65535 incl. the CHD config; 3–8 channels; inputs of 0–4095 samples; every metadata block type, alone and combined |

A test is a plain comparison:

```rust
let cfg = EncoderConfig::new(2, 24, 96_000).with_compression_level(5);
assert_eq!(Encoder::new(cfg).encode(&pcm), oracle::encode(&cfg, &pcm));
```

Two libFLAC behaviors to keep in mind when writing one: it **always** writes a
VORBIS_COMMENT (an empty one right after STREAMINFO if the block list has none),
and it **always** writes its own vendor string. A block list that matches therefore
starts with `MetadataBlock::VorbisComment(LIBFLAC_VENDOR_STRING)` (or
`VorbisCommentEntries` with that vendor).

## 4. Stage-level debugging (the `c-oracle` tag)

`oracle/` compares whole outputs through the public API. To localize a float
divergence, the original rig at the tag **`c-oracle`** additionally wrapped the
individual pipeline stages in C — `window_tukey`, `lpc_window_data[_partial]`,
`compute_autocorrelation`, `compute_lp_coefficients`, `compute_best_order`,
`quantize_coefficients`, `compute_residual`, CRC-8/16, MD5 — and diffed them against
crate internals exposed under a `cref` feature. Restore it into a scratch branch
when needed:

```sh
git checkout c-oracle -- cref build.rs tests/cref.rs
git show c-oracle:src/lib.rs     # copy the `#[cfg(feature = "cref")] pub mod testing { … }`
git show c-oracle:src/lpc/mod.rs # copy the `#[cfg(feature = "cref")] pub use quantize::Quantized;`
```

and re-add the feature to `Cargo.toml` (`cref = ["dep:cc"]`, `cc = { version =
"1.2", optional = true }` under `[build-dependencies]`). Its `build.rs` links with
GNU `--whole-archive`, so run it on Linux: `cargo test --features cref`. The
internals it references may have moved since the tag; adapt the `testing` module.

Compare stage by stage — **frame header → subframe header → `qlp_coeff` integers →
residual/rice → CRC-16** — and diff at the first mismatching field. The hazards the
original bring-up pinned down (scalar `double` autocorrelation with no FMA, the
`lround` half-away rounding, the `frexp`/`ilog2` derivations, the per-frame mid-side
decision, the 32-bit `i64` paths, the libogg `pageout` flush heuristic + CRC-32, …)
are documented in [`CLAUDE.md`](CLAUDE.md) ("Bit-exactness hazards" and the
per-phase notes).
