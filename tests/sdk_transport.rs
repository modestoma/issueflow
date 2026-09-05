use http::Method;
use issueflow::{
    config::{Config, Overrides, Platform},
    transport::{SdkTransport, Transport},
};
use serde_json::json;
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

fn server(
    status: u16,
    body: &'static str,
    headers: &'static str,
) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "server timed out");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = stream.read(&mut chunk).unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..end]);
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    break;
                }
            }
        }
        write!(stream,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",body.len()).unwrap();
        String::from_utf8(bytes).unwrap()
    });
    (address, handle)
}
fn config(platform: Platform, address: &str) -> Config {
    let mut config = Config::resolve(
        HashMap::from([
            ("ISSUEFLOW_GITHUB_TOKEN".into(), "sdk-test-secret".into()),
            ("ISSUEFLOW_GITLAB_TOKEN".into(), "sdk-test-secret".into()),
        ]),
        HashMap::new(),
        Overrides::default(),
    )
    .unwrap();
    // Directly constructed test config: production parsing permits HTTPS only.
    if platform == Platform::Github {
        config.github_api_url = format!("{address}/api/v3/").parse().unwrap();
    } else {
        config.gitlab_url = Some(format!("{address}/tools/").parse().unwrap());
    }
    config
}

#[tokio::test]
async fn both_sdks_send_correct_base_path_auth_and_json() {
    for platform in [Platform::Github, Platform::Gitlab] {
        let (address, server) = server(201, "{\"id\":42}", "");
        let transport = SdkTransport::new(&config(platform, &address), platform).unwrap();
        let result = transport
            .request(
                Method::POST,
                "projects/group%2Frepo/issues",
                Some(json!({"title":"中文 `literal`"})),
            )
            .await
            .unwrap();
        assert_eq!(result["id"], 42);
        let request = server.join().unwrap();
        let lower = request.to_ascii_lowercase();
        let prefix = if platform == Platform::Github {
            "POST /api/v3/"
        } else {
            "POST /tools/api/v4/"
        };
        assert!(request.starts_with(prefix), "{request}");
        assert!(lower.contains(if platform == Platform::Github {
            "authorization: bearer sdk-test-secret"
        } else {
            "private-token: sdk-test-secret"
        }));
        assert!(request.contains("中文 `literal`"));
    }
}

#[tokio::test]
async fn both_sdks_handle_empty_success() {
    for platform in [Platform::Github, Platform::Gitlab] {
        let (address, server) = server(204, "", "");
        let transport = SdkTransport::new(&config(platform, &address), platform).unwrap();
        assert!(
            transport
                .request(Method::DELETE, "issues/2", None)
                .await
                .unwrap()
                .is_null()
        );
        server.join().unwrap();
    }
}

#[tokio::test]
async fn redirects_are_not_followed_and_errors_are_redacted() {
    for platform in [Platform::Github, Platform::Gitlab] {
        let (address, server) = server(
            302,
            "{\"message\":\"sdk-test-secret\"}",
            "Location: https://example.invalid/secret\r\n",
        );
        let transport = SdkTransport::new(&config(platform, &address), platform).unwrap();
        let error = transport
            .request(Method::GET, "user", None)
            .await
            .unwrap_err();
        assert_eq!(error.status, Some(302));
        assert!(!error.to_string().contains("sdk-test-secret"));
        server.join().unwrap();
    }
}

#[tokio::test]
async fn rate_limits_are_structured_without_retries() {
    for platform in [Platform::Github, Platform::Gitlab] {
        let (address, server) = server(429, "{\"message\":\"rate limit\"}", "");
        let transport = SdkTransport::new(&config(platform, &address), platform).unwrap();
        let error = transport
            .request(Method::POST, "issues", Some(json!({})))
            .await
            .unwrap_err();
        assert_eq!(error.code, "rate_limited");
        server.join().unwrap();
    }
}

#[tokio::test]
async fn projects_use_sdk_graphql_post_and_classify_permission_errors() {
    use issueflow::project::{ProjectTarget, Projects};
    let (address, server) = server(
        200,
        r#"{"errors":[{"type":"INSUFFICIENT_SCOPES","message":"sdk-test-secret"}]}"#,
        "",
    );
    let mut cfg = config(Platform::Github, &address);
    cfg.github_api_url = format!("{address}/").parse().unwrap();
    let transport = SdkTransport::new(&cfg, Platform::Github).unwrap();
    let projects = Projects {
        transport: &transport,
        target: ProjectTarget {
            owner: "test-owner".into(),
            kind: "user",
            number: 1,
        },
    };
    let error = projects.show().await.unwrap_err();
    assert_eq!(error.code, "permission");
    assert!(!error.outcome_unknown);
    assert!(!error.message.contains("sdk-test-secret"));
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /graphql "));
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    let payload: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        payload["variables"],
        json!({"owner":"test-owner","number":1})
    );
    assert!(
        payload["query"]
            .as_str()
            .unwrap()
            .contains("owner:user(login:$owner)")
    );
}

#[tokio::test]
async fn gitlab_graphql_uses_instance_base_path_and_private_token() {
    let (address, server) = server(
        200,
        r#"{"data":{"workItem":{"id":"gid://gitlab/WorkItem/1"}}}"#,
        "",
    );
    let transport =
        SdkTransport::new(&config(Platform::Gitlab, &address), Platform::Gitlab).unwrap();
    let result = transport
        .request(
            Method::POST,
            "graphql",
            Some(json!({"query":"query { workItem { id } }"})),
        )
        .await
        .unwrap();
    assert_eq!(result["data"]["workItem"]["id"], "gid://gitlab/WorkItem/1");
    let request = server.join().unwrap();
    assert!(
        request.starts_with("POST /tools/api/graphql? "),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("private-token: sdk-test-secret")
    );
}
