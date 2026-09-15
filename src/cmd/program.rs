//! Building the `Command` for a stage's program.
//!
//! std spawns a bare name (`jj`) with `posix_spawnp`, and macOS's searches `PATH` by attempting a
//! real spawn in each directory: the kernel creates a process for every directory that lacks the
//! program and tears it down again. Sixteenth on a developer's `PATH`, one `jj` cost sixteen PIDs,
//! and when the spawning process is not a Developer Tool each doomed process is also reported to
//! `syspolicyd`, which cannot find it by the time it looks: a tool spawning often drove thousands
//! of `Unable to initialize qtn_proc` errors a second. So the name is resolved here, once, and
//! spawned by path wherever that gives exactly the program the OS's own search would run. Where
//! it might not, the name is left for the OS to search, as it always was.

use std::process::Command;

use super::SingleCmd;

impl SingleCmd {
    /// A `Command` for this stage's program, with nothing else applied yet.
    pub(super) fn program_command(&self) -> Command {
        #[cfg(unix)]
        if self.inherits_path()
            && let Some(path) = std::env::var_os("PATH")
            && let Some(found) = search::resolve(&self.program, &path)
        {
            let mut cmd = Command::new(found);
            // The program still sees the name it was given, as it would have from the OS's search.
            std::os::unix::process::CommandExt::arg0(&mut cmd, &self.program);
            return cmd;
        }
        Command::new(&self.program)
    }

    /// Whether the child's `PATH` is this process's own: the condition std itself checks before
    /// using `posix_spawnp`. A stage that sets, removes or clears it makes std search the child's
    /// environment by fork and `execvp`, which spends no extra processes, so the name is left to it.
    #[cfg(unix)]
    fn inherits_path(&self) -> bool {
        !self.env_clear && !self.env_remove.iter().any(|k| k == "PATH") && !self.envs.iter().any(|(k, _)| k == "PATH")
    }

    pub(super) fn apply_to(&self, cmd: &mut Command) {
        cmd.args(&self.args);
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        if self.env_clear {
            cmd.env_clear();
        }
        for k in &self.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
    }
}

