#!/usr/bin/env python3
"""Offline isolation checks; no Herdr, agents, Linear, or credentials."""
import json
import os
from pathlib import Path
import subprocess
import tempfile


with tempfile.TemporaryDirectory(prefix="hla-harness-check-") as tmp:
    base = Path(tmp).resolve()
    script = Path(__file__).with_name("dev-harness.sh").resolve()
    ordinary = base / "ordinary"
    (base / "bin").mkdir()
    (base / "bin/python3").symlink_to(os.sys.executable)
    env = dict(os.environ, XDG_CONFIG_HOME=str(ordinary / "config"),
               XDG_STATE_HOME=str(ordinary / "state"), HERDR_ENV="1",
               PATH=str(base / "bin") + os.pathsep + os.environ["PATH"],
               HERDR_PLUGIN_STATE_DIR="must-be-cleared", HERDR_SOCKET_PATH="must-be-cleared")
    env.pop("GH_CONFIG_DIR", None)

    def run(*args, ok=True):
        result = subprocess.run([str(script), *map(str, args)], env=env,
                                text=True, capture_output=True)
        assert (result.returncode == 0) == ok, result.stderr + result.stdout
        return result.stdout

    run("init", base / "bad-session", "default", ok=False)
    run("init", ordinary / "config/herdr-linear-agent/test", "scratch", ok=False)
    root = base / "scratch"
    run("init", root, "scratch-check")
    run("init", root, "scratch-check", ok=False)
    assert (root / "state/herdr-linear-agent").stat().st_mode & 0o777 == 0o700
    assert str(root / "worktrees") in (root / "config/herdr/config.toml").read_text()
    output = run(root, "exec", os.sys.executable, "-c",
                 "import json,os; keys=['XDG_CONFIG_HOME','XDG_STATE_HOME','GH_CONFIG_DIR',"
                 "'HERDR_PLUGIN_STATE_DIR','HERDR_SOCKET_PATH']; "
                 "print(json.dumps({k:os.environ[k] for k in keys if k in os.environ}))")
    child = json.loads(output)
    assert child["XDG_CONFIG_HOME"] == str(root / "config")
    assert child["XDG_STATE_HOME"] == str(root / "state")
    assert child["GH_CONFIG_DIR"] == str(ordinary / "config/gh")
    assert "HERDR_PLUGIN_STATE_DIR" not in child and "HERDR_SOCKET_PATH" not in child
    assert run(root, "exec", "pwd").strip() == str(root)
    fake = base / "fake"
    fake.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$HARNESS_CHECK_LOG"\n')
    fake.chmod(0o700)
    env.update(HLA_DEV_BINARY=str(fake), HERDR_BIN_PATH=str(fake), HARNESS_CHECK_LOG=str(base / "calls"))
    run(root, "herdr", "agent", "list")
    run(root, "plugin", "ticker", "status")
    run(root, "stop")
    assert (base / "calls").read_text().splitlines() == [
        "--session scratch-check agent list", "ticker status", "ticker stop",
        "session stop scratch-check --json", "session delete scratch-check --json"]
    assert (root / ".hla-dev-session").exists()  # Evidence survives stop.
    (root / "config/herdr/config.toml").write_text('[worktrees]\ndirectory = "/outside"\n')
    run(root, "herdr", "agent", "list", ok=False)
    (root / "config/herdr/config.toml").write_text('[worktrees]\ndirectory = ' + json.dumps(str(root / "worktrees")) + '\n')
    (root / "config/herdr-linear-agent/config.toml").write_text('[herdr]\nsession = "default"\n')
    run(root, "plugin", "ticker", "start", ok=False)
    run(root, "stop")  # The marker still identifies cleanup after config damage.
    (root / "state/herdr-linear-agent").rmdir()
    (root / "state/herdr-linear-agent").symlink_to(ordinary)
    run(root, "exec", "true", ok=False)
    assert not ordinary.exists()
print("dev-harness offline checks passed")
