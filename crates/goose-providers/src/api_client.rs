use anyhow::Result;
use async_trait::async_trait;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
    Client, Response, StatusCode,
};
#[cfg(feature = "rustls-tls")]
use reqwest::{Certificate, Identity};
use serde_json::Value;
use std::fmt;
#[cfg(feature = "rustls-tls")]
use std::fs::read_to_string;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use url::Host;

pub const DEFAULT_PROVIDER_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;

pub type RequestBuilderDecorator =
    Arc<dyn Fn(reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> + Send + Sync>;

pub struct ApiClient {
    client: Client,
    host: String,
    auth: AuthMethod,
    default_headers: HeaderMap,
    default_query: Vec<(String, String)>,
    timeout: Duration,
    tls_config: Option<TlsConfig>,
    request_builder: Option<RequestBuilderDecorator>,
    transport_policy: TransportPolicy,
}

#[derive(Clone)]
enum TransportPolicy {
    Default,
    HttpsOnly,
    LoopbackHttp,
    SameOrigin(url::Origin),
}

pub enum AuthMethod {
    NoAuth,
    BearerToken(String),
    ApiKey { header_name: String, key: String },
    Custom(Box<dyn AuthProvider>),
}

#[derive(Debug, Clone)]
pub struct TlsCertKeyPair {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub client_identity: Option<TlsCertKeyPair>,
    pub ca_cert_path: Option<PathBuf>,
}

impl TlsConfig {
    pub fn new() -> Self {
        Self {
            client_identity: None,
            ca_cert_path: None,
        }
    }

    pub fn with_client_cert_and_key(mut self, cert_path: PathBuf, key_path: PathBuf) -> Self {
        self.client_identity = Some(TlsCertKeyPair {
            cert_path,
            key_path,
        });
        self
    }

    pub fn with_ca_cert(mut self, path: PathBuf) -> Self {
        self.ca_cert_path = Some(path);
        self
    }

    pub fn is_configured(&self) -> bool {
        self.client_identity.is_some() || self.ca_cert_path.is_some()
    }

    #[cfg(feature = "rustls-tls")]
    fn load_identity(&self) -> Result<Option<Identity>> {
        if let Some(cert_key_pair) = &self.client_identity {
            let cert_pem = read_to_string(&cert_key_pair.cert_path)
                .map_err(|e| anyhow::anyhow!("Failed to read client certificate: {}", e))?;
            let key_pem = read_to_string(&cert_key_pair.key_path)
                .map_err(|e| anyhow::anyhow!("Failed to read client private key: {}", e))?;

            let identity = {
                let combined_pem = format!("{}\n{}", cert_pem, key_pem);
                Identity::from_pem(combined_pem.as_bytes()).map_err(|e| {
                    anyhow::anyhow!("Failed to create identity from cert and key: {}", e)
                })?
            };

            Ok(Some(identity))
        } else {
            Ok(None)
        }
    }

    #[cfg(feature = "rustls-tls")]
    fn load_ca_certificates(&self) -> Result<Vec<Certificate>> {
        match &self.ca_cert_path {
            Some(ca_path) => {
                let ca_pem = read_to_string(ca_path)
                    .map_err(|e| anyhow::anyhow!("Failed to read CA certificate: {}", e))?;

                let certs = Certificate::from_pem_bundle(ca_pem.as_bytes())
                    .map_err(|e| anyhow::anyhow!("Failed to parse CA certificate bundle: {}", e))?;

                Ok(certs)
            }
            None => Ok(Vec::new()),
        }
    }
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
pub trait AuthProvider: Send + Sync {
    async fn get_auth_header(&self) -> Result<(String, String)>;

    async fn refresh_credentials(&self) -> Result<()> {
        anyhow::bail!("credential refresh not supported")
    }
}

pub struct ApiResponse {
    pub status: StatusCode,
    pub payload: Option<Value>,
}

impl fmt::Debug for AuthMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthMethod::NoAuth => f.debug_tuple("NoAuth").finish(),
            AuthMethod::BearerToken(_) => f.debug_tuple("BearerToken").field(&"[hidden]").finish(),
            AuthMethod::ApiKey { header_name, .. } => f
                .debug_struct("ApiKey")
                .field("header_name", header_name)
                .field("key", &"[hidden]")
                .finish(),
            AuthMethod::Custom(_) => f.debug_tuple("Custom").field(&"[provider]").finish(),
        }
    }
}

impl ApiResponse {
    pub async fn from_response(response: Response) -> Result<Self> {
        let status = response.status();
        let payload = response.json().await.ok();
        Ok(Self { status, payload })
    }
}

