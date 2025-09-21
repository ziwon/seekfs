use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::{debug, warn};
use futures::{Stream, StreamExt};
use std::pin::Pin;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("client error: {0}")]
    Client(String),
    #[error("get error: {0}")]
    Get(String),
    #[error("head error: {0}")]
    Head(String),
    #[error("budget exceeded: {0}")]
    Budget(String),
    #[error("retry exhausted: {0}")]
    Retry(String),
    #[error("not implemented")]
    NotImplemented,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeadInfo {
    pub size: Option<u64>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    pub engine: Option<String>,          // sdk | crt (default sdk)
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub get_budget_per_sec: u32,
    pub range_coalesce_max_mib: u32,
    pub max_inflight_gets: u32,
    pub retry_max_attempts: u32,
    pub retry_initial_backoff_ms: u64,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            engine: Some("sdk".to_string()),
            endpoint: None,
            region: "us-east-1".to_string(),
            bucket: "seekfs".to_string(),
            access_key_id: None,
            secret_access_key: None,
            get_budget_per_sec: 200,
            range_coalesce_max_mib: 32,
            max_inflight_gets: 128,
            retry_max_attempts: 3,
            retry_initial_backoff_ms: 100,
        }
    }
}

#[async_trait::async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    async fn head(&self, key: &str) -> Result<Option<HeadInfo>, StoreError>;
    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, StoreError>;
    async fn get_range_stream(&self, key: &str, range: Range<u64>) -> Result<Pin<Box<dyn Stream<Item=Result<Bytes, StoreError>> + Send>>, StoreError>;
    fn name(&self) -> &'static str;
}

// SDK-based implementation
#[cfg(feature = "sdk")] 
pub mod sdk_impl {
    use super::*;
    use aws_config::BehaviorVersion;
    use aws_sdk_s3::config::{Credentials, Region, SharedCredentialsProvider};
    use aws_sdk_s3::{Client, Config};
    use backoff::future::retry;
    use backoff::ExponentialBackoff;

    pub struct SdkInner {
        client: Client,
        cfg: S3Config,
        inflight: Arc<Semaphore>,
        budget: Arc<Semaphore>,
        _budget_refill: tokio::task::JoinHandle<()>,
    }

    #[derive(Clone)]
    pub struct SdkStore(pub Arc<SdkInner>);

    impl SdkStore {
        pub async fn new(cfg: S3Config) -> Result<Self, StoreError> {
            let mut b = Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new(cfg.region.clone()));

            if let Some(endpoint) = &cfg.endpoint {
                b = b.endpoint_url(endpoint).force_path_style(true);
            }
            if let (Some(ak), Some(sk)) = (&cfg.access_key_id, &cfg.secret_access_key) {
                let creds = Credentials::new(ak, sk, None, None, "seekfs");
                b = b.credentials_provider(SharedCredentialsProvider::new(creds));
            }

            let client = Client::from_conf(b.build());

