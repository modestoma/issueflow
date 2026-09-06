use async_trait::async_trait;
use http::Method;
use issueflow::{
    board::Boards, config::Platform, error::Result, target::Target, transport::Transport,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex};

struct Mock {
    replies: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<(Method, String, Option<Value>)>>,
}
#[async_trait]
impl Transport for Mock {
    async fn request(&self, method: Method, endpoint: &str, body: Option<Value>) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((method, endpoint.into(), body));
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request"))
    }
}
fn target() -> Target {
    Target {
        platform: Platform::Gitlab,
        repository: "group/sub/repo".into(),
        number: None,
    }
}
fn labels() -> Value {
    Value::Array(
        [
            "workflow::Backlog",
            "workflow::Ready",
            "workflow::In progress",
            "workflow::In review",
            "workflow::Done",
            "workflow::Cancelled",
            "needs-clarification",
            "blocked",
            "resolution::Cancelled",
            "resolution::Duplicate",
            "resolution::Invalid",
            "type::bug",
            "type::feature",
            "type::improvement",
            "type::refactor",
            "type::docs",
            "type::chore",
            "type::research",
            "priority::P0",
            "priority::P1",
            "priority::P2",
            "priority::P3",
        ]
        .iter()
        .enumerate()
        .map(|(index, name)| json!({"id":index + 1,"name":name}))
        .collect(),
    )
}
fn lists() -> Value {
    Value::Array([
        "workflow::Backlog", "workflow::Ready", "workflow::In progress",
        "workflow::In review", "workflow::Done", "workflow::Cancelled",
    ].iter().enumerate().map(|(index, name)| json!({"id":index + 11,"position":index,"label":{"id":index + 1,"name":name}})).collect())
}

fn lists_with_positions(positions: [usize; 6]) -> Value {
    Value::Array([
        "workflow::Backlog", "workflow::Ready", "workflow::In progress",
        "workflow::In review", "workflow::Done", "workflow::Cancelled",
    ].iter().enumerate().map(|(index, name)| json!({"id":index + 11,"position":positions[index],"label":{"id":index + 1,"name":name}})).collect())
}

#[tokio::test]
async fn list_and_show_use_nested_project_path() {
    let board = json!({"id":3,"name":"Issueflow Workflow","lists":[]});
    let mock = Mock {
        replies: Mutex::new(vec![json!([board.clone()]), board.clone()].into()),
        calls: Mutex::new(vec![]),
    };
    let service = Boards {
        transport: &mock,
        target: target(),
    };
    assert_eq!(service.list().await.unwrap()[0]["id"], 3);
    assert_eq!(service.show(3).await.unwrap()["name"], "Issueflow Workflow");
    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls[0].1,
        "projects/group%2Fsub%2Frepo/boards?per_page=100&page=1"
    );
    assert_eq!(calls[1].1, "projects/group%2Fsub%2Frepo/boards/3");
}

#[tokio::test]
async fn create_reuses_a_unique_named_board_without_writing() {
    let board = json!({"id":3,"name":"Delivery","lists":[]});
    let mock = Mock {
        replies: Mutex::new(vec![json!([board.clone()]), board].into()),
        calls: Mutex::new(vec![]),
    };
    let result = Boards {
        transport: &mock,
        target: target(),
    }
    .create("Delivery")
    .await
    .unwrap();
    assert_eq!(result["changed"], false);
    assert_eq!(result["board"]["id"], 3);
    assert!(
        mock.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _, _)| *method == Method::GET)
    );
}

#[tokio::test]
async fn create_writes_once_and_reads_the_new_board_back() {
    let board = json!({"id":3,"name":"Delivery","lists":[]});
    let mock = Mock {
        replies: Mutex::new(vec![json!([]), json!({"id":3}), board].into()),
        calls: Mutex::new(vec![]),
    };
    let result = Boards {
        transport: &mock,
        target: target(),
    }
    .create("Delivery")
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    assert_eq!(result["board"]["name"], "Delivery");
    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _, _)| *method == Method::POST)
            .count(),
        1
    );
    assert_eq!(calls[1].2.as_ref().unwrap()["name"], "Delivery");
}

