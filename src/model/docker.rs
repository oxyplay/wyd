/// One Docker resource shown in the TUI list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerResource {
    pub kind: DockerKind,
    pub id: String,
    pub name: String,
    pub detail: String,
    pub size_bytes: u64,
    pub compose: Option<String>,
    /// Volumes always; UI requires `D` to delete.
    pub persistent: bool,
    /// Anonymous volume (hash-named, no compose project). Volumes only.
    pub anonymous: bool,
    /// Unix seconds when created; 0 = unknown.
    pub created: i64,
}

impl DockerResource {
    /// Running container — the only resource wyd can stop.
    pub fn running(&self) -> bool {
        self.kind == DockerKind::Container && self.detail == "running"
    }

    /// Anonymous volume not attached to any container — `y` on the prune
    /// popup deletes exactly these.
    pub fn prunable(&self) -> bool {
        self.kind == DockerKind::Volume && self.anonymous && self.detail == "unused"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerKind {
    Container,
    DanglingImage,
    Volume,
    BuildCache,
}

impl DockerKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::DanglingImage => "dangling image",
            Self::Volume => "volume",
            Self::BuildCache => "build cache",
        }
    }
}

/// Latest Docker Engine view. `ok == false` is a degraded state, not a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerSnapshot {
    pub ok: bool,
    pub note: String,
    pub disk_bytes: u64,
    pub reclaimable_bytes: u64,
    pub resources: Vec<DockerResource>,
}

impl Default for DockerSnapshot {
    fn default() -> Self {
        Self {
            ok: false,
            note: "not running".into(),
            disk_bytes: 0,
            reclaimable_bytes: 0,
            resources: Vec::new(),
        }
    }
}

impl DockerSnapshot {
    pub fn down(note: impl Into<String>) -> Self {
        Self {
            ok: false,
            note: note.into(),
            ..Self::default()
        }
    }
}

impl DockerSnapshot {
    /// (count, bytes) of anonymous unused volumes — what `P` offers to prune.
    pub fn prunable_stats(&self) -> (usize, u64) {
        let prunable: Vec<&DockerResource> =
            self.resources.iter().filter(|r| r.prunable()).collect();
        let bytes = prunable.iter().map(|r| r.size_bytes).sum();
        (prunable.len(), bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(name: &str, anonymous: bool, attached: bool) -> DockerResource {
        DockerResource {
            kind: DockerKind::Volume,
            id: name.into(),
            name: name.into(),
            detail: if attached { "attached" } else { "unused" }.into(),
            size_bytes: 100,
            compose: None,
            persistent: true,
            anonymous,
            created: 0,
        }
    }

    #[test]
    fn prunable_stats_counts_only_anonymous_unused_volumes() {
        let snap = DockerSnapshot {
            ok: true,
            note: String::new(),
            disk_bytes: 0,
            reclaimable_bytes: 0,
            resources: vec![
                vol("anon_unused", true, false),
                vol("anon_attached", true, true),
                vol("named_unused", false, false),
                DockerResource {
                    kind: DockerKind::Container,
                    id: "c".into(),
                    name: "c".into(),
                    detail: "running".into(),
                    size_bytes: 5,
                    compose: None,
                    persistent: false,
                    anonymous: false,
                    created: 0,
                },
            ],
        };
        let (count, bytes) = snap.prunable_stats();
        assert_eq!(count, 1);
        assert_eq!(bytes, 100);
    }
}
