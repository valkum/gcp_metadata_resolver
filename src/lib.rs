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
//! - [`detected_resource`] returns a [`MonitoredResource`], the GCP monitored
//!   resource type and labels that the Cloud Logging and Cloud Monitoring APIs
//!   accept.
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
//! | `cloud_availability_zone` | x | x† | | | | x |
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
//! *† On GKE, the location comes only from the `cluster-location` instance
//! attribute, never from the node zone. `cloud_availability_zone` is only set
//! for zonal clusters. Regional clusters (e.g. Autopilot) set only `cloud_region`.*
//!
//! Unlike the reference Go detector (which is stateless and expects the SDK to
//! cache the resulting `Resource`), this crate caches the underlying metadata
//! client so repeated calls to [`project_id`], [`instance_id`], etc. reuse the
//! same HTTP connection pool. [`detected_resource`] and
//! [`resource_attributes`] also cache their first verdict for the life of the
//! process, also when detection fails. Like the Python detector, the crate is
//! meant to describe the environment at startup.
//!
//! [GCE metadata server]: https://docs.cloud.google.com/compute/docs/metadata/overview
//! [opentelemetry-stackdriver]: https://crates.io/crates/opentelemetry-stackdriver
//! [OpenTelemetry semantic convention]: https://opentelemetry.io/docs/specs/semconv/resource/cloud/
//! [GCP Managed Prometheus via OTLP]: https://docs.cloud.google.com/stackdriver/docs/otlp-metrics/overview
//! [Go GCP resource detector]: https://pkg.go.dev/go.opentelemetry.io/contrib/detectors/gcp
//! [OTel Collector GCP processor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/processor/resourcedetectionprocessor/internal/gcp
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::env::{self, VarError};
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use async_once_cell::OnceCell;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use thiserror::Error;

mod metadata;
use metadata::{HttpMetadataClient, MetadataClient};

mod monitored_resource;
pub use monitored_resource::MonitoredResource;

#[cfg(feature = "opentelemetry")]
mod otel;

/// Detects the [`MonitoredResource`] for the current GCP environment.
///
/// Use [`MonitoredResource::resource_type`] and [`MonitoredResource::labels`]
/// to get the flat [`google.api.MonitoredResource`] form, or match on the
/// variant to build whatever your client needs.
///
/// If the metadata server answers but no known platform matches, the result
/// is [`MonitoredResource::Global`].
///
/// The first verdict that completes is cached for the life of the process.
/// This includes a failure: later calls return the same error and do not
/// query the metadata server again. If a call is dropped before it completes,
/// for example by a timeout, the next call starts detection again. The crate
/// is meant for startup, where the environment does not change.
///
/// With the `stackdriver` feature, [`From`] converts the result into
/// [`opentelemetry_stackdriver::MonitoredResource`]. That crate is
/// [deprecated], so the feature is off by default.
///
/// For OTLP exporters (metrics, logs), prefer [`resource_attributes`] instead.
///
/// [deprecated]: https://github.com/open-telemetry/opentelemetry-rust-contrib/issues/609
/// [`google.api.MonitoredResource`]: https://cloud.google.com/logging/docs/reference/v2/rest/v2/MonitoredResource
/// [`opentelemetry_stackdriver::MonitoredResource`]: https://docs.rs/opentelemetry-stackdriver/latest/opentelemetry_stackdriver/enum.MonitoredResource.html
///
/// # Errors
///
/// Returns [`DetectError`] if the metadata server is unreachable or the
/// project ID is not available.
pub async fn detected_resource() -> Result<&'static MonitoredResource, DetectError> {
    DETECTED_RESOURCE
        .get_or_init(async { build_resource(detector(), detection().await).await })
        .await
        .as_ref()
        .map_err(Clone::clone)
}

/// Returns the GCP project ID from the [metadata server](https://cloud.google.com/compute/docs/metadata/predefined-metadata-keys),
/// or `None` if unavailable.
pub async fn project_id() -> Option<String> {
    detector().metadata_project_id().await
}

