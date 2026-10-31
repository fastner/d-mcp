use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::definition::parse_definition;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

fn rules(command: &str, list: &[&str]) -> Vec<&'static str> {
    audit_command(command, &args(list))
        .iter()
        .map(|finding| finding.rule.as_str())
        .collect()
}

const SHA40: &str = "4f2a9c0d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39";

#[test]
fn runner_table() {
    let npm = "unpinned-npm-package";
    let python = "unpinned-python-package";
    let vcs = "unpinned-vcs-spec";
    let image = "unpinned-container-image";
    let opaque = "opaque-command";
    let commit = format!("github:org/repo#{SHA40}");
    let at_commit = format!("git+https://h/org/repo@{SHA40}");
    let cases: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        // npm family: exact SemVer only.
        ("npx", vec!["-y", "pkg@1.2.3"], vec![]),
        ("npx", vec!["--yes", "pkg@1.2.3-rc.1+b.2"], vec![]),
        ("npx", vec!["pkg"], vec![npm]),
        ("npx", vec!["pkg@latest"], vec![npm]),
        ("npx", vec!["pkg@next"], vec![npm]),
        ("npx", vec!["pkg@^1.2.3"], vec![npm]),
        ("npx", vec!["pkg@1"], vec![npm]),
        ("npx", vec!["pkg@1.2"], vec![npm]),
        ("npx", vec!["pkg@>=1.0.0"], vec![npm]),
        ("npx", vec!["pkg@1.2.3-"], vec![npm]),
        ("npx", vec!["pkg@1.2.3+"], vec![npm]),
        ("npx", vec!["@scope/pkg@1.2.3"], vec![]),
        ("npx", vec!["@scope/pkg"], vec![npm]),
        ("npx", vec!["@scope@1.2.3"], vec![npm]),
        ("npx", vec!["@1.2.3"], vec![npm]),
        ("npx", vec!["npm:pkg@1.2.3"], vec![]),
        ("npx", vec!["-p", "pkg@1.2.3", "bin"], vec![]),
        ("npx", vec!["-p", "pkg", "bin"], vec![npm]),
        (
            "npx",
            vec!["--package=pkg@1.2.3", "--package", "b", "bin"],
            vec![npm],
        ),
        (
            "npx",
            vec!["--registry", "https://r.test", "pkg@1.2.3"],
            vec![],
        ),
        ("npx", vec!["--", "pkg@1.2.3", "--flag"], vec![]),
        ("npx", vec!["-y"], vec![npm]),
        ("npx", vec!["-c", "pkg"], vec![opaque]),
        ("npx", vec!["--call=pkg run", "-p", "a@1.0.0"], vec![opaque]),
        ("/usr/local/bin/npx", vec!["pkg"], vec![npm]),
        ("C:\\node\\npx.cmd", vec!["pkg"], vec![npm]),
        ("npm", vec!["exec", "pkg"], vec![npm]),
        ("npm", vec!["--yes", "exec", "pkg@1.0.0"], vec![]),
        ("npm", vec!["--prefix", "/x", "x", "pkg"], vec![npm]),
        ("npm", vec!["start"], vec![]),
        ("npm", vec![], vec![]),
        ("pnpm", vec!["dlx", "pkg"], vec![npm]),
        ("pnpm", vec!["-C", "/w", "dlx", "pkg@1.0.0"], vec![]),
        ("pnpm", vec!["dlx", "--shell-mode", "pkg"], vec![opaque]),
        ("npx", vec!["-c", "x", "pkg"], vec![opaque, npm]),
        ("pnpx", vec!["pkg"], vec![npm]),
        ("bunx", vec!["pkg"], vec![npm]),
        ("bun", vec!["x", "pkg"], vec![npm]),
        ("bun", vec!["run", "server.ts"], vec![]),
        ("yarn", vec!["dlx", "pkg"], vec![npm]),
        // VCS and URL specs need a commit in a digest context.
        ("npx", vec![commit.as_str()], vec![]),
        ("npx", vec!["github:org/repo"], vec![vcs]),
        ("npx", vec!["github:org/repo#main"], vec![vcs]),
        ("npx", vec!["org/repo"], vec![vcs]),
        ("npx", vec!["https://r.test/p-1.2.3.tgz"], vec![vcs]),
        (
            "npx",
            vec!["git+ssh://git@h/org/repo.git#abc123"],
            vec![vcs],
        ),
        ("uvx", vec!["--from", at_commit.as_str(), "bin"], vec![]),
        (
            "uvx",
            vec!["--from", "git+https://h/org/repo", "bin"],
            vec![vcs],
        ),
        (
            "uvx",
            vec!["--from", "name@git+https://h/r", "bin"],
            vec![vcs],
        ),
        // Local paths are not audited.
        ("npx", vec!["./pkg"], vec![]),
        ("npx", vec!["file:../pkg"], vec![]),
        ("uvx", vec!["--from", "/opt/pkg", "bin"], vec![]),
        // Python family: exact PEP 440 versions.
        ("uvx", vec!["pkg==1.2.3"], vec![]),
        ("uvx", vec!["pkg===1.2.3"], vec![]),
        ("uvx", vec!["pkg[extra]==1!2.0.post1"], vec![]),
        ("uvx", vec!["pkg==1.0; python_version>'3'"], vec![]),
        ("uvx", vec!["pkg@1.2.3"], vec![]),
        ("uvx", vec!["pkg"], vec![python]),
        ("uvx", vec!["pkg@latest"], vec![python]),
        ("uvx", vec!["pkg==1.*"], vec![python]),
        ("uvx", vec!["pkg>=1.0"], vec![python]),
        ("uvx", vec!["pkg~=1.0"], vec![python]),
        ("uvx", vec!["--python", "3.12", "pkg==1.0"], vec![]),
        ("uvx", vec!["--with", "extra", "pkg==1.0"], vec![python]),
        ("uvx", vec!["--with=extra==2.0", "pkg==1.0"], vec![]),
        ("uv", vec!["tool", "run", "pkg"], vec![python]),
        (
            "uv",
            vec!["--directory", "/w", "tool", "run", "pkg==1.0"],
            vec![],
        ),
        ("uv", vec!["tool", "install", "pkg"], vec![]),
        ("uv", vec!["run", "server.py"], vec![]),
        ("pipx", vec!["run", "pkg"], vec![python]),
        ("pipx", vec!["run", "--spec", "pkg==1.0", "bin"], vec![]),
        ("pipx", vec!["install", "pkg"], vec![]),
        // Containers: a digest or a tag other than `latest`.
        ("docker", vec!["run", "-i", "--rm", "img:1.2"], vec![]),
        ("docker", vec!["run", "img"], vec![image]),
        ("docker", vec!["run", "img:latest"], vec![image]),
        ("docker", vec!["run", "img:"], vec![image]),
        ("docker", vec!["run", "reg:5000/img"], vec![image]),
        ("docker", vec!["run", "reg:5000/img:2"], vec![]),
        (
            "docker",
            vec!["run", "-e", "A=1", "--name", "x", "-v", "/a:/b", "img:1"],
            vec![],
        ),
        ("docker", vec!["run", "--env=A", "img"], vec![image]),
        (
            "docker",
            vec!["--context", "remote", "run", "img:1"],
            vec![],
        ),
        ("docker", vec!["container", "run", "img"], vec![image]),
        ("docker", vec!["ps"], vec![]),
        ("docker", vec!["run"], vec![image]),
        ("podman", vec!["run", "img"], vec![image]),
        ("nerdctl", vec!["run", "img:latest"], vec![image]),
        // Wrappers.
        ("sh", vec!["-c", "npx pkg@1.0.0"], vec![opaque]),
        ("bash", vec!["-lc", "x"], vec![opaque]),
        ("zsh", vec!["-c"], vec![opaque]),
        ("bash", vec!["script.sh"], vec![]),
        ("bash", vec!["--login", "script.sh", "-c"], vec![]),
        ("bash", vec!["--", "-c"], vec![]),
        ("env", vec!["A=1", "npx", "pkg"], vec![npm]),
        (
            "env",
            vec!["-i", "-u", "X", "--", "npx", "pkg@1.0.0"],
            vec![],
        ),
        ("env", vec!["A=1", "sh", "-c", "x"], vec![opaque]),
        ("env", vec!["env", "npx", "pkg@1.0.0"], vec![opaque]),
        ("env", vec!["-S", "npx pkg"], vec![opaque]),
        ("env", vec!["-iS", "npx pkg"], vec![opaque]),
        ("env", vec!["--split-string=npx pkg"], vec![opaque]),
        ("env", vec!["A=1"], vec![]),
        ("env", vec!["node", "server.js"], vec![]),
        // Not audited: arbitrary binaries.
        ("node", vec!["server.js"], vec![]),
        ("/opt/bin/mcp-server", vec!["--stdio"], vec![]),
    ];
    for (command, list, expected) in cases {
        let mut expected: Vec<&str> = expected;
        expected.sort_unstable();
        let mut found = rules(command, &list);
        found.sort_unstable();
        assert_eq!(found, expected, "{command} {list:?}");
    }
}

