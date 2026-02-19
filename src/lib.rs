//! # OpenTelemetry Stackdriver Resource Detector
//!
//! This library provides a way to detect the monitored resource for the current environment.
//! This is a companion crate for the `opentelemetry-stackdriver` crate.
//!
//! Having this standalone makes it easier to test out different implementations before trying
//! to merge them into `opentelemetry-stackdriver`.
use std::env::{self, VarError};
use std::fs::File;
use std::io::Read;

use async_once_cell::OnceCell;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use opentelemetry_stackdriver::MonitoredResource;
use thiserror::Error;

mod metadata;
use metadata::{HttpMetadataClient, MetadataClient};

static DETECTED_RESOURCE: OnceCell<MonitoredResource> = OnceCell::new();

/// Detects the monitored resource for the current environment.
///
/// Uses an async once cell to only do the work once.
/// If the resource is already detected, it will return the cached value.
///
/// # Errors
/// This will return an error if the resource could not be detected.
pub async fn detected_resource() -> Result<&'static MonitoredResource, DetectError> {
    DETECTED_RESOURCE
        .get_or_try_init(detect_resource(ResourceAttributesGetter::default()))
        .await
}

#[derive(Debug, Error)]
pub enum DetectError {
    #[error("Failed to detect projectId")]
    NoProjectId,
    #[error("Failed to detect resource")]
    DetectionFailed,
}

/// Detect the environment using the given getter
async fn detect_resource<C: MetadataClient>(
    getter: ResourceAttributesGetter<C>,
) -> Result<MonitoredResource, DetectError> {
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

struct ResourceAttributesGetter<C> {
    /// A generic metadata client.
    ///
    /// You normally would use HttpMetadataClient.
    metadata_client: C,
    /// This is used to allow testing of environment variable getters.
    env_getter: fn(&str) -> Result<String, VarError>,
}

impl<C: MetadataClient> ResourceAttributesGetter<C> {
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
        let service = (self.env_getter)("GAE_SERVICE").unwrap_or_default();
        let version = (self.env_getter)("GAE_VERSION").unwrap_or_default();
        let instance = (self.env_getter)("GAE_INSTANCE").unwrap_or_default();
        !service.is_empty() && !version.is_empty() && !instance.is_empty()
    }

    fn is_cloud_function(&self) -> bool {
        (self.env_getter)("FUNCTION_TARGET").is_ok_and(|v| !v.is_empty())
    }

    fn is_cloud_run_service(&self) -> bool {
        let has_config = (self.env_getter)("K_CONFIGURATION").is_ok_and(|v| !v.is_empty());
        let has_function_target = (self.env_getter)("FUNCTION_TARGET").is_ok_and(|v| !v.is_empty());
        has_config && !has_function_target
    }

    fn is_cloud_run_job(&self) -> bool {
        (self.env_getter)("CLOUD_RUN_JOB").is_ok_and(|v| !v.is_empty())
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

impl Default for ResourceAttributesGetter<HttpMetadataClient> {
    fn default() -> Self {
        Self {
            metadata_client: HttpMetadataClient::new(
                Client::builder(TokioExecutor::new()).build_http(),
            ),
            env_getter: |key| env::var(key),
        }
    }
}

async fn detect_app_engine_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
) -> Option<MonitoredResource> {
    // We are not sure if the metadata service can return an empty string
    // for project ID. Thus, we do some unergonormic string base work here.
    let mut project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        project_id = (getter.env_getter)("GOOGLE_CLOUD_PROJECT").unwrap_or_default();
    }
    if project_id.is_empty() {
        return None;
    }
    let zone = getter.metadata_zone().await;
    let module_id = (getter.env_getter)("GAE_SERVICE")
        .ok()
        .or_else(|| (getter.env_getter)("GAE_MODULE_NAME").ok());
    let version_id = (getter.env_getter)("GAE_VERSION").ok();

    Some(MonitoredResource::AppEngine {
        project_id,
        module_id,
        version_id,
        zone,
    })
}

async fn detect_cloud_function_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
) -> Option<MonitoredResource> {
    let project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        return None;
    }
    let region = getter.metadata_region().await;
    // This used to be FUNCTION_NAME, but that seems to be legacy.
    let function_name = (getter.env_getter)("K_SERVICE").ok();
    Some(MonitoredResource::CloudFunction {
        project_id,
        region,
        function_name,
    })
}

async fn detect_cloud_run_service_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
) -> Option<MonitoredResource> {
    let project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        return None;
    }
    let location = getter.metadata_region().await;
    let service_name = (getter.env_getter)("K_SERVICE").ok();
    let revision_name = (getter.env_getter)("K_REVISION").ok();
    let configuration_name = (getter.env_getter)("K_CONFIGURATION").ok();
    Some(MonitoredResource::CloudRunRevision {
        project_id,
        location,
        service_name,
        revision_name,
        configuration_name,
    })
}

async fn detect_cloud_run_job_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
) -> Option<MonitoredResource> {
    let project_id = getter.metadata_project_id().await.unwrap_or_default();
    if project_id.is_empty() {
        return None;
    }
    let location = getter.metadata_region().await;
    let job_name = (getter.env_getter)("CLOUD_RUN_JOB").ok();
    Some(MonitoredResource::CloudRunJob {
        project_id,
        location,
        job_name,
    })
}

