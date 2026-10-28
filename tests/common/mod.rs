//! Test isolation for command-level tests.
//!
//! Every command runs with `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and the
//! `XDG_*` directories pointing into a fresh temporary directory, with
//! `D_MCP_STORE` removed and the current directory inside the temporary
//! directory. No test touches the real `~/.d-mcp` or real harness
//! configuration. `PATH` is inherited unless a test controls it with
//! [`TestEnv::with_path`] or [`TestEnv::without_codex`], which matters
//! because a `codex` binary on `PATH` makes Codex count as present.

#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use tempfile::TempDir;

/// An isolated environment for running `d-mcp`.
pub struct TestEnv {
    dir: TempDir,
    path: Option<OsString>,
    codex_home: bool,
}

impl TestEnv {
    /// Creates the isolated directories.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a temp dir");
        for sub in [
            "home",
            "claude",
            "codex",
            "xdg/config",
            "xdg/data",
            "xdg/state",
            "xdg/cache",
            "work",
        ] {
            fs::create_dir_all(dir.path().join(sub)).expect("create isolation dirs");
        }
        Self {
            dir,
            path: None,
            codex_home: true,
        }
    }

    /// Sets `PATH` to exactly these directories.
    pub fn with_path(mut self, dirs: &[PathBuf]) -> Self {
        self.path = Some(std::env::join_paths(dirs).expect("join PATH"));
        self
    }

    /// Sets `PATH` to an empty directory, so no `codex` binary is found.
    pub fn without_codex(self) -> Self {
        let empty = self.root().join("empty-bin");
        fs::create_dir_all(&empty).expect("create empty bin dir");
        self.with_path(&[empty])
    }

    /// Unsets `CODEX_HOME`, so Codex falls back to `$HOME/.codex`.
    pub fn without_codex_home(mut self) -> Self {
        self.codex_home = false;
        self
    }

    /// The isolated `CODEX_HOME` (or `$HOME/.codex` without one).
    pub fn codex_home(&self) -> PathBuf {
        if self.codex_home {
            self.root().join("codex")
        } else {
            self.home().join(".codex")
        }
    }

    /// The Codex config file.
    pub fn codex_config(&self) -> PathBuf {
        self.codex_home().join("config.toml")
    }

    /// Installs `tests/support/fake-codex.sh` as `<root>/bin/codex` (mode
    /// 0755) and returns the directory.
    pub fn fake_codex_dir(&self) -> PathBuf {
        let bin = self.root().join("bin");
        fs::create_dir_all(&bin).expect("create bin dir");
        let target = bin.join("codex");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-codex.sh"),
            &target,
        )
        .expect("install fake codex");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
                .expect("make fake codex executable");
        }
        bin
    }

    /// The library environment matching [`TestEnv::cmd`], for in-process
    /// tests of the sync engine.
    pub fn environment(&self) -> d_mcp::store::Environment {
        d_mcp::store::Environment {
            home: Some(self.home().into_os_string()),
            store: None,
            cwd: Some(self.work()),
            codex_home: self
                .codex_home
                .then(|| self.root().join("codex").into_os_string()),
            path: self.path.clone().or_else(|| std::env::var_os("PATH")),
        }
    }

    /// The temporary root.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The isolated `HOME`.
    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    /// The default store, `$HOME/.d-mcp`.
    pub fn default_store(&self) -> PathBuf {
        self.home().join(".d-mcp")
    }

    /// The working directory of every command.
    pub fn work(&self) -> PathBuf {
        self.root().join("work")
    }

    /// A `d-mcp` command with the isolated environment and no `D_MCP_STORE`.
    pub fn cmd(&self) -> Command {
        let mut cmd = cargo_bin_cmd!("d-mcp");
        cmd.env("HOME", self.home())
            .env("CLAUDE_CONFIG_DIR", self.root().join("claude"))
            .env("XDG_CONFIG_HOME", self.root().join("xdg/config"))
            .env("XDG_DATA_HOME", self.root().join("xdg/data"))
            .env("XDG_STATE_HOME", self.root().join("xdg/state"))
            .env("XDG_CACHE_HOME", self.root().join("xdg/cache"))
            .env("XDG_RUNTIME_DIR", self.root().join("xdg"))
            .env_remove("D_MCP_STORE")
            .current_dir(self.work());
        if self.codex_home {
            cmd.env("CODEX_HOME", self.root().join("codex"));
        } else {
            cmd.env_remove("CODEX_HOME");
        }
        if let Some(path) = &self.path {
            cmd.env("PATH", path);
        }
        cmd
    }

    /// Runs `d-mcp <args>` in text mode and returns exit code, stdout and
    /// stderr.
    pub fn text(&self, args: &[&str]) -> (i32, String, String) {
        let output = self.cmd().args(args).output().expect("run d-mcp");
        (
            output.status.code().expect("exit code"),
            String::from_utf8(output.stdout).expect("utf-8 stdout"),
            String::from_utf8(output.stderr).expect("utf-8 stderr"),
        )
    }

    /// Runs `d-mcp --json <args>` and returns exit code, parsed stdout and
    /// stderr.
    pub fn json(&self, args: &[&str]) -> (i32, Value, String) {
        let output = self
            .cmd()
            .arg("--json")
            .args(args)
            .output()
            .expect("run d-mcp");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        let value: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout}"));
        (
            output.status.code().expect("exit code"),
            value,
            String::from_utf8(output.stderr).expect("utf-8 stderr"),
        )
    }

    /// Initializes the default store.
    pub fn init(&self) {
        self.cmd().arg("init").assert().success();
    }

    /// Writes `servers/<name>.toml` in the built-in local source.
    pub fn write_local(&self, name: &str, text: &str) {
        let servers = self.default_store().join("local/servers");
        fs::create_dir_all(&servers).expect("create local/servers");
        fs::write(servers.join(format!("{name}.toml")), text).expect("write definition");
    }

    /// Creates a source directory `<root>/<name>` with a `servers/`
    /// directory and returns its path.
    pub fn source_dir(&self, name: &str) -> PathBuf {
        let dir = self.root().join(name);
        fs::create_dir_all(dir.join("servers")).expect("create source dir");
        dir
    }
}

/// A minimal valid stdio definition.
pub fn stdio(name: &str, command: &str) -> String {
    format!("name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"{command}\"\n")
}

/// Snapshot of every file and directory below `root`, with file contents.
pub fn snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut entries = Vec::new();
    if fs::symlink_metadata(root).is_err() {
        return entries;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = fs::symlink_metadata(&path).expect("metadata");
        if metadata.is_dir() {
            entries.push((path.clone(), None));
            for entry in fs::read_dir(&path).expect("read dir") {
                stack.push(entry.expect("entry").path());
            }
        } else {
            entries.push((path.clone(), Some(fs::read(&path).expect("read"))));
        }
    }
    entries.sort();
    entries
}
