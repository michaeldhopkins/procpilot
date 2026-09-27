//! What an error keeps of a child's output is exactly its tail: never reordered, never cut from
//! the back, never longer than `STREAM_SUFFIX_SIZE`.
//!
//! A child's stdout and stderr are arbitrary bytes: invalid UTF-8, a multi-byte character split
//! anywhere. stdout is kept as bytes; stderr is decoded lossily and then cut at a character
//! boundary, and the cut is where a bug would live.
//!
//! The cap is 128 KiB and libFuzzer's inputs are a few KiB, so the fuzzed bytes are followed by
//! enough ASCII padding to put the cut inside them (fix the frame, fuzz the variable): the output
//! ends with the last `keep` fuzzed bytes and then `STREAM_SUFFIX_SIZE - keep` bytes of padding.
//! The first two bytes choose `keep`. The fuzzed bytes are also run alone, where nothing may be
//! dropped.
#![no_main]

use libfuzzer_sys::fuzz_target;
use procpilot::STREAM_SUFFIX_SIZE;
use procpilot::fuzz_api::{stderr_suffix, stdout_suffix};

fn check(bytes: &[u8]) {
    let kept = stdout_suffix(bytes.to_vec());
    let want = &bytes[bytes.len().saturating_sub(STREAM_SUFFIX_SIZE)..];
    assert_eq!(kept.as_slice(), want, "stdout must keep exactly the last STREAM_SUFFIX_SIZE bytes");

    let decoded = String::from_utf8_lossy(bytes);
    let kept = stderr_suffix(bytes);
    assert!(decoded.ends_with(kept.as_str()), "stderr must be a tail of the decoded output");
    if decoded.len() <= STREAM_SUFFIX_SIZE {
        assert_eq!(kept, decoded, "stderr within the cap must be kept whole");
    } else {
        assert!(kept.len() <= STREAM_SUFFIX_SIZE, "stderr kept {} bytes, over the cap", kept.len());
        // The cut moves forward to the next character boundary, and a UTF-8 character is at most
        // four bytes, so at most three bytes under the cap are lost to it.
        assert!(kept.len() + 3 >= STREAM_SUFFIX_SIZE, "stderr kept {} bytes, far under the cap", kept.len());
    }
}

fuzz_target!(|data: &[u8]| {
    check(data);
    if data.len() < 2 {
        return;
    }
    let (pivot, fuzzed) = data.split_at(2);
    let keep = usize::from(u16::from_le_bytes([pivot[0], pivot[1]])) % (fuzzed.len() + 1);
    let mut framed = fuzzed.to_vec();
    framed.resize(fuzzed.len() + STREAM_SUFFIX_SIZE - keep, b'x');
    check(&framed);
});