#[test]
fn findings_name_the_launcher_and_the_matched_arguments() {
    let findings = audit_command("npx", &args(&["-y", "pkg@latest", "--flag"]));
    assert_eq!(
        findings,
        [Finding {
            rule: AuditRule::UnpinnedNpmPackage,
            launcher: "npx".to_owned(),
            args: args(&["pkg@latest"]),
        }]
    );
    let findings = audit_command("docker", &args(&["container", "run", "img"]));
    assert_eq!(findings[0].launcher, "docker container run");
    assert_eq!(findings[0].args, ["img"]);
    let findings = audit_command("bash", &args(&["-lc", "run it"]));
    assert_eq!(findings[0].launcher, "bash -c");
    assert_eq!(findings[0].args, ["-lc", "run it"]);
    let findings = audit_command("npx", &args(&[]));
    assert!(findings[0].args.is_empty());
    let findings = audit_command("env", &args(&["-S", "npx x"]));
    assert_eq!(findings[0].launcher, "env");
    assert_eq!(findings[0].args, ["-S"]);
    let json = serde_json::to_value(&findings[0]).unwrap();
    assert_eq!(json["rule"], "opaque-command");
}

#[test]
fn findings_hash_covers_rules_and_arguments_only() {
    let a = audit_command("npx", &args(&["pkg"]));
    let b = audit_command("npx", &args(&["-y", "pkg", "--other"]));
    let c = audit_command("npx", &args(&["pkg@latest"]));
    assert_eq!(findings_hash(&a), findings_hash(&b));
    assert_ne!(findings_hash(&a), findings_hash(&c));
    // The launcher label is not part of the hash; the order is irrelevant.
    let mut relabeled = a.clone();
    relabeled[0].launcher = "pnpx".to_owned();
    assert_eq!(findings_hash(&a), findings_hash(&relabeled));
    let mut both = [a.clone(), c].concat();
    let forward = findings_hash(&both);
    both.reverse();
    assert_eq!(forward, findings_hash(&both));
    assert!(crate::canonical::is_digest(&findings_hash(&[])));
    assert_ne!(findings_hash(&[]), findings_hash(&a));
}

