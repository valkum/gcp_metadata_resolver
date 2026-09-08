# gcp_metadata_resolver

[![Crates.io](https://img.shields.io/crates/v/gcp_metadata_resolver.svg)](https://crates.io/crates/gcp_metadata_resolver)
[![Docs.rs](https://docs.rs/gcp_metadata_resolver/badge.svg)](https://docs.rs/gcp_metadata_resolver)
[![CI](https://github.com/valkum/gcp_metadata_resolver/workflows/CI/badge.svg)](https://github.com/valkum/gcp_metadata_resolver/actions)

Detects the GCP environment and provides resource metadata for use with telemetry options such as OpenTelemetry.

This crate queries the [GCE metadata server](https://cloud.google.com/compute/docs/metadata/overview) to identify the running platform
(Compute Engine, GKE, Cloud Run, Cloud Functions, App Engine) and exposes two complementary APIs:

- `detected_resource()` returns a `MonitoredResource`, the
  [GCP monitored resource](https://cloud.google.com/logging/docs/api/v2/resource-list) type and labels that the
  Cloud Logging and Cloud Monitoring APIs accept.
- `resource_attributes()` returns `GcpResourceAttributes`, a typed struct of
  [OpenTelemetry semantic convention](https://opentelemetry.io/docs/specs/semconv/resource/cloud/) resource attributes
  suitable for any OTLP exporter (e.g. [GCP Managed Prometheus via OTLP](https://cloud.google.com/stackdriver/docs/otlp-metrics/overview)).

### Converting a `MonitoredResource`

Match on the variant, or use the accessors to get the flat
[`google.api.MonitoredResource`](https://cloud.google.com/logging/docs/reference/v2/rest/v2/MonitoredResource) form:

```rust
let resource = gcp_metadata_resolver::detected_resource().await?;

let r#type = resource.resource_type().to_owned(); // "gce_instance", "cloud_run_revision", ...
let labels: HashMap<String, String> = resource
    .labels()
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
```

`resource_type()` reports the Cloud Logging resource types. Cloud Monitoring accepts the same types except for
App Engine: it has no `gae_app` and uses `gae_instance`, which takes different labels. For metrics, prefer
`resource_attributes()` and an OTLP exporter.

### `stackdriver` feature

[opentelemetry-stackdriver](https://crates.io/crates/opentelemetry-stackdriver) is
[deprecated](https://github.com/open-telemetry/opentelemetry-rust-contrib/issues/609), so this crate does not
depend on it by default. Enable the `stackdriver` feature to get a `From` conversion into its `MonitoredResource`:

```toml
gcp_metadata_resolver = { version = "0.5", features = ["stackdriver"] }
```

```rust
let resource: opentelemetry_stackdriver::MonitoredResource =
    gcp_metadata_resolver::detected_resource().await?.into();
```

The detection logic mirrors the [Go GCP resource detector](https://pkg.go.dev/go.opentelemetry.io/contrib/detectors/gcp) and the
[OTel Collector GCP processor](https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/processor/resourcedetectionprocessor/internal/gcp).

### Attribute matrix

Which `GcpResourceAttributes` fields are populated depends on the detected platform:

| Field | GCE | GKE | Cloud Run | Cloud Run Job | Cloud Functions | App Engine |
|---|---|---|---|---|---|---|
| `cloud_account_id` | x | x | x | x | x | x |
| `cloud_platform` | x | x | x | x | x | x |
| `cloud_region` | x | x | x | x | x | x |
| `cloud_availability_zone` | x | x | | | | x |
| `host_id` | x | x | | | | |
| `host_name` | x | x | | | | |
| `host_type` | x | x | | | | |
| `gce_instance_name` | x | x | | | | |
| `gce_instance_hostname` | x | x | | | | |
| `gce_instance_group_manager_*` | x* | x* | | | | |
| `k8s_cluster_name` | | x | | | | |
| `faas_name` | | | x | x | x | x |
| `faas_version` | | | x | | x | x |
| `faas_instance` | | | x | x | x | x |

*\* MIG fields are only set when the instance belongs to a managed instance group.*

## License

Licensed under either of

* Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
* MIT license
   ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

See [CONTRIBUTING.md](CONTRIBUTING.md).
