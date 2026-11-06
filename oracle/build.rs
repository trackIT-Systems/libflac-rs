//! Compiles the C reference — libFLAC 1.4.3 (scalar path) and libogg 1.3.5 — plus
//! the FFI shim in `shim.c` into one static library the oracle crate links.
//!
//! The C sources are not vendored. `fetch-sources.sh` clones the Xiph release tags
//! into `vendor/`; `LIBFLAC_SRC` / `LIBOGG_SRC` point elsewhere. The two headers the
//! upstream builds generate (libFLAC's `config.h`, libogg's `config_types.h`) are
//! written into `OUT_DIR` here.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// libFLAC's `config.h` as MAME ships it (`3rdparty/flac`): CPU detection and the
/// package version, which feeds `FLAC__VENDOR_STRING` (`format.c:58`).
const FLAC_CONFIG_H: &str = r#"#if defined(__i386__) || defined(_M_IX86)
#define FLAC__CPU_IA32
#endif
#if defined(__x86_64__) || defined(_M_X64)
#define FLAC__CPU_X86_64
#endif
#if defined(__i386__) || defined(__x86_64__) || defined(_M_IX86) || defined(_M_X64)
#define FLAC__ALIGN_MALLOC_DATA
#define FLAC__HAS_X86INTRIN 1
#else
#define FLAC__HAS_X86INTRIN 0
#endif
#if defined(__aarch64__) || defined(_M_ARM64)
#define FLAC__CPU_ARM64
#define FLAC__HAS_NEONINTRIN 1
#define FLAC__HAS_A64NEONINTRIN 1
#else
#define FLAC__HAS_NEONINTRIN 0
#define FLAC__HAS_A64NEONINTRIN 0
#endif
#define PACKAGE_VERSION "1.4.3"
"#;

/// libogg's `config_types.h`, which its configure step generates from
/// `config_types.h.in`.
const OGG_CONFIG_TYPES_H: &str = r#"#ifndef __CONFIG_TYPES_H__
#define __CONFIG_TYPES_H__
#include <stdint.h>
typedef int16_t  ogg_int16_t;
typedef uint16_t ogg_uint16_t;
typedef int32_t  ogg_int32_t;
typedef uint32_t ogg_uint32_t;
typedef int64_t  ogg_int64_t;
typedef uint64_t ogg_uint64_t;
#endif
"#;

/// The libFLAC translation units: the encoder, the decoder its verify path
/// references, and the Ogg layer. No `*_intrin_*` / `*_asm_*` or metadata-object
/// units.
const FLAC_SOURCES: &[&str] = &[
    "stream_encoder.c",
    "stream_encoder_framing.c",
    "lpc.c",
    "fixed.c",
    "window.c",
    "bitwriter.c",
    "bitmath.c",
    "crc.c",
    "format.c",
    "memory.c",
    "float.c",
    "md5.c",
    "cpu.c",
    "stream_decoder.c",
    "bitreader.c",
    "ogg_encoder_aspect.c",
    "ogg_decoder_aspect.c",
    "ogg_helper.c",
    "ogg_mapping.c",
];

fn source_dir(var: &str, default: &str, probe: &str) -> PathBuf {
    println!("cargo:rerun-if-env-changed={var}");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dir = env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("vendor").join(default));
    if !dir.join(probe).exists() {
        panic!(
            "C reference sources not found at {} (missing {probe}).\n\
             Run oracle/fetch-sources.sh, or set {var} to a checkout.",
            dir.display()
        );
    }
    dir
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Settings both libraries share: no FMA contraction (the Rust port never fuses a
/// multiply-add, and clang/gcc would on arm64), optimized, warnings off.
fn base_build() -> cc::Build {
    let mut build = cc::Build::new();
    build.flag_if_supported("-ffp-contract=off").warnings(false);
    build
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim.c");

    let flac = source_dir("LIBFLAC_SRC", "flac", "src/libFLAC/stream_encoder.c");
    let ogg = source_dir("LIBOGG_SRC", "ogg", "src/framing.c");
    let libflac = flac.join("src").join("libFLAC");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let generated = out.join("include");
    write(&generated.join("config.h"), FLAC_CONFIG_H);
    write(
        &generated.join("ogg").join("config_types.h"),
        OGG_CONFIG_TYPES_H,
    );

    // libFLAC with MAME's define set (scripts/src/3rdparty.lua), forced onto the
    // scalar path with FLAC__NO_ASM, plus the Ogg layer (runtime-gated on is_ogg,
    // so native output is unchanged).
    let mut build = base_build();
    build
        .include(&generated)
        .include(flac.join("include"))
        .include(libflac.join("include"))
        .include(ogg.join("include"))
        .define("HAVE_CONFIG_H", None)
        .define("FLAC__NO_ASM", None)
        .define("FLAC__HAS_OGG", "1")
        .define("OGG_FOUND", "1")
        .define("NDEBUG", None)
        .define("HAVE_LROUND", "1")
        .define("ENABLE_64_BIT_WORDS", "1")
        .define("HAVE_INTTYPES_H", None)
        .define("HAVE_STDBOOL_H", None)
        .define("HAVE_STDINT_H", None)
        .define("HAVE_STDIO_H", None)
        .define("HAVE_STDLIB_H", None)
        .define("HAVE_STRING_H", None);
    for f in FLAC_SOURCES {
        build.file(libflac.join(f));
    }
    build.file("shim.c").compile("flac_oracle");

    // libogg: page framing and bit I/O. Compiled after libFLAC so GNU ld sees the
    // archive that needs it first. Built without HAVE_CONFIG_H so it never
    // sees libFLAC's config.h.
    let mut ogg_build = base_build();
    ogg_build
        .include(&generated)
        .include(ogg.join("include"))
        .file(ogg.join("src").join("framing.c"))
        .file(ogg.join("src").join("bitwise.c"))
        .compile("ogg_oracle");
}
