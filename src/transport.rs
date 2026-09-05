use crate::{
    config::{Config, Platform},
    error::{Error, Result},
};
use async_trait::async_trait;
use bytes::Bytes;
use gitlab::api::{ApiError, AsyncClient, AsyncQuery, Endpoint, RestClient};
use http::{Method, Request, Response};
use http_body_util::BodyExt;
use serde_json::Value;
use std::{borrow::Cow, time::Duration};
use url::Url;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn request(&self, method: Method, endpoint: &str, body: Option<Value>) -> Result<Value>;
}

pub struct SdkTransport {
    backend: Backend,
    timeout: Duration,
}
enum Backend {
    Github(Box<octocrab::Octocrab>),
    Gitlab(GitlabHttp),
}

impl SdkTransport {
    pub fn new(config: &Config, platform: Platform) -> Result<Self> {
        let timeout = Duration::from_secs(config.timeout_seconds);
        let backend =
            match platform {
                Platform::Github => {
                    let token = config.github_token.as_ref().ok_or_else(|| {
                        Error::new("configuration", "缺少 ISSUEFLOW_GITHUB_TOKEN")
                    })?;
                    let client = octocrab::Octocrab::builder()
                        .personal_token(token.expose().to_string())
                        .base_uri(config.github_api_url.as_str())
                        .map_err(|_| Error::new("configuration", "GitHub API 地址无效"))?
                        .build()
                        .map_err(|_| Error::new("configuration", "无法初始化 GitHub SDK"))?;
                    Backend::Github(Box::new(client))
                }
                Platform::Gitlab => {
                    let token = config.gitlab_token.as_ref().ok_or_else(|| {
                        Error::new("configuration", "缺少 ISSUEFLOW_GITLAB_TOKEN")
                    })?;
                    let base = config
                        .gitlab_url
                        .as_ref()
                        .ok_or_else(|| Error::new("configuration", "缺少 ISSUEFLOW_GITLAB_URL"))?
                        .join("api/v4/")
                        .map_err(|_| Error::new("configuration", "GitLab 地址无效"))?;
                    let graphql = config
                        .gitlab_url
                        .as_ref()
                        .ok_or_else(|| Error::new("configuration", "缺少 ISSUEFLOW_GITLAB_URL"))?
                        .join("api/graphql")
                        .map_err(|_| Error::new("configuration", "GitLab GraphQL 地址无效"))?;
                    let client = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .timeout(timeout)
                        .user_agent(concat!("issueflow/", env!("CARGO_PKG_VERSION")))
                        .build()
                        .map_err(|_| Error::new("configuration", "无法初始化 HTTP 客户端"))?;
                    Backend::Gitlab(GitlabHttp {
                        base,
                        graphql,
                        client,
                        token: token.expose().to_string(),
                    })
                }
            };
        Ok(Self { backend, timeout })
    }
}

#[async_trait]
impl Transport for SdkTransport {
    async fn request(&self, method: Method, endpoint: &str, body: Option<Value>) -> Result<Value> {
        if endpoint.starts_with('/') || endpoint.contains("://") || endpoint.contains('#') {
            return Err(Error::new("input", "只允许内部生成的相对 API 路径"));
        }
        let write = method != Method::GET;
        let operation = async {
            let bytes = match &self.backend {
                Backend::Github(client) => {
                    // Relative URI lets the SDK attach credentials only to its configured API base.
                    let request = client
                        .build_request(
                            Request::builder()
                                .method(method)
                                .uri(format!("/{endpoint}")),
                            body.as_ref(),
                        )
                        .map_err(|_| Error::new("input", "无法构造 GitHub 请求"))?;
                    let response = client
                        .execute(request)
                        .await
                        .map_err(|_| Error::network(write))?;
                    let status = response.status();
                    if !status.is_success() {
                        return Err(Error::http(status.as_u16(), write));
                    }
                    response
                        .into_body()
                        .collect()
                        .await
                        .map_err(|_| Error::network(write))?
                        .to_bytes()
                        .to_vec()
                }
                Backend::Gitlab(client) => {
                    let endpoint = JsonEndpoint {
                        method,
                        path: endpoint.into(),
                        body,
                    };
                    gitlab::api::raw(endpoint)
                        .query_async(client)
                        .await
                        .map_err(|error| match error {
                            ApiError::GitlabWithStatus { status, .. }
                            | ApiError::GitlabObjectWithStatus { status, .. }
                            | ApiError::GitlabUnrecognizedWithStatus { status, .. }
                            | ApiError::GitlabService { status, .. } => {
                                Error::http(status.as_u16(), write)
                            }
                            _ => Error::network(write),
                        })?
                }
            };
            if bytes.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_slice(&bytes).map_err(|_| Error {
                outcome_unknown: write,
                ..Error::new("response", "API 返回了无法解析的 JSON")
            })
        };
        tokio::time::timeout(self.timeout, operation)
            .await
            .map_err(|_| Error::network(write))?
    }
}

struct JsonEndpoint {
    method: Method,
    path: String,
    body: Option<Value>,
}
impl Endpoint for JsonEndpoint {
    fn method(&self) -> Method {
        self.method.clone()
    }
    fn endpoint(&self) -> Cow<'static, str> {
        self.path.clone().into()
    }
    fn body(&self) -> std::result::Result<Option<(&'static str, Vec<u8>)>, gitlab::api::BodyError> {
        Ok(self
            .body
            .as_ref()
            .map(|body| ("application/json", body.to_string().into_bytes())))
    }
}

// GitLab's SDK supports custom clients. Keep its endpoint/query layer while disabling
// redirects and supporting an instance base path, without an implicit auth probe.
struct GitlabHttp {
    base: Url,
    graphql: Url,
    client: reqwest::Client,
    token: String,
}
impl RestClient for GitlabHttp {
    type Error = std::io::Error;
    fn rest_endpoint(&self, endpoint: &str) -> std::result::Result<Url, ApiError<Self::Error>> {
        if endpoint == "graphql" {
            Ok(self.graphql.clone())
        } else {
            Ok(self.base.join(endpoint)?)
        }
    }
}
#[async_trait]
impl AsyncClient for GitlabHttp {
    async fn rest_async(
        &self,
        request: http::request::Builder,
        body: Vec<u8>,
    ) -> std::result::Result<Response<Bytes>, ApiError<Self::Error>> {
        let fail = || ApiError::client(std::io::Error::other("HTTP transport failed"));
        let request = request.body(body).map_err(|_| fail())?;
        let url = Url::parse(&request.uri().to_string()).map_err(|_| fail())?;
        if url.origin() != self.base.origin()
            || !(url.path().starts_with(self.base.path()) || url.path() == self.graphql.path())
        {
            return Err(fail());
        }
        let (parts, body) = request.into_parts();
        let mut token = http::HeaderValue::from_str(&self.token).map_err(|_| fail())?;
        token.set_sensitive(true);
        let response = self
            .client
            .request(parts.method, url)
            .headers(parts.headers)
            .header("PRIVATE-TOKEN", token)
            .body(body)
            .send()
            .await
            .map_err(|_| fail())?;
        let mut builder = Response::builder().status(response.status());
        *builder.headers_mut().ok_or_else(fail)? = response.headers().clone();
        builder
            .body(response.bytes().await.map_err(|_| fail())?)
            .map_err(|_| fail())
    }
}
