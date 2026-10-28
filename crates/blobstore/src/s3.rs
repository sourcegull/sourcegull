//! Blobs in an S3-compatible object store: MinIO, Garage,
//! SeaweedFS, Ceph RGW, or Amazon S3. Only the S3 protocol is used (signature V4, path-style
//! requests by default), through the `object_store` crate.
//!
//! The store is named by `s3://<bucket>[/<prefix>]` (see [`S3Config::from_url`]); the
//! endpoint, region and credentials come from the URL's query or the environment.
//!
//! Writes are buffered up to the part size: smaller blobs are one `PUT`, larger ones a
//! multipart upload. Either way the object only appears once complete, and a failed write
//! aborts the upload, so readers never see a partial blob and the old blob stays.

use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::buffered::BufWriter;
use object_store::path::Path as ObjectPath;
use object_store::prefix::PrefixStore;
use object_store::{ClientOptions, GetOptions, GetRange, ObjectStore};
use tokio::io::AsyncWriteExt;

use crate::{BlobStore, ByteStream, Error, Result, validate_key};

/// The default part size of multipart uploads (S3 requires at least 5 MiB but the last part).
pub const DEFAULT_PART_SIZE: usize = 8 * 1024 * 1024;

/// The smallest part size S3 accepts.
pub const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

/// Where and how to reach the bucket.
#[derive(Clone, PartialEq, Eq)]
pub struct S3Config {
    pub bucket: String,
    /// Key prefix inside the bucket (no leading or trailing `/`), so several installations or
    /// other data can share a bucket.
    pub prefix: String,
    /// `http(s)://host:port` of the S3 API. `None`: Amazon S3 for `region`.
    pub endpoint: Option<String>,
    pub region: String,
    /// Path-style requests (`endpoint/bucket/key`) rather than virtual-hosted style
    /// (`bucket.endpoint/key`). Most self-hosted servers need path style.
    pub path_style: bool,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// PEM file with an extra CA certificate for the endpoint (self-signed or private CA).
    pub ca_cert: Option<PathBuf>,
    /// Accept any TLS certificate (testing only).
    pub insecure_skip_verify: bool,
    /// Part size of multipart uploads; blobs below it are written with one `PUT`.
    pub part_size: usize,
    /// Create the bucket at startup when it does not exist.
    pub create_bucket: bool,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("path_style", &self.path_style)
            .field("access_key_id", &self.access_key_id)
            .field("ca_cert", &self.ca_cert)
            .field("insecure_skip_verify", &self.insecure_skip_verify)
            .field("part_size", &self.part_size)
            .field("create_bucket", &self.create_bucket)
            .finish_non_exhaustive()
    }
}

fn bool_value(name: &str, v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(Error::Remote(format!(
            "{name}: expected a boolean, got {v:?}"
        ))),
    }
}

