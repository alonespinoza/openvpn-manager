//! Live capture of session log events.
//!
//! openvpn3's log service has no API for reading back a completed session's
//! log, and its `ProxyLogEvents` method is restricted to the `openvpn` user —
//! neither is available to an unprivileged front-end. So whatever the applet
//! captures while the session runs is all there will ever be (KTD4), which is
//! why `LogForward(true)` has to be enabled before `Connect` rather than after.

use std::collections::HashMap;
use std::collections::VecDeque;

/// One `Log` signal, or a note the applet wrote itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub group: u32,
    pub level: u32,
    pub message: String,
}

impl LogEntry {
    /// A line the applet generated rather than received — an unmapped status
    /// (KTD3) or an unsupported attention request (KTD8). Without these the
    /// user sees a failed icon and a log that never explains itself.
    pub fn note(message: impl Into<String>) -> Self {
        Self {
            group: 0,
            level: 0,
            message: message.into(),
        }
    }
}

/// What the log window has to show.
#[derive(Debug)]
pub enum SessionLog<'a> {
    Captured {
        path: &'a str,
        entries: Vec<&'a LogEntry>,
    },
    /// A session exists but nothing was captured for it — typically one adopted
    /// from outside the applet, which never had log forwarding enabled. The
    /// window says so rather than rendering blank, because a blank pane reads
    /// as either a bug or a clean run and it is neither.
    NotCaptured { path: &'a str },
    /// No session has been seen at all.
    None,
}

/// Bounded per-session log capture.
///
/// Only the current session's log is kept once a session goes away. This is a
/// failure escape hatch, not a history: R13 asks for the most recent attempt.
#[derive(Debug)]
pub struct LogStore {
    capacity: usize,
    /// Buffers for sessions believed to still exist.
    buffers: HashMap<String, VecDeque<LogEntry>>,
    /// The current session's buffer, kept after its object was removed so the
    /// log survives long enough for the user to open the window.
    retained: Option<(String, VecDeque<LogEntry>)>,
    most_recent: Option<String>,
}

impl LogStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            buffers: HashMap::new(),
            retained: None,
            most_recent: None,
        }
    }

    /// Begin capturing for a session the applet started. Supersedes any
    /// retained log — a new attempt is not a continuation of the last one.
    pub fn begin_session(&mut self, path: impl Into<String>) {
        let path = path.into();
        self.retained = None;
        self.buffers.insert(path.clone(), VecDeque::new());
        self.most_recent = Some(path);
    }

    /// Track a session the applet did not start (AE4). No buffer is created:
    /// log forwarding was never enabled for it, so there is nothing to capture
    /// and nothing to pretend about.
    pub fn adopt_external_session(&mut self, path: impl Into<String>) {
        let path = path.into();
        self.retained = None;
        self.most_recent = Some(path);
    }

    /// Append a received `Log` signal. Signals for sessions the applet is not
    /// tracking are dropped rather than creating a phantom buffer.
    pub fn push(&mut self, path: &str, entry: LogEntry) {
        if let Some(buffer) = self.buffers.get_mut(path) {
            if buffer.len() == self.capacity {
                buffer.pop_front();
            }
            buffer.push_back(entry);
        }
    }

    /// Append a line the applet wrote. Unlike `push`, this creates a buffer if
    /// none exists — an unsupported attention request can fire before anything
    /// else has been captured, and that reason must not be lost.
    pub fn note(&mut self, path: &str, message: impl Into<String>) {
        let capacity = self.capacity;
        let buffer = self
            .buffers
            .entry(path.to_owned())
            .or_default();

        if buffer.len() == capacity {
            buffer.pop_front();
        }
        buffer.push_back(LogEntry::note(message));

        if self.most_recent.is_none() {
            self.most_recent = Some(path.to_owned());
        }
    }

    /// A session object went away. The current session's log is kept so it can
    /// still be read after a failure; anything older is dropped.
    pub fn remove_session(&mut self, path: &str) {
        let Some(buffer) = self.buffers.remove(path) else {
            return;
        };

        if self.most_recent.as_deref() == Some(path) {
            self.retained = Some((path.to_owned(), buffer));
        }
    }

    pub fn has_buffer(&self, path: &str) -> bool {
        self.buffers.contains_key(path)
            || self
                .retained
                .as_ref()
                .is_some_and(|(retained, _)| retained == path)
    }

    /// What the log window should render.
    pub fn most_recent(&self) -> SessionLog<'_> {
        let Some(path) = self.most_recent.as_deref() else {
            return SessionLog::None;
        };

        let buffer = self
            .buffers
            .get(path)
            .or_else(|| match &self.retained {
                Some((retained, buffer)) if retained == path => Some(buffer),
                _ => None,
            });

        match buffer {
            Some(buffer) if !buffer.is_empty() => SessionLog::Captured {
                path,
                entries: buffer.iter().collect(),
            },
            // An existing-but-empty buffer gets the same treatment as none at
            // all: there is nothing to read either way.
            _ => SessionLog::NotCaptured { path },
        }
    }
}
