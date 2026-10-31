//! Shells and wrappers: a shell's `-c` script is opaque; `env` and the
//! pass-through wrappers run their operand command, which is audited.

use super::env::changes_execution;
use super::{AuditRule, Finding};

/// A shell whose command-string options make it opaque.
pub(super) struct Shell {
    /// Short option letters that run a command string (`-c`).
    script_letters: &'static str,
    /// Short option letters that take the next argument (`-o pipefail`).
    value_letters: &'static str,
    /// Long options that run a command string (`--command`).
    long_script: &'static [&'static str],
    /// Long options that take the next argument (`--rcfile f`).
    long_values: &'static [&'static str],
    /// Long options known to take no value (`--login`); any other long
    /// option might take the next argument.
    long_flags: &'static [&'static str],
}

const POSIX_SHELL: Shell = Shell {
    script_letters: "c",
    value_letters: "oO",
    long_script: &[],
    long_values: &["--rcfile", "--init-file", "--emulate"],
    long_flags: &[
        "--login",
        "--noprofile",
        "--norc",
        "--posix",
        "--restricted",
        "--verbose",
        "--noediting",
        "--debugger",
        "--help",
        "--version",
    ],
};

const FISH: Shell = Shell {
    script_letters: "cC",
    value_letters: "dfop",
    long_script: &["--command", "--init-command"],
    long_values: &["--debug", "--debug-output", "--features", "--profile"],
    long_flags: &[
        "--login",
        "--interactive",
        "--private",
        "--no-config",
        "--no-execute",
        "--help",
        "--version",
    ],
};

/// The shells whose `-c` script is opaque.
pub(super) fn shell(program: &str) -> Option<&'static Shell> {
    const POSIX: &[&str] = &[
        "sh", "bash", "zsh", "dash", "ksh", "mksh", "ash", "yash", "csh", "tcsh",
    ];
    if POSIX.contains(&program) {
        Some(&POSIX_SHELL)
    } else if program == "fish" {
        Some(&FISH)
    } else {
        None
    }
}

/// The command-string option and its script, among the shell's leading
/// options (a `-` or `+` cluster, value options and their values, long
/// options); `None` when the first operand (a script file) comes first.
/// An unknown long option may have taken that operand as its value: when a
/// command-string option follows later, the unknown option is returned
/// (fail closed).
pub(super) fn shell_script(shell: &Shell, args: &[String]) -> Option<Vec<String>> {
    let mut index = 0;
    // The last unknown long option without an `=value`.
    let mut unknown: Option<&str> = None;
    let operand = |index: usize, unknown: Option<&str>| {
        let unknown = unknown?;
        args[index..]
            .iter()
            .any(|later| is_script_option(shell, later))
            .then(|| vec![unknown.to_owned()])
    };
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" || arg == "-" {
            return operand(index, unknown);
        }
        if arg.starts_with("--") {
            let (name, inline) = match arg.split_once('=') {
                Some((name, _)) => (name, true),
                None => (arg, false),
            };
            if shell.long_script.contains(&name) {
                let mut found = vec![arg.to_owned()];
                if !inline {
                    found.extend(args.get(index + 1).cloned());
                }
                return Some(found);
            }
            if !inline && shell.long_values.contains(&name) {
                index += 2;
                unknown = None;
            } else {
                index += 1;
                unknown = (!inline && !shell.long_flags.contains(&name)).then_some(arg);
            }
            continue;
        }
        // An operand (a script file) ends the options.
        let Some(letters) = arg.strip_prefix('-').or_else(|| arg.strip_prefix('+')) else {
            return operand(index, unknown);
        };
        unknown = None;
        let values = letters
            .chars()
            .filter(|letter| shell.value_letters.contains(*letter))
            .count();
        if arg.starts_with('-')
            && letters
                .chars()
                .any(|letter| shell.script_letters.contains(letter))
        {
            let mut found = vec![arg.to_owned()];
            found.extend(args.get(index + 1 + values).cloned());
            return Some(found);
        }
        index += 1 + values;
    }
    None
}

/// Whether `arg` runs a command string: a `-` cluster with a script letter
/// or a long script option.
fn is_script_option(shell: &Shell, arg: &str) -> bool {
    if arg.starts_with("--") {
        let name = arg.split_once('=').map_or(arg, |(name, _)| name);
        return shell.long_script.contains(&name);
    }
    arg.strip_prefix('-').is_some_and(|letters| {
        letters
            .chars()
            .any(|letter| shell.script_letters.contains(letter))
    })
}

/// A wrapper that runs its operand command: `env` or a pass-through
/// wrapper. Options follow getopt: a value letter takes the rest of its
/// cluster or the next argument.
pub(super) struct Wrapper {
    /// Short flag letters.
    short_flags: &'static str,
    /// Short letters that take a value.
    short_values: &'static str,
    /// Short letters that make the command opaque (`env -S`).
    short_opaque: &'static str,
    /// Long options without a value (an optional `=value` is accepted).
    long_flags: &'static [&'static str],
    /// Long options that take a value.
    long_values: &'static [&'static str],
    /// Long options that make the command opaque.
    long_opaque: &'static [&'static str],
    /// Operands before the command (`timeout`'s duration).
    operands: usize,
    /// Whether `NAME=value` assignments come before the command (`env`).
    assignments: bool,
    /// Whether `-` alone is a flag (`env -`).
    dash_flag: bool,
    /// Whether a numeric option such as `-10` is accepted (`nice`).
    numeric: bool,
    /// Whether the command may carry a version (`corepack pnpm@9.1.0`).
    pub(super) versioned_command: bool,
}

