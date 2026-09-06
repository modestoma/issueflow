use serde_json::Value;
use std::process::Command;

fn issueflow() -> Command {
    Command::new(env!("CARGO_BIN_EXE_issueflow"))
}

#[test]
fn global_output_flags_work_before_and_after_subcommands() {
    for args in [
        ["--json", "--verbose", "capabilities"],
        ["capabilities", "--verbose", "--json"],
    ] {
        let output = issueflow()
            .args(args)
            .env("ISSUEFLOW_GITHUB_TOKEN", "must-not-appear")
            .output()
            .unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<Value>(&output.stdout).unwrap();
        assert!(!String::from_utf8_lossy(&output.stdout).contains("must-not-appear"));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn piped_stdout_preserves_json_without_an_explicit_flag() {
    let output = issueflow().arg("capabilities").output().unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["capability_schema_version"], 1);
}

#[test]
fn non_terminal_errors_remain_structured_and_keep_exit_codes() {
    let output = issueflow()
        .args(["--no-env-file", "--platform", "github", "doctor"])
        .env_remove("ISSUEFLOW_GITHUB_TOKEN")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["error"]["code"], "configuration");
}
