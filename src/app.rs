// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::io::{self, Write, stdout};
use std::sync::{Arc, Mutex, mpsc};

use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;

use crate::pipeline::{self, Context, ProgressCb, Runner, StepSelection};
use crate::subproc::LogFn;

const INSTALL_LOG_PATH: &str = "/run/puu-installer/install.log";

pub enum Trap {
    Quit,
    Reboot,
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display)]
pub(crate) enum ScreenAction {
    Install,
    Network,
    Shell,
    Reboot,
    Continue,
    Back,
    Menu,
    Quit,
}

pub enum TaskEvent {
    Log(String),
    Progress {
        step_index: usize,
        status: String,
        fraction: f64,
    },
    Done,
    Failed {
        at: usize,
        error: String,
    },
}

fn open_install_log() -> Option<std::fs::File> {
    std::fs::create_dir_all("/run/puu-installer").ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(INSTALL_LOG_PATH)
        .ok()
}

fn send_log(
    tx: &mpsc::Sender<TaskEvent>,
    log_file: &Arc<Mutex<Option<std::fs::File>>>,
    line: &str,
) {
    if let Ok(mut file) = log_file.lock() {
        if let Some(file) = file.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }
    let _ = tx.send(TaskEvent::Log(line.to_string()));
}

fn make_log(tx: mpsc::Sender<TaskEvent>, log_file: Arc<Mutex<Option<std::fs::File>>>) -> LogFn {
    Box::new(move |line: String| send_log(&tx, &log_file, &line))
}

pub struct InstallTask {
    pub events_rx: mpsc::Receiver<TaskEvent>,
    cancel_tx: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InstallTask {
    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub fn start(ctx: &Context, start_index: usize, skip_indices: &[usize]) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();

        let mut pipeline_ctx = ctx.clone();
        pipeline_ctx.parts = crate::partition::DerivedPartitions::default();
        let skip = skip_indices.to_vec();

        let thread = std::thread::spawn(move || {
            let log_file = Arc::new(Mutex::new(open_install_log()));
            let log = make_log(events_tx.clone(), log_file.clone());

            let progress: ProgressCb = {
                let tx = events_tx.clone();
                Box::new(move |step_index: usize, status: &str, fraction: f64| {
                    let _ = tx.send(TaskEvent::Progress {
                        step_index,
                        status: status.to_string(),
                        fraction,
                    });
                })
            };

            let mut runner = Runner {
                ctx: pipeline_ctx,
                log,
                progress,
                step_index: 0,
            };

            let steps = pipeline::STEPS;
            let selection = StepSelection {
                start_index,
                skip_indices: skip,
            };

            let fail = |at: usize, error: String| {
                let mut log = make_log(events_tx.clone(), log_file.clone());
                pipeline::rollback(&mut log);
                let _ = events_tx.send(TaskEvent::Failed { at, error });
            };

            match runner.execute(steps, &selection, || cancel_rx.try_recv().is_ok()) {
                Ok(true) => {
                    let _ = events_tx.send(TaskEvent::Done);
                }
                Ok(false) => {
                    fail(runner.step_index, "installation cancelled".to_string());
                }
                Err(e) => {
                    let short = if e.to_string().starts_with("missing dependencies:") {
                        "missing required tools".to_string()
                    } else {
                        e.to_string()
                    };
                    for line in short.lines() {
                        send_log(&events_tx, &log_file, &format!("error: {line}"));
                    }
                    fail(runner.step_index, short);
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
        let _ = self.cancel_tx.send(());
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
        let _ = terminal::enable_raw_mode();
        let _console_log_level = ConsoleLogLevelGuard::quiet();
        let backend = CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend).unwrap_or_else(|e| {
            let _ = terminal::disable_raw_mode();
            eprintln!("Terminal error (Terminal::new): {e}");
            std::process::exit(1);
        });

        let _ = crossterm::execute!(terminal.backend_mut(), EnterAlternateScreen);

        let result = self.run_screens(&mut terminal);

        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen);
        result
    }

    fn run_screens(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Trap {
        loop {
            match self.welcome_screen(term) {
                ScreenAction::Install => {
                    if let Some(Trap::Reboot) = self.install_flow(term) {
                        return Trap::Reboot;
                    }
                }
                ScreenAction::Network => {
                    self.run_network(term);
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

    fn install_flow(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Option<Trap> {
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
                    ScreenAction::Continue => InstallFlowStep::SecureBoot,
                    ScreenAction::Back => InstallFlowStep::WipeConfirm,
                    _ => return None,
                },
                InstallFlowStep::SecureBoot => match self.secureboot_screen(term) {
                    ScreenAction::Install => InstallFlowStep::Review,
                    ScreenAction::Back => InstallFlowStep::Identity,
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
