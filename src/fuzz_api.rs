//! Entry points for the fuzz targets in `fuzz/`, compiled only under `cargo fuzz` (which passes
//! `--cfg fuzzing`). What they wrap is crate-private; this module reaches it without making it
//! part of the published API.

/// What an error variant keeps of a child's captured stdout: the runner's `truncate_suffix`.
pub fn stdout_suffix(bytes: Vec<u8>) -> Vec<u8> {
    crate::error::truncate_suffix(bytes)
}

/// What an error variant keeps of a child's captured stderr, decoded as the runner decodes it
/// (`String::from_utf8_lossy`, then `truncate_suffix_string`).
pub fn stderr_suffix(bytes: &[u8]) -> String {
    crate::error::truncate_suffix_string(String::from_utf8_lossy(bytes).into_owned())
}
