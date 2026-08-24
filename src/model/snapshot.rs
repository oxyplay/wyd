use crate::model::{ProcessInfo, RuntimeItem};

/// Immutable view of runtime state produced by the scanners, read by the TUI.
#[derive(Debug, Default)]
pub struct RuntimeSnapshot {
    pub processes: Vec<ProcessInfo>,
    pub logical_items: Vec<RuntimeItem>,
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    /// Aggregate CPU usage across all cores, percent.
    pub cpu_percent: f32,
    /// Bumped on every scan so the TUI can skip redundant redraws.
    pub version: u64,
}
