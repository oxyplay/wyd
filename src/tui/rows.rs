use std::collections::{HashMap, HashSet};

use crate::classify::{leftover_count, leftover_ram};
use crate::model::{
    Category, DockerResource, ListeningPort, RuntimeItem, RuntimeSnapshot, RuntimeState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Overview,
    Runtime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    All,
    Category(Category),
    Leftovers,
    Ports,
    Projects,
    Docker,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Self::All => "Runtime",
            Self::Category(c) => c.label(),
            Self::Leftovers => "Leftovers",
            Self::Ports => "Ports",
            Self::Projects => "Projects",
            Self::Docker => "Docker",
        }
    }
}

pub struct OverviewLine {
    pub section: Section,
    pub label: &'static str,
    pub count: u32,
    pub extra: String,
}

pub enum Row<'a> {
    Item {
        item: &'a RuntimeItem,
        depth: usize,
        last: bool,
    },
    Docker(&'a DockerResource),
    /// One summary row for all anonymous unused volumes.
    DockerAgg {
        count: u32,
        bytes: u64,
        oldest: i64,
    },
    Port {
        port: &'a ListeningPort,
        owner: &'a RuntimeItem,
    },
    Project {
        name: String,
        ram: u64,
        kids: usize,
    },
}

pub fn hay_hit(hay: &str, q: &str) -> bool {
    q.is_empty() || hay.to_ascii_lowercase().contains(&q.to_ascii_lowercase())
}

pub fn item_hit(item: &RuntimeItem, q: &str) -> bool {
    hay_hit(&item.title(), q)
        || hay_hit(item.category.label(), q)
        || item
            .project
            .as_ref()
            .is_some_and(|p| hay_hit(&p.name, q) || hay_hit(&p.root.to_string_lossy(), q))
        || item.ports.iter().any(|p| hay_hit(&p.label(), q))
}

fn query_ok(item: &RuntimeItem, q: &str) -> bool {
    q.is_empty() || item_hit(item, q) || item.children.iter().any(|c| query_ok(c, q))
}

fn project_ok(item: &RuntimeItem, project: Option<&str>) -> bool {
    match project {
        None => true,
        Some(name) => {
            item.project
                .as_ref()
                .is_some_and(|p| p.name.eq_ignore_ascii_case(name))
                || item.children.iter().any(|c| project_ok(c, project))
        }
    }
}

fn section_match(item: &RuntimeItem, section: Section) -> bool {
    match section {
        Section::All => true,
        Section::Category(c) => item.category == c,
        Section::Leftovers => item.state == RuntimeState::Suspicious,
        Section::Ports | Section::Projects | Section::Docker => false,
    }
}

fn section_ok(item: &RuntimeItem, section: Section) -> bool {
    section_match(item, section) || item.children.iter().any(|c| section_ok(c, section))
}

fn docker_hit(res: &DockerResource, q: &str, project: Option<&str>) -> bool {
    let q_ok = hay_hit(&res.name, q)
        || hay_hit(&res.detail, q)
        || hay_hit(res.kind.label(), q)
        || res.compose.as_ref().is_some_and(|c| hay_hit(c, q));
    let p_ok = match project {
        None => true,
        Some(name) => res.compose.as_ref().is_some_and(|c| c == name),
    };
    q_ok && p_ok
}

pub fn overview(snap: &RuntimeSnapshot) -> Vec<OverviewLine> {
    let mut counts: HashMap<Category, u32> = HashMap::new();
    let mut ram: HashMap<Category, u64> = HashMap::new();
    add_cat(&snap.logical_items, &mut counts, &mut ram);
    let mut lines = vec![OverviewLine {
        section: Section::All,
        label: "All",
        count: count_items(&snap.logical_items) as u32,
        extra: String::new(),
    }];
    const ALWAYS: [Category; 5] = [
        Category::Agent,
        Category::Mcp,
        Category::Browser,
        Category::DevServer,
        Category::Database,
    ];
    for c in ALWAYS {
        lines.push(cat_line(c, &counts, &ram, true));
    }
    for c in [
        Category::LanguageServer,
        Category::DevService,
        Category::UnknownDev,
    ] {
        if counts.get(&c).copied().unwrap_or(0) > 0 {
            lines.push(cat_line(c, &counts, &ram, false));
        }
    }
    lines.push(OverviewLine {
        section: Section::Docker,
        label: "Docker",
        count: snap.docker.resources.len() as u32,
        extra: if snap.docker.ok {
            fmt_bytes(snap.docker.disk_bytes)
        } else {
            "—".into()
        },
    });
    lines.push(OverviewLine {
        section: Section::Ports,
        label: "Ports",
        count: count_ports(&snap.logical_items) as u32,
        extra: String::new(),
    });
    lines.push(OverviewLine {
        section: Section::Projects,
        label: "Projects",
        count: count_projects(&snap.logical_items) as u32,
        extra: String::new(),
    });
    let n_left = leftover_count(&snap.logical_items) as u32;
    lines.push(OverviewLine {
        section: Section::Leftovers,
        label: "Leftovers",
        count: n_left,
        extra: if n_left == 0 {
            String::new()
        } else {
            format!(
                "~{}",
                fmt_bytes(leftover_ram(&snap.logical_items) + snap.docker.reclaimable_bytes)
            )
        },
    });
    lines
}

