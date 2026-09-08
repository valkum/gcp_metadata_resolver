use std::collections::BTreeMap;

/// A GCP [monitored resource] for the detected environment.
///
/// Each variant maps to one [`google.api.MonitoredResource`] type, and its
/// fields map to that type's labels. Use [`resource_type`] and [`labels`] to
/// get the flat form that the Cloud Logging and Cloud Monitoring APIs accept,
/// or match on the variant to build whatever your client needs.
///
/// Marked `#[non_exhaustive]` so new platforms can be added in future minor
/// versions without breaking callers.
///
/// # Converting
///
/// With the `stackdriver` feature, [`From`] converts this into
/// [`opentelemetry_stackdriver::MonitoredResource`].
///
/// For a client that takes the protobuf form:
///
/// ```
/// # use gcp_metadata_resolver::MonitoredResource;
/// # use std::collections::HashMap;
/// # let resource = MonitoredResource::ComputeEngine {
/// #     project_id: "my-project".to_owned(),
/// #     instance_id: None,
/// #     zone: None,
/// # };
/// let r#type = resource.resource_type().to_owned();
/// let labels: HashMap<String, String> = resource
///     .labels()
///     .into_iter()
///     .map(|(key, value)| (key.to_owned(), value.to_owned()))
///     .collect();
/// ```
///
/// [monitored resource]: https://cloud.google.com/logging/docs/api/v2/resource-list
/// [`google.api.MonitoredResource`]: https://cloud.google.com/logging/docs/reference/v2/rest/v2/MonitoredResource
/// [`resource_type`]: MonitoredResource::resource_type
/// [`labels`]: MonitoredResource::labels
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MonitoredResource {
    /// A Compute Engine VM instance: `gce_instance`.
    ComputeEngine {
        project_id: String,
        instance_id: Option<String>,
        zone: Option<String>,
    },
    /// A container in a GKE cluster: `k8s_container`.
    KubernetesEngine {
        project_id: String,
        location: Option<String>,
        cluster_name: Option<String>,
        namespace_name: Option<String>,
        pod_name: Option<String>,
        container_name: Option<String>,
    },
    /// A Cloud Run service revision: `cloud_run_revision`.
    CloudRunRevision {
        project_id: String,
        location: Option<String>,
        service_name: Option<String>,
        revision_name: Option<String>,
        configuration_name: Option<String>,
    },
    /// A Cloud Run job: `cloud_run_job`.
    CloudRunJob {
        project_id: String,
        location: Option<String>,
        job_name: Option<String>,
    },
    /// A Cloud Functions function: `cloud_function`.
    CloudFunction {
        project_id: String,
        region: Option<String>,
        function_name: Option<String>,
    },
    /// An App Engine application: `gae_app`.
    AppEngine {
        project_id: String,
        module_id: Option<String>,
        version_id: Option<String>,
        zone: Option<String>,
    },
}

impl MonitoredResource {
    /// Returns the [monitored resource type] for this variant, for example
    /// `gce_instance` or `cloud_run_revision`.
    ///
    /// These are the [Cloud Logging resource types]. Cloud Monitoring accepts
    /// the same types except for App Engine: it has no `gae_app` and uses
    /// `gae_instance`, which takes different labels. For metrics, prefer
    /// [`resource_attributes`](crate::resource_attributes) and an OTLP
    /// exporter.
    ///
    /// [monitored resource type]: https://cloud.google.com/logging/docs/api/v2/resource-list
    /// [Cloud Logging resource types]: https://cloud.google.com/logging/docs/api/v2/resource-list
    pub fn resource_type(&self) -> &'static str {
        match self {
            Self::ComputeEngine { .. } => "gce_instance",
            Self::KubernetesEngine { .. } => "k8s_container",
            Self::CloudRunRevision { .. } => "cloud_run_revision",
            Self::CloudRunJob { .. } => "cloud_run_job",
            Self::CloudFunction { .. } => "cloud_function",
            Self::AppEngine { .. } => "gae_app",
        }
    }

    /// Returns the GCP project ID, which every variant carries.
    pub fn project_id(&self) -> &str {
        match self {
            Self::ComputeEngine { project_id, .. }
            | Self::KubernetesEngine { project_id, .. }
            | Self::CloudRunRevision { project_id, .. }
            | Self::CloudRunJob { project_id, .. }
            | Self::CloudFunction { project_id, .. }
            | Self::AppEngine { project_id, .. } => project_id,
        }
    }

    /// Returns the resource labels for the type that [`resource_type`] reports.
    ///
    /// Fields that were not detected are absent from the map. The map is
    /// ordered by label name.
    ///
    /// [`resource_type`]: MonitoredResource::resource_type
    pub fn labels(&self) -> BTreeMap<&'static str, &str> {
        let mut labels = BTreeMap::from([("project_id", self.project_id())]);
        match self {
            Self::ComputeEngine {
                instance_id, zone, ..
            } => {
                labels.extend(instance_id.as_deref().map(|v| ("instance_id", v)));
                labels.extend(zone.as_deref().map(|v| ("zone", v)));
            }
            Self::KubernetesEngine {
                location,
                cluster_name,
                namespace_name,
                pod_name,
                container_name,
                ..
            } => {
                labels.extend(location.as_deref().map(|v| ("location", v)));
                labels.extend(cluster_name.as_deref().map(|v| ("cluster_name", v)));
                labels.extend(namespace_name.as_deref().map(|v| ("namespace_name", v)));
                labels.extend(pod_name.as_deref().map(|v| ("pod_name", v)));
                labels.extend(container_name.as_deref().map(|v| ("container_name", v)));
            }
            Self::CloudRunRevision {
                location,
                service_name,
                revision_name,
                configuration_name,
                ..
            } => {
                labels.extend(location.as_deref().map(|v| ("location", v)));
                labels.extend(service_name.as_deref().map(|v| ("service_name", v)));
                labels.extend(revision_name.as_deref().map(|v| ("revision_name", v)));
                labels.extend(
                    configuration_name
                        .as_deref()
                        .map(|v| ("configuration_name", v)),
                );
            }
            Self::CloudRunJob {
                location, job_name, ..
            } => {
                labels.extend(location.as_deref().map(|v| ("location", v)));
                labels.extend(job_name.as_deref().map(|v| ("job_name", v)));
            }
            Self::CloudFunction {
                region,
                function_name,
                ..
            } => {
                labels.extend(region.as_deref().map(|v| ("region", v)));
                labels.extend(function_name.as_deref().map(|v| ("function_name", v)));
            }
            Self::AppEngine {
                module_id,
                version_id,
                zone,
                ..
            } => {
                labels.extend(module_id.as_deref().map(|v| ("module_id", v)));
                labels.extend(version_id.as_deref().map(|v| ("version_id", v)));
                labels.extend(zone.as_deref().map(|v| ("zone", v)));
            }
        }
        labels
    }
}

