---
name: integration-harness
description: Run real Herdr, agent CLI and Linear integration checks for herdr-linear-agent with isolated configuration, state, worktrees and a dedicated session. Use for live development verification; ordinary unit tests use World and temporary state.
---

# Integration harness

Read the repository README for configuration and the relevant lifecycle section
of `docs/behavior.md`. Read `docs/verification.md` for existing live evidence and
its limits before choosing a check.

Use only the scratch team, repository and external actions authorized for the
current task. This skill does not grant permission to create issues, publish
PRs, merge changes, change sandbox permissions or stop ordinary processes.
Reuse authorization already given in the conversation. If an ordinary ticker
also watches the scratch team, arrange its temporary stop before delegation and
restore that exact installed binary afterward.

## Run an isolated check

Run the commands below from the repository root.

Use `.agents/skills/integration-harness/scripts/dev-harness.sh` for development that starts real processes. It
requires Python 3.11 or newer and a new root outside ordinary plugin paths:

```bash
.agents/skills/integration-harness/scripts/dev-harness.sh init /tmp/hla-dev hla-dev
# Configure only scratch Linear teams, repository clones and profiles under
# /tmp/hla-dev/config/herdr-linear-agent. Keep its explicit Herdr session.
.agents/skills/integration-harness/scripts/dev-harness.sh /tmp/hla-dev herdr
# In another terminal, after the dedicated session is running:
.agents/skills/integration-harness/scripts/dev-harness.sh /tmp/hla-dev plugin action doctor
.agents/skills/integration-harness/scripts/dev-harness.sh /tmp/hla-dev plugin ticker start
.agents/skills/integration-harness/scripts/dev-harness.sh /tmp/hla-dev plugin ticker status
# Stop only these processes; retain their files as evidence:
.agents/skills/integration-harness/scripts/dev-harness.sh /tmp/hla-dev stop
```

The default binary is `target/debug/herdr-linear-agent`; set `HLA_DEV_BINARY`
to the signed release binary for real macOS credential access. `HERDR_BIN_PATH`
can select an installed Herdr executable. The wrapper sets `XDG_CONFIG_HOME`
and `XDG_STATE_HOME`, clears inherited plugin and Herdr location overrides,
and places Herdr worktrees under the harness root. Agent panes inherit these
XDG paths, so their plugin subcommands use the same isolated state. Commands
run from the root to avoid loading an unrelated checkout's mise configuration.
Use absolute paths for commands passed through `exec`. Configure
mise versions in `ROOT/config/mise/config.toml` if using shims. Initialization
creates no credentials and starts no processes. Stop retains evidence.

Agent sandbox permissions still come from profiles. Workers need access to
their worktree, its common Git directory and the isolated plugin state;
GitHub tasks also need network access. Set these explicitly in scratch profiles.
Codex may keep Git metadata read-only even with `--add-dir <ROOT>`, so test the
chosen commit method as well as startup. Answer any trust dialog for the scratch
clone before expecting work. Do not disable commit signing to work around a
sandbox limitation.

Native agent data, `HOME`, the OS credential store and GitHub authentication
remain shared. Use scratch repositories and teams only. If another ticker
watches that team, stop it before delegation and restore it afterward. Do not
copy production run or supervisor state into the harness. Its plugin config
must keep the explicit test session; default sessions and symlinked location
files are rejected. A harness is filesystem separation, not an OS sandbox.

Run `python3 .agents/skills/integration-harness/scripts/test-dev-harness.py` for offline wrapper checks. This
does not start Herdr, a ticker, agents or Linear requests.


## Evidence and cleanup

- Distinguish startup readiness from a model turn and a complete issue workflow.
  For startup only, use the explicit agent smoke check in `docs/verification.md`.
- For lifecycle checks, inspect real Linear state, Herdr requests, the worker
  report and PR, and persisted run records. Do not manufacture events, reports
  or committed state to complete a check.
- For supervisor persistence, follow the real saved postmortem into its report,
  decision, receipt and checkpoint. Check the decision actor, sequence, source
  report and hash; restart the isolated ticker and read the decision back.
  A Done issue or idle pane alone is insufficient evidence.
- Capture hashes of ordinary configuration, run and supervisor files before the
  check and compare them while its ordinary ticker is still stopped. Do not copy
  those files into the harness. Ordinary ticker logs and locks are operational
  files and are outside this comparison.
- Stop the harness, verify its named session is gone, and restore any ordinary
  ticker stopped for the check. Retain isolated evidence and record the scope,
  assistance and unresolved limits in `docs/verification.md`.

## Check changes to this skill

Run the offline script listed above and:

```bash
shellcheck .agents/skills/integration-harness/scripts/dev-harness.sh
git diff --check
```

These checks do not authorize another live integration run.
