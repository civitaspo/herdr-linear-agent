# Running live workflow integration tests

Use this guide for a real Linear, Herdr, GitHub, and agent CLI workflow check.
Keep run results in [verification.md](verification.md). The reference run is
[THLA-32](https://linear.app/civitaspo/issue/THLA-32/verify-complete-workflow-after-pr-merge-automation-adjustment),
which completed on 2026-10-13 in Japan with Herdr 0.9.3 and OpenCode 2.0.24.
Its preceding THLA-31 attempt exposed a conflicting Linear automation.

Run the plugin commands below from the development checkout. Service commands
must use the same scratch environment, even when issued from another terminal.

## Scope and evidence

The reference test used one coordinator, one worker, one sandbox repository,
and a documentation-only deployment. Declare the scope before creating the
issue and check each row against external state and persisted records.

| Item | Required evidence |
| --- | --- |
| Delegation and intake | A permitted human delegates a new scratch issue to the app; the plugin creates an active run and moves it to `In Progress`. |
| Real agents and shared space | Herdr lists the configured coordinator and worker in the same workspace. Their records refer to the assigned shared checkout and branch. The coordinator pane itself runs in the canonical run folder. |
| State-change notification | Actual Linear transitions create `[state]` inbox items with the old and new states. Check their processing through `context` while the coordinator remains active. THLA-32 retained review, deploy, and postmortem transition items; its postmortem item remained pending when the run closed. |
| Worker, PR, and CI | The worker creates the intended diff, a signed commit with the required trailer, and a PR. Actual required GitHub checks succeed before its implementation report. |
| Review | Coordinator `finish` moves the issue to `In Review`; the PR stays open until approval and the plugin merge command. |
| Allowed-user merge | `worker request-merge` saves a fresh token for the recorded PR. An allowed user's exact `merge <token>` session reply changes authorization to `approved`; `worker merge` produces the actual GitHub merge and saves `merged`. |
| Deployment | Linear reaches `In Deploy`. The existing worker fetches `origin/main`, verifies the document at the actual merge commit, and saves that result in its updated report. |
| Workflow postmortem | Deployment `finish` reaches `In Postmortem`. A real routed postmortem runs, its `workflow` JSON is saved, and its ordinary Linear comment is delivered. |
| Completion and cleanup | Linear reaches `Done` after postmortem success. The run is closed, its agents and workspace are removed, its outbox drains, and `postmortem_due` clears. |

THLA-31 additionally exercised a real GitHub merge failure, a fresh token and
approval before retry, and terminal cleanup when Linear's GitHub automation
moved directly to `Done`. Its **final** postmortem did not establish that the
**workflow** postmortem or deployment had run.

These runs did not check an application rollout, multiple repositories,
unauthorized-user rejection, stale-token rejection, crash recovery, or lost
write acknowledgements. Do not infer those results from this workflow test.
Record human guidance and permission responses: THLA-32 required a correction
to the deployment report handoff and was not an unattended autonomy check.

## 1. Prepare before delegation

1. Pick one scratch Linear workspace/team and one sandbox GitHub repository.
   Confirm the connector account, app user, permitted delegator and reply user.
   Put only those targets in the test config. Keep OAuth tokens in the existing
   Keychain or Secret Service; do not copy them into a test config or script.
2. Build the executable once. On macOS, follow
   [development-signing.md](development-signing.md) and run `mise run build-dev`
   with `HLA_SIGN_IDENTITY`. Use that exact signed release executable throughout
   the test. Run `action doctor` to confirm credential access and a known real
   Linear budget before delegating an issue. A rebuild can require another
   Keychain authorization; avoid rebuilding while diagnosing the environment.
3. Confirm the team's configured state names and types through Linear: review,
   deploy, and postmortem states must be active states; Done must be completed.
   Create missing states in the scratch team when authorized.
4. Inspect **Workflows & automations → On PR merge, move to…**. It must target
   the configured deploy state or **No action**. Record the previous setting
   and whether a change should remain after testing. `Done` closes the run
   before deployment; a queued deploy write will not reopen a terminal issue.
5. Resolve real Herdr, OpenCode, and GitHub CLI executable paths in the normal
   environment, for example with `mise which herdr` and `mise which opencode`.
   Check versions and CLI help. Avoid mise shim paths in the isolated setup.
6. Check GitHub login, Git identity, fetch access, commit signing, and local
   signature verification before assigning work. An SSH signature requires an
   existing `gpg.ssh.allowedSignersFile` to verify locally. If verification uses
   a signing wrapper, use `git -c gpg.ssh.program=/usr/bin/ssh-keygen` for the
   verification command. A temporary signer file should contain the expected
   principal and public key; supply it with
   `git -c gpg.ssh.allowedSignersFile=<file> verify-commit <commit>`.
   A successful signing command and a failed local verification are different
   observations. Check GitHub's `commit.verification` too.
7. Check whether the installed ticker also polls the scratch team. Prefer a
   team it does not poll. If overlap is necessary, temporarily pause only its
   intake immediately before delegation and record the prior pause state and
   ticker PID. `action pause` leaves existing runs running. Restore only a pause
   introduced by this test during cleanup.

## 2. Use one environment for every test process

Create a fresh temporary config/state root and a named scratch Herdr session.
Set up `$XDG_CONFIG_HOME/herdr-linear-agent/config.toml` and the required
`profiles/<name>/config.toml` and `instructions.md` files. Copy only the profiles
and their base profiles needed for this run, not an entire unrelated config.

The team settings for this test include:

```toml
[workspaces.scratch.teams.TEST]
allowed_user_ids = ["allowed-human-uuid"]
allowed_delegator_ids = ["allowed-human-uuid"]
started_state = "In Progress"
review_state = "In Review"
deploy_state = "In Deploy"
postmortem_state = "In Postmortem"
done_state = "Done"
single_space_repository = "testing"
merge_on_authorization = true
routing = "testing"

[herdr]
session = "hla-test"

[routing.testing]
coordinators = ["test-coordinator"]
workers = ["test-worker"]
postmortems = ["test-postmortem"]
```

Complete the workspace's OAuth client ID, repository path/base, and profiles
using the [README configuration](../README.md#configuration). Keep the workspace
name consistent with the existing credential account. A postmortem profile
needs `instructions.md`; an empty postmortem list cannot verify that stage.
With one candidate per role, this test does not exercise routing selection
among multiple candidates.

Capture normal config locations **before** changing XDG variables. Apply the
same environment to the ticker, Herdr server, OpenCode service, and test-driver
CLI calls. A small temporary `env.sh` wrapper ending in `exec "$@"` worked in
THLA-32. It contained paths and Git identity only, no credentials.

| Variable | Value or purpose |
| --- | --- |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | The test's temporary config/state directories. |
| `HERDR_BIN_PATH` | The absolute real Herdr executable used by the plugin. |
| `PATH` | Put real agent CLI directories ahead of mise shims. |
| `OPENCODE_CONFIG` | The existing OpenCode config file when profiles use its custom `--agent`; temporary XDG otherwise hides it. |
| `GH_CONFIG_DIR` | The existing GitHub CLI config directory; temporary XDG otherwise hides login. |
| `GIT_CONFIG_GLOBAL` | The actual normal global Git config file, if it lives under the original XDG config directory. |
| `GIT_AUTHOR_NAME`, `GIT_COMMITTER_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_EMAIL` | The intended Git identity if the scratch repository's local identity would override it. |
| `MISE_CONFIG_DIR`, `MISE_TRUSTED_CONFIG_PATHS` | Existing mise config and specifically trusted plugin/sandbox repository paths when agent shell commands still invoke mise. |

Keep `HOME` and login data locations unchanged. Do not export Linear tokens to
agents. Coordinator/worker profiles cannot use `env`; they inherit the Herdr
server environment. OpenCode shell tools run in its service environment, so
fixing only the pane or ticker environment is insufficient.

For OpenCode 2.0.24, select an unused port in the **scratch** environment before
starting its service. THLA-32 used 49375 because 49374 was already in use:

```sh
opencode service set port 49375
opencode service start
opencode service status
```

Start the scratch Herdr server with the same environment in a separate terminal:

```sh
"$HERDR_BIN_PATH" --session hla-test server
```

Then run the signed plugin with that environment:

```sh
target/release/herdr-linear-agent action doctor
target/release/herdr-linear-agent ticker run
```

Before the ticker starts, doctor may report that no scratch ticker is running;
that is separate from its credential/API result. After startup, check the
matching session and ticker, then delegate the new issue. Never stop the user's
ordinary Herdr server or OpenCode service to resolve a scratch port collision.

## 3. Drive and check the workflow

Use a new issue and a unique document filename for each attempt. The issue
should request a signed Conventional Commit, actual PR CI, and the complete
state sequence. Include literal email/trailer values in fenced code blocks:
Linear rendered inline values as links in THLA-31, and those Markdown links
were copied into a malformed commit trailer and PR title.

1. Ask for one real worker in the configured shared checkout. Require it to
   write the expected sequence without claiming later stages have occurred.
   Check the actual diff, signature and exact trailer before push/review.
2. Wait for actual required PR checks and the worker's implementation report.
   An initial empty check list is not success; check registration may lag PR
   creation. Use `gh pr checks <number> --watch` once checks are registered.
3. Let the coordinator call `finish`. Read back `In Review` in Linear and the
   persisted workflow. Send the review instruction as an allowed human's reply
   **in the Agent Session thread**, not a generic issue comment. The Personal
   Linear connector's `save_comment` with `parentId` was used in THLA-32.
4. Let the coordinator request merge authorization through the plugin. Reply
   with exactly `merge <fresh-token>` and verify the saved approving user ID.
   Let it invoke `worker merge`; the driver must not substitute `gh pr merge`.
   If the merge fails, read back GitHub state before retrying and require a new
   plugin request and fresh exact approval. THLA-31 exercised this path.
5. Verify the actual merged PR/SHA and `In Deploy`. Prompt the existing worker
   to fetch `origin/main` and check the document at that SHA. Explicitly permit
   updating its report: prohibit source changes and new commits, rather than
   saying "do not modify files", which also prevents `report.md` updates.
6. Require the worker to rewrite its report to describe the current deployment
   result, retain `PR: <same-url>`, and invoke the generated report command in
   its brief. Through `context` and the copied `workers/<id>.md`, confirm the
   deployment SHA/result and that `report_hash` equals `announced_report_hash`
   before deployment `finish`. A pane reply alone is not a registered report.
7. Verify `In Postmortem`, the real postmortem output and its delivered Linear
   comment, then `Done`. Confirm `workflow.postmortem_succeeded = true`,
   `workflow.confirmed = done`, `status = closed`, `closed_state = Done`,
   stopped agent records, no remaining run workspace, an empty
   `.state/outbox/`, and `postmortem_due = null`.

Observe actual API state history and recorded inbox items rather than treating
an agent's statement that it invoked a command as proof of delivery. Save UTC
times, issue/PR links, SHAs, check results, and any guidance in verification.md.
Keep driver observations in the run conversation before the postmortem if its
assessment should include them. GitHub normalized the squash co-author trailer
capitalization in THLA-32; inspect the actual merge message when exact trailer
spelling is a test requirement.

## 4. Known failures and recovery

| Symptom observed in these runs | Next action |
| --- | --- |
| Doctor says the credential is unavailable or waits on its lock | Confirm the exact signed executable and credential item's access controls using the development signing guide. Bound the diagnostic wait and handle the actual Keychain request. Do not repeatedly rebuild or replace the token to diagnose an access failure. |
| Agents cannot find the scratch session/state | Align XDG directories on the Herdr server as well as the ticker. A server started with normal XDG cannot provide the intended scratch environment to its panes. |
| OpenCode starts then exits, or the service port is occupied | Start a separate service under scratch config on an unused port. Do not reuse the normal service, which retains its own tool environment. |
| GitHub is signed out, signing identity is wrong, or mise refuses trust | Check the service's inherited Git/GitHub/mise environment, not just the pane's. Select real CLI paths and preserve the intended config and identity. |
| Repeated external-directory prompts for the plugin executable | Inspect the actual prompt. When already authorized, accept its one-time option without asking again for the same test operation. THLA-32 continued with inspected `Allow once` responses after automatic approval review rejected persistent `Always allow`; do not bypass that rejection. |
| The coordinator stopped after a startup failure | Use the plugin's exact session reply `resume` when it requests that action. A longer explanatory reply containing the word is not the same command. |
| Literal commit/PR metadata becomes Markdown links | Put literal values in a fenced block and inspect the final commit message and PR title. Correct unmerged work without rewriting previously merged commits. |
| GitHub merge returns an error | Read back the PR and persisted authorization. If it is open and authorization failed, request a fresh token and reply before another plugin merge. |
| Merge moves Linear directly to Done | Record the attempt as incomplete, inspect the team's PR-merge automation, and use a new issue after correcting it. A final postmortem is not a completed workflow postmortem. |
| Worker says deployment passed but the coordinator waits | Check the actual `report.md`, copied report, and hashes. Prompt the worker to persist its result and register the report before finishing. |

## 5. Cleanup on success or failure

1. Save the evidence and settle the test issue to its intended terminal state.
   Wait for cleanup and queued writes; do not claim a pass for unfinished stages.
2. Under the scratch environment, stop only the test processes:

   ```sh
   target/release/herdr-linear-agent ticker stop
   opencode service stop
   "$HERDR_BIN_PATH" session stop hla-test
   ```

   Verify `ticker status`, `opencode service status`, and Herdr's session list.
3. Restore any intake pause introduced by the test. With a different development
   binary, `action resume` can start/replace a ticker; to preserve the already
   running installed ticker, remove only the exact pause marker this test
   created in its normal state directory. Verify the original PID is alive and
   the marker is absent. Leave a pre-existing pause intact.
4. Restore temporary Git settings using the recorded previous values. Repository
   local config is shared across worktrees: THLA-32 left an
   `allowedSignersFile` pointing to a temporary file until the documentation
   review removed that exact value. Prefer per-command `git -c` for verification.
5. Restore or retain scratch Linear automation changes as agreed. Keep completed
   issue/PR links and run records as evidence; list retained scratch artifacts.
   Update verification.md with limits, interventions, and cleanup results.
