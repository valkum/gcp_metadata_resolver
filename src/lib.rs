//! # GCP Resource Detector
//!
//! Detects the GCP environment and provides resource metadata.
//! This metadata can be used to populate OpenTelemetry resource attributes
//! for traces, metrics, and logs.
//!
//! This crate queries the [GCE metadata server] to identify the running platform
//! (Compute Engine, GKE, Cloud Run, Cloud Functions, App Engine) and exposes
//! two complementary APIs:
//!
//! - [`detected_resource`] returns a [`MonitoredResource`] for use with the
//!   [Cloud Trace / Stackdriver exporter][opentelemetry-stackdriver].
//! - [`resource_attributes`] returns [`GcpResourceAttributes`], a typed struct
//!   of [OpenTelemetry semantic convention] resource attributes suitable for
//!   any OTLP exporter (e.g. [GCP Managed Prometheus via OTLP]).
//!
//! The detection logic mirrors the [Go GCP resource detector] and the
//! [OTel Collector GCP processor].
//!
//! ## Attribute matrix
//!
//! Which [`GcpResourceAttributes`] fields are populated depends on the detected
//! platform:
//!
//! | Field | GCE | GKE | Cloud Run | Cloud Run Job | Cloud Functions | App Engine |
//! |---|---|---|---|---|---|---|
//! | `cloud_account_id` | x | x | x | x | x | x |
//! | `cloud_platform` | x | x | x | x | x | x |
//! | `cloud_region` | x | x | x | x | x | x |
//! | `cloud_availability_zone` | x | x | | | | x |
//! | `host_id` | x | x | | | | |
//! | `host_name` | x | x | | | | |
//! | `host_type` | x | x | | | | |
//! | `gce_instance_name` | x | x | | | | |
//! | `gce_instance_hostname` | x | x | | | | |
//! | `gce_instance_group_manager_*` | x* | x* | | | | |
//! | `k8s_cluster_name` | | x | | | | |
//! | `faas_name` | | | x | x | x | x |
//! | `faas_version` | | | x | | x | x |
//! | `faas_instance` | | | x | x | x | x |
//!
//! *\* MIG fields are only set when the instance belongs to a managed instance group.*
//!
//! Unlike the reference Go detector (which is stateless and expects the SDK to
//! cache the resulting `Resource`), this crate caches the underlying metadata
//! client so repeated calls to [`project_id`], [`instance_id`], etc. reuse the
//! same HTTP connection pool. [`detected_resource`] additionally caches its
//! result via an async once-cell.
//!
//! [GCE metadata server]: https://docs.cloud.google.com/compute/docs/metadata/overview
//! [opentelemetry-stackdriver]: https://crates.io/crates/opentelemetry-stackdriver
//! [OpenTelemetry semantic convention]: https://opentelemetry.io/docs/specs/semconv/resource/cloud/
//! [GCP Managed Prometheus via OTLP]: https://docs.cloud.google.com/stackdriver/docs/otlp-metrics/overview
//! [Go GCP resource detector]: https://pkg.go.dev/go.opentelemetry.io/contrib/detectors/gcp
//! [OTel Collector GCP processor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/processor/resourcedetectionprocessor/internal/gcp
use std::env::{self, VarError};
use std::fs::File;
use std::io::Read;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use async_once_cell::OnceCell;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use opentelemetry_stackdriver::MonitoredResource;
use thiserror::Error;

mod metadata;
use metadata::{HttpMetadataClient, MetadataClient};

/// Detects the [`MonitoredResource`] for the current GCP environment.
///
/// Returns a Stackdriver-typed resource for use with the
/// [`opentelemetry-stackdriver`](https://crates.io/crates/opentelemetry-stackdriver)
/// Cloud Trace exporter. The result is cached; subsequent calls return the
/// same value without re-querying the metadata server.
///
/// For OTLP exporters (metrics, logs), prefer [`resource_attributes`] instead.
///
/// # Errors
///
/// Returns [`DetectError`] if the metadata server is unreachable or the
/// platform could not be identified.
pub async fn detected_resource() -> Result<&'static MonitoredResource, DetectError> {
    DETECTED_RESOURCE
        .get_or_try_init(detect_resource(
            DETECTOR.get_or_init(ResourceAttributesGetter::default),
        ))
        .await
}

