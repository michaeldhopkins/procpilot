//! Owned, shell-escaped, secret-respecting snapshot of a command for display.
//!
//! `CmdDisplay` is what error variants and tracing hooks carry — no lifetime
//! entanglement with [`Cmd`](crate::Cmd), no references back into a builder.
//! It implements `Display`, `Debug`, `Clone`, `Send`, `Sync`, `'static`.
//!
//! Supports both single commands and pipelines (`a | b | c`); for pipelines
//! the `Display` impl renders shell-style with ` | ` separators, and
//! [`program`](CmdDisplay::program) / [`raw_args`](CmdDisplay::raw_args)
//! return the first stage.

use std::ffi::OsString;
use std::fmt;

/// One stage in a pipeline, or the whole command for a single invocation.
#[derive(Debug, Clone)]
pub struct StageDisplay {
    program: OsString,
    args: Vec<OsString>,
}

impl StageDisplay {
    pub fn program(&self) -> &OsString {
        &self.program
    }
    pub fn raw_args(&self) -> &[OsString] {
        &self.args
    }
}

/// Owned snapshot of a command's program + args, formatted shell-style on
/// `Display`. For pipelines, renders each stage separated by ` | `.
///
/// If the source [`Cmd`](crate::Cmd) was marked `.secret()`, the args are
/// replaced with `<secret>` in `Display` output.
#[derive(Clone)]
pub struct CmdDisplay {
    stages: Vec<StageDisplay>,
    secret: bool,
}

impl fmt::Debug for CmdDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("CmdDisplay");
        if self.secret {
            s.field("stages", &"<secret>");
        } else {
            s.field("stages", &self.stages);
        }
        s.field("secret", &self.secret).finish()
    }
}

impl CmdDisplay {
    pub(crate) fn new(program: OsString, args: Vec<OsString>, secret: bool) -> Self {
        Self {
            stages: vec![StageDisplay { program, args }],
            secret,
        }
    }

    pub(crate) fn push_stage(&mut self, program: OsString, args: Vec<OsString>) {
        self.stages.push(StageDisplay { program, args });
    }

    /// Program name of the first stage (for single commands, the program).
    pub fn program(&self) -> &OsString {
        &self.stages[0].program
    }

    /// Whether the source `Cmd` was marked secret.
    pub fn is_secret(&self) -> bool {
        self.secret
    }

    /// Raw args of the first stage.
    pub fn raw_args(&self) -> &[OsString] {
        &self.stages[0].args
    }

    /// All stages (≥ 1). Length > 1 indicates a pipeline.
    pub fn stages(&self) -> &[StageDisplay] {
        &self.stages
    }

    /// Whether this snapshot represents a multi-stage pipeline.
    pub fn is_pipeline(&self) -> bool {
        self.stages.len() > 1
    }
}

impl fmt::Display for CmdDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, stage) in self.stages.iter().enumerate() {
            if i > 0 {
                f.write_str(" | ")?;
            }
            write!(f, "{}", shell_quote_os(&stage.program))?;
            if self.secret {
                write!(f, " <secret>")?;
            } else {
                for arg in &stage.args {
                    write!(f, " {}", shell_quote_os(arg))?;
                }
            }
        }
        Ok(())
    }
}

/// Shell-style quote an `OsStr`. If the value is empty, contains whitespace,
/// or contains shell-special characters, wrap in single quotes (and escape
/// embedded single quotes by closing-then-reopening). Otherwise pass through
/// unchanged. Falls back to lossy display for non-UTF-8 values.
fn shell_quote_os(s: &std::ffi::OsStr) -> String {
    let lossy = s.to_string_lossy();
    let needs_quote = lossy.is_empty()
        || lossy.chars().any(|c| {
            !(c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '_' | '-' | '.' | '/' | ':' | '@' | '%' | '+' | '=' | ','
                ))
        });
    if !needs_quote {
        return lossy.into_owned();
    }
    let mut out = String::with_capacity(lossy.len() + 2);
    out.push('\'');
    for ch in lossy.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cd(program: &str, args: &[&str], secret: bool) -> CmdDisplay {
        CmdDisplay::new(
            program.into(),
            args.iter().map(|s| OsString::from(*s)).collect(),
            secret,
        )
    }

    #[test]
    fn simple_command() {
        let d = cd("git", &["status"], false);
        assert_eq!(d.to_string(), "git status");
        assert!(!d.is_pipeline());
    }

    #[test]
    fn no_args() {
        let d = cd("ls", &[], false);
        assert_eq!(d.to_string(), "ls");
    }

    #[test]
    fn args_with_spaces_are_quoted() {
        let d = cd("git", &["commit", "-m", "fix bug"], false);
        assert_eq!(d.to_string(), "git commit -m 'fix bug'");
    }

    #[test]
    fn empty_arg_quoted() {
        let d = cd("echo", &[""], false);
        assert_eq!(d.to_string(), "echo ''");
    }

    #[test]
    fn embedded_single_quote_escaped() {
        let d = cd("echo", &["it's fine"], false);
        assert_eq!(d.to_string(), "echo 'it'\\''s fine'");
    }

    #[test]
    fn safe_chars_unquoted() {
        let d = cd("git", &["log", "-r", "trunk()..@", "--no-graph"], false);
        assert_eq!(d.to_string(), "git log -r 'trunk()..@' --no-graph");
    }

    #[test]
    fn paths_unquoted() {
        let d = cd("cat", &["/tmp/file.txt", "/usr/bin/foo"], false);
        assert_eq!(d.to_string(), "cat /tmp/file.txt /usr/bin/foo");
    }

    #[test]
    fn secret_redacts_args() {
        let d = cd("docker", &["login", "-p", "hunter2"], true);
        assert_eq!(d.to_string(), "docker <secret>");
    }

    #[test]
    fn secret_preserves_program() {
        let d = cd("docker", &["login", "-p", "hunter2"], true);
        assert_eq!(d.program(), &OsString::from("docker"));
        assert!(d.is_secret());
        assert_eq!(d.raw_args().len(), 3);
    }

    #[test]
    fn special_chars_quoted() {
        let d = cd("sh", &["-c", "echo foo > bar"], false);
        assert_eq!(d.to_string(), "sh -c 'echo foo > bar'");
    }

    #[test]
    fn debug_impl_works() {
        let d = cd("git", &["status"], false);
        let dbg = format!("{d:?}");
        assert!(dbg.contains("git"));
        assert!(dbg.contains("status"));
    }

    #[test]
    fn debug_respects_secret_flag() {
        let d = cd("docker", &["login", "-p", "hunter2"], true);
        let dbg = format!("{d:?}");
        assert!(!dbg.contains("hunter2"), "secret leaked in Debug: {dbg}");
        assert!(dbg.contains("<secret>"));
    }

    #[test]
    fn pipeline_renders_with_separator() {
        let mut d = cd("git", &["log"], false);
        d.push_stage("grep".into(), vec!["foo".into()]);
        d.push_stage("head".into(), vec!["-5".into()]);
        assert_eq!(d.to_string(), "git log | grep foo | head -5");
        assert!(d.is_pipeline());
        assert_eq!(d.stages().len(), 3);
    }

    #[test]
    fn pipeline_with_secret_redacts_every_stage() {
        let mut d = cd("docker", &["login", "-p", "hunter2"], true);
        d.push_stage("jq".into(), vec![".token".into()]);
        let rendered = d.to_string();
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains(".token"));
        assert!(rendered.contains("docker <secret>"));
        assert!(rendered.contains("jq <secret>"));
    }

}
