use std::collections::HashSet;

use bollard::Docker;
use bollard::models::{
    ContainerSummary, ContainerSummaryStateEnum, ImageSummary, SystemDataUsageResponse, Volume,
    VolumeListResponse,
};
use bollard::query_parameters::{ListContainersOptionsBuilder, ListImagesOptionsBuilder};

use crate::model::{DockerKind, DockerResource, DockerSnapshot};

const COMPOSE_PROJECT: &str = "com.docker.compose.project";

/// Blocking scan for the background thread. Never panics; socket-down is `ok: false`.
pub fn scan_blocking() -> DockerSnapshot {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(scan()),
        Err(err) => DockerSnapshot::down(err.to_string()),
    }
}

async fn scan() -> DockerSnapshot {
    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(_) => return DockerSnapshot::down("not running"),
    };
    if docker.ping().await.is_err() {
        return DockerSnapshot::down("not running");
    }

    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .size(true)
                .build(),
        ))
        .await
        .unwrap_or_default();
    let images = docker
        .list_images(Some(ListImagesOptionsBuilder::default().all(true).build()))
        .await
        .unwrap_or_default();
    let volumes = docker
        .list_volumes(Some(
            bollard::query_parameters::ListVolumesOptionsBuilder::default().build(),
        ))
        .await
        .unwrap_or(VolumeListResponse {
            volumes: None,
            warnings: None,
        });
    let usage = docker.df(None).await.ok();

    assemble(&containers, &images, &volumes, usage.as_ref())
}

pub fn assemble(
    containers: &[ContainerSummary],
    images: &[ImageSummary],
    volumes: &VolumeListResponse,
    usage: Option<&SystemDataUsageResponse>,
) -> DockerSnapshot {
    let attached = attached_volumes(containers);
    let mut resources = Vec::new();

    for c in containers {
        resources.push(container_row(c));
    }
    for img in images {
        if dangling(img) {
            resources.push(dangling_row(img));
        }
    }
    if let Some(vols) = &volumes.volumes {
        for vol in vols {
            resources.push(volume_row(vol, attached.contains(&vol.name)));
        }
    }
    if let Some(cache) = usage.and_then(|u| u.build_cache_usage.as_ref()) {
        let size = cache.reclaimable.unwrap_or(0).max(0) as u64;
        if size > 0 {
            resources.push(DockerResource {
                kind: DockerKind::BuildCache,
                id: "*build-cache".into(),
                name: "build cache".into(),
                detail: "reclaimable".into(),
                size_bytes: size,
                compose: None,
                persistent: false,
                anonymous: false,
                created: 0,
            });
        }
    }
    // Running containers on top; stable — keeps kind grouping for the rest.
    resources.sort_by_key(|r| !r.running());

    let disk_bytes = resources.iter().map(|r| r.size_bytes).sum();
    let reclaimable_bytes = resources
        .iter()
        .filter(|r| {
            matches!(r.kind, DockerKind::DanglingImage | DockerKind::BuildCache)
                || (r.kind == DockerKind::Container && r.detail == "stopped")
                || (r.kind == DockerKind::Volume && r.detail == "unused")
        })
        .map(|r| r.size_bytes)
        .sum();

    DockerSnapshot {
        ok: true,
        note: String::new(),
        disk_bytes,
        reclaimable_bytes,
        resources,
    }
}

pub fn dangling(img: &ImageSummary) -> bool {
    img.repo_tags.is_empty()
        || img
            .repo_tags
            .iter()
            .all(|t| t.is_empty() || t.contains("<none>"))
}

fn container_row(c: &ContainerSummary) -> DockerResource {
    let name = c
        .names
        .as_ref()
        .and_then(|n| n.first())
        .map(|n| n.trim_start_matches('/').to_string())
        .or_else(|| c.id.clone().map(|id| id.chars().take(12).collect()))
        .unwrap_or_else(|| "?".into());
    let running = c.state == Some(ContainerSummaryStateEnum::RUNNING);
    let compose = c
        .labels
        .as_ref()
        .and_then(|l| l.get(COMPOSE_PROJECT).cloned());
    DockerResource {
        kind: DockerKind::Container,
        id: c.id.clone().unwrap_or_default(),
        name,
        detail: if running { "running" } else { "stopped" }.into(),
        size_bytes: c.size_rw.unwrap_or(0).max(0) as u64,
        compose,
        persistent: false,
        anonymous: false,
        created: c.created.unwrap_or(0),
    }
}

fn dangling_row(img: &ImageSummary) -> DockerResource {
    DockerResource {
        kind: DockerKind::DanglingImage,
        id: img.id.clone(),
        name: short_id(&img.id),
        detail: "dangling".into(),
        size_bytes: img.size.max(0) as u64,
        compose: None,
        persistent: false,
        anonymous: false,
        created: img.created.max(0),
    }
}

