use std::path::PathBuf;

/// A detected project root (usually a git work tree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub root: PathBuf,
}
