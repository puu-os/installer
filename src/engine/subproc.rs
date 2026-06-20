// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::VecDeque;
use std::ffi::CStr;
use std::ffi::CString;
use std::io::{self, BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use duct::Expression;

/// Options whose next value must be redacted in log output.
const SECRET_OPTIONS: &[&str] = &["--password", "--passphrase", "--secret", "--token"];

const REDACTED: &str = "<redacted>";

pub type LogFn = Box<dyn FnMut(String) + Send>;

const OUTPUT_TAIL_LINES: usize = 12;

fn format_argv(argv: &[String]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for arg in argv {
        if redact_next {
            parts.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        if SECRET_OPTIONS.contains(&arg.as_str()) {
            parts.push(arg.clone());
            redact_next = true;
            continue;
        }
        if let Some((name, _value)) = arg.split_once('=') {
            if SECRET_OPTIONS.contains(&name) {
                parts.push(format!("{name}={REDACTED}"));
                continue;
            }
        }
        parts.push(arg.clone());
    }
    parts.join(" ")
}

fn push_tail(tail: &mut VecDeque<String>, line: String) {
    if line.trim().is_empty() {
        return;
    }
    if tail.len() >= OUTPUT_TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(line);
}

/// Build a duct expression that creates a new session and merges stderr into
/// stdout. When `chroot_dir` is set, the child chroots into it before exec,
/// replacing the external `chroot` helper. The expression is `unchecked`, so a
/// non-zero exit is reported as a code rather than an error.
fn build_expression(
    argv: &[String],
    env: Option<&[(String, String)]>,
    chroot_dir: Option<&str>,
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

    let chroot_dir = chroot_dir.map(str::to_string);
    expr.before_spawn(move |command| {
        let chroot_c = chroot_dir
            .as_deref()
            .map(CString::new)
            .transpose()
            .map_err(|_| io::Error::other("chroot path contains an interior NUL"))?;

        unsafe {
            command.pre_exec(move || enter_chroot(chroot_c.as_deref()));
        }
        Ok(())
    })
}

unsafe fn enter_chroot(target: Option<&CStr>) -> io::Result<()> {
    if let Some(dir) = target {
        if unsafe { libc::chroot(dir.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::chdir(c"/".as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    unsafe { libc::setsid() };
    Ok(())
}

/// A subprocess invocation built from [`crate::engine::pipeline::Runner::cmd`]. Merged
/// stdout+stderr is streamed to the log; a non-zero exit becomes an error.
pub struct Cmd<'a> {
    log: &'a mut LogFn,
    argv: Vec<String>,
    env: Option<&'a [(String, String)]>,
    heartbeat: Option<(Duration, &'a str)>,
    chroot_dir: Option<&'a str>,
}

impl<'a> Cmd<'a> {
    pub fn new(log: &'a mut LogFn, argv: Vec<String>) -> Self {
        Cmd {
            log,
            argv,
            env: None,
            heartbeat: None,
            chroot_dir: None,
        }
    }

    /// Run with the given environment variables set for the child.
    #[must_use]
    pub fn env(mut self, env: &'a [(String, String)]) -> Self {
        self.env = Some(env);
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
    pub fn chroot(mut self, dir: &'a str) -> Self {
        self.chroot_dir = Some(dir);
        self
    }

    /// Run the command, streaming merged stdout+stderr line-by-line to the log
    /// callback, and raise an error if the exit code is non-zero. The child
    /// chroots into `chroot_dir` before exec when it is set.
    pub fn run(self) -> Result<()> {
        let display = format_argv(&self.argv);
        (self.log)(format!("$ {display}"));

        let reader = build_expression(&self.argv, self.env, self.chroot_dir)
            .reader()
            .with_context(|| format!("failed to spawn {}", self.argv[0]))?;
        let (tx, rx) = mpsc::channel::<String>();

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
        let mut output_tail = VecDeque::new();

        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    let line = crate::engine::util::clean_log_line(&line);
                    if !line.is_empty() {
                        push_tail(&mut output_tail, line.clone());
                        (self.log)(line);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if let Some((interval, msg)) = self.heartbeat {
                if last_heartbeat.elapsed() >= interval {
                    let elapsed = started.elapsed().as_secs();
                    (self.log)(format!("{msg} ({elapsed}s elapsed)"));
                    last_heartbeat = Instant::now();
                }
            }
        }

        let rc = reader_thread.join().unwrap_or(-1);
        if rc != 0 {
            if let Some(last_line) = output_tail.back() {
                bail!("command failed (exit {rc}): {last_line}\ncommand: {display}");
            }
            bail!("command failed (exit {rc}): {display}");
        }
        Ok(())
    }
}
