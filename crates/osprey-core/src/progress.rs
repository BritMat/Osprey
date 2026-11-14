//! Progress reporting. The engine never prints anything itself. It sends
//! events to a callback so the CLI, a Tauri UI or BM Aegis can show them
//! however they like.

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Bytes,
    Segments,
}

#[derive(Debug, Clone)]
pub enum Progress {
    /// A new stage started (for example "video" or "audio").
    Start {
        label: String,
        total: Option<u64>,
        unit: Unit,
    },
    /// Some amount of work finished, in the unit given by `Start`.
    Advance(u64),
    /// Bytes received. Sent by stream downloads, whose `Advance` counts
    /// pieces, so speed and time left can be shown in bytes too.
    Bytes(u64),
    /// A human readable note, such as "picked 1280x720 variant".
    Status(String),
    /// The current stage is done.
    Finish,
}

pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

/// A callback that ignores everything. Handy for tests and library users.
pub fn silent() -> ProgressFn {
    Arc::new(|_| {})
}
