use async_trait::async_trait;
use http::Method;
use issueflow::{
    config::{Config, Overrides},
    error::{Error, Result},
    project::{ProjectTarget, Projects},
    target::Target,
    transport::Transport,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};
struct Mock {
    replies: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<Value>>,
}
impl Mock {
    fn new(v: Vec<Value>) -> Self {
        Self {
            replies: Mutex::new(v.into()),
            calls: Mutex::new(vec![]),
        }
    }
}
#[async_trait]
impl Transport for Mock {
    async fn request(&self, method: Method, endpoint: &str, body: Option<Value>) -> Result<Value> {
        assert_eq!(method, Method::POST);
        assert_eq!(endpoint, "graphql");
        self.calls.lock().unwrap().push(body.unwrap());
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected call"))
    }
}
fn config() -> Config {
    Config::resolve(HashMap::new(), HashMap::new(), Overrides::default()).unwrap()
}
fn project() -> ProjectTarget {
    ProjectTarget::parse(&config(), "https://github.com/users/modestoma/projects/1").unwrap()
}
fn issue() -> Target {
    Target::from_url(&config(), "https://github.com/modestoma/issueflow/issues/4").unwrap()
}
fn meta() -> Value {
    json!({"data":{"owner":{"projectV2":{"id":"P1","title":"Board","url":"https://github.com/users/modestoma/projects/1","closed":false}}}})
}
fn page(field: &str, nodes: Value, next: bool, cursor: Value) -> Value {
    json!({"data":{"node":{field:{"nodes":nodes,"pageInfo":{"hasNextPage":next,"endCursor":cursor}}}}})
}
fn fields() -> Value {
    page(
        "fields",
        json!([{"id":"F1","name":"Status","options":[{"id":"O1","name":"Ready"},{"id":"O2","name":"Done"}]}]),
        false,
        Value::Null,
    )
}
fn id() -> Value {
    json!({"data":{"repository":{"issue":{"id":"I1"}}}})
}
fn item(option: &str) -> Value {
    json!({"id":"ITEM1","isArchived":false,"content":{"id":"I1","__typename":"Issue"},"fieldValueByName":{"optionId":option}})
}
#[test]
fn validates_project_urls() {
    for url in [
        "https://evil.test/users/a/projects/1",
        "https://user:secret@github.com/users/a/projects/1",
        "https://github.com/users/a/projects/0",
        "https://github.com/users/a/projects/1?x=y",
        "https://github.com/users/a/projects/1/views/2",
    ] {
        assert!(ProjectTarget::parse(&config(), url).is_err());
    }
    assert_eq!(
        ProjectTarget::parse(&config(), "https://github.com/orgs/example/projects/2")
            .unwrap()
            .kind,
        "organization"
    );
}
#[tokio::test]
async fn paginates_fields_and_items() {
    let m = Mock::new(vec![
        meta(),
        page(
            "fields",
            json!([{"id":"other","name":"Text"}]),
            true,
            json!("F-next"),
        ),
        fields(),
        page("items", json!([{"id":"a"}]), true, json!("I-next")),
        page("items", json!([{"id":"b"}]), false, Value::Null),
    ]);
    let p = Projects {
        transport: &m,
        target: project(),
    };
    let v = p.items().await.unwrap();
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    let c = m.calls.lock().unwrap();
    assert_eq!(c[2]["variables"]["after"], "F-next");
    assert_eq!(c[4]["variables"]["after"], "I-next");
}
#[tokio::test]
async fn rejects_partial_graphql_data() {
    let m = Mock::new(vec![
        json!({"data":{"owner":{}},"errors":[{"message":"SECRET"}]}),
    ]);
    let e = Projects {
        transport: &m,
        target: project(),
    }
    .show()
    .await
    .unwrap_err();
    assert!(!e.outcome_unknown);
    assert!(!e.message.contains("SECRET"));
}
#[tokio::test]
async fn reuse_membership_does_not_mutate() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([item("O1")]), false, Value::Null),
    ]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .add(&issue())
        .await
        .unwrap()["reused"],
        true
    );
    assert!(
        m.calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c["query"].as_str().unwrap().starts_with("query"))
    );
}
#[tokio::test]
async fn unknown_option_stops_before_mutation() {
    let m = Mock::new(vec![meta(), fields()]);
    assert!(
        Projects {
            transport: &m,
            target: project()
        }
        .status(&issue(), Some("In Progress"))
        .await
        .is_err()
    );
    assert_eq!(m.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn status_uses_ids_and_reads_back() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([item("O1")]), false, Value::Null),
        json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM1"}}}}),
        page("items", json!([item("O2")]), false, Value::Null),
    ]);
    let v = Projects {
        transport: &m,
        target: project(),
    }
    .status(&issue(), Some("Done"))
    .await
    .unwrap();
    assert_eq!(v["changed"], true);
    let calls = m.calls.lock().unwrap();
    assert_eq!(
        calls[4]["variables"],
        json!({"project":"P1","item":"ITEM1","field":"F1","option":"O2"})
    );
    assert!(!calls[4]["query"].as_str().unwrap().contains("closeIssue"));
}
#[tokio::test]
async fn mutation_errors_have_unknown_outcome() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([]), false, Value::Null),
        json!({"data":null,"errors":[{"message":"PRIVATE"}]}),
    ]);
    let e = Projects {
        transport: &m,
        target: project(),
    }
    .add(&issue())
    .await
    .unwrap_err();
    assert!(e.outcome_unknown);
    assert!(!e.message.contains("PRIVATE"));
}
#[tokio::test]
async fn repeated_cursor_is_not_partial_success() {
    let m = Mock::new(vec![
        meta(),
        page("fields", json!([{"id":"1"}]), true, json!("same")),
        page("fields", json!([{"id":"2"}]), true, json!("same")),
    ]);
    assert!(
        Projects {
            transport: &m,
            target: project()
        }
        .show()
        .await
        .is_err()
    );
}
struct Offline;
#[async_trait]
impl Transport for Offline {
    async fn request(&self, _: Method, _: &str, _: Option<Value>) -> Result<Value> {
        Err(Error::network(true))
    }
}
#[tokio::test]
async fn read_timeout_does_not_claim_unknown_write() {
    assert!(
        !Projects {
            transport: &Offline,
            target: project()
        }
        .show()
        .await
        .unwrap_err()
        .outcome_unknown
    );
}

