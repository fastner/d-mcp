//! Test isolation for command-level tests.
//!
//! Every command runs with an otherwise empty environment: `HOME`,
//! `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and the `XDG_*` directories point into
//! a fresh temporary directory, `D_MCP_STORE` is unset and the current
//! directory is inside the temporary directory. No test touches the real `~/.d-mcp` or real harness
//! configuration.
//!
//! `PATH` is always controlled: by default it holds only an empty directory,
//! so neither a real `codex` nor a real `claude` is ever found or run (a
//! binary on `PATH` makes its harness count as present, and d-mcp writes
//! Claude Code entries through `claude`). Tests that need a harness CLI put
//! the fakes on `PATH` with [`TestEnv::with_path`], [`TestEnv::fake_codex_dir`]
//! and [`TestEnv::fake_claude_dir`]. The fake `claude` refuses to run unless
//! its `HOME` lies in a directory marked by [`TEST_ROOT_MARKER`].
//!
//! Git tests use [`TestEnv::with_git`]: `PATH` then holds only a symlink to
//! the resolved `git` binary, and git's configuration is isolated for d-mcp
//! and for the repository builders alike (`GIT_CONFIG_GLOBAL` points to a
//! file in the temporary root, `GIT_CONFIG_NOSYSTEM=1`, and
//! `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS` are unset). Remotes are
//! local bare repositories built by [`Remote`]; no test uses the network.

#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use tempfile::TempDir;

/// The marker file in the temporary root; the fake `claude` runs only when
/// its `HOME` is a child of a directory that holds it.
pub const TEST_ROOT_MARKER: &str = ".d-mcp-test-root";

/// An isolated environment for running `d-mcp`.
pub struct TestEnv {
    /// Keeps the temporary directory alive; removed on drop.
    _dir: TempDir,
    /// The canonical form of the temporary directory: d-mcp sees the
    /// working directory through `getcwd`, which resolves symlinks (on
    /// macOS the temp dir lies under `/var`, a symlink to `/private/var`),
    /// so every path a test compares against must be canonical too.
    root: PathBuf,
    path: OsString,
    codex_home: bool,
    claude_config_dir: bool,
    git_config: Option<PathBuf>,
}