async fn detect_kubernetes_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
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
        namespace_name = (getter.env_getter)("NAMESPACE_NAME").ok();
    }
    // note: if deployment customizes hostname, HOSTNAME envvar will have invalid content
    let pod_name = (getter.env_getter)("HOSTNAME").ok();
    // there is no way to derive container name from within container; use custom envvar if available
    let container_name = (getter.env_getter)("CONTAINER_NAME").ok();
    Some(MonitoredResource::KubernetesEngine {
        project_id,
        cluster_name,
        location,
        namespace_name,
        pod_name,
        container_name,
    })
}

async fn detect_compute_engine_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
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
#[cfg(test)]
mod tests {
    //! Tests taken from the go SDK implementation.
    use super::metadata::Error as MetadataError;
    use super::*;

    use std::collections::HashMap;
    use std::env::VarError;

    use opentelemetry_stackdriver::MonitoredResource;

    #[tokio::test]
    async fn cloud_platform_gke() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[(
                "instance/attributes/cluster-name",
                "my-cluster",
            )]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let resource = detect_resource(getter).await.unwrap();
        assert!(matches!(
            resource,
            MonitoredResource::KubernetesEngine { .. }
        ));
    }

    #[tokio::test]
    async fn cloud_platform_k8s_not_gke() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let resource = detect_resource(getter).await.unwrap();
        assert!(matches!(resource, MonitoredResource::ComputeEngine { .. }));
    }

    #[tokio::test]
    async fn cloud_platform_unknown() {
        let getter = ResourceAttributesGetter {
            metadata_client: FailingMetadataClient,
            env_getter: |_| Err(VarError::NotPresent),
        };
        let result = detect_resource(getter).await;
        assert!(matches!(result, Err(DetectError::DetectionFailed)));
    }

    #[tokio::test]
    async fn cloud_platform_gce() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let resource = detect_resource(getter).await.unwrap();
        assert!(matches!(resource, MonitoredResource::ComputeEngine { .. }));
    }

    #[tokio::test]
    async fn cloud_platform_cloud_run() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |key| match key {
                "K_CONFIGURATION" => Ok("my-config".into()),
                "K_SERVICE" => Ok("my-service".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        let resource = detect_resource(getter).await.unwrap();
        assert!(matches!(
            resource,
            MonitoredResource::CloudRunRevision { service_name, .. } if service_name.as_deref() == Some("my-service")
        ));
    }

    #[tokio::test]
    async fn cloud_platform_cloud_run_jobs() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |key| match key {
                "CLOUD_RUN_JOB" => Ok("my-job".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        let resource = detect_resource(getter).await.unwrap();
        assert!(
            matches!(resource, MonitoredResource::CloudRunJob { job_name, .. } if job_name.as_deref() == Some("my-job"))
        );
    }

    #[tokio::test]
    async fn cloud_platform_cloud_functions() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |key| match key {
                "FUNCTION_TARGET" => Ok("my-function".into()),
                "K_SERVICE" => Ok("my-function".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        let resource = detect_resource(getter).await.unwrap();
        assert!(
            matches!(resource, MonitoredResource::CloudFunction { function_name, .. } if function_name.as_deref() == Some("my-function"))
        );
    }

    #[tokio::test]
    async fn project_id() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |key| match key {
                "K_CONFIGURATION" => Ok("my-config".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        let resource = detect_resource(getter).await.unwrap();
        assert!(matches!(
            resource,
            MonitoredResource::CloudRunRevision { project_id, .. } if project_id == "my-project"
        ));
    }

    #[tokio::test]
    async fn project_id_err() {
        let getter = ResourceAttributesGetter {
            metadata_client: FailingMetadataClient,
            env_getter: |_| Err(VarError::NotPresent),
        };
        let result = detect_resource(getter).await;
        assert!(result.is_err());
    }

    struct FakeMetadataClient {
        metadata: HashMap<&'static str, &'static str>,
    }

    impl FakeMetadataClient {
        fn new(extra: &[(&'static str, &'static str)]) -> Self {
            let mut metadata = HashMap::from([
                ("", "ok"),
                ("project/project-id", "my-project"),
                ("instance/id", "1234567891"),
                ("instance/zone", "projects/1234567890/zones/us-central1-a"),
                ("instance/preempted", "false"),
                ("instance/cpu-platform", "Intel Broadwell"),
            ]);
            for &(k, v) in extra {
                metadata.insert(k, v);
            }
            Self { metadata }
        }
    }

    impl MetadataClient for FakeMetadataClient {
        async fn resolve_etag(
            &self,
            suffix: &str,
        ) -> Result<(String, Option<String>), MetadataError> {
            match self.metadata.get(suffix) {
                Some(value) => Ok(((*value).to_owned(), None)),
                None => Err(MetadataError::NotDefined(suffix.to_owned())),
            }
        }

        async fn resolve(&self, suffix: &str) -> Result<String, MetadataError> {
            let (body, _) = self.resolve_etag(suffix).await?;
            Ok(body)
        }
    }

    /// A metadata client that always returns an error (simulates no metadata server).
    struct FailingMetadataClient;

    impl MetadataClient for FailingMetadataClient {
        async fn resolve_etag(
            &self,
            suffix: &str,
        ) -> Result<(String, Option<String>), MetadataError> {
            Err(MetadataError::NotDefined(suffix.to_owned()))
        }

        async fn resolve(&self, suffix: &str) -> Result<String, MetadataError> {
            Err(MetadataError::NotDefined(suffix.to_owned()))
        }
    }
}
