//! Basic end-to-end example for procpilot.
//!
//! Run with: cargo run --example basic

use std::time::Duration;

use procpilot::{RunError, run_cmd, run_cmd_in_with_timeout};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Simple captured command
    let output = run_cmd("echo", &["hello from procpilot"])?;
    println!("stdout: {}", output.stdout_lossy().trim());

    // 2. Typed error handling — spawn failure (binary not found) is distinct
    //    from command failure (non-zero exit).
    match run_cmd("procpilot_example_missing_binary_xyz", &[]) {
        Ok(_) => println!("unexpected: binary doesn't exist but succeeded?"),
        Err(RunError::Spawn { source, .. }) => {
            println!("couldn't spawn binary: {source}");
        }
        Err(other) => println!("other failure: {other}"),
    }

    // 3. Timeout — kill if too slow
    let tmp = std::env::temp_dir();
    match run_cmd_in_with_timeout(&tmp, "sleep", &["10"], Duration::from_millis(100)) {
        Ok(_) => println!("sleep finished unexpectedly"),
        Err(RunError::Timeout { elapsed, .. }) => {
            println!("killed sleep after {elapsed:?}");
        }
        Err(other) => println!("sleep failed: {other}"),
    }

    Ok(())
}