            let inflight = Arc::new(Semaphore::new(cfg.max_inflight_gets as usize));
            let budget = Arc::new(Semaphore::new(cfg.get_budget_per_sec as usize));
            let budget_clone = budget.clone();
            let rate = cfg.get_budget_per_sec;
            let _budget_refill = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                loop {
                    interval.tick().await;
                    let avail = budget_clone.available_permits();
                    let target = rate as usize;
                    if avail < target {
                        budget_clone.add_permits(target - avail);
                    }
                }
            });

            Ok(SdkStore(Arc::new(SdkInner { client, cfg, inflight, budget, _budget_refill })))
        }

        fn coalesce_ranges(&self, mut ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
            if ranges.is_empty() { return vec![]; }
            ranges.sort_by_key(|r| r.start);
            let max_bytes = (self.0.cfg.range_coalesce_max_mib as u64) * 1024 * 1024;
            let mut out = Vec::new();
            let mut cur = ranges[0].clone();
            let original_count = ranges.len();
            for r in ranges.into_iter().skip(1) {
                let gap = r.start.saturating_sub(cur.end);
                let combined = r.end - cur.start;
                if gap <= 65536 && combined <= max_bytes {
                    cur.end = r.end.max(cur.end);
                } else {
                    out.push(cur);
                    cur = r;
                }
            }
            out.push(cur);
            debug!(original_count, coalesced_count = out.len(), "Range coalescing");
            out
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for SdkStore {
        async fn head(&self, key: &str) -> Result<Option<HeadInfo>, StoreError> {
            let res = self.0.client.head_object().bucket(&self.0.cfg.bucket).key(key).send().await;
            match res {
                Ok(o) => Ok(Some(HeadInfo { size: o.content_length().map(|v| v as u64), etag: o.e_tag().map(|s| s.to_string()) })),
                Err(e) => {
                    let s = e.to_string();
                    if s.contains("NotFound") || s.contains("404") { Ok(None) } else { Err(StoreError::Head(s)) }
                }
            }
        }

        async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, StoreError> {
            let _b = self.0.budget.acquire().await.map_err(|e| StoreError::Budget(e.to_string()))?;
            let _i = self.0.inflight.acquire().await.map_err(|e| StoreError::Client(e.to_string()))?;

            let range_header = format!("bytes={}-{}", range.start, range.end.saturating_sub(1));
            let eb = ExponentialBackoff {
                initial_interval: Duration::from_millis(self.0.cfg.retry_initial_backoff_ms),
                max_elapsed_time: Some(Duration::from_secs(30)),
                ..Default::default()
            };

            let op = || async {
                let start = Instant::now();
                let res = self.0.client.get_object()
                    .bucket(&self.0.cfg.bucket)
                    .key(key)
                    .range(&range_header)
                    .send().await;
                match res {
                    Ok(o) => {
                        let body = o.body.collect().await.map_err(|e| backoff::Error::transient(StoreError::Get(e.to_string())))?;
                        debug!(key, range_start = range.start, range_end = range.end, latency_ms = start.elapsed().as_millis(), "S3 GET ok");
                        Ok(body.into_bytes())
                    }
                    Err(e) => {
                        let s = e.to_string();
                        warn!(key, range_start = range.start, range_end = range.end, error = %s, "S3 GET failed");
                        if s.contains("throttl") || s.contains("timeout") || s.contains("500") || s.contains("503") {
                            Err(backoff::Error::transient(StoreError::Get(s)))
                        } else {
                            Err(backoff::Error::permanent(StoreError::Get(s)))
                        }
                    }
                }
            };

            retry(eb, op).await.map_err(|e| StoreError::Retry(e.to_string()))
        }

        async fn get_range_stream(&self, key: &str, range: Range<u64>) -> Result<Pin<Box<dyn Stream<Item=Result<Bytes, StoreError>> + Send>>, StoreError> {
            let budget = self.0.budget.clone();
            let inflight = self.0.inflight.clone();
            let _b = budget.acquire_owned().await.map_err(|e| StoreError::Budget(e.to_string()))?;
            let _i = inflight.acquire_owned().await.map_err(|e| StoreError::Client(e.to_string()))?;
            let range_header = format!("bytes={}-{}", range.start, range.end.saturating_sub(1));
            let res = self.0.client.get_object().bucket(&self.0.cfg.bucket).key(key).range(&range_header).send().await.map_err(|e| StoreError::Get(e.to_string()))?;
            let reader = res.body.into_async_read();
            use tokio::io::{AsyncRead, ReadBuf};
            struct ReaderStream<R> { reader: R, buf: Vec<u8> }
            impl<R: AsyncRead + Unpin + Send> Stream for ReaderStream<R> {
                type Item = Result<Bytes, StoreError>;
                fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
                    let me = self.get_mut();
                    if me.buf.len() < 64 * 1024 { me.buf.resize(64 * 1024, 0); }
                    let mut rb = ReadBuf::new(&mut me.buf);
                    match Pin::new(&mut me.reader).poll_read(cx, &mut rb) {
                        std::task::Poll::Ready(Ok(())) => {
                            let n = rb.filled().len();
                            if n == 0 {
                                std::task::Poll::Ready(None)
                            } else {
                                let out = Bytes::copy_from_slice(&rb.filled()[..n]);
                                std::task::Poll::Ready(Some(Ok(out)))
                            }
                        }
                        std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Some(Err(StoreError::Get(e.to_string())))),
                        std::task::Poll::Pending => std::task::Poll::Pending,
                    }
                }
            }
            struct PermitStream<S> { inner: S, _b: tokio::sync::OwnedSemaphorePermit, _i: tokio::sync::OwnedSemaphorePermit }
            impl<S: Stream<Item=Result<Bytes, StoreError>> + Unpin> Stream for PermitStream<S> {
                type Item = Result<Bytes, StoreError>;
                fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
                    let this = self.get_mut();
                    Pin::new(&mut this.inner).poll_next(cx)
                }
            }
            let rs = ReaderStream { reader, buf: Vec::with_capacity(64 * 1024) };
            Ok(Box::pin(PermitStream { inner: rs, _b, _i }))
        }

        fn name(&self) -> &'static str { "sdk" }
    }
}

// Re-export SdkStore at top-level for convenience
#[cfg(feature = "sdk")]
pub use sdk_impl::SdkStore;

// Placeholder CRT-based implementation (not wired to aws-crt yet)
#[cfg(feature = "crt")]
pub mod crt_impl {
    use super::*;
    use mountpoint_s3_client as mpsc;
    use mpsc::types::{GetObjectParams, HeadObjectParams};
    use mpsc::{ObjectClient};
    use tokio::sync::Mutex;

    struct CrtInner {
        client: mpsc::S3CrtClient,
        cfg: S3Config,
        inflight: Arc<Semaphore>,
        budget: Arc<Semaphore>,
    }

    #[derive(Clone)]
    pub struct CrtStore(Arc<CrtInner>);

