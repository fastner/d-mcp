//! A fake `claude` for d-mcp's tests (built as the `fake_claude` test target
//! without the libtest harness, and installed by `tests/common/mod.rs` into a
//! temporary `PATH` directory). When cargo runs it as a test, without a
//! `mcp` or `--version` argument, it exits at once and touches nothing.
//!
//! It implements the part of `claude mcp` that d-mcp uses, on the same file
//! Claude Code 2.1.310 uses: a legacy `.config.json` in the configuration
//! home (`$CLAUDE_CONFIG_DIR`, `$HOME/.claude` when it is unset, and the
//! working directory when it is set but empty) when it exists, otherwise
//! `$CLAUDE_CONFIG_DIR/.claude.json` or `$HOME/.claude.json`. Like Claude
//! Code, every write of an existing file first copies it to
//! `<configuration home>/backups/<file name>.backup.<millis>`, so with an
//! empty `CLAUDE_CONFIG_DIR` the backup lands in the working directory:
//!
//! - `mcp add-json [-s local|user] [--client-secret] <name> <json>`, which
//!   normalizes like Claude Code (adds `args = []` to stdio entries, drops
//!   unknown keys, writes integral numbers as integers), rejects reserved
//!   names and an existing user-scope name, accepts a user-scope name that
//!   exists in local scope, and needs `MCP_CLIENT_SECRET` for
//!   `--client-secret`;
//! - `mcp remove [-s local|user] <name>`;
//! - `mcp get <name>`, printing the recorded outputs from
//!   `tests/fixtures/claude/get/`;
//! - `mcp list` and `--version`.
//!
//! It only ever changes `mcpServers` (or a project's `mcpServers`) and keeps
//! every other key. It refuses to run (exit 97) unless `HOME` lies in a
//! directory marked by `.d-mcp-test-root` inside the system temp directory,
//! and, with an empty `CLAUDE_CONFIG_DIR`, unless its working directory lies
//! inside the system temp directory, so it can never touch a real
//! configuration.
//!
//! Every call is logged to `<root>/fake-claude.log` as one JSON object with
//! the arguments, the working directory, `PATH`, `HOME`, `CLAUDE_CONFIG_DIR`,
//! `DISABLE_AUTOUPDATER` and whether `MCP_CLIENT_SECRET` is present (never
//! its value). Directives in `<root>/fake-claude-mode`, one per line:
//!
//! - `fail <subcommand> <name>`: exit 1 without a change;
//! - `fail-once <subcommand> <name>`: the same, for the first such call;
//! - `noop <subcommand> <name>`: exit 0 without a change;
//! - `alter <subcommand> <name>`: write the entry with an extra key;
//! - `sleep <subcommand> <name>`: sleep 30 s before doing anything;
//! - `sleep-after <subcommand> <name>`: do the work, then sleep 30 s;
//! - `edit-on <subcommand> <name> <target>`: also change `<target>`, as a
//!   concurrent Claude Code session would;
//! - `login <name> <connected|needs-authentication|failed>`: the `get` status.
//!
//! `<name>` may be `*`. It prints canary strings that must never reach
//! d-mcp's output or store.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Map, Number, Value, json};

const CONNECTED: &str = include_str!("../fixtures/claude/get/connected.txt");
const NEEDS_AUTHENTICATION: &str = include_str!("../fixtures/claude/get/needs-authentication.txt");
const FAILED: &str = include_str!("../fixtures/claude/get/failed.txt");

const RESERVED: &[&str] = &[
    "workspace",
    "claude-in-chrome",
    "computer-use",
    "Claude Preview",
    "Claude Browser",
];

/// The keys Claude Code 2.1.310 keeps in an entry; others are dropped.
const KNOWN_KEYS: &[&str] = &[
    "type",
    "command",
    "args",
    "env",
    "url",
    "headers",
    "oauth",
    "timeout",
    "headersHelper",
];

fn refuse(reason: &str) -> ExitCode {
    eprintln!("fake claude refuses to run: {reason}");
    ExitCode::from(97)
}

