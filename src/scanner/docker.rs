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
            });
        }
    }

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