impl TestEnv {
    /// Creates the isolated directories.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a temp dir");
        let root = dir
            .path()
            .canonicalize()
            .expect("canonicalize the temp dir");
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
            fs::create_dir_all(root.join(sub)).expect("create isolation dirs");
        }
        fs::write(root.join(TEST_ROOT_MARKER), "").expect("write the test root marker");
        let empty = root.join("empty-bin");
        fs::create_dir_all(&empty).expect("create empty bin dir");
        Self {
            path: std::env::join_paths([&empty]).expect("join PATH"),
            _dir: dir,
            root,
            codex_home: true,
            claude_config_dir: true,
            git_config: None,
        }
    }

    /// Puts only the resolved `git` binary on `PATH` (a symlink in
    /// `<root>/git-bin`) and gives d-mcp's git the isolated global
    /// configuration [`TestEnv::git_config`] (an empty file) and no system
    /// configuration.
    pub fn with_git(mut self) -> Self {
        let bin = self.git_bin();
        fs::create_dir_all(&bin).expect("create git bin dir");
        let link = bin.join("git");
        if fs::symlink_metadata(&link).is_err() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(system_git(), &link).expect("link git");
        }
        let config = self.root().join("gitconfig");
        fs::write(&config, "").expect("write the isolated git config");
        self.git_config = Some(config);
        self.with_path(&[bin])
    }

    /// The directory that holds the `git` symlink of [`TestEnv::with_git`].
    pub fn git_bin(&self) -> PathBuf {
        self.root().join("git-bin")
    }

    /// The global git configuration d-mcp's git reads with
    /// [`TestEnv::with_git`].
    pub fn git_config(&self) -> PathBuf {
        self.root().join("gitconfig")
    }

    /// A bare repository `<root>/remotes/<name>.git` with a work tree
    /// `<root>/remotes/<name>-work` whose `origin` it is.
    pub fn remote(&self, name: &str) -> Remote {
        Remote::new(self.root(), name)
    }

    /// Sets `PATH` to exactly these directories.
    pub fn with_path(mut self, dirs: &[PathBuf]) -> Self {
        self.path = std::env::join_paths(dirs).expect("join PATH");
        self
    }

    /// Sets `PATH` to an empty directory, so no `codex` binary is found (the
    /// default).
    pub fn without_codex(self) -> Self {
        let empty = self.root().join("empty-bin");
        self.with_path(&[empty])
    }

    /// Sets `PATH` to an empty directory, so no `claude` binary is found (the
    /// default).
    pub fn without_claude(self) -> Self {
        self.without_codex()
    }

    /// Unsets `CLAUDE_CONFIG_DIR`, so Claude Code falls back to
    /// `$HOME/.claude.json`.
    pub fn without_claude_config_dir(mut self) -> Self {
        self.claude_config_dir = false;
        self
    }

    /// The isolated `CLAUDE_CONFIG_DIR`.
    pub fn claude_dir(&self) -> PathBuf {
        self.root().join("claude")
    }

    /// Claude Code's configuration file (no legacy `.config.json`).
    pub fn claude_config(&self) -> PathBuf {
        if self.claude_config_dir {
            self.claude_dir().join(".claude.json")
        } else {
            self.home().join(".claude.json")
        }
    }

    /// Installs the fake `claude` (the `fake_claude` test target, built from
    /// `tests/support/fake_claude.rs`) as `<root>/claude-bin/claude` and
    /// returns the directory.
    pub fn fake_claude_dir(&self) -> PathBuf {
        let bin = self.root().join("claude-bin");
        fs::create_dir_all(&bin).expect("create claude bin dir");
        let built = fake_claude_binary();
        let target = bin.join("claude");
        fs::copy(&built, &target).expect("install fake claude");
        bin
    }

    /// The fake `claude`'s call log: one JSON object per call.
    pub fn claude_calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root().join("fake-claude.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("fake claude log line"))
            .collect()
    }

    /// Sets the fake `claude`'s directives, one per line.
    pub fn claude_mode(&self, lines: &[&str]) {
        fs::write(self.root().join("fake-claude-mode"), lines.join("\n"))
            .expect("write fake claude mode");
        let _ = fs::remove_file(self.root().join("fake-claude-state"));
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
            path: Some(self.path.clone()),
            claude_config_dir: self
                .claude_config_dir
                .then(|| self.claude_dir().into_os_string()),
        }
    }

    /// The temporary root, canonicalized.
    pub fn root(&self) -> &Path {
        &self.root
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
    ///
    /// The environment starts empty, so no variable of the developer's shell
    /// (such as a `GITHUB_TOKEN` a test expects to be unset) reaches d-mcp.
    /// Only the system temp directory (`TMPDIR`, which the fakes use to find
    /// the test root) and the coverage variables of `cargo llvm-cov`
    /// (`LLVM_PROFILE_FILE`, `CARGO_LLVM_COV*`) are passed on.
    pub fn cmd(&self) -> Command {
        let mut cmd = cargo_bin_cmd!("d-mcp");
        cmd.env_clear();
        for (name, value) in std::env::vars_os() {
            let keep = name.to_str().is_some_and(|name| {
                name == "TMPDIR"
                    || name == "LLVM_PROFILE_FILE"
                    || name.starts_with("CARGO_LLVM_COV")
            });
            if keep {
                cmd.env(name, value);
            }
        }
        cmd.env("HOME", self.home())
            .env("PATH", &self.path)
            .env("XDG_CONFIG_HOME", self.root().join("xdg/config"))
            .env("XDG_DATA_HOME", self.root().join("xdg/data"))
            .env("XDG_STATE_HOME", self.root().join("xdg/state"))
            .env("XDG_CACHE_HOME", self.root().join("xdg/cache"))
            .env("XDG_RUNTIME_DIR", self.root().join("xdg"))
            .env_remove("D_MCP_STORE")
            .env_remove("MCP_CLIENT_SECRET")
            .current_dir(self.work());
        if self.claude_config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", self.claude_dir());
        } else {
            cmd.env_remove("CLAUDE_CONFIG_DIR");
        }
        if self.codex_home {
            cmd.env("CODEX_HOME", self.root().join("codex"));
        } else {
            cmd.env_remove("CODEX_HOME");
        }
        if let Some(config) = &self.git_config {
            cmd.env("GIT_CONFIG_GLOBAL", config)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_PARAMETERS");
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

/// The built fake `claude`: the `fake_claude` test target (no libtest
/// harness) next to this test executable in `target/<profile>/deps/`
/// (`cargo llvm-cov` uses its own target directory with the same layout).
/// Every `cargo test` without `--test` builds it; a run filtered with
/// `--test` needs `--test fake_claude` as well.
///
/// The lookup never uses a stale build. A candidate counts only when it is
/// fresh the way cargo decides it: the binary is at least as new as every
/// source listed in its rustc dep-info file (`fake_claude-<hash>.d`: the
/// fake's source, the fixtures it includes and `Cargo.toml`). Among fresh
/// candidates (one per feature set or profile, all built from the same
/// sources) the newest wins, ties broken by name. Without a fresh candidate
/// the test panics with the command that builds one.
pub fn fake_claude_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let deps = exe.parent().expect("target/<profile>/deps");
    let suffix = std::env::consts::EXE_SUFFIX;
    let mut fresh: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(deps)
        .expect("read the deps directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let rest = name.strip_prefix("fake_claude-")?;
            let hash = rest.strip_suffix(suffix).unwrap_or(rest);
            if hash.len() != 16 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            let built = fs::metadata(&path).ok().filter(fs::Metadata::is_file)?;
            let built = built.modified().ok()?;
            let dep_info = deps.join(format!("fake_claude-{hash}.d"));
            is_fresh(&dep_info, built).then_some((built, path))
        })
        .collect();
    fresh.sort();
    fresh.pop().map(|(_, path)| path).unwrap_or_else(|| {
        panic!(
            "no up-to-date fake_claude build in {}; run `cargo test` (or add `--test fake_claude`)",
            deps.display()
        )
    })
}

