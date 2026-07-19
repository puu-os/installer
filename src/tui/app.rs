// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::cell::RefCell;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use crate::engine::command::LogFn;
use crate::engine::pipeline::{self, Context, ProgressCb, Runner, SkipCb};
use crate::tui::engine::RenderTree;
use crate::tui::message::{Message, bounded_text};
use crate::tui::render::{Terminal, fatal_io, install_panic_hook};
use crate::tui::state::AppState;

const INSTALL_LOG_PATH: &str = "/run/puu-installer/install.log";
const EVENT_QUEUE_CAPACITY: usize = 256;

pub enum Trap {
    Quit,
    Reboot,
    Shell,
}

fn open_install_log() -> io::Result<std::fs::File> {
    std::fs::create_dir_all("/run/puu-installer")?;
    std::fs::set_permissions("/run/puu-installer", PermissionsExt::from_mode(0o700))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(INSTALL_LOG_PATH)?;
    file.set_permissions(PermissionsExt::from_mode(0o600))?;
    Ok(file)
}

fn send_log(
    tx: &mpsc::SyncSender<Message>,
    log_file: &Arc<Mutex<Option<std::fs::File>>>,
    line: &str,
) {
    if let Ok(mut log_file) = log_file.lock() {
        if let Some(file) = log_file.as_mut() {
            if let Err(error) = writeln!(file, "{line}") {
                *log_file = None;
                let _ = tx.try_send(Message::Log(bounded_text(&format!(
                    "warning: cannot write {INSTALL_LOG_PATH}: {error}"
                ))));
            }
        }
    }
    let _ = tx.try_send(Message::Log(bounded_text(line)));
}

fn make_log(tx: mpsc::SyncSender<Message>, log_file: Arc<Mutex<Option<std::fs::File>>>) -> LogFn {
    Box::new(move |line: String| send_log(&tx, &log_file, &line))
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
            let started = Instant::now();
            let log_file = match open_install_log() {
                Ok(file) => Some(file),
                Err(error) => {
                    let _ = events_tx.try_send(Message::Log(bounded_text(&format!(
                        "warning: cannot open {INSTALL_LOG_PATH}: {error}"
                    ))));
                    None
                }
            };
            let log_file = Arc::new(Mutex::new(log_file));
            let log = make_log(events_tx.clone(), log_file.clone());

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
                log,
                progress,
                skipped,
                step_index: 0,
            };

            let steps = pipeline::STEPS;

            let fail = |at: usize, error: anyhow::Error| {
                let mut log = make_log(events_tx.clone(), log_file.clone());
                pipeline::rollback(&mut log);
                let _ = events_tx.send(Message::InstallFinished {
                    at: Some(at),
                    elapsed: Some(started.elapsed()),
                    error: Some(bounded_text(&format!("{error:#}"))),
                });
            };

            match runner.execute(steps, || cancel_rx.try_recv().is_ok()) {
                Ok(true) => {
                    let _ = events_tx.send(Message::InstallFinished {
                        at: None,
                        elapsed: Some(started.elapsed()),
                        error: None,
                    });
                }
                Ok(false) => {
                    fail(runner.step_index, anyhow::anyhow!("installation cancelled"));
                }
                Err(e) => {
                    send_log(&events_tx, &log_file, &format!("error: {e:#}"));
                    fail(runner.step_index, e);
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

        let Some(thread) = thread else {
            return Ok(());
        };
        join_worker(thread)
    }

    pub fn join(&mut self) -> Result<(), String> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        join_worker(thread)
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

struct ConsoleLogLevelGuard {
    original: Option<String>,
}

impl ConsoleLogLevelGuard {
    fn quiet() -> Self {
        let path = "/proc/sys/kernel/printk";
        let original = std::fs::read_to_string(path).ok();
        if original.is_some() {
            let _ = std::fs::write(path, "1\n");
        }
        Self { original }
    }
}

impl Drop for ConsoleLogLevelGuard {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            let _ = std::fs::write("/proc/sys/kernel/printk", original);
        }
    }
}

pub struct Installer {
    pub ctx: Context,
    pub(crate) render_tree: RefCell<RenderTree>,
    pub(crate) state: AppState,
}

impl Installer {
    pub fn new(ctx: Context) -> Self {
        Self {
            ctx,
            render_tree: RefCell::new(RenderTree::new()),
            state: AppState::default(),
        }
    }

    pub fn into_context(self) -> Context {
        self.ctx
    }

    pub fn run(&mut self) -> Trap {
        let _panic_hook = install_panic_hook();
        let _console_log_level = ConsoleLogLevelGuard::quiet();
        let mut terminal = Terminal::new(std::io::stdout());
        if let Err(e) = terminal.activate() {
            fatal_io(&e, "activate terminal");
        }
        crate::tui::runtime::run(self, &mut terminal)
    }
}
