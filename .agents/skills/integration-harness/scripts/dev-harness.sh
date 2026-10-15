#!/bin/sh
# All plugin commands and Herdr children share these isolated XDG directories.
set -eu
umask 077

usage() {
  echo "Usage: $0 init ROOT SESSION | ROOT {plugin|herdr|exec|stop} [ARGS...]" >&2
  exit 2
}

[ "$#" -ge 2 ] || usage
python=$(python3 -c 'import sys; print(sys.executable)')
if [ "$1" = init ]; then
  [ "$#" -eq 3 ] || usage
  root=$2
  session=$3
  case "$session" in ''|default|*[!a-z0-9_-]*) echo "Use a non-default test session name" >&2; exit 2 ;; esac
  [ ! -e "$root" ] && [ ! -L "$root" ] || { echo "ROOT must be new" >&2; exit 2; }
  # Resolve parents before checking overlap, including /tmp -> /private/tmp.
  root=$("$python" - "$root" <<'PY'
import os, pathlib, sys
root = pathlib.Path(sys.argv[1]).resolve()
home = pathlib.Path.home()
for var, fallback in [('XDG_CONFIG_HOME', '.config'), ('XDG_STATE_HOME', '.local/state')]:
    base = pathlib.Path(os.environ.get(var, ''))
    base = base if base.is_absolute() else home / fallback
    protected = (base / 'herdr-linear-agent').resolve()
    if root == protected or root in protected.parents or protected in root.parents:
        sys.exit('ROOT overlaps ordinary plugin configuration or state')
print(root)
PY
  )
  mkdir -p "$root/config/herdr-linear-agent" "$root/config/herdr" "$root/state/herdr-linear-agent"
  printf '%s\n' "$session" > "$root/.hla-dev-session"
  printf '[herdr]\nsession = "%s"\n' "$session" > "$root/config/herdr-linear-agent/config.toml"
  "$python" - "$root" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
(root / 'config/herdr/config.toml').write_text('[worktrees]\ndirectory = ' + json.dumps(str(root / 'worktrees')) + '\n')
PY
  if [ "${HERDR_ENV:-}" = 1 ]; then
    printf '\n[experimental]\nallow_nested = true\n' >> "$root/config/herdr/config.toml"
  fi
  echo "Created $root; configure only scratch workspaces/teams in config/herdr-linear-agent."
  exit 0
fi

root=$(cd "$1" && pwd -P)
shift
[ -f "$root/.hla-dev-session" ] && [ ! -L "$root/.hla-dev-session" ] || usage
session=$(cat "$root/.hla-dev-session")
case "$session" in ''|default|*[!a-z0-9_-]*) usage ;; esac
for dir in config state config/herdr config/herdr-linear-agent state/herdr-linear-agent; do
  [ -d "$root/$dir" ] && [ ! -L "$root/$dir" ] || { echo "Unsafe harness directory: $dir" >&2; exit 2; }
done
for path in config/herdr/config.toml config/herdr-linear-agent/config.toml worktrees; do
  [ ! -L "$root/$path" ] || { echo "Unsafe harness symlink: $path" >&2; exit 2; }
done
# gh authentication is shared deliberately; never copy tokens into the harness.
export GH_CONFIG_DIR="${GH_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/gh}"
export XDG_CONFIG_HOME="$root/config" XDG_STATE_HOME="$root/state"
unset HERDR_PLUGIN_CONFIG_DIR HERDR_PLUGIN_STATE_DIR HERDR_CONFIG_PATH HERDR_SOCKET_PATH
unset HERDR_WORKSPACE_ID HERDR_TAB_ID HERDR_PANE_ID
herdr=${HERDR_BIN_PATH:-herdr}
binary=${HLA_DEV_BINARY:-$(CDPATH='' cd -- "$(dirname -- "$0")/../../../.." && pwd)/target/debug/herdr-linear-agent}
command=$1
shift
# Avoid loading a checkout's mise configuration with isolated trust settings.
cd "$root"
# Child agents resolve the config through XDG, without this wrapper.
if [ "$command" != stop ]; then
  "$python" - "$root" "$session" <<'PY'
import pathlib, sys, tomllib
root = pathlib.Path(sys.argv[1])
with (root / 'config/herdr-linear-agent/config.toml').open('rb') as f:
    config = tomllib.load(f)
if config.get('herdr', {}).get('session') != sys.argv[2]:
    sys.exit('Config must name the harness session explicitly')
with (root / 'config/herdr/config.toml').open('rb') as f:
    herdr = tomllib.load(f)
if herdr.get('worktrees', {}).get('directory') != str(root / 'worktrees'):
    sys.exit('Herdr worktrees must remain inside the harness')
PY
fi
case "$command" in
  plugin|stop)
    if [ "$command" = plugin ]; then
      exec "$binary" "$@"
    fi
    # Retain evidence; only the ticker and session belonging to ROOT are stopped.
    failed=0
    "$binary" ticker stop || failed=1
    "$herdr" session stop "$session" --json || failed=1
    "$herdr" session delete "$session" --json || failed=1
    exit "$failed"
    ;;
  herdr) exec "$herdr" --session "$session" "$@" ;;
  exec) [ "$#" -gt 0 ] || usage; exec "$@" ;;
  *) usage ;;
esac
