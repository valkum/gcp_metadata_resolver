# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `DetectError` implements `Clone`.

### Changed

- `detected_resource()` and `resource_attributes()` share one platform
  detection, and detection sends independent metadata requests together.
- `detected_resource()` caches its first verdict, also when it is an error.
  Before, it ran detection again after an error. The documentation now states
  that both functions cache the first verdict for the life of the process.
- An empty environment variable or metadata value is now absent. Before, a
  label or an attribute got the empty string, e.g. `K_SERVICE=""` gave
  `Some("")`.

### Fixed

- Trim the DMI product name. Before, the trailing newline stopped the App
  Engine and Cloud Functions fast path from matching.
- Log a metadata key that is not defined at `debug` level, not at `error`
  level. Detection probes keys that are absent on most platforms.
- Take the GKE namespace from `NAMESPACE_NAME` also when the service account
  file is not present.

## [0.5.1](https://github.com/valkum/gcp_metadata_resolver/compare/v0.5.0...v0.5.1) - 2026-10-15

### Other

- *(deps)* bump actions/checkout from 7.0.0 to 7.0.1
- *(deps)* bump actions-rust-lang/setup-rust-toolchain
- *(deps)* bump release-plz/action from 0.5.130 to 0.5.141

### Fixed

- On GKE, take `cloud.region` and `cloud.availability_zone` only from the
  `cluster-location` instance attribute. Before, a regional cluster also got
  the node zone in `cloud.availability_zone`. Cloud Logging fills the
  `location` label from `cloud.availability_zone` first, so logs from a
  regional cluster (e.g. Autopilot) had the node zone, not the cluster region.
- A `cluster-location` in an unknown format now sets neither field.

## [0.5.0](https://github.com/valkum/gcp_metadata_resolver/compare/v0.4.0...v0.5.0) - 2026-10-15

### Added

- Add `MonitoredResource`, a GCP monitored resource type that this crate owns.
  It has `resource_type()`, `project_id()` and `labels()` accessors that give
  the `google.api.MonitoredResource` form.
- Add the optional `stackdriver` feature. It converts `MonitoredResource` into
  `opentelemetry_stackdriver::MonitoredResource`.
- Add `GcpResourceAttributes::attributes()`. It returns the populated
  attributes, keyed by semantic convention name.
- Add the optional `opentelemetry` feature. It converts
  `GcpResourceAttributes` into `Vec<KeyValue>` and into
  `opentelemetry_sdk::Resource`.

### Changed

- **Breaking**: `detected_resource()` returns the `MonitoredResource` of this
  crate. Before, it returned `opentelemetry_stackdriver::MonitoredResource`.
  Enable the `stackdriver` feature and call `.into()` to keep the old type.
- **Breaking**: `opentelemetry-stackdriver` is deprecated. It is now an
  optional dependency and it is off by default. This removes 98 of the 140
  transitive dependencies from a default build.

### Fixed

- Make the metadata server URL in the documentation a hyperlink. It failed the
  `rustdoc::bare_urls` lint.

## [0.4.0](https://github.com/valkum/gcp_metadata_resolver/compare/v0.3.2...v0.4.0) - 2026-05-19

### Other

- *(deps)* bump actions-rust-lang/setup-rust-toolchain
- *(deps)* bump release-plz/action from 0.5.128 to 0.5.129
- *(deps)* bump EmbarkStudios/cargo-deny-action from 2.0.16 to 2.0.18
- *(deps)* update opentelemetry-stackdriver requirement

## [0.3.2](https://github.com/valkum/gcp_metadata_resolver/compare/v0.3.1...v0.3.2) - 2026-04-14

### Added

- add OpenTelemetry semantic convention resource attributes
- add timeouts to metadata HTTP client
- stick to single ResourceAttributesGetter
- add pub getters for instanceId and projectId

### Other

- *(deps)* bump EmbarkStudios/cargo-deny-action from 2.0.15 to 2.0.16
- extract cloud_platform string literals into constants
- Bump release-plz/action from 0.5.127 to 0.5.128
- Bump actions-rust-lang/setup-rust-toolchain from 1.15.2 to 1.15.4

## [0.3.1](https://github.com/valkum/gcp_metadata_resolver/compare/v0.3.0...v0.3.1) - 2026-02-19

### Fixed

- Fixed identification of Cloud Run services (#19)

### Other

- Pin GitHub actions
- Switch to cargo-deny
- Bump actions/checkout from 5 to 6

## [0.3.0](https://github.com/valkum/gcp_metadata_resolver/compare/v0.2.0...v0.3.0) - 2025-10-06

### Changed

- Update opentelemetry-stackdriver requirement from 0.27 to 0.28

### Other

- Bump actions/checkout from 4 to 5
- fix changelog

## [0.2.0](https://github.com/valkum/gcp_metadata_resolver/compare/v0.1.1...v0.2.0) - 2025-06-24

### Changed

- Update opentelemetry-stackdriver requirement from 0.26 to 0.27

### Other

- switch to release-plz

## [0.1.1](https://github.com/valkum/gcp_metadata_resolver/compare/v0.1.0...v0.1.0) - 2025-04-17

### Other

- remove lockfile

## [0.1.0] - 2025-04-17

Initial Release
