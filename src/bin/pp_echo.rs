//! Test helper: print args separated by spaces, followed by a newline.
//!
//! Not part of procpilot's public API. Used by internal tests.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("{}", args.join(" "));
}
