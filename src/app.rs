// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::io::{self, stdout};
#[allow(clippy::doc_markdown)]
/// Progress callback: (step_index, status_text, fraction)
type ProgressCb = Box<dyn FnMut(usize, &str, f64) + Send>;
use std::sync::mpsc;

use crossterm::terminal::{self, LeaveAlternateScreen};
use ratatui::prelude::*;

use crate::pipeline::{self, Context, Runner, StepSelection};
use crate::subproc::LogFn;

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
        pipeline_ctx.parts = pipeline::DerivedPartitions::default();
        let skip = skip_indices.to_vec();

        let thread = std::thread::spawn(move || {
            let log: LogFn = {
                let tx = events_tx.clone();
                Box::new(move |line: String| {
                    let _ = tx.send(TaskEvent::Log(line));
                })
            };

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

            let steps = &*pipeline::STEPS;
            let selection = StepSelection {
                start_index,
                skip_indices: skip,
            };

            match runner.execute(steps, &selection, || cancel_rx.try_recv().is_ok()) {
                Ok(()) => {
                    let _ = events_tx.send(TaskEvent::Done);
                }
                Err(e) => {
                    let failed_at = runner.step_index;
                    let short = if e.to_string().starts_with("missing dependencies:") {
                        "missing required tools".to_string()
                    } else {
                        e.to_string()
                    };
                    pipeline::rollback(&mut {
                        let tx = events_tx.clone();
                        let log: LogFn = Box::new(move |line: String| {
                            let _ = tx.send(TaskEvent::Log(line));
                        });
                        log
                    });
                    let _ = events_tx.send(TaskEvent::Failed {
                        at: failed_at,
                        error: short,
                    });
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

pub struct Installer {
    pub ctx: Context,
}

impl Installer {
    pub fn new(ctx: Context) -> Self {
        Self { ctx }
    }

    pub fn into_context(self) -> Context {
        self.ctx
    }

    pub fn run(&mut self) -> Trap {
        Self::tui_enable();
        let backend = CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend).unwrap_or_else(|e| {
            let _ = terminal::disable_raw_mode();
            eprintln!("Terminal error (Terminal::new): {e}");
            std::process::exit(1);
        });

        let result = self.run_screens(&mut terminal);

        Self::tui_disable();
        let _ = crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen);
        result
    }

    fn tui_enable() {
        let _ = terminal::enable_raw_mode();
    }

    fn tui_disable() {
        let _ = terminal::disable_raw_mode();
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
        match self.target_screen(term) {
            ScreenAction::Continue => {}
            _ => return None,
        }

        if !self.confirm(
            term,
            "Wipe target disk?",
            &format!(
                "{}\nAll data on this disk will be destroyed.",
                self.ctx.drive_label
            ),
            true,
        ) {
            return None;
        }

        match self.language_screen(term) {
            ScreenAction::Continue => {}
            _ => return None,
        }

        match self.account_screen(term) {
            ScreenAction::Continue => {}
            _ => return None,
        }

        match self.secureboot_screen(term) {
            ScreenAction::Install => {}
            ScreenAction::Reboot => return Some(Trap::Reboot),
            _ => return None,
        }

        match self.install_screen(term) {
            ScreenAction::Reboot => Some(self.reboot_screen(term)),
            _ => None,
        }
    }
}
