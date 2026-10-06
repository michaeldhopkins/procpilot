//! Test helper: run `pp_cat` twice through one `Cmd` whose stdin is a one-shot reader, and
//! print what the second run, which finds the reader already taken, read on its stdin.
//!
//! Usage: `pp_rerun_reader <pp_cat path> <run|pipeline|async>`
//!
//! It runs as its own process so the test controls the stdin a taken reader could fall back
//! to: the test hands it a pipe that never closes, so a second run that inherits it hangs.
//!
//! Not part of procpilot's public API. Used by internal tests.

use std::io::{Cursor, Write};

use procpilot::{Cmd, StdinData};

#[path = "support/orphan.rs"]
mod orphan;

fn main() {
    orphan::exit_when_orphaned();
    let mut args = std::env::args().skip(1);
    let cat = args.next().expect("pp_cat path");
    let mode = args.next().expect("mode");
    let reader = || StdinData::from_reader(Cursor::new(b"taken once".to_vec()));
    let second = match mode.as_str() {
        "run" => {
            let cmd = Cmd::new(&cat).stdin(reader());
            cmd.clone().run().expect("first run");
            cmd.run().expect("second run")
        }
        "pipeline" => {
            let cmd = Cmd::new(&cat).stdin(reader()).pipe(Cmd::new(&cat));
            cmd.clone().run().expect("first run");
            cmd.run().expect("second run")
        }
        "async" => run_async_twice(&cat),
        other => panic!("unknown mode {other}"),
    };
    std::io::stdout().write_all(&second.stdout).expect("write");
}

#[cfg(feature = "tokio")]
fn run_async_twice(cat: &str) -> procpilot::RunOutput {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let cmd = Cmd::new(cat).stdin(StdinData::from_async_reader(Cursor::new(b"taken once".to_vec())));
        cmd.clone().run_async().await.expect("first run");
        cmd.run_async().await.expect("second run")
    })
}

#[cfg(not(feature = "tokio"))]
fn run_async_twice(_cat: &str) -> procpilot::RunOutput {
    panic!("the async mode needs the tokio feature")
}
