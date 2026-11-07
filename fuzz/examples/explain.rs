//! Print the details of `diff_decode` artifacts that still disagree: Rust's blocks
//! and libFLAC's output and errors.
//! `cargo run --release --example explain -- artifacts/diff_decode/*`.

use libflac_rs::StreamDecoder;
use libflac_rs_fuzz::stream_decode;

fn main() {
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path).unwrap();
        let Some(rust) = stream_decode(&data) else {
            continue;
        };
        let c = libflac_rs_oracle::decode(&data, data.starts_with(b"OggS"), false);
        let c_pcm: &[i32] = c.links.first().map_or(&[], |l| &l.interleaved);
        let bad = match &rust {
            Ok(p) => !c_pcm.starts_with(p),
            Err(_) => c.errors.is_empty() && c.links.len() == 1 && !c_pcm.is_empty(),
        };
        if !bad {
            continue;
        }
        println!("=== {path} ({} bytes)", data.len());
        match StreamDecoder::new(&data[..]) {
            Ok(mut d) => {
                let h = d.header().clone();
                println!(
                    "  header {}ch {}b {}Hz total {} bs {}..{}",
                    h.channels,
                    h.bits_per_sample,
                    h.sample_rate,
                    h.total_samples,
                    h.min_blocksize,
                    h.max_blocksize
                );
                for b in d.by_ref() {
                    match b {
                        Ok(b) => println!(
                            "  rust block @{} n={} ch={} bps={}{}",
                            b.first_sample,
                            b.samples(),
                            b.channels,
                            b.bits_per_sample,
                            if b.silence { " silence" } else { "" }
                        ),
                        Err(e) => println!("  rust error {e}"),
                    }
                }
                println!("  rust truncated={}", d.truncated());
            }
            Err(e) => println!("  rust new() error {e}"),
        }
        println!("  libFLAC {} values, errors {:?}", c_pcm.len(), c.errors);
    }
}
