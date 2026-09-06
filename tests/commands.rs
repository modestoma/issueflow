use serde_json::Value;
use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_issueflow"))
}

#[test]
fn issue_help_exposes_relationship_commands_and_hides_compatibility_names() {
    let output = binary().args(["issue", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for name in [
        "relationships",
        "parent",
        "sub-issues",
        "add-sub-issue",
        "remove-sub-issue",
        "remove-parent",
        "move-sub-issue",
        "blocked-by",
        "blocking",
    ] {
        assert!(stdout.contains(name), "missing {name} in {stdout}");
    }
    assert!(!stdout.contains("dependencies"));

    let top = binary().arg("--help").output().unwrap();
    assert!(top.status.success());
    assert!(!String::from_utf8(top.stdout).unwrap().contains("hierarchy"));
}

#[test]
fn capabilities_exclude_hidden_compatibility_commands() {
    let output = binary().arg("capabilities").output().unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let top_level = value["cli"]["subcommands"].as_array().unwrap();
    let top_level_names = top_level
        .iter()
        .filter_map(|command| command["name"].as_str())
        .collect::<Vec<_>>();
    assert!(!top_level_names.contains(&"hierarchy"));
    let issue = top_level
        .iter()
        .find(|command| command["name"] == "issue")
        .unwrap();
    let issue_names = issue["subcommands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|command| command["name"].as_str())
        .collect::<Vec<_>>();
    assert!(!issue_names.contains(&"dependencies"));
    assert!(issue_names.contains(&"relationships"));
    assert!(issue_names.contains(&"sub-issues"));
}

#[test]
fn hierarchy_alias_is_invocable_and_warns_only_on_stderr() {
    let output = binary()
        .args([
            "--no-env-file",
            "--platform",
            "github",
            "--repository",
            "owner/repo",
            "hierarchy",
            "parent",
            "invalid",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("warning: `hierarchy` is deprecated"));
}

#[test]
fn move_sub_issue_requires_exactly_one_relative_position() {
    for args in [
        vec![
            "issue",
            "move-sub-issue",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/issues/2",
        ],
        vec![
            "issue",
            "move-sub-issue",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/issues/2",
            "--before",
            "https://github.com/o/r/issues/3",
            "--after",
            "https://github.com/o/r/issues/4",
        ],
    ] {
        let output = binary().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}