/// Whether a build is at least as new as every source its rustc dep-info
/// file lists. Source paths are relative to the package root; `\ ` escapes
/// a space. A missing or empty dep-info file, or a missing source, is not
/// fresh.
pub fn is_fresh(dep_info: &Path, built: std::time::SystemTime) -> bool {
    let Ok(text) = fs::read_to_string(dep_info) else {
        return false;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // rustc lists every source once more as a rule without prerequisites,
    // `<source>:`; the rules of the outputs have `: ` and prerequisites.
    let sources: Vec<PathBuf> = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_suffix(':'))
        .filter(|source| !source.is_empty() && !source.contains(": "))
        .map(|source| root.join(source.replace("\\ ", " ")))
        .collect();
    !sources.is_empty()
        && sources.iter().all(|source| {
            fs::metadata(source)
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified <= built)
        })
}

/// The combined fixture `tests/fixtures/real-setup/`, installed into a
/// [`TestEnv`] by [`TestEnv::install_real_setup`].
pub struct RealSetup {
    /// The repository root (the isolated working directory, with `.git`).
    pub repo: PathBuf,
    /// The working directory for commands: `<repo>/sub`, so the broken Codex
    /// layer in `sub/.codex/` is read too.
    pub cwd: PathBuf,
    /// The team source with a canary in a definition's `description`.
    pub team: PathBuf,
}

/// Every canary of the fixtures and fakes starts with this.
pub const CANARY: &str = "CANARY";

fn copy_tree(from: &Path, to: &Path, substitute: &dyn Fn(String) -> String) {
    fs::create_dir_all(to).expect("create fixture dir");
    for entry in fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target, substitute);
        } else {
            let text = fs::read_to_string(entry.path()).expect("read fixture");
            fs::write(&target, substitute(text)).expect("write fixture");
        }
    }
}

impl TestEnv {
    /// Initializes the default store and installs the `real-setup` fixture:
    /// the Codex and Claude Code user files, the repository files, the
    /// local definitions and the team source (not registered).
    pub fn install_real_setup(&self) -> RealSetup {
        self.init();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real-setup");
        let repo = self.work();
        let cwd = repo.join("sub");
        let team = self.root().join("team");
        let (repo_text, cwd_text) = (repo.display().to_string(), cwd.display().to_string());
        let substitute = move |text: String| {
            text.replace("@REPO@", &repo_text)
                .replace("@CWD@", &cwd_text)
        };
        copy_tree(&fixture.join("repo"), &repo, &substitute);
        fs::create_dir_all(repo.join(".git")).expect("create .git");
        copy_tree(&fixture.join("codex"), &self.codex_home(), &substitute);
        copy_tree(
            &fixture.join("claude"),
            self.claude_config().parent().expect("claude dir"),
            &substitute,
        );
        copy_tree(
            &fixture.join("local"),
            &self.default_store().join("local"),
            &substitute,
        );
        copy_tree(&fixture.join("team"), &team, &substitute);
        RealSetup { repo, cwd, team }
    }

    /// A `d-mcp` command like [`TestEnv::cmd`], run from `dir`.
    pub fn cmd_in(&self, dir: &Path) -> Command {
        let mut cmd = self.cmd();
        cmd.current_dir(dir);
        cmd
    }

