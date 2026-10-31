//! The runner table and the option lists of each runner family. An option
//! missing from these lists makes the launcher `unparsed-launcher`, so a
//! list errs on the side of fewer entries, never on a wrong arity.

use super::AuditRule;

/// The family of a runner: which spec rules and option lists apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Family {
    Npm,
    Python,
    Container,
}

impl Family {
    pub(super) fn options(self) -> &'static Options {
        match self {
            Family::Npm => &NPM_OPTIONS,
            Family::Python => &PYTHON_OPTIONS,
            Family::Container => &CONTAINER_OPTIONS,
        }
    }

    pub(super) fn rule(self) -> AuditRule {
        match self {
            Family::Npm => AuditRule::UnpinnedNpmPackage,
            Family::Python => AuditRule::UnpinnedPythonPackage,
            Family::Container => AuditRule::UnpinnedContainerImage,
        }
    }
}

/// A covered runner.
pub(super) struct Runner {
    /// The command basename.
    pub(super) program: &'static str,
    /// The subcommand words after the global options; empty for none.
    pub(super) subcommand: &'static [&'static str],
    /// The spec rules and option lists.
    pub(super) family: Family,
    /// Global options (before the subcommand) that take a value.
    pub(super) global_values: &'static [&'static str],
}

/// The covered runners. Further launchers are added here.
pub(super) const RUNNERS: &[Runner] = &[
    runner("npx", &[], Family::Npm, NPM_GLOBALS),
    runner("npm", &["exec"], Family::Npm, NPM_GLOBALS),
    runner("npm", &["x"], Family::Npm, NPM_GLOBALS),
    runner("pnpm", &["dlx"], Family::Npm, PNPM_GLOBALS),
    runner("pnpx", &[], Family::Npm, PNPM_GLOBALS),
    runner("bunx", &[], Family::Npm, &[]),
    runner("bun", &["x"], Family::Npm, &[]),
    runner("yarn", &["dlx"], Family::Npm, &["--cwd"]),
    runner("uvx", &[], Family::Python, UV_GLOBALS),
    runner("uv", &["tool", "run"], Family::Python, UV_GLOBALS),
    runner("pipx", &["run"], Family::Python, &[]),
    runner("docker", &["run"], Family::Container, CONTAINER_GLOBALS),
    runner(
        "docker",
        &["container", "run"],
        Family::Container,
        CONTAINER_GLOBALS,
    ),
    runner("podman", &["run"], Family::Container, CONTAINER_GLOBALS),
    runner(
        "podman",
        &["container", "run"],
        Family::Container,
        CONTAINER_GLOBALS,
    ),
    runner("nerdctl", &["run"], Family::Container, CONTAINER_GLOBALS),
    runner(
        "nerdctl",
        &["container", "run"],
        Family::Container,
        CONTAINER_GLOBALS,
    ),
];

const fn runner(
    program: &'static str,
    subcommand: &'static [&'static str],
    family: Family,
    global_values: &'static [&'static str],
) -> Runner {
    Runner {
        program,
        subcommand,
        family,
        global_values,
    }
}

const NPM_GLOBALS: &[&str] = &[
    "--prefix",
    "--userconfig",
    "--globalconfig",
    "--cache",
    "--registry",
    "--loglevel",
    "--tag",
    "-w",
    "--workspace",
];
const PNPM_GLOBALS: &[&str] = &["-C", "--dir", "--filter", "-F", "--reporter", "--loglevel"];
const UV_GLOBALS: &[&str] = &[
    "--directory",
    "--project",
    "--cache-dir",
    "--config-file",
    "--color",
];
const CONTAINER_GLOBALS: &[&str] = &[
    "-c",
    "--context",
    "-H",
    "--host",
    "--config",
    "-l",
    "--log-level",
    "--tlscacert",
    "--tlscert",
    "--tlskey",
    "--url",
    "--connection",
    "--root",
    "--runroot",
    "--storage-driver",
    "-n",
    "--namespace",
    "-a",
    "--address",
];

/// The option lists of a family, after the subcommand.
pub(super) struct Options {
    /// Options whose value is the package spec; the first positional
    /// argument is then the command it runs, not a spec.
    pub(super) spec: &'static [&'static str],
    /// Options whose value is an additional package spec.
    pub(super) extra_spec: &'static [&'static str],
    /// Options that run a shell string (opaque).
    pub(super) shell: &'static [&'static str],
    /// Other options that take a value.
    pub(super) values: &'static [&'static str],
    /// Options that take no value (a `--flag=value` form is accepted too).
    pub(super) flags: &'static [&'static str],
}

impl Options {
    /// Whether `option` (such as `-y` or `--rm`) is a known flag.
    pub(super) fn is_flag(&self, option: &str) -> bool {
        self.flags.contains(&option)
    }

    /// Whether `option` takes a value: a value, spec, extra-spec or shell
    /// option.
    pub(super) fn takes_value(&self, option: &str) -> bool {
        [self.values, self.spec, self.extra_spec, self.shell]
            .iter()
            .any(|list| list.contains(&option))
    }
}