#[test]
fn http_definitions_are_not_audited() {
    let http = parse_definition(
        "name = \"h\"\n[transport]\ntype = \"http\"\nurl = \"https://mcp.example.test/mcp\"\n",
        "h",
    )
    .unwrap();
    assert!(audit(&http).is_empty());
}

fn fixtures(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|_| panic!("fixture dir {}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
}

fn audit_fixture(path: &Path) -> Vec<Finding> {
    let stem = path.file_stem().unwrap().to_str().unwrap();
    let text = fs::read_to_string(path).unwrap();
    let definition = parse_definition(&text, stem)
        .unwrap_or_else(|error| panic!("{} does not parse: {error:?}", path.display()));
    audit(&definition)
}

/// Every pinned fixture passes; every unpinned fixture in
/// `unpinned/<rule>/` has a finding of exactly that rule; every rule has
/// fixtures.
#[test]
fn launcher_fixtures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/launchers");
    let pinned = fixtures(&root.join("pinned"));
    assert!(pinned.len() >= 15);
    for path in pinned {
        assert_eq!(audit_fixture(&path), [], "{}", path.display());
    }
    let mut seen = Vec::new();
    for dir in fixtures(&root.join("unpinned")) {
        let rule = dir.file_name().unwrap().to_str().unwrap().to_owned();
        let files = fixtures(&dir);
        assert!(!files.is_empty(), "{rule}");
        for path in files {
            let found: Vec<&str> = audit_fixture(&path)
                .iter()
                .map(|finding| finding.rule.as_str())
                .collect();
            assert_eq!(found, [rule.as_str()], "{}", path.display());
        }
        seen.push(rule);
    }
    let mut expected: Vec<String> = AuditRule::ALL
        .iter()
        .map(|rule| rule.as_str().to_owned())
        .collect();
    expected.sort();
    assert_eq!(seen, expected);
}

