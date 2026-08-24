use crate::model::{DockerKind, DockerResource};

/// Remove one selected Docker resource. Volumes are never implied — caller
/// must have required the `D` confirm.
pub fn remove_blocking(res: &DockerResource) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(remove(res))
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