    /// Runs `d-mcp --json <args>` from `dir`; returns exit code, parsed
    /// stdout and stderr.
    pub fn json_in(&self, dir: &Path, args: &[&str]) -> (i32, Value, String) {
        let output = self
            .cmd_in(dir)
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

    /// Runs `d-mcp <args>` in text mode from `dir`.
    pub fn text_in(&self, dir: &Path, args: &[&str]) -> (i32, String, String) {
        let output = self.cmd_in(dir).args(args).output().expect("run d-mcp");
        (
            output.status.code().expect("exit code"),
            String::from_utf8(output.stdout).expect("utf-8 stdout"),
            String::from_utf8(output.stderr).expect("utf-8 stderr"),
        )
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

/// The `git` binary of the machine running the tests, resolved from the test
/// process's `PATH` and canonicalized.
pub fn system_git() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .and_then(|git| git.canonicalize().ok())
        .expect("the git tests need `git` on PATH")
}

/// A local bare repository and a work tree that pushes to it, built with
/// the system `git` under an isolated configuration
/// (`<root>/builder-gitconfig`, no system configuration, `HOME` in the
/// temporary root).
pub struct Remote {
    /// The bare repository; its path is the URL d-mcp clones.
    pub bare: PathBuf,
    /// The work tree.
    pub work: PathBuf,
    root: PathBuf,
}

impl Remote {
    fn new(root: &Path, name: &str) -> Self {
        let remotes = root.join("remotes");
        fs::create_dir_all(&remotes).expect("create remotes dir");
        let config = root.join("builder-gitconfig");
        if !config.exists() {
            fs::write(
                &config,
                "[user]\n\tname = d-mcp test\n\temail = test@example.invalid\n\
                 [init]\n\tdefaultBranch = main\n[commit]\n\tgpgsign = false\n\
                 [tag]\n\tgpgsign = false\n",
            )
            .expect("write builder git config");
            fs::create_dir_all(root.join("builder-home")).expect("create builder home");
        }
        let remote = Self {
            bare: remotes.join(format!("{name}.git")),
            work: remotes.join(format!("{name}-work")),
            root: root.to_path_buf(),
        };
        remote.git_in(
            &remotes,
            &["init", "-q", "--bare", "-b", "main", &format!("{name}.git")],
        );
        remote.git_in(
            &remotes,
            &["init", "-q", "-b", "main", &format!("{name}-work")],
        );
        remote.git(&[
            "remote",
            "add",
            "origin",
            remote.bare.to_str().expect("utf-8"),
        ]);
        remote
    }

    /// The URL to clone: the bare repository's absolute path.
    pub fn url(&self) -> String {
        self.bare.to_str().expect("utf-8 path").to_owned()
    }

    /// The isolated `git` command of the builders, run in `dir`.
    pub fn command(&self, dir: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(system_git());
        command
            .env_clear()
            .env("HOME", self.root.join("builder-home"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("builder-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "2026-10-31T12:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-10-31T12:00:00Z")
            .current_dir(dir);
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmp);
        }
        command
    }

    /// Runs `git <args>` in `dir`, asserts success and returns stdout.
    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.command(dir).args(args).output().expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 git output")
    }

    /// Runs `git <args>` in the work tree.
    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.work, args)
    }

    /// Writes a file in the work tree, creating its directories.
    pub fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let target = self.work.join(path);
        fs::create_dir_all(target.parent().expect("parent")).expect("create dirs");
        fs::write(target, bytes).expect("write work-tree file");
    }

    /// Removes a file from the work tree.
    pub fn remove(&self, path: &str) {
        fs::remove_file(self.work.join(path)).expect("remove work-tree file");
    }

    /// Stages every change, commits and returns the new commit id (without
    /// pushing).
    pub fn commit_local(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.commit_staged(message)
    }

    /// Commits what is staged and returns the new commit id.
    pub fn commit_staged(&self, message: &str) -> String {
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.head()
    }

    /// Stages every change, commits, pushes the branch and returns the
    /// commit id.
    pub fn commit(&self, message: &str) -> String {
        let commit = self.commit_local(message);
        self.push();
        commit
    }

    /// Pushes the current branch to `origin`.
    pub fn push(&self) {
        let branch = self.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        self.git(&[
            "push",
            "-q",
            "--force",
            "origin",
            &format!("HEAD:refs/heads/{}", branch.trim()),
        ]);
    }

    /// The commit id of `HEAD` in the work tree.
    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// Runs `git <args>` on the bare repository.
    pub fn bare_git(&self, args: &[&str]) -> String {
        self.git_in(&self.bare, args)
    }
}
