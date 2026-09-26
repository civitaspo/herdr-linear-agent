# Verification with a real Linear workspace

The test suite runs the ticker and the coordinator's commands against a fake Herdr and an in-memory fake of the Linear API. The questions below depend on how Linear and the agent CLIs behave for real. Check them with a scratch Linear team and a scratch Herdr session before relying on the plugin, and record the results here.

| # | Question | How the plugin behaves today |
| --- | --- | --- |
| 1 | When a person delegates an issue to an app without a webhook, does Linear create an Agent Session by itself? | The plugin always creates its own session with `agentSessionCreateOnIssue` and posts a thought in the same tick. |
| 2 | Does an `elicitation` activity notify the person who delegated the issue? | A Herdr notification is also shown when `notifications.herdr` is on. |
| 3 | Can an activity created with our `id` be read back with `activities(filter: { id: { eq } })`? | A write whose response was lost is checked this way before it is sent again. |
| 4 | Does `agentSessionUpdate` accept a list for `plan`? | `plan set` sends a list of `{content, status}`. |
| 5 | How should Claude Code's trust dialog in a new worktree be handled? | The worker shows as Waiting on you and the session is told which pane needs a person. |
| 6 | Does Codex read `AGENTS.md` in a run folder that is not a git repository, and how do its approvals behave? | Untested; approvals come from the profile's `args`. |
| 7 | Does the Keychain ask for confirmation when the ticker, started by the startup hook, reads the token? After a rebuild? | Untested. |
| 8 | What is the complexity of the batched run read (`HlaRuns`) with several runs? | One request per tick for all active runs, 50 prompts per run. |
| 9 | Does a worker start building before the worktree setup plugin finishes? | The brief tells workers to set up missing dependencies themselves. |
| 10 | What does `Issue.estimate` return for a team with T-shirt estimates? | Read as the Fibonacci scale, as Linear's docs describe. |
| 11 | Where does `claude -p --output-format json --json-schema` put the schema-checked JSON? | The plugin reads `structured_output`, then `result`, then the whole output. |
| 12 | Does Linear report the granted scope with commas or spaces? | Both are accepted; the set must equal `read`, `write`, `app:assignable`. |
| 13 | Does the Secret Service store work on a Linux desktop and in a headless session (for example over SSH, where no keyring runs)? | The store needs a running Secret Service; without one, login and Linear reads fail and the doctor action says so. |
