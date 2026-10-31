//! Unit tests of the guarded git runner. The tests that run git use the
//! system `git` with an isolated configuration (`GIT_CONFIG_GLOBAL` pointing
//! to an empty file, `GIT_CONFIG_NOSYSTEM=1`, `HOME` in a temporary
//! directory) and local repositories only; they are skipped when no `git`
//! is on `PATH`.

use super::*;

#[test]
fn ls_tree_output_is_parsed_strictly() {
    let oid = "a".repeat(40);
    let text = format!(
        "100644 blob {oid}     120\tgithub.toml\0120000 blob {oid}      11\tlink.toml\0160000 commit {oid}       -\tsub\0040000 tree {oid}       -\tdir\0"
    );
    let entries = parse_ls_tree(text.as_bytes()).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].name, b"github.toml");
    assert_eq!(entries[0].size, Some(120));
    assert_eq!(entries[1].mode, "120000");
    assert_eq!(entries[1].size, Some(11));
    assert_eq!(entries[2].kind, "commit");
    assert_eq!(entries[2].size, None);
    assert_eq!(entries[3].size, None);
    assert_eq!(parse_ls_tree(b"").unwrap(), Vec::new());
    assert!(parse_ls_tree(b"100644 blob xyz 1\tname\0").is_none());
    assert!(parse_ls_tree(format!("100644 blob {oid} 1name\0").as_bytes()).is_none());
    assert!(parse_ls_tree(format!("100644 blob {oid} 1\t\0").as_bytes()).is_none());
    // Without a size, with a blob size of `-`, or with a size on a tree.
    assert!(parse_ls_tree(format!("100644 blob {oid}\tname\0").as_bytes()).is_none());
    assert!(parse_ls_tree(format!("100644 blob {oid} -\tname\0").as_bytes()).is_none());
    assert!(parse_ls_tree(format!("040000 tree {oid} 5\tname\0").as_bytes()).is_none());
    assert!(parse_ls_tree(format!("100644 blob {oid} 1 2\tname\0").as_bytes()).is_none());
}

#[test]
fn failures_map_to_stable_codes() {
    let output = |class| Output {
        exit: Exit::Code(128),
        stdout: Vec::new(),
        truncated: false,
        stderr_class: class,
    };
    let code = |class| failure("clone", &output(class)).code();
    assert_eq!(code(Some(StderrClass::AuthFailed)), "git-auth-failed");
    assert_eq!(code(Some(StderrClass::HostUnknown)), "git-host-unknown");
    assert_eq!(code(Some(StderrClass::Unreachable)), "git-unreachable");
    assert_eq!(code(Some(StderrClass::RefNotFound)), "git-ref-not-found");
    assert_eq!(code(Some(StderrClass::Failed)), "git-failed");
    let error = failure("ls-tree", &output(None));
    assert_eq!(
        error.hint(),
        "`git ls-tree` failed (exit status 128); its output is not shown"
    );
    let signal = Output {
        exit: Exit::Signal(9),
        ..output(None)
    };
    assert!(failure("x", &signal).hint().contains("signal 9"));
    let unknown = Output {
        exit: Exit::Unknown,
        ..output(None)
    };
    assert!(failure("x", &unknown).hint().contains("unknown status"));
    assert!(checked_oid("HEAD", "cat-file").is_err());
}

#[test]
fn locate_needs_git_on_the_given_path() {
    let env = Environment {
        path: Some(OsString::from("/nonexistent-d-mcp-bin")),
        ..Environment::default()
    };
    assert!(matches!(Git::locate(&env), Err(Error::GitNotFound)));
    assert!(matches!(
        Git::locate(&Environment::default()),
        Err(Error::GitNotFound)
    ));
}

/// The system git with an isolated configuration, or `None` without git.
fn isolated_git(temp: &Path) -> Option<Git> {
    let env = Environment {
        path: std::env::var_os("PATH"),
        ..Environment::default()
    };
    let mut git = Git::locate(&env).ok()?;
    let config = temp.join("gitconfig");
    std::fs::write(&config, "").unwrap();
    std::fs::create_dir_all(temp.join("home")).unwrap();
    // The same isolation as `tests/common`: nothing of the developer's or
    // the CI runner's git configuration or SSH command reaches the calls.
    git.extra_env = vec![
        ("GIT_CONFIG_GLOBAL".into(), Some(config.into_os_string())),
        ("GIT_CONFIG_NOSYSTEM".into(), Some("1".into())),
        ("HOME".into(), Some(temp.join("home").into_os_string())),
        ("GIT_CONFIG_COUNT".into(), None),
        ("GIT_CONFIG_PARAMETERS".into(), None),
        ("GIT_SSH_COMMAND".into(), None),
        ("GIT_SSH".into(), None),
    ];
    Some(git)
}