/// The test root: the parent of `HOME`, which must hold the marker and lie
/// in the system temp directory.
fn test_root() -> Result<PathBuf, &'static str> {
    let home = std::env::var_os("HOME").ok_or("HOME is unset")?;
    let home = fs::canonicalize(home).map_err(|_| "HOME does not exist")?;
    let root = home.parent().ok_or("HOME has no parent")?.to_path_buf();
    let temp = fs::canonicalize(std::env::temp_dir()).map_err(|_| "no temp dir")?;
    if !root.starts_with(&temp) {
        return Err("HOME is not inside the system temp directory");
    }
    if !root.join(".d-mcp-test-root").is_file() {
        return Err("HOME is not inside a d-mcp test root");
    }
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if dir.is_empty() => {
            let cwd = std::env::current_dir().map_err(|_| "no working directory")?;
            let cwd = fs::canonicalize(&cwd).unwrap_or(cwd);
            if !cwd.starts_with(&temp) {
                return Err("the working directory is outside the system temp directory");
            }
        }
        Some(dir) => {
            let dir = PathBuf::from(dir);
            let resolved = fs::canonicalize(&dir).unwrap_or(dir);
            if !resolved.starts_with(&root) {
                return Err("CLAUDE_CONFIG_DIR is outside the test root");
            }
        }
        None => {}
    }
    Ok(root)
}

/// The configuration home, as in Claude Code 2.1.310: `$CLAUDE_CONFIG_DIR`;
/// an empty value is relative to the working directory, so it is the
/// working directory itself; `$HOME/.claude` when it is unset.
fn config_home() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if dir.is_empty() => std::env::current_dir().expect("working directory"),
        Some(dir) => PathBuf::from(dir),
        None => home.join(".claude"),
    }
}

/// The configuration file: the legacy `.config.json` in the configuration
/// home when it exists, otherwise `$CLAUDE_CONFIG_DIR/.claude.json`, or
/// `$HOME/.claude.json` when the variable is unset or empty.
fn config_file() -> PathBuf {
    let legacy = config_home().join(".config.json");
    if legacy.exists() {
        return legacy;
    }
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude.json"),
    }
}

/// Copies an existing configuration file to
/// `<configuration home>/backups/<file name>.backup.<millis>`, as Claude
/// Code 2.1.310 does before it writes.
fn backup(file: &Path) {
    let Ok(bytes) = fs::read(file) else {
        return;
    };
    let dir = config_home().join("backups");
    fs::create_dir_all(&dir).expect("create backups dir");
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    fs::write(dir.join(format!("{name}.backup.{millis}")), bytes).expect("write backup");
}

struct Directives {
    lines: Vec<Vec<String>>,
    state: PathBuf,
}

impl Directives {
    fn load(root: &Path) -> Self {
        let text = fs::read_to_string(root.join("fake-claude-mode")).unwrap_or_default();
        Self {
            lines: text
                .lines()
                .map(|line| line.split_whitespace().map(str::to_owned).collect())
                .filter(|words: &Vec<String>| !words.is_empty())
                .collect(),
            state: root.join("fake-claude-state"),
        }
    }

    /// The directive `<verb> <subcommand> <name> ...` that applies, if any.
    fn find(&self, verb: &str, subcommand: &str, name: &str) -> Option<&Vec<String>> {
        self.lines.iter().find(|words| {
            words.len() >= 3
                && words[0] == verb
                && words[1] == subcommand
                && (words[2] == name || words[2] == "*")
        })
    }

    fn has(&self, verb: &str, subcommand: &str, name: &str) -> bool {
        self.find(verb, subcommand, name).is_some()
    }

    /// Whether a `fail-once` directive fires now; it fires once.
    fn fail_once(&self, subcommand: &str, name: &str) -> bool {
        let Some(words) = self.find("fail-once", subcommand, name) else {
            return false;
        };
        let key = words.join(" ");
        let used = fs::read_to_string(&self.state).unwrap_or_default();
        if used.lines().any(|line| line == key) {
            return false;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.state)
            .expect("fake claude state");
        writeln!(file, "{key}").expect("write fake claude state");
        true
    }