#[cfg(feature = "stackdriver")]
#[cfg_attr(docsrs, doc(cfg(feature = "stackdriver")))]
// The target type is deprecated. Building it is the whole point of this impl.
#[allow(deprecated)]
impl From<MonitoredResource> for opentelemetry_stackdriver::MonitoredResource {
    fn from(resource: MonitoredResource) -> Self {
        match resource {
            MonitoredResource::ComputeEngine {
                project_id,
                instance_id,
                zone,
            } => Self::ComputeEngine {
                project_id,
                instance_id,
                zone,
            },
            MonitoredResource::KubernetesEngine {
                project_id,
                location,
                cluster_name,
                namespace_name,
                pod_name,
                container_name,
            } => Self::KubernetesEngine {
                project_id,
                location,
                cluster_name,
                namespace_name,
                pod_name,
                container_name,
            },
            MonitoredResource::CloudRunRevision {
                project_id,
                location,
                service_name,
                revision_name,
                configuration_name,
            } => Self::CloudRunRevision {
                project_id,
                location,
                service_name,
                revision_name,
                configuration_name,
            },
            MonitoredResource::CloudRunJob {
                project_id,
                location,
                job_name,
            } => Self::CloudRunJob {
                project_id,
                location,
                job_name,
            },
            MonitoredResource::CloudFunction {
                project_id,
                region,
                function_name,
            } => Self::CloudFunction {
                project_id,
                region,
                function_name,
            },
            MonitoredResource::AppEngine {
                project_id,
                module_id,
                version_id,
                zone,
            } => Self::AppEngine {
                project_id,
                module_id,
                version_id,
                zone,
            },
        }
    }
}

