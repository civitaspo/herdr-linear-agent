//! Postmortems of the runs whose record marks one due: each is written off
//! the pass, and its comment and labels are queued when it answers.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use jiff::Timestamp;

use super::reconcile::{Deps, Reconciler, blocking, update_run};
use crate::config::{Config, Profile};
use crate::herdr::Herdr;
use crate::outbox::Op;
use crate::postmortem::{self, Method, Outcome, SavedPostmortem, Stage};
use crate::routing::{self, Choice};
use crate::run::{Run, RunRecord};
use crate::supervisor::{ProfileObservation, ReportOutcome};
use crate::transcript::Roots;

/// The profile used for one postmortem attempt.
#[derive(Debug, Clone)]
pub(super) struct MethodUse {
    pub name: String,
    pub kind: String,
    pub picked: String,
    pub version: String,
}

/// A completed write or its diagnostic. A selected method is retained when
/// the model call failed so the report can attribute the failure.
#[derive(Debug, Clone)]
pub(super) enum Attempt {
    Success {
        method: MethodUse,
        outcome: Outcome,
    },
    Failure {
        error: String,
        method: Option<MethodUse>,
    },
}

pub(super) type Written = Result<(String, Attempt)>;

struct PostCompletion {
    stage: Stage,
    attempt_id: String,
    written: Written,
    saved: Option<SavedPostmortem>,
    now: Timestamp,
}

/// What writing a run's postmortem needs, taken from the config when it
/// starts: the routing's candidates with their profiles, its agent with its
/// timeout, and the postmortem agents' default timeout.
struct Plan {
    routing: String,
    candidates: Vec<routing::Candidate>,
    profiles: BTreeMap<String, Profile>,
    agent: Option<(Profile, Duration)>,
    postmortem_timeout: u64,
}

/// The run's routing's postmortem plan; none when it lists no profile.
fn plan(config: &Config, record: &RunRecord) -> Option<Plan> {
    let routing_name = config
        .team(&record.workspace, &record.team_key)
        .ok()?
        .routing
        .clone();
    let routing = config.routing.get(&routing_name)?;
    let names = config.profile_candidates_for(
        &record.workspace,
        &record.team_key,
        crate::config::GeneratedRole::Postmortem,
        &routing.postmortems,
    );
    let profiles: BTreeMap<String, Profile> = names
        .iter()
        .map(|name| Some((name.clone(), config.profile(name).ok()?.clone())))
        .collect::<Option<_>>()?;
    if profiles.is_empty() {
        return None;
    }
    let agent = routing.agent.as_deref().and_then(|name| {
        let agent = config.profile(name).ok()?.clone();
        let seconds = agent
            .timeout_seconds
            .unwrap_or(config.limits.routing_agent_timeout_seconds);
        Some((agent, Duration::from_secs(seconds)))
    });
    Some(Plan {
        routing: routing_name,
        candidates: routing::candidates(config, &names),
        profiles,
        agent,
        postmortem_timeout: config.limits.postmortem_agent_timeout_seconds,
    })
}

fn bounded_error(error: &str) -> String {
    const MAX_CHARS: usize = 4096;
    let mut bounded: String = error.chars().take(MAX_CHARS).collect();
    if error.chars().count() > MAX_CHARS {
        bounded.push('…');
    }
    bounded
}

/// Picks the profile, with the routing agent when there are several, then
/// has it write the postmortem.
async fn write(
    plan: Plan,
    brief: String,
    input: String,
    path: Option<String>,
    parent: std::path::PathBuf,
) -> Written {
    let routing = plan.routing.clone();
    let choice = match (&plan.agent, &plan.candidates[..]) {
        (Some((agent, timeout)), [_, _, ..]) => {
            routing::choose(
                agent,
                routing::Pick::Postmortem,
                &plan.candidates,
                &brief,
                *timeout,
                path.as_deref(),
                &parent,
            )
            .await
        }
        _ => Choice::Only(plan.candidates[0].name.clone()),
    };
    let name = choice.profile();
    let profile = &plan.profiles[name];
    let timeout = profile.timeout_seconds.unwrap_or(plan.postmortem_timeout);
    let method = Method::new(name, profile, timeout);
    let use_of_method = MethodUse {
        name: method.name.clone(),
        kind: profile.kind.clone(),
        picked: choice.source(),
        version: method.version.clone(),
    };
    match postmortem::write(&method, &input, path.as_deref(), &parent).await {
        Ok(outcome) => Ok((
            routing,
            Attempt::Success {
                method: use_of_method,
                outcome,
            },
        )),
        Err(error) => Ok((
            routing,
            Attempt::Failure {
                error: format!("{error:#}"),
                method: Some(use_of_method),
            },
        )),
    }
}

