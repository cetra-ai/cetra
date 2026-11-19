//! The S3 target of the benches and the soak (D119, track R M7).

use cetra_storage::config::{ObjectStoreUrl, S3Config};
use futures::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::{ObjectMeta, ObjectStore};

/// The S3 target of a benchmark (D119), from the environment.
#[derive(Debug, Clone)]
pub(crate) struct S3Env {
    /// `CETRA_TEST_S3_ENDPOINT`.
    pub(crate) endpoint: String,
    /// `CETRA_TEST_S3_BUCKET`.
    pub(crate) bucket: String,
    /// `CETRA_TEST_S3_REGION`, default "us-east-1".
    pub(crate) region: String,
    /// `CETRA_TEST_S3_PATH_STYLE`, default true; "false" or "0" turn it off.
    pub(crate) path_style: bool,
}

impl S3Env {
    /// Pure parser: `lookup(name)` returns the variable's value. `None` when
    /// CETRA_TEST_S3_ENDPOINT is absent; panics when the endpoint is set but the bucket is not.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let endpoint = lookup("CETRA_TEST_S3_ENDPOINT")?;
        let bucket = lookup("CETRA_TEST_S3_BUCKET")
            .expect("CETRA_TEST_S3_BUCKET must be set with CETRA_TEST_S3_ENDPOINT");
        let region = lookup("CETRA_TEST_S3_REGION").unwrap_or_else(|| "us-east-1".to_owned());
        let path_style = !matches!(
            lookup("CETRA_TEST_S3_PATH_STYLE").as_deref(),
            Some("false" | "0")
        );
        Some(Self {
            endpoint,
            bucket,
            region,
            path_style,
        })
    }

    /// `from_lookup` over the process environment.
    pub(crate) fn from_env() -> Option<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The storage config for this target under `prefix`.
    pub(crate) fn config(&self, prefix: &str) -> (ObjectStoreUrl, S3Config) {
        (
            ObjectStoreUrl::S3 {
                bucket: self.bucket.clone(),
                prefix: prefix.to_owned(),
            },
            S3Config {
                region: Some(self.region.clone()),
                endpoint: Some(self.endpoint.clone()),
                force_path_style: self.path_style,
                allow_http: self.endpoint.starts_with("http://"),
            },
        )
    }

    /// A store for listing and deleting, built like the storage's own.
    pub(crate) fn store(&self) -> Result<object_store::aws::AmazonS3, String> {
        AmazonS3Builder::from_env()
            .with_bucket_name(&self.bucket)
            .with_endpoint(&self.endpoint)
            .with_region(&self.region)
            .with_virtual_hosted_style_request(!self.path_style)
            .with_allow_http(self.endpoint.starts_with("http://"))
            .build()
            .map_err(|error| error.to_string())
    }

    /// Every object under `prefix`.
    pub(crate) async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, String> {
        let store = self.store()?;
        let root = object_store::path::Path::from(prefix);
        store
            .list(Some(&root))
            .try_collect()
            .await
            .map_err(|error| error.to_string())
    }

    /// Deletes every object under `prefix`, in one bulk request stream.
    pub(crate) async fn delete_prefix(&self, prefix: &str) -> Result<(), String> {
        let store = self.store()?;
        let root = object_store::path::Path::from(prefix);
        let locations: Vec<object_store::path::Path> = store
            .list(Some(&root))
            .map_ok(|meta| meta.location)
            .try_collect()
            .await
            .map_err(|error| error.to_string())?;
        let input =
            futures::stream::iter(locations.into_iter().map(Ok::<_, object_store::Error>)).boxed();
        let mut deleted = store.delete_stream(input);
        while let Some(result) = deleted.next().await {
            result.map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// What the benches print: "s3://<bucket>/<prefix> at <endpoint>, region <r>,
    /// path style <b>". The real bucket and endpoint appear only for a loopback
    /// endpoint (127.0.0.1, localhost); otherwise the placeholders `<bucket>` and `<endpoint>`.
    pub(crate) fn describe(&self, prefix: &str) -> String {
        let (bucket, endpoint) = if is_loopback(&self.endpoint) {
            (self.bucket.as_str(), self.endpoint.as_str())
        } else {
            ("<bucket>", "<endpoint>")
        };
        format!(
            "s3://{bucket}/{prefix} at {endpoint}, region {}, path style {}",
            self.region, self.path_style
        )
    }
}

/// The host of an endpoint URL: IPv6 literals keep their brackets' contents, without the port.
pub(crate) fn endpoint_host(endpoint: &str) -> &str {
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or("");
    match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    }
}

/// Whether the endpoint's host is 127.0.0.1, localhost or ::1.
fn is_loopback(endpoint: &str) -> bool {
    matches!(endpoint_host(endpoint), "127.0.0.1" | "localhost" | "::1")
}
