//! Any valid settings and samples encode and decode back to the same samples, as
//! a stream, raw frames, Ogg, and through the streaming decoder.
#![no_main]

use libflac_rs::{Encoder, decode, decode_frames, decode_ogg};
use libflac_rs_fuzz::{EncodeInput, stream_decode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: EncodeInput| {
    let config = input.config();
    let pcm = input.pcm();
    let encoder = Encoder::try_new(config).expect("generated settings are valid");
    let encoded = match input.mode % 3 {
        0 => encoder.try_encode(&pcm),
        1 => encoder.try_encode_frames(&pcm),
        _ => encoder.try_encode_ogg(&pcm, 7),
    };
    let Ok(encoded) = encoded else {
        // Only out-of-range samples may be rejected.
        let (min, max) = (
            i32::MIN >> (32 - config.bits_per_sample),
            i32::MAX >> (32 - config.bits_per_sample),
        );
        assert!(
            pcm.iter().any(|&s| s < min || s > max),
            "valid input rejected"
        );
        return;
    };
    // Raw frames carry no STREAMINFO, so only the depths with a frame-header code
    // (8, 12, 16, 20, 24, 32) can be decoded from them alone; libFLAC agrees.
    if input.mode % 3 == 1 && ![8, 12, 16, 20, 24, 32].contains(&config.bits_per_sample) {
        return;
    }
    let decoded = match input.mode % 3 {
        0 => decode(&encoded).map(|d| (d.interleaved, d.md5_ok)),
        1 => decode_frames(&encoded).map(|d| (d.interleaved, true)),
        _ => decode_ogg(&encoded).map(|d| (d.interleaved, d.md5_ok)),
    };
    let (samples, md5_ok) = decoded.expect("own output decodes");
    assert!(samples == pcm, "round trip changed the samples");
    assert!(md5_ok, "MD5 mismatch");
    if !pcm.is_empty() {
        let streamed = stream_decode(&encoded).expect("small").expect("streams");
        assert!(streamed == pcm, "streaming decode changed the samples");
    }
});