#[tokio::test]
async fn repeated_workflow_initialization_is_a_read_only_noop() {
    let board = json!({"id":3,"name":"Issueflow Workflow","lists":lists()});
    let mock = Mock {
        replies: Mutex::new(
            vec![
                labels(),
                labels(),
                json!([board.clone()]),
                lists(),
                lists(),
                board,
                lists(),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let result = Boards {
        transport: &mock,
        target: target(),
    }
    .init_workflow("Issueflow Workflow")
    .await
    .unwrap();
    assert_eq!(result["changed"], false);
    assert_eq!(result["workflow_lists"].as_array().unwrap().len(), 6);
    assert!(
        mock.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _, _)| *method == Method::GET)
    );
}

#[tokio::test]
async fn targeted_workflow_initialization_uses_the_explicit_board_id() {
    let board = json!({"id":9,"name":"Team Board","lists":lists()});
    let mock = Mock {
        replies: Mutex::new(
            vec![
                board.clone(),
                labels(),
                labels(),
                lists(),
                lists(),
                board,
                lists(),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let result = Boards {
        transport: &mock,
        target: target(),
    }
    .init_workflow_target("ignored for an explicit board", Some(9))
    .await
    .unwrap();
    assert_eq!(result["changed"], false);
    assert_eq!(result["board"]["id"], 9);
    assert!(mock.calls.lock().unwrap()[0].1.ends_with("/boards/9"));
}

#[tokio::test]
async fn invalid_explicit_board_stops_before_label_writes() {
    let mock = Mock {
        replies: Mutex::new(vec![json!({"message":"not found"})].into()),
        calls: Mutex::new(vec![]),
    };
    let error = Boards {
        transport: &mock,
        target: target(),
    }
    .init_workflow_target("ignored", Some(9))
    .await
    .unwrap_err();
    assert_eq!(error.code, "response");
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Method::GET);
    assert!(calls[0].1.ends_with("/boards/9"));
}

#[tokio::test]
async fn misordered_workflow_lists_move_only_mismatches_to_zero_based_positions() {
    let current = lists_with_positions([1, 0, 2, 3, 4, 5]);
    let board = json!({"id":3,"name":"Issueflow Workflow","lists":lists()});
    let mock = Mock {
        replies: Mutex::new(
            vec![
                labels(),
                labels(),
                json!([board.clone()]),
                current.clone(),
                current,
                json!({"id":11}),
                json!({"id":12}),
                board,
                lists(),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let result = Boards {
        transport: &mock,
        target: target(),
    }
    .init_workflow("Issueflow Workflow")
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    let calls = mock.calls.lock().unwrap();
    let moves: Vec<_> = calls
        .iter()
        .filter(|(method, _, _)| *method == Method::PUT)
        .map(|(_, endpoint, body)| {
            (
                endpoint.rsplit('/').next().unwrap(),
                body.as_ref().unwrap()["position"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(moves, vec![("11", 0), ("12", 1)]);
}

#[tokio::test]
async fn creates_board_and_missing_workflow_lists_then_verifies() {
    let board = json!({"id":3,"name":"Issueflow Workflow","lists":lists()});
    let mut replies = vec![labels(), labels(), json!([]), json!({"id":3}), json!([])];
    replies.extend((0..6).map(|_| json!({"id":99})));
    replies.extend([lists(), board, lists()]);
    let mock = Mock {
        replies: Mutex::new(replies.into()),
        calls: Mutex::new(vec![]),
    };
    let result = Boards {
        transport: &mock,
        target: target(),
    }
    .init_workflow("Issueflow Workflow")
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _, _)| *method == Method::POST)
            .count(),
        7
    );
    assert_eq!(calls[3].2.as_ref().unwrap()["name"], "Issueflow Workflow");
    let label_ids: Vec<_> = calls
        .iter()
        .filter(|(method, endpoint, _)| *method == Method::POST && endpoint.ends_with("/lists"))
        .map(|(_, _, body)| body.as_ref().unwrap()["label_id"].as_u64().unwrap())
        .collect();
    assert_eq!(label_ids, vec![1, 2, 3, 4, 5, 6]);
}

#[tokio::test]
async fn ambiguous_named_boards_stop_without_board_writes() {
    let board = |id| json!({"id":id,"name":"Issueflow Workflow","lists":[]});
    let mock = Mock {
        replies: Mutex::new(vec![labels(), labels(), json!([board(2), board(3)])].into()),
        calls: Mutex::new(vec![]),
    };
    let error = Boards {
        transport: &mock,
        target: target(),
    }
    .init_workflow("Issueflow Workflow")
    .await
    .unwrap_err();
    assert_eq!(error.code, "conflict");
    assert!(
        mock.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _, _)| *method == Method::GET)
    );
}

#[tokio::test]
async fn board_commands_reject_github_before_api_access() {
    let mock = Mock {
        replies: Mutex::new(vec![].into()),
        calls: Mutex::new(vec![]),
    };
    let service = Boards {
        transport: &mock,
        target: Target {
            platform: Platform::Github,
            repository: "owner/repo".into(),
            number: None,
        },
    };
    assert!(service.list().await.is_err());
    assert!(mock.calls.lock().unwrap().is_empty());
}
