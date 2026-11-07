//! Arbitrary bytes through every decode entry point: none may panic or hang.
#![no_main]

use libflac_rs::{
    DecodeOptions, StreamDecoder, decode, decode_frames, decode_ogg_chained, decode_seek,
    decode_with,
};
use libflac_rs_fuzz::MAX_OUTPUT;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Streaming, across chained Ogg links, counting the output.
    let mut produced = 0u64;
    if let Ok(mut decoder) = StreamDecoder::new(data) {
        'links: loop {
            for block in decoder.by_ref() {
                match block {
                    Ok(block) => produced += block.interleaved.len() as u64,
                    Err(_) => break 'links,
                }
                if produced > MAX_OUTPUT {
                    return;
                }
            }
            if !matches!(decoder.next_link(), Ok(true)) {
                break;
            }
        }
    }
    let _ = decode_with(
        data,
        &DecodeOptions {
            max_samples: Some(MAX_OUTPUT / 8),
        },
    );
    // The unlimited in-memory functions, only for inputs that stay small.
    if produced <= MAX_OUTPUT {
        let _ = decode(data);
        let _ = decode_frames(data);
        let _ = decode_ogg_chained(data);
        let target = data
            .iter()
            .take(3)
            .fold(0u64, |t, &b| t << 8 | u64::from(b));
        let _ = decode_seek(data, target);
    }
});
