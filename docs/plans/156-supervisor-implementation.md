# Implement the postmortem supervisor

Add one opt-in supervisor agent that learns across Linear runs.
Keep each issue's existing postmortem comment and labels.
Feed saved postmortems into the supervisor without requiring people to transfer them.
Let the supervisor add, revise, and retire generated instruction profiles within an explicit routing policy.
Deliver one focused pull request, P1, with three internally verified implementation stages.
Track the user-facing goal in [GitHub Issue #156](https://github.com/civitaspo/herdr-linear-agent/issues/156).
This document is the implementation plan, refreshed on 2026-10-15 against current `origin/main` commit `58298436ea73b1b510c6dfee68f6d4280d9c97bc` on branch `feat/postmortem-supervisor`.

## Implementation checkpoint (2026-10-15)

The implementation is in a signed local commit on `feat/postmortem-supervisor`. The locked Rust suite passed 467 tests; `routing_live` was ignored. Direct `cargo check`, Clippy with warnings denied, `cargo build`, formatting, diff, shell and workflow checks passed. The `mise run lint`, `mise run test`, and `mise run build` wrappers could not start because mise attempted to install `aqua:DataDog/pup@latest` and the environment returned `Operation not permitted`.

PR #157 is open against `main`. Its new rebased head still needs to be pushed and checked by GitHub. An exact-head review found and helped close a replay window for postmortem comments; the regression now covers queue replay, an uncertain response and replay after queue removal. Review of the final signed head and remote checks are pending. Live Herdr/Linear/agent lanes and performance measurements remain unverified; this feature is not called fully verified.

## How to read this

One box is one unit of work. Every box names the evidence that checks it. Check a box only when its evidence exists. Evidence can be a persisted record, a command transcript, a test result, or a commit SHA.

Use `poteto-mode/playbooks/autopilot-stack.md` when that workflow is installed, adapted to one local owner and one pull request. This document records the complete feature contract for a successor without that workflow. The operator reviews and merges P1.

Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

The user has now requested implementation after changing models. Keep this document current as the three implementation stages complete. The operator reviews and merges the resulting pull request.

## Program checklist

### Arm the program

- [x] Create an English Issue containing the overview and core value. Evidence is [Issue #156](https://github.com/civitaspo/herdr-linear-agent/issues/156), created and read back on 2026-10-15.
- [x] The user requested implementation after changing models. Branch `feat/postmortem-supervisor` tracks current `origin/main`; the scope is tracked in [Issue #156](https://github.com/civitaspo/herdr-linear-agent/issues/156).
- [x] Skip `/goal` and recurring scheduling unless the user explicitly requests them. This is a bounded implementation and no recurring schedule is needed.
- [x] Read the current repository instructions and named skill files at execution start. `git show origin/main:AGENTS.md` matches the supplied instructions. The rebased source base is `origin/main` commit `58298436ea73b1b510c6dfee68f6d4280d9c97bc`.

### Spawn owners

- [ ] Assign P1 to the primary agent. The report feed, lifecycle, and generated catalog are sequential stages of the same feature, not independent implementations.
- [ ] Use bounded read-only delegates where required by the invoked skills. Missing model configuration means inherit the parent model. Record that this is a same-model review.

### PR mechanics, for every PR

- [ ] Resolve the forge. Use `gh` unless an installed Origin CLI can resolve this repository.
- [ ] Create signed commits with the exact `Co-Authored-By: Codex <noreply@openai.com>` trailer. Verify signatures and trailers before pushing.
- [ ] Run `mise run lint`, `mise run test`, `mise run build`, and `git diff --check` before opening P1.
- [ ] Open P1 against `main` with a Conventional Commit title and an English body that links the approved Issue. Keep `CHANGELOG.md` unchanged.

### Verdict and merge, for every PR

- [ ] Obtain an independent review of the final head, including crash recovery, generated profile authorization, and compatibility with old records. Record the exact reviewed SHA.
- [ ] Triage CI and review findings on their merits. Re-run affected checks after fixes.
- [ ] Stop with a reviewable PR. The operator owns the merge.

### Boot recipe, for every live lane

- [ ] Build the approved head and use isolated XDG config/state directories, a scratch Herdr session, and a scratch repository. Preserve the user's running plugin and manual profile files.
- [ ] Drive the actual plugin CLI and actual Herdr panes. Use fixture reports for failure injection. Mark fixture checks separately from real agent and Linear checks.
- [ ] Use a logged-in agent CLI for the supervisor's real context, decision, and resume path. Follow `docs/development-signing.md` for a local macOS binary that accesses real Linear credentials.
- [ ] Save each lane's command log, relevant JSON records, and named screenshot under the task's evidence directory. A screenshot supplements command/state evidence.
- [ ] If a real agent, scratch Linear workspace, or screenshot mechanism is unavailable, report that lane as unverified. Do not replace its evidence with a fake result.

## Implement the report feed, supervisor, and generated catalog (P1)

**Depends on.** None.

**Files.**

- [x] Extend `src/config.rs`, `src/paths.rs`, `src/cli.rs`, `src/commands.rs`, and `src/actions.rs` for opt-in policy and supervisor commands.
- [x] Add focused `src/supervisor.rs` and `src/ticker/supervisor.rs` modules, with local memory retrieval under their ownership. Wire them from the existing module owners.
- [x] Update `Cargo.toml` and `Cargo.lock` for SQLite FTS5 retrieval. Use ai-memory as a reference implementation without importing its code.
- [x] Extend `src/postmortem.rs`, `src/ticker/postmortems.rs`, `src/ticker/reconcile.rs`, `src/ticker/mod.rs`, and the relevant launch/intake code.
- [x] Extend `src/run.rs`, `src/worker.rs`, `src/coordinator.rs`, and `src/routing.rs` only where catalog pinning, attribution, and candidate selection require it.
- [x] Add `assets/SUPERVISOR.md`. Keep agent command sheets and generated allow-lists consistent.
- [x] Update `README.md`, `docs/behavior.md`, and `docs/verification.md`. Add regression scenarios in `src/ticker/scenarios.rs` using the existing World controls.

**Build.**

- [x] Stage 1. Define `SupervisorReport`, `SupervisorRecord`, `SupervisorDecision`, `SupervisorMemoryUpdate`, `SupervisorCheckpoint`, and generated profile revision metadata before wiring behavior. Use deserialization defaults for old persisted records.
- [x] Keep memory inside the plugin with local CLI access. Store reusable knowledge as Markdown bodies with typed JSON metadata and keep a rebuildable SQLite FTS5 index. Implement the small required queries directly against the database API. Use ai-memory as a reference without copying its implementation. Follow Appendices F and G for generation catch-up and local access boundaries.
- [x] Keep the postmortem agent's existing `summary` and `labels` answer. Extend the saved outcome with binary-derived routing, profile revisions, and available run observations. Persist failed attempts as typed diagnostic artifacts before clearing the due stage. Preserve the existing no-retry and no-Linear-comment behavior for failures. Do not fabricate a postmortem summary or add another agent call.
- [x] Derive a durable supervisor report feed from both successful saved outcomes and failed-attempt diagnostic artifacts. Identify a report by workspace-qualified run key and immutable outcome artifact identity. Preserve distinct interim/final outcomes and reopened-run history.
- [x] Rebuild missing feed entries from saved outcomes and diagnostic artifacts on startup and after relevant effects. Use an incremental cursor or bounded batches rather than rescanning all history on every reconciliation pass. An interruption between outcome persistence and feed materialization must lose no report.
- [x] Keep Linear outbox delivery independent. A supervisor failure must not prevent the issue comment or labels from being delivered. A Linear delivery failure must not erase the saved supervisor report.
- [x] Add bounded `supervisor context` and `supervisor decide --decision-file <path>` commands. Context returns unseen report IDs, recent decisions, active variants, and available observations. Record the shown IDs. Decisions acknowledge only shown report IDs after the decision and any publication are durable.
- [x] Require a portable memory delta and checkpoint on every completed supervisor batch, including no-change decisions. Use Markdown for knowledge content and typed JSON for identity, evidence, scope, revision references, and control state. Reuse immutable decision artifacts as the canonical episode history. Keep topic knowledge and indexes as reconstructible projections. Save observed facts, concise lessons, evidence, unresolved work, and intended follow-up checks.
- [x] Commit the decision, memory generation, and any profile registry update through one authoritative manifest pointer before acknowledging reports. Stage immutable files first. Failed validation or memory persistence commits none of these effects. Readers use only manifest-referenced data. Replaying a prepared or partially materialized decision must converge.
- [x] Expose bounded memory lookup in `supervisor context` and `supervisor memory search`. Filter by current workspace/team authorization, routing, topic, and profile revision before rendering. Return compact records and short natural-language summaries, with explicit handles for full source details.
- [x] Make decision IDs and profile publication idempotent. Replaying after a crash must converge without duplicate profile revisions or lost acknowledgements. Rebuild derived acknowledgement state from committed decisions where necessary.
- [x] Verify report identity, recovery, Linear independence, and old-record parsing before stage 2.
- [x] Stage 2. Start exactly one supervisor in a dedicated Herdr workspace labeled exactly `herdr-linear-agent-supervisor` under the state directory when `[supervisor]` is configured. An ordinary human-managed profile controls its agent kind, model, arguments, and method.
- [x] Reconcile lifecycle against the existing pass's one Herdr snapshot. Persist native session and placement identity. Adopt an existing matching pane after reload or restart. Use complete snapshots for disappearance decisions and the existing bounded recovery policy.
- [x] Reserve one slot under `limits.max_agents` while the supervisor is enabled. Validate that the configured total still permits one coordinator and one worker. Include the reservation in both intake and worker-start accounting.
- [x] Wake an idle supervisor with a fixed message when unseen reports arrive or a policy/base change requires reconsidering variant eligibility. Busy panes keep reports queued. A context call reads the durable feed. End the turn when there is no work. Bound redelivery and resume attempts so missing decisions cannot create an endless prompt loop.
- [x] On ordinary config reload, preserve and adopt the supervisor only if its resolved profile fingerprint and Herdr session are unchanged. For a changed supervisor profile, persist a bounded drain and pending replacement as specified in Appendix J. Stop issuing new batches, accept completion of the already issued authorized batch, and await its durable decision/checkpoint and idle turn before closing the old agent. On timeout, fence the old mutation authority before interruption and close, then redeliver uncommitted reports. Start a fresh native session with the same exact dedicated space label and stable working directory only after old placement cleanup is confirmed. Reuse placement only if supported; otherwise close and recreate the managed workspace. Bootstrap from committed portable memory rather than native-session history. On disable, Herdr-session change, authorization revocation or ticker stop, apply the bounded lifecycle rules in Appendix J. Close only the supervisor workspace. Preserve drafts and committed decisions. If Herdr is unreachable or the ticker is killed, reconcile the orphan on the next startup.
- [x] Require a current ticker/supervisor generation for mutation commands. An orphaned supervisor cannot publish after its management generation expires.
- [x] Verify duplicate prevention, slot limits, idle behavior, resume, and stop/reload distinctions before stage 3. `ticker::scenarios::supervisor_starts_once_adopts_on_restart_and_replaces_a_changed_profile`, `supervisor_reserves_an_agent_slot_from_issue_intake`, `supervisor_waits_for_a_busy_agent_before_waking_for_a_report`, and `disabling_supervisor_closes_only_its_managed_workspace` cover these paths.
- [x] Stage 3. Publish variants under `<state_dir>/supervisor/generated-profiles/`. Keep immutable revisions and stage each registry manifest using `src/files.rs`. Activate it only through the authoritative supervisor commit pointer shared with the decision and memory generation. Do not introduce an independent registry activation point. Separate agent-authored drafts from the plugin-published catalog.
- [x] Require per-routing opt-in and an allow-list of manual base profiles for each role. Generated variants use an isolated name namespace. Add them only to that routing's authorized candidate lists. Require a routing agent when generated coordinator or postmortem variants can create multiple candidates.
- [x] Permit the supervisor to change the variant's description and `instructions.md`, or choose another allowed base for that role. Inherit kind, model, effort, arguments, environment, and timeout from the approved base. Reject payloads that try to override those fields.
- [x] Validate role, routing, base, evidence references, names, collisions, sizes, paths, and symlinks at the publication boundary. Reject a stale registry generation. Preserve the last valid registry when a candidate is invalid.
- [x] Pin the generated candidate catalog for a run and the fully resolved generated revision used by each agent, including the manual base identity, content hash, inherited instruction layers, kind, model, effort, arguments, environment, and timeout. Use the pin in routing, coordinator context, worker-start authorization, launches, restarts, resumes, and postmortems. Manual profile reload behavior retains its existing semantics.
- [x] Recompute eligibility on manual config reload. A changed base hash removes its stale variants from new-run candidates until republished against the current base. Human removal of a role/base authorization blocks new generated agent launches, even from an older catalog pin, and shows an explicit recovery reason. Ordinary generated retirement preserves existing-run use.
- [x] Apply validated generated publications through a shared effective-catalog path for the ticker and short-lived commands. Do not trigger a full ticker reload or reread Linear credentials for each generated change.
- [x] Retire a generated profile by removing it from future candidate lists. Keep revisions referenced by existing runs. Support list, show, retire, and rollback operator commands with evidence and source-report links. Defer automatic history deletion.
- [x] Provide a bounded export helper for one explicit generated revision into an arbitrary operator-specified directory. Write the ordinary profile files directly under that destination without assuming a `profiles/` layout, Git checkout or deployment system. Preserve an unchanged manual base and only the generated instruction delta; verify the base fingerprint against the loaded plugin configuration and effective configuration/instruction equivalence before export. Refuse destination overwrite or a changed/missing source base. Include readable provenance without exporting raw reports or transcripts. Export neither changes routing nor retires the source. Document declarative review, deployment, reload and retirement as separate operator steps in Appendix I.
- [x] Bound active generated variants to five per routing by default. Process a bounded report batch and allow at most one catalog change per supervisor decision. Treat these as proposed defaults, configurable within the human-owned policy.
- [x] Write the supervisor's method to group recurring failures, link concrete evidence, compare later runs against the intended change, revise or retire variants, and record a no-change decision when evidence is weak. Count interim/final reports for one execution as one observation.
- [x] Record the observed profile revision, restarts, waiting episodes, worker reports, closure state, and elapsed time when available. Mark unavailable observations as unknown. Do not infer success from Done, failure from Canceled, or correctness from a postmortem label.
- [ ] Validate the full feature and update documentation after stage 3. Keep the decision trail local and link it from the completion report.
**You see.**

- [ ] Enabling the supervisor starts one workspace named exactly `herdr-linear-agent-supervisor`. An unchanged reload preserves its session. A changed supervisor profile starts a fresh native session that recovers committed memory without duplicate work.
- [ ] A postmortem remains a comment on its issue and also appears as an unseen report in `supervisor context`.
- [ ] A committed supervisor decision shows its source reports, reason, intended outcome, and generated profile change or explicit no-change result.
- [ ] A generated variant becomes eligible for a future authorized run. A run already using an older revision still starts and resumes with that revision after update or retirement.
- [ ] Status and doctor show supervisor state, pending profile replacement and drain deadline, blocked/recovery status, backlog, registry version, and invalid generated catalog errors without hiding ordinary runs. Distinguish a reload request being accepted from supervisor replacement being complete.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Add affected-module tests for policy validation, namespace collisions, base inheritance, path/symlink rejection, malformed/stale proposals, registry commit recovery, idempotent decisions, portable memory schema, projection rebuild, superseded lessons, authorization-filtered retrieval, memory-write failure before acknowledgement, profile-swap bootstrap, plain-text query edge cases, index-generation lag, and legacy defaults. Use locked Cargo commands.
- [ ] Add World scenarios for duplicate and reordered reports, interim/final observations, reopened runs, independently failed Linear delivery, supervisor absence/busy state, bounded wakes, restart adoption, partial snapshots, shutdown, slot limits, and pinned-profile worker start/resume after retirement. Exercise graceful drain completion, deadline expiry, crash/reload during drain, unchanged supervisor configuration, repeated replacement requests without deadline extension, immediate authorization revocation, and no overlap between old and replacement sessions.
- [ ] Run `mise run lint`, `mise run test`, `mise run build`, and `git diff --check`. Keep the resulting logs.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. The upstream template names its default "Ten lanes on `grok-4.6-fast-xhigh` at the PR head". The installed runtime overrides that model default with inherit-parent. Respect the user-selected implementation model and configured review models. The ten lanes below are scenario definitions. Run them locally within available capacity. Do not treat lane count as a requirement to spawn ten agents.

- [ ] Lane 1. Regression lane against trunk. Exercise finish and final closure on trunk and head. Trunk lacks a supervisor. Gate unchanged issue comments plus head's durable report and one supervisor decision. Save `p1-regression.png`. Pass when existing delivery succeeds and the new report is acknowledged only after a durable decision.
- [ ] Lane 2. Launch twice and reload with the same config. Save `p1-singleton.png`. Pass when one matching supervisor exists with the exact required space label, its native session survives unchanged reload, and a profile change uses a fresh session that reconstructs the same committed knowledge and pending work.
- [ ] Lane 3. Deliver reports while the real supervisor agent is busy, then idle. Save `p1-busy.png`. Pass when all reports are eventually visible and no prompt interrupts the busy turn.
- [ ] Lane 4. Leave the system without new reports after acknowledgement. Save `p1-idle.png`. Pass when no repeated agent prompt or new model invocation occurs for ten minutes.
- [ ] Lane 5. Interrupt after saved outcome creation and before feed creation, then after publication and before acknowledgement. Save `p1-recovery.png`. Pass when restart recovers the report and converges to one committed catalog change with the matching memory generation. Inject memory-write failure and a missing derived index. Verify no early acknowledgement and successful projection rebuild.
- [ ] Lane 6. Gracefully stop, restart, disable, and reload into another scratch Herdr session. Save `p1-stop.png`. Pass when old managed workspaces close where reachable and the new scope has one supervisor.
- [ ] Lane 7. Publish a variant and route a new run to it. Save `p1-routing.png`. Pass when the router sees only authorized variants and actual agent instructions contain the selected revision.
- [ ] Lane 8. Update and retire a profile while an older run has a pending worker, then start and resume that worker. Save `p1-pinned.png`. Pass when the older run retains its exact resolved revision and new runs exclude the retired variant.
- [ ] Lane 9. Submit an invalid base, forbidden field, collision, stale generation, traversal, and symlink. Save `p1-rejected.png`. Pass when each fails without modifying the valid registry or manual profile files.
- [ ] Lane 10. Produce a real postmortem in the scratch Linear team and let the real supervisor consume it and publish or record no change. Save `p1-linear.png`. Pass when the real issue comment, saved report, durable decision, and resulting catalog status agree. This lane needs real Linear and agent logins.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Measure reconciliation elapsed time, Herdr snapshot requests per pass, prompts per idle interval, and report/candidate publication processing time. Exclude agent inference and Linear latency from the publication processing metric.
- [ ] Probe. Run interleaved trunk/head passes with the same fake external inputs at zero and 1,000 saved postmortems. Separately time head's report indexing, registry commit, and next candidate lookup. Keep the raw samples and workload description.
- [ ] Baseline. Record trunk timings before head measurements. Trunk has no supervisor, so do not claim a supervisor throughput ratio. Record the existing one-snapshot behavior and idle prompt count.
- [ ] Rule. Fail if a pass adds another snapshot, an acknowledged idle supervisor gets any repeated prompt, or a normal pass scans the full report history. Proposed local processing budget is a p95 increase below 50 ms per steady-state pass and below 100 ms per publication/candidate lookup with 1,000 report artifacts. Report cold-start indexing separately. These are acceptance targets, not measured results.

**Review gate.** The operator reviews before merge.

- [ ] Show the operator a screenshot of the supervisor pane and catalog status, plus a 30 to 60 second video or terminal recording of report consumption, publication, and retirement.
- [ ] Link verification logs and explicitly identify any real-integration or performance lane that remains unverified.

**Merge.**

- [ ] Confirm the final head's CI, independent review, signed commits, and Codex trailers.
- [ ] Hand P1 and the evidence to the operator. Do not merge or arm auto-merge.

## Close the program

- [ ] Deliver the Issue URL, P1 URL, implementation summary, and validation limits in Japanese. Keep the Issue, PR, code comments, documents, and workflow text in English.
- [ ] Preserve unresolved integration gaps explicitly. Do not call the feature fully verified if a required lane did not run.

## Appendix A. Prototype evidence

At the planning handoff, no feature implementation, executable prototype, feature test, or live integration check had been performed. The implementation and subsequent evidence are recorded in the checkpoint and `docs/verification.md`.
Source reads at `7c05133eacbc1b20123569664e871dd566d1a091` established current behavior.
The postmortem outcome is saved and Linear writes are queued in `src/ticker/postmortems.rs`.
The answer schema is `summary` plus `labels` in `src/postmortem.rs`.
`src/config.rs` loads only user-managed profile folders and requires explicit routing candidates.
`src/ticker/mod.rs` restarts service tasks on reload while retaining the ticker lock.
`src/commands.rs` loads config in short-lived processes, so ticker-only catalog updates would be inconsistent.
Real supervisor wake, native-session recovery, CLI permissions, and new catalog timing remain unproven until implementation and live checks.

## Appendix B. Alternatives rejected

A short-lived aggregation call reuses headless routing but does not provide the user's persistent supervisor pane or native session.
Letting the supervisor edit manual profile folders or `config.toml` conflicts with potentially tracked user configuration.
Automatically activating a generated folder without routing integration produces unused profiles.
Rerunning postmortems solely to report to the supervisor adds model cost and changes existing failure behavior.
An external memory service, message broker, periodic agent polling loop, or automatic skill installer is unnecessary for this plugin-only workflow. A rebuildable SQLite FTS5 index is selected for local retrieval; it does not own lifecycle decisions or report acknowledgement.
A recommendations-only first release would defer the requested automatic profile management. The plan instead verifies delivery and lifecycle first, then enables publication only after validation, pinning, and rollback exist in the same PR.

## Appendix C. Risks

Profile instructions are executable guidance to future agents. Validate publication structurally and retain provenance, but do not claim arbitrary Markdown can be proven safe.
The existing CLI allow-lists are guidance, not an OS sandbox. A supervisor inherits its manual profile's actual permissions. Plugin mutation commands enforce policy, but they cannot revoke the shell's filesystem authority. Recommend a supervisor profile restricted to its state workspace where the CLI supports that restriction.
A killed ticker cannot immediately close a surviving pane. Expire mutation authority and adopt or clean the orphan when connectivity and management return.
Many variants can dilute routing choices. Bound active variants, preserve a static fallback, and require evidence-backed retirement decisions.
An interim and a final review are correlated observations of the same execution. Count them once and retain their different stages.
Changes in repositories, issue difficulty, human choices, models, or manual bases confound comparisons. Show counts and unknowns. Do not claim causal improvement from a small sample.
A removed allowed base or revoked routing policy blocks new publication, selection, launches, and resumes that require the revoked authorization. An already running agent keeps its process until stopped through the normal lifecycle. Ordinary generated retirement allows existing runs to continue with their fully resolved pin. A manual base edit excludes stale variants from new-run selection but keeps existing authorized pins.
Do not silently expose reports from removed or non-enabled teams through a shared routing. Resolve authorization by workspace/team and routing policy. Legacy records with unknown attribution may be shown as historical evidence but cannot justify automatic publication.
Automatic revision deletion and learned model/effort/permission changes are outside P1. Keep retained revisions until a separate cleanup design can prove they are unreferenced.

## Appendix D. Links and reading list

Read `README.md`, the config/reload, reconciliation, launch/recovery, limits, and postmortem sections of `docs/behavior.md`, and `docs/verification.md` before editing.
Read `docs/development-signing.md` before running a development binary against real macOS credentials.
Read `assets/COORDINATOR.md` and `assets/WORKER.md` before adding the supervisor's sheet and command allow-lists.
Use the installed pstack how, architect, unslop, technical-writing, no-comments, and applicable independent-review skills during implementation. Read each leaf before claiming its principles.
Use the agreed decisions in the appendices as the design record. Save future execution receipts in a task evidence directory and record its actual path in the PR. The earlier temporary planning directory is not needed to implement this feature.
The source article is [Self-improving software is inevitable](https://boristane.com/blog/self-improving-software-is-inevitable/). It motivates feeding observed results back into changes and defining what better means. It supplies no integration proof for this repository.

Proposed human-owned configuration shape is below. Exact key names can be refined during implementation without changing this authority model.

```toml
[supervisor]
profile = "supervisor"
max_generated_profiles_per_routing = 5

[routing.default.generated]
coordinators = ["coordinator"]
workers = ["standard", "deep"]
postmortems = ["postmortem"]
```

Each list names allowed manual bases for generated variants in that role. Omit a role to disable generation for it. Omit `[supervisor]` to retain today's behavior.
A generated variant's `config.toml` contains its approved base and description. Its `instructions.md` contains the additional work method.
Keep the current plural filename `instructions.md` for compatibility with existing profiles.

## Appendix E. Principles that changed the plan

Laziness Protocol reused the saved postmortem and file persistence instead of adding another model call or message broker.
Make Operations Idempotent required restart adoption, stable decision IDs, recoverable report indexing, and crash-safe publication acknowledgements.
Separate Before Serializing Shared State separated manual profiles, agent drafts, report artifacts, and the plugin's published registry.
Sequence Work into Verifiable Units ordered report delivery, supervisor lifecycle, and automatic publication into three checked stages within P1.
Guard the Context Window partitioned source exploration into bounded read-only tasks and required bounded supervisor context batches.
Encode Lessons in Structure put route authorization, immutable revisions, active-variant limits, and forbidden-field rejection in plugin validation.
Prove It Works separated the plan checker from future feature evidence and specified real CLI, Herdr, and Linear checks with explicit limits.


## Appendix F. Portable supervisor memory selection

The space label is fixed by the user to `herdr-linear-agent-supervisor`. Pass it as the `label` of the existing `workspace.create` request. Match persisted placement and working directory when adopting a workspace, not the label alone. Do not take over an unrelated user-created workspace with the same label.

Treat a processing operation as a completed supervisor report batch and its decision, including a no-change decision. It does not mean every shell command or tool call. An interrupted turn has no committed completion and its unseen work remains eligible for redelivery.

Select a small in-process memory module with Markdown knowledge bodies, typed JSON state and metadata, and a rebuildable SQLite FTS5 search index. Treat ai-memory as a reference implementation. Keep memory independent of supervisor profile name, model, kind, and native session ID under `<state_dir>/supervisor/memory/`.

Separate knowledge representation, control state, and retrieval.

| Artifact | Representation and owner |
| --- | --- |
| Episode and decision | Immutable plugin-owned JSON with report IDs, observations, selected profile revisions, action or no-change decision, and references to committed knowledge revisions. Reuse existing decision records. |
| Reusable knowledge | Immutable Markdown bodies describing failure patterns, procedures, attempted fixes, contradictory evidence, and remaining questions. Use a short consistent outline that agents can read directly. The supervisor updates this content through its memory delta. |
| Knowledge metadata | Typed JSON records the stable ID, workspace/team/routing scope, status, source IDs, profile revision applicability, content hash, and Markdown revision reference. Explicit states include hypothesis, supported, refuted, and superseded. |
| Restart checkpoint | Small typed JSON containing the committed generation, pending-work references, open questions, and next validation steps. No transcript or private chain of thought. |
| Search | SQLite FTS5 indexes committed Markdown content and relevant metadata. It is a disposable derived index, not completion truth. |

Markdown is the authoritative knowledge content. JSON owns the fields the plugin must validate or act on. Do not parse free-form Markdown headings or sentences to infer report acknowledgement, authorization, active profile revisions, or pending-work state. Do not add a YAML/frontmatter parser solely for this feature. The decision log references staged immutable knowledge artifacts rather than maintaining a second independently editable copy of their text.

Every completed decision supplies a typed memory delta and checkpoint. An empty topic delta is valid when there is no new lesson. Validate IDs, scope, source references, bounds, content hashes and revision conflicts; schema validation cannot prove semantic truth. Do not accept an earlier model assertion as an established fact without its evidence.

Reuse `serde_json` and `src/files.rs` for authoritative writes. Stage immutable decision, Markdown and metadata artifacts first, then publish the one manifest pointer binding the decision, memory generation and generated profile registry revision. Only referenced committed artifacts are visible to acknowledgement, profile selection or context. A memory-write failure activates no profile and acknowledges no report. Test crashes between persistence boundaries. File writes and a SQLite transaction do not become atomic merely because they share one binary.

Use `rusqlite` with bundled SQLite for a narrow local index. Preserve single-binary distribution without requiring a user-installed SQLite shared library, SQLite CLI, or separate FTS extension. Confirm FTS5 support and external-link dependencies in release artifacts on macOS and Linux. Building from source requires a C compiler for bundled SQLite. Store stable document IDs, revision/hash, authorization scope and title/body. Use prepared SQL, bounded plain-text query handling, FTS relevance ranking and bounded snippets/results. Do not initially implement advanced user-supplied FTS syntax, custom rank fusion, graph traversal, embeddings or a storage-backend abstraction.

The ticker manages index catch-up through blocking work. The index generation must match the requested committed memory view before search is reported complete. While catching up, return explicit index-not-ready state and allow canonical record retrieval by ID. Rebuild a missing or incompatible index from committed artifacts without a model call. A stale index is not evidence that prior knowledge is absent.

At bootstrap, load the checkpoint and a bounded knowledge index. Retrieve only records authorized by the current workspace/team/routing policy, then narrow by repository, topic and profile revision. Read episode details and original postmortems on demand. Include unresolved contradictory evidence with selected topics. Paginate discovery rather than silently reporting no knowledge after a keyword miss. Never inject all historical episodes into each prompt.

The alternatives are compared as follows.

| Candidate | Decision | Maintenance rationale |
| --- | --- | --- |
| Native session or transcript alone | Do not use as durable memory | Profile/model/kind changes need an independent restart path and maintained knowledge. |
| Markdown knowledge with JSON metadata and SQLite FTS5 | Select for P1 | Plain knowledge content, explicit machine state, existing atomic persistence, and a standard search engine. No copied application subsystem to maintain. |
| Structured JSON for all knowledge prose | Retain JSON for machine fields | No demonstrated token advantage justifies forcing all natural-language knowledge into a structured knowledge engine. |
| Selected ai-memory code, whole crates, or an external ai-memory service | Reference only | The small required queries can use the database API directly. Avoid owning an application-code fork or an extra service. |
| Turso Database | Consider if requirements favor it | In-process Rust and asynchronous APIs fit embedding, but current documented FTS uses Tantivy, is experimental, and does not implement SQLite FTS5. It is not a transparent FTS5 replacement. |
| Embedding memory framework | Defer | No measured semantic-retrieval need justifies another model, vector index or runtime. |

The original [LLM Wiki idea](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) and [ai-memory architecture](https://github.com/akitaonrails/ai-memory/blob/main/docs/ARCHITECTURE.md) inform the source/compiled-knowledge split. [SQLite FTS5](https://sqlite.org/fts5.html) documents ranking, snippets and full-text queries. [Turso's manual](https://github.com/tursodatabase/turso/blob/main/docs/manual.md#full-text-search-experimental) and [compatibility reference](https://github.com/tursodatabase/turso/blob/main/COMPAT.md#full-text-search-fts) document its current FTS status and distinct API. These capabilities do not establish a fastest or lowest-token winner for this workload.

During implementation compare a history-only baseline with bounded knowledge bootstrap on the same frozen report corpus. Include a repeated failure, a recorded failed fix, a contradicted lesson, a retired profile revision and interrupted work. Measure source retrieval, duplicate-action avoidance, actual input tokens where available, and pending-work recovery after switching between at least two configured supervisor profiles. Check retrieval with representative Japanese text and code identifiers. Label same-model switches separately from cross-model or cross-kind checks. Report rendered bytes or characters under their real names if CLI token counts are unavailable.

Accept when a profile switch recovers required pending work and evidence-backed lessons without reprocessing acknowledged reports or repeating a recorded failed change. Historical growth must not force all episodes into each prompt. These checks are planned and have not run.

Foundational Thinking separated storage representation, maintained knowledge, control state and retrieval. Model the Domain kept typed identity and evidence beside prose. Laziness Protocol uses the existing atomic persistence and standard database queries. Make Operations Idempotent keeps one completion boundary with a rebuildable index.

## Appendix G. Keep memory inside the plugin

The user clarified that only herdr-linear-agent uses this memory. Keep memory ownership in the existing ticker lifecycle. Agents use `supervisor context`, `supervisor memory search` and `supervisor decide` through local plugin CLI and state-request conventions. No HTTP endpoint, MCP server, second daemon, API key or account subsystem is needed. Same-user filesystem access is the local access boundary. Preserve state-directory permissions, current supervisor generation validation, and workspace/team/routing checks.

The ai-memory source review used commit `705aba70bc1a3ce6349194135206fb622ca55749`, obtained as a source archive. This was source inspection only. Its store includes user/authentication state, and its wiki layer depends on store, LLM, watcher and git layers. Copying those application layers would make this repository own an application-code fork. Reference the design and implement only the plugin's narrow database queries and memory commit path. Do not port its consolidation engine, hooks, provider clients, web UI, Git history, decay or managed execution.

A supervisor processing operation still ends only after its decision, memory and catalog change have committed. The supervisor maintains knowledge, including no-change results and failed approaches; a search index alone does not perform that work. A replacement native session gets the checkpoint plus bounded related knowledge. Keep query edge cases, index lag/rebuild, authorization scope, crash recovery and profile-switch bootstrap in P1 verification.

The planning task created Issue #156 and this document without changing implementation, dependencies, installed services, or running plugin state. Implementation is tracked in PR #157.

## Appendix H. Store memory independently of agent sessions

The user accepted the Markdown, typed JSON and bundled SQLite FTS5 direction. Use the existing `Env::state_dir()` resolution. The default is `~/.local/state/herdr-linear-agent`; an absolute `XDG_STATE_HOME` overrides its parent. Keep all paths below the plugin-owned state directory, outside source repositories and manual profile folders.

```text
<state_dir>/
  runs/                                      # Existing source run records and outcomes
  supervisor/
    runtime.json                             # Placement, native session, generation and pending drain
    current.json                             # Sole authoritative commit pointer
    workspace/                               # Stable supervisor working directory and drafts
    reports/
      <report-id>.json                        # Derived feed entry referencing its source artifact
    decisions/
      <decision-id>/
        decision.json                        # Consumed reports, action, evidence and memory delta
        checkpoint.json                      # Pending work and next checks at this completion
    memory/
      topics/
        <topic-id>/
          <revision>/
            content.md                       # Immutable knowledge body
            metadata.json                    # Scope, evidence, status and revision identity
      manifests/
        <generation>.json                    # Complete topic-to-revision view for this generation
      index.sqlite3                          # Rebuildable FTS5 projection and its generation
    generated-profiles/                      # Immutable profile revisions and catalog manifests
```

`current.json` references the committed decision, its checkpoint, the memory manifest and the generated-profile catalog manifest. Stage and durably persist referenced immutable artifacts before atomically replacing this pointer through the existing file helpers. Only reachable committed decisions acknowledge reports. The checkpoint is not copied into a second independently updated file. The memory manifest lists topic revision references; episode history reuses decision records instead of a separate prose episode directory. Prepared artifacts unreachable from the commit pointer are not committed memory.

Markdown may use short readable topic names such as `worker-test-selection`, but topic IDs, report IDs, decision IDs and revisions are validated path components chosen or checked by the plugin. Metadata owns workspace/team/routing authorization, repository applicability and profile revision references. A human-readable name is not an access boundary, and readers must not discover committed content by walking all topic files. Keep superseded revisions for evidence. Defer automatic history deletion.

Neither the root nor topic identity includes a supervisor profile, model or native session ID. A replacement session bootstraps from the committed checkpoint and bounded authorized topic records. The working directory and drafts are not a memory source. A missing or incompatible `index.sqlite3`, including associated SQLite sidecar files where used, is rebuilt from the current committed view. A normal backup includes the canonical supervisor artifacts and the referenced source run artifacts; the search database can be omitted and rebuilt.

## Appendix I. Adopt a generated revision into declarative profiles

Use promotion as the operational term for adopting one immutable generated revision as an ordinary human-owned profile. This is an explicit handoff, not an ongoing synchronization or a move of generated files. Keep the configuration repository, its deployment mechanism and its review policy under operator control.

1. Inspect the generated revision, its routing/role applicability, source evidence, base fingerprint and later observed results. Choose the exact revision rather than a moving latest alias. No fixed number of successful runs establishes fitness across different tasks.
2. Export that revision to any new directory specified by the operator, using a stable ordinary profile name. An output-directory option names the exact destination; write `config.toml`, the generated delta as `instructions.md`, and a short provenance `README.md` directly inside it. Do not require a `profiles/` parent, Git metadata or a particular repository layout. The destination may be a scratch directory that the operator later imports into a configuration repository or deployment tool. The provenance names the generated identity/revision, base fingerprint and content hashes. Do not copy run transcripts, issue bodies or raw reports. Refuse an existing destination. The selected revision remains immutable if the supervisor publishes a later revision during review.
3. Preserve `base = "<manual-base>"` only when the currently loaded plugin configuration still resolves that source base to the fingerprint used by the selected generated revision. Check effective configuration and instruction content before export, allowing intentional naming/provenance differences. Do not duplicate inherited instructions into the child. If the source base is missing or has changed, stop the export with the difference and require the operator to review/rebase the content; do not silently copy current base semantics or flatten the inheritance. Record the required base in the exported provenance. The exporter does not inspect or infer a destination configuration layout; the operator checks that the deployment target provides the required base when adopting the files. Flattened standalone exports can be designed later if needed.
4. Review the ordinary profile and the required routing change in a configuration-repository PR. Merge and apply through the existing declarative mechanism, then explicitly reload the plugin. Confirm the ordinary profile is eligible in the intended role and routing. Export itself edits no live configuration, starts no reload and creates no commit or PR automatically.
5. Retire the generated profile from future selection through the existing operator command, recording the promoted source revision and the destination manual profile identity in the retirement reason/provenance and committed supervisor history. Check the currently active source revision before retiring because the supervisor may have published a newer revision during review. A different active revision requires a deliberate operator choice. Keep generated revisions used by existing runs; ordinary retirement must not interrupt them.

After promotion, the ordinary profile follows normal declarative ownership and reload behavior. Supervisor guidance must consult the adoption record before proposing equivalent generated work again. Do not infer adoption by scanning folders or comparing prose, and do not promise semantic duplicate detection. The operator may separately authorize the adopted profile as a new base through the existing per-routing policy. New generated descendants live in the generated area; they never update the adopted folder.

Use the existing retirement and decision records for adoption provenance. A separate promotion database, automatic PR bot, file synchronization watcher or cross-repository transaction is unnecessary for P1. Export, declarative deployment and retirement cannot be made atomic across the external configuration repository and plugin state. Leaving the source active until the replacement is available preserves a usable candidate; the operator completes retirement after checking the replacement.

## Appendix J. Drain the supervisor before replacing its profile

Use explicit config reload as the normal trigger for a supervisor profile change. Editing configuration files only produces the existing reload notification. Compare the fully resolved supervisor profile, including inherited instructions and launch settings, and the configured Herdr session. An unchanged supervisor remains in its native session even when unrelated configuration changes. Generated profile publication does not reload configuration or restart the supervisor. Existing coordinators and workers are not restarted as a side effect of supervisor replacement; generated revision pins retain their documented semantics.

The transition is running, draining, closing, starting and running. These are reconciler states and persisted runtime intent, not a second background service.

1. Validate the requested config before starting a transition. On reload, persist the pending target fingerprint, old session/generation, issued batch identity and a deadline in `runtime.json`. The newly configured policy applies immediately. Restarting the ticker's internal tasks must carry this intent forward and adopt the old supervisor for draining rather than immediately closing or duplicating it.
2. During drain, issue no new report batch or fresh-work prompt. Report the replacement request through the plugin's context/decision command responses and supervisor method; do not assume a busy CLI accepts an injected shutdown prompt safely. Allow the previously issued batch to submit its normal decision, memory delta and checkpoint only while current authorization still permits it. Do not grant a blanket grace period for old catalog writes or revoked teams/bases.
3. If no batch is in progress, close after verifying the agent is idle. Otherwise wait for the issued batch's durable commit and idle turn, then fence that session's mutation authority and close the managed workspace. An idle pane alone does not acknowledge reports or prove that memory was saved. The already committed checkpoint is enough if there is no pending operation; do not invoke an extra model call just to say goodbye.
4. Use a proposed fixed 120-second grace limit for normal profile replacement. On expiry, fence old mutation authority under the same commit serialization used by decisions, then request Escape and workspace close. Any commit racing the fence must be deterministically before or after it. Only a commit accepted before the fence acknowledges reports. The replacement redelivers uncommitted work and loads the last committed memory/checkpoint. Never manufacture a partial decision from a transcript.
5. Confirm the old agent/placement is gone using a complete Herdr snapshot or supported definitive termination result before starting the replacement. A lost close response, partial snapshot or unreachable session cannot justify an overlapping launch. Recreate the exact named workspace and stable working directory when termination in place is unavailable. If replacement startup fails, retain committed memory and report the blocked replacement; do not silently revert to an unrequested profile.

The drain and deadline survive ticker restarts and repeated reloads. A newer valid request replaces the pending target without extending the original deadline. Status shows the current and requested profile, draining/closing/starting state, deadline and recovery error. Reload action output must distinguish request acceptance/config application from replacement completion; the action need not block until the agent exits. Continue ordinary reconciler passes and Linear polling/delivery while the supervisor drains.

Disable and authorization revocation fence mutation authority immediately and do not wait for the normal profile-change grace period. Herdr-session changes require cleanup of the old managed placement before a replacement can be launched; if cleanup cannot be confirmed, report it explicitly. An ordinary requested ticker stop uses a shorter drain budget within the existing 30-second `ticker stop` wait, reserving time for interruption and close. An abrupt process kill cannot guarantee graceful shutdown; durable commit and generation checks provide recovery.

The current plugin Herdr adapter has `agent.prompt`, Escape through `agent.send_keys`, and `workspace.close`; it does not expose a verified universal graceful agent-process exit operation. Promise an application-level graceful boundary, where decision and memory finish before workspace closure. Do not equate Escape with process termination or assume every agent CLI handles SIGTERM, `/exit` or another CLI's quit sequence identically. A provider-specific exit API can improve termination only after its behavior has been checked; it is not required to preserve committed memory.

## Appendix K. Define the persisted records and command contracts

The names below describe new types to implement. They do not exist at the inspected source revision. Refine Rust field names while preserving the identities, ownership and invariants recorded here. Use explicit schema versions for new supervisor artifacts and serde defaults for additions to existing records.

### Record identity and evidence

| Type | Required information | Owner |
| --- | --- | --- |
| `SupervisorReport` | Stable report ID, workspace-qualified run key, issue ID, team, routing, source artifact reference and hash, interim or final stage, attempt outcome, observed time, selected profile revisions, and available observations | Plugin, derived from a saved source artifact |
| `SupervisorRecord` | Lifecycle state, managed Herdr session and placement, native agent session, management generation, resolved supervisor profile fingerprint, active policy revision, current issued batch, wake state, pending replacement and deadline | Reconciler, with narrow command-owned batch updates under the same supervisor lock |
| `SupervisorBatch` | Stable batch ID, management generation, report IDs actually shown, context commit sequence, policy revision, and completion state | Plugin when issuing bounded context |
| `SupervisorDecision` | Stable decision ID, previous committed decision reference, actor, batch ID for supervisor work, consumed report IDs, base commit sequence, policy revision, observations, concise reason, expected result, optional catalog mutation, memory delta, and checkpoint reference | Plugin after validating a proposal or operator catalog action |
| `SupervisorMemoryUpdate` | Topic creations, revisions or supersessions, expected prior topic revision, Markdown content, evidence references, applicability, and knowledge status | Agent proposal validated and persisted by plugin |
| `SupervisorCheckpoint` | Pending work references, unresolved questions, intended checks, and completed decision identity | Plugin persists the agent's portable summary |
| `KnowledgeMetadata` | Topic ID, revision, title, content hash, authorization scope, repository and profile applicability, source IDs, status, and supersession links | Plugin-owned machine metadata beside Markdown |
| `SupervisorCommit` | Monotonic commit sequence, committed decision and checkpoint references, memory manifest reference, and profile catalog manifest reference | Sole authoritative `current.json` pointer |
| `GeneratedProfileRevision` | Generated identity, immutable revision, role, routing, approved manual base and fingerprint, description, instruction delta, resolved profile snapshot, evidence, author decision, and expected result | Plugin publication |
| `GeneratedRunCatalog` | Catalog revision and fully resolved generated candidates permitted for this run, grouped by role | Run record, fixed when a new run is created |

Use separate commit, memory and catalog generations. A no-change decision advances the commit sequence and saves a checkpoint. It need not rebuild an unchanged memory index or publish an unchanged profile catalog.

Distinguish a supervisor batch decision from an operator catalog action with a typed actor or action variant. An operator retirement or rollback records its reason and provenance, retains the current memory and checkpoint, and consumes no reports. Do not invent an agent batch for that action. List, show and export are read or output operations, not catalog commits.

Use opaque validated IDs as path components. Do not use a Linear title, repository URL, report body, absolute path or profile description as a path. A report ID must include the run key and the source artifact identity. A content hash alone would incorrectly combine distinct equal summaries. New source artifacts must have collision-resistant attempt identities rather than second-resolution timestamps alone. Existing postmortem filenames remain readable.

Capture routing and profile attribution when the operation is prepared or completed. Do not infer an old report's routing from today's config. Mark unavailable legacy attribution as unknown and disallow automatic publication justified solely by that record. Keep reopened-run and repeated postmortem attempts distinct. Preserve the existing run's catalog pin when the same stored run resumes. Do not assume reactivation creates a new run directory.

Default a topic to one workspace, team and routing scope. A topic that combines evidence from several scopes must require access to every contributing scope. A keyword hit or shared repository does not authorize revealing another team's report or derived lesson.

### Serialize the completion boundary

Add a supervisor state lock, such as `<state_dir>/supervisor/lock`, following the repository's file-lock conventions. Serialize current-pointer updates, decision replay checks, catalog operator mutations, issued-batch changes and generation fencing through this lock. Run locking work in `spawn_blocking` from async paths. Do not hold this lock across Herdr, Linear, git, a model call, or search-index catch-up.

Keep the run lock and supervisor lock separate. Save source outcomes under their run lock, release it, then materialize report entries idempotently. Do not introduce nested acquisition of run and supervisor locks. If an agent-facing operation needs a generated catalog to allocate a worker, take an immutable catalog snapshot first. Under the run lock, update only worker and pin fields owned by that operation, then release the lock before external I/O.

Commit a supervisor decision in this order.

1. Parse the proposal with bounded sizes and strict known fields. Validate paths, IDs, content hashes, evidence scope, batch membership, expected revisions and allowed catalog changes.
2. Stage immutable Markdown, metadata, profile revisions and manifests. Staging may happen outside the supervisor lock because staged files are not visible through `current.json`. Create immutable identities without replacement. On an existing identity, verify matching content or reject the conflict. The replacing atomic-write helper is for mutable pointers and records, not permission to overwrite a committed immutable revision.
3. Acquire the supervisor lock and read the latest current pointer and lifecycle state. First check whether this exact decision ID and payload already committed. Return its existing receipt on an exact replay. Reject the same ID with different content.
4. Recheck the current management generation, policy, issued batch, unacknowledged report IDs and expected commit sequence. Reject a stale proposal without acknowledging its reports. Require the caller to refresh context before revising the proposal.
5. Persist the immutable decision and checkpoint with the previous committed decision reference. Complete required file persistence before atomically replacing `current.json` with the new commit reference.
6. Release the lock, wake the ticker, and catch up derived report-receipt and memory-search projections. A failure after pointer replacement is a committed decision with pending projection work, not a new uncommitted attempt.

Reconstruct report acknowledgement from the reachable committed decision history. Use a derived receipt index or snapshot to avoid traversing all decisions for every context request. Check its commit watermark before returning a complete unseen-report view. An index failure cannot justify treating committed reports as unseen. On startup, rebuild or catch up this projection before completing context issuance.

Distinguish missing initial state from malformed existing state. A corrupt `current.json`, missing referenced artifact or hash mismatch is a recovery error. Do not silently initialize an empty history or let a directory scan activate staged files. Expose the error and retain ordinary manual candidates where they remain valid.

The existing `files::write_atomic` syncs file data and renames a sibling temporary file. It does not sync parent directories. Preserve its reuse and distinguish process-crash recovery from power-loss guarantees. If the implementation claims durable ordering across a power loss, add the necessary directory sync behavior in `src/files.rs` and verify its platform behavior. Do not claim a file-plus-SQLite transaction.

### Expose a small local command set

The command names below are proposed implementation interfaces. Check the actual `--help` output after implementation. They are not commands available in the current binary.

| Proposed command | Contract |
| --- | --- |
| `supervisor context --generation <generation>` | Return or resume one bounded issued batch, active profile variants, the committed checkpoint, related knowledge, policy revision, and base commit sequence. Persist the exact shown report IDs. During drain, return the existing batch and replacement instruction without issuing new work. |
| `supervisor decide --generation <generation> --decision-file <path>` | Validate and commit a decision, memory delta and checkpoint. Accept only reports shown to that batch. Return a durable receipt and any pending replacement instruction. No Linear call. |
| `supervisor memory search <query>` | Search authorized committed knowledge with bounded results, snippets, source handles, and the indexed memory generation. Report catch-up explicitly. |
| `supervisor show <artifact-handle>` | Read a bounded report, decision or topic revision by a plugin-issued handle. Validate identity and current scope. Do not accept an arbitrary filesystem path as a handle. |
| `supervisor profiles list` | Show active and retired generated identities, revisions, bases, routing, evidence, and adoption provenance. |
| `supervisor profiles show <id> --revision <revision>` | Show the exact immutable revision and its effective inherited configuration. |
| `supervisor profiles retire <id> --expected-revision <revision> --reason <text>` | Commit an operator retirement through the same catalog path. Require the expected active revision so a concurrent update cannot be retired accidentally. Allow the reason to record the adopted manual profile. |
| `supervisor profiles rollback <id> --revision <revision>` | Create a new catalog transition selecting retained content after revalidating current base and policy. Do not overwrite history or bypass revoked authorization. |
| `supervisor profiles export <id> --revision <revision> --name <name> --output <directory>` | Export ordinary profile files directly into the exact new destination directory. Enforce Appendix I without assuming a repository layout or changing activation. |

Separate supervisor agent commands from operator catalog commands in generated command guidance. An agent retires or publishes through its validated `decide` action. Operator commands still validate policy and revisions. Same-user filesystem authority is not an authentication system, and allow-lists are not an OS sandbox.

Start with bounded constants for report count, rendered context, topic body size, source detail and result count. Pick the numeric text budgets from the implementation corpus and document them. Return pagination and truncation information rather than silently omitting work. Never ask the model to paste its private reasoning or entire transcript into a checkpoint.

### Maintain source, knowledge and search separately

Keep original reports as evidence references. Let `content.md` use a short outline with observation, guidance, supporting and opposing evidence, applicability, and next validation. A decision may record an empty topic delta when there is no reusable lesson.

Use SQLite for derived retrieval and receipt projections only. Key rows by stable artifact identity and revision hash. Store the indexed memory generation and receipt commit sequence separately. Run writes and rebuild work off the async reconciliation path. Only index topic revisions reachable from the committed memory manifest.

Use prepared SQL and escape literal full-text query terms. Do not expose arbitrary FTS syntax. Check tokenizer behavior with Japanese text, English prose and code identifiers before choosing it. The bundled trigram tokenizer is a candidate for the mixed-language corpus. Default tokenization is not proof of useful Japanese retrieval. Document short-query behavior and preserve exact-ID lookup when full-text search cannot answer.

Do not add embeddings, a knowledge graph, RRF, a generic memory backend interface or a second consolidation model. The supervisor maintains the Markdown knowledge. The index only finds the maintained content.

## Appendix L. Execute the implementation in three checked stages

These steps expand the P1 build checklist. Keep the feature opt-in and preserve existing behavior with no supervisor configuration. Do not publish a partial feature that mutates profiles before its report and memory commit paths are reliable.

### Stage 1. Persist reports, decisions and portable memory

1. Add the optional supervisor config and per-routing generated policy in `src/config.rs`. Resolve manual bases using the existing inheritance loader. Validate role suitability, base existence, routing-agent availability and agent capacity. Keep the current sample valid without new fields. Do not mutate user config during publication.
2. Extend `src/paths.rs` with supervisor path helpers under `Env::state_dir()`. Keep the existing XDG and HOME resolution. Create the tree with the existing private-state ownership rules. Add the supervisor lock and any derived receipt files to the layout in Appendix H when choosing their final names.
3. Define the new types in the supervisor owner module. Add only optional or defaulted pin fields to `RunRecord`, `AgentRecord` or worker records. Avoid scattering unchecked `serde_json::Value` field access across lifecycle code.
4. Extend `src/ticker/postmortems.rs` at the saved-success and handled-failure boundaries. Keep `Outcome` and the postmortem model answer as `summary` and `labels`. Attach binary-derived observations to saved artifacts, not model-authored routing or policy assertions. On failure, save a diagnostic artifact before clearing the due marker. Preserve no automatic retry and no fabricated Linear comment.
5. Keep source persistence and existing Linear outbox enqueueing under the current run-lock ownership. Materialize the supervisor report only after releasing that lock. Queue a recoverable local feed reference or use a bounded repair path so a crash between source persistence and report materialization loses no completed source artifact. Do not add local supervisor effects to the Linear-write outbox.
6. Build the startup backfill and incremental feed delivery. Backfill historical success artifacts with unknown fields marked explicitly. Use stable source identities and idempotent writes. Handle late artifacts and equal timestamps. A timestamp-only cursor cannot prove that all earlier writes arrived. Permit one bounded repair sweep, then incremental or round-robin bounded work, rather than a full history scan every pass.
7. Implement context issuance, proposal parsing, the serialized commit path, exact replay behavior and source-handle reads. A no-change batch still saves a decision and checkpoint. A validation or memory persistence failure leaves all its reports unacknowledged and activates no catalog change.
8. Add bundled `rusqlite` and the local index, selecting a version compatible with the repository's declared Rust support or explicitly resolving any MSRV change. Update `Cargo.lock`. Keep the original Markdown and typed JSON independent of that crate's database format.
9. Implement projection catch-up and rebuild. Expose an explicit not-ready result while either receipt completeness or memory retrieval is behind the requested committed view. Exact artifact reads remain available. Rebuild a deleted database without a model call.
10. Complete the stage's fake regression checks before wiring automatic supervisor launch. Save receipts for source persistence, report identity, exact replay, memory failure, crash after pointer commit and old record parsing.

### Stage 2. Reconcile one supervisor and graceful replacement

1. Add the supervisor lifecycle to the existing reconciliation pass in `src/ticker/reconcile.rs`, with effects in `src/ticker/supervisor.rs`. Reuse the pass's snapshot. Events and timers wake the reconciler and never create agents directly.
2. Reserve capacity in both intake and `commands::worker_start` accounting before a supervisor is placed. A lost or temporarily unreachable supervisor retains its configured reservation so recovery cannot overbook the limit. Do not count it once as a reservation and again as an ordinary run agent.
3. Create and adopt the managed workspace with the exact label and stable `workspace/` cwd. Save placement identity before advancing to agent launch where the existing effect pattern permits it. Handle an unknown create or start response by adoption and verification rather than issuing duplicate work.
4. Build supervisor priming using the selected ordinary profile's resolved instructions and supported `src/agents.rs` launch arguments. Add `assets/SUPERVISOR.md`, role-specific allowed commands and generation/batch guidance. Keep Linear credentials out of the pane and prompt. Do not install skills or alter model or permission flags automatically.
5. Wake the supervisor only when unseen work or a relevant policy change needs attention and its state permits a turn. Leave busy turns uninterrupted. No work means no repeated prompt and no repeated model invocation. A wake is not acknowledgement.
6. Add persisted drain intent and the lifecycle transitions from Appendix J. Distinguish task shutdown for an unchanged reload from a request to replace or disable the supervisor. The new `serve` instance must adopt a draining supervisor and its original deadline.
7. Gate context and decision mutations by management generation and the current issued batch. Fence old authority before timeout interruption or immediate revocation. Perform Herdr calls after releasing state locks. Wait for definitive cleanup before replacement launch.
8. Update action/status/doctor output. The existing reload action treats removal of `ticker.reload` as completion, which is insufficient for an asynchronous supervisor replacement. Report accepted config separately from pending or completed agent replacement without blocking the Herdr action for the full grace period.
9. Preserve the existing 30-second ticker-stop contract or explicitly update its caller and tests if choosing a different contract. The proposed normal-reload grace of 120 seconds must not make `ticker stop` wait on an impossible deadline. Abrupt exit recovery uses committed memory and persistent generation checks.
10. Check same-config reload, unrelated-config reload, busy drain, idle drain, timeout, crash during drain, repeated reloads, failed replacement launch, unknown close result, partial snapshot and disable. Confirm that Linear polling and ordinary run reconciliation continue while waiting.

### Stage 3. Publish profiles and activate the shared catalog

1. Give generated identities a reserved namespace that cannot collide with manual names. Keep roles and routing scope explicit. Generated `config.toml` stores the approved base and description. `instructions.md` stores the delta. Persist the fully resolved revision separately for attribution and pinning.
2. Implement strict publish, revise and retire payloads. Permit changes to description, instruction delta and choice of an allowed base. Reject kind, model, effort, arguments, environment, timeout and unknown override fields. Validate revision conflicts, path containment, symlinks, size bounds and evidence scope.
3. Stage generated revisions and catalog manifests with the memory artifacts, then commit through the one current pointer. Do not let a directory scan activate an unreferenced draft or let a separate registry pointer publish before memory succeeds.
4. Expose one effective-catalog resolver to ticker routing and short-lived commands. Its inputs include the active approved generated policy, manual base fingerprints, the committed catalog and an optional run pin. Cache only by these revisions. Do not add generated profiles only to the ticker's in-memory `Config` and leave worker commands loading a different set.
5. Keep the active generated-policy revision synchronized with explicit config activation. Persist enough approved policy and base identity for short-lived commands to validate the same view. Merely reading edited config files must not silently activate a new generated policy before reload. Keep ordinary manual-profile behavior unchanged.
6. After a committed generated change, poke the existing ticker socket. On the next pass, load the new committed catalog view. Context and start commands read that same view. Do not force config reload, recreate Linear clients or reread credentials for a catalog publication. A lost poke is recoverable from the committed generation on subsequent passes and startup.
7. At new-run intake, pin eligible generated candidates and their fully resolved revisions. Use the same snapshot for coordinator routing, coordinator-visible worker choices and future postmortem choices for that run. At each selected launch, persist which resolved generated revision is actually used. Give old records without a pin no retroactive generated candidates.
8. Make `commands::worker_start`, `worker_restart`, `coordinator::digest`, ticker launch/recovery and postmortem planning consult the pin for generated names. Preserve existing manual-profile reload semantics. An active coordinator does not automatically gain a newly published candidate. Refreshing an active run's catalog is outside P1.
9. For new runs, exclude a generated revision whose base fingerprint no longer matches. For existing authorized pins, a manual base edit does not rewrite the saved resolved revision. A removed team, routing role or allowed base blocks further generated launches and resumes that require that permission, even with a pin. Ordinary retirement alone does not revoke a pin.
10. Preserve an ordered manual fallback and enforce routing-agent availability for multiple coordinator or postmortem candidates. Cap active generated variants at five per routing by default and allow at most one catalog mutation per supervisor decision.
11. Implement operator list, show, retire, rollback and export with the shared revision and policy validation. Route retirement and rollback through the serialized catalog commit path. List, show and export do not create catalog commits. Export accepts the exact arbitrary destination directory. The source revision remains pinned throughout export even if the active generated revision changes concurrently. Do not automate a configuration-repository PR or deployment.
12. Add promotion provenance to the existing retirement or operator decision history. Make the supervisor consult that history. Adopted ordinary profiles remain outside its write ownership and can become new bases only through explicit human policy.
13. Run the complete P1 unit, live and performance checks. Update README config and operator examples, agent command sheets, behavior persistence/reload sections, and verification results. Keep fake results distinct from actual Herdr, agent CLI and Linear evidence.

## Appendix M. Check the failure and concurrency cases

Treat the following rows as observable assertions. Use `World`, `FakeHerdr`, `FakeLinear`, `FakeRunner`, injected time and the existing effect controls. Add affected-module tests beside the implementation. Do not depend on real credentials or installed CLIs in ordinary tests.

| Case | Required result |
| --- | --- |
| Supervisor omitted from config | Existing routing, agent capacity, postmortems and commands keep their behavior. |
| Source saved, feed not yet written | Recovery materializes one report from the source. Linear delivery remains independent. |
| Same workspace-qualified issue reported twice with different artifacts | Both source events remain distinct. Equal issue identifiers in different workspaces never share a path or acknowledgement. |
| Postmortem failure | A diagnostic report survives. The due marker clears according to existing semantics. No invented summary, retry or Linear comment appears. |
| Memory or revision validation fails | No pointer change, no report acknowledgement and no profile activation. |
| Crash before pointer replacement | Staged artifacts remain invisible. The issued work is eligible for redelivery. |
| Crash after pointer replacement, before response or index update | Replaying the decision returns the same receipt. Catch-up reconstructs acknowledgement and knowledge without another catalog revision. |
| Same decision ID with different payload | The command rejects the conflict and leaves the original commit unchanged. |
| Topic or catalog updated after context issuance | A stale proposal fails its expected-revision check and consumes no additional reports. |
| SQLite database deleted, stale or incompatible | The plugin reports catch-up and rebuilds committed projections. No missing-history claim or model consolidation call appears. |
| Unauthorized report or mixed-scope knowledge | Context, search, show and publish reject or exclude the data before rendering. |
| Supervisor busy and several reports arrive | Reports queue durably. No prompt interrupts the current turn. New batches stay bounded. |
| Supervisor acknowledges all work and becomes idle | Ten minutes produce no repeated prompt or extra model call. |
| Unchanged or unrelated config reload | The existing supervisor native session survives. |
| Changed supervisor profile while a batch runs | No new batch starts. A committed decision and idle turn trigger replacement using the saved checkpoint. |
| Drain expires or ticker restarts during drain | The original deadline remains. Old authority is fenced. Only uncommitted reports are redelivered. |
| Decision commit races with timeout fencing | Exactly one serialized order wins. No acknowledged report loses its committed memory or catalog state. |
| Workspace close response is lost or snapshot is partial | The plugin verifies the old placement before launching another agent. No duplicate supervisor appears. |
| Generated revision published | New-run routing, context and worker authorization agree without full config reload or credential reads. |
| Generated revision retired or base edited | New-run eligibility updates. Existing authorized pins retain their resolved instructions. |
| Generated base or role authorization revoked | Further affected launches, restarts and resumes are blocked with a recovery reason. |
| Export to arbitrary directory | The exact three ordinary profile files appear there. An existing destination or changed source base fails without overwriting user files. |
| Export review outlives a newer generated revision | Exported content stays fixed. Retirement requires an explicit expected active revision. |

Use two configured supervisor profiles to verify portable bootstrap. A same-kind, same-model profile switch proves independence from the profile name and native session only. Cross-model or cross-kind claims require those actual configurations and logins. Measure rendered input characters or bytes when token counts are unavailable, and label them accurately.

## Appendix N. Pick up this plan in the implementation session

1. Read [Issue #156](https://github.com/civitaspo/herdr-linear-agent/issues/156), this plan, `AGENTS.md`, `README.md`, the relevant `docs/behavior.md` sections, and `docs/verification.md`. Treat proposed commands and types in this document as future work.
2. Inspect `git status`, the current branch and HEAD. This handoff added only this document locally. It created the Issue but made no feature code changes, dependency changes, commits, pushes or PRs. Recheck those facts rather than assuming a clean checkout or discarding local work.
3. Compare the implementation with the inspected baseline `7c05133eacbc1b20123569664e871dd566d1a091`. Reconfirm the postmortem persistence boundary, reload task shutdown, command config loading, profile inheritance and Herdr adapter before editing. Concurrent changes to issue placement or run lifecycle may affect these entry points.
4. Once the user requests implementation, work through stages 1, 2 and 3 in order. Keep one focused P1 and link Issue #156. Preserve the user's selected model. Follow repository signing and Codex trailer requirements for any later commits.
5. Run the repository checks only in the implementation and PR flow described above. This planning task ran document structure and whitespace checks only. No feature or real integration result is established here.
6. Hand back the PR and evidence for operator review. Keep merge authority with the operator. Report any unavailable live lane and empirical selection honestly.

The remaining empirical checks concern actual CLI priming and termination, mixed-language retrieval, projection timing, release linkage, model-switch recovery and whether later outcomes support an improvement. These are implementation checks, not requests to redesign the accepted product scope.