/// Every runner of the table has a pinned and an unpinned fixture.
#[test]
fn every_runner_has_fixtures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/launchers");
    let launchers = |dir: &Path, pinned: bool| -> Vec<String> {
        let paths = if pinned {
            fixtures(dir)
        } else {
            fixtures(dir).iter().flat_map(|d| fixtures(d)).collect()
        };
        paths
            .iter()
            .map(|path| {
                let stem = path.file_stem().unwrap().to_str().unwrap();
                let text = fs::read_to_string(path).unwrap();
                let definition = parse_definition(&text, stem).unwrap();
                let Transport::Stdio { command, args, .. } = definition.transport else {
                    panic!("stdio fixture");
                };
                let base = basename(&command).to_owned();
                RUNNERS
                    .iter()
                    .find(|runner| {
                        runner.program == base
                            && args
                                .iter()
                                .zip(runner.subcommand)
                                .all(|(arg, word)| arg == word)
                            && args.len() >= runner.subcommand.len()
                    })
                    .map(|runner| {
                        std::iter::once(runner.program)
                            .chain(runner.subcommand.iter().copied())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or(base)
            })
            .collect()
    };
    let pinned = launchers(&root.join("pinned"), true);
    let unpinned = launchers(&root.join("unpinned"), false);
    for runner in RUNNERS {
        let label = std::iter::once(runner.program)
            .chain(runner.subcommand.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(pinned.contains(&label), "no pinned fixture for {label}");
        assert!(unpinned.contains(&label), "no unpinned fixture for {label}");
    }
}

/// Guard: only the neutral definition is audited, so no adapter override
/// allow-list may gain a key that runs a command or changes what runs.
#[test]
fn override_allow_lists_hold_no_command_running_key() {
    const FORBIDDEN: &[&str] = &[
        "command",
        "args",
        "env",
        "env_vars",
        "headersHelper",
        "headers_helper",
        "http_headers_helper",
        "experimental_environment",
    ];
    for adapter in crate::adapter::registry() {
        for key in adapter.override_keys() {
            let lower = key.key.to_ascii_lowercase();
            assert!(
                !FORBIDDEN.contains(&key.key)
                    && !lower.contains("command")
                    && !lower.contains("helper")
                    && !lower.contains("args"),
                "{} override allow-list gained the command-running key `{}`",
                adapter.harness().as_str(),
                key.key
            );
        }
    }
}

const SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Options the tables do not know, and short clusters with a value
/// letter, leave the spec unknown: `unparsed-launcher`, never a guess.
#[test]
fn unknown_options_and_value_clusters_are_unparsed() {
    let unparsed = "unparsed-launcher";
    let npm = "unpinned-npm-package";
    let python = "unpinned-python-package";
    let image = "unpinned-container-image";
    let cases: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        // The bypasses: a value letter in a cluster took the real image.
        (
            "docker",
            vec!["run", "-dp", "127.0.0.1:3000:3000", "evil"],
            vec![unparsed],
        ),
        (
            "docker",
            vec!["run", "-ip", "127.0.0.1:3000:3000", "evil"],
            vec![unparsed],
        ),
        (
            "docker",
            vec!["run", "-p3000:3000", "img:1"],
            vec![unparsed],
        ),
        (
            "docker",
            vec!["run", "--frobnicate", "x:1", "evil"],
            vec![unparsed],
        ),
        (
            "docker",
            vec!["run", "--frobnicate=x", "img:1"],
            vec![unparsed],
        ),
        (
            "npx",
            vec!["--frobnicate", "x@1.0.0", "evil"],
            vec![unparsed],
        ),
        ("npx", vec!["-yc", "x"], vec![unparsed]),
        ("uvx", vec!["-qp", "3.12", "evil"], vec![unparsed]),
        ("pipx", vec!["run", "--frobnicate", "evil"], vec![unparsed]),
        // An unknown global option may have taken a word before `exec`.
        (
            "npm",
            vec!["--frobnicate", "x", "exec", "evil"],
            vec![unparsed],
        ),
        (
            "docker",
            vec!["--frobnicate", "x", "run", "evil"],
            vec![unparsed],
        ),
        // Without the subcommand later, it is not this runner at all.
        ("npm", vec!["--frobnicate", "y", "start"], vec![]),
        // ... but `x` is `npm x`: the unknown option is taken as a flag.
        ("npm", vec!["--frobnicate", "x", "start"], vec![npm]),
        ("docker", vec!["compose", "run", "web"], vec![]),
        // Known options parse: the real spec is checked.
        (
            "npm",
            vec!["--loglevel", "silent", "exec", "evil"],
            vec![npm],
        ),
        ("npm", vec!["--debug", "exec", "pkg@1.0.0"], vec![]),
        ("docker", vec!["run", "-it", "--rm", "img:1"], vec![]),
        (
            "docker",
            vec!["run", "-d", "-p", "80:80", "evil"],
            vec![image],
        ),
        ("docker", vec!["run", "--rm=false", "img:1"], vec![]),
        (
            "docker",
            vec!["run", "--blkio-weight-device", "/dev/sda:200", "evil"],
            vec![image],
        ),
        (
            "docker",
            vec!["run", "--device-write-iops", "/dev/sda:10", "evil"],
            vec![image],
        ),
        (
            "docker",
            vec!["run", "--detach-keys", "ctrl-x", "evil"],
            vec![image],
        ),
        (
            "docker",
            vec!["run", "--oom-score-adj", "5", "evil"],
            vec![image],
        ),
        (
            "podman",
            vec!["run", "--uidmap", "0:1:1", "evil"],
            vec![image],
        ),
        (
            "podman",
            vec!["run", "--gidmap", "0:1:1", "evil"],
            vec![image],
        ),
        ("podman", vec!["run", "--pod", "p", "evil"], vec![image]),
        ("podman", vec!["run", "--secret", "s", "evil"], vec![image]),
        (
            "podman",
            vec!["run", "--arch", "arm64", "evil"],
            vec![image],
        ),
        ("podman", vec!["run", "--creds", "u", "evil"], vec![image]),
        (
            "podman",
            vec!["run", "--authfile", "/a", "evil"],
            vec![image],
        ),
        ("podman", vec!["container", "run", "evil"], vec![image]),
        ("nerdctl", vec!["container", "run", "img:1"], vec![]),
        ("npx", vec!["--tag", "x@1.0.0", "evil"], vec![npm]),
        ("npx", vec!["--before", "2024-01-01", "evil"], vec![npm]),
        ("npx", vec!["--globalconfig", "/g", "pkg@1.0.0"], vec![]),
        ("npx", vec!["-yq", "pkg@1.0.0"], vec![]),
        (
            "uvx",
            vec!["--no-build-package", "x==1.0", "evil"],
            vec![python],
        ),
        (
            "uvx",
            vec!["--no-binary-package", "x", "evil"],
            vec![python],
        ),
        (
            "uvx",
            vec!["--exclude-newer-package", "x=2024-01-01", "evil"],
            vec![python],
        ),
        (
            "uvx",
            vec!["--fork-strategy", "fewest", "evil"],
            vec![python],
        ),
        ("uvx", vec!["--torch-backend", "cpu", "evil"], vec![python]),
        ("uvx", vec!["-qq", "--isolated", "pkg==1.0"], vec![]),
    ];
    for (command, list, expected) in cases {
        assert_eq!(rules(command, &list), expected, "{command} {list:?}");
    }
    let findings = audit_command("docker", &args(&["run", "-dp", "80:80", "evil"]));
    assert_eq!(findings[0].launcher, "docker run");
    assert_eq!(findings[0].args, ["-dp"]);
    let findings = audit_command("npm", &args(&["--frobnicate", "x", "exec", "evil"]));
    assert_eq!(findings[0].launcher, "npm exec");
    assert_eq!(findings[0].args, ["--frobnicate"]);
}

/// Only a Git transport's commit in ref position pins; a hex run anywhere
/// else (a tarball fragment, a path segment, the host) does not.
#[test]
fn commits_pin_only_git_transports_in_ref_position() {
    let vcs = "unpinned-vcs-spec";
    let pinned = |spec: &str| (spec.to_owned(), Vec::<&str>::new());
    let unpinned = |spec: &str| (spec.to_owned(), vec![vcs]);
    let npm_cases = [
        pinned(&format!("github:org/repo#{SHA40}")),
        pinned(&format!("gitlab:org/repo#{SHA40}")),
        pinned(&format!("git+https://h/org/repo.git#{SHA40}")),
        pinned(&format!("git+ssh://git@h/org/repo.git#{SHA40}")),
        pinned(&format!("git://h/org/repo.git#{SHA40}")),
        pinned(&format!("org/repo#{SHA40}")),
        pinned(&format!("org/repo#{SHA256}")),
        unpinned(&format!("https://r.test/pkg.tgz#{SHA40}")),
        unpinned(&format!("https://r.test/pkg.tgz@{SHA40}")),
        unpinned(&format!("https://r.test/{SHA40}/pkg.tgz")),
        unpinned(&format!("git+https://h/{SHA40}/repo.git")),
        unpinned(&format!("github:org/repo#{SHA40}x")),
        unpinned(&format!("github:org/repo#{SHA40}#main")),
        unpinned("github:org/repo#semver:^1.0.0"),
        unpinned(&format!("ssh://h/org/repo.git#{SHA40}")),
    ];
    for (spec, expected) in npm_cases {
        assert_eq!(rules("npx", &[spec.as_str()]), expected, "npx {spec}");
    }
    let python_cases = [
        pinned(&format!("git+https://h/org/repo@{SHA40}")),
        pinned(&format!("git+https://h/org/repo@{SHA40}#subdirectory=x")),
        pinned(&format!("git+ssh://git@h/org/repo@{SHA40}")),
        pinned(&format!("name @ git+https://h/org/repo@{SHA40}")),
        pinned(&format!("name@git+https://h/org/repo@{SHA40}")),
        pinned(&format!(
            "name @ git+https://h/r@{SHA40} ; python_version>'3'"
        )),
        unpinned(&format!(
            "https://h/pkg-1.0-py3-none-any.whl#sha256={SHA256}"
        )),
        unpinned(&format!(
            "name @ https://h/pkg.tar.gz#egg=x&sha256={SHA256}"
        )),
        unpinned(&format!("https://h/pkg-1.0.tar.gz#{SHA40}")),
        unpinned(&format!("https://h/pkg-1.0.tar.gz@{SHA40}")),
        unpinned(&format!("https://h/pkg.whl#sha256={SHA40}")),
        unpinned(&format!("git+https://h/{SHA40}/repo")),
        unpinned(&format!("git+https://git@{SHA40}/repo")),
        unpinned(&format!("git+https://h/org/repo@{SHA40}x")),
        unpinned("git+https://h/org/repo@main"),
        unpinned(&format!("git+h:org/repo@{SHA40}")),
    ];
    for (spec, expected) in python_cases {
        assert_eq!(
            rules("uvx", &["--from", spec.as_str(), "bin"]),
            expected,
            "uvx --from {spec}"
        );
    }
}

/// Shell options before `-c` are skipped with their values; more shells
/// are covered; `env` clusters take values; pass-through wrappers are
/// unwrapped one level.
#[test]
fn wrappers_and_shells_are_seen_through() {
    let npm = "unpinned-npm-package";
    let opaque = "opaque-command";
    let python = "unpinned-python-package";
    let cases: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        ("bash", vec!["-o", "pipefail", "-c", "x"], vec![opaque]),
        ("bash", vec!["-eo", "pipefail", "-c", "x"], vec![opaque]),
        ("bash", vec!["+x", "-c", "x"], vec![opaque]),
        ("bash", vec!["+O", "extglob", "-c", "x"], vec![opaque]),
        ("bash", vec!["--rcfile", "f", "-c", "x"], vec![opaque]),
        (
            "bash",
            vec!["--init-file=f", "--norc", "-c", "x"],
            vec![opaque],
        ),
        ("bash", vec!["-o", "pipefail", "script.sh"], vec![]),
        ("bash", vec!["-", "-c"], vec![]),
        // An unknown long option may have taken the operand as its value.
        ("bash", vec!["--frob", "script.sh"], vec![]),
        ("bash", vec!["--frob", "--", "-c", "x"], vec![opaque]),
        ("bash", vec!["--frob=1", "script.sh", "-c"], vec![]),
        ("fish", vec!["--frob", "x", "--command=y"], vec![opaque]),
        ("zsh", vec!["--emulate", "sh", "-c", "x"], vec![opaque]),
        // `env --` ends the options, not the assignments.
        ("env", vec!["--", "A=1", "npx", "pkg@1.0.0"], vec![]),
        ("timeout", vec!["--", "5", "npx", "pkg"], vec![npm]),
        // `uv --with` takes a comma-separated list.
        (
            "uvx",
            vec!["--with", "a[x,y]==1.0, b==2.0", "tool==1.0"],
            vec![],
        ),
        ("uvx", vec!["-w", "a==1.0,b", "tool==1.0"], vec![python]),
        ("uvx", vec!["--from", "a,b==1.0", "tool"], vec![python]),
        ("uvx", vec!["a[x==1.0"], vec![python]),
        ("dash", vec!["-c", "x"], vec![opaque]),
        ("ksh", vec!["-c", "x"], vec![opaque]),
        ("mksh", vec!["-c", "x"], vec![opaque]),
        ("/bin/ash", vec!["-c", "x"], vec![opaque]),
        ("fish", vec!["-c", "x"], vec![opaque]),
        ("fish", vec!["--command=x"], vec![opaque]),
        ("fish", vec!["--command", "x"], vec![opaque]),
        ("fish", vec!["-C", "x", "script.fish"], vec![opaque]),
        ("fish", vec!["--init-command=x", "s.fish"], vec![opaque]),
        ("fish", vec!["--debug", "x", "s.fish"], vec![]),
        ("env", vec!["-iu", "X", "npx", "pkg"], vec![npm]),
        ("env", vec!["-uX", "npx", "pkg"], vec![npm]),
        ("env", vec!["-P", "/opt/bin", "npx", "pkg"], vec![npm]),
        ("env", vec!["-C", "/w", "npx", "pkg"], vec![npm]),
        (
            "env",
            vec!["--chdir=/w", "--unset", "X", "npx", "pkg"],
            vec![npm],
        ),
        ("env", vec!["-", "npx", "pkg"], vec![npm]),
        ("env", vec!["--ignore-signal=PIPE", "npx", "pkg"], vec![npm]),
        ("env", vec!["--frobnicate", "npx", "pkg"], vec![opaque]),
        ("env", vec!["-x", "npx", "pkg"], vec![opaque]),
        ("timeout", vec!["30", "npx", "pkg"], vec![npm]),
        (
            "timeout",
            vec!["-k", "5", "-s", "KILL", "30", "npx", "pkg"],
            vec![npm],
        ),
        (
            "timeout",
            vec!["--foreground", "30", "npx", "pkg@1.0.0"],
            vec![],
        ),
        ("timeout", vec!["30"], vec![]),
        ("nice", vec!["-n", "5", "npx", "pkg"], vec![npm]),
        ("nice", vec!["-10", "npx", "pkg"], vec![npm]),
        ("nice", vec!["--adjustment=5", "npx", "pkg"], vec![npm]),
        ("nohup", vec!["npx", "pkg"], vec![npm]),
        ("stdbuf", vec!["-oL", "-e", "0", "npx", "pkg"], vec![npm]),
        ("stdbuf", vec!["--output=L", "npx", "pkg"], vec![npm]),
        ("setsid", vec!["-fw", "npx", "pkg"], vec![npm]),
        ("corepack", vec!["pnpm@9.1.0", "dlx", "pkg"], vec![npm]),
        ("corepack", vec!["npx", "pkg@1.0.0"], vec![]),
        ("corepack", vec!["yarn", "dlx", "pkg"], vec![npm]),
        // One level only: a wrapper inside a wrapper is opaque.
        (
            "timeout",
            vec!["5", "env", "npx", "pkg@1.0.0"],
            vec![opaque],
        ),
        ("env", vec!["nice", "npx", "pkg@1.0.0"], vec![opaque]),
        ("nohup", vec!["sh", "-c", "x"], vec![opaque]),
        ("nohup", vec!["--frobnicate", "npx"], vec![opaque]),
    ];
    for (command, list, expected) in cases {
        assert_eq!(rules(command, &list), expected, "{command} {list:?}");
    }
    let findings = audit_command("timeout", &args(&["5", "env", "npx", "x"]));
    assert_eq!(findings[0].launcher, "env");
    let findings = audit_command("env", &args(&["--frobnicate", "npx"]));
    assert_eq!(findings[0].args, ["--frobnicate"]);
    let findings = audit_command("fish", &args(&["-c", "run it"]));
    assert_eq!(findings[0].launcher, "fish -c");
    assert_eq!(findings[0].args, ["-c", "run it"]);
}

