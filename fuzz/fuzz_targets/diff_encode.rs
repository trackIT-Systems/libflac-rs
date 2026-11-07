//! The Rust encoder against libFLAC: identical bytes, or both reject the input.
#![no_main]

use libflac_rs::Encoder;
use libflac_rs_fuzz::EncodeInput;
use libflac_rs_oracle as oracle;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: EncodeInput| {
    let config = input.config();
    let pcm = input.pcm();
    let encoder = Encoder::try_new(config).expect("generated settings are valid");
    let (rust, c) = match input.mode % 3 {
        0 => (
            encoder.try_encode(&pcm).ok(),
            oracle::try_encode_with_metadata(
                &config,
                &pcm,
                &[libflac_rs::MetadataBlock::VorbisComment(
                    libflac_rs::LIBFLAC_VENDOR_STRING,
                )],
            )
            .ok(),
        ),
        1 => (
            encoder.try_encode_frames(&pcm).ok(),
            oracle::try_encode_frames(&config, &pcm).ok(),
        ),
        _ => (
            encoder.try_encode_ogg(&pcm, 7).ok(),
            oracle::try_encode_ogg(&config, &pcm, 7).ok(),
        ),
    };
    match (rust, c) {
        (None, None) => {}
        (Some(r), Some(c)) => {
            if r != c {
                let at = r.iter().zip(&c).position(|(a, b)| a != b);
                panic!(
                    "bytes differ: {config:?}, mode {}, first difference at {at:?}, lengths {} vs {}",
                    input.mode % 3,
                    r.len(),
                    c.len()
                );
            }
        }
        (r, c) => panic!(
            "only one side rejected: Rust ok={}, libFLAC ok={}, {config:?}",
            r.is_some(),
            c.is_some()
        ),
    }
});