/// Returns the GCE instance ID from the [metadata server](https://cloud.google.com/compute/docs/metadata/predefined-metadata-keys),
/// or `None` if unavailable.
pub async fn instance_id() -> Option<String> {
    detector().metadata_instance_id().await
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
/// Like [`detected_resource`], this caches the first verdict for the life of
/// the process, also when it is `None`. Both functions share one platform
/// detection.
///
/// When sending metrics to the [Google Telemetry API] `v1.metrics` endpoint, the
/// `prometheus_target` monitored resource requires `location` (mapped from
/// `location`, [`cloud.availability_zone`][GcpResourceAttributes::cloud_availability_zone],
/// or [`cloud.region`][GcpResourceAttributes::cloud_region]) and `instance`
/// (mapped from `service.instance.id`, [`host.id`][GcpResourceAttributes::host_id], or
/// [`faas.instance`][GcpResourceAttributes::faas_instance]). Points missing
/// either attribute are rejected. On GKE, `location` is the cluster location:
/// the zone of a zonal cluster or the region of a regional cluster.
///
/// [OpenTelemetry resource attributes]: https://opentelemetry.io/docs/specs/semconv/resource/cloud/
/// [Go GCP detector]: https://pkg.go.dev/go.opentelemetry.io/contrib/detectors/gcp
/// [metadata server]: https://cloud.google.com/compute/docs/metadata/overview
/// [Google Telemetry API]: https://cloud.google.com/stackdriver/docs/reference/telemetry/v1.metrics
pub async fn resource_attributes() -> Option<&'static GcpResourceAttributes> {
    DETECTED_ATTRIBUTES
        .get_or_init(async { build_resource_attributes(detector(), detection().await).await })
        .await
        .as_ref()
}

#[derive(Debug, Clone, Error)]
pub enum DetectError {
    #[error("Failed to detect projectId")]
    NoProjectId,
    #[error("Failed to detect resource")]
    DetectionFailed,
}

/// The detected GCP platform.
#[derive(Debug, PartialEq, Eq)]
enum Platform {
    AppEngine,
    CloudFunction,
    CloudRunService,
    CloudRunJob,
    KubernetesEngine {
        cluster_name: String,
    },
    ComputeEngine,
    /// The metadata server answers, but no known platform matches.
    Unknown,
}

/// The result of platform detection.
#[derive(Debug)]
struct Detection {
    platform: Platform,
    project_id: Option<String>,
}

/// Detects the platform and the project ID. Returns `None` if the metadata
/// server does not answer.
async fn detect<C: MetadataClient>(getter: &ResourceAttributesGetter<C>) -> Option<Detection> {
    // Probe first, so that a host that is not GCP sends one request only.
    if !getter.is_metadata_active().await {
        return None;
    }
    let (platform, project_id) =
        tokio::join!(detect_platform(getter), getter.metadata_project_id());
    Some(Detection {
        platform,
        project_id,
    })
}

/// Detects the platform of a host where the metadata server answers.
async fn detect_platform<C: MetadataClient>(getter: &ResourceAttributesGetter<C>) -> Platform {
    // Fast path
    match system_product_name().as_deref() {
        Some("Google App Engine") => return Platform::AppEngine,
        Some("Google Cloud Functions") => return Platform::CloudFunction,
        _ => {}
    }
    if getter.is_app_engine() {
        Platform::AppEngine
    } else if getter.is_cloud_function() {
        Platform::CloudFunction
    } else if getter.is_cloud_run_service() {
        Platform::CloudRunService
    } else if getter.is_cloud_run_job() {
        Platform::CloudRunJob
    } else {
        // Probe GKE and GCE together to save one round trip.
        match tokio::join!(getter.kubernetes_cluster_name(), getter.is_compute_engine()) {
            (Some(cluster_name), _) => Platform::KubernetesEngine { cluster_name },
            (None, true) => Platform::ComputeEngine,
            (None, false) => Platform::Unknown,
        }
    }
}

/// Returns the detection for this process. The first verdict is cached, also
/// when it is `None`, and [`detected_resource`] and [`resource_attributes`]
/// share it.
async fn detection() -> Option<&'static Detection> {
    DETECTION.get_or_init(detect(detector())).await.as_ref()
}

/// Detect the environment using the given getter
#[cfg(test)]
async fn detect_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
) -> Result<MonitoredResource, DetectError> {
    build_resource(getter, detect(getter).await.as_ref()).await
}

