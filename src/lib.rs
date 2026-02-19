//! # OpenTelemetry Stackdriver Resource Detector
//!
//! This library provides a way to detect the monitored resource for the current environment.
//! This is a companion crate for the `opentelemetry-stackdriver` crate.
//!
//! Having this standalone makes it easier to test out different implementations before trying
//! to merge them into `opentelemetry-stackdriver`.
use std::{env, fs::File, io::Read};

use async_once_cell::OnceCell;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use metadata::MetadataClient;

use opentelemetry_stackdriver::MonitoredResource;
use thiserror::Error;

mod metadata;

static DETECTED_RESOURCE: OnceCell<MonitoredResource> = OnceCell::new();

/// Detects the monitored resource for the current environment.
///
/// Uses an async once cell to only do the work once.
/// If the resource is already detected, it will return the cached value.
///
/// # Errors
/// This will return an error if the resource could not be detected.
pub async fn detected_resource() -> Result<&'static MonitoredResource, DetectError> {
    DETECTED_RESOURCE.get_or_try_init(detect_resource()).await
}

#[derive(Debug, Error)]
pub enum DetectError {
    #[error("Failed to detect projectId")]
    NoProjectId,
    #[error("Failed to detect resource")]
    DetectionFailed,
}

async fn detect_resource() -> Result<MonitoredResource, DetectError> {
    let getter = ResourceAttributesGetter::default();

    if getter.is_metadata_active().await {
        // Fast path
        match system_product_name().as_deref() {
            Some("Google App Engine") => {
                return detect_app_engine_resource(&getter)
                    .await
                    .ok_or(DetectError::NoProjectId);
            }
            Some("Google Cloud Functions") => {
                return detect_cloud_function_resource(&getter)
                    .await
                    .ok_or(DetectError::NoProjectId);
            }
            _ => {}
        }

        if getter.is_app_engine() {
            return detect_app_engine_resource(&getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_cloud_function() {
            return detect_cloud_function_resource(&getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_cloud_run_service() {
            return detect_cloud_run_service_resource(&getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_cloud_run_job() {
            return detect_cloud_run_job_resource(&getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_kubernetes_engine().await {
            return detect_kubernetes_resource(&getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_compute_engine().await {
            return detect_compute_engine_resource(&getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
    }
    Err(DetectError::DetectionFailed)
}

/// Reads resource type on the Linux-based environments such as
// Cloud Functions, Cloud Run, GKE, GCE, GAE, etc.
fn system_product_name() -> Option<String> {
    #[cfg(not(target_os = "linux"))]
    return None;

    #[cfg(target_os = "linux")]
    {
        let path = "/sys/class/dmi/id/product_name";
        Some(
            File::open(path)
                .and_then(|mut file| {
                    let mut s = String::new();
                    file.read_to_string(&mut s)?;
                    Ok(s)
                })
                .unwrap_or_else(|_| String::new()),
        )
    }
}

struct ResourceAttributesGetter {
    metadata_client: MetadataClient,
}

impl ResourceAttributesGetter {
    async fn metadata(&self, path: &str) -> Option<String> {
        match self.metadata_client.resolve(path).await {
            Ok(body) => Some(body.trim().to_string()),
            Err(err) => {
                tracing::error!(?err, "Failed to get metadata from {}", path);
                None
            }
        }
    }

    async fn metadata_project_id(&self) -> Option<String> {
        self.metadata("project/project-id").await
    }

    async fn metadata_zone(&self) -> Option<String> {
        let zone = self.metadata("instance/zone").await.unwrap_or_default();
        if !zone.is_empty() {
            return zone.rsplit_once('/').map(|(_, zone)| zone.to_owned());
        }
        None
    }

    async fn metadata_region(&self) -> Option<String> {
        let region = self.metadata("instance/region").await.unwrap_or_default();
        if !region.is_empty() {
            return region.rsplit_once('/').map(|(_, region)| region.to_owned());
        }
        None
    }

    async fn is_metadata_active(&self) -> bool {
        self.metadata("").await.unwrap_or_default() != ""
    }

    fn is_app_engine(&self) -> bool {
        let service = env::var("GAE_SERVICE").unwrap_or_default();
        let version = env::var("GAE_VERSION").unwrap_or_default();
        let instance = env::var("GAE_INSTANCE").unwrap_or_default();
        !service.is_empty() && !version.is_empty() && !instance.is_empty()
    }

    fn is_cloud_function(&self) -> bool {
        env::var("FUNCTION_TARGET").is_ok_and(|v| !v.is_empty())
    }

    fn is_cloud_run_service(&self) -> bool {
        let has_config = env::var("K_CONFIGURATION").is_ok_and(|v| !v.is_empty());
        let has_function_target = env::var("FUNCTION_TARGET").is_ok_and(|v| !v.is_empty());
        has_config && !has_function_target
    }

    fn is_cloud_run_job(&self) -> bool {
        env::var("CLOUD_RUN_JOB").is_ok_and(|v| !v.is_empty())
    }

    async fn is_kubernetes_engine(&self) -> bool {
        let cluster_name = self
            .metadata("instance/attributes/cluster-name")
            .await
            .unwrap_or_default();
        if cluster_name.is_empty() {
            return false;
        }
        true
    }

    async fn is_compute_engine(&self) -> bool {
        let (preempted, platform, app_bucket) = tokio::join!(
            self.metadata("instance/preempted"),
            self.metadata("instance/cpu-platform"),
            self.metadata("instance/attributes/gae_app_bucket")
        );
        preempted.unwrap_or_default() != ""
            && platform.unwrap_or_default() != ""
            && app_bucket.unwrap_or_default() == ""
    }
}

impl Default for ResourceAttributesGetter {
    fn default() -> Self {
        Self {
            metadata_client: MetadataClient::new(
                Client::builder(TokioExecutor::new()).build_http(),
            ),
        }
    }
}

async fn detect_app_engine_resource(
    getter: &ResourceAttributesGetter,
) -> Option<MonitoredResource> {
    // We are not sure if the metadata service can return an empty string
    // for project ID. Thus, we do some unergonormic string base work here.
    let mut project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        project_id = env::var("GOOGLE_CLOUD_PROJECT").unwrap_or_default();
    }
    if project_id.is_empty() {
        return None;
    }
    let zone = getter.metadata_zone().await;
    let module_id = env::var("GAE_SERVICE")
        .ok()
        .or_else(|| env::var("GAE_MODULE_NAME").ok());
    let version_id = env::var("GAE_VERSION").ok();

    Some(MonitoredResource::AppEngine {
        project_id,
        module_id,
        version_id,
        zone,
    })
}

async fn detect_cloud_function_resource(
    getter: &ResourceAttributesGetter,
) -> Option<MonitoredResource> {
    let project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        return None;
    }
    let region = getter.metadata_region().await;
    let function_name = env::var("K_SERVICE").ok();
    Some(MonitoredResource::CloudFunction {
        project_id,
        region,
        function_name,
    })
}

async fn detect_cloud_run_service_resource(
    getter: &ResourceAttributesGetter,
) -> Option<MonitoredResource> {
    let project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        return None;
    }
    let location = getter.metadata_region().await;
    let service_name = env::var("K_SERVICE").ok();
    let revision_name = env::var("K_REVISION").ok();
    let configuration_name = env::var("K_CONFIGURATION").ok();
    Some(MonitoredResource::CloudRunRevision {
        project_id,
        location,
        service_name,
        revision_name,
        configuration_name,
    })
}

async fn detect_cloud_run_job_resource(
    getter: &ResourceAttributesGetter,
) -> Option<MonitoredResource> {
    let project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        return None;
    }
    let location = getter.metadata_region().await;
    let job_name = env::var("CLOUD_RUN_JOB").ok();
    Some(MonitoredResource::CloudRunJob {
        project_id,
        location,
        job_name,
    })
}

async fn detect_kubernetes_resource(
    getter: &ResourceAttributesGetter,
) -> Option<MonitoredResource> {
    let project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        return None;
    }

    let (cluster_name, location) = tokio::join!(
        getter.metadata("instance/attributes/cluster-name"),
        getter.metadata("instance/attributes/cluster-location")
    );
    let mut namespace_name = File::open("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .and_then(|mut file| {
            let mut s = String::new();
            file.read_to_string(&mut s)?;
            Ok(s)
        })
        .ok();
    if namespace_name.as_deref() == Some("") {
        // if automountServiceAccountToken is disabled allow to customize
        // the namespace via environment
        namespace_name = env::var("NAMESPACE_NAME").ok();
    }
    // note: if deployment customizes hostname, HOSTNAME envvar will have invalid content
    let pod_name = env::var("HOSTNAME").ok();
    // there is no way to derive container name from within container; use custom envvar if available
    let container_name = env::var("CONTAINER_NAME").ok();
    Some(MonitoredResource::KubernetesEngine {
        project_id,
        cluster_name,
        location,
        namespace_name,
        pod_name,
        container_name,
    })
}

async fn detect_compute_engine_resource(
    getter: &ResourceAttributesGetter,
) -> Option<MonitoredResource> {
    let project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        return None;
    }
    let (instance_id, zone) = tokio::join!(getter.metadata("instance/id"), getter.metadata_zone());
    Some(MonitoredResource::ComputeEngine {
        project_id,
        instance_id,
        zone,
    })
}
