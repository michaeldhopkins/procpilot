//! A command rendered by `CmdDisplay`, pasted into a POSIX shell, runs the same argv.
//!
//! Programs and arguments are whatever the caller hands `Cmd`: file names, branch names, user
//! input. The rendered form goes into logs and error messages, where people copy it into a shell
//! to reproduce a failure, so it must quote everything the shell would otherwise reinterpret.
//!
//! Asserts, with `shlex` as an independent POSIX word splitter:
//! - not secret: splitting the render gives back every stage's program and arguments, in order,
//!   with a `|` token between stages;
//! - secret: splitting gives exactly each program followed by `<secret>`, so no argument leaks;
//! - a program left unquoted is never read by the shell as something other than a command name
//!   (a `NAME=value` assignment, or a reserved word such as `if`).
//!
//! Words that are not UTF-8 are rendered lossily by design, so no argv can come back from them:
//! for those inputs only the never-panics half applies.
#![no_main]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use procpilot::Cmd;

#[derive(Arbitrary, Debug)]
struct Stage {
    program: Vec<u8>,
    args: Vec<Vec<u8>>,
}

#[derive(Arbitrary, Debug)]
struct Input {
    first: Stage,
    rest: Vec<Stage>,
    secret: bool,
}

/// Words a POSIX shell (and bash) reads as syntax in command position.
const RESERVED: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "fi", "for", "if", "in", "then", "until", "while",
    "function", "select", "time", "coproc",
];

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else { return false };
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn cmd_of(stage: &Stage) -> Cmd {
    Cmd::new(OsString::from_vec(stage.program.clone()))
        .args(stage.args.iter().map(|a| OsString::from_vec(a.clone())))
}

/// Characters a POSIX shell (or bash) expands or treats as syntax when they are not quoted.
const SPECIAL: &str = "$`\\\"'*?[]#~!&;()<>{}^";

/// Outside single quotes, the render holds no shell-special character. `shlex` does no expansion,
/// so it splits a bare `$HOME` or `*` back unchanged and the roundtrip alone cannot see one; this
/// is the half of the oracle that can. The ` | ` stage separators and the `<secret>` marker are
/// the renderer's own syntax and are taken out first.
fn assert_no_bare_special(rendered: &str, secret: bool) {
    let text = if secret { rendered.replace(" <secret>", "") } else { rendered.to_string() };
    let text = text.replace(" | ", " ");
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            quoted = !quoted;
        } else if !quoted && c == '\\' && chars.peek() == Some(&'\'') {
            // `\'` between two quoted runs: how a single quote is written inside one.
            chars.next();
        } else if !quoted {
            assert!(!SPECIAL.contains(c) && c != '|', "bare {c:?} in render {rendered:?}");
        }
    }
    assert!(!quoted, "unterminated quote in render {rendered:?}");
}

fn utf8(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec()).ok()
}

fuzz_target!(|input: Input| {
    let stages: Vec<&Stage> = std::iter::once(&input.first).chain(&input.rest).collect();
    let mut cmd = cmd_of(stages[0]);
    for stage in &stages[1..] {
        cmd = cmd.pipe(cmd_of(stage));
    }
    if input.secret {
        cmd = cmd.secret();
    }
    let display = cmd.display();
    let rendered = display.to_string();
    let _ = format!("{display:?}");
    assert_no_bare_special(&rendered, input.secret);
    assert_eq!(display.stages().len(), stages.len());

    let programs: Option<Vec<String>> = stages.iter().map(|s| utf8(&s.program)).collect();
    let Some(programs) = programs else { return };

    for program in &programs {
        let alone = Cmd::new(program).display().to_string();
        if &alone == program {
            assert!(!is_assignment(program), "program {program:?} rendered bare reads as an assignment");
            assert!(!RESERVED.contains(&program.as_str()), "program {program:?} rendered bare is a reserved word");
        }
    }

    let mut expected = Vec::new();
    for (i, (stage, program)) in stages.iter().zip(&programs).enumerate() {
        if i > 0 {
            expected.push("|".to_string());
        }
        expected.push(program.clone());
        if input.secret {
            expected.push("<secret>".to_string());
        } else {
            for arg in &stage.args {
                let Some(arg) = utf8(arg) else { return };
                expected.push(arg);
            }
        }
    }
    assert_eq!(shlex::split(&rendered), Some(expected), "render: {rendered:?}");
});
