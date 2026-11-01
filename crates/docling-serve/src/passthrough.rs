//! Cloud source/target passthrough (#139, upstream docling#3795).
//!
//! The JSON convert body accepts docling's service-datamodel `sources` /
//! `target` wire shape: `kind`-tagged source items (`file`, `http`, `s3`,
//! `azure_blob`, `google_cloud_storage` — field names mirror upstream's
//! `FileSource`/`HttpSource`/`*Coordinates` models) and a `kind`-tagged
//! output target (`inbody` default, or the same three cloud stores). The
//! cloud stores go through the `object_store` crate behind the opt-in
//! `cloud` cargo feature; without it the kinds still *parse* and answer a
//! clear "rebuild with --features cloud" error instead of a serde puzzle.
//!
//! The jobkit targets `zip` (one archive of the rendered outputs, in the
//! response body) and `put` (HTTP PUT each output to a caller-supplied —
//! typically pre-signed — URL) are covered too (#303), and need neither
//! `object_store` nor the `cloud` feature.
//!
//! Deliberately not covered (documented in MIGRATION.md): `google_drive`
//! (OAuth flows, no object_store backend) and `presigned_url` (jobkit's
//! ask-the-server-for-upload-URLs flow — `put` with a URL the caller minted
//! covers the use case) — an unknown `kind` is a 400 listing
//! the supported ones. Credentials ride in the request exactly as they do
//! upstream: passthrough assumes a trusted client and TLS, and the whole
//! surface sits behind `--allow-url-fetch` like every other outbound
//! fetch this server makes.

use serde::Deserialize;

fn default_true() -> bool {
    true
}

/// Upstream `S3Coordinates` (docling.datamodel.service.sources), field for
/// field. `endpoint` is host[:port] without protocol; `verify_ssl: false`
/// selects plain http (docling hands it to boto3 the same way).
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[cfg_attr(not(feature = "cloud"), allow(dead_code))]
pub struct S3Coordinates {
    pub endpoint: String,
    #[serde(default = "default_true")]
    pub verify_ssl: bool,
    pub access_key: String,
    pub secret_key: String,
    pub bucket: String,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default)]
    pub max_num_elements: Option<usize>,
}

/// Upstream `AzureBlobCoordinates`. The connection string is parsed for
/// `AccountKey=`/`SharedAccessSignature=` — the two auth shapes the Azure
/// portal hands out.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[cfg_attr(not(feature = "cloud"), allow(dead_code))]
pub struct AzureBlobCoordinates {
    pub account_name: String,
    pub container: String,
    pub connection_string: String,
    #[serde(default)]
    pub blob_prefix: String,
    #[serde(default)]
    pub max_num_elements: Option<usize>,
}

/// Upstream `GoogleCloudStorageCoordinates`. `service_account_key` is the
/// standard service-account JSON (upstream models its fields; any JSON
/// object with the usual keys is accepted here) — omitted, Application
/// Default Credentials apply, as upstream documents.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[cfg_attr(not(feature = "cloud"), allow(dead_code))]
pub struct GcsCoordinates {
    pub bucket: String,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default)]
    pub max_num_elements: Option<usize>,
    /// Accepted for wire parity; object_store takes quota attribution from
    /// the credentials, so nothing reads it.
    #[serde(default)]
    #[allow(dead_code)]
    pub project: Option<String>,
    #[serde(default)]
    pub service_account_key: Option<serde_json::Value>,
}

/// One `sources[]` item — docling's `kind`-discriminated source union.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceSpec {
    /// Upstream `FileSource`: base64 bytes + the filename that selects the
    /// input format. Needs no network and no feature gate.
    File {
        base64_string: String,
        filename: String,
    },
    /// Upstream `HttpSource`: URL plus request headers (auth tokens etc.).
    Http {
        url: String,
        #[serde(default)]
        headers: std::collections::BTreeMap<String, String>,
    },
    S3(S3Coordinates),
    AzureBlob(AzureBlobCoordinates),
    GoogleCloudStorage(GcsCoordinates),
}