async fn build_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    detection: Option<&Detection>,
) -> Result<MonitoredResource, DetectError> {
    let Some(detection) = detection else {
        return Err(DetectError::DetectionFailed);
    };
    let project_id = || detection.project_id.clone().ok_or(DetectError::NoProjectId);
    Ok(match &detection.platform {
        Platform::AppEngine => {
            // App Engine also takes the project ID from the environment.
            let project_id =
                project_id().or_else(|err| getter.env("GOOGLE_CLOUD_PROJECT").ok_or(err))?;
            detect_app_engine_resource(getter, project_id).await
        }
        Platform::CloudFunction => detect_cloud_function_resource(getter, project_id()?).await,
        Platform::CloudRunService => detect_cloud_run_service_resource(getter, project_id()?).await,
        Platform::CloudRunJob => detect_cloud_run_job_resource(getter, project_id()?).await,
        Platform::KubernetesEngine { cluster_name } => {
            detect_kubernetes_resource(getter, project_id()?, cluster_name).await
        }
        Platform::ComputeEngine => detect_compute_engine_resource(getter, project_id()?).await,
        Platform::Unknown => MonitoredResource::Global {
            project_id: project_id()?,
        },
    })
}

/// Reads resource type on the Linux-based environments such as
// Cloud Functions, Cloud Run, GKE, GCE, GAE, etc.
fn system_product_name() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // sysfs values end with a newline, so trim before the caller matches on it.
    read_trimmed("/sys/class/dmi/id/product_name")
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
    /// Returns a metadata value, or `None` if it is absent or empty.
    async fn metadata(&self, path: &str) -> Option<String> {
        match self.metadata_client.resolve(path).await {
            Ok(body) => non_empty(body.trim()),
            // Detection probes keys that are absent on most platforms.
            Err(err @ metadata::Error::NotDefined(_)) => {
                tracing::debug!(?err, "Metadata key {} is not defined", path);
                None
            }
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

    /// Returns an environment variable, or `None` if it is unset or empty.
    fn env(&self, key: &str) -> Option<String> {
        (self.env_getter)(key).ok().filter(|v| !v.is_empty())
    }

    async fn metadata_zone(&self) -> Option<String> {
        last_segment(&self.metadata("instance/zone").await?)
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
        last_segment(&self.metadata("instance/region").await?)
    }

    async fn is_metadata_active(&self) -> bool {
        self.metadata("").await.is_some()
    }

    fn is_app_engine(&self) -> bool {
        ["GAE_SERVICE", "GAE_VERSION", "GAE_INSTANCE"]
            .iter()
            .all(|key| self.env(key).is_some())
    }

    fn is_cloud_function(&self) -> bool {
        self.env("FUNCTION_TARGET").is_some()
    }

    fn is_cloud_run_service(&self) -> bool {
        // Cloud Functions also sets K_CONFIGURATION.
        self.env("K_CONFIGURATION").is_some() && !self.is_cloud_function()
    }

    fn is_cloud_run_job(&self) -> bool {
        self.env("CLOUD_RUN_JOB").is_some()
    }

    /// Returns the GKE cluster name, which is only set on GKE nodes.
    async fn kubernetes_cluster_name(&self) -> Option<String> {
        self.metadata("instance/attributes/cluster-name").await
    }

    async fn is_compute_engine(&self) -> bool {
        let (preempted, platform, app_bucket) = tokio::join!(
            self.metadata("instance/preempted"),
            self.metadata("instance/cpu-platform"),
            self.metadata("instance/attributes/gae_app_bucket")
        );
        preempted.is_some() && platform.is_some() && app_bucket.is_none()
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
    project_id: String,
) -> MonitoredResource {
    let zone = getter.metadata_zone().await;
    let module_id = getter
        .env("GAE_SERVICE")
        .or_else(|| getter.env("GAE_MODULE_NAME"));
    let version_id = getter.env("GAE_VERSION");

    MonitoredResource::AppEngine {
        project_id,
        module_id,
        version_id,
        zone,
    }
}

async fn detect_cloud_function_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    project_id: String,
) -> MonitoredResource {
    let region = getter.metadata_region().await;
    // This used to be FUNCTION_NAME, but that seems to be legacy.
    let function_name = getter.env("K_SERVICE");
    MonitoredResource::CloudFunction {
        project_id,
        region,
        function_name,
    }
}

async fn detect_cloud_run_service_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    project_id: String,
) -> MonitoredResource {
    let location = getter.metadata_region().await;
    let service_name = getter.env("K_SERVICE");
    let revision_name = getter.env("K_REVISION");
    let configuration_name = getter.env("K_CONFIGURATION");
    MonitoredResource::CloudRunRevision {
        project_id,
        location,
        service_name,
        revision_name,
        configuration_name,
    }
}

