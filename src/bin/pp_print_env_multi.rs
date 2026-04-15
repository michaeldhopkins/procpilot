//! Test helper: print the values of multiple named environment variables,
//! space-separated, on one line. Missing variables print as empty strings.
//!
//! Usage: `pp_print_env_multi <VAR1> [VAR2 ...]`
//!
//! Not part of procpilot's public API. Used by internal tests.

fn main() {
    let values: Vec<String> = std::env::args()
        .skip(1)
        .map(|var| std::env::var(&var).unwrap_or_default())
        .collect();
    println!("{}", values.join(" "));
}