    fn login(&self, name: &str) -> Option<&str> {
        self.lines
            .iter()
            .find(|words| {
                words.len() == 3 && words[0] == "login" && (words[1] == name || words[1] == "*")
            })
            .map(|words| words[2].as_str())
    }
}

fn log(root: &Path, args: &[String]) {
    let var = |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
    let line = json!({
        "args": args,
        "cwd": std::env::current_dir().ok().map(|dir| dir.to_string_lossy().into_owned()),
        "path": var("PATH"),
        "home": var("HOME"),
        "config_dir": var("CLAUDE_CONFIG_DIR"),
        "autoupdater": var("DISABLE_AUTOUPDATER"),
        "secret": if std::env::var_os("MCP_CLIENT_SECRET").is_some() { "present" } else { "absent" },
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("fake-claude.log"))
        .expect("open fake claude log");
    writeln!(file, "{line}").expect("write fake claude log");
}

fn load(file: &Path) -> Map<String, Value> {
    match fs::read(file) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(Value::Object(object)) => object,
            _ => {
                eprintln!("fake claude: the configuration is not valid JSON");
                std::process::exit(1);
            }
        },
        Err(_) => Map::new(),
    }
}

fn save(file: &Path, config: &Map<String, Value>) {
    backup(file);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).expect("create config dir");
    }
    let temp = file.with_extension("fake-tmp");
    let text = serde_json::to_string_pretty(&Value::Object(config.clone())).unwrap();
    fs::write(&temp, text).expect("write config");
    fs::rename(&temp, file).expect("replace config");
}

fn js_numbers(value: Value) -> Value {
    match value {
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() && float.fract() == 0.0 && float.abs() < 1e15 => {
                Value::Number(Number::from(float as i64))
            }
            _ => Value::Number(number),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(js_numbers).collect()),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, item)| (key, js_numbers(item)))
                .collect(),
        ),
        other => other,
    }
}

/// Claude Code's normalization of an `add-json` entry.
fn normalize(entry: Value) -> Option<Value> {
    let Value::Object(mut object) = js_numbers(entry) else {
        return None;
    };
    object.retain(|key, _| KNOWN_KEYS.contains(&key.as_str()));
    let stdio = object
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind == "stdio");
    if stdio {
        object.get("command")?;
        object.entry("args").or_insert_with(|| json!([]));
    } else {
        object.get("url")?;
    }
    Some(Value::Object(object))
}

fn servers<'a>(config: &'a mut Map<String, Value>, scope: &str) -> &'a mut Map<String, Value> {
    let holder = if scope == "user" {
        config
    } else {
        let cwd = std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        let projects = config
            .entry("projects")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("projects object");
        projects
            .entry(cwd)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("project object")
    };
    holder
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("mcpServers object")
}

fn sleep() {
    std::thread::sleep(Duration::from_secs(30));
}

struct Call {
    scope: String,
    client_secret: bool,
    positional: Vec<String>,
}

fn parse_call(args: &[String]) -> Call {
    let mut call = Call {
        scope: "local".to_owned(),
        client_secret: false,
        positional: Vec::new(),
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-s" | "--scope" => call.scope = iter.next().cloned().unwrap_or_default(),
            "--client-secret" => call.client_secret = true,
            _ => call.positional.push(arg.clone()),
        }
    }
    call
}

