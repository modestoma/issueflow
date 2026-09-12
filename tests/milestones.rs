use async_trait::async_trait;
use http::Method;
use issueflow::{
    config::Platform,
    error::{Error, Result},
    milestone::{Input, Milestones},
    target::Target,
    transport::Transport,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex};
struct Mock {
    replies: Mutex<VecDeque<Result<Value>>>,
    calls: Mutex<Vec<(Method, String, Option<Value>)>>,
}
#[async_trait]
impl Transport for Mock {
    async fn request(&self, m: Method, p: &str, b: Option<Value>) -> Result<Value> {
        self.calls.lock().unwrap().push((m, p.into(), b));
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request")
    }
}
fn mock(values: Vec<Value>) -> Mock {
    Mock {
        replies: Mutex::new(values.into_iter().map(Ok).collect()),
        calls: Mutex::new(vec![]),
    }
}
fn service(m: &Mock, gh: bool) -> Milestones<'_> {
    Milestones {
        transport: m,
        target: Target {
            platform: if gh {
                Platform::Github
            } else {
                Platform::Gitlab
            },
            repository: if gh { "o/r" } else { "g/sub/r" }.into(),
            number: None,
        },
    }
}
fn value() -> Value {
    json!({"id":200,"number":3,"title":"Demo","description":"Summary","state":"open"})
}
fn input() -> Input {
    Input {
        title: Some("Demo".into()),
        description: Some("Summary".into()),
    }
}
#[tokio::test]
async fn preview_and_stale_snapshot_never_write() {
    let m = mock(vec![value()]);
    assert_eq!(
        service(&m, true)
            .update(3, input(), &value(), false)
            .await
            .unwrap()["preview"],
        true
    );
    assert_eq!(m.calls.lock().unwrap().len(), 1);
    let m = mock(vec![value()]);
    let mut old = value();
    old["description"] = "old".into();
    assert!(
        service(&m, true)
            .update(3, input(), &old, true)
            .await
            .is_err()
    );
    assert_eq!(m.calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn bind_uses_platform_identifiers_and_readback() {
    for gh in [true, false] {
        let issue = if gh {
            json!({"number":12,"milestone":null})
        } else {
            json!({"iid":12,"milestone":null})
        };
        let mut bound = issue.clone();
        bound["milestone"] = value();
        let m = mock(vec![value(), issue, json!({}), bound]);
        service(&m, gh)
            .bind(if gh { 3 } else { 200 }, 12, true)
            .await
            .unwrap();
        let calls = m.calls.lock().unwrap();
        assert_eq!(
            calls[2].2,
            Some(if gh {
                json!({"milestone":3})
            } else {
                json!({"milestone_id":200})
            })
        );
        if !gh {
            assert_eq!(calls[2].1, "projects/g%2Fsub%2Fr/issues/12");
        }
    }
}
#[tokio::test]
async fn binding_conflict_preserves_existing_membership() {
    let m = mock(vec![value(), json!({"number":12,"milestone":{"id":999}})]);
    assert!(service(&m, true).bind(3, 12, true).await.is_err());
    assert_eq!(m.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn close_maps_state_event_and_verifies() {
    for gh in [true, false] {
        let mut closed = value();
        closed["state"] = "closed".into();
        let m = mock(vec![value(), json!({}), closed]);
        service(&m, gh)
            .close(if gh { 3 } else { 200 }, &value(), "acceptance.md", true)
            .await
            .unwrap();
        assert_eq!(
            m.calls.lock().unwrap()[1].2,
            Some(if gh {
                json!({"state":"closed"})
            } else {
                json!({"state_event":"close"})
            })
        );
    }
}
#[tokio::test]
async fn create_recovers_marker_and_preserves_it_on_update() {
    let uuid = "74f27026-ea63-4908-8eea-c954e1b82af1";
    let mut existing = value();
    existing["description"] = format!("Summary\n\n<!-- roadmap-operation:{uuid} -->").into();
    let m = mock(vec![json!([existing.clone()])]);
    assert_eq!(
        service(&m, true).create(input(), uuid, true).await.unwrap()["reused"],
        true
    );
    assert_eq!(m.calls.lock().unwrap().len(), 1);
    let m = mock(vec![existing.clone(), json!({}), existing.clone()]);
    service(&m, true)
        .update(3, input(), &existing, true)
        .await
        .unwrap();
    assert_eq!(
        m.calls.lock().unwrap()[1].2.as_ref().unwrap()["description"],
        existing["description"]
    );
}
#[tokio::test]
async fn successful_write_then_bad_readback_is_unknown() {
    let m = mock(vec![value(), json!({}), json!({"id":999,"number":4})]);
    assert!(
        service(&m, true)
            .update(3, input(), &value(), true)
            .await
            .unwrap_err()
            .outcome_unknown
    );
}
#[tokio::test]
async fn unknown_create_is_not_retried() {
    let m = Mock {
        replies: Mutex::new(vec![Ok(json!([])), Err(Error::network(true))].into()),
        calls: Mutex::new(vec![]),
    };
    assert!(
        service(&m, true)
            .create(input(), "74f27026-ea63-4908-8eea-c954e1b82af1", true)
            .await
            .unwrap_err()
            .outcome_unknown
    );
    assert_eq!(m.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn malformed_create_response_is_unknown() {
    let m = mock(vec![json!([]), json!({})]);
    assert!(
        service(&m, true)
            .create(input(), "74f27026-ea63-4908-8eea-c954e1b82af1", true)
            .await
            .unwrap_err()
            .outcome_unknown
    );
}
#[tokio::test]
async fn create_reads_back_and_rejects_duplicate_markers() {
    let uuid = "74f27026-ea63-4908-8eea-c954e1b82af1";
    let mut v = value();
    v["description"] = format!("Summary\n\n<!-- roadmap-operation:{uuid} -->").into();
    let m = mock(vec![json!([]), v.clone(), v.clone()]);
    assert_eq!(
        service(&m, true).create(input(), uuid, true).await.unwrap()["applied"],
        true
    );
    let m = mock(vec![json!([v.clone(), v])]);
    assert!(service(&m, true).create(input(), uuid, true).await.is_err());
}
