// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

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

fn clean_stream_line(line: &str) -> String {
    let visible = line.rsplit('\r').next().unwrap_or(line);
    visible
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect::<String>()
        .trim_end()
        .to_string()
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

/// Build a Command that creates a new session and merges stderr into stdout.
/// When `chroot_dir` is set, the child chroots into it before exec, replacing
/// the external `chroot` helper.
fn build_streaming_command(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    has_stdin: bool,
    chroot_dir: Option<&str>,
) -> Result<Command> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).stdout(Stdio::piped());

    if has_stdin {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    if let Some(pairs) = env {
        for (k, v) in pairs {
            command.env(k, v);
        }
    }

    let chroot_c = chroot_dir
        .map(std::ffi::CString::new)
        .transpose()
        .context("chroot path contains an interior NUL")?;

    // chroot (if requested), then setsid + dup2(stdout→stderr) for one merged stream.
    unsafe {
        command.pre_exec(move || {
            if let Some(ref dir) = chroot_c {
                if libc::chroot(dir.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::chdir(c"/".as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            libc::setsid();
            libc::dup2(1, 2);
            Ok(())
        });
    }

    Ok(command)
}

/// Run a command, streaming merged stdout+stderr line-by-line to the log
/// callback. Returns the exit code and a tail of command output.
#[allow(clippy::too_many_arguments)]
pub fn run(
    argv: &[String],
    log: &mut LogFn,
    stdin_data: Option<&[u8]>,
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    heartbeat_interval: Option<Duration>,
    heartbeat_msg: Option<&str>,
    chroot_dir: Option<&str>,
) -> Result<(i32, Vec<String>)> {
    let display = format_argv(argv);
    (log)(format!("$ {display}"));

    let mut command = build_streaming_command(argv, env, cwd, stdin_data.is_some(), chroot_dir)?;
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv[0]))?;

    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(data);
            drop(stdin);
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child process has no stdout"))?;
    let (tx, rx) = mpsc::channel::<String>();

    let reader_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(std::result::Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let started = Instant::now();
    let heartbeat_msg = heartbeat_msg.unwrap_or(&display);
    let mut last_heartbeat = Instant::now();
    let mut output_tail = VecDeque::new();

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                let line = clean_stream_line(&line);
                if !line.is_empty() {
                    push_tail(&mut output_tail, line.clone());
                    (log)(line);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(interval) = heartbeat_interval {
            if last_heartbeat.elapsed() >= interval {
                let elapsed = started.elapsed().as_secs();
                (log)(format!("{heartbeat_msg} ({elapsed}s elapsed)"));
                last_heartbeat = Instant::now();
            }
        }
    }

    let _ = reader_thread.join();
    let status = child.wait()?;
    Ok((
        status.code().unwrap_or(-1),
        output_tail.into_iter().collect(),
    ))
}

/// Run a command, raising an error if the exit code is non-zero.
pub fn check(
    argv: &[String],
    log: &mut LogFn,
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    heartbeat_interval: Option<Duration>,
    heartbeat_msg: Option<&str>,
) -> Result<()> {
    check_chroot(argv, log, env, cwd, heartbeat_interval, heartbeat_msg, None)
}

/// Like [`check`], but the child chroots into `chroot_dir` before exec.
#[allow(clippy::too_many_arguments)]
pub fn check_chroot(
    argv: &[String],
    log: &mut LogFn,
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    heartbeat_interval: Option<Duration>,
    heartbeat_msg: Option<&str>,
    chroot_dir: Option<&str>,
) -> Result<()> {
    let (rc, output_tail) = run(
        argv,
        log,
        None,
        env,
        cwd,
        heartbeat_interval,
        heartbeat_msg,
        chroot_dir,
    )?;
    if rc != 0 {
        let display = format_argv(argv);
        if let Some(last_line) = output_tail.last() {
            bail!("command failed (exit {rc}): {last_line}\ncommand: {display}");
        }
        bail!("command failed (exit {rc}): {display}");
    }
    Ok(())
}