#[tokio::test]
async fn missing_membership_is_not_implicitly_added() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([]), false, Value::Null),
    ]);
    let e = Projects {
        transport: &m,
        target: project(),
    }
    .status(&issue(), Some("Ready"))
    .await
    .unwrap_err();
    assert_eq!(e.code, "not_found");
    assert!(
        m.calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c["query"].as_str().unwrap().starts_with("query"))
    );
}
#[tokio::test]
async fn matching_status_is_a_noop() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([item("O1")]), false, Value::Null),
    ]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .status(&issue(), Some("Ready"))
        .await
        .unwrap()["changed"],
        false
    );
    assert_eq!(m.calls.lock().unwrap().len(), 4);
}
#[tokio::test]
async fn mismatched_readback_reports_possible_write() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([item("O1")]), false, Value::Null),
        json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM1"}}}}),
        page("items", json!([item("O1")]), false, Value::Null),
    ]);
    let e = Projects {
        transport: &m,
        target: project(),
    }
    .status(&issue(), Some("Done"))
    .await
    .unwrap_err();
    assert!(e.outcome_unknown);
    assert_eq!(e.code, "conflict");
}
#[tokio::test]
async fn adds_once_and_confirms_membership() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([]), false, Value::Null),
        json!({"data":{"addProjectV2ItemById":{"item":{"id":"ITEM1"}}}}),
        page("items", json!([item("O1")]), false, Value::Null),
    ]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .add(&issue())
        .await
        .unwrap()["reused"],
        false
    );
    assert_eq!(
        m.calls.lock().unwrap()[4]["variables"],
        json!({"project":"P1","content":"I1"})
    );
}
#[tokio::test]
async fn permission_errors_are_actionable_and_redacted() {
    let m = Mock::new(vec![
        json!({"errors":[{"type":"INSUFFICIENT_SCOPES","message":"SECRET"}]}),
    ]);
    let e = Projects {
        transport: &m,
        target: project(),
    }
    .show()
    .await
    .unwrap_err();
    assert_eq!(e.code, "permission");
    assert_eq!(e.exit_code(), 3);
    assert!(!e.message.contains("SECRET"));
}
#[tokio::test]
async fn archived_status_writes_are_rejected() {
    let mut archived = item("O1");
    archived["isArchived"] = json!(true);
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([archived]), false, Value::Null),
    ]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .status(&issue(), Some("Done"))
        .await
        .unwrap_err()
        .code,
        "input"
    );
}

