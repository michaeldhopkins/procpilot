//! Test helper: print the current working directory.
//!
//! Not part of procpilot's public API. Used by internal tests.

fn main() {
    let cwd = std::env::current_dir().expect("cwd");
    println!("{}", cwd.display());
}
