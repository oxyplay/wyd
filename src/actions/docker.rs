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

/// Delete exactly the anonymous volumes the UI showed the user, by id.
/// This is safer than an engine-wide prune: what wyd listed is what gets
/// removed — no blanket engine call that could differ from the preview.
pub fn prune_anonymous_volumes_blocking(ids: &[String]) -> Result<(u32, u64), String> {
    runtime()?.block_on(async {
        let docker = bollard::Docker::connect_with_local_defaults().map_err(|e| e.to_string())?;
        let mut deleted = 0u32;
        let mut bytes = 0u64;
        for id in ids {
            // remove_volume on an attached volume fails harmlessly (busy).
            if docker
                .remove_volume(
                    id,
                    Some(bollard::query_parameters::RemoveVolumeOptionsBuilder::default().build()),
                )
                .await
                .is_ok()
            {
                deleted += 1;
                bytes += 0; // size not known per-id here; caller reports count
            }
        }
        Ok((deleted, bytes))
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