fn owner_list(nodes: Value) -> Value {
    json!({"data":{"owner":{"id":"OWNER1","projectsV2":{"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}}}}})
}
fn initialized_fields() -> Value {
    page(
        "fields",
        json!([{"id":"F1","name":"Status","options":[
{"id":"old","name":"Todo","color":"BLUE","description":"keep me"},
{"id":"b","name":"Backlog","color":"GRAY","description":""},
{"id":"r","name":"Ready","color":"BLUE","description":""},
{"id":"p","name":"In progress","color":"YELLOW","description":""},
{"id":"v","name":"In review","color":"PURPLE","description":""},
{"id":"d","name":"Done","color":"GREEN","description":""},
{"id":"c","name":"Cancelled","color":"GRAY","description":""}]}]),
        false,
        Value::Null,
    )
}
#[tokio::test]
async fn create_reuses_unique_open_title() {
    let m = Mock::new(vec![
        owner_list(json!([{"id":"P1","number":1,"title":"Board","closed":false}])),
        meta(),
        fields(),
    ]);
    let v = Projects {
        transport: &m,
        target: project(),
    }
    .create("Board")
    .await
    .unwrap();
    assert_eq!(v["reused"], true);
    assert!(
        m.calls
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["query"].as_str().unwrap().starts_with("query"))
    );
}
#[tokio::test]
async fn create_verifies_new_project() {
    let m = Mock::new(vec![
        owner_list(json!([])),
        json!({"data":{"createProjectV2":{"projectV2":{"id":"P1","number":1}}}}),
        meta(),
        fields(),
    ]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .create("Board")
        .await
        .unwrap()["reused"],
        false
    );
    assert_eq!(m.calls.lock().unwrap()[1]["variables"]["owner"], "OWNER1");
}
#[tokio::test]
async fn ambiguous_titles_never_create() {
    let m = Mock::new(vec![owner_list(
        json!([{"title":"Board"},{"title":"Board"}]),
    )]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .create("Board")
        .await
        .unwrap_err()
        .code,
        "response"
    );
}
#[tokio::test]
async fn statuses_preserve_existing_ids_and_metadata() {
    let old = page(
        "fields",
        json!([{"id":"F1","name":"Status","options":[{"id":"old","name":"Todo","color":"BLUE","description":"keep me"}]}]),
        false,
        Value::Null,
    );
    let m = Mock::new(vec![
        meta(),
        old.clone(),
        meta(),
        old,
        json!({"data":{"updateProjectV2Field":{}}}),
        meta(),
        initialized_fields(),
    ]);
    let v = Projects {
        transport: &m,
        target: project(),
    }
    .init_statuses()
    .await
    .unwrap();
    assert_eq!(v["changed"], true);
    let c = m.calls.lock().unwrap();
    assert_eq!(
        c[4]["variables"]["options"][0],
        json!({"id":"old","name":"Todo","color":"BLUE","description":"keep me"})
    );
}
#[tokio::test]
async fn initialized_statuses_do_not_write() {
    let m = Mock::new(vec![meta(), initialized_fields()]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .init_statuses()
        .await
        .unwrap()["changed"],
        false
    );
    assert_eq!(m.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn ambiguous_existing_title_reports_conflict() {
    let m = Mock::new(vec![owner_list(
        json!([{"id":"p1","title":"Board"},{"id":"p2","title":"Board"}]),
    )]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .create("Board")
        .await
        .unwrap_err()
        .code,
        "conflict"
    );
}

fn view_page(nodes: Value) -> Value {
    json!({"data":{"node":{"views":{"nodes":nodes,"pageInfo":{"hasNextPage":false}}}}})
}
fn board() -> Value {
    json!({"id":"V1","name":"Kanban","layout":"BOARD_LAYOUT","filter":"","groupByFields":{"nodes":[],"pageInfo":{"hasNextPage":false}},"verticalGroupByFields":{"nodes":[{"id":"F1","name":"Status"}],"pageInfo":{"hasNextPage":false}}})
}
#[tokio::test]
async fn existing_status_board_is_reused() {
    let m = Mock::new(vec![meta(), fields(), view_page(json!([board()]))]);
    let v = Projects {
        transport: &m,
        target: project(),
    }
    .ensure_board()
    .await
    .unwrap();
    assert_eq!(v["created"], false);
    assert!(
        m.calls
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["query"].as_str().unwrap().starts_with("query"))
    );
}
#[tokio::test]
async fn new_board_is_read_back_with_status_grouping() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        view_page(json!([])),
        json!({"data":{"createProjectV2View":{"projectV2View":{"id":"V1"}}}}),
        view_page(json!([board()])),
    ]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .ensure_board()
        .await
        .unwrap()["created"],
        true
    );
}
#[tokio::test]
async fn wrong_board_grouping_is_not_reported_success() {
    let mut b = board();
    b["verticalGroupByFields"]["nodes"] = json!([]);
    let m = Mock::new(vec![
        meta(),
        fields(),
        view_page(json!([])),
        json!({"data":{"createProjectV2View":{"projectV2View":{"id":"V1"}}}}),
        view_page(json!([b])),
    ]);
    assert!(
        Projects {
            transport: &m,
            target: project()
        }
        .ensure_board()
        .await
        .unwrap_err()
        .outcome_unknown
    );
}
#[tokio::test]
async fn arbitrary_select_field_uses_exact_name_and_preserves_status() {
    let mut f = fields();
    f["data"]["node"]["fields"]["nodes"][0]["name"] = json!("Priority");
    let m = Mock::new(vec![
        meta(),
        f,
        id(),
        page("items", json!([item("O2")]), false, Value::Null),
        json!({"data":{"updateProjectV2ItemFieldValue":{}}}),
        page("items", json!([item("O1")]), false, Value::Null),
    ]);
    let v = Projects {
        transport: &m,
        target: project(),
    }
    .field(&issue(), "Priority", Some("Ready"), false)
    .await
    .unwrap();
    assert_eq!(v["changed"], true);
    assert!(
        m.calls.lock().unwrap()[3]["query"]
            .as_str()
            .unwrap()
            .contains("fieldValueByName(name:\"Priority\")")
    );
}
#[tokio::test]
async fn clear_field_verifies_unset_value() {
    let mut cleared = item("O1");
    cleared["fieldValueByName"] = Value::Null;
    let m = Mock::new(vec![
        meta(),
        fields(),
        id(),
        page("items", json!([item("O1")]), false, Value::Null),
        json!({"data":{"clearProjectV2ItemFieldValue":{}}}),
        page("items", json!([cleared]), false, Value::Null),
    ]);
    assert_eq!(
        Projects {
            transport: &m,
            target: project()
        }
        .field(&issue(), "Status", None, true)
        .await
        .unwrap()["changed"],
        true
    );
    assert!(
        m.calls.lock().unwrap()[4]["query"]
            .as_str()
            .unwrap()
            .contains("clearProjectV2ItemFieldValue")
    );
}

fn repository() -> Value {
    json!({"id":"R1","nameWithOwner":"modestoma/issueflow","url":"https://github.com/modestoma/issueflow"})
}
fn repository_lookup() -> Value {
    json!({"data":{"repository":repository()}})
}
#[tokio::test]
async fn repository_link_writes_once_and_verifies() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        repository_lookup(),
        page("repositories", json!([]), false, Value::Null),
        json!({"data":{"linkProjectV2ToRepository":{"repository":{"id":"R1"}}}}),
        page("repositories", json!([repository()]), false, Value::Null),
    ]);
    let v = Projects {
        transport: &m,
        target: project(),
    }
    .link_repository("modestoma/issueflow")
    .await
    .unwrap();
    assert_eq!(v["changed"], true);
    let calls = m.calls.lock().unwrap();
    let writes: Vec<_> = calls
        .iter()
        .filter(|v| v["query"].as_str().unwrap().starts_with("mutation"))
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0]["variables"],
        json!({"project":"P1","repository":"R1"})
    );
    assert!(m.replies.lock().unwrap().is_empty());
}
#[tokio::test]
async fn existing_repository_link_on_later_page_is_reused() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        repository_lookup(),
        page(
            "repositories",
            json!([{"id":"R2","nameWithOwner":"modestoma/other","url":"https://github.com/modestoma/other"}]),
            true,
            json!("next"),
        ),
        page("repositories", json!([repository()]), false, Value::Null),
    ]);
    let v = Projects {
        transport: &m,
        target: project(),
    }
    .link_repository("modestoma/issueflow")
    .await
    .unwrap();
    assert_eq!(v["changed"], false);
    assert!(
        m.calls
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["query"].as_str().unwrap().starts_with("query"))
    );
}
#[tokio::test]
async fn invalid_or_redirected_repository_never_mutates() {
    let m = Mock::new(vec![]);
    let p = Projects {
        transport: &m,
        target: project(),
    };
    assert!(p.link_repository("https://github.com/a/b").await.is_err());
    assert!(m.calls.lock().unwrap().is_empty());
    let m = Mock::new(vec![meta(), fields(), repository_lookup()]);
    let err = Projects {
        transport: &m,
        target: project(),
    }
    .link_repository("modestoma/renamed")
    .await
    .unwrap_err();
    assert_eq!(err.code, "conflict");
}
#[tokio::test]
async fn failed_repository_link_readback_is_unknown() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        repository_lookup(),
        page("repositories", json!([]), false, Value::Null),
        json!({"data":{"linkProjectV2ToRepository":{"repository":{"id":"R1"}}}}),
        page("repositories", json!([]), false, Value::Null),
    ]);
    let err = Projects {
        transport: &m,
        target: project(),
    }
    .link_repository("modestoma/issueflow")
    .await
    .unwrap_err();
    assert!(err.outcome_unknown);
}
#[tokio::test]
async fn repository_link_permission_failure_is_not_retried() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        repository_lookup(),
        page("repositories", json!([]), false, Value::Null),
        json!({"errors":[{"type":"FORBIDDEN","message":"secret"}]}),
    ]);
    let err = Projects {
        transport: &m,
        target: project(),
    }
    .link_repository("modestoma/issueflow")
    .await
    .unwrap_err();
    assert_eq!(err.code, "permission");
    assert!(err.outcome_unknown);
    assert!(!err.message.contains("secret"));
    assert!(m.replies.lock().unwrap().is_empty());
}
#[tokio::test]
async fn repository_pagination_rejects_repeated_cursor() {
    let m = Mock::new(vec![
        meta(),
        fields(),
        page("repositories", json!([]), true, json!("same")),
        page("repositories", json!([]), true, json!("same")),
    ]);
    assert!(
        Projects {
            transport: &m,
            target: project()
        }
        .repositories()
        .await
        .is_err()
    );
}
