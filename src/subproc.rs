// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

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

/// Build a Command that creates a new session and merges stderr into stdout.
fn build_streaming_command(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    has_stdin: bool,
) -> Command {
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

    // setsid + dup2(stdout→stderr) so we read one merged stream.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            libc::dup2(1, 2);
            Ok(())
        });
    }

    command
}

/// Run a command, streaming merged stdout+stderr line-by-line to the log
/// callback.  Returns the exit code.
pub fn run(
    argv: &[String],
    log: &mut LogFn,
    stdin_data: Option<&[u8]>,
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    heartbeat_interval: Option<Duration>,
    heartbeat_msg: Option<&str>,
) -> Result<i32> {
    let display = format_argv(argv);
    (log)(format!("$ {display}"));

    let mut command = build_streaming_command(argv, env, cwd, stdin_data.is_some());
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

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                (log)(line);
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
    Ok(status.code().unwrap_or(-1))
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
    let rc = run(argv, log, None, env, cwd, heartbeat_interval, heartbeat_msg)?;
    if rc != 0 {
        bail!("command failed (exit {rc}): {}", format_argv(argv));
    }
    Ok(())
}

/// Run a command and capture its stdout.  Raises an error on non-zero exit.
pub fn capture(
    argv: &[String],
    stdin_data: Option<&[u8]>,
    env: Option<&[(String, String)]>,
) -> Result<String> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if stdin_data.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    if let Some(pairs) = env {
        for (k, v) in pairs {
            command.env(k, v);
        }
    }

    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv[0]))?;

    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(data);
            drop(stdin);
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "command failed (exit {}): {}\n{}",
            output.status.code().unwrap_or(-1),
            format_argv(argv),
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