pub struct ApiRequestBuilder<'a> {
    client: &'a ApiClient,
    path: &'a str,
    headers: HeaderMap,
    streaming: bool,
}

impl ApiClient {
    pub fn new_with_tls(
        host: String,
        auth: AuthMethod,
        tls_config: Option<TlsConfig>,
    ) -> Result<Self> {
        Self::with_timeout_and_tls(
            host,
            auth,
            Duration::from_secs(DEFAULT_PROVIDER_TIMEOUT_SECS),
            tls_config,
        )
    }

    pub fn with_timeout_and_tls(
        host: String,
        auth: AuthMethod,
        timeout: Duration,
        tls_config: Option<TlsConfig>,
    ) -> Result<Self> {
        let mut client_builder = Self::client_builder(timeout);

        if let Some(ref config) = tls_config {
            client_builder = Self::configure_tls(client_builder, config)?;
        }

        let client = client_builder.build()?;

        Ok(Self {
            client,
            host,
            auth,
            default_headers: HeaderMap::new(),
            default_query: Vec::new(),
            timeout,
            tls_config,
            request_builder: None,
            transport_policy: TransportPolicy::Default,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn client_builder(timeout: Duration) -> reqwest::ClientBuilder {
        Client::builder()
            .connect_timeout(Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS))
            .read_timeout(timeout)
    }

    fn rebuild_client(&mut self) -> Result<()> {
        let mut client_builder =
            Self::client_builder(self.timeout).default_headers(self.default_headers.clone());
        client_builder = Self::configure_transport(client_builder, &self.transport_policy);

        // Configure TLS if needed
        if let Some(ref tls_config) = self.tls_config {
            client_builder = Self::configure_tls(client_builder, tls_config)?;
        }

        self.client = client_builder.build()?;
        Ok(())
    }

    fn configure_transport(
        client_builder: reqwest::ClientBuilder,
        transport_policy: &TransportPolicy,
    ) -> reqwest::ClientBuilder {
        match transport_policy {
            TransportPolicy::Default => client_builder,
            TransportPolicy::HttpsOnly => client_builder.https_only(true),
            TransportPolicy::LoopbackHttp => {
                client_builder
                    .no_proxy()
                    .redirect(Policy::custom(|attempt| {
                        if attempt.previous().len() > 10 {
                            return attempt.error("too many redirects");
                        }
                        let url = attempt.url();
                        let is_loopback_http = url.scheme() == "http"
                            && match url.host() {
                                Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
                                Some(Host::Ipv4(address)) => address.is_loopback(),
                                Some(Host::Ipv6(address)) => address.is_loopback(),
                                None => false,
                            };
                        if url.scheme() == "https" || is_loopback_http {
                            attempt.follow()
                        } else {
                            attempt.error("redirect violates the loopback transport policy")
                        }
                    }))
            }
            TransportPolicy::SameOrigin(origin) => {
                let origin = origin.clone();
                client_builder.redirect(Policy::custom(move |attempt| {
                    if attempt.previous().len() >= 10 {
                        return attempt.error("too many redirects");
                    }
                    if attempt.url().origin() == origin {
                        attempt.follow()
                    } else {
                        attempt.error("redirect crosses the authenticated request origin")
                    }
                }))
            }
        }
    }

    /// Configure TLS settings on a reqwest ClientBuilder
    #[cfg(feature = "rustls-tls")]
    fn configure_tls(
        mut client_builder: reqwest::ClientBuilder,
        tls_config: &TlsConfig,
    ) -> Result<reqwest::ClientBuilder> {
        if tls_config.is_configured() {
            // Load client identity (certificate + private key)
            if let Some(identity) = tls_config.load_identity()? {
                client_builder = client_builder.identity(identity);
            }

            // Load CA certificates
            let ca_certs = tls_config.load_ca_certificates()?;
            for ca_cert in ca_certs {
                client_builder = client_builder.add_root_certificate(ca_cert);
            }
        }
        Ok(client_builder)
    }

    /// Reject custom TLS settings when ghosty is compiled without a TLS backend.
    #[cfg(not(feature = "rustls-tls"))]
    fn configure_tls(
        client_builder: reqwest::ClientBuilder,
        tls_config: &TlsConfig,
    ) -> Result<reqwest::ClientBuilder> {
        if tls_config.is_configured() {
            return Err(anyhow::anyhow!(
                "Custom TLS configuration requires the `rustls-tls` feature"
            ));
        }
        Ok(client_builder)
    }

    pub fn with_headers(mut self, headers: HeaderMap) -> Result<Self> {
        self.default_headers = headers;
        self.rebuild_client()?;
        Ok(self)
    }

    pub fn with_query(mut self, params: Vec<(String, String)>) -> Self {
        self.default_query = params;
        self
    }

    pub fn with_header(mut self, key: &str, value: &str) -> Result<Self> {
        let header_name = HeaderName::from_bytes(key.as_bytes())?;
        let header_value = HeaderValue::from_str(value)?;
        self.default_headers.insert(header_name, header_value);
        self.rebuild_client()?;
        Ok(self)
    }

    pub fn with_request_builder(mut self, request_builder: RequestBuilderDecorator) -> Self {
        self.request_builder = Some(request_builder);
        self
    }

    pub fn with_https_only(mut self) -> Result<Self> {
        self.transport_policy = TransportPolicy::HttpsOnly;
        self.rebuild_client()?;
        Ok(self)
    }

    pub fn with_loopback_http_only(mut self) -> Result<Self> {
        self.transport_policy = TransportPolicy::LoopbackHttp;
        self.rebuild_client()?;
        Ok(self)
    }

    pub fn with_same_origin_redirects(mut self) -> Result<Self> {
        let origin = url::Url::parse(&self.host)
            .map_err(|error| anyhow::anyhow!("Invalid base URL: {}", error))?
            .origin();
        self.transport_policy = TransportPolicy::SameOrigin(origin);
        self.rebuild_client()?;
        Ok(self)
    }

    pub fn request<'a>(&'a self, path: &'a str) -> ApiRequestBuilder<'a> {
        ApiRequestBuilder {
            client: self,
            path,
            headers: HeaderMap::new(),
            streaming: false,
        }
    }

    pub async fn refresh_credentials(&self) -> Result<()> {
        match &self.auth {
            AuthMethod::Custom(provider) => provider.refresh_credentials().await,
            _ => anyhow::bail!("credential refresh not supported"),
        }
    }

    pub async fn api_post(&self, path: &str, payload: &Value) -> Result<ApiResponse> {
        self.request(path).api_post(payload).await
    }

    pub async fn response_post(&self, path: &str, payload: &Value) -> Result<Response> {
        self.request(path).response_post(payload).await
    }

    pub async fn api_get(&self, path: &str) -> Result<ApiResponse> {
        self.request(path).api_get().await
    }

    pub async fn response_get(&self, path: &str) -> Result<Response> {
        self.request(path).response_get().await
    }

    fn build_url(&self, path: &str) -> Result<url::Url> {
        use url::Url;
        let mut base_url =
            Url::parse(&self.host).map_err(|e| anyhow::anyhow!("Invalid base URL: {}", e))?;

        let base_path = base_url.path();
        if !base_path.is_empty() && base_path != "/" && !base_path.ends_with('/') {
            base_url.set_path(&format!("{}/", base_path));
        }

        let mut url = base_url
            .join(path)
            .map_err(|e| anyhow::anyhow!("Failed to construct URL: {}", e))?;

        for (key, value) in &self.default_query {
            url.query_pairs_mut().append_pair(key, value);
        }

        Ok(url)
    }
}

impl<'a> ApiRequestBuilder<'a> {
    pub fn header(mut self, key: &str, value: &str) -> Result<Self> {
        let header_name = HeaderName::from_bytes(key.as_bytes())?;
        let header_value = HeaderValue::from_str(value)?;
        self.headers.insert(header_name, header_value);
        Ok(self)
    }

    #[allow(dead_code)]
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.headers.extend(headers);
        self
    }

    /// Apply per-request headers from a model config, overriding any static
    /// client headers on key collision.
    pub fn model_headers(self, model_config: &crate::model::ModelConfig) -> Result<Self> {
        match &model_config.request_headers {
            Some(headers) => headers
                .iter()
                .try_fold(self, |builder, (key, value)| builder.header(key, value)),
            None => Ok(self),
        }
    }

    pub fn streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    pub async fn api_post(self, payload: &Value) -> Result<ApiResponse> {
        let response = self.response_post(payload).await?;
        ApiResponse::from_response(response).await
    }

    async fn send_bounded(&self, request: reqwest::RequestBuilder) -> Result<Response> {
        if self.streaming {
            Ok(crate::http_status::send_bounded(request, self.client.timeout).await?)
        } else {
            Ok(request.send().await?)
        }
    }

    pub async fn response_post(self, payload: &Value) -> Result<Response> {
        let request = self.send_request(|url, client| client.post(url)).await?;
        self.send_bounded(request.json(payload)).await
    }

    pub async fn multipart_post(self, form: reqwest::multipart::Form) -> Result<Response> {
        let request = self.send_request(|url, client| client.post(url)).await?;
        Ok(request.multipart(form).send().await?)
    }

    pub async fn api_get(self) -> Result<ApiResponse> {
        let response = self.response_get().await?;
        ApiResponse::from_response(response).await
    }

    pub async fn response_get(self) -> Result<Response> {
        let request = self.send_request(|url, client| client.get(url)).await?;
        self.send_bounded(request).await
    }

    async fn send_request<F>(&self, request_builder: F) -> Result<reqwest::RequestBuilder>
    where
        F: FnOnce(url::Url, &Client) -> reqwest::RequestBuilder,
    {
        let url = self.client.build_url(self.path)?;
        let headers = self.headers.clone();
        let mut request = request_builder(url, &self.client.client);
        request = request.headers(headers);

        if !self.streaming {
            request = request.timeout(self.client.timeout);
        }

        if let Some(decorator) = &self.client.request_builder {
            request = decorator(request)?;
        }

        request = match &self.client.auth {
            AuthMethod::NoAuth => request,
            AuthMethod::BearerToken(token) => {
                request.header("Authorization", format!("Bearer {}", token))
            }
            AuthMethod::ApiKey { header_name, key } => request.header(header_name.as_str(), key),
            AuthMethod::Custom(provider) => {
                let (header_name, header_value) = provider.get_auth_header().await?;
                request.header(header_name, header_value)
            }
        };

        Ok(request)
    }
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("host", &self.host)
            .field("auth", &"[auth method]")
            .field("timeout", &self.timeout)
            .field("default_headers", &self.default_headers)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_chunked_server(gap_ms: u64, chunks: usize) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    if sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\n\
                              content-type: text/event-stream\r\n\
                              transfer-encoding: chunked\r\n\r\n",
                        )
                        .await
                        .is_err()
                    {
                        return;
                    }
                    for i in 0..chunks {
                        if i > 0 {
                            tokio::time::sleep(Duration::from_millis(gap_ms)).await;
                        }
                        let data = format!("data: {}\n\n", i);
                        let chunk = format!("{:x}\r\n{}\r\n", data.len(), data);
                        if sock.write_all(chunk.as_bytes()).await.is_err() {
                            return;
                        }
                        let _ = sock.flush().await;
                    }
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                });
            }
        });
        addr
    }

    fn client_with_timeout(addr: SocketAddr, timeout_ms: u64) -> ApiClient {
        let mut client = ApiClient::with_timeout_and_tls(
            format!("http://{}", addr),
            AuthMethod::NoAuth,
            Duration::from_millis(timeout_ms),
            None,
        )
        .unwrap();
        client.client = Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS))
            .read_timeout(client.timeout)
            .build()
            .unwrap();
        client
    }

    async fn drain_counting_data_lines(mut response: Response) -> Result<usize, reqwest::Error> {
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&body).matches("data:").count())
    }

    #[tokio::test]
    async fn https_only_rejects_http_after_client_rebuild() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = ApiClient::new_with_tls(
            format!("http://{addr}"),
            AuthMethod::BearerToken("secret".to_string()),
            None,
        )
        .unwrap()
        .with_https_only()
        .unwrap()
        .with_header("x-test", "value")
        .unwrap();

        assert!(client.response_get("models").await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn loopback_transport_rejects_remote_http_redirect() {
        for status in ["307 Temporary Redirect", "308 Permanent Redirect"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\nLocation: http://192.0.2.1/capture\r\nContent-Length: 0\r\n\r\n"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let client = ApiClient::new_with_tls(
                format!("http://{addr}"),
                AuthMethod::BearerToken("secret".to_string()),
                None,
            )
            .unwrap()
            .with_loopback_http_only()
            .unwrap()
            .with_header("x-test", "value")
            .unwrap();

            let error = client
                .response_post("chat", &serde_json::json!({ "secret": "prompt" }))
                .await
                .unwrap_err();

            assert!(
                format!("{error:#}").contains("redirect violates the loopback transport policy"),
                "unexpected redirect error: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn streaming_request_survives_beyond_total_timeout() {
        let addr = spawn_chunked_server(50, 12).await;
        let client = client_with_timeout(addr, 400);

        let response = client
            .request("v1/messages")
            .streaming(true)
            .response_post(&serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let count = drain_counting_data_lines(response).await.unwrap();
        assert_eq!(count, 12);
    }

    #[tokio::test]
    async fn streaming_request_fails_when_stream_stalls() {
        let addr = spawn_chunked_server(5_000, 2).await;
        let client = client_with_timeout(addr, 400);

        let response = client
            .request("v1/messages")
            .streaming(true)
            .response_post(&serde_json::json!({}))
            .await
            .unwrap();

        let err = drain_counting_data_lines(response)
            .await
            .expect_err("stalled stream should time out, not complete");
        assert!(err.is_timeout(), "expected a timeout error, got: {err}");
    }

    #[tokio::test]
    async fn non_streaming_request_enforces_total_deadline() {
        let addr = spawn_chunked_server(50, 12).await;
        let client = client_with_timeout(addr, 400);

        let response = client
            .request("v1/messages")
            .response_post(&serde_json::json!({}))
            .await
            .unwrap();

        let err = drain_counting_data_lines(response)
            .await
            .expect_err("total deadline should cut off the response body");
        assert!(err.is_timeout(), "expected a timeout error, got: {err}");
    }

    #[tokio::test]
    async fn streaming_request_times_out_before_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    while sock.read(&mut buf).await.is_ok_and(|n| n > 0) {}
                });
            }
        });
        let client = client_with_timeout(addr, 400);

        let started = std::time::Instant::now();
        let err = client
            .request("v1/messages")
            .streaming(true)
            .response_post(&serde_json::json!({}))
            .await
            .expect_err("the phase before the response body must stay bounded");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "should fail near the configured timeout, took {:?}",
            started.elapsed()
        );
        assert!(matches!(
            crate::errors::ProviderError::from(err),
            crate::errors::ProviderError::NetworkError(message)
                if message.starts_with("Request timed out")
        ));
    }

    #[tokio::test]
    async fn streaming_error_body_shares_send_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 1\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = socket.write_all(b"x").await;
        });

        let client = client_with_timeout(addr, 400);
        let started = std::time::Instant::now();
        let response = client
            .request("v1/messages")
            .streaming(true)
            .response_post(&serde_json::json!({}))
            .await
            .unwrap();
        crate::http_status::handle_status(response)
            .await
            .unwrap_err();

        assert!(
            started.elapsed() < Duration::from_millis(550),
            "send and error body used separate deadlines: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn test_model_headers_applied_and_override_static_headers() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let client = ApiClient::new_with_tls(
                "http://localhost:8080".to_string(),
                AuthMethod::NoAuth,
                None,
            )
            .unwrap()
            .with_header("x-static", "static-value")
            .unwrap()
            .with_header("queue_threshold", "1000")
            .unwrap();

            let model_config = crate::model::ModelConfig::new("test-model").with_request_headers(
                Some(std::collections::HashMap::from([
                    ("queue_threshold".to_string(), "500".to_string()),
                    ("Idempotency-Key".to_string(), "abc-123".to_string()),
                ])),
            );

            let request = client
                .request("/test")
                .model_headers(&model_config)
                .unwrap()
                .send_request(|url, client| client.get(url))
                .await
                .unwrap();

            let headers = request.build().unwrap().headers().clone();
            let get = |name: &str| {
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            };
            assert_eq!(get("queue_threshold"), Some("500".to_string()));
            assert_eq!(get("Idempotency-Key"), Some("abc-123".to_string()));
        });
    }

    #[test]
    fn test_model_headers_rejects_invalid_header_name() {
        let client = ApiClient::new_with_tls(
            "http://localhost:8080".to_string(),
            AuthMethod::NoAuth,
            None,
        )
        .unwrap();

        let model_config = crate::model::ModelConfig::new("test-model").with_request_headers(Some(
            std::collections::HashMap::from([("bad header name".to_string(), "value".to_string())]),
        ));

        assert!(client
            .request("/test")
            .model_headers(&model_config)
            .is_err());
    }

    #[test]
    fn test_request_builder_decorator() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let client = ApiClient::new_with_tls(
                "http://localhost:8080".to_string(),
                AuthMethod::BearerToken("test-token".to_string()),
                None,
            )
            .unwrap()
            .with_request_builder(Arc::new(|request| {
                Ok(request.header("test-my-session-id", "test-session_id-456"))
            }));

            let request = client
                .request("/test")
                .send_request(|url, client| client.get(url))
                .await
                .unwrap();

            let headers = request.build().unwrap().headers().clone();
            let actual = headers
                .get("test-my-session-id")
                .and_then(|value| value.to_str().ok());
            assert_eq!(actual, Some("test-session_id-456"));
        });
    }
}