fn cat_line(
    c: Category,
    counts: &HashMap<Category, u32>,
    ram: &HashMap<Category, u64>,
    always: bool,
) -> OverviewLine {
    let n = counts.get(&c).copied().unwrap_or(0);
    let extra = if always || n > 0 {
        ram.get(&c).copied().map(fmt_bytes).unwrap_or_default()
    } else {
        String::new()
    };
    OverviewLine {
        section: Section::Category(c),
        label: c.label(),
        count: n,
        extra,
    }
}

fn add_cat(
    items: &[RuntimeItem],
    counts: &mut HashMap<Category, u32>,
    ram: &mut HashMap<Category, u64>,
) {
    for item in items {
        *counts.entry(item.category).or_insert(0) += item.process_ids.len() as u32;
        *ram.entry(item.category).or_insert(0) += item.memory_bytes;
        add_cat(&item.children, counts, ram);
    }
}

fn count_items(items: &[RuntimeItem]) -> usize {
    items.iter().map(|i| 1 + count_items(&i.children)).sum()
}

fn count_ports(items: &[RuntimeItem]) -> usize {
    items
        .iter()
        .map(|i| i.ports.len() + count_ports(&i.children))
        .sum()
}

fn count_projects(items: &[RuntimeItem]) -> usize {
    let mut names = std::collections::HashSet::new();
    walk_projects(items, &mut |n| {
        names.insert(n);
    });
    names.len()
}

fn walk_projects<'a>(items: &'a [RuntimeItem], f: &mut dyn FnMut(&'a str)) {
    for item in items {
        if let Some(p) = &item.project {
            f(&p.name);
        }
        walk_projects(&item.children, f);
    }
}

pub fn rows<'a>(
    snap: &'a RuntimeSnapshot,
    section: Section,
    project: Option<&str>,
    q: &str,
) -> Vec<Row<'a>> {
    let mut out = Vec::new();
    match section {
        Section::Ports => collect_ports(
            &snap.logical_items,
            project,
            q,
            &mut out,
            &mut HashSet::new(),
        ),
        Section::Projects => collect_projects(&snap.logical_items, q, &mut out),
        Section::Docker => {
            let mut agg_count = 0u32;
            let mut agg_bytes = 0u64;
            let mut agg_oldest = i64::MAX;
            for res in &snap.docker.resources {
                if res.prunable() {
                    agg_count += 1;
                    agg_bytes += res.size_bytes;
                    agg_oldest = agg_oldest.min(res.created);
                } else if docker_hit(res, q, project) {
                    out.push(Row::Docker(res));
                }
            }
            if agg_count > 0 {
                let oldest = if agg_oldest == i64::MAX {
                    0
                } else {
                    agg_oldest
                };
                out.push(Row::DockerAgg {
                    count: agg_count,
                    bytes: agg_bytes,
                    oldest,
                });
            }
        }
        other => collect_items(&snap.logical_items, 0, false, other, project, q, &mut out),
    }
    out
}

fn collect_items<'a>(
    items: &'a [RuntimeItem],
    depth: usize,
    inside: bool,
    section: Section,
    project: Option<&str>,
    q: &str,
    out: &mut Vec<Row<'a>>,
) {
    let vis: Vec<&RuntimeItem> = items
        .iter()
        .filter(|i| {
            project_ok(i, project)
                && query_ok(i, q)
                && (inside || section_match(i, section) || section_ok(i, section))
        })
        .collect();
    let last = vis.len().saturating_sub(1);
    for (i, item) in vis.into_iter().enumerate() {
        out.push(Row::Item {
            item,
            depth,
            last: i == last,
        });
        collect_items(
            &item.children,
            depth + 1,
            inside || section_match(item, section),
            section,
            project,
            q,
            out,
        );
    }
}

fn collect_ports<'a>(
    items: &'a [RuntimeItem],
    project: Option<&str>,
    q: &str,
    out: &mut Vec<Row<'a>>,
    seen: &mut HashSet<u16>,
) {
    for item in items {
        if project_ok(item, project) {
            for port in &item.ports {
                if seen.insert(port.port)
                    && (q.is_empty()
                        || hay_hit(&port.label(), q)
                        || hay_hit(&item.title(), q)
                        || item.project.as_ref().is_some_and(|p| hay_hit(&p.name, q)))
                {
                    out.push(Row::Port { port, owner: item });
                }
            }
        }
        collect_ports(&item.children, project, q, out, seen);
    }
}

fn collect_projects<'a>(items: &'a [RuntimeItem], q: &str, out: &mut Vec<Row<'a>>) {
    let mut map: HashMap<String, (u64, usize)> = HashMap::new();
    walk_projects(items, &mut |name| {
        map.entry(name.to_string()).or_insert((0, 0));
    });
    add_project_stats(items, &mut map);
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    for name in names {
        if !hay_hit(&name, q) {
            continue;
        }
        let (ram, kids) = map[&name];
        out.push(Row::Project { name, ram, kids });
    }
}

fn add_project_stats(items: &[RuntimeItem], map: &mut HashMap<String, (u64, usize)>) {
    for item in items {
        if let Some(p) = &item.project
            && let Some(e) = map.get_mut(&p.name)
        {
            e.0 += item.memory_bytes;
            e.1 += 1;
        }
        add_project_stats(&item.children, map);
    }
}

pub fn fmt_bytes(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    } else {
        format!("{}M", bytes / MIB)
    }
}

pub fn fmt_age(start_time: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    if start_time == 0 {
        return "—".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let secs = now.saturating_sub(start_time);
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
