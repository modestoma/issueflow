use std::{fs, process::Command};

use serde_json::Value;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_issueflow"))
        .env_clear()
        .env("ISSUEFLOW_TIMEOUT_SECONDS", "not-a-number")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn config_validate_is_offline_and_workflow_alias_warns_only_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("workflow.json");
    fs::write(
        &config,
        r#"{"schema_version":1,"platform":"github","host":"github.com","repository":"owner/repo","remote":"origin","base_branch":"main","timezone":"Asia/Shanghai","permissions":{"local_commit":true,"push":true,"pull_request":true},"github_project_url":"https://github.com/users/owner/projects/1"}"#,
    )
    .unwrap();
    let path = config.to_str().unwrap();
    let primary = run(&["config", "validate", "--file", path]);
    assert!(primary.status.success());
    assert!(primary.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&primary.stdout).unwrap()["valid"],
        true
    );

    let legacy = run(&["workflow", "validate", "--file", path]);
    assert!(legacy.status.success());
    serde_json::from_slice::<Value>(&legacy.stdout).unwrap();
    assert!(String::from_utf8_lossy(&legacy.stderr).contains("deprecated"));
}

#[test]
fn delivery_contract_validation_is_offline() {
    let dir = tempfile::tempdir().unwrap();
    let contract = dir.path().join("contract.json");
    fs::write(
        &contract,
        r#"{"schema_version":1,"issue_url":"https://github.com/owner/repo/issues/1","parent_issue_url":null,"source_branch":"main","branch":"refactor/command_groups","pr_target":"main","pr_url":null}"#,
    )
    .unwrap();
    let output = run(&[
        "delivery",
        "validate-contract",
        "--file",
        contract.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["valid"],
        true
    );
}

#[test]
fn public_help_uses_cross_platform_pr_and_hides_workflow() {
    let output = Command::new(env!("CARGO_BIN_EXE_issueflow"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("GitHub PRs and same-project GitLab MRs"));
    assert!(help.contains("delivery"));
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("workflow "))
    );
}
