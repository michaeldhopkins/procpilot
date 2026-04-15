//! Test helper: print the value of a named environment variable, or empty if unset.
//!
//! Usage: `pp_print_env <VAR>`
//!
//! Not part of procpilot's public API. Used by internal tests.

fn main() {
    let var = std::env::args().nth(1).unwrap_or_default();
    let value = std::env::var(&var).unwrap_or_default();
    println!("{value}");
}
