//! Owned, shell-escaped, secret-respecting snapshot of a command for display.
//!
//! `CmdDisplay` is what error variants and tracing hooks carry — no lifetime
//! entanglement with [`Cmd`](crate::Cmd), no references back into a builder.
//! It implements `Display`, `Debug`, `Clone`, `Send`, `Sync`, `'static`.

use std::ffi::OsString;
use std::fmt;

/// Owned snapshot of a command's program + args, formatted shell-style on
/// `Display`.
///
/// If the source [`Cmd`](crate::Cmd) was marked `.secret()`, the args are
/// replaced with `<secret>` in `Display` output but the field values are
/// preserved structurally — useful when you want to redact for human-readable
/// logs but retain raw data for structured (and themselves-redacted)
/// observability sinks.
#[derive(Clone)]
pub struct CmdDisplay {
    program: OsString,
    args: Vec<OsString>,
    secret: bool,
}

impl fmt::Debug for CmdDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Respect secret flag in Debug too — otherwise `format!("{err:?}")`
        // leaks redacted args into logs.
        let mut s = f.debug_struct("CmdDisplay");
        s.field("program", &self.program);
        if self.secret {
            s.field("args", &"<secret>");
        } else {
            s.field("args", &self.args);
        }
        s.field("secret", &self.secret).finish()
    }
}

impl CmdDisplay {
    pub(crate) fn new(program: OsString, args: Vec<OsString>, secret: bool) -> Self {
        Self {
            program,
            args,
            secret,
        }
    }

    /// The program name (always shown, even when `secret` is true — only
    /// args get redacted, on the assumption that the program path itself is
    /// not sensitive).
    pub fn program(&self) -> &OsString {
        &self.program
    }

    /// Whether the source `Cmd` was marked secret.
    pub fn is_secret(&self) -> bool {
        self.secret
    }

    /// Args, raw. For redacted access, format via `Display`.
    pub fn raw_args(&self) -> &[OsString] {
        &self.args
    }
}

impl fmt::Display for CmdDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", shell_quote_os(&self.program))?;
        if self.secret {
            write!(f, " <secret>")?;
        } else {
            for arg in &self.args {
                write!(f, " {}", shell_quote_os(arg))?;
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
            // close quote, escaped quote, reopen
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
        // parens trigger quoting
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
}