impl Reconciler {
    /// Posts the postmortems that answered and starts the ones due.
    pub(super) async fn postmortems<H: Herdr>(&mut self, d: &Deps<'_, H>, now: Timestamp) {
        let done: Vec<String> = self
            .writing
            .iter()
            .filter(|(_, (_, _, handle))| handle.is_finished())
            .map(|(key, _)| key.clone())
            .collect();
        for key in done {
            let Some((stage, attempt_id, handle)) = self.writing.remove(&key) else {
                continue;
            };
            let written = handle
                .await
                .unwrap_or_else(|e| Err(anyhow!("the postmortem task failed: {e}")));
            let Ok(run) = Run::load(&d.ctx.runs_dir(), &key) else {
                continue;
            };
            if let Err(error) = self
                .post(
                    d,
                    &run,
                    PostCompletion {
                        stage,
                        attempt_id,
                        written,
                        saved: None,
                        now,
                    },
                )
                .await
            {
                d.fail(&key, &error);
            }
        }
        for run in Run::list(&d.ctx.runs_dir()) {
            let Ok(record) = run.record() else {
                continue;
            };
            let Some(stage) = record.postmortem_due else {
                continue;
            };
            if self.writing.contains_key(&run.key) {
                continue;
            }
            // Legacy records may have a due marker without the stable attempt
            // identity. Allocate it under the run lock before starting work.
            let current = run.clone();
            if let Err(error) = blocking(move || {
                current.update(|r| {
                    if r.postmortem_due == Some(stage) {
                        postmortem::begin_attempt(r, stage);
                    }
                })?;
                Ok(())
            })
            .await
            {
                d.fail(&run.key, &error);
                continue;
            }
            let Ok(record) = run.record() else {
                continue;
            };
            if record.postmortem_due != Some(stage) {
                continue;
            }
            let attempt_id = record.postmortem_attempt_id.clone();
            let attempt_run = run.clone();
            let attempt_id_for_lookup = attempt_id.clone();
            let saved = match blocking(move || {
                postmortem::saved_attempt(&attempt_run, &attempt_id_for_lookup)
            })
            .await
            {
                Ok(saved) => saved,
                Err(error) => {
                    d.fail(&run.key, &error);
                    continue;
                }
            };
            if let Some(saved) = saved {
                let attempt_id = record.postmortem_attempt_id.clone();
                if let Err(error) = self
                    .post(
                        d,
                        &run,
                        PostCompletion {
                            stage,
                            attempt_id,
                            written: Ok((
                                String::new(),
                                Attempt::Failure {
                                    error: String::new(),
                                    method: None,
                                },
                            )),
                            saved: Some(saved),
                            now,
                        },
                    )
                    .await
                {
                    d.fail(&run.key, &error);
                }
                continue;
            }
            let Some(plan) = plan(d.config, &record) else {
                // The routing writes no postmortem.
                let cleared = update_run(&run, move |r| {
                    if r.postmortem_due == Some(stage) && r.postmortem_attempt_id == attempt_id {
                        postmortem::clear_attempt(r);
                    }
                });
                if let Err(error) = cleared.await {
                    d.fail(&run.key, &error);
                }
                continue;
            };
            let roots = Roots::from_env(d.ctx.env);
            let path = d.ctx.env.var("PATH").map(str::to_string);
            let parent = std::env::temp_dir();
            let key = run.key.clone();
            let attempt_id_for_task = attempt_id.clone();
            let task = tokio::spawn(async move {
                let routing = plan.routing.clone();
                let (brief, input) = match tokio::task::spawn_blocking(move || {
                    let brief = postmortem::brief(&run, &record, stage);
                    let input = postmortem::input(&roots, &run, &record, &brief);
                    (brief, input)
                })
                .await
                {
                    Ok(input) => input,
                    Err(error) => {
                        return Ok((
                            routing,
                            Attempt::Failure {
                                error: format!("the postmortem input task failed: {error}"),
                                method: None,
                            },
                        ));
                    }
                };
                match write(plan, brief, input, path, parent).await {
                    Ok(written) => Ok(written),
                    Err(error) => Ok((
                        routing,
                        Attempt::Failure {
                            error: format!("{error:#}"),
                            method: None,
                        },
                    )),
                }
            });
            self.writing.insert(key, (stage, attempt_id_for_task, task));
        }
    }

