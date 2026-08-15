// Shared scaffolding for the integration tests — the ones that run the real
// `scout` binary as a subprocess instead of calling into the crate.
//
// Everything in here exists to make that safe to do on a developer's machine.
// scout reads its config from `$XDG_CONFIG_HOME`, seeds one if it is missing,
// appends a row to `$XDG_STATE_HOME/scout/calls.jsonl` on every filter call, and
// merges user preset overrides from `$SCOUT_PRESET_DIR`. A test that inherited
// the ambient environment would write to the developer's real state, and — worse
// for a test — would *read* their real presets, so whether it passed would
// depend on whose machine it ran on.
//
// Not compiled as its own test target: cargo only turns top-level `tests/*.rs`
// into test binaries, so a subdirectory module is the idiomatic place for this.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A throwaway HOME + project root for one test.
///
/// Holds the `TempDir` alive; when it drops, the whole tree goes. Each test
/// makes its own, which is what keeps the suite parallel-safe — cargo runs
/// integration tests concurrently and there is no shared path or port here.
pub struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let dir = TempDir::new().expect("tempdir");
        for sub in ["home", "project", "presets", "run"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("sandbox subdir");
        }
        Sandbox { dir }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The directory the binary is run from — scout's "project".
    pub fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// Create a file under the project root, parent directories and all.
    pub fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.project().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture parent dir");
        }
        std::fs::write(&path, contents).expect("fixture file");
        path
    }

    /// A `scout` invocation pinned to this sandbox.
    ///
    /// `CARGO_BIN_EXE_scout` is the binary cargo just built for this test run,
    /// so these tests always exercise the current tree rather than whatever
    /// `scout` happens to be on `PATH` (which, on a dev machine, is the plugin
    /// payload copy and can be weeks stale).
    ///
    /// The env is scrubbed rather than cleared: `check_output` and the shell
    /// helpers need a usable `PATH`, and clearing everything would make the
    /// sandbox less like the environment scout actually runs in. Every variable
    /// scout itself consults is overridden explicitly.
    pub fn scout(&self) -> Command {
        let home = self.dir.path().join("home");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_scout"));
        cmd.current_dir(self.project())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".state"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("run"))
            // An empty override directory, so the built-in presets compiled
            // into the binary are exactly what the test sees. Without this a
            // developer with a `~/.config/scout/presets/grep.toml` would get
            // different tool schemas than CI does.
            .env("SCOUT_PRESET_DIR", self.dir.path().join("presets"))
            .env("SCOUT_CALLS_LOG", self.dir.path().join("calls.jsonl"))
            // Never let a stray dashboard socket or config path leak in.
            .env_remove("SCOUT_LIVE_SOCK")
            .env_remove("SCOUT_CONFIG")
            .env_remove("SCOUT_VIA")
            .env_remove("LM_HOST")
            .env_remove("LM_MODEL")
            // Output is piped anyway, so color is already off; this pins it in
            // case a test ever captures a TTY-shaped stream.
            .env("NO_COLOR", "1");
        cmd
    }
}