#[cfg(unix)]
mod search {
    use std::ffi::{CString, OsStr};
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    /// What one directory on `PATH` holds under the program's name.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Found {
        /// Nothing the OS would run: absent, a directory, or not executable by us. Its search
        /// moves on to the next directory, and so does ours.
        Nothing,
        /// A file the kernel runs directly: a native binary, or a `#!` script whose interpreter
        /// is present.
        Runnable,
        /// An executable whose outcome we cannot vouch for: unreadable, or a format the OS may
        /// hand to `/bin/sh` or pass over. The search is left to the OS.
        Unsure,
    }

    /// The path the OS's search of `path` would run `program` from, when that is certain.
    pub(super) fn resolve(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
        let name = program.as_bytes();
        if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
            return None;
        }
        first_runnable(program, std::env::split_paths(path), probe)
    }

    /// The first directory's candidate that is [`Found::Runnable`], provided every directory
    /// before it is absolute and holds [`Found::Nothing`]. Anything else ends the search with
    /// `None`: an empty or relative entry is searched relative to the child's working directory,
    /// which `current_dir` changes after we look, and an unsure candidate is the OS's to judge.
    pub(super) fn first_runnable(
        program: &OsStr,
        dirs: impl IntoIterator<Item = PathBuf>,
        probe: impl Fn(&Path) -> Found,
    ) -> Option<PathBuf> {
        for dir in dirs {
            if !dir.is_absolute() {
                return None;
            }
            let candidate = dir.join(program);
            match probe(&candidate) {
                Found::Nothing => {}
                Found::Runnable => return Some(candidate),
                Found::Unsure => return None,
            }
        }
        None
    }

    fn probe(candidate: &Path) -> Found {
        // `metadata` follows symlinks, as exec does.
        if !std::fs::metadata(candidate).is_ok_and(|m| m.is_file()) || !executable(candidate) {
            return Found::Nothing;
        }
        let mut head = Vec::with_capacity(HEAD_BYTES);
        match std::fs::File::open(candidate).and_then(|f| f.take(HEAD_BYTES as u64).read_to_end(&mut head)) {
            Ok(_) => format_of(&head),
            Err(_) => Found::Unsure,
        }
    }

    /// Enough of the file to hold a `#!` line: macOS reads 512 bytes of one.
    const HEAD_BYTES: usize = 512;

    /// Executable by this process's effective ids, which is what exec checks.
    fn executable(path: &Path) -> bool {
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return false };
        // SAFETY: `c` is a NUL-terminated string that outlives the call.
        unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
    }

    #[cfg(target_vendor = "apple")]
    const NATIVE_MAGIC: &[&[u8]] = &[
        &[0xfe, 0xed, 0xfa, 0xce],
        &[0xce, 0xfa, 0xed, 0xfe],
        &[0xfe, 0xed, 0xfa, 0xcf],
        &[0xcf, 0xfa, 0xed, 0xfe],
        &[0xca, 0xfe, 0xba, 0xbe],
        &[0xca, 0xfe, 0xba, 0xbf],
    ];
    #[cfg(not(target_vendor = "apple"))]
    const NATIVE_MAGIC: &[&[u8]] = &[b"\x7fELF"];

    /// Whether the kernel runs a file that starts with `head` directly. A `#!` script whose
    /// interpreter is missing fails with ENOENT, and the OS's search moves past it to a later
    /// directory, so it is unsure rather than runnable.
    pub(super) fn format_of(head: &[u8]) -> Found {
        if let Some(line) = head.strip_prefix(b"#!") {
            return match interpreter(line) {
                Some(p) if std::fs::metadata(p).is_ok_and(|m| m.is_file()) => Found::Runnable,
                _ => Found::Unsure,
            };
        }
        if NATIVE_MAGIC.iter().any(|m| head.starts_with(m)) { Found::Runnable } else { Found::Unsure }
    }

    /// The absolute interpreter path a `#!` line names, when the whole line was read. A `\r` is
    /// part of the name, as it is to the kernel.
    fn interpreter(line: &[u8]) -> Option<&Path> {
        let line = &line[..line.iter().position(|&b| b == b'\n')?];
        let start = line.iter().position(|&b| b != b' ' && b != b'\t')?;
        let rest = &line[start..];
        let name = &rest[..rest.iter().position(|&b| b == b' ' || b == b'\t').unwrap_or(rest.len())];
        let path = Path::new(OsStr::from_bytes(name));
        path.is_absolute().then_some(path)
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use proptest::prelude::*;

    use super::super::SingleCmd;
    use super::search::{Found, first_runnable, format_of, resolve};

    fn stage(env_clear: bool, env_remove: &[&str], envs: &[(&str, &str)]) -> SingleCmd {
        let mut s = SingleCmd::new("jj".into());
        s.env_clear = env_clear;
        s.env_remove = env_remove.iter().map(OsString::from).collect();
        s.envs = envs.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))).collect();
        s
    }

    #[test]
    fn only_a_stage_that_leaves_path_alone_inherits_it() {
        assert!(stage(false, &[], &[]).inherits_path());
        assert!(stage(false, &["HOME"], &[("LANG", "C")]).inherits_path(), "other variables do not matter");
        assert!(!stage(true, &[], &[]).inherits_path(), "a cleared environment has no PATH of ours");
        assert!(!stage(false, &["PATH"], &[]).inherits_path());
        assert!(!stage(false, &[], &[("PATH", "/bin")]).inherits_path());
        assert!(stage(false, &[], &[("Path", "/bin")]).inherits_path(), "std matches the key exactly on unix");
    }

    #[test]
    fn a_stage_whose_path_is_set_keeps_the_bare_name() {
        let s = stage(false, &[], &[("PATH", "/bin")]);
        assert_eq!(s.program_command().get_program(), OsStr::new("jj"));
    }

    fn write_file(dir: &Path, name: &str, contents: &[u8], mode: u32) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    const SCRIPT: &[u8] = b"#!/bin/sh\necho hi\n";

    #[test]
    fn resolves_past_directories_that_lack_the_program() {
        let (a, b, c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let want = write_file(c.path(), "tool", SCRIPT, 0o755);
        assert_eq!(resolve(OsStr::new("tool"), &path_of(&[a.path(), b.path(), c.path()])), Some(want));
    }

    #[test]
    fn passes_over_what_the_os_would_not_run() {
        let (a, b, c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write_file(a.path(), "tool", SCRIPT, 0o644);
        std::fs::create_dir(b.path().join("tool")).unwrap();
        let want = write_file(c.path(), "tool", SCRIPT, 0o755);
        assert_eq!(resolve(OsStr::new("tool"), &path_of(&[a.path(), b.path(), c.path()])), Some(want), "a non-executable file and a directory are skipped");
    }

    #[test]
    fn follows_a_symlink_but_returns_the_path_on_path() {
        let (real, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let target = write_file(real.path(), "tool-1.2", SCRIPT, 0o755);
        let link = bin.path().join("tool");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(resolve(OsStr::new("tool"), &path_of(&[bin.path()])), Some(link), "a package upgrade moves the target, not the link");
    }

    #[test]
    fn leaves_the_search_to_the_os_when_unsure() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write_file(b.path(), "tool", SCRIPT, 0o755);
        write_file(a.path(), "tool", b"echo no shebang\n", 0o755);
        assert_eq!(resolve(OsStr::new("tool"), &path_of(&[a.path(), b.path()])), None, "a script without #! may be run by /bin/sh");
        write_file(a.path(), "tool", b"#!/no/such/interpreter\n", 0o755);
        assert_eq!(resolve(OsStr::new("tool"), &path_of(&[a.path(), b.path()])), None, "the OS moves past a missing interpreter");
        let locked = write_file(a.path(), "tool", SCRIPT, 0o111);
        // Root reads it anyway, and then it is simply runnable.
        let unreadable = std::fs::File::open(&locked).is_err();
        let expected = if unreadable { None } else { Some(locked) };
        assert_eq!(resolve(OsStr::new("tool"), &path_of(&[a.path(), b.path()])), expected, "executable but unreadable");
    }

    #[test]
    fn a_relative_or_empty_entry_before_the_program_ends_the_search() {
        let bin = tempfile::tempdir().unwrap();
        let want = write_file(bin.path(), "tool", SCRIPT, 0o755);
        let bin_path = bin.path().as_os_str().to_owned();
        let mut relative_first = OsString::from("bin:");
        relative_first.push(&bin_path);
        assert_eq!(resolve(OsStr::new("tool"), &relative_first), None);
        let mut empty_first = OsString::from(":");
        empty_first.push(&bin_path);
        assert_eq!(resolve(OsStr::new("tool"), &empty_first), None, "an empty entry is the working directory");
        let mut relative_after = bin_path.clone();
        relative_after.push(":bin");
        assert_eq!(resolve(OsStr::new("tool"), &relative_after), Some(want), "entries after the program do not matter");
    }

    #[test]
    fn names_the_os_would_not_search_are_left_alone() {
        let bin = tempfile::tempdir().unwrap();
        write_file(bin.path(), "tool", SCRIPT, 0o755);
        let path = path_of(&[bin.path()]);
        for name in ["", "./tool", "sub/tool", "to\0ol"] {
            assert_eq!(resolve(OsStr::new(name), &path), None, "{name:?}");
        }
        assert_eq!(resolve(OsStr::new("absent"), &path), None, "a name found nowhere is the OS's to report");
    }

    #[test]
    fn a_native_binary_or_a_script_with_its_interpreter_is_runnable() {
        let exe = std::env::current_exe().unwrap();
        let head: Vec<u8> = std::fs::read(&exe).unwrap().into_iter().take(512).collect();
        assert_eq!(format_of(&head), Found::Runnable, "this test binary");
        assert_eq!(format_of(b"#!/bin/sh\n"), Found::Runnable);
        assert_eq!(format_of(b"#! /bin/sh -e\n"), Found::Runnable, "space before the interpreter, arguments after");
        assert_eq!(format_of(b"#!/bin/sh\r\n"), Found::Unsure, "the kernel reads the \\r as part of the name");
        assert_eq!(format_of(b"#!sh\n"), Found::Unsure, "a relative interpreter");
        assert_eq!(format_of(b"#!/bin/sh"), Found::Unsure, "a #! line longer than the bytes read");
        assert_eq!(format_of(b""), Found::Unsure);
        assert_eq!(format_of(b"echo hi\n"), Found::Unsure);
    }

    /// A PATH entry in a property case: relative, or absolute with what it holds.
    fn arb_entry() -> impl Strategy<Value = Option<Found>> {
        prop_oneof![
            1 => Just(None),
            4 => Just(Some(Found::Nothing)),
            2 => Just(Some(Found::Runnable)),
            1 => Just(Some(Found::Unsure)),
        ]
    }

    proptest! {
        /// The answer is the first entry's candidate exactly when the OS's search is certain to
        /// run that same file; every other layout is left to the OS.
        #[test]
        fn resolves_only_to_the_file_the_os_would_run(entries in prop::collection::vec(arb_entry(), 0..8)) {
            let dirs: Vec<PathBuf> = entries.iter().enumerate()
                .map(|(i, e)| if e.is_some() { PathBuf::from(format!("/d{i}")) } else { PathBuf::from(format!("d{i}")) })
                .collect();
            let held: HashMap<PathBuf, Found> = dirs.iter().zip(&entries)
                .filter_map(|(d, e)| e.map(|f| (d.join("tool"), f)))
                .collect();
            let got = first_runnable(OsStr::new("tool"), dirs.clone(), |c| held[c]);

            let decisive = entries.iter().position(|e| *e != Some(Found::Nothing));
            match got {
                Some(p) => {
                    let i = dirs.iter().position(|d| d.join("tool") == p).unwrap();
                    prop_assert_eq!(entries[i], Some(Found::Runnable));
                    prop_assert_eq!(decisive, Some(i), "every entry before it holds nothing");
                }
                None => prop_assert!(decisive.is_none_or(|i| entries[i] != Some(Found::Runnable))),
            }
        }
    }
}
