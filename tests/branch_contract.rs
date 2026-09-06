use issueflow::branch_contract::BranchContract;
use serde_json::{Value, json};
fn parent() -> Value {
    json!({"schema_version":1,"issue_url":"https://github.com/owner/repo/issues/7","source_branch":"main","branch":"feat/issue-7-parent","pr_target":"main"})
}
fn child() -> Value {
    json!({"schema_version":1,"issue_url":"https://github.com/owner/repo/issues/12","parent_issue_url":"https://github.com/owner/repo/issues/7","source_branch":"feat/issue-7-parent","branch":"feat/issue-12-child","pr_target":"feat/issue-7-parent"})
}
#[test]
fn correct_child_contract_is_local_only() {
    let p: BranchContract = serde_json::from_value(parent()).unwrap();
    let c: BranchContract = serde_json::from_value(child()).unwrap();
    let v = c.validate(Some(&p)).unwrap();
    assert_eq!(v["relationship"], "child");
    assert_eq!(v["remote_verified"], false);
}
#[test]
fn child_cannot_silently_target_main() {
    let p = serde_json::from_value(parent()).unwrap();
    let mut c = child();
    c["source_branch"] = json!("main");
    c["pr_target"] = json!("main");
    assert!(
        serde_json::from_value::<BranchContract>(c)
            .unwrap()
            .validate(Some(&p))
            .is_err()
    );
}
#[test]
fn child_requires_matching_parent() {
    let c: BranchContract = serde_json::from_value(child()).unwrap();
    assert!(c.validate(None).is_err());
    let mut p = parent();
    p["issue_url"] = json!("https://github.com/owner/repo/issues/8");
    assert!(
        c.validate(Some(&serde_json::from_value(p).unwrap()))
            .is_err()
    );
}
#[test]
fn rejects_cross_repository_pr() {
    let mut p = parent();
    p["pr_url"] = json!("https://github.com/other/repo/pull/5");
    assert!(
        serde_json::from_value::<BranchContract>(p)
            .unwrap()
            .validate(None)
            .is_err()
    );
}
#[test]
fn root_validates_and_invalid_branch_fails() {
    let c: BranchContract = serde_json::from_value(parent()).unwrap();
    assert!(c.validate(None).is_ok());
    let mut p = parent();
    p["branch"] = json!("../main");
    assert!(
        serde_json::from_value::<BranchContract>(p)
            .unwrap()
            .validate(None)
            .is_err()
    );
}

#[test]
fn gitlab_contract_preserves_non_default_port() {
    let contract: BranchContract = serde_json::from_value(json!({
        "schema_version":1,
        "issue_url":"https://gitlab.example:8443/group/repo/-/issues/1",
        "source_branch":"main",
        "branch":"feat/change",
        "pr_target":"main",
        "pr_url":"https://gitlab.example:8443/group/repo/-/merge_requests/2"
    }))
    .unwrap();
    assert!(contract.validate(None).is_ok());
}

#[test]
fn gitlab_task_child_contract_is_supported() {
    let parent: BranchContract = serde_json::from_value(json!({
        "schema_version":1,
        "issue_url":"https://gitlab.example/group/repo/-/issues/1",
        "parent_issue_url":null,
        "source_branch":"main",
        "branch":"chore/gitlab_e2e_parent",
        "pr_target":"main",
        "pr_url":null
    }))
    .unwrap();
    let child: BranchContract = serde_json::from_value(json!({
        "schema_version":1,
        "issue_url":"https://gitlab.example/group/repo/-/work_items/9",
        "parent_issue_url":"https://gitlab.example/group/repo/-/issues/1",
        "source_branch":"chore/gitlab_e2e_parent",
        "branch":"chore/gitlab_e2e_child_a",
        "pr_target":"chore/gitlab_e2e_parent",
        "pr_url":null
    }))
    .unwrap();

    let value = child.validate(Some(&parent)).unwrap();
    assert_eq!(value["relationship"], "child");
    assert_eq!(value["remote_verified"], false);
}
