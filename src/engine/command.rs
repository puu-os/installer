// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::VecDeque;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::{self, BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use duct::Expression;

use crate::engine::progress::ProgressSource;

/// Options whose next value must be redacted in log output.
const SECRET_OPTIONS: &[&str] = &["--password", "--passphrase", "--secret", "--token"];

const REDACTED: &str = "<redacted>";

/// A [`ProgressSource`] paired with a sink that forwards each sampled reading.
type ProgressTracker<'a> = (Box<dyn ProgressSource>, Box<dyn FnMut(f64) + 'a>);

const OUTPUT_TAIL_LINES: usize = 12;
const OUTPUT_QUEUE_CAPACITY: usize = 64;
const PROGRESS_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

fn format_argv(argv: &[OsString]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for arg in argv {
        let arg = arg.to_string_lossy();
        if redact_next {
            parts.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        if SECRET_OPTIONS.contains(&arg.as_ref()) {
            parts.push(arg.into_owned());
            redact_next = true;
            continue;
        }
        if let Some((name, _value)) = arg.split_once('=') {
            if SECRET_OPTIONS.contains(&name) {
                parts.push(format!("{name}={REDACTED}"));
                continue;
            }
        }
        parts.push(arg.into_owned());
    }
    parts.join(" ")
}

/// Build a duct expression that creates a new session and merges stderr into
/// stdout. When `chroot_dir` is set, the child chroots into it before exec,
/// replacing the external `chroot` helper. The expression is `unchecked`, so a
/// non-zero exit is reported as a code rather than an error.
fn build_expression(
    argv: &[OsString],
    env: Option<&[(OsString, OsString)]>,
    chroot_dir: Option<&Path>,
) -> Expression {
    let mut expr = duct::cmd(&argv[0], &argv[1..])
        .stderr_to_stdout()
        .stdin_null()
        .unchecked();

    if let Some(pairs) = env {
        for (k, v) in pairs {
            expr = expr.env(k, v);
        }
    }

    let chroot_dir = chroot_dir.map(Path::to_path_buf);
    expr.before_spawn(move |command| {
        let chroot_c = chroot_dir
            .as_deref()
            .map(|dir| CString::new(dir.as_os_str().as_bytes()))
            .transpose()
            .map_err(|_| io::Error::other("chroot path contains an interior NUL"))?;

        unsafe {
            command.pre_exec(move || enter_chroot(chroot_c.as_deref()));
        }
        Ok(())
    })
}

fn enter_chroot(target: Option<&CStr>) -> io::Result<()> {
    if let Some(dir) = target {
        nix::unistd::chroot(dir)?;
        nix::unistd::chdir(c"/")?;
    }
    let _ = nix::unistd::setsid();
    Ok(())
}

/// A subprocess invocation. Merged stdout+stderr is emitted as tracing events;
/// a non-zero exit becomes an error.
pub struct Command<'a> {
    argv: Vec<OsString>,
    env: Option<Vec<(OsString, OsString)>>,
    heartbeat: Option<(Duration, &'a str)>,
    chroot_dir: Option<PathBuf>,
    tracker: Option<ProgressTracker<'a>>,
}

impl<'a> Command<'a> {
    pub fn new<S: AsRef<OsStr>>(argv: &[S]) -> Self {
        let argv = argv.iter().map(|arg| arg.as_ref().to_os_string()).collect();
        Self {
            argv,
            env: None,
            heartbeat: None,
            chroot_dir: None,
            tracker: None,
        }
    }

    /// Run with the given environment variables set for the child.
    #[must_use]
    pub fn env<K, V>(mut self, env: &[(K, V)]) -> Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.env = Some(
            env.iter()
                .map(|(key, value)| (key.as_ref().to_os_string(), value.as_ref().to_os_string()))
                .collect(),
        );
        self
    }

    /// Emit `msg` to the log every `interval` while the command runs.
    #[must_use]
    pub fn heartbeat(mut self, interval: Duration, msg: &'a str) -> Self {
        self.heartbeat = Some((interval, msg));
        self
    }

    /// Chroot into `dir` before exec, replacing the external `chroot` helper.
    #[must_use]
    pub fn chroot(mut self, dir: impl AsRef<Path>) -> Self {
        self.chroot_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Sample `source` while the command runs and forward each reading to
    /// `emit`, surfacing real progress for a command with no native output.
    #[must_use]
    pub fn track(
        mut self,
        source: Box<dyn ProgressSource>,
        emit: Box<dyn FnMut(f64) + 'a>,
    ) -> Self {
        self.tracker = Some((source, emit));
        self
    }
    /// Run the command, emitting merged stdout+stderr line-by-line as tracing
    /// events, and raise an error if the exit code is non-zero. The child
    /// chroots into `chroot_dir` before exec when it is set.
    pub fn run(mut self) -> Result<()> {
        let command_line = self.to_string();
        tracing::info!(target: crate::engine::INSTALLATION_LOG_TARGET, "$ {command_line}");

        let reader = build_expression(&self.argv, self.env.as_deref(), self.chroot_dir.as_deref())
            .reader()
            .with_context(|| format!("failed to spawn {}", self.argv[0].to_string_lossy()))?;
        let (tx, rx) = mpsc::sync_channel::<String>(OUTPUT_QUEUE_CAPACITY);

        let reader_thread = std::thread::spawn(move || -> i32 {
            for line in BufReader::new(&reader)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
            // The read above ran to EOF, so the child has been reaped and
            // `try_wait` is guaranteed to report its exit status.
            match reader.try_wait() {
                Ok(Some(output)) => output.status.code().unwrap_or(-1),
                _ => -1,
            }
        });

        let started = Instant::now();
        let mut last_heartbeat = Instant::now();
        let mut last_sample = Instant::now();
        let mut output_tail = VecDeque::new();

        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    let line = crate::engine::util::clean_log_line(&line);
                    if !line.is_empty() {
                        if let Some((source, _)) = self.tracker.as_mut() {
                            source.observe_line(&line);
                        }
                        if output_tail.len() >= OUTPUT_TAIL_LINES {
                            output_tail.pop_front();
                        }
                        output_tail.push_back(line.clone());
                        tracing::info!(target: crate::engine::INSTALLATION_LOG_TARGET, "{line}");
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if let Some((interval, msg)) = self.heartbeat {
                if last_heartbeat.elapsed() >= interval {
                    let elapsed = started.elapsed().as_secs();
                    tracing::info!(
                        target: crate::engine::INSTALLATION_LOG_TARGET,
                        "{msg} ({elapsed}s elapsed)"
                    );
                    last_heartbeat = Instant::now();
                }
            }

            if let Some((source, emit)) = self.tracker.as_mut() {
                if last_sample.elapsed() >= PROGRESS_SAMPLE_INTERVAL {
                    if let Some(fraction) = source.fraction() {
                        emit(fraction);
                    }
                    last_sample = Instant::now();
                }
            }
        }

        let rc = reader_thread
            .join()
            .map_err(|_| anyhow::anyhow!("command output reader thread panicked"))?;
        if rc != 0 {
            if let Some(last_line) = output_tail.back() {
                bail!("command failed (exit {rc}): {last_line}\ncommand: {command_line}");
            }
            bail!("command failed (exit {rc}): {command_line}");
        }
        Ok(())
    }
}

impl std::fmt::Display for Command<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format_argv(&self.argv))
    }
}
