//! Basic end-to-end example for procpilot.
//!
//! Run with: cargo run --example basic

use std::time::Duration;

use procpilot::{Cmd, RunError};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Simple captured command.
    let output = Cmd::new("echo").arg("hello from procpilot").run()?;
    println!("stdout: {}", output.stdout_lossy().trim());

    // 2. Typed error handling — spawn failure (binary not found) is distinct
    //    from command failure (non-zero exit).
    match Cmd::new("procpilot_example_missing_binary_xyz").run() {
        Ok(_) => println!("unexpected: binary doesn't exist but succeeded?"),
        Err(RunError::Spawn { source, .. }) => {
            println!("couldn't spawn binary: {source}");
        }
        Err(other) => println!("other failure: {other}"),
    }

    // 3. Timeout — kill if too slow.
    match Cmd::new("sleep")
        .arg("10")
        .timeout(Duration::from_millis(100))
        .run()
    {
        Ok(_) => println!("sleep finished unexpectedly"),
        Err(RunError::Timeout { elapsed, .. }) => {
            println!("killed sleep after {elapsed:?}");
        }
        Err(other) => println!("sleep failed: {other}"),
    }

    // 4. Stdin piped into the child (kubectl apply -f - style).
    let out = Cmd::new("cat").stdin("piped input\n").run()?;
    println!("echoed: {}", out.stdout_lossy().trim());

    Ok(())
}
