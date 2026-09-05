use async_trait::async_trait;
use http::Method;
use issueflow::{
    config::Platform, error::Result, service::Service, target::Target, transport::Transport,
};
use serde_json::{Value, json};
use std::sync::Mutex;
struct Mock {
    items: Value,
    calls: Mutex<usize>,
}
#[async_trait]
impl Transport for Mock {
    async fn request(&self, m: Method, p: &str, _: Option<Value>) -> Result<Value> {
        assert_eq!(m, Method::GET);
        assert!(p.contains("state=all"));
        *self.calls.lock().unwrap() += 1;
        Ok(self.items.clone())
    }
}
fn item(id: u64) -> Value {
    json!({"id":id,"number":id,"html_url":format!("https://github.com/a/b/issues/{id}"),"state":"closed","labels":[],"body":"text\n\n<!-- issueflow-operation: ABCDEF00-0000-4000-8000-000000000000 -->"})
}
#[tokio::test]
async fn zero_one_and_multiple_matches_never_authorize_retry() {
    for (items, status) in [
        (json!([]), "not_visible"),
        (json!([item(1)]), "found"),
        (json!([item(1), item(2)]), "ambiguous"),
    ] {
        let m = Mock {
            items,
            calls: Mutex::new(0),
        };
        let s = Service {
            transport: &m,
            target: Target {
                platform: Platform::Github,
                repository: "a/b".into(),
                number: None,
            },
        };
        let v = s
            .recover_create("abcdef00-0000-4000-8000-000000000000")
            .await
            .unwrap();
        assert_eq!(v["status"], status);
        assert_eq!(v["safe_to_retry"], false);
        assert_eq!(*m.calls.lock().unwrap(), 1);
    }
}
#[tokio::test]
async fn invalid_uuid_never_reaches_api() {
    let m = Mock {
        items: json!([]),
        calls: Mutex::new(0),
    };
    let s = Service {
        transport: &m,
        target: Target {
            platform: Platform::Github,
            repository: "a/b".into(),
            number: None,
        },
    };
    assert!(s.recover_create("wrong").await.is_err());
    assert_eq!(*m.calls.lock().unwrap(), 0);
}