    /// Keeps the outcome and queues its comment and labels, or logs why there
    /// is none. A supervisor report stays due until its feed entry is durable.
    async fn post<H>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        completion: PostCompletion,
    ) -> Result<()> {
        let PostCompletion {
            stage,
            attempt_id,
            written,
            saved,
            now,
        } = completion;
        let at = now.to_string();
        let saved_or_written = saved;
        let replaying_saved = saved_or_written.is_some();
        let persist_report =
            d.config.supervisor.is_some() || d.ctx.supervisor_dir().join("current.json").exists();
        let report_attempt_id = attempt_id.clone();
        let report_stage = stage;
        let (applied, queued, failure, told, artifact) = self
            .guarded(run, move |run, lock| {
                let record = run.record()?;
                if record.postmortem_due != Some(stage)
                    || record.postmortem_attempt_id != attempt_id
                    || (!replaying_saved
                        && stage == Stage::Workflow
                        && (record.status != crate::run::Status::Active
                            || record.workflow.confirmed
                                != Some(crate::run::WorkflowStage::InPostmortem)))
                {
                    return Ok(((false, false, None, String::new(), None), Vec::new()));
                }
                let mut artifact = if let Some(saved) = saved_or_written {
                    saved
                } else {
                    let (routing, attempt) = match written {
                        Ok(written) => written,
                        Err(error) => (
                            String::new(),
                            Attempt::Failure {
                                error: format!("{error:#}"),
                                method: None,
                            },
                        ),
                    };
                    let postmortem_profile = match &attempt {
                        Attempt::Success { method, .. }
                        | Attempt::Failure {
                            method: Some(method),
                            ..
                        } => Some(
                            ProfileObservation {
                                role: "postmortem".into(),
                                name: method.name.clone(),
                                kind: method.kind.clone(),
                                revision: Some(method.version.clone()),
                            },
                        ),
                        Attempt::Failure { method: None, .. } => None,
                    };
                    let (outcome, failure, method) = match attempt {
                        Attempt::Success { method, outcome } => (
                            ReportOutcome::Success {
                                summary: outcome.summary.clone(),
                                labels: outcome.labels.clone(),
                            },
                            None,
                            Some(method),
                        ),
                        Attempt::Failure { error, method } => (
                            ReportOutcome::Failure {
                                error: bounded_error(&error),
                            },
                            Some(bounded_error(&error)),
                            method,
                        ),
                    };
                    let report = crate::supervisor::report_from_run(
                        crate::supervisor::ReportInput {
                            run,
                            record: &record,
                            stage,
                            attempt: &attempt_id,
                            at: &at,
                            routing: &routing,
                            outcome,
                            postmortem_profile,
                        },
                    );
                    let method = method.unwrap_or(MethodUse {
                        name: String::new(),
                        kind: String::new(),
                        picked: String::new(),
                        version: String::new(),
                    });
                    SavedPostmortem {
                        schema_version: 1,
                        attempt_id: attempt_id.clone(),
                        run_key: run.key.clone(),
                        stage: Some(stage),
                        at: at.clone(),
                        method: method.name,
                        method_kind: method.kind,
                        picked: method.picked,
                        version: method.version,
                        summary: match &report.outcome {
                            ReportOutcome::Success { summary, .. } => summary.clone(),
                            ReportOutcome::Failure { .. } => String::new(),
                        },
                        labels: match &report.outcome {
                            ReportOutcome::Success { labels, .. } => labels.clone(),
                            ReportOutcome::Failure { .. } => Vec::new(),
                        },
                        failure,
                        supervisor_report: Some(report),
                    }
                };
                if artifact.attempt_id != attempt_id
                    || artifact.run_key != run.key
                    || artifact.stage != Some(stage)
                {
                    anyhow::bail!("saved postmortem attempt does not match the current due marker");
                }
                if artifact.supervisor_report.is_none() {
                    anyhow::bail!("saved postmortem attempt has no supervisor report");
                }
                let expected_source_hash = crate::supervisor::source_hash(&artifact)?;
                let report = artifact
                    .supervisor_report
                    .as_mut()
                    .expect("report was checked above");
                ensure!(
                    report.source_hash.is_empty() || report.source_hash == expected_source_hash,
                    "saved postmortem source hash does not match"
                );
                report.source_hash = expected_source_hash;
                postmortem::save_attempt(run, artifact.clone())?;
                let mut queued = false;
                let mut ops = Vec::new();
                let failure = artifact.failure.clone();
                let told = format!("with the `{}` profile ({})", artifact.method, artifact.picked);
                if let Some(failure) = &failure {
                    if record.postmortem_notice_attempt_id != attempt_id {
                        let summary = format!(
                            "The {} postmortem failed: {failure}. The issue remains in its postmortem state.",
                            stage.word()
                        );
                        crate::inbox::write_once_fields_held(
                            run,
                            lock,
                            &format!("postmortem-failure-{attempt_id}"),
                            "postmortem",
                            "failure",
                            &summary,
                        )?;
                    }
                } else if record.postmortem_queued_attempt_id != attempt_id {
                    ops.push(Op::Comment {
                        body: postmortem::comment(
                            stage,
                            &artifact.method,
                            &artifact.version,
                            &artifact.summary,
                        ),
                    });
                    if !artifact.labels.is_empty() {
                        ops.push(Op::Labels {
                            names: artifact.labels.clone(),
                        });
                    }
                    if stage == Stage::Workflow {
                        ops.push(Op::IssueState {
                            target: crate::outbox::StateTarget::Workflow(
                                crate::run::WorkflowStage::Done,
                            ),
                        });
                    }
                    for op in ops {
                        let request = match op {
                            Op::Comment { body } => {
                                crate::outbox::push_idempotent_comment_held(
                                    run,
                                    lock,
                                    &attempt_id,
                                    &body,
                                )?
                            }
                            op => crate::outbox::push_held(run, lock, op)?,
                        };
                        queued |= request.is_some();
                    }
                }
                run.update_held(lock, |r| {
                    if failure.is_some() {
                        r.postmortem_notice_attempt_id = attempt_id.clone();
                    } else {
                        r.postmortem_queued_attempt_id = attempt_id.clone();
                        if stage == Stage::Workflow {
                            r.workflow.postmortem_succeeded = true;
                        }
                    }
                    if !persist_report {
                        postmortem::clear_attempt(r);
                    }
                })?;
                Ok(((true, queued, failure, told, Some(artifact)), Vec::new()))
            })
            .await?;
        self.queued |= queued;
        if !applied {
            return Ok(());
        }
        if persist_report && let Some(artifact) = artifact {
            let store = crate::supervisor::SupervisorStore::new(d.ctx.supervisor_dir());
            blocking(move || store.materialize_report(&artifact)).await?;
            let attempt_id = report_attempt_id;
            update_run(run, move |r| {
                if r.postmortem_due == Some(report_stage) && r.postmortem_attempt_id == attempt_id {
                    postmortem::clear_attempt(r);
                }
            })
            .await?;
        }
        if let Some(failure) = failure {
            d.log.line(&format!(
                "{}: the {} postmortem failed: {failure}",
                run.key,
                stage.word()
            ));
            return Ok(());
        }
        d.log.line(&format!(
            "{}: wrote the {} postmortem {told}",
            run.key,
            stage.word()
        ));
        Ok(())
    }
}
