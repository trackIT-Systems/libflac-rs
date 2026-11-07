# Fuzzing

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets for libflac-rs. A
workspace of its own, like `oracle/`; the library does not depend on it.

| Target | Input | Fails when |
| --- | --- | --- |
| `decode` | arbitrary bytes | any decode function panics or hangs |
| `roundtrip` | generated settings and signal | `decode(encode(x)) != x` |
| `diff_encode` | generated settings and signal | the bytes differ from libFLAC's, or only one side rejects the input |
| `diff_decode` | arbitrary bytes | Rust decodes samples libFLAC does not, or rejects what libFLAC decodes without any error |

The signal generator builds constant, ramp, sine, noise, square and raw segments,
so LPC, fixed and constant subframes all occur. The `diff_*` targets link the C
oracle, so fetch its sources first (`../oracle/fetch-sources.sh`).

```sh
cargo install cargo-fuzz        # needs a C compiler; nightly only for sanitizers
cargo run --example seed -- corpus/decode corpus/diff_decode   # seed corpus
cargo fuzz run -s none decode -- -max_total_time=600
cargo fuzz run -s none diff_decode -- -fork=4 -ignore_crashes=1 -max_total_time=1800
cargo run --release --example replay_diff_decode -- artifacts/diff_decode/*
```

`-s none` skips the address sanitizer: the library has no `unsafe`, and it is
several times faster. Decoded output above about 4 M samples per input is not
collected (`MAX_OUTPUT`), since tiny inputs can legitimately expand to gigabytes.

`corpus/` and `artifacts/` are not committed. `replay_diff_decode` re-checks saved
`diff_decode` findings against the current decoder and groups them; `explain`
prints the blocks and libFLAC errors of those that still disagree.

## Differences from libFLAC's decoder

Every `diff_decode` finding so far (864 from three runs, then none in 24 M executions) decodes to libFLAC's
samples or is rejected by both. The remaining differences are deliberate or not
reachable by this harness:

- After a corrupt frame libFLAC searches for the next frame sync and continues;
  this decoder stops with `DecodeError::Malformed`. (`diff_decode` only checks
  that Rust's output is a prefix of libFLAC's.)
- Decoding stops at STREAMINFO's sample count; libFLAC decodes past it.
- libFLAC skips the Rice quotient limit when a symbol straddles the end of its
  read buffer (`incomplete_msbs`), which depends on how the input is fed.
