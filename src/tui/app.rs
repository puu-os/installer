// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};

use crate::engine::command::LogFn;
use crate::engine::pipeline::{self, Context, ProgressCb, Runner, SkipCb};
use crate::tui::engine::RenderTree;
use crate::tui::message::{Message, bounded_text};
use crate::tui::render::{Terminal, fatal_io};
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
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(INSTALL_LOG_PATH)
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
                let _ = tx.send(Message::Log(bounded_text(&format!(
                    "warning: cannot write {INSTALL_LOG_PATH}: {error}"
                ))));
            }
        }
    }
    let _ = tx.send(Message::Log(bounded_text(line)));
}

fn make_log(tx: mpsc::SyncSender<Message>, log_file: Arc<Mutex<Option<std::fs::File>>>) -> LogFn {
    Box::new(move |line: String| send_log(&tx, &log_file, &line))
}

pub struct InstallTask {
    pub events_rx: mpsc::Receiver<Message>,
    cancel_tx: mpsc::SyncSender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InstallTask {
    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub fn start(ctx: &Context) -> Self {
        let (events_tx, events_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::sync_channel::<()>(1);

        let mut pipeline_ctx = ctx.clone();
        pipeline_ctx.parts = crate::engine::partition::DerivedPartitions::default();

        let thread = std::thread::spawn(move || {
            let log_file = match open_install_log() {
                Ok(file) => Some(file),
                Err(error) => {
                    let _ = events_tx.send(Message::Log(bounded_text(&format!(
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
                    let _ = tx.send(Message::InstallProgress {
                        step_index,
                        fraction_pct,
                    });
                })
            };

            let skipped: SkipCb = {
                let tx = events_tx.clone();
                Box::new(move |step_index: usize| {
                    let _ = tx.send(Message::InstallSkipped { step_index });
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
                    elapsed: None,
                    error: Some(bounded_text(&format!("{error:#}"))),
                });
            };

            match runner.execute(steps, || cancel_rx.try_recv().is_ok()) {
                Ok(true) => {
                    let _ = events_tx.send(Message::InstallFinished {
                        at: None,
                        elapsed: None,
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
            cancel_tx,
            thread: Some(thread),
        }
    }

    pub fn cancel(&self) {
        let _ = self.cancel_tx.try_send(());
    }

    pub fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
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
        let _console_log_level = ConsoleLogLevelGuard::quiet();
        let mut terminal = Terminal::new(std::io::stdout());
        if let Err(e) = terminal.activate() {
            fatal_io(&e, "activate terminal");
        }
        crate::tui::runtime::run(self, &mut terminal)
    }
}
