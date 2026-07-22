// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::cell::RefCell;
use std::marker::PhantomData;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::rc::Rc;
use std::sync::mpsc;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

use crate::tui::message::{Message, bounded_text};

thread_local! {
    static SCREEN_LOG: RefCell<Option<mpsc::SyncSender<Message>>> = const { RefCell::new(None) };
}

pub(crate) struct ScreenLogGuard(PhantomData<Rc<()>>);

impl Drop for ScreenLogGuard {
    fn drop(&mut self) {
        SCREEN_LOG.with(|log| {
            log.borrow_mut().take();
        });
    }
}

pub(crate) fn show_on_screen(sender: mpsc::SyncSender<Message>) -> ScreenLogGuard {
    SCREEN_LOG.with(|log| {
        let previous = log.borrow_mut().replace(sender);
        assert!(previous.is_none(), "screen logging is already active");
    });
    ScreenLogGuard(PhantomData)
}

pub(crate) fn init() -> anyhow::Result<()> {
    let directory = "/run/puu-installer";
    std::fs::create_dir_all(directory)?;
    std::fs::set_permissions(directory, PermissionsExt::from_mode(0o700))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/run/puu-installer/install.log")?;
    file.set_permissions(PermissionsExt::from_mode(0o600))?;

    tracing_subscriber::registry()
        .with(ScreenLogLayer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file),
        )
        .try_init()
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

struct ScreenLogLayer;

impl<S> Layer<S> for ScreenLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        if metadata.target() != crate::engine::INSTALLATION_LOG_TARGET
            || !matches!(*metadata.level(), Level::ERROR | Level::WARN | Level::INFO)
        {
            return;
        }

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let Some(message) = visitor.message else {
            return;
        };
        SCREEN_LOG.with(|log| {
            let Some(sender) = log.borrow().as_ref().cloned() else {
                return;
            };
            let _ = sender.try_send(Message::Log(bounded_text(&message)));
        });
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}
