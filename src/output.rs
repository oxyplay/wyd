use serde::Serialize;

use crate::model::{
    Category, DockerKind, DockerResource, RuntimeItem, RuntimeSnapshot, RuntimeState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Leftovers,
    Mcp,
    Agents,
    Docker,
    Project,
}

impl Filter {
    pub fn parse(kind: Option<&str>) -> Result<Self, String> {
        match kind.map(|s| s.to_ascii_lowercase()).as_deref() {
            None | Some("all") => Ok(Self::All),
            Some("leftovers" | "leftover") => Ok(Self::Leftovers),
            Some("mcp") => Ok(Self::Mcp),
            Some("agents" | "agent") => Ok(Self::Agents),
            Some("docker") => Ok(Self::Docker),
            Some("project") => Ok(Self::Project),
            Some(other) => Err(format!(
                "unknown filter {other:?}; use leftovers, mcp, agents, docker, project"
            )),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonReport {
    pub runtime: Vec<JsonItem>,
    pub docker: Vec<JsonDocker>,
}

#[derive(Debug, Serialize)]
pub struct JsonItem {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub memory_bytes: u64,
    pub status: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<JsonItem>,
}

#[derive(Debug, Serialize)]
pub struct JsonDocker {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub status: String,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose: Option<String>,
    /// True only for volumes Docker created as anonymous.
    pub anonymous: bool,
    /// Unix seconds created; 0 = unknown.
    #[serde(skip_serializing_if = "is_zero")]
    pub created: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

pub fn render_json(snap: &RuntimeSnapshot, filter: Filter, project: Option<&str>) -> String {
    let report = report(snap, filter, project);
    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
}

pub fn render_plain(snap: &RuntimeSnapshot, filter: Filter, project: Option<&str>) -> String {
    let report = report(snap, filter, project);
    let mut lines = Vec::new();
    for item in &report.runtime {
        push_plain(&mut lines, item, 0);
    }
    for d in &report.docker {
        lines.push(format!(
            "docker  {:<12} {:<16} {:>10}  {}",
            d.kind, d.name, d.size_bytes, d.status
        ));
    }
    if lines.is_empty() {
        lines.push("none".into());
    }
    lines.join("\n")
}

fn report(snap: &RuntimeSnapshot, filter: Filter, project: Option<&str>) -> JsonReport {
    let runtime = match filter {
        Filter::Docker => Vec::new(),
        Filter::Leftovers => filter_items(&snap.logical_items, project, |i| {
            i.state == RuntimeState::Suspicious
        }),
        Filter::Mcp => filter_items(&snap.logical_items, project, |i| {
            i.category == Category::Mcp
        }),
        Filter::Agents => filter_items(&snap.logical_items, project, |i| {
            i.category == Category::Agent
        }),
        Filter::All | Filter::Project => filter_items(&snap.logical_items, project, |_| true),
    };
    let docker = match filter {
        Filter::All | Filter::Docker | Filter::Leftovers => snap
            .docker
            .resources
            .iter()
            .filter(|r| filter != Filter::Leftovers || docker_is_leftover(r))
            .map(json_docker)
            .collect(),
        _ => Vec::new(),
    };
    JsonReport { runtime, docker }
}

fn filter_items(
    items: &[RuntimeItem],
    project: Option<&str>,
    keep: impl Fn(&RuntimeItem) -> bool + Copy,
) -> Vec<JsonItem> {
    let mut out = Vec::new();
    collect_items(items, project, keep, &mut out);
    out
}

fn collect_items(
    items: &[RuntimeItem],
    project: Option<&str>,
    keep: impl Fn(&RuntimeItem) -> bool + Copy,
    out: &mut Vec<JsonItem>,
) {
    for item in items {
        if keep(item) && project_ok(item, project) {
            out.push(json_item(item, keep, project));
        } else {
            collect_items(&item.children, project, keep, out);
        }
    }
}

fn json_item(
    item: &RuntimeItem,
    keep: impl Fn(&RuntimeItem) -> bool + Copy,
    project: Option<&str>,
) -> JsonItem {
    JsonItem {
        kind: type_name(item),
        name: item.title(),
        pid: item.root_pid,
        project: item.project.as_ref().map(|p| p.name.clone()),
        memory_bytes: item.memory_bytes,
        status: status(item),
        ports: item.ports.iter().map(|p| p.port).collect(),
        reasons: item
            .suspicion
            .as_ref()
            .map(|s| s.reasons.iter().map(|r| r.as_str().to_string()).collect())
            .unwrap_or_default(),
        children: item
            .children
            .iter()
            .filter(|c| keep(c) && project_ok(c, project))
            .map(|c| json_item(c, keep, project))
            .collect(),
    }
}

fn json_docker(r: &DockerResource) -> JsonDocker {
    JsonDocker {
        kind: match r.kind {
            DockerKind::Container => "container",
            DockerKind::DanglingImage => "dangling-image",
            DockerKind::Volume => "volume",
            DockerKind::BuildCache => "build-cache",
        }
        .into(),
        name: r.name.clone(),
        status: r.detail.clone(),
        size_bytes: r.size_bytes,
        compose: r.compose.clone(),
        anonymous: r.anonymous,
        created: r.created,
    }
}

fn type_name(item: &RuntimeItem) -> String {
    if item.state == RuntimeState::Suspicious {
        return "leftover".into();
    }
    match item.category {
        Category::Agent => "agent",
        Category::Mcp => "mcp",
        Category::Browser => "browser",
        Category::DevServer => "dev-server",
        Category::LanguageServer => "language-server",
        Category::Database => "database",
        Category::DevService => "dev-service",
        Category::UnknownDev => "other",
    }
    .into()
}

fn status(item: &RuntimeItem) -> String {
    match item.state {
        RuntimeState::Active => "active",
        RuntimeState::Persistent => "persistent",
        RuntimeState::Suspicious => "leftover",
    }
    .into()
}

fn docker_is_leftover(r: &DockerResource) -> bool {
    matches!(r.kind, DockerKind::DanglingImage | DockerKind::BuildCache)
        || r.detail == "stopped"
        || r.detail == "unused"
}

fn project_ok(item: &RuntimeItem, project: Option<&str>) -> bool {
    let Some(want) = project else {
        return true;
    };
    item.project
        .as_ref()
        .is_some_and(|p| p.name.eq_ignore_ascii_case(want))
        || item.children.iter().any(|c| project_ok(c, project))
}

fn push_plain(lines: &mut Vec<String>, item: &JsonItem, depth: usize) {
    let pad = "  ".repeat(depth);
    let pid = item.pid.map(|p| format!(" pid={p}")).unwrap_or_default();
    let proj = item
        .project
        .as_deref()
        .map(|p| format!(" project={p}"))
        .unwrap_or_default();
    let reasons = if item.reasons.is_empty() {
        String::new()
    } else {
        format!("  {}", item.reasons.join("; "))
    };
    lines.push(format!(
        "{pad}{:<12} {}{pid}{proj}  {}{reasons}",
        item.kind, item.name, item.status
    ));
    for c in &item.children {
        push_plain(lines, c, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{group, mark};
    use crate::config::Config;
    use crate::model::ProcessInfo;

    fn proc(pid: u32, ppid: Option<u32>, name: &str, cmd: &[&str]) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 10,
            start_time: 1,
            tty: None,
        }
    }

    fn snap() -> RuntimeSnapshot {
        let processes = vec![
            proc(1, None, "launchd", &["launchd"]),
            proc(10, Some(1), "omp", &["omp"]),
            proc(11, Some(10), "node", &["node", "chrome-devtools-mcp"]),
            proc(20, Some(1), "node", &["node", "playwright-mcp"]),
        ];
        let mut logical_items = group(&processes);
        mark(&mut logical_items, &processes, &Config::default());
        RuntimeSnapshot {
            logical_items,
            processes,
            ..RuntimeSnapshot::default()
        }
    }

    #[test]
    fn json_leftovers_only_detached_mcp() {
        let text = render_json(&snap(), Filter::Leftovers, None);
        assert!(text.contains("playwright-mcp"), "{text}");
        assert!(text.contains("\"type\": \"leftover\""), "{text}");
        assert!(!text.contains("omp"), "{text}");
    }

    #[test]
    fn json_mcp_includes_owned_and_detached() {
        let text = render_json(&snap(), Filter::Mcp, None);
        assert!(text.contains("chrome-devtools-mcp"), "{text}");
        assert!(text.contains("playwright-mcp"), "{text}");
    }

    #[test]
    fn plain_none_when_empty_filter() {
        let empty = RuntimeSnapshot::default();
        assert_eq!(render_plain(&empty, Filter::All, None), "none");
    }

    #[test]
    fn unknown_filter_errors() {
        assert!(Filter::parse(Some("wat")).is_err());
        assert_eq!(Filter::parse(Some("leftovers")).unwrap(), Filter::Leftovers);
    }
}
