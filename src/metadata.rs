//! A small client for the Google Cloud Platform metadata service.
use std::str;

use http_body_util::{BodyExt, Full};
use hyper::{StatusCode, body::Bytes};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use thiserror::Error;

/// A client for the GCP metadata service.
#[allow(async_fn_in_trait)]
pub trait MetadataClient {
    /// Returns a value from the metadata service as well as the associated ETag.
    async fn resolve_etag(&self, suffix: &str) -> Result<(String, Option<String>), Error>;

    /// Returns a value from the metadata service.
    async fn resolve(&self, suffix: &str) -> Result<String, Error>;
}

pub struct HttpMetadataClient {
    client: Client<HttpConnector, Full<Bytes>>,
}

impl HttpMetadataClient {
    pub fn new(client: Client<HttpConnector, Full<Bytes>>) -> Self {
        Self { client }
    }
}

impl MetadataClient for HttpMetadataClient {
    /// Returns a value from the metadata service as well as the associated ETag.
    ///
    /// Follows the go SDK implementation.
    async fn resolve_etag(&self, suffix: &str) -> Result<(String, Option<String>), Error> {
        // Using a fixed IP makes it very difficult to spoof the metadata service in
        // a container, which is an important use-case for local testing of cloud
        // deployments. To enable spoofing of the metadata service, the environment
        // variable GCE_METADATA_HOST is first inspected to decide where metadata
        // requests shall go.
        let possible_host_override = std::env::var(METADATA_HOST_ENV);
        let host = possible_host_override.as_deref().unwrap_or({
            // Using 169.254.169.254 instead of "metadata" or "metadata.google.internal" here because
            // we can't know how the user's network is configured.
            METADATA_IP
        });

        let suffix = suffix.trim_end_matches('/');
        let url = format!("http://{host}/computeMetadata/v1/{suffix}");
        let req = hyper::http::Request::builder()
            .uri(url)
            .header("Metadata-Flavor", "Google")
            .header("User-Agent", USER_AGENT)
            .body(Full::default())
            .map_err(HttpError::from)?;
        // The Go SDK retries this request. We don't do that here. For now.
        let res = self.client.request(req).await.map_err(HttpError::from)?;
        let (parts, body) = res.into_parts();

        if parts.status == StatusCode::NOT_FOUND {
            return Err(Error::NotDefined(suffix.to_owned()));
        }

        let body_bytes = body.collect().await.map_err(HttpError::from)?.to_bytes();
        let body = str::from_utf8(&body_bytes)
            .map_err(HttpError::from)?
            .to_owned();
        if parts.status != 200 {
            return Err(Error::NotOk(parts.status, body));
        }
        let etag = parts
            .headers
            .get("ETag")
            .and_then(|header| header.to_str().map(ToOwned::to_owned).ok());
        Ok((body, etag))
    }

    async fn resolve(&self, suffix: &str) -> Result<String, Error> {
        let (body, _) = self.resolve_etag(suffix).await?;
        Ok(body)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("HTTP error: {0}")]
    Http(#[from] HttpError),

    #[error("Metadata Server error: {0}, {1}")]
    NotOk(StatusCode, String),

    #[error("Suffix {0} not defined")]
    NotDefined(String),
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("HTTP error: {0}")]
    HyperHttp(#[from] hyper::http::Error),

    #[error("HTTP error: {0}")]
    HyperClient(#[from] hyper_util::client::legacy::Error),

    #[error("HTTP error: {0}")]
    Hyper(#[from] hyper::Error),

    #[error("HTTP encoding error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

/// The documented metadata server IP address.
///
/// See: https://cloud.google.com/compute/docs/metadata/querying-metadata#metadata_server_endpoints
const METADATA_IP: &str = "169.254.169.254";

/// The environment variable specifying the GCE metadata hostname.
/// If empty, the default value of metadataIP ("169.254.169.254") is used instead.
///
/// According to the go SDK, this is variable name is not defined by any spec and
/// was made up for the Go package.
const METADATA_HOST_ENV: &str = "GCE_METADATA_HOST";

const USER_AGENT: &str = "rust-gcp_metadata_resolver/0.1";