/// The `target` — docling's `kind`-discriminated target union: the `inbody`
/// default, the object-store subset, and the jobkit `zip` / `put` kinds
/// (#303).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetSpec {
    Inbody,
    S3(S3Coordinates),
    AzureBlob(AzureBlobCoordinates),
    GoogleCloudStorage(GcsCoordinates),
    /// One zip archive of the rendered outputs, answered in the response
    /// body (#303, jobkit's `ZipTarget`) — a batch download, not outbound.
    Zip,
    /// HTTP PUT each rendered output to `url` (#303, jobkit's `PutTarget`) —
    /// the pre-signed S3/GCS/Azure upload shape: credentials live in the URL
    /// the caller minted, never in the request body.
    Put(PutTarget),
}

/// Upstream `PutTarget`: the one URL every rendered output is PUT to.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PutTarget {
    pub url: String,
}

/// A cloud store reference: coordinates + the key prefix reads list under
/// and writes prepend.
pub enum CloudCoords {
    S3(S3Coordinates),
    Azure(AzureBlobCoordinates),
    Gcs(GcsCoordinates),
}

#[cfg_attr(not(feature = "cloud"), allow(dead_code))]
impl CloudCoords {
    pub fn prefix(&self) -> &str {
        match self {
            CloudCoords::S3(c) => &c.key_prefix,
            CloudCoords::Azure(c) => &c.blob_prefix,
            CloudCoords::Gcs(c) => &c.key_prefix,
        }
    }

    fn max_num_elements(&self) -> Option<usize> {
        match self {
            CloudCoords::S3(c) => c.max_num_elements,
            CloudCoords::Azure(c) => c.max_num_elements,
            CloudCoords::Gcs(c) => c.max_num_elements,
        }
    }

    /// Credential-free display form for responses/errors,
    /// e.g. `s3://bucket/prefix`.
    pub fn display(&self) -> String {
        match self {
            CloudCoords::S3(c) => format!("s3://{}/{}", c.bucket, c.key_prefix),
            CloudCoords::Azure(c) => {
                format!(
                    "azure://{}/{}/{}",
                    c.account_name, c.container, c.blob_prefix
                )
            }
            CloudCoords::Gcs(c) => format!("gs://{}/{}", c.bucket, c.key_prefix),
        }
    }
}

#[cfg(feature = "cloud")]
mod cloud {
    use super::CloudCoords;
    use object_store::path::Path as StorePath;
    // `get` / `put` moved to the `ObjectStoreExt` extension trait in 0.13.
    use object_store::{ObjectStore, ObjectStoreExt};
    use std::sync::Arc;
    use tokio_stream::StreamExt;

    /// How many objects a prefix may expand to when the request doesn't
    /// bound it — matches the multipart path's "a request is one batch"
    /// scale, not a bucket crawl.
    const DEFAULT_MAX_ELEMENTS: usize = 100;

    fn build(coords: &CloudCoords) -> Result<Arc<dyn ObjectStore>, String> {
        match coords {
            CloudCoords::S3(c) => {
                let scheme = if c.verify_ssl { "https" } else { "http" };
                // The endpoint carries the region for AWS-style hosts
                // (s3.us-east-2.amazonaws.com); other S3 implementations
                // ignore the signing region, so a lenient parse + default
                // keeps MinIO/IBM COS endpoints working.
                let region = c
                    .endpoint
                    .split('.')
                    .nth(1)
                    .filter(|s| s.contains('-'))
                    .unwrap_or("us-east-1");
                object_store::aws::AmazonS3Builder::new()
                    .with_endpoint(format!("{scheme}://{}", c.endpoint))
                    .with_allow_http(!c.verify_ssl)
                    .with_region(region)
                    .with_bucket_name(&c.bucket)
                    .with_access_key_id(&c.access_key)
                    .with_secret_access_key(&c.secret_key)
                    .build()
                    .map(|s| Arc::new(s) as Arc<dyn ObjectStore>)
                    .map_err(|e| format!("s3 store: {e}"))
            }
            CloudCoords::Azure(c) => {
                let mut builder = object_store::azure::MicrosoftAzureBuilder::new()
                    .with_account(&c.account_name)
                    .with_container_name(&c.container);
                // The portal's connection strings carry either an
                // AccountKey or a SharedAccessSignature.
                let mut authed = false;
                for part in c.connection_string.split(';') {
                    if let Some(key) = part.strip_prefix("AccountKey=") {
                        builder = builder.with_access_key(key);
                        authed = true;
                    } else if let Some(sas) = part.strip_prefix("SharedAccessSignature=") {
                        builder =
                            builder.with_config(object_store::azure::AzureConfigKey::SasKey, sas);
                        authed = true;
                    }
                }
                if !authed {
                    return Err("azure connection_string carries neither AccountKey= nor \
                         SharedAccessSignature="
                        .into());
                }
                builder
                    .build()
                    .map(|s| Arc::new(s) as Arc<dyn ObjectStore>)
                    .map_err(|e| format!("azure store: {e}"))
            }
            CloudCoords::Gcs(c) => {
                let mut builder =
                    object_store::gcp::GoogleCloudStorageBuilder::new().with_bucket_name(&c.bucket);
                if let Some(sa) = &c.service_account_key {
                    builder = builder.with_service_account_key(sa.to_string());
                }
                // ADC applies when no key is passed (object_store reads
                // GOOGLE_APPLICATION_CREDENTIALS / metadata itself);
                // `project` only matters for quota attribution, which the
                // XML/JSON storage APIs take from the credentials.
                builder
                    .build()
                    .map(|s| Arc::new(s) as Arc<dyn ObjectStore>)
                    .map_err(|e| format!("gcs store: {e}"))
            }
        }
    }

