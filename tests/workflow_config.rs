use issueflow::workflow_config::WorkflowConfig;
use serde_json::{Value, json};
fn sample() -> Value {
    json!({"schema_version":1,"platform":"github","host":"github.com","repository":"owner/repo","remote":"origin","base_branch":"main","timezone":"Asia/Shanghai","permissions":{"local_commit":true,"push":true,"pull_request":true},"github_project_url":"https://github.com/users/owner/projects/1"})
}
#[test]
fn defaults_to_explicit_acceptance() {
    let c: WorkflowConfig = serde_json::from_value(sample()).unwrap();
    assert_eq!(
        c.validate().unwrap()["delivery_policy"],
        "acceptance_required"
    );
}
#[test]
fn rejects_credentials_and_unknown_policy() {
    let mut v = sample();
    v["token"] = json!("secret");
    assert!(serde_json::from_value::<WorkflowConfig>(v).is_err());
    let mut v = sample();
    v["delivery_policy"] = json!("automatic");
    assert!(serde_json::from_value::<WorkflowConfig>(v).is_err());
}
#[test]
fn rejects_invalid_target_or_branch() {
    for (key, value) in [
        ("repository", "../repo"),
        ("base_branch", "--delete"),
        ("github_project_url", "https://evil.test/users/a/projects/1"),
    ] {
        let mut v = sample();
        v[key] = json!(value);
        assert!(
            serde_json::from_value::<WorkflowConfig>(v)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
#[test]
fn preserves_legacy_permissions() {
    let mut v = sample();
    v["permissions"] = json!({"local_commit":false,"push":false,"draft_pr_mr":false});
    let c: WorkflowConfig = serde_json::from_value(v).unwrap();
    let r = c.validate().unwrap();
    assert_eq!(r["permissions"]["draft_pr_mr"], false);
    assert!(r["permissions"]["pull_request"].is_null());
}
#[test]
fn capabilities_ignore_bad_environment() {
    let r = std::process::Command::new(env!("CARGO_BIN_EXE_issueflow"))
        .env("ISSUEFLOW_TIMEOUT_SECONDS", "invalid")
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(r.status.success());
    let v: Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(v["capability_schema_version"], 1);
    assert!(v.to_string().contains("workflow"));
}