    impl CrtStore {
        pub async fn new(cfg: S3Config) -> Result<Self, StoreError> {
            use mpsc::config::{AddressingStyle, Allocator, EndpointConfig, S3ClientAuthConfig, S3ClientConfig, Uri, CredentialsProvider, CredentialsProviderStaticOptions};
            let mut ep = EndpointConfig::new(&cfg.region).addressing_style(AddressingStyle::Path);
            if let Some(endpoint) = &cfg.endpoint {
                let uri = Uri::new_from_str(&Allocator::default(), endpoint)
                    .map_err(|e| StoreError::Client(format!("invalid endpoint: {e}")))?;
                ep = ep.endpoint(uri);
            }

            let auth = if let (Some(ak), Some(sk)) = (&cfg.access_key_id, &cfg.secret_access_key) {
                let opts = CredentialsProviderStaticOptions { access_key_id: ak, secret_access_key: sk, session_token: None };
                let provider = CredentialsProvider::new_static(&Allocator::default(), opts)
                    .map_err(|e| StoreError::Client(format!("creds: {e}")))?;
                S3ClientAuthConfig::Provider(provider)
            } else {
                S3ClientAuthConfig::Default
            };

            let conf = S3ClientConfig::new()
                .endpoint_config(ep)
                .auth_config(auth);
            let client = mpsc::S3CrtClient::new(conf).map_err(|e| StoreError::Client(format!("s3client: {e}")))?;

            let inner = CrtInner { client, cfg: cfg.clone(), inflight: Arc::new(Semaphore::new(cfg.max_inflight_gets as usize)), budget: Arc::new(Semaphore::new(cfg.get_budget_per_sec as usize)) };

            // refill bucket
            let budget = inner.budget.clone();
            let rate = inner.cfg.get_budget_per_sec;
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                loop { interval.tick().await; let avail = budget.available_permits(); if avail < rate as usize { budget.add_permits(rate as usize - avail); } }
            });

            Ok(CrtStore(Arc::new(inner)))
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for CrtStore {
        async fn head(&self, key: &str) -> Result<Option<HeadInfo>, StoreError> {
            use mpsc::error::{ObjectClientError, HeadObjectError};
            let _b = self.0.budget.acquire().await.map_err(|e| StoreError::Budget(e.to_string()))?;
            let _i = self.0.inflight.acquire().await.map_err(|e| StoreError::Client(e.to_string()))?;
            let params = HeadObjectParams::new();
            match self.0.client.head_object(&self.0.cfg.bucket, key, &params).await {
                Ok(resp) => Ok(Some(HeadInfo { size: Some(resp.size), etag: Some(resp.etag.as_str().to_string()) })),
                Err(ObjectClientError::ServiceError(HeadObjectError::NotFound)) => Ok(None),
                Err(e) => Err(StoreError::Head(e.to_string())),
            }
        }

        async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, StoreError> {
            let _b = self.0.budget.acquire().await.map_err(|e| StoreError::Budget(e.to_string()))?;
            let _i = self.0.inflight.acquire().await.map_err(|e| StoreError::Client(e.to_string()))?;
            let params = GetObjectParams::new().range(Some(range.clone()));
            let mut stream = self.0.client.get_object(&self.0.cfg.bucket, key, &params)
                .await
                .map_err(|e| StoreError::Get(e.to_string()))?;
            let mut buf = Vec::with_capacity((range.end - range.start) as usize);
            while let Some(next) = StreamExt::next(&mut stream).await {
                let part = next.map_err(|e| StoreError::Get(e.to_string()))?;
                buf.extend_from_slice(&part.data);
            }
            Ok(Bytes::from(buf))
        }

        async fn get_range_stream(&self, key: &str, range: Range<u64>) -> Result<Pin<Box<dyn Stream<Item=Result<Bytes, StoreError>> + Send>>, StoreError> {
            let budget = self.0.budget.clone();
            let inflight = self.0.inflight.clone();
            let _b = budget.acquire_owned().await.map_err(|e| StoreError::Budget(e.to_string()))?;
            let _i = inflight.acquire_owned().await.map_err(|e| StoreError::Client(e.to_string()))?;
            let params = GetObjectParams::new().range(Some(range));
            let stream = self.0.client.get_object(&self.0.cfg.bucket, key, &params)
                .await
                .map_err(|e| StoreError::Get(e.to_string()))?
                .map(|res| res
                    .map(|part| Bytes::from(part.data))
                    .map_err(|e| StoreError::Get(e.to_string()))
                );
            struct PermitStream<S> { inner: S, _b: tokio::sync::OwnedSemaphorePermit, _i: tokio::sync::OwnedSemaphorePermit }
            impl<S: Stream<Item=Result<Bytes, StoreError>> + Unpin> Stream for PermitStream<S> {
                type Item = Result<Bytes, StoreError>;
                fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
                    let this = self.get_mut();
                    Pin::new(&mut this.inner).poll_next(cx)
                }
            }
            Ok(Box::pin(PermitStream { inner: stream, _b, _i }))
        }

        fn name(&self) -> &'static str { "crt" }
    }
}
