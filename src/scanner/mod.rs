pub mod ports;
pub mod processes;

use crate::model::ProcessInfo;

/// Implementations must never spawn external commands per refresh.
pub trait ProcessScanner {
    fn scan(&mut self) -> Result<Vec<ProcessInfo>>;
}

pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;