const NPM_OPTIONS: Options = Options {
    spec: &["-p", "--package"],
    extra_spec: &[],
    shell: &["-c", "--call", "--shell-mode"],
    values: &[
        "--registry",
        "--cache",
        "--userconfig",
        "--globalconfig",
        "--prefix",
        "-w",
        "--workspace",
        "--node-options",
        "--script-shell",
        "--cwd",
        "--tag",
        "--loglevel",
        "--before",
        "--color",
        "--reporter",
        "--allow-build",
        "--cpu",
        "--os",
        "--libc",
    ],
    flags: &[
        "-y",
        "--yes",
        "--no",
        "--no-install",
        "--ignore-existing",
        "-q",
        "--quiet",
        "-s",
        "--silent",
        "--verbose",
        "--prefer-offline",
        "--prefer-online",
        "--offline",
        "--workspaces",
        "--ws",
        "--include-workspace-root",
        "--legacy-peer-deps",
        "--no-color",
        "--bun",
    ],
};

const PYTHON_OPTIONS: Options = Options {
    spec: &["--from", "--spec"],
    extra_spec: &["--with", "-w"],
    shell: &[],
    values: &[
        "--python",
        "-p",
        "--index",
        "--index-url",
        "-i",
        "--extra-index-url",
        "--default-index",
        "--find-links",
        "-f",
        "--with-editable",
        "--with-requirements",
        "--constraints",
        "-c",
        "--overrides",
        "--build-constraints",
        "--prerelease",
        "--resolution",
        "--exclude-newer",
        "--exclude-newer-package",
        "--link-mode",
        "--cache-dir",
        "--config-setting",
        "-C",
        "--env-file",
        "--keyring-provider",
        "--python-preference",
        "--index-strategy",
        "--fork-strategy",
        "--torch-backend",
        "--allow-insecure-host",
        "--refresh-package",
        "--reinstall-package",
        "--upgrade-package",
        "-P",
        "--no-build-package",
        "--no-binary-package",
        "--pip-args",
        "--backend",
        "--directory",
        "--project",
        "--color",
        "--config-file",
    ],
    flags: &[
        "--isolated",
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "-n",
        "--no-cache",
        "--offline",
        "--no-progress",
        "--native-tls",
        "--no-config",
        "--refresh",
        "--reinstall",
        "-U",
        "--upgrade",
        "--no-build",
        "--no-binary",
        "--no-sources",
        "--no-index",
        "--compile-bytecode",
        "--no-build-isolation",
        "--no-python-downloads",
        "--managed-python",
        "--no-managed-python",
        "--no-env-file",
        "--show-resolution",
        "--preview",
        "--system-site-packages",
        "--pypackages",
        "--fetch-missing-python",
    ],
};

const CONTAINER_OPTIONS: Options = Options {
    spec: &[],
    extra_spec: &[],
    shell: &[],
    values: &[
        "-a",
        "--attach",
        "--add-host",
        "--annotation",
        "--arch",
        "--authfile",
        "--blkio-weight",
        "--blkio-weight-device",
        "--cap-add",
        "--cap-drop",
        "--cgroup-conf",
        "--cgroup-parent",
        "--cgroupns",
        "--cgroups",
        "--cidfile",
        "--conmon-pidfile",
        "--cpu-period",
        "--cpu-quota",
        "--cpu-rt-period",
        "--cpu-rt-runtime",
        "--cpu-shares",
        "-c",
        "--cpus",
        "--cpuset-cpus",
        "--cpuset-mems",
        "--creds",
        "--detach-keys",
        "--device",
        "--device-cgroup-rule",
        "--device-read-bps",
        "--device-read-iops",
        "--device-write-bps",
        "--device-write-iops",
        "--dns",
        "--dns-option",
        "--dns-search",
        "--domainname",
        "--entrypoint",
        "-e",
        "--env",
        "--env-file",
        "--expose",
        "--gidmap",
        "--gpus",
        "--group-add",
        "--health-cmd",
        "--health-interval",
        "--health-retries",
        "--health-start-interval",
        "--health-start-period",
        "--health-timeout",
        "-h",
        "--hostname",
        "--ip",
        "--ip6",
        "--ipc",
        "--isolation",
        "--kernel-memory",
        "-l",
        "--label",
        "--label-file",
        "--link",
        "--link-local-ip",
        "--log-driver",
        "--log-opt",
        "--mac-address",
        "-m",
        "--memory",
        "--memory-reservation",
        "--memory-swap",
        "--memory-swappiness",
        "--mount",
        "--name",
        "--network",
        "--net",
        "--network-alias",
        "--oom-score-adj",
        "--os",
        "--pid",
        "--pidfile",
        "--pids-limit",
        "--platform",
        "--pod",
        "-p",
        "--publish",
        "--pull",
        "--restart",
        "--runtime",
        "--secret",
        "--security-opt",
        "--shm-size",
        "--stop-signal",
        "--stop-timeout",
        "--storage-opt",
        "--sysctl",
        "--systemd",
        "--tmpfs",
        "--tz",
        "--uidmap",
        "--ulimit",
        "--umask",
        "--unsetenv",
        "-u",
        "--user",
        "--userns",
        "--uts",
        "--variant",
        "-v",
        "--volume",
        "--volume-driver",
        "--volumes-from",
        "-w",
        "--workdir",
    ],
    flags: &[
        "-d",
        "--detach",
        "-i",
        "--interactive",
        "-t",
        "--tty",
        "--rm",
        "-P",
        "--publish-all",
        "--privileged",
        "--init",
        "--read-only",
        "--read-only-tmpfs",
        "-q",
        "--quiet",
        "--sig-proxy",
        "--no-healthcheck",
        "--oom-kill-disable",
        "--disable-content-trust",
        "--replace",
        "--rmi",
        "--tls-verify",
        "--env-host",
        "--http-proxy",
        "--no-hosts",
    ],
};
