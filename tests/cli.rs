use std::process::Command;

fn issueflow() -> Command {
    Command::new(env!("CARGO_BIN_EXE_issueflow"))
}

#[test]
fn root_help_exposes_kanban_and_hides_compatibility_aliases() {
    let output = issueflow().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("kanban"));
    assert!(!stdout.contains("setup-labels"));
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("board "))
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("project "))
    );
}

#[test]
fn capability_discovery_excludes_hidden_compatibility_aliases() {
    let output = issueflow().arg("capabilities").output().unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<_> = value["cli"]["subcommands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|command| command["name"].as_str())
        .collect();
    assert!(names.contains(&"kanban"));
    assert!(!names.contains(&"project"));
    assert!(!names.contains(&"board"));
    assert!(!names.contains(&"setup-labels"));
}

#[test]
fn kanban_help_exposes_the_agreed_command_set() {
    let output = issueflow().args(["kanban", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for command in [
        "list",
        "show",
        "create",
        "init",
        "items",
        "add",
        "status",
        "field",
        "repositories",
        "link-repository",
    ] {
        assert!(
            stdout
                .lines()
                .any(|line| line.trim_start().starts_with(command))
        );
    }
}

#[test]
fn hidden_alias_emits_a_stderr_only_deprecation_warning() {
    let output = issueflow()
        .args([
            "--no-env-file",
            "--platform",
            "gitlab",
            "--repository",
            "group/repo",
            "setup-labels",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.is_empty());
    assert!(stderr.contains("warning: `setup-labels` is deprecated"));
}

#[test]
fn unsupported_gitlab_field_fails_before_authentication_or_api_access() {
    let output = issueflow()
        .args([
            "--no-env-file",
            "--platform",
            "gitlab",
            "--repository",
            "group/repo",
            "kanban",
            "field",
            "3",
            "https://gitlab.example.com/group/repo/-/issues/1",
            "--name",
            "Priority",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("kanban field is not supported for GitLab Issue Boards"));
    assert!(!stderr.contains("token"));
}

#[test]
fn github_init_requires_an_explicit_repository_for_linkage() {
    let output = issueflow()
        .args([
            "--no-env-file",
            "kanban",
            "init",
            "https://github.com/users/modestoma/projects/1",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("requires --repository for native linkage verification"));
    assert!(!stderr.contains("token"));
}