/// Returns the GCP project ID from the [metadata server](https://cloud.google.com/compute/docs/metadata/predefined-metadata-keys),
/// or `None` if unavailable.
pub async fn project_id() -> Option<String> {
    DETECTOR
        .get_or_init(ResourceAttributesGetter::default)
        .metadata_project_id()
        .await
}

/// Returns the GCE instance ID from the [metadata server](https://cloud.google.com/compute/docs/metadata/predefined-metadata-keys),
/// or `None` if unavailable.
pub async fn instance_id() -> Option<String> {
    DETECTOR
        .get_or_init(ResourceAttributesGetter::default)
        .metadata_instance_id()
        .await
}

/// Returns [OpenTelemetry resource attributes] for the detected GCP environment.
///
/// Detects the platform (GCE, GKE, Cloud Run, Cloud Functions, App Engine)
/// and populates the corresponding fields in [`GcpResourceAttributes`].
/// On GKE, GCE host attributes are included alongside Kubernetes-specific
/// fields, matching the behavior of the [Go GCP detector].
///
/// Returns `None` when the [metadata server] is unreachable or the project
/// ID cannot be determined (e.g. local development).
///
/// When sending metrics to the [GCP Telemetry (OTLP) API] `v1.metrics` endpoint, the
/// `prometheus_target` monitored resource requires `location` (mapped from
/// `location`, [`cloud.availability_zone`][GcpResourceAttributes::cloud_availability_zone],
/// or [`cloud.region`][GcpResourceAttributes::cloud_region]) and `instance`
/// (mapped from `service.instance.id`, [`host.id`][GcpResourceAttributes::host_id], or
/// [`faas.instance`][GcpResourceAttributes::faas_instance]). Points missing
/// either attribute are rejected.
///
/// [OpenTelemetry resource attributes]: https://opentelemetry.io/docs/specs/semconv/resource/cloud/
/// [Go GCP detector]: https://pkg.go.dev/go.opentelemetry.io/contrib/detectors/gcp
/// [metadata server]: https://cloud.google.com/compute/docs/metadata/overview
/// [GCP Telemetry (OTLP) API]: https://cloud.google.com/stackdriver/docs/reference/telemetry/v1.metrics
pub async fn resource_attributes() -> Option<&'static GcpResourceAttributes> {
    DETECTED_ATTRIBUTES
        .get_or_init(detect_resource_attributes(
            DETECTOR.get_or_init(ResourceAttributesGetter::default),
        ))
        .await
        .as_ref()
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
    getter: &ResourceAttributesGetter<C>,
) -> Result<MonitoredResource, DetectError> {
    if getter.is_metadata_active().await {
        // Fast path
        match system_product_name().as_deref() {
            Some("Google App Engine") => {
                return detect_app_engine_resource(getter)
                    .await
                    .ok_or(DetectError::NoProjectId);
            }
            Some("Google Cloud Functions") => {
                return detect_cloud_function_resource(getter)
                    .await
                    .ok_or(DetectError::NoProjectId);
            }
            _ => {}
        }

        if getter.is_app_engine() {
            return detect_app_engine_resource(getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_cloud_function() {
            return detect_cloud_function_resource(getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_cloud_run_service() {
            return detect_cloud_run_service_resource(getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_cloud_run_job() {
            return detect_cloud_run_job_resource(getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_kubernetes_engine().await {
            return detect_kubernetes_resource(getter)
                .await
                .ok_or(DetectError::NoProjectId);
        }
        if getter.is_compute_engine().await {
            return detect_compute_engine_resource(getter)
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

    async fn metadata_instance_id(&self) -> Option<String> {
        self.metadata("instance/id").await
    }

    async fn metadata_zone(&self) -> Option<String> {
        let zone = self.metadata("instance/zone").await.unwrap_or_default();
        if !zone.is_empty() {
            return zone.rsplit_once('/').map(|(_, zone)| zone.to_owned());
        }
        None
    }

    async fn metadata_instance_name(&self) -> Option<String> {
        self.metadata("instance/name").await
    }

    async fn metadata_instance_hostname(&self) -> Option<String> {
        self.metadata("instance/hostname").await
    }

    async fn metadata_machine_type(&self) -> Option<String> {
        self.metadata("instance/machine-type").await
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
        // Set up a hyper client with the same timeouts as the go SDK.
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(2)));
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .build(connector);
        Self {
            metadata_client: HttpMetadataClient::new(client),
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

async fn detect_resource_attributes<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
) -> Option<GcpResourceAttributes> {
    if !getter.is_metadata_active().await {
        return None;
    }

    let cloud_account_id = getter.metadata_project_id().await?;

    let mut attrs = GcpResourceAttributes {
        cloud_account_id,
        cloud_platform: None,
        cloud_region: None,
        cloud_availability_zone: None,
        host_id: None,
        host_name: None,
        host_type: None,
        gce_instance_name: None,
        gce_instance_hostname: None,
        gce_instance_group_manager_name: None,
        gce_instance_group_manager_region: None,
        gce_instance_group_manager_zone: None,
        k8s_cluster_name: None,
        faas_name: None,
        faas_version: None,
        faas_instance: None,
    };

    // Fast path via system product name
    match system_product_name().as_deref() {
        Some("Google App Engine") => {
            detect_app_engine_attrs(getter, &mut attrs).await;
            return Some(attrs);
        }
        Some("Google Cloud Functions") => {
            detect_cloud_function_attrs(getter, &mut attrs).await;
            return Some(attrs);
        }
        _ => {}
    }

    if getter.is_app_engine() {
        detect_app_engine_attrs(getter, &mut attrs).await;
    } else if getter.is_cloud_function() {
        detect_cloud_function_attrs(getter, &mut attrs).await;
    } else if getter.is_cloud_run_service() {
        attrs.cloud_platform = Some(CLOUD_PLATFORM_CLOUD_RUN.to_owned());
        attrs.cloud_region = getter.metadata_region().await;
        attrs.faas_name = (getter.env_getter)("K_SERVICE").ok();
        attrs.faas_version = (getter.env_getter)("K_REVISION").ok();
        attrs.faas_instance = getter.metadata_instance_id().await;
    } else if getter.is_cloud_run_job() {
        attrs.cloud_platform = Some(CLOUD_PLATFORM_CLOUD_RUN.to_owned());
        attrs.cloud_region = getter.metadata_region().await;
        attrs.faas_name = (getter.env_getter)("CLOUD_RUN_JOB").ok();
        attrs.faas_instance = getter.metadata_instance_id().await;
    } else if getter.is_kubernetes_engine().await {
        detect_gce_attrs(getter, &mut attrs).await;
        attrs.cloud_platform = Some(CLOUD_PLATFORM_KUBERNETES_ENGINE.to_owned());
        attrs.k8s_cluster_name = getter.metadata("instance/attributes/cluster-name").await;
        if let Some(location) = getter
            .metadata("instance/attributes/cluster-location")
            .await
        {
            if location.contains('-') && location.matches('-').count() == 2 {
                attrs.cloud_region = zone_to_region(&location).map(str::to_owned);
                attrs.cloud_availability_zone = Some(location);
            } else {
                attrs.cloud_region = Some(location);
            }
        }
    } else if getter.is_compute_engine().await {
        detect_gce_attrs(getter, &mut attrs).await;
    }

    Some(attrs)
}

async fn detect_gce_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
) {
    attrs.cloud_platform = Some(CLOUD_PLATFORM_COMPUTE_ENGINE.to_owned());

    let (zone, host_id, instance_name, hostname, machine_type, created_by) = tokio::join!(
        getter.metadata_zone(),
        getter.metadata_instance_id(),
        getter.metadata_instance_name(),
        getter.metadata_instance_hostname(),
        getter.metadata_machine_type(),
        getter.metadata("instance/attributes/created-by"),
    );

    if let Some(zone) = zone {
        attrs.cloud_region = zone_to_region(&zone).map(str::to_owned);
        attrs.cloud_availability_zone = Some(zone);
    }

    attrs.host_id = host_id;
    attrs.host_name = instance_name.clone();
    attrs.host_type = machine_type;
    attrs.gce_instance_name = instance_name;
    attrs.gce_instance_hostname = hostname;

    if let Some(created_by) = created_by
        && let Some(caps) = MIG_RE.captures(&created_by)
    {
        attrs.gce_instance_group_manager_name = Some(caps[3].to_owned());
        match &caps[1] {
            "zones" => attrs.gce_instance_group_manager_zone = Some(caps[2].to_owned()),
            "regions" => attrs.gce_instance_group_manager_region = Some(caps[2].to_owned()),
            _ => {}
        }
    }
}

async fn detect_app_engine_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
) {
    attrs.cloud_platform = Some(CLOUD_PLATFORM_APP_ENGINE.to_owned());
    if let Some(zone) = getter.metadata_zone().await {
        attrs.cloud_region = zone_to_region(&zone).map(str::to_owned);
        attrs.cloud_availability_zone = Some(zone);
    }
    attrs.faas_name = (getter.env_getter)("GAE_SERVICE")
        .ok()
        .or_else(|| (getter.env_getter)("GAE_MODULE_NAME").ok());
    attrs.faas_version = (getter.env_getter)("GAE_VERSION").ok();
    attrs.faas_instance = (getter.env_getter)("GAE_INSTANCE").ok();
}

async fn detect_cloud_function_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
) {
    attrs.cloud_platform = Some(CLOUD_PLATFORM_CLOUD_FUNCTIONS.to_owned());
    attrs.cloud_region = getter.metadata_region().await;
    attrs.faas_name = (getter.env_getter)("K_SERVICE").ok();
    attrs.faas_version = (getter.env_getter)("FUNCTION_TARGET").ok();
    attrs.faas_instance = getter.metadata_instance_id().await;
}

/// Resource attributes for a detected GCP environment.
///
/// Fields map to [OpenTelemetry semantic conventions] for cloud, host, Kubernetes,
/// FaaS, and [GCP-specific] resource attributes. Which fields are populated depends
/// on the detected platform; see the [attribute matrix](crate#attribute-matrix).
///
/// Marked `#[non_exhaustive]` so new fields (e.g. GCE instance labels) can be
/// added in future minor versions without breaking callers.
///
/// [OpenTelemetry semantic conventions]: https://opentelemetry.io/docs/specs/semconv/resource/
/// [GCP-specific]: https://opentelemetry.io/docs/specs/semconv/resource/cloud-provider/gcp/
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GcpResourceAttributes {
    /// [`cloud.account.id`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/cloud/): GCP project ID.
    pub cloud_account_id: String,
    /// [`cloud.platform`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/cloud/):
    /// e.g. `gcp_compute_engine`, `gcp_kubernetes_engine`, `gcp_cloud_run`.
    pub cloud_platform: Option<String>,
    /// [`cloud.region`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/cloud/):
    /// e.g. `us-central1`. Derived from zone on GCE, from cluster-location on GKE.
    pub cloud_region: Option<String>,
    /// [`cloud.availability_zone`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/cloud/):
    /// e.g. `us-central1-a`. Set on GCE and zonal GKE clusters.
    pub cloud_availability_zone: Option<String>,
    /// [`host.id`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/host/):
    /// GCE instance ID. Set on GCE and GKE.
    pub host_id: Option<String>,
    /// [`host.name`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/host/):
    /// GCE instance name. Set on GCE and GKE.
    pub host_name: Option<String>,
    /// [`host.type`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/host/):
    /// machine type (e.g. `projects/123/machineTypes/e2-medium`). Set on GCE and GKE.
    pub host_type: Option<String>,
    /// [`gcp.gce.instance.name`](https://opentelemetry.io/docs/specs/semconv/resource/cloud-provider/gcp/):
    /// instance name visible in the Cloud Console.
    pub gce_instance_name: Option<String>,
    /// [`gcp.gce.instance.hostname`](https://opentelemetry.io/docs/specs/semconv/resource/cloud-provider/gcp/):
    /// full default or custom hostname.
    pub gce_instance_hostname: Option<String>,
    /// `gcp.gce.instance_group_manager.name`: MIG name, parsed from the
    /// `created-by` [instance attribute].
    ///
    /// [instance attribute]: https://docs.cloud.google.com/compute/docs/instance-groups/getting-info-about-migs#checking_if_a_vm_instance_is_part_of_a_mig
    pub gce_instance_group_manager_name: Option<String>,
    /// `gcp.gce.instance_group_manager.region`: set for regional MIGs.
    pub gce_instance_group_manager_region: Option<String>,
    /// `gcp.gce.instance_group_manager.zone`: set for zonal MIGs.
    pub gce_instance_group_manager_zone: Option<String>,
    /// [`k8s.cluster.name`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/k8s/):
    /// GKE cluster name from the `cluster-name` instance attribute. Only set on GKE.
    pub k8s_cluster_name: Option<String>,
    /// [`faas.name`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/faas/):
    /// function or service name. Set on Cloud Run, Cloud Functions, and App Engine.
    pub faas_name: Option<String>,
    /// [`faas.version`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/faas/):
    /// revision, version, or function target. Set on Cloud Run, Cloud Functions, and App Engine.
    pub faas_version: Option<String>,
    /// [`faas.instance`](https://opentelemetry.io/docs/specs/semconv/attributes-registry/faas/):
    /// execution environment instance ID. Set on Cloud Run, Cloud Functions, and App Engine.
    pub faas_instance: Option<String>,
}

pub const CLOUD_PLATFORM_COMPUTE_ENGINE: &str = "gcp_compute_engine";
pub const CLOUD_PLATFORM_KUBERNETES_ENGINE: &str = "gcp_kubernetes_engine";
pub const CLOUD_PLATFORM_CLOUD_RUN: &str = "gcp_cloud_run";
pub const CLOUD_PLATFORM_CLOUD_FUNCTIONS: &str = "gcp_cloud_functions";
pub const CLOUD_PLATFORM_APP_ENGINE: &str = "gcp_app_engine";

fn zone_to_region(zone: &str) -> Option<&str> {
    zone.rsplit_once('-').map(|(region, _)| region)
}

static DETECTOR: OnceLock<ResourceAttributesGetter<HttpMetadataClient>> = OnceLock::new();
static DETECTED_ATTRIBUTES: OnceCell<Option<GcpResourceAttributes>> = OnceCell::new();
static DETECTED_RESOURCE: OnceCell<MonitoredResource> = OnceCell::new();
static MIG_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^projects/[^/]+/(zones|regions)/([^/]+)/instanceGroupManagers/([^/]+)$")
        .unwrap()
});

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
        let resource = detect_resource(&getter).await.unwrap();
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
        let resource = detect_resource(&getter).await.unwrap();
        assert!(matches!(resource, MonitoredResource::ComputeEngine { .. }));
    }

    #[tokio::test]
    async fn cloud_platform_unknown() {
        let getter = ResourceAttributesGetter {
            metadata_client: FailingMetadataClient,
            env_getter: |_| Err(VarError::NotPresent),
        };
        let result = detect_resource(&getter).await;
        assert!(matches!(result, Err(DetectError::DetectionFailed)));
    }

    #[tokio::test]
    async fn cloud_platform_gce() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let resource = detect_resource(&getter).await.unwrap();
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
        let resource = detect_resource(&getter).await.unwrap();
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
        let resource = detect_resource(&getter).await.unwrap();
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
        let resource = detect_resource(&getter).await.unwrap();
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
        let resource = detect_resource(&getter).await.unwrap();
        assert!(matches!(
            resource,
            MonitoredResource::CloudRunRevision { project_id, .. } if project_id == "my-project"
        ));
    }

    #[tokio::test]
    async fn instance_id() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |key| match key {
                "K_CONFIGURATION" => Ok("my-config".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        let instance_id = getter.metadata_instance_id().await.unwrap();
        assert_eq!(&instance_id, "1234567891");
    }

    #[tokio::test]
    async fn project_id_err() {
        let getter = ResourceAttributesGetter {
            metadata_client: FailingMetadataClient,
            env_getter: |_| Err(VarError::NotPresent),
        };
        let result = detect_resource(&getter).await;
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
                ("instance/name", "my-instance"),
                (
                    "instance/hostname",
                    "my-instance.us-central1-a.c.my-project.internal",
                ),
                (
                    "instance/machine-type",
                    "projects/1234567890/machineTypes/e2-medium",
                ),
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

    #[tokio::test]
    async fn resource_attributes_gce() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(attrs.cloud_account_id, "my-project");
        assert_eq!(attrs.cloud_platform.as_deref(), Some("gcp_compute_engine"));
        assert_eq!(
            attrs.cloud_availability_zone.as_deref(),
            Some("us-central1-a")
        );
        assert_eq!(attrs.cloud_region.as_deref(), Some("us-central1"));
        assert_eq!(attrs.host_id.as_deref(), Some("1234567891"));
        assert_eq!(attrs.host_name.as_deref(), Some("my-instance"));
        assert_eq!(
            attrs.host_type.as_deref(),
            Some("projects/1234567890/machineTypes/e2-medium")
        );
        assert_eq!(attrs.gce_instance_name.as_deref(), Some("my-instance"));
        assert_eq!(
            attrs.gce_instance_hostname.as_deref(),
            Some("my-instance.us-central1-a.c.my-project.internal")
        );
        assert_eq!(attrs.k8s_cluster_name, None);
        assert_eq!(attrs.faas_name, None);
    }

    #[tokio::test]
    async fn resource_attributes_gce_with_mig_zonal() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[(
                "instance/attributes/created-by",
                "projects/my-project/zones/us-central1-a/instanceGroupManagers/my-mig",
            )]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(
            attrs.gce_instance_group_manager_name.as_deref(),
            Some("my-mig")
        );
        assert_eq!(
            attrs.gce_instance_group_manager_zone.as_deref(),
            Some("us-central1-a")
        );
        assert_eq!(attrs.gce_instance_group_manager_region, None);
    }

    #[tokio::test]
    async fn resource_attributes_gce_with_mig_regional() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[(
                "instance/attributes/created-by",
                "projects/my-project/regions/us-central1/instanceGroupManagers/my-rmig",
            )]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(
            attrs.gce_instance_group_manager_name.as_deref(),
            Some("my-rmig")
        );
        assert_eq!(
            attrs.gce_instance_group_manager_region.as_deref(),
            Some("us-central1")
        );
        assert_eq!(attrs.gce_instance_group_manager_zone, None);
    }

    #[tokio::test]
    async fn resource_attributes_gke() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[
                ("instance/attributes/cluster-name", "my-cluster"),
                ("instance/attributes/cluster-location", "us-central1"),
            ]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(
            attrs.cloud_platform.as_deref(),
            Some("gcp_kubernetes_engine")
        );
        assert_eq!(attrs.cloud_region.as_deref(), Some("us-central1"));
        // cluster-location is regional, but the instance still has a zone
        assert_eq!(
            attrs.cloud_availability_zone.as_deref(),
            Some("us-central1-a")
        );
        assert_eq!(attrs.k8s_cluster_name.as_deref(), Some("my-cluster"));
        assert_eq!(attrs.host_id.as_deref(), Some("1234567891"));
        assert_eq!(attrs.host_name.as_deref(), Some("my-instance"));
    }

    #[tokio::test]
    async fn resource_attributes_gke_zonal() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[
                ("instance/attributes/cluster-name", "my-cluster"),
                ("instance/attributes/cluster-location", "us-central1-a"),
            ]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(
            attrs.cloud_platform.as_deref(),
            Some("gcp_kubernetes_engine")
        );
        assert_eq!(attrs.cloud_region.as_deref(), Some("us-central1"));
        assert_eq!(
            attrs.cloud_availability_zone.as_deref(),
            Some("us-central1-a")
        );
    }

    #[tokio::test]
    async fn resource_attributes_cloud_run() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[(
                "instance/region",
                "projects/123/regions/us-east1",
            )]),
            env_getter: |key| match key {
                "K_CONFIGURATION" => Ok("my-config".into()),
                "K_SERVICE" => Ok("my-service".into()),
                "K_REVISION" => Ok("my-service-00001".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(attrs.cloud_platform.as_deref(), Some("gcp_cloud_run"));
        assert_eq!(attrs.cloud_region.as_deref(), Some("us-east1"));
        assert_eq!(attrs.faas_name.as_deref(), Some("my-service"));
        assert_eq!(attrs.faas_version.as_deref(), Some("my-service-00001"));
        assert_eq!(attrs.faas_instance.as_deref(), Some("1234567891"));
        assert_eq!(attrs.host_id, None);
    }

    #[tokio::test]
    async fn resource_attributes_cloud_run_job() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[(
                "instance/region",
                "projects/123/regions/us-west1",
            )]),
            env_getter: |key| match key {
                "CLOUD_RUN_JOB" => Ok("my-job".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(attrs.cloud_platform.as_deref(), Some("gcp_cloud_run"));
        assert_eq!(attrs.faas_name.as_deref(), Some("my-job"));
        assert_eq!(attrs.faas_version, None);
        assert_eq!(attrs.faas_instance.as_deref(), Some("1234567891"));
    }

    #[tokio::test]
    async fn resource_attributes_no_metadata() {
        let getter = ResourceAttributesGetter {
            metadata_client: FailingMetadataClient,
            env_getter: |_| Err(VarError::NotPresent),
        };
        assert!(detect_resource_attributes(&getter).await.is_none());
    }
}
