//! Parsing a runner's arguments: the global options before its subcommand
//! and the options before its package spec.

use super::spec::check_spec;
use super::tables::{Family, Runner};
use super::{AuditRule, Finding};

/// Where a runner's subcommand ends.
pub(super) enum Subcommand {
    /// The position after the subcommand.
    At(usize),
    /// The arguments do not invoke the subcommand.
    NotInvoked,
    /// An unknown global option may have taken the next word as its value,
    /// and the subcommand follows later: the launcher cannot be parsed.
    Unparsed(String),
}

/// Finds the end of the runner's subcommand, skipping global options.
pub(super) fn after_subcommand(runner: &Runner, args: &[String]) -> Subcommand {
    let mut index = 0;
    let mut words = runner.subcommand.iter();
    let Some(mut expected) = words.next() else {
        return Subcommand::At(0);
    };
    // The last option, when it is not known to take a value but might.
    let mut ambiguous: Option<&str> = None;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == *expected {
            index += 1;
            ambiguous = None;
            match words.next() {
                Some(next) => expected = next,
                None => return Subcommand::At(index),
            }
            continue;
        }
        if arg.starts_with('-') && arg != "-" && arg != "--" {
            if takes_value(arg, runner.global_values) {
                index += 2;
                ambiguous = None;
            } else {
                ambiguous = (!arg.contains('=')).then_some(arg);
                index += 1;
            }
            continue;
        }
        return match ambiguous {
            Some(option) if args[index + 1..].iter().any(|later| later == *expected) => {
                Subcommand::Unparsed(option.to_owned())
            }
            _ => Subcommand::NotInvoked,
        };
    }
    Subcommand::NotInvoked
}

/// Whether an option takes the next argument as its value (it is in
/// `values` and has no `=value` of its own).
fn takes_value(arg: &str, values: &[&str]) -> bool {
    !arg.contains('=') && values.contains(&arg)
}

/// The value of an option written as `--opt=value` or `--opt value`, for an
/// option in `names`; the second element is how many arguments it uses.
fn option_value<'a>(args: &'a [String], index: usize, names: &[&str]) -> Option<(&'a str, usize)> {
    let arg = args[index].as_str();
    if let Some((name, value)) = arg.split_once('=')
        && names.contains(&name)
    {
        return Some((value, 1));
    }
    if names.contains(&arg) {
        return Some((args.get(index + 1).map_or("", String::as_str), 2));
    }
    None
}

/// The items of a comma-separated requirement list; a comma inside the
/// `[...]` of an extras list does not separate.
fn split_list(value: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (position, byte) in value.bytes().enumerate() {
        match byte {
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                items.push(value[start..position].trim());
                start = position + 1;
            }
            _ => {}
        }
    }
    items.push(value[start..].trim());
    items
}

/// How many arguments a known option that is neither a spec nor a shell
/// option uses, or `None` for an option the family does not know. A short
/// option cluster (`-it`) is known only when every letter is a flag: a
/// letter that takes a value (`-dp`) would take the rest of the cluster or
/// the next argument, which parsers handle differently.
fn known_option(family: Family, arg: &str) -> Option<usize> {
    let options = family.options();
    if let Some((name, _)) = arg.split_once('=') {
        return (options.takes_value(name) || options.is_flag(name)).then_some(1);
    }
    if options.takes_value(arg) {
        return Some(2);
    }
    if options.is_flag(arg) {
        return Some(1);
    }
    let letters = arg
        .strip_prefix('-')
        .filter(|rest| !rest.starts_with('-'))?;
    (letters.chars().count() > 1
        && letters
            .chars()
            .all(|letter| options.is_flag(&format!("-{letter}"))))
    .then_some(1)
}

pub(super) fn audit_runner(
    family: Family,
    launcher: &str,
    args: &[String],
    findings: &mut Vec<Finding>,
) {
    let options = family.options();
    let mut specs: Vec<&str> = Vec::new();
    let mut named_spec = false;
    let mut shell_mode = false;
    let mut positional = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            positional = args.get(index + 1).map(String::as_str);
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            positional = Some(arg);
            break;
        }
        if let Some((value, used)) = option_value(args, index, options.shell) {
            let mut matched = vec![args[index].clone()];
            if used == 2 {
                matched.push(value.to_owned());
            }
            findings.push(Finding::new(AuditRule::OpaqueCommand, launcher, matched));
            shell_mode = true;
            index += used;
            continue;
        }
        if let Some((value, used)) = option_value(args, index, options.spec) {
            specs.push(value);
            named_spec = true;
            index += used;
            continue;
        }
        if let Some((value, used)) = option_value(args, index, options.extra_spec) {
            // `uv --with a,b==1.0` takes a comma-separated list: each item
            // is a spec of its own.
            specs.extend(split_list(value));
            index += used;
            continue;
        }
        let Some(used) = known_option(family, arg) else {
            // The spec is unknown: fail closed.
            findings.push(Finding::new(
                AuditRule::UnparsedLauncher,
                launcher,
                vec![arg.to_owned()],
            ));
            return;
        };
        index += used;
    }
    if !named_spec {
        match positional {
            Some(spec) => specs.push(spec),
            // The shell string is the command; it is already opaque.
            None if shell_mode => {}
            None => {
                findings.push(Finding::new(family.rule(), launcher, Vec::new()));
                return;
            }
        }
    }
    for spec in specs {
        if let Some(rule) = check_spec(family, spec) {
            findings.push(Finding::new(rule, launcher, vec![spec.to_owned()]));
        }
    }
}