fn stdio_with_env(env: &str) -> Definition {
    parse_definition(
        &format!(
            "name = \"e\"\n[transport]\ntype = \"stdio\"\ncommand = \"node\"\nargs = [\"server.js\"]\n[env]\n{env}\n"
        ),
        "e",
    )
    .unwrap()
}

/// A literal value of a deny-listed variable is `execution-env`; the
/// finding names the variable, never the value; forwarded values are the
/// user's own and not a finding.
#[test]
fn execution_env_names_the_variable_only() {
    let canary = "CANARY-hook-value";
    for name in [
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "BASH_ENV",
        "npm_config_registry",
        "NPM_CONFIG_USERCONFIG",
        "UV_INDEX_URL",
        "UV_EXTRA_INDEX_URL",
        "UV_DEFAULT_INDEX",
        "PIP_INDEX_URL",
        "PIP_EXTRA_INDEX_URL",
        "GIT_SSH_COMMAND",
        "GIT_CONFIG_COUNT",
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "PATH",
    ] {
        let definition = stdio_with_env(&format!("{name} = {{ value = \"{canary}\" }}"));
        let findings = audit(&definition);
        assert_eq!(
            findings,
            [Finding {
                rule: AuditRule::ExecutionEnv,
                launcher: "[env]".to_owned(),
                args: vec![name.to_owned()],
            }],
            "{name}"
        );
        let json = serde_json::to_string(&findings).unwrap();
        assert!(!json.contains(canary), "{name}: {json}");
    }
    for env in [
        "NODE_OPTIONS = { forward = true }",
        "LOG_LEVEL = { value = \"debug\" }",
        "UV_CACHE_DIR = { value = \"/c\" }",
        "PIP_NO_CACHE_DIR = { value = \"1\" }",
    ] {
        assert_eq!(audit(&stdio_with_env(env)), [], "{env}");
    }
    // An `env` wrapper assignment too; the value is never in the finding.
    let findings = audit_command(
        "env",
        &args(&["NODE_OPTIONS=--require=./hook.js", "LANG=C", "node", "s.js"]),
    );
    assert_eq!(
        findings,
        [Finding {
            rule: AuditRule::ExecutionEnv,
            launcher: "env".to_owned(),
            args: args(&["NODE_OPTIONS"]),
        }]
    );
    // A changed value keeps the findings hash: the definition hash binds it.
    let a = audit(&stdio_with_env("NODE_OPTIONS = { value = \"a\" }"));
    let b = audit(&stdio_with_env("NODE_OPTIONS = { value = \"b\" }"));
    assert_eq!(findings_hash(&a), findings_hash(&b));
}

/// The reference documents every rule id and every deny-listed name.
#[test]
fn reference_documents_the_rules_and_the_env_deny_list() {
    let reference =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/reference.md"))
            .unwrap();
    for rule in AuditRule::ALL {
        assert!(
            reference.contains(&format!("| `{}` |", rule.as_str())),
            "the audit rule table lacks `{}`",
            rule.as_str()
        );
    }
    for entry in env::documented() {
        assert!(
            reference.contains(&format!("`{entry}`")),
            "the reference does not list `{entry}`"
        );
    }
}
