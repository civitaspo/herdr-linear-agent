//! The reconciler: the one task that decides. Herdr and Linear events only
//! wake it; each pass reads one `session.snapshot` and the run folders and
//! acts on them.
//!
//! Not written yet: this stub holds its inputs and waits for shutdown, so
//! `ticker run` keeps the lock, follows Herdr and reads Linear.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{Notify, mpsc, watch};

use super::Log;
use crate::config::Config;
use crate::herdr::{Herdr, Link};
use crate::linear::task::{LinearEvent, LinearLevel, RunQuery};
use crate::paths::Ctx;

/// Everything a pass reads from and writes to.
// The reconciler reads these fields once it is written.
#[allow(dead_code)]
pub struct Inputs<'a, H> {
    pub ctx: &'a Ctx<'a>,
    pub config: &'a Config,
    pub herdr: H,
    /// The configured session's socket path, which keys progress records.
    pub socket: String,
    pub log: Arc<Log>,
    /// Wakes on every Herdr event; `connected` says whether snapshots can work.
    pub link: watch::Receiver<Link>,
    pub level: watch::Receiver<LinearLevel>,
    pub events: mpsc::Receiver<LinearEvent>,
    /// What the Linear task reads and flushes.
    pub queries: watch::Sender<Vec<RunQuery>>,
    /// `notify_one` after queuing outbox requests, so they go out at once.
    pub linear_wake: Arc<Notify>,
    /// Becomes `true` when the ticker should exit.
    pub shutdown: watch::Receiver<bool>,
}

pub async fn run<H: Herdr>(mut inputs: Inputs<'_, H>) -> Result<()> {
    let _ = inputs.shutdown.wait_for(|stop| *stop).await;
    Ok(())
}
