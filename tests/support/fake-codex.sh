#!/bin/sh
# A fake `codex` for d-mcp's tests. The behaviour comes from the file
# `$CODEX_HOME/fake-codex-mode` (default `list`), and every call is recorded
# in `$CODEX_HOME/fake-codex.log`, so tests need no environment variables.
# The CANARY strings stand for foreign secrets: none of them may ever reach
# d-mcp's output or store.
#
# Tests run it with a `PATH` that holds only the fake itself, so it uses
# shell builtins only, except `sleep`, which it calls by absolute path.
sleep_bin=/bin/sleep
[ -x "$sleep_bin" ] || sleep_bin=/usr/bin/sleep
mode=list
if [ -n "$CODEX_HOME" ]; then
  echo "called: $*" >> "$CODEX_HOME/fake-codex.log"
  if [ -f "$CODEX_HOME/fake-codex-mode" ]; then
    IFS= read -r mode < "$CODEX_HOME/fake-codex-mode" || [ -n "$mode" ]
  fi
fi
echo "CANARY_FAKE_CODEX_STDERR" >&2
case "$mode" in
  list)
    # d-mcp must pass the resolved CODEX_HOME.
    [ -n "$CODEX_HOME" ] || exit 7
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
