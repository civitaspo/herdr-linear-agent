# Supervisor command and decision reference

Start with [Supervisor in the README](../README.md#supervisor) for setup,
permissions, lifecycle and the complete command list. These commands operate on
the local plugin state selected by `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME`.
They require `[supervisor]`; development mode is independent of them.

## Read evidence

```bash
herdr-linear-agent action status
herdr-linear-agent action doctor
herdr-linear-agent supervisor profiles list
herdr-linear-agent supervisor memory search "worker retries"
herdr-linear-agent supervisor show report:<report-id>
herdr-linear-agent supervisor show decision:<decision-id>
herdr-linear-agent supervisor show topic:<topic-id>@<revision>
```

Replace angle-bracket placeholders with IDs returned by the commands. Search
accepts one quoted query of up to 256 characters and returns at most 20 hits,
with the indexed memory generation and each topic's ID, revision, title and
snippet. Matching uses literal substrings, including terms shorter than three
characters. `show topic:<id>` reads the current topic revision; add `@<revision>`
for a retained revision. Topic output contains metadata and `content_md`.
Report and decision handles return their saved JSON.

Scopes are exact `(workspace, team, routing)` triples derived from the current
configuration. Reads reject evidence outside those scopes. Profile catalog
entries contain an immutable `revision` object and a `current` boolean;
`current` identifies the catalog pointer, while `revision.status` is `active`
or `retired`. An active entry can still be ineligible after its base permissions
or fingerprint change. Inspect the policy and eligibility returned by context
before proposing a change.

## Process a batch

The ticker prompts the supervisor with its current generation. The agent uses
that number for both commands:

```bash
herdr-linear-agent supervisor context --generation <generation>
herdr-linear-agent supervisor decide --generation <generation> --decision-file <path>
```

`context` issues a batch or resumes the already issued one. It can repair the
report feed and rebuild the search index, and records the active batch in local
state. The JSON includes `batch`, `reports`, `checkpoint`, `knowledge`,
`profile_policies`, `active_profiles` and recent decisions. Batches contain at
most 20 reports and may be smaller to bound the context size;
`remaining_report_count` counts reports waiting for later batches. Saved reports
include postmortem outcomes and run observations, including available worker
reports, without raw issue bodies or transcripts.

`decide` reads a strict JSON proposal from a file, or standard input when
`--decision-file -` is used. The proposal must account for every report in the
issued batch, even when the conclusion is to keep existing behavior. Unknown
fields are rejected. The following example shows a decision with no topic or
profile change. Replace the ID, batch fields and report IDs with current values;
write the reason and checkpoint from the actual evidence.

```json
{
  "schema_version": 1,
  "id": "<new-uuid>",
  "batch_id": "<context.batch.id>",
  "base_commit_sequence": 0,
  "policy_revision": "<context.batch.policy_revision>",
  "consumed_report_ids": ["<report-id>"],
  "observations": [{"report_id": "<report-id>", "text": "Evidence is insufficient to change the profile."}],
  "reason": "Keep the current method pending more evidence.",
  "expected_result": "Future reports can confirm whether this observation repeats.",
  "topic_changes": [],
  "profile_change": null,
  "checkpoint": {
    "summary": "Reviewed the issued batch; retained the current method.",
    "pending_report_ids": [],
    "unresolved_questions": [],
    "intended_checks": ["Compare the next run's outcome."]
  }
}
```

Set `base_commit_sequence` to `context.batch.commit_sequence`, and copy
`context.batch.report_ids` into `consumed_report_ids`. Each observation is
`{report_id, text}` and must refer to a consumed report. `id` is a new UUID for
this decision. Keep that ID and exact content when retrying after an unknown
command outcome. The plugin fills the checkpoint's schema version and decision
ID when committing it.

### Knowledge and generated-profile changes

Every change is an object with an `action` field. Fields below are required
unless marked optional. A scope is
`{"workspace":"acme","team":"DATA","routing":"default"}`. Choose scopes and
manual bases from the current context. Source report IDs must belong to this
batch and support the change in the same scope.

| Proposal location and action | Other fields |
| --- | --- |
| `topic_changes[]`, `create` | `id` (new `t-` ID), `title`, `scope`, `repositories` (array), `profiles` (array), `source_report_ids` (array), `content_md` |
| `topic_changes[]`, `revise` | `id`, `expected_revision`, `title`, `repositories`, `profiles`, `source_report_ids`, `content_md` |
| `topic_changes[]`, `retire` | `id`, `expected_revision`, `reason`, `source_report_ids` |
| `profile_change`, `create` | `id` (new `p-` ID), `role` (`coordinator`, `worker` or `postmortem`), `scope`, `base_profile`, `description`, `instructions_md`, `source_report_ids` |
| `profile_change`, `revise` | `id`, `expected_revision`, optional `base_profile`, `description`, `instructions_md`, `source_report_ids` |
| `profile_change`, `retire` | `id`, `expected_revision`, `reason`, `source_report_ids` |

Topic and profile IDs use ASCII letters, digits and `-`; topic IDs are at most
96 characters and profile IDs at most 48. Generated candidate names have the form
`generated-<profile-id>-r<revision>`. Revisions are immutable; revise or retire
using the current expected revision. A decision may change at most five topics
and one generated profile. The proposal is limited to 64,000 bytes, each topic's
Markdown to 16,000 bytes, and the checkpoint to 8,000 bytes. Reasons, observations
and checkpoint summary/items are bounded to 4,000 characters each. Checkpoint
pending report IDs, unresolved questions and intended checks each allow up to
20 entries.

Generated profile changes need one to five distinct source report IDs. Generated
profiles inherit execution settings from an explicitly allowed manual base.
Publishing validates current permissions, the base fingerprint and the
configured active-variant limit. `source_report_ids` must support the profile
change; the agent cannot override model, kind, effort, arguments, environment or
timeout in this JSON.

## Receipts and recovery

A successful decision prints `decision_id`, `commit_sequence`,
`acknowledged_report_ids`, `memory_generation`, `catalog_generation` and
`projections_pending`. The receipt confirms that the decision, knowledge,
checkpoint and optional profile change share one durable commit. A true
`projections_pending` means a derived projection still needs repair; the durable
decision has committed. Search can rebuild the derived index on its next use.

Retrying the same committed proposal ID and content returns its receipt.
Reusing an ID with different content fails. A stale generation, policy,
commit sequence or batch fails without acknowledging the reports. Refresh the
context using the generation in the current ticker prompt and reassess the
proposal; changing numbers in an old proposal bypasses that reassessment.
If report recovery is still in progress, retry context after the ticker has
repaired it.

Operator catalog changes wait until the supervisor has no active report batch.
`retire` requires the current `--expected-revision` and one to five report IDs
supplied with repeatable `--source-report`. Rollback
requires an active variant and republishes the target revision's content and
evidence as a new revision, subject to current base permissions. It cannot
reactivate a retired variant. Export requires a new output directory and a
manual name using letters, digits, `.`, `_` or `-`, without a `generated-`
prefix. Export validates that the retained base still matches. It leaves
activation, routing changes and source retirement to the operator.

Use status, doctor, the managed Herdr pane and `ticker.log` to investigate a
blocked supervisor, permission dialog, failed launch or pending replacement.
Launch and wake retries are bounded. Apply configuration corrections through
the normal config reload action. Durable state lives under
`$XDG_STATE_HOME/herdr-linear-agent/supervisor/`: committed JSON and Markdown are
the source of truth, and `memory/index.sqlite3` is derived. See
[lifecycle and persistence behavior](behavior.md#local-supervisor) for recovery
invariants and [live verification evidence](verification.md) for tested limits.
