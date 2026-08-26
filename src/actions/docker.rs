use crate::model::{DockerKind, DockerResource};

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

/// Remove one selected Docker resource. Volumes are never implied — caller
/// must have required the `D` confirm.
pub fn remove_blocking(res: &DockerResource) -> Result<(), String> {
    runtime()?.block_on(remove(res))
}

/// Graceful stop (SIGTERM, engine default timeout). Only valid on running containers.
pub fn stop_blocking(res: &DockerResource) -> Result<(), String> {
    runtime()?.block_on(async {
        let docker = bollard::Docker::connect_with_local_defaults().map_err(|e| e.to_string())?;
        docker
            .stop_container(&res.id, None)
            .await
            .map_err(|e| e.to_string())
    })
}

/// Returns (deleted count, bytes reclaimed).
pub fn prune_anonymous_volumes_blocking() -> Result<(u32, u64), String> {
    runtime()?.block_on(async {
        let docker = bollard::Docker::connect_with_local_defaults().map_err(|e| e.to_string())?;
        let mut filters = std::collections::HashMap::new();
        filters.insert("all", vec!["false"]);
        let opts = bollard::query_parameters::PruneVolumesOptionsBuilder::default()
            .filters(&filters)
            .build();
        let res = docker
            .prune_volumes(Some(opts))
            .await
            .map_err(|e| e.to_string())?;
        Ok((
            res.volumes_deleted.as_ref().map(|v| v.len()).unwrap_or(0) as u32,
            res.space_reclaimed.unwrap_or(0).max(0) as u64,
        ))
    })
}

async fn remove(res: &DockerResource) -> Result<(), String> {
    let docker = bollard::Docker::connect_with_local_defaults().map_err(|e| e.to_string())?;
    match res.kind {
        DockerKind::Container => docker
            .remove_container(
                &res.id,
                Some(bollard::query_parameters::RemoveContainerOptionsBuilder::default().build()),
            )
            .await
            .map_err(|e| e.to_string()),
        DockerKind::DanglingImage => docker
            .remove_image(
                &res.id,
                Some(bollard::query_parameters::RemoveImageOptionsBuilder::default().build()),
                None,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        DockerKind::Volume => docker
            .remove_volume(
                &res.id,
                Some(bollard::query_parameters::RemoveVolumeOptionsBuilder::default().build()),
            )
            .await
            .map_err(|e| e.to_string()),
        DockerKind::BuildCache => docker
            .prune_build(Some(
                bollard::query_parameters::PruneBuildOptionsBuilder::default().build(),
            ))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
    }
}
