//! The Rust decoder against libFLAC's on arbitrary bytes. Whatever Rust decodes
//! must match the start of libFLAC's output (libFLAC may decode further: past
//! STREAMINFO's sample count, or past damage, where Rust stops). Whatever libFLAC
//! decodes without reporting any error, Rust must decode too.
#![no_main]

use libflac_rs_fuzz::stream_decode;
use libflac_rs_oracle as oracle;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some(rust) = stream_decode(data) else {
        return; // too large to compare
    };
    let ogg = data.starts_with(b"OggS");
    let c = oracle::decode(data, ogg, false);
    let c_pcm: &[i32] = c.links.first().map_or(&[], |l| &l.interleaved);
    match &rust {
        Ok(pcm) => assert!(
            c_pcm.starts_with(pcm),
            "Rust decoded {} values that differ from libFLAC's {} (errors {:?})",
            pcm.len(),
            c_pcm.len(),
            c.errors
        ),
        Err(e) => assert!(
            !(c.errors.is_empty() && c.links.len() == 1 && !c_pcm.is_empty()),
            "Rust rejected ({e}) what libFLAC decodes without errors ({} values)",
            c_pcm.len()
        ),
    }
});