async fn detect_cloud_run_job_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    project_id: String,
) -> MonitoredResource {
    let location = getter.metadata_region().await;
    let job_name = getter.env("CLOUD_RUN_JOB");
    MonitoredResource::CloudRunJob {
        project_id,
        location,
        job_name,
    }
}

async fn detect_kubernetes_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    project_id: String,
    cluster_name: &str,
) -> MonitoredResource {
    let location = getter
        .metadata("instance/attributes/cluster-location")
        .await;
    // if automountServiceAccountToken is disabled allow to customize
    // the namespace via environment
    let namespace_name = read_trimmed("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .filter(|namespace| !namespace.is_empty())
        .or_else(|| getter.env("NAMESPACE_NAME"));
    // note: if deployment customizes hostname, HOSTNAME envvar will have invalid content
    let pod_name = getter.env("HOSTNAME");
    // there is no way to derive container name from within container; use custom envvar if available
    let container_name = getter.env("CONTAINER_NAME");
    MonitoredResource::KubernetesEngine {
        project_id,
        cluster_name: Some(cluster_name.to_owned()),
        location,
        namespace_name,
        pod_name,
        container_name,
    }
}

async fn detect_compute_engine_resource<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    project_id: String,
) -> MonitoredResource {
    let (instance_id, zone) = tokio::join!(getter.metadata("instance/id"), getter.metadata_zone());
    MonitoredResource::ComputeEngine {
        project_id,
        instance_id,
        zone,
    }
}

#[cfg(test)]
async fn detect_resource_attributes<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
) -> Option<GcpResourceAttributes> {
    build_resource_attributes(getter, detect(getter).await.as_ref()).await
}

async fn build_resource_attributes<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    detection: Option<&Detection>,
) -> Option<GcpResourceAttributes> {
    let detection = detection?;
    let mut attrs = GcpResourceAttributes::new(detection.project_id.clone()?);

    match &detection.platform {
        Platform::AppEngine => detect_app_engine_attrs(getter, &mut attrs).await,
        Platform::CloudFunction => detect_cloud_function_attrs(getter, &mut attrs).await,
        Platform::CloudRunService => detect_cloud_run_service_attrs(getter, &mut attrs).await,
        Platform::CloudRunJob => detect_cloud_run_job_attrs(getter, &mut attrs).await,
        Platform::KubernetesEngine { cluster_name } => {
            detect_gke_attrs(getter, &mut attrs, cluster_name).await
        }
        Platform::ComputeEngine => detect_gce_attrs(getter, &mut attrs).await,
        Platform::Unknown => {}
    }

    Some(attrs)
}

async fn detect_cloud_run_service_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
) {
    attrs.cloud_platform = Some(CLOUD_PLATFORM_CLOUD_RUN.to_owned());
    (attrs.cloud_region, attrs.faas_instance) =
        tokio::join!(getter.metadata_region(), getter.metadata_instance_id());
    attrs.faas_name = getter.env("K_SERVICE");
    attrs.faas_version = getter.env("K_REVISION");
}

async fn detect_cloud_run_job_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
) {
    attrs.cloud_platform = Some(CLOUD_PLATFORM_CLOUD_RUN.to_owned());
    (attrs.cloud_region, attrs.faas_instance) =
        tokio::join!(getter.metadata_region(), getter.metadata_instance_id());
    attrs.faas_name = getter.env("CLOUD_RUN_JOB");
}

async fn detect_gce_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
) {
    attrs.cloud_platform = Some(CLOUD_PLATFORM_COMPUTE_ENGINE.to_owned());

    let (zone, ()) = tokio::join!(getter.metadata_zone(), detect_gce_host_attrs(getter, attrs));

    if let Some(zone) = zone {
        attrs.cloud_region = zone_to_region(&zone).map(str::to_owned);
        attrs.cloud_availability_zone = Some(zone);
    }
}