impl S3Config {
    /// Parses `s3://<bucket>[/<prefix>][?endpoint=…&region=…&path_style=…]`, with the other
    /// settings from `env` (the process environment in [`crate::open`]). Query parameters win
    /// over the environment; empty values count as unset.
    ///
    /// | Setting | Query | Environment (first set wins) | Default |
    /// |---|---|---|---|
    /// | endpoint | `endpoint` | `SOURCEGULL_S3_ENDPOINT`, `AWS_ENDPOINT_URL` | Amazon S3 |
    /// | region | `region` | `SOURCEGULL_S3_REGION`, `AWS_REGION`, `AWS_DEFAULT_REGION` | `us-east-1` |
    /// | path-style requests | `path_style` | `SOURCEGULL_S3_PATH_STYLE` | `true` |
    /// | access key | | `SOURCEGULL_S3_ACCESS_KEY_ID`, `AWS_ACCESS_KEY_ID` | required |
    /// | secret key | | `SOURCEGULL_S3_SECRET_ACCESS_KEY`, `AWS_SECRET_ACCESS_KEY` | required |
    /// | session token | | `SOURCEGULL_S3_SESSION_TOKEN`, `AWS_SESSION_TOKEN` | none |
    /// | extra CA (PEM file) | `ca_cert` | `SOURCEGULL_S3_CA_CERT` | none |
    /// | skip TLS verification | `insecure` | `SOURCEGULL_S3_INSECURE_SKIP_VERIFY` | `false` |
    /// | multipart part size (bytes) | `part_size` | `SOURCEGULL_S3_PART_SIZE` | 8 MiB |
    /// | create the bucket at startup | `create_bucket` | `SOURCEGULL_S3_CREATE_BUCKET` | `false` |
    ///
    /// Credentials are never read from the URL, which is logged.
    pub fn from_url(s3_url: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let bad = |why: &str| Error::Remote(format!("invalid S3 blob store URL {s3_url:?}: {why}"));
        let url = url::Url::parse(s3_url).map_err(|e| bad(&e.to_string()))?;
        if url.scheme() != "s3" {
            return Err(bad("the scheme must be s3"));
        }
        let bucket = url.host_str().unwrap_or_default().to_owned();
        if bucket.is_empty() {
            return Err(bad("no bucket"));
        }
        let prefix = url.path().trim_matches('/').to_owned();
        if !prefix.is_empty() {
            validate_key(&prefix).map_err(|_| bad("invalid key prefix"))?;
        }
        let from_env = |names: &[&str]| names.iter().find_map(|n| env(n).filter(|v| !v.is_empty()));
        let mut query = std::collections::HashMap::new();
        for (k, v) in url.query_pairs() {
            if !v.is_empty() {
                query.insert(k.into_owned(), v.into_owned());
            }
        }
        let mut setting = |key: &str, names: &[&str]| query.remove(key).or_else(|| from_env(names));

        let endpoint = setting("endpoint", &["SOURCEGULL_S3_ENDPOINT", "AWS_ENDPOINT_URL"])
            .map(|e| e.trim_end_matches('/').to_owned());
        if let Some(e) = &endpoint
            && !(e.starts_with("http://") || e.starts_with("https://"))
        {
            return Err(bad("the endpoint must be an http:// or https:// URL"));
        }
        let region = setting(
            "region",
            &["SOURCEGULL_S3_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"],
        )
        .unwrap_or_else(|| "us-east-1".to_owned());
        let path_style = setting("path_style", &["SOURCEGULL_S3_PATH_STYLE"])
            .map(|v| bool_value("path_style", &v))
            .transpose()?
            .unwrap_or(true);
        let ca_cert = setting("ca_cert", &["SOURCEGULL_S3_CA_CERT"]).map(PathBuf::from);
        let insecure_skip_verify = setting("insecure", &["SOURCEGULL_S3_INSECURE_SKIP_VERIFY"])
            .map(|v| bool_value("insecure", &v))
            .transpose()?
            .unwrap_or(false);
        let part_size = match setting("part_size", &["SOURCEGULL_S3_PART_SIZE"]) {
            Some(v) => v
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= MIN_PART_SIZE)
                .ok_or_else(|| bad(&format!("part_size must be at least {MIN_PART_SIZE}")))?,
            None => DEFAULT_PART_SIZE,
        };
        let create_bucket = setting("create_bucket", &["SOURCEGULL_S3_CREATE_BUCKET"])
            .map(|v| bool_value("create_bucket", &v))
            .transpose()?
            .unwrap_or(false);
        if let Some(k) = query.keys().next() {
            return Err(bad(&format!("unknown parameter {k:?}")));
        }
        let access_key_id = from_env(&["SOURCEGULL_S3_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"])
            .ok_or_else(|| bad("set SOURCEGULL_S3_ACCESS_KEY_ID"))?;
        let secret_access_key =
            from_env(&["SOURCEGULL_S3_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY"])
                .ok_or_else(|| bad("set SOURCEGULL_S3_SECRET_ACCESS_KEY"))?;
        let session_token = from_env(&["SOURCEGULL_S3_SESSION_TOKEN", "AWS_SESSION_TOKEN"]);
        Ok(Self {
            bucket,
            prefix,
            endpoint,
            region,
            path_style,
            access_key_id,
            secret_access_key,
            session_token,
            ca_cert,
            insecure_skip_verify,
            part_size,
            create_bucket,
        })
    }
}