    /// List `prefix` (bounded by `max_num_elements`, default
    /// [`DEFAULT_MAX_ELEMENTS`]) and download each object. Names are the
    /// key's last segment — what format detection runs on.
    pub async fn fetch_sources(coords: &CloudCoords) -> Result<Vec<(String, Vec<u8>)>, String> {
        let store = build(coords)?;
        let max = coords.max_num_elements().unwrap_or(DEFAULT_MAX_ELEMENTS);
        let prefix = coords.prefix().trim_matches('/').to_string();
        let list_prefix = (!prefix.is_empty()).then(|| StorePath::from(prefix.as_str()));
        let mut keys: Vec<StorePath> = Vec::new();
        {
            let mut listing = store.list(list_prefix.as_ref());
            while let Some(meta) = listing.next().await {
                let meta = meta.map_err(|e| format!("listing {}: {e}", coords.display()))?;
                keys.push(meta.location);
                if keys.len() >= max {
                    break;
                }
            }
        }
        if keys.is_empty() {
            return Err(format!("no objects under {}", coords.display()));
        }
        // Deterministic order for reproducible batch responses.
        keys.sort();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let bytes = store
                .get(&key)
                .await
                .map_err(|e| format!("fetching {key}: {e}"))?
                .bytes()
                .await
                .map_err(|e| format!("reading {key}: {e}"))?;
            let name = key
                .filename()
                .map(str::to_string)
                .unwrap_or_else(|| key.to_string());
            out.push((name, bytes.to_vec()));
        }
        Ok(out)
    }

    /// Upload one rendered output under the target's prefix.
    pub async fn put_object(coords: &CloudCoords, key: &str, bytes: Vec<u8>) -> Result<(), String> {
        let store = build(coords)?;
        let prefix = coords.prefix().trim_matches('/');
        let full = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}/{key}")
        };
        store
            .put(&StorePath::from(full.as_str()), bytes.into())
            .await
            .map(|_| ())
            .map_err(|e| format!("uploading {key} to {}: {e}", coords.display()))
    }
}

#[cfg(feature = "cloud")]
pub use cloud::{fetch_sources, put_object};

#[cfg(not(feature = "cloud"))]
mod no_cloud {
    use super::CloudCoords;

    const MSG: &str = "cloud sources/targets need the `cloud` build feature — rebuild \
                       docling-serve with `--features cloud` (feature-gated object_store, #139)";

    pub async fn fetch_sources(_coords: &CloudCoords) -> Result<Vec<(String, Vec<u8>)>, String> {
        Err(MSG.into())
    }

    pub async fn put_object(
        _coords: &CloudCoords,
        _key: &str,
        _bytes: Vec<u8>,
    ) -> Result<(), String> {
        Err(MSG.into())
    }
}

#[cfg(not(feature = "cloud"))]
pub use no_cloud::{fetch_sources, put_object};
