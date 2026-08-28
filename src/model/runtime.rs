use crate::model::{ListeningPort, Project};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Agent,
    Mcp,
    Browser,
    DevServer,
    LanguageServer,
    Database,
    Worker,
    UnknownDev,
    DevService,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agents",
            Self::Mcp => "MCP",
            Self::Browser => "Browsers",
            Self::DevServer => "Dev servers",
            Self::LanguageServer => "Language servers",
            Self::Database => "Databases",
            Self::DevService => "Dev services",
            Self::Worker => "Workers",
            Self::UnknownDev => "Other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Active,
    Persistent,
    Suspicious,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspicionReason {
    ParentExited,
    OwningAgentMissing,
    McpOwnerMissing,
    HeadlessBrowserDetached,
    LongRunningDevServer,
    LongRunningWorker,
    SessionOwnerEnded,
}

impl SuspicionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParentExited => "parent exited / re-parented",
            Self::OwningAgentMissing => "owning agent missing",
            Self::McpOwnerMissing => "MCP without owner",
            Self::HeadlessBrowserDetached => "detached headless browser",
            Self::LongRunningDevServer => "dev server older than threshold",
            Self::LongRunningWorker => "background worker older than threshold",
            Self::SessionOwnerEnded => "owning session ended",
        }
    }

    /// One shared human-readable explanation per reason, used by both the
    /// TUI details and the web dashboard. Heuristic evidence is worded with
    /// hedges ("may", "associated") — it must not sound exact.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::ParentExited => {
                "The original parent is gone or the process was re-parented. It may \
                 have outlived the process that launched it."
            }
            Self::OwningAgentMissing => {
                "WYD can no longer find the agent associated with this runtime."
            }
            Self::McpOwnerMissing => {
                "This MCP server is still running but no owning agent is currently present."
            }
            Self::HeadlessBrowserDetached => {
                "A headless browser is still running after its controller or owning \
                 runtime disappeared."
            }
            Self::LongRunningDevServer => {
                "This dev server has been running longer than the configured leftover \
                 threshold."
            }
            Self::LongRunningWorker => {
                "This background worker has been running longer than the configured \
                 leftover threshold."
            }
            Self::SessionOwnerEnded => {
                "WYD previously observed this resource under a runtime session that has \
                 ended, but the resource is still alive."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suspicion {
    pub score: u8,
    pub reasons: Vec<SuspicionReason>,
}

/// One grouped development runtime item (not a raw process).
#[derive(Debug, Clone)]
pub struct RuntimeItem {
    pub category: Category,
    pub display_name: String,
    pub root_pid: Option<u32>,
    pub process_ids: Vec<u32>,
    pub memory_bytes: u64,
    pub cpu_percent: f32,
    pub state: RuntimeState,
    pub suspicion: Option<Suspicion>,
    pub ports: Vec<ListeningPort>,
    pub project: Option<Project>,
    pub children: Vec<RuntimeItem>,
}

impl RuntimeItem {
    pub fn title(&self) -> String {
        let n = self.process_ids.len();
        if n > 1 {
            format!("{} ×{n}", self.display_name)
        } else {
            self.display_name.clone()
        }
    }

    /// Semantic verdict, separate from the raw numeric score. The score is a
    /// decision output; the verdict is how WYD labels the decision.
    pub fn verdict(&self) -> &'static str {
        match self.state {
            RuntimeState::Suspicious => "leftover candidate",
            RuntimeState::Persistent => "persistent",
            RuntimeState::Active => "active",
        }
    }
}
