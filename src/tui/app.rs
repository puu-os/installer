// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::sync::mpsc;
use std::time::Instant;

use crate::engine::pipeline::{self, Context, ProgressCb, Runner, SkipCb};
use crate::tui::message::{Message, bounded_text};
use crate::tui::state::AppState;

const EVENT_QUEUE_CAPACITY: usize = 256;

pub enum Trap {
    Quit,
    Reboot,
    Shell,
}

pub struct InstallTask {
    pub events_rx: mpsc::Receiver<Message>,
    pub commit_rx: mpsc::Receiver<usize>,
    cancel_tx: mpsc::SyncSender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InstallTask {
    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub fn start(ctx: &Context) -> Self {
        let (events_tx, events_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::sync_channel::<()>(1);
        let (commit_tx, commit_rx) = mpsc::sync_channel::<usize>(0);
        let mut pipeline_ctx = ctx.clone();
        pipeline_ctx.parts = crate::engine::partition::DerivedPartitions::default();

        let thread = std::thread::spawn(move || {
            let _screen_log = crate::tui::logging::show_on_screen(events_tx.clone());
            let started = Instant::now();
            tracing::info!(
                target: crate::engine::INSTALLATION_LOG_TARGET,
                "installation worker started"
            );
            let progress: ProgressCb = {
                let tx = events_tx.clone();
                Box::new(move |step_index: usize, fraction: f64| {
                    let fraction_pct = (fraction.clamp(0.0, 1.0) * 100.0).round() as u16;
                    if fraction_pct == 0
                        && pipeline::STEPS
                            .get(step_index)
                            .is_some_and(|step| step.point_of_no_return)
                        && commit_tx.send(step_index).is_err()
                    {
                        return;
                    }
                    let _ = tx.try_send(Message::InstallProgress {
                        step_index,
                        fraction_pct,
                    });
                })
            };
            let skipped: SkipCb = {
                let tx = events_tx.clone();
                Box::new(move |step_index: usize| {
                    let _ = tx.try_send(Message::InstallSkipped { step_index });
                })
            };
            let mut runner = Runner {
                ctx: pipeline_ctx,
                progress,
                skipped,
                step_index: 0,
            };
            let fail = |at: usize, error: anyhow::Error| {
                pipeline::rollback();
                let _ = events_tx.send(Message::InstallFinished {
                    at: Some(at),
                    elapsed: Some(started.elapsed()),
                    error: Some(bounded_text(&format!("{error:#}"))),
                });
            };
            match runner.execute(pipeline::STEPS, || cancel_rx.try_recv().is_ok()) {
                Ok(true) => {
                    tracing::info!(target: crate::engine::INSTALLATION_LOG_TARGET, "installation completed");
                    let _ = events_tx.send(Message::InstallFinished {
                        at: None,
                        elapsed: Some(started.elapsed()),
                        error: None,
                    });
                }
                Ok(false) => fail(runner.step_index, anyhow::anyhow!("installation cancelled")),
                Err(error) => {
                    tracing::error!(target: crate::engine::INSTALLATION_LOG_TARGET, "error: {error:#}");
                    fail(runner.step_index, error);
                }
            }
        });
        Self {
            events_rx,
            commit_rx,
            cancel_tx,
            thread: Some(thread),
        }
    }

    pub fn cancel(&self) {
        let _ = self.cancel_tx.try_send(());
    }

    pub fn cancel_and_join(self) -> Result<(), String> {
        let Self {
            events_rx,
            commit_rx,
            cancel_tx,
            thread,
        } = self;
        let _ = cancel_tx.try_send(());
        drop(events_rx);
        drop(commit_rx);
        thread.map_or(Ok(()), join_worker)
    }
}

fn join_worker(thread: std::thread::JoinHandle<()>) -> Result<(), String> {
    thread.join().map_err(|payload| {
        payload.downcast_ref::<&str>().map_or_else(
            || {
                payload
                    .downcast_ref::<String>()
                    .map_or_else(|| "unknown panic".to_string(), Clone::clone)
            },
            |message| (*message).to_string(),
        )
    })
}

pub struct Installer {
    pub ctx: Context,
    pub(crate) state: AppState,
}

impl Installer {
    pub fn new(ctx: Context) -> Self {
        Self {
            ctx,
            state: AppState::default(),
        }
    }

    pub fn into_context(self) -> Context {
        self.ctx
    }

    pub fn run(&mut self) -> Trap {
        self.run_screen()
    }
}