fn volume_row(vol: &Volume, attached: bool) -> DockerResource {
    let size = vol
        .usage_data
        .as_ref()
        .map(|u| u.size.max(0) as u64)
        .unwrap_or(0);
    DockerResource {
        kind: DockerKind::Volume,
        id: vol.name.clone(),
        name: vol.name.clone(),
        detail: if attached { "attached" } else { "unused" }.into(),
        size_bytes: size,
        compose: vol.labels.get(COMPOSE_PROJECT).cloned(),
        persistent: true,
        anonymous: vol.labels.contains_key("com.docker.volume.anonymous"),
        created: vol.created_at.as_deref().map_or(0, parse_rfc3339),
    }
}

fn attached_volumes(containers: &[ContainerSummary]) -> HashSet<String> {
    let mut out = HashSet::new();
    for c in containers {
        let Some(mounts) = &c.mounts else { continue };
        for m in mounts {
            if let Some(name) = &m.name {
                out.insert(name.clone());
            }
        }
    }
    out
}

/// Docker's RFC3339 ("2024-01-02T15:04:05[.frac][Z|±hh:mm]") → unix secs; 0 on garbage.
fn parse_rfc3339(s: &str) -> i64 {
    let num = |r: Option<&str>| r.and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d)) = (num(s.get(0..4)), num(s.get(5..7)), num(s.get(8..10)))
    else {
        return 0;
    };
    let (Some(h), Some(mi), Some(sec)) =
        (num(s.get(11..13)), num(s.get(14..16)), num(s.get(17..19)))
    else {
        return 0;
    };
    // days-from-civil (Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let mut t = days * 86_400 + h * 3_600 + mi * 60 + sec;
    let zone = s
        .get(19..)
        .unwrap_or("")
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
    let (sign, rest) = match zone.as_bytes().first() {
        Some(b'+') => (1, zone.get(1..)),
        Some(b'-') => (-1, zone.get(1..)),
        _ => return t.max(0),
    };
    if let (Some(zh), Some(zm)) = (
        num(rest.and_then(|r| r.get(0..2))),
        num(rest.and_then(|r| r.get(3..5))),
    ) {
        t -= sign * (zh * 3_600 + zm * 60);
    }
    t.max(0)
}

fn short_id(id: &str) -> String {
    let bare = id.trim_start_matches("sha256:");
    bare.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(tags: Vec<String>) -> ImageSummary {
        ImageSummary {
            id: "sha256:abcdef0123456789".into(),
            repo_tags: tags,
            size: 1_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn dangling_when_untagged() {
        assert!(dangling(&image(vec![])));
        assert!(dangling(&image(vec!["<none>:<none>".into()])));
        assert!(!dangling(&image(vec!["nginx:latest".into()])));
    }

    #[test]
    fn running_containers_sort_first() {
        let stopped = ContainerSummary {
            id: Some("aaa".into()),
            names: Some(vec!["/old".into()]),
            state: Some(ContainerSummaryStateEnum::EXITED),
            ..Default::default()
        };
        let running = ContainerSummary {
            id: Some("bbb".into()),
            names: Some(vec!["/live".into()]),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            ..Default::default()
        };
        let snap = assemble(
            &[stopped, running],
            &[],
            &VolumeListResponse::default(),
            None,
        );
        assert_eq!(snap.resources[0].name, "live");
        assert_eq!(snap.resources[1].name, "old");
    }

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), 0);
        assert_eq!(parse_rfc3339("1970-01-01T01:00:00+01:00"), 0);
        assert_eq!(parse_rfc3339("2024-01-02T03:04:05Z"), 1_704_164_645);
        assert_eq!(
            parse_rfc3339("2024-01-02T03:04:05.123456789Z"),
            1_704_164_645
        );
        assert_eq!(parse_rfc3339("garbage"), 0);
        assert_eq!(parse_rfc3339(""), 0);
    }

    #[test]
    fn assemble_lists_stopped_and_dangling() {
        let c = ContainerSummary {
            id: Some("abc123def456".into()),
            names: Some(vec!["/old-api".into()]),
            state: Some(ContainerSummaryStateEnum::EXITED),
            size_rw: Some(310),
            ..Default::default()
        };
        let img = image(vec!["<none>:<none>".into()]);
        let snap = assemble(&[c], &[img], &VolumeListResponse::default(), None);
        assert!(snap.ok);
        assert_eq!(snap.resources.len(), 2);
        assert_eq!(snap.resources[0].name, "old-api");
        assert_eq!(snap.resources[0].detail, "stopped");
        assert_eq!(snap.resources[1].kind, DockerKind::DanglingImage);
    }

    #[test]
    fn scan_blocking_never_panics() {
        let snap = scan_blocking();
        if snap.ok {
            let _ = snap.resources.len();
        } else {
            assert!(!snap.note.is_empty());
        }
    }
}