/// A store in an S3 bucket.
#[derive(Debug, Clone)]
pub struct S3BlobStore {
    store: Arc<dyn ObjectStore>,
    config: S3Config,
    description: String,
}

fn remote(e: object_store::Error) -> Error {
    Error::Remote(e.to_string())
}

fn map_err(key: &str) -> impl Fn(object_store::Error) -> Error + '_ {
    move |e| match e {
        object_store::Error::NotFound { .. } => Error::NotFound(key.to_owned()),
        e => remote(e),
    }
}

impl S3BlobStore {
    /// A client of the bucket. Does not contact the server.
    pub fn new(config: &S3Config) -> Result<Self> {
        let mut options = ClientOptions::new()
            .with_connect_timeout(Duration::from_secs(10))
            // The timeout covers a whole request, body included: reading a large SCIP index
            // over a slow link takes long (object_store's default is 30 s).
            .with_timeout(Duration::from_secs(30 * 60));
        if let Some(path) = &config.ca_cert {
            let pem = std::fs::read(path)?;
            let cert = object_store::Certificate::from_pem(&pem).map_err(remote)?;
            options = options.with_root_certificate(cert);
        }
        if config.insecure_skip_verify {
            options = options.with_allow_invalid_certificates(true);
        }
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_virtual_hosted_style_request(!config.path_style)
            .with_client_options(options);
        if let Some(endpoint) = &config.endpoint {
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"));
        }
        if let Some(token) = &config.session_token {
            builder = builder.with_token(token);
        }
        let s3 = builder.build().map_err(remote)?;
        let store: Arc<dyn ObjectStore> = if config.prefix.is_empty() {
            Arc::new(s3)
        } else {
            Arc::new(PrefixStore::new(
                s3,
                ObjectPath::parse(&config.prefix).map_err(|e| Error::Remote(e.to_string()))?,
            ))
        };
        let description = format!(
            "s3://{}/{} at {}",
            config.bucket,
            config.prefix,
            config.endpoint.as_deref().unwrap_or("Amazon S3")
        );
        Ok(Self {
            store,
            config: config.clone(),
            description,
        })
    }

    /// Creates the bucket (`PUT /<bucket>`); an existing bucket owned by these credentials is
    /// fine. object_store has no bucket API, so this signs the request with its SigV4 signer.
    pub async fn create_bucket(&self) -> Result<()> {
        let c = &self.config;
        let url = match &c.endpoint {
            Some(e) if c.path_style => format!("{e}/{}", c.bucket),
            Some(e) => {
                let mut u = url::Url::parse(e).map_err(|e| Error::Remote(e.to_string()))?;
                let host = format!("{}.{}", c.bucket, u.host_str().unwrap_or_default());
                u.set_host(Some(&host))
                    .map_err(|e| Error::Remote(e.to_string()))?;
                u.to_string()
            }
            None => format!("https://s3.{}.amazonaws.com/{}", c.region, c.bucket),
        };
        // Outside us-east-1, Amazon S3 wants the region in the body.
        let body = if c.region == "us-east-1" {
            String::new()
        } else {
            format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                c.region
            )
        };
        let mut request = http::Request::put(&url)
            .body(object_store::client::HttpRequestBody::from(body.clone()))
            .map_err(|e| Error::Remote(e.to_string()))?;
        let credential = object_store::aws::AwsCredential {
            key_id: c.access_key_id.clone(),
            secret_key: c.secret_access_key.clone(),
            token: c.session_token.clone(),
        };
        object_store::aws::AwsAuthorizer::new(&credential, "s3", &c.region)
            .authorize(&mut request, None);

