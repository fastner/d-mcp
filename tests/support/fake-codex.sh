#!/bin/sh
# A fake `codex` for d-mcp's tests. The behaviour comes from the file
# `$CODEX_HOME/fake-codex-mode` (default `list`), and every call is recorded
# in `$CODEX_HOME/fake-codex.log` (its working directory in
# `$CODEX_HOME/fake-codex-cwd.log`), so tests need no environment variables.
# The CANARY strings stand for foreign secrets: none of them may ever reach
# d-mcp's output or store. It also echoes its arguments to stderr, which
# d-mcp must drain and drop.
#
# `--version` prints the verified version (in the mode `version-mismatch`
# an unverified one, in the mode `version-garbage` a canary), in every mode.
# d-mcp runs only `codex --version` and `codex mcp list --json`.
#
# Like the fake `claude`, it refuses to run (exit 97) unless `$CODEX_HOME`
# lies in a test root: a directory in the system temp directory that holds
# the marker `.d-mcp-test-root`, one level up (`<root>/codex`) or two
# (`$HOME/.codex` with `HOME=<root>/home`). So it never touches a real
# Codex home.
#
# Tests run it with a `PATH` that holds only the fake itself, so it uses
# shell builtins only, except `sleep`, which it calls by absolute path.
sleep_bin=/bin/sleep
[ -x "$sleep_bin" ] || sleep_bin=/usr/bin/sleep
refuse() {
  echo "fake codex refuses to run: $1" >&2
  exit 97
}
# d-mcp must pass the resolved CODEX_HOME.
[ -n "$CODEX_HOME" ] || refuse "CODEX_HOME is unset"
temp=$(cd "${TMPDIR:-/tmp}" 2>/dev/null && pwd -P) || refuse "no temp dir"
# The resolved CODEX_HOME; when it does not exist, its resolved parent.
home=$(cd "$CODEX_HOME" 2>/dev/null && pwd -P) ||
  home="$(cd "${CODEX_HOME%/*}" 2>/dev/null && pwd -P)/${CODEX_HOME##*/}" ||
  refuse "CODEX_HOME cannot be resolved"
root=
for candidate in "${home%/*}" "${home%/*/*}"; do
  if [ -n "$candidate" ] && [ -f "$candidate/.d-mcp-test-root" ]; then
    root=$candidate
    break
  fi
done
[ -n "$root" ] || refuse "CODEX_HOME is not inside a d-mcp test root"
case "$root/" in
  "$temp"/*) ;;
  *) refuse "CODEX_HOME is not inside the system temp directory" ;;
esac
mode=list
echo "called: $*" >> "$CODEX_HOME/fake-codex.log"
echo "$PWD" >> "$CODEX_HOME/fake-codex-cwd.log"
if [ -f "$CODEX_HOME/fake-codex-mode" ]; then
  IFS= read -r mode < "$CODEX_HOME/fake-codex-mode" || [ -n "$mode" ]
fi
echo "CANARY_FAKE_CODEX_STDERR" >&2
echo "args: $*" >&2
case "$*" in
  --version)
    case "$mode" in
      version-mismatch) echo "codex-cli 0.100.0" ;;
      version-garbage) echo "codex-cli CANARY_FAKE_CODEX_STDOUT" ;;
      *) echo "codex-cli 0.165.0" ;;
    esac
    exit 0
    ;;
esac
case "$mode" in
  list)
    printf '%s\n' \
      '[' \
      '  {"name": "stdio1", "transport": {"type": "stdio", "env": {"TOKEN": "CANARY_FAKE_CODEX_STDOUT"}}, "auth_status": "unsupported"},' \
      '  {"name": "oauth1", "transport": {"type": "streamable_http"}, "auth_status": "not_logged_in"},' \
      '  {"name": "oauth2", "transport": {"type": "streamable_http"}, "auth_status": "o_auth"},' \
      '  {"name": "bearer1", "transport": {"type": "streamable_http"}, "auth_status": "bearer_token"},' \
      '  {"name": "odd1", "auth_status": "unknown"}' \
      ']'
    ;;
  timeout)
    # `exec`, so the sleeping process is the direct child that is killed.
    exec "$sleep_bin" 30
    ;;
  grandchild)
    # A background grandchild inherits stdout and keeps it open after this
    # process exits.
    ("$sleep_bin" 15; echo late) &
    echo "[]"
    ;;
  oversized)
    i=0
    while [ "$i" -lt 2000 ]; do
      echo "CANARY_FAKE_CODEX_STDOUT padding padding padding padding padding"
      i=$((i + 1))
    done
    ;;
  fail)
    echo "CANARY_FAKE_CODEX_STDOUT"
    exit 3
    ;;
  garbage)
    echo "not json CANARY_FAKE_CODEX_STDOUT"
    ;;
  probe)
    # Proves that the timeout and grandchild modes really sleep under the
    # restricted PATH.
    [ -x "$sleep_bin" ] && echo "[]"
    ;;
esac