fn main() -> ExitCode {
    // Run by `cargo test` (no arguments, or test filters): nothing to do.
    let first = std::env::args().nth(1);
    if !matches!(first.as_deref(), Some("mcp" | "--version")) {
        return ExitCode::SUCCESS;
    }
    let root = match test_root() {
        Ok(root) => root,
        Err(reason) => return refuse(reason),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    log(&root, &args);
    eprintln!("CANARY_FAKE_CLAUDE_STDERR");
    let directives = Directives::load(&root);
    if args.first().map(String::as_str) == Some("--version") {
        println!("2.1.310 (Claude Code)");
        return ExitCode::SUCCESS;
    }
    if args.first().map(String::as_str) != Some("mcp") || args.len() < 2 {
        return ExitCode::from(2);
    }
    let subcommand = args[1].as_str();
    let call = parse_call(&args[2..]);
    let name = call.positional.first().cloned().unwrap_or_default();
    if directives.has("sleep", subcommand, &name) {
        sleep();
    }
    if directives.has("fail", subcommand, &name) || directives.fail_once(subcommand, &name) {
        println!("CANARY_FAKE_CLAUDE_STDOUT");
        return ExitCode::from(1);
    }
    let file = config_file();
    let mut config = load(&file);
    let code = match subcommand {
        "add-json" => add_json(&directives, &call, &name, &mut config, &file),
        "remove" => remove(&directives, &call, &name, &mut config, &file),
        "get" => get(&directives, &name, &mut config),
        "list" => {
            for name in servers(&mut config, "user").keys() {
                println!("{name}: CANARY_FAKE_CLAUDE_STDOUT");
            }
            0
        }
        _ => 2,
    };
    if code == 0 {
        if let Some(words) = directives.find("edit-on", subcommand, &name)
            && let Some(target) = words.get(3)
        {
            let mut config = load(&file);
            if let Some(Value::Object(entry)) = servers(&mut config, "user").get_mut(target) {
                entry.insert("edited".to_owned(), json!(true));
            }
            save(&file, &config);
        }
        if directives.has("sleep-after", subcommand, &name) {
            sleep();
        }
    }
    ExitCode::from(code)
}

fn add_json(
    directives: &Directives,
    call: &Call,
    name: &str,
    config: &mut Map<String, Value>,
    file: &Path,
) -> u8 {
    if RESERVED.contains(&name) {
        eprintln!("Cannot add MCP server \"{name}\": this name is reserved.");
        return 1;
    }
    let Some(entry) = call
        .positional
        .get(1)
        .and_then(|json| serde_json::from_str::<Value>(json).ok())
        .and_then(normalize)
    else {
        eprintln!("Invalid configuration");
        return 1;
    };
    if call.client_secret && std::env::var_os("MCP_CLIENT_SECRET").is_none() {
        // The real CLI would prompt; stdin is null.
        return 1;
    }
    let scope = call.scope.clone();
    let servers = servers(config, &scope);
    if servers.contains_key(name) {
        eprintln!("MCP server {name} already exists in {scope} config");
        return 1;
    }
    if directives.has("noop", "add-json", name) {
        return 0;
    }
    let mut entry = entry;
    if directives.has("alter", "add-json", name)
        && let Value::Object(object) = &mut entry
    {
        object.insert("timeout".to_owned(), json!(1234));
    }
    servers.insert(name.to_owned(), entry);
    save(file, config);
    println!("Added MCP server {name} to {scope} config CANARY_FAKE_CLAUDE_STDOUT");
    0
}

fn remove(
    directives: &Directives,
    call: &Call,
    name: &str,
    config: &mut Map<String, Value>,
    file: &Path,
) -> u8 {
    let scope = call.scope.clone();
    let servers = servers(config, &scope);
    if !servers.contains_key(name) {
        eprintln!("No MCP server named \"{name}\" in {scope} scope");
        return 1;
    }
    if directives.has("noop", "remove", name) {
        return 0;
    }
    servers.remove(name);
    save(file, config);
    println!("Removed MCP server {name} from {scope} config CANARY_FAKE_CLAUDE_STDOUT");
    0
}

fn get(directives: &Directives, name: &str, config: &mut Map<String, Value>) -> u8 {
    let names: BTreeSet<String> = servers(config, "user").keys().cloned().collect();
    if !names.contains(name) {
        eprintln!("No MCP server named \"{name}\"");
        return 1;
    }
    let text = match directives.login(name) {
        Some("connected") => CONNECTED,
        Some("needs-authentication") => NEEDS_AUTHENTICATION,
        _ => FAILED,
    };
    print!("{}", text.replace("linear", name));
    println!("CANARY_FAKE_CLAUDE_STDOUT");
    0
}
