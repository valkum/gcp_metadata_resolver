# gcp_metadata_resolver

[![Crates.io](https://img.shields.io/crates/v/gcp_metadata_resolver.svg)](https://crates.io/crates/gcp_metadata_resolver)
[![Docs.rs](https://docs.rs/gcp_metadata_resolver/badge.svg)](https://docs.rs/gcp_metadata_resolver)
[![CI](https://github.com/valkum/gcp_metadata_resolver/workflows/CI/badge.svg)](https://github.com/valkum/gcp_metadata_resolver/actions)

This is a helper crate to support setting up `optentelemetry-stackdriver`.
It will figure in which environment it runs and will create a fitting `MonitoredResource`.
This is based on existing official stackdriver implementations in other languages.

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