        let mut client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
        if let Some(path) = &c.ca_cert {
            let cert = reqwest::Certificate::from_pem(&std::fs::read(path)?)
                .map_err(|e| Error::Remote(e.to_string()))?;
            client = client.add_root_certificate(cert);
        }
        if c.insecure_skip_verify {
            client = client.danger_accept_invalid_certs(true);
        }
        let client = client.build().map_err(|e| Error::Remote(e.to_string()))?;
        let resp = client
            .put(&url)
            .headers(request.headers().clone())
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Remote(format!("creating bucket {}: {e}", c.bucket)))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        // MinIO and Amazon S3 answer 409 BucketAlreadyOwnedByYou for our own bucket.
        if status.as_u16() == 409 && text.contains("BucketAlreadyOwnedByYou") {
            return Ok(());
        }
        Err(Error::Remote(format!(
            "creating bucket {}: {status}: {text}",
            c.bucket
        )))
    }

    /// Where the blobs are, for logs.
    pub fn description(&self) -> &str {
        &self.description
    }

    fn path(&self, key: &str) -> Result<ObjectPath> {
        validate_key(key)?;
        ObjectPath::parse(key).map_err(|_| Error::InvalidKey(key.to_owned()))
    }
}

#[async_trait::async_trait]
impl BlobStore for S3BlobStore {
    async fn init(&self) -> Result<()> {
        if self.config.create_bucket {
            self.create_bucket().await?;
        }
        tracing::info!(store = %self.description, "using an S3 blob store");
        Ok(())
    }

    async fn put(&self, key: &str, mut body: ByteStream) -> Result<u64> {
        let path = self.path(key)?;
        let mut writer = BufWriter::with_capacity(self.store.clone(), path, self.config.part_size);
        let mut n = 0u64;
        while let Some(chunk) = body.next().await {
            let written = match chunk {
                Ok(chunk) => {
                    n += chunk.len() as u64;
                    writer.put(chunk).await.map_err(remote)
                }
                Err(e) => Err(e.into()),
            };
            if let Err(e) = written {
                // Nothing becomes visible: a buffered write was never sent, and a started
                // multipart upload is aborted.
                if let Err(abort) = writer.abort().await {
                    tracing::warn!(error = %abort, key, "aborting a multipart upload failed");
                }
                return Err(e);
            }
        }
        writer
            .shutdown()
            .await
            .map_err(|e| Error::Remote(format!("writing blob {key}: {e}")))?;
        Ok(n)
    }

