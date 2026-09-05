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