/// Runs a builder git command with the same isolation, asserting success.
fn build(git: &Git, dir: &Path, args: &[&str]) -> String {
    let mut command = std::process::Command::new(&git.program);
    command.env_clear().current_dir(dir);
    for (name, value) in &git.extra_env {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    let output = command
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// A bare repository with two commits on `main` (the second adds
/// `servers/a.toml` and a 100-byte `big.toml`) and a tag `v1` on the first.
fn origin(git: &Git, temp: &Path) -> (PathBuf, CommitId, CommitId) {
    let work = temp.join("work");
    std::fs::create_dir_all(work.join("servers")).unwrap();
    build(git, temp, &["init", "-q", "-b", "main", "work"]);
    std::fs::write(work.join("README"), "x").unwrap();
    build(git, &work, &["add", "-A"]);
    build(git, &work, &["commit", "-q", "-m", "one"]);
    build(git, &work, &["tag", "v1"]);
    let first = CommitId::parse(build(git, &work, &["rev-parse", "HEAD"]).trim()).unwrap();
    std::fs::write(work.join("servers/a.toml"), "name = \"a\"\n").unwrap();
    std::fs::write(work.join("servers/big.toml"), "#".repeat(100)).unwrap();
    build(git, &work, &["add", "-A"]);
    build(git, &work, &["commit", "-q", "-m", "two"]);
    let second = CommitId::parse(build(git, &work, &["rev-parse", "HEAD"]).trim()).unwrap();
    build(git, temp, &["clone", "-q", "--bare", "work", "origin.git"]);
    (temp.join("origin.git"), first, second)
}

#[test]
fn guarded_operations_on_a_local_repository() {
    let temp = tempfile::tempdir().unwrap();
    let temp = temp.path().canonicalize().unwrap();
    let Some(git) = isolated_git(&temp) else {
        return;
    };
    let (origin, first, second) = origin(&git, &temp);
    let url = validate_url(origin.to_str().unwrap()).unwrap();
    let staging = temp.join("staging");
    std::fs::create_dir(&staging).unwrap();

    git.check_ref_format(&staging, "main").unwrap();
    assert!(matches!(
        git.check_ref_format(&staging, "a..b"),
        Err(Error::SourceRefInvalid { what: "ref" })
    ));
    assert!(matches!(
        git.check_ref_format(&staging, "-x"),
        Err(Error::SourceRefInvalid { .. })
    ));

    let repo = staging.join("repo");
    git.clone_repo(&staging, &url, None, &repo).unwrap();
    let git_dir = repo.join(".git");
    assert_eq!(git.head_branch(&git_dir).unwrap().as_deref(), Some("main"));
    assert_eq!(
        git.rev_parse_commit(&git_dir, "HEAD").unwrap(),
        Some(second.clone())
    );
    assert_eq!(
        git.rev_parse_commit(&git_dir, &"0".repeat(40)).unwrap(),
        None
    );
    assert!(git.is_ancestor(&git_dir, &first, &second).unwrap());
    assert!(!git.is_ancestor(&git_dir, &second, &first).unwrap());
    git.update_pin(&git_dir, &first).unwrap();
    assert_eq!(
        git.rev_parse_commit(&git_dir, PIN_REF).unwrap(),
        Some(first.clone())
    );

    let root = git.ls_tree(&git_dir, &second, "").unwrap();
    assert!(
        root.iter()
            .any(|entry| entry.name == b"servers" && entry.kind == "tree")
    );
    let servers = git.ls_tree(&git_dir, &second, "servers").unwrap();
    let big = servers
        .iter()
        .find(|entry| entry.name == b"big.toml")
        .unwrap();
    assert_eq!(big.size, Some(100));
    assert_eq!(git.read_blob(&git_dir, &big.oid, 99).unwrap(), None);
    assert_eq!(
        git.read_blob(&git_dir, &big.oid, 100)
            .unwrap()
            .unwrap()
            .len(),
        100
    );
    assert!(git.ls_tree(&git_dir, &first, "servers").is_err());
    assert!(git.read_blob(&git_dir, "HEAD", 10).is_err());

    let refs = git.ls_remote(&staging, &url, "refs/tags/v1").unwrap();
    assert_eq!(refs, [(first.to_string(), "refs/tags/v1".to_owned())]);
    git.fetch(
        &staging,
        &git_dir,
        &url,
        &["+refs/heads/main:refs/remotes/origin/main".to_owned()],
    )
    .unwrap();

    // A tag clone has a detached HEAD.
    let tagged = staging.join("tagged");
    git.clone_repo(&staging, &url, Some("v1"), &tagged).unwrap();
    assert_eq!(git.head_branch(&tagged.join(".git")).unwrap(), None);
    assert!(matches!(
        git.clone_repo(&staging, &url, Some("nope"), &staging.join("nope")),
        Err(Error::GitRefNotFound { operation: "clone" })
    ));
    let missing = validate_url(temp.join("missing.git").to_str().unwrap()).unwrap();
    assert!(matches!(
        git.clone_repo(&staging, &missing, None, &staging.join("missing")),
        Err(Error::GitUnreachable { operation: "clone" })
    ));
    // Without a user-configured SSH command, the batch-mode one is used.
    assert!(!git.ssh_command_configured(&staging).unwrap());
    let mut with_env = git.clone();
    with_env.extra_env.retain(|(name, _)| name != "GIT_SSH");
    with_env
        .extra_env
        .push(("GIT_SSH".into(), Some("/usr/bin/ssh".into())));
    assert!(with_env.ssh_command_configured(&staging).unwrap());
    let config = temp.join("gitconfig");
    std::fs::write(&config, "[core]\n\tsshCommand = ssh -i /dev/null\n").unwrap();
    assert!(git.ssh_command_configured(&staging).unwrap());
}

#[test]
fn git_itself_refuses_the_ext_transport() {
    let temp = tempfile::tempdir().unwrap();
    let temp = temp.path().canonicalize().unwrap();
    let Some(git) = isolated_git(&temp) else {
        return;
    };
    let marker = temp.join("pwned");
    // Even if URL validation were bypassed, `GIT_ALLOW_PROTOCOL` blocks it.
    let url = GitUrl::unchecked(&format!("ext::sh -c touch% {}", marker.display()));
    let result = git.clone_repo(&temp, &url, None, &temp.join("clone"));
    assert!(result.is_err());
    assert!(!marker.exists());
}
