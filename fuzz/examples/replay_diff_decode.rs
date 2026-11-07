//! Re-check `diff_decode` artifacts against the current decoder:
//! `cargo run --release --example replay_diff_decode -- artifacts/diff_decode/*`.

use libflac_rs_fuzz::stream_decode;
use libflac_rs_oracle as oracle;
use std::collections::BTreeMap;

fn main() {
    let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path).unwrap();
        let outcome = match stream_decode(&data) {
            None => "too large".to_string(),
            Some(rust) => {
                let c = oracle::decode(&data, data.starts_with(b"OggS"), false);
                let c_pcm: &[i32] = c.links.first().map_or(&[], |l| &l.interleaved);
                match rust {
                    Ok(pcm) if c_pcm.starts_with(&pcm) => "agrees".into(),
                    Ok(pcm) => format!(
                        "DIFFERS: Rust {} values, libFLAC {} errors {:?} (header {:02x?})",
                        pcm.len(),
                        c_pcm.len(),
                        c.errors,
                        &data[..data.len().min(8)]
                    ),
                    Err(_) if c.errors.is_empty() && c.links.len() == 1 && !c_pcm.is_empty() => {
                        format!(
                            "STILL: Rust rejects what libFLAC decodes cleanly ({} values, header {:02x?})",
                            c_pcm.len(),
                            &data[..data.len().min(8)]
                        )
                    }
                    Err(_) => "agrees (both fail)".into(),
                }
            }
        };
        if outcome.starts_with("DIFFERS") || outcome.starts_with("STILL") {
            println!("{path}: {outcome}");
        }
        *outcomes
            .entry(outcome.split(':').next().unwrap().to_string())
            .or_default() += 1;
    }
    println!("{outcomes:?}");
}
