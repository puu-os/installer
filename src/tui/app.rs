// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};

use crate::tui::render::{Terminal, fatal_io};

use crate::engine::command::LogFn;
use crate::engine::pipeline::{self, Context, ProgressCb, Runner, SkipCb};

const INSTALL_LOG_PATH: &str = "/run/puu-installer/install.log";
const EVENT_QUEUE_CAPACITY: usize = 256;

pub enum Trap {
    Quit,
    Reboot,
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display)]
pub(crate) enum ScreenAction {
    Install,
    Shell,
    Reboot,
    Continue,
    Back,
    Menu,
    Quit,
}

pub enum TaskEvent {
    Log(String),
    Progress { step_index: usize, fraction: f64 },
    Skipped { step_index: usize },
    Done,
    Failed { at: usize, error: anyhow::Error },
}

fn open_install_log() -> io::Result<std::fs::File> {
    std::fs::create_dir_all("/run/puu-installer")?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(INSTALL_LOG_PATH)
}

fn send_log(
    tx: &mpsc::SyncSender<TaskEvent>,
    log_file: &Arc<Mutex<Option<std::fs::File>>>,
    line: &str,
) {
    if let Ok(mut log_file) = log_file.lock() {
        if let Some(file) = log_file.as_mut() {
            if let Err(error) = writeln!(file, "{line}") {
                *log_file = None;
                let _ = tx.send(TaskEvent::Log(format!(
                    "warning: cannot write {INSTALL_LOG_PATH}: {error}"
                )));
            }
        }
    }
    let _ = tx.send(TaskEvent::Log(line.to_string()));
}

fn make_log(tx: mpsc::SyncSender<TaskEvent>, log_file: Arc<Mutex<Option<std::fs::File>>>) -> LogFn {
    Box::new(move |line: String| send_log(&tx, &log_file, &line))
}

pub struct InstallTask {
    pub events_rx: mpsc::Receiver<TaskEvent>,
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
                    let _ = events_tx.send(TaskEvent::Log(format!(
                        "warning: cannot open {INSTALL_LOG_PATH}: {error}"
                    )));
                    None
                }
            };
            let log_file = Arc::new(Mutex::new(log_file));
            let log = make_log(events_tx.clone(), log_file.clone());

            let progress: ProgressCb = {
                let tx = events_tx.clone();
                Box::new(move |step_index: usize, fraction: f64| {
                    let _ = tx.send(TaskEvent::Progress {
                        step_index,
                        fraction,
                    });
                })
            };

            let skipped: SkipCb = {
                let tx = events_tx.clone();
                Box::new(move |step_index: usize| {
                    let _ = tx.send(TaskEvent::Skipped { step_index });
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
                let _ = events_tx.send(TaskEvent::Failed { at, error });
            };

            match runner.execute(steps, || cancel_rx.try_recv().is_ok()) {
                Ok(true) => {
                    let _ = events_tx.send(TaskEvent::Done);
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
}

#[derive(Clone, Copy)]
enum InstallFlowStep {
    Target,
    WipeConfirm,
    Identity,
    Timezone,
    SecureBoot,
    Review,
    Install,
}

impl Installer {
    pub fn new(ctx: Context) -> Self {
        Self { ctx }
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
        self.run_screens(&mut terminal)
    }

    fn run_screens(&mut self, term: &mut Terminal) -> Trap {
        loop {
            match self.welcome_screen(term) {
                ScreenAction::Install => {
                    if let Some(Trap::Reboot) = self.install_flow(term) {
                        return Trap::Reboot;
                    }
                }
                ScreenAction::Shell => return Trap::Shell,
                ScreenAction::Reboot => {
                    if self.confirm(
                        term,
                        "Reboot now?",
                        "The machine will reboot immediately.",
                        false,
                    ) {
                        return Trap::Reboot;
                    }
                }
                _ => return Trap::Quit,
            }
        }
    }

    fn install_flow(&mut self, term: &mut Terminal) -> Option<Trap> {
        let mut step = InstallFlowStep::Target;

        loop {
            step = match step {
                InstallFlowStep::Target => match self.target_screen(term) {
                    ScreenAction::Continue => InstallFlowStep::WipeConfirm,
                    _ => return None,
                },
                InstallFlowStep::WipeConfirm => {
                    if self.confirm(
                        term,
                        "Wipe target disk?",
                        &format!(
                            "{}\nAll data on this disk will be destroyed.",
                            self.ctx.drive_label
                        ),
                        true,
                    ) {
                        InstallFlowStep::Identity
                    } else {
                        InstallFlowStep::Target
                    }
                }
                InstallFlowStep::Identity => match self.identity_screen(term) {
                    ScreenAction::Continue => InstallFlowStep::Timezone,
                    ScreenAction::Back => InstallFlowStep::WipeConfirm,
                    _ => return None,
                },
                InstallFlowStep::Timezone => match self.timezone_screen(term) {
                    ScreenAction::Continue => InstallFlowStep::SecureBoot,
                    ScreenAction::Back => InstallFlowStep::Identity,
                    _ => return None,
                },
                InstallFlowStep::SecureBoot => match self.secureboot_screen(term) {
                    ScreenAction::Install => InstallFlowStep::Review,
                    ScreenAction::Back => InstallFlowStep::Timezone,
                    ScreenAction::Reboot => return Some(Trap::Reboot),
                    _ => return None,
                },
                InstallFlowStep::Review => match self.review_screen(term) {
                    ScreenAction::Install => InstallFlowStep::Install,
                    ScreenAction::Back => InstallFlowStep::SecureBoot,
                    _ => return None,
                },
                InstallFlowStep::Install => {
                    return match self.install_screen(term) {
                        ScreenAction::Reboot => Some(self.reboot_screen(term)),
                        _ => None,
                    };
                }
            };
        }
    }
}