/// Convenience impl for [`detected_resource`](crate::detected_resource), which
/// returns a shared reference.
#[cfg(feature = "stackdriver")]
#[cfg_attr(docsrs, doc(cfg(feature = "stackdriver")))]
#[allow(deprecated)]
impl From<&MonitoredResource> for opentelemetry_stackdriver::MonitoredResource {
    fn from(resource: &MonitoredResource) -> Self {
        resource.clone().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compute_engine() -> MonitoredResource {
        MonitoredResource::ComputeEngine {
            project_id: "my-project".to_owned(),
            instance_id: Some("1234".to_owned()),
            zone: Some("us-central1-a".to_owned()),
        }
    }

    #[test]
    fn resource_types() {
        let cases = [
            (compute_engine(), "gce_instance"),
            (
                MonitoredResource::KubernetesEngine {
                    project_id: "my-project".to_owned(),
                    location: None,
                    cluster_name: None,
                    namespace_name: None,
                    pod_name: None,
                    container_name: None,
                },
                "k8s_container",
            ),
            (
                MonitoredResource::CloudRunRevision {
                    project_id: "my-project".to_owned(),
                    location: None,
                    service_name: None,
                    revision_name: None,
                    configuration_name: None,
                },
                "cloud_run_revision",
            ),
            (
                MonitoredResource::CloudRunJob {
                    project_id: "my-project".to_owned(),
                    location: None,
                    job_name: None,
                },
                "cloud_run_job",
            ),
            (
                MonitoredResource::CloudFunction {
                    project_id: "my-project".to_owned(),
                    region: None,
                    function_name: None,
                },
                "cloud_function",
            ),
            (
                MonitoredResource::AppEngine {
                    project_id: "my-project".to_owned(),
                    module_id: None,
                    version_id: None,
                    zone: None,
                },
                "gae_app",
            ),
        ];
        for (resource, expected) in cases {
            assert_eq!(resource.resource_type(), expected);
            assert_eq!(resource.project_id(), "my-project");
            // project_id is a required label on every resource type.
            assert_eq!(resource.labels().get("project_id"), Some(&"my-project"));
        }
    }

    #[test]
    fn labels_compute_engine() {
        assert_eq!(
            compute_engine().labels(),
            BTreeMap::from([
                ("project_id", "my-project"),
                ("instance_id", "1234"),
                ("zone", "us-central1-a"),
            ])
        );
    }

    #[test]
    fn labels_kubernetes_engine() {
        let resource = MonitoredResource::KubernetesEngine {
            project_id: "my-project".to_owned(),
            location: Some("us-central1".to_owned()),
            cluster_name: Some("my-cluster".to_owned()),
            namespace_name: Some("default".to_owned()),
            pod_name: Some("my-pod".to_owned()),
            container_name: Some("my-container".to_owned()),
        };
        assert_eq!(
            resource.labels(),
            BTreeMap::from([
                ("project_id", "my-project"),
                ("location", "us-central1"),
                ("cluster_name", "my-cluster"),
                ("namespace_name", "default"),
                ("pod_name", "my-pod"),
                ("container_name", "my-container"),
            ])
        );
    }

    #[test]
    fn labels_cloud_run_revision() {
        let resource = MonitoredResource::CloudRunRevision {
            project_id: "my-project".to_owned(),
            location: Some("us-central1".to_owned()),
            service_name: Some("my-service".to_owned()),
            revision_name: Some("my-service-00001".to_owned()),
            configuration_name: Some("my-service".to_owned()),
        };
        assert_eq!(
            resource.labels(),
            BTreeMap::from([
                ("project_id", "my-project"),
                ("location", "us-central1"),
                ("service_name", "my-service"),
                ("revision_name", "my-service-00001"),
                ("configuration_name", "my-service"),
            ])
        );
    }

    #[test]
    fn labels_cloud_run_job() {
        let resource = MonitoredResource::CloudRunJob {
            project_id: "my-project".to_owned(),
            location: Some("us-central1".to_owned()),
            job_name: Some("my-job".to_owned()),
        };
        assert_eq!(
            resource.labels(),
            BTreeMap::from([
                ("project_id", "my-project"),
                ("location", "us-central1"),
                ("job_name", "my-job"),
            ])
        );
    }

    #[test]
    fn labels_cloud_function() {
        let resource = MonitoredResource::CloudFunction {
            project_id: "my-project".to_owned(),
            region: Some("us-central1".to_owned()),
            function_name: Some("my-function".to_owned()),
        };
        assert_eq!(
            resource.labels(),
            BTreeMap::from([
                ("project_id", "my-project"),
                ("region", "us-central1"),
                ("function_name", "my-function"),
            ])
        );
    }

    #[test]
    fn labels_app_engine() {
        let resource = MonitoredResource::AppEngine {
            project_id: "my-project".to_owned(),
            module_id: Some("default".to_owned()),
            version_id: Some("v1".to_owned()),
            zone: Some("us-central1-a".to_owned()),
        };
        assert_eq!(
            resource.labels(),
            BTreeMap::from([
                ("project_id", "my-project"),
                ("module_id", "default"),
                ("version_id", "v1"),
                ("zone", "us-central1-a"),
            ])
        );
    }

    #[test]
    fn labels_skip_undetected_fields() {
        let resource = MonitoredResource::ComputeEngine {
            project_id: "my-project".to_owned(),
            instance_id: None,
            zone: None,
        };
        assert_eq!(
            resource.labels(),
            BTreeMap::from([("project_id", "my-project")])
        );
    }

    #[cfg(feature = "stackdriver")]
    #[test]
    #[allow(deprecated)]
    fn into_stackdriver_resource() {
        let resource: opentelemetry_stackdriver::MonitoredResource = compute_engine().into();
        assert!(matches!(
            resource,
            opentelemetry_stackdriver::MonitoredResource::ComputeEngine {
                project_id,
                instance_id,
                zone,
            } if project_id == "my-project"
                && instance_id.as_deref() == Some("1234")
                && zone.as_deref() == Some("us-central1-a")
        ));
    }

    #[cfg(feature = "stackdriver")]
    #[test]
    #[allow(deprecated)]
    fn into_stackdriver_resource_by_ref() {
        let resource: opentelemetry_stackdriver::MonitoredResource = (&compute_engine()).into();
        assert!(matches!(
            resource,
            opentelemetry_stackdriver::MonitoredResource::ComputeEngine { .. }
        ));
    }
}