const fn wrapper_spec() -> Wrapper {
    Wrapper {
        short_flags: "",
        short_values: "",
        short_opaque: "",
        long_flags: &[],
        long_values: &[],
        long_opaque: &[],
        operands: 0,
        assignments: false,
        dash_flag: false,
        numeric: false,
        versioned_command: false,
    }
}

const ENV: Wrapper = Wrapper {
    short_flags: "i0v",
    short_values: "uCPL",
    short_opaque: "S",
    long_flags: &[
        "--ignore-environment",
        "--null",
        "--debug",
        "--block-signal",
        "--default-signal",
        "--ignore-signal",
        "--list-signal-handling",
    ],
    long_values: &["--unset", "--chdir"],
    long_opaque: &["--split-string"],
    assignments: true,
    dash_flag: true,
    ..wrapper_spec()
};

const TIMEOUT: Wrapper = Wrapper {
    short_flags: "v",
    short_values: "ks",
    long_flags: &["--preserve-status", "--foreground", "--verbose"],
    long_values: &["--kill-after", "--signal"],
    operands: 1,
    ..wrapper_spec()
};

const NICE: Wrapper = Wrapper {
    short_values: "n",
    long_values: &["--adjustment"],
    numeric: true,
    ..wrapper_spec()
};

const NOHUP: Wrapper = wrapper_spec();

const STDBUF: Wrapper = Wrapper {
    short_values: "ioe",
    long_values: &["--input", "--output", "--error"],
    ..wrapper_spec()
};

const SETSID: Wrapper = Wrapper {
    short_flags: "cfw",
    long_flags: &["--ctty", "--fork", "--wait"],
    ..wrapper_spec()
};

const COREPACK: Wrapper = Wrapper {
    versioned_command: true,
    ..wrapper_spec()
};

/// The wrapper of a program basename.
pub(super) fn wrapper(program: &str) -> Option<&'static Wrapper> {
    match program {
        "env" => Some(&ENV),
        "timeout" => Some(&TIMEOUT),
        "nice" => Some(&NICE),
        "nohup" => Some(&NOHUP),
        "stdbuf" => Some(&STDBUF),
        "setsid" => Some(&SETSID),
        "corepack" => Some(&COREPACK),
        _ => None,
    }
}

/// What a wrapper runs.
pub(super) enum Unwrapped {
    /// No command (`env` prints the environment).
    None,
    /// The position of the command it runs.
    Nested(usize),
    /// An option that makes the command opaque: `env -S`, or an option the
    /// table does not know.
    Opaque(String),
}

/// Skips a wrapper's options (and `env`'s assignments, reporting
/// `execution-env` for a deny-listed name) up to the command it runs.
pub(super) fn unwrap(spec: &Wrapper, args: &[String], findings: &mut Vec<Finding>) -> Unwrapped {
    let mut index = 0;
    // `--` ends the options, but not `env`'s assignments:
    // `env -- LD_PRELOAD=/x cmd` still sets the variable.
    let mut options = true;
    while index < args.len() {
        let arg = args[index].as_str();
        if options {
            if arg == "--" {
                index += 1;
                if !spec.assignments {
                    break;
                }
                // Only assignments are left before the command.
                options = false;
                continue;
            }
            if arg == "-" && spec.dash_flag {
                index += 1;
                continue;
            }
            if let Some(used) = option(spec, arg) {
                match used {
                    Ok(used) => index += used,
                    Err(opaque) => return Unwrapped::Opaque(opaque),
                }
                continue;
            }
        }
        if spec.assignments
            && let Some((name, _)) = arg.split_once('=')
        {
            if changes_execution(name) {
                findings.push(Finding::new(
                    AuditRule::ExecutionEnv,
                    "env",
                    vec![name.to_owned()],
                ));
            }
            index += 1;
            continue;
        }
        break;
    }
    index += spec.operands;
    if index < args.len() {
        Unwrapped::Nested(index)
    } else {
        Unwrapped::None
    }
}

/// A wrapper option: `None` when `arg` is not an option, else how many
/// arguments it uses, or the option itself when it makes the command
/// opaque.
fn option(spec: &Wrapper, arg: &str) -> Option<Result<usize, String>> {
    if arg.starts_with("--") {
        let (name, inline) = match arg.split_once('=') {
            Some((name, _)) => (name, true),
            None => (arg, false),
        };
        if spec.long_opaque.contains(&name) {
            return Some(Err(arg.to_owned()));
        }
        return Some(if spec.long_values.contains(&name) {
            Ok(if inline { 1 } else { 2 })
        } else if spec.long_flags.contains(&name) {
            Ok(1)
        } else {
            Err(arg.to_owned())
        });
    }
    let letters = arg.strip_prefix('-').filter(|rest| !rest.is_empty())?;
    if spec.numeric && letters.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Ok(1));
    }
    Some(short_cluster(spec, letters).ok_or_else(|| arg.to_owned()))
}

/// How many arguments a short cluster uses (getopt: a value letter takes
/// the rest of the cluster, or the next argument when it is last), or
/// `None` when it is opaque or unknown.
fn short_cluster(spec: &Wrapper, letters: &str) -> Option<usize> {
    for (position, letter) in letters.char_indices() {
        if spec.short_opaque.contains(letter) {
            return None;
        }
        if spec.short_values.contains(letter) {
            let last = position + letter.len_utf8() == letters.len();
            return Some(if last { 2 } else { 1 });
        }
        if !spec.short_flags.contains(letter) {
            return None;
        }
    }
    Some(1)
}
