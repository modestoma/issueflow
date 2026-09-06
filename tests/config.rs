use std::{collections::HashMap, fs, process::Command};

use issueflow::config::{Config, Overrides, Platform, read_env_file};

fn values(items: &[(&str, &str)]) -> HashMap<String, String> {
    items
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn precedence_is_flags_environment_file_defaults() {
    let file = values(&[
        ("ISSUEFLOW_PLATFORM", "gitlab"),
        ("ISSUEFLOW_TIMEOUT_SECONDS", "10"),
        ("ISSUEFLOW_GITHUB_TOKEN", "file-secret"),
    ]);
    let env = values(&[
        ("ISSUEFLOW_TIMEOUT_SECONDS", "20"),
        ("ISSUEFLOW_GITHUB_TOKEN", "env-secret"),
    ]);
    let config = Config::resolve(file.clone(), env.clone(), Overrides::default()).unwrap();
    assert_eq!(config.platform, Some(Platform::Gitlab));
    assert_eq!(config.timeout_seconds, 20);
    assert_eq!(config.github_token.as_ref().unwrap().expose(), "env-secret");
    assert_eq!(config.github_api_url.as_str(), "https://api.github.com/");
    let config = Config::resolve(
        file,
        env,
        Overrides {
            timeout_seconds: Some(40),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(config.timeout_seconds, 40);
}

#[test]
fn empty_environment_token_clears_file_token() {
    let config = Config::resolve(
        values(&[("ISSUEFLOW_GITHUB_TOKEN", "secret")]),
        values(&[("ISSUEFLOW_GITHUB_TOKEN", "")]),
        Overrides::default(),
    )
    .unwrap();
    assert!(config.github_token.is_none());
}

#[test]
fn secrets_are_not_in_json_or_debug() {
    let config = Config::resolve(
        values(&[("ISSUEFLOW_GITHUB_TOKEN", "unique-test-secret")]),
        HashMap::new(),
        Overrides::default(),
    )
    .unwrap();
    assert!(!config.redacted().to_string().contains("unique-test-secret"));
    assert!(!format!("{config:?}").contains("unique-test-secret"));
}

#[test]
fn malformed_values_fail_without_echoing_secrets() {
    for (key, value) in [
        (
            "ISSUEFLOW_GITHUB_API_URL",
            "https://user:unique-secret@example.com",
        ),
        ("ISSUEFLOW_TIMEOUT_SECONDS", "unique-secret"),
        ("ISSUEFLOW_PLATFORM", "unique-secret"),
        ("ISSUEFLOW_GITLAB_URL", "http://example.com"),
    ] {
        let error = Config::resolve(
            values(&[(key, value)]),
            HashMap::new(),
            Overrides::default(),
        )
        .unwrap_err();
        assert!(!error.to_string().contains("unique-secret"));
    }
}

#[test]
fn env_files_support_quotes_comments_and_literal_dollars() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    fs::write(
        &path,
        "# comment\nISSUEFLOW_GITHUB_TOKEN='literal$secret'\nISSUEFLOW_REPOSITORY=\"owner/repo\"\n",
    )
    .unwrap();
    let map = read_env_file(&path, true).unwrap();
    assert_eq!(map["ISSUEFLOW_GITHUB_TOKEN"], "literal$secret");
    assert_eq!(map["ISSUEFLOW_REPOSITORY"], "owner/repo");
}

#[test]
fn missing_explicit_file_and_duplicate_keys_fail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    assert!(read_env_file(&path, false).unwrap().is_empty());
    assert!(read_env_file(&path, true).is_err());
    fs::write(
        &path,
        "ISSUEFLOW_PLATFORM=github\nISSUEFLOW_PLATFORM=gitlab\n",
    )
    .unwrap();
    assert!(read_env_file(&path, true).is_err());
    fs::write(&path, "ISSUEFLOW_GITHUB_TOKEN=\"unique-secret\n").unwrap();
    assert!(
        !read_env_file(&path, true)
            .unwrap_err()
            .to_string()
            .contains("unique-secret")
    );
}

#[test]
fn cli_loads_explicit_file_and_accepts_environment_override() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("custom.env");
    fs::write(
        &path,
        "ISSUEFLOW_TIMEOUT_SECONDS=12\nISSUEFLOW_GITHUB_TOKEN=unique-secret\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_issueflow"))
        .env_clear()
        .current_dir(dir.path())
        .env("ISSUEFLOW_TIMEOUT_SECONDS", "15")
        .args(["--env-file", path.to_str().unwrap(), "config", "show"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["timeout_seconds"], 15);
    assert_eq!(json["github_token_configured"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("unique-secret"));
}

#[test]
fn cli_does_not_search_parents_and_can_skip_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".env"), "ISSUEFLOW_TIMEOUT_SECONDS=12").unwrap();
    let child = dir.path().join("child");
    fs::create_dir(&child).unwrap();
    for (cwd, args) in [
        (&child, vec!["config", "show"]),
        (
            &dir.path().to_path_buf(),
            vec!["--no-env-file", "config", "show"],
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_issueflow"))
            .env_clear()
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["timeout_seconds"], 30);
    }
}

#[test]
fn bare_config_remains_a_stderr_only_compatibility_alias() {
    let output = Command::new(env!("CARGO_BIN_EXE_issueflow"))
        .env_clear()
        .args(["--no-env-file", "config"])
        .output()
        .unwrap();
    assert!(output.status.success());
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("deprecated"));
}
