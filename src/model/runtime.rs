use crate::model::{ListeningPort, Project};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Agent,
    Mcp,
    Browser,
    DevServer,
    LanguageServer,
    Database,
    DevService,
    UnknownDev,
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
            Self::UnknownDev => "Other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Active,
    Persistent,
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
}