    async fn get(&self, key: &str) -> Result<ByteStream> {
        let result = self
            .store
            .get(&self.path(key)?)
            .await
            .map_err(map_err(key))?;
        Ok(result.into_stream().map_err(io::Error::other).boxed())
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<ByteStream> {
        let path = self.path(key)?;
        // S3 answers 416 for ranges starting at or after the end; the trait clips instead.
        let size = self.store.head(&path).await.map_err(map_err(key))?.size;
        let end = range.end.min(size);
        if range.start >= end {
            return Ok(futures::stream::empty().boxed());
        }
        let options = GetOptions {
            range: Some(GetRange::Bounded(range.start..end)),
            ..GetOptions::default()
        };
        let result = self
            .store
            .get_opts(&path, options)
            .await
            .map_err(map_err(key))?;
        Ok(result.into_stream().map_err(io::Error::other).boxed())
    }

    async fn size(&self, key: &str) -> Result<Option<u64>> {
        match self.store.head(&self.path(key)?).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(remote(e)),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        match self.store.delete(&self.path(key)?).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(remote(e)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        // Object store prefixes are whole path segments; list the directory holding the
        // prefix and filter.
        let dir = match prefix.rfind('/') {
            Some(i) => Some(self.path(&prefix[..i])?),
            None => None,
        };
        let mut out: Vec<String> = self
            .store
            .list(dir.as_ref())
            .map_ok(|meta| meta.location.to_string())
            .try_collect()
            .await
            .map_err(remote)?;
        // Objects written by other tools may not be valid keys; they are not ours.
        out.retain(|k| k.starts_with(prefix) && validate_key(k).is_ok());
        out.sort();
        Ok(out)
    }
}

/// Helpers for tests against a real S3 server (MinIO in the dev compose, profile `s3`).
pub mod testing {
    use super::*;

    /// A store in the test bucket under `prefix`, the bucket created if missing; `None` (the
    /// test is skipped) when `SOURCEGULL_TEST_S3_ENDPOINT` (e.g. `http://minio:9000`) is not set.
    /// Credentials: `SOURCEGULL_TEST_S3_ACCESS_KEY_ID` and `SOURCEGULL_TEST_S3_SECRET_ACCESS_KEY`
    /// (default `minioadmin`); bucket: `SOURCEGULL_TEST_S3_BUCKET` (default `sourcegull-test`). Parts
    /// are the smallest S3 allows, so multipart uploads start at 5 MiB.
    pub async fn s3_from_env(prefix: &str) -> Option<S3BlobStore> {
        let endpoint = std::env::var("SOURCEGULL_TEST_S3_ENDPOINT")
            .ok()
            .filter(|v| !v.is_empty());
        let Some(endpoint) = endpoint else {
            eprintln!("SOURCEGULL_TEST_S3_ENDPOINT is not set: no S3 test store");
            return None;
        };
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
        let config = S3Config {
            bucket: var("SOURCEGULL_TEST_S3_BUCKET", "sourcegull-test"),
            prefix: prefix.to_owned(),
            endpoint: Some(endpoint),
            region: "us-east-1".to_owned(),
            path_style: true,
            access_key_id: var("SOURCEGULL_TEST_S3_ACCESS_KEY_ID", "minioadmin"),
            secret_access_key: var("SOURCEGULL_TEST_S3_SECRET_ACCESS_KEY", "minioadmin"),
            session_token: None,
            ca_cert: None,
            insecure_skip_verify: false,
            part_size: MIN_PART_SIZE,
            create_bucket: true,
        };
        let store = S3BlobStore::new(&config).expect("S3 test store");
        store.init().await.expect("creating the S3 test bucket");
        Some(store)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| vars.get(k).cloned()
    }

    const CREDS: [(&str, &str); 2] = [
        ("SOURCEGULL_S3_ACCESS_KEY_ID", "AKID"),
        ("SOURCEGULL_S3_SECRET_ACCESS_KEY", "SECRET"),
    ];

    #[test]
    fn config_from_url_and_env() {
        let c = S3Config::from_url("s3://sourcegull", env(&CREDS)).unwrap();
        assert_eq!(c.bucket, "sourcegull");
        assert_eq!(c.prefix, "");
        assert_eq!(c.endpoint, None);
        assert_eq!(c.region, "us-east-1");
        assert!(c.path_style);
        assert_eq!(c.part_size, DEFAULT_PART_SIZE);
        assert_eq!(c.access_key_id, "AKID");
        assert_eq!(c.session_token, None);

        let mut vars = CREDS.to_vec();
        vars.extend([
            ("SOURCEGULL_S3_ENDPOINT", "http://minio:9000/"),
            ("SOURCEGULL_S3_REGION", "eu-1"),
            ("SOURCEGULL_S3_PATH_STYLE", "false"),
            ("AWS_SESSION_TOKEN", "tok"),
            ("SOURCEGULL_S3_REGION_UNUSED", "x"),
        ]);
        let c = S3Config::from_url("s3://b/a/b/", env(&vars)).unwrap();
        assert_eq!(c.prefix, "a/b");
        assert_eq!(c.endpoint.as_deref(), Some("http://minio:9000"));
        assert_eq!(c.region, "eu-1");
        assert!(!c.path_style);
        assert_eq!(c.session_token.as_deref(), Some("tok"));

        // The query wins over the environment.
        let c = S3Config::from_url(
            "s3://b?endpoint=https://s3.example.com&region=r2&path_style=1&part_size=6000000",
            env(&vars),
        )
        .unwrap();
        assert_eq!(c.endpoint.as_deref(), Some("https://s3.example.com"));
        assert_eq!(c.region, "r2");
        assert!(c.path_style);
        assert_eq!(c.part_size, 6_000_000);

        // AWS_* fallbacks; empty SOURCEGULL_* values are unset.
        let c = S3Config::from_url(
            "s3://b",
            env(&[
                ("SOURCEGULL_S3_ACCESS_KEY_ID", ""),
                ("AWS_ACCESS_KEY_ID", "A"),
                ("AWS_SECRET_ACCESS_KEY", "S"),
                ("AWS_REGION", "us-west-2"),
            ]),
        )
        .unwrap();
        assert_eq!(
            (c.access_key_id.as_str(), c.region.as_str()),
            ("A", "us-west-2")
        );
        // The secret never shows up in logs.
        assert!(!format!("{c:?}").contains("\"S\""));
    }

    #[test]
    fn bad_configs() {
        for url in [
            "s3://",
            "http://b",
            "s3://b/.hidden",
            "s3://b?endpoint=minio:9000",
            "s3://b?path_style=maybe",
            "s3://b?part_size=1024",
            "s3://b?unknown=1",
        ] {
            assert!(S3Config::from_url(url, env(&CREDS)).is_err(), "{url}");
        }
        let err = S3Config::from_url("s3://b", env(&[])).unwrap_err();
        assert!(
            err.to_string().contains("SOURCEGULL_S3_ACCESS_KEY_ID"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn s3_store_against_minio() {
        let prefix = format!("run-{}", uuid::Uuid::new_v4());
        let Some(store) = testing::s3_from_env(&prefix).await else {
            return;
        };
        let config = store.config.clone();
        let store = Arc::new(store);
        crate::tests::exercise(store.clone()).await;

        // Multipart: three parts of the minimum size and a short last one, streamed in
        // uneven chunks.
        let data: Vec<u8> = (0..(3 * MIN_PART_SIZE + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        let chunks: Vec<io::Result<bytes::Bytes>> = data
            .chunks(1_000_003)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        let n = store
            .put("big/blob", futures::stream::iter(chunks).boxed())
            .await
            .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(store.size("big/blob").await.unwrap(), Some(n));
        assert_eq!(crate::get_bytes(&*store, "big/blob").await.unwrap(), data);
        let range = MIN_PART_SIZE as u64 - 10..MIN_PART_SIZE as u64 + 10;
        let got: Vec<bytes::Bytes> = store
            .get_range("big/blob", range.clone())
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            got.concat(),
            &data[range.start as usize..range.end as usize]
        );

        // A multipart upload that fails half way leaves the old blob.
        let failing = futures::stream::iter(
            data.chunks(MIN_PART_SIZE)
                .take(2)
                .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                .chain([Err(io::Error::other("client went away"))])
                .collect::<Vec<_>>(),
        )
        .boxed();
        assert!(store.put("big/blob", failing).await.is_err());
        assert_eq!(store.size("big/blob").await.unwrap(), Some(n));

        // The prefix keeps installations apart.
        let other = S3BlobStore::new(&S3Config {
            prefix: format!("{prefix}-other"),
            ..config
        })
        .unwrap();
        // Creating an existing bucket succeeds.
        other.create_bucket().await.unwrap();
        assert!(other.list("").await.unwrap().is_empty());

        // Clean up.
        for key in store.list("").await.unwrap() {
            store.delete(&key).await.unwrap();
        }
        assert!(store.list("").await.unwrap().is_empty());
    }
}