/// Matches `GKEAvailabilityZoneOrRegion` of the Go detector: the location
/// comes only from `cluster-location`, never from the node's `instance/zone`.
/// Cloud Logging fills the `location` label from `cloud.availability_zone`
/// first, so a node zone on a regional cluster hides the cluster region.
async fn detect_gke_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
    cluster_name: &str,
) {
    attrs.cloud_platform = Some(CLOUD_PLATFORM_KUBERNETES_ENGINE.to_owned());
    attrs.k8s_cluster_name = Some(cluster_name.to_owned());

    let (location, ()) = tokio::join!(
        getter.metadata("instance/attributes/cluster-location"),
        detect_gce_host_attrs(getter, attrs),
    );

    if let Some(location) = location {
        match location.matches('-').count() {
            1 => attrs.cloud_region = Some(location),
            2 => {
                attrs.cloud_region = zone_to_region(&location).map(str::to_owned);
                attrs.cloud_availability_zone = Some(location);
            }
            _ => tracing::debug!("unrecognized format for cluster location: {location}"),
        }
    }
}

/// Sets the host and `gce.*` attributes of the underlying VM, but not its zone.
async fn detect_gce_host_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
) {
    let (host_id, instance_name, hostname, machine_type, created_by) = tokio::join!(
        getter.metadata_instance_id(),
        getter.metadata_instance_name(),
        getter.metadata_instance_hostname(),
        getter.metadata_machine_type(),
        getter.metadata("instance/attributes/created-by"),
    );

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
    attrs.faas_name = getter
        .env("GAE_SERVICE")
        .or_else(|| getter.env("GAE_MODULE_NAME"));
    attrs.faas_version = getter.env("GAE_VERSION");
    attrs.faas_instance = getter.env("GAE_INSTANCE");
}

async fn detect_cloud_function_attrs<C: MetadataClient>(
    getter: &ResourceAttributesGetter<C>,
    attrs: &mut GcpResourceAttributes,
) {
    attrs.cloud_platform = Some(CLOUD_PLATFORM_CLOUD_FUNCTIONS.to_owned());
    (attrs.cloud_region, attrs.faas_instance) =
        tokio::join!(getter.metadata_region(), getter.metadata_instance_id());
    attrs.faas_name = getter.env("K_SERVICE");
    attrs.faas_version = getter.env("FUNCTION_TARGET");
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

impl GcpResourceAttributes {
    fn new(cloud_account_id: String) -> Self {
        Self {
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
        }
    }

    /// Returns the populated attributes, keyed by [semantic convention] name.
    ///
    /// Fields that were not detected are absent. `cloud.provider` is always
    /// present and is always `gcp`. Use this to build the `Resource` of an
    /// OTLP exporter; the [Google Telemetry API] derives the monitored
    /// resource from these attributes.
    ///
    /// With the `opentelemetry` feature, [`From`] builds a
    /// [`Vec<KeyValue>`][opentelemetry::KeyValue] or a
    /// [`Resource`][opentelemetry_sdk::Resource] directly.
    ///
    /// ```no_run
    /// # async fn example() -> Option<()> {
    /// let attrs = gcp_metadata_resolver::resource_attributes().await?;
    /// for (key, value) in attrs.attributes() {
    ///     println!("{key}={value}");
    /// }
    /// # Some(())
    /// # }
    /// ```
    ///
    /// [semantic convention]: https://opentelemetry.io/docs/specs/semconv/resource/
    /// [Google Telemetry API]: https://cloud.google.com/stackdriver/docs/reference/telemetry/overview
    /// [opentelemetry::KeyValue]: https://docs.rs/opentelemetry/latest/opentelemetry/struct.KeyValue.html
    /// [opentelemetry_sdk::Resource]: https://docs.rs/opentelemetry_sdk/latest/opentelemetry_sdk/struct.Resource.html
    pub fn attributes(&self) -> Vec<(&'static str, &str)> {
        let optional = [
            ("cloud.platform", &self.cloud_platform),
            ("cloud.region", &self.cloud_region),
            ("cloud.availability_zone", &self.cloud_availability_zone),
            ("host.id", &self.host_id),
            ("host.name", &self.host_name),
            ("host.type", &self.host_type),
            ("gcp.gce.instance.name", &self.gce_instance_name),
            ("gcp.gce.instance.hostname", &self.gce_instance_hostname),
            (
                "gcp.gce.instance_group_manager.name",
                &self.gce_instance_group_manager_name,
            ),
            (
                "gcp.gce.instance_group_manager.region",
                &self.gce_instance_group_manager_region,
            ),
            (
                "gcp.gce.instance_group_manager.zone",
                &self.gce_instance_group_manager_zone,
            ),
            ("k8s.cluster.name", &self.k8s_cluster_name),
            ("faas.name", &self.faas_name),
            ("faas.version", &self.faas_version),
            ("faas.instance", &self.faas_instance),
        ];
        let mut attributes = vec![("cloud.provider", "gcp")];
        attributes.push(("cloud.account.id", self.cloud_account_id.as_str()));
        attributes.extend(
            optional
                .into_iter()
                .filter_map(|(key, value)| Some((key, value.as_deref()?))),
        );
        attributes
    }
}

pub const CLOUD_PLATFORM_COMPUTE_ENGINE: &str = "gcp_compute_engine";
pub const CLOUD_PLATFORM_KUBERNETES_ENGINE: &str = "gcp_kubernetes_engine";
pub const CLOUD_PLATFORM_CLOUD_RUN: &str = "gcp_cloud_run";
pub const CLOUD_PLATFORM_CLOUD_FUNCTIONS: &str = "gcp_cloud_functions";
pub const CLOUD_PLATFORM_APP_ENGINE: &str = "gcp_app_engine";

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// Returns the last segment of a path such as `projects/123/zones/us-central1-a`.
fn last_segment(path: &str) -> Option<String> {
    path.rsplit_once('/').map(|(_, segment)| segment.to_owned())
}

fn detector() -> &'static ResourceAttributesGetter<HttpMetadataClient> {
    DETECTOR.get_or_init(ResourceAttributesGetter::default)
}

fn zone_to_region(zone: &str) -> Option<&str> {
    zone.rsplit_once('-').map(|(region, _)| region)
}

/// Reads a small text file and trims surrounding whitespace.
fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

static DETECTOR: OnceLock<ResourceAttributesGetter<HttpMetadataClient>> = OnceLock::new();
static DETECTED_ATTRIBUTES: OnceCell<Option<GcpResourceAttributes>> = OnceCell::new();
static DETECTION: OnceCell<Option<Detection>> = OnceCell::new();
static DETECTED_RESOURCE: OnceCell<Result<MonitoredResource, DetectError>> = OnceCell::new();
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
    async fn cloud_platform_empty_cluster_name_is_not_gke() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[("instance/attributes/cluster-name", "\n")]),
            env_getter: |key| match key {
                "K_CONFIGURATION" => Ok(String::new()),
                _ => Err(VarError::NotPresent),
            },
        };
        let resource = detect_resource(&getter).await.unwrap();
        assert!(matches!(resource, MonitoredResource::ComputeEngine { .. }));
    }

    #[tokio::test]
    async fn detect_platform_from_metadata() {
        let gke = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[(
                "instance/attributes/cluster-name",
                "my-cluster",
            )]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        assert_eq!(
            detect_platform(&gke).await,
            Platform::KubernetesEngine {
                cluster_name: "my-cluster".to_owned()
            }
        );

        let gce = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        assert_eq!(detect_platform(&gce).await, Platform::ComputeEngine);

        let unknown = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[("instance/preempted", "")]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        assert_eq!(detect_platform(&unknown).await, Platform::Unknown);

        let inactive = ResourceAttributesGetter {
            metadata_client: FailingMetadataClient,
            env_getter: |_| Err(VarError::NotPresent),
        };
        assert!(detect(&inactive).await.is_none());
    }

    #[tokio::test]
    async fn detect_platform_cloud_function_before_cloud_run() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |key| match key {
                "K_CONFIGURATION" => Ok("my-config".into()),
                "FUNCTION_TARGET" => Ok("handler".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        assert_eq!(detect_platform(&getter).await, Platform::CloudFunction);
    }

    #[test]
    fn cloud_function_is_not_cloud_run_service() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |key| match key {
                "K_CONFIGURATION" => Ok("my-config".into()),
                "FUNCTION_TARGET" => Ok("handler".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        assert!(!getter.is_cloud_run_service());
    }

    #[tokio::test]
    async fn app_engine_project_id_from_env() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[("project/project-id", "")]),
            env_getter: |key| match key {
                "GAE_SERVICE" => Ok("default".into()),
                "GAE_VERSION" => Ok("v1".into()),
                "GAE_INSTANCE" => Ok("instance-1".into()),
                "GOOGLE_CLOUD_PROJECT" => Ok("env-project".into()),
                _ => Err(VarError::NotPresent),
            },
        };
        let resource = detect_resource(&getter).await.unwrap();
        assert!(matches!(resource, MonitoredResource::AppEngine { .. }));
        assert_eq!(resource.project_id(), "env-project");
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
    async fn unknown_platform_is_global() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[("instance/preempted", "")]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let resource = detect_resource(&getter).await.unwrap();
        assert_eq!(
            resource,
            MonitoredResource::Global {
                project_id: "my-project".to_owned()
            }
        );
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
    async fn attributes_are_semconv_keyed() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(
            attrs.attributes(),
            vec![
                ("cloud.provider", "gcp"),
                ("cloud.account.id", "my-project"),
                ("cloud.platform", "gcp_compute_engine"),
                ("cloud.region", "us-central1"),
                ("cloud.availability_zone", "us-central1-a"),
                ("host.id", "1234567891"),
                ("host.name", "my-instance"),
                ("host.type", "projects/1234567890/machineTypes/e2-medium"),
                ("gcp.gce.instance.name", "my-instance"),
                (
                    "gcp.gce.instance.hostname",
                    "my-instance.us-central1-a.c.my-project.internal"
                ),
            ]
        );
    }

    #[cfg(feature = "opentelemetry")]
    #[tokio::test]
    async fn attributes_into_otel_resource() {
        use opentelemetry::{Key, KeyValue, Value};

        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();

        let key_values = Vec::<KeyValue>::from(&attrs);
        assert_eq!(key_values.len(), attrs.attributes().len());

        let resource = opentelemetry_sdk::Resource::from(&attrs);
        assert_eq!(
            resource.get(&Key::from_static_str("cloud.provider")),
            Some(Value::from("gcp"))
        );
        assert_eq!(
            resource.get(&Key::from_static_str("host.id")),
            Some(Value::from("1234567891"))
        );
        // Undetected fields must not reach the resource.
        assert_eq!(resource.get(&Key::from_static_str("faas.name")), None);
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
    async fn resource_attributes_gke_regional() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[
                ("instance/attributes/cluster-name", "my-cluster"),
                (
                    "instance/attributes/cluster-location",
                    "northamerica-northeast1",
                ),
                (
                    "instance/zone",
                    "projects/1234567890/zones/northamerica-northeast1-a",
                ),
            ]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(
            attrs.cloud_platform.as_deref(),
            Some("gcp_kubernetes_engine")
        );
        assert_eq!(
            attrs.cloud_region.as_deref(),
            Some("northamerica-northeast1")
        );
        // the node zone must not hide the cluster region
        assert_eq!(attrs.cloud_availability_zone, None);
        assert_eq!(attrs.k8s_cluster_name.as_deref(), Some("my-cluster"));
        assert_eq!(attrs.host_id.as_deref(), Some("1234567891"));
        assert_eq!(attrs.host_name.as_deref(), Some("my-instance"));
        assert_eq!(
            attrs.host_type.as_deref(),
            Some("projects/1234567890/machineTypes/e2-medium")
        );
    }

    #[tokio::test]
    async fn resource_attributes_gke_zonal() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[
                ("instance/attributes/cluster-name", "my-cluster"),
                ("instance/attributes/cluster-location", "us-east1-b"),
            ]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(
            attrs.cloud_platform.as_deref(),
            Some("gcp_kubernetes_engine")
        );
        // the node is in us-central1-a; the location comes from cluster-location
        assert_eq!(attrs.cloud_region.as_deref(), Some("us-east1"));
        assert_eq!(attrs.cloud_availability_zone.as_deref(), Some("us-east1-b"));
    }

    #[tokio::test]
    async fn resource_attributes_gke_malformed_location() {
        let getter = ResourceAttributesGetter {
            metadata_client: FakeMetadataClient::new(&[
                ("instance/attributes/cluster-name", "my-cluster"),
                ("instance/attributes/cluster-location", "global"),
            ]),
            env_getter: |_| Err(VarError::NotPresent),
        };
        let attrs = detect_resource_attributes(&getter).await.unwrap();
        assert_eq!(
            attrs.cloud_platform.as_deref(),
            Some("gcp_kubernetes_engine")
        );
        assert_eq!(attrs.cloud_region, None);
        assert_eq!(attrs.cloud_availability_zone, None);
        assert_eq!(attrs.host_id.as_deref(), Some("1234567891"));
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
