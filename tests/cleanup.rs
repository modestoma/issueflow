use issueflow::{
    branch_contract::BranchContract,
    cleanup::{LocalState, blockers, inspect_local},
    workflow_config::WorkflowConfig,
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
fn workflow() -> WorkflowConfig {
    serde_json::from_value(json!({"schema_version":1,"platform":"github","host":"github.com","repository":"a/b","remote":"origin","base_branch":"main","timezone":"Asia/Shanghai","permissions":{"local_commit":true,"push":true},"delivery_policy":"merged"})).unwrap()
}
fn contract() -> BranchContract {
    serde_json::from_value(json!({"schema_version":1,"issue_url":"https://github.com/a/b/issues/1","source_branch":"main","branch":"feat/issue-1","pr_target":"main"})).unwrap()
}
fn local() -> LocalState {
    LocalState {
        path: "/tmp/example".into(),
        branch: "feat/issue-1".into(),
        head_sha: "a".repeat(40),
        primary: false,
        locked: false,
        remote_matches: true,
        tracked_changes: false,
        untracked_files: false,
        ignored_paths: vec![],
    }
}
#[test]
fn clean_delivered_worktree_requires_dependency_confirmation() {
    let l = local();
    assert!(
        blockers(&l, &contract(), Some(&l.head_sha), true, 0, false)
            .contains(&"dependent_work_not_confirmed")
    );
    assert!(blockers(&l, &contract(), Some(&l.head_sha), true, 0, true).is_empty());
}
#[test]
fn dirty_ignored_or_dependent_work_is_never_eligible() {
    let mut l = local();
    l.tracked_changes = true;
    l.untracked_files = true;
    l.ignored_paths = vec![".env".into()];
    let b = blockers(&l, &contract(), Some(&l.head_sha), true, 1, true);
    for name in [
        "tracked_changes",
        "untracked_files",
        "ignored_files_require_review",
        "open_dependent_pull_requests",
    ] {
        assert!(b.contains(&name));
    }
}
#[test]
fn primary_locked_and_unreviewed_commits_are_blocked() {
    let mut l = local();
    l.primary = true;
    l.locked = true;
    let b = blockers(&l, &contract(), Some(&"b".repeat(40)), false, 0, true);
    assert!(b.contains(&"primary_worktree"));
    assert!(b.contains(&"locked_worktree"));
    assert!(b.contains(&"local_head_differs_from_reviewed_pr"));
    assert!(b.contains(&"remote_delivery_not_complete"));
}
fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Issueflow Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
        ])
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let wt = temp.path().join("worktree");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "--initial-branch=main"]);
    git(
        &root,
        &["remote", "add", "origin", "git@github.com:a/b.git"],
    );
    fs::write(root.join("README"), "tracked").unwrap();
    fs::write(root.join(".gitignore"), ".env\n/target\n").unwrap();
    git(&root, &["add", "README", ".gitignore"]);
    git(&root, &["commit", "-m", "Fixture"]);
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "feat/issue-1",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    (temp, root, wt)
}
#[test]
fn inspects_real_git_without_deleting_files() {
    let (_temp, root, wt) = fixture();
    let l = inspect_local(&wt, &workflow()).unwrap();
    assert!(!l.primary && l.remote_matches && !l.tracked_changes);
    assert_eq!(l.branch, "feat/issue-1");
    fs::write(wt.join("README"), "changed").unwrap();
    fs::write(wt.join("untracked.txt"), "keep").unwrap();
    fs::write(wt.join(".env"), "PRIVATE_FIXTURE=keep").unwrap();
    let l = inspect_local(&wt, &workflow()).unwrap();
    assert!(l.tracked_changes && l.untracked_files);
    assert!(l.ignored_paths.iter().any(|p| p == ".env"));
    assert_eq!(
        fs::read_to_string(wt.join(".env")).unwrap(),
        "PRIVATE_FIXTURE=keep"
    );
    assert!(inspect_local(&root, &workflow()).unwrap().primary);
}
#[test]
fn locked_worktree_and_nested_paths_are_detected() {
    let (_temp, root, wt) = fixture();
    git(&root, &["worktree", "lock", wt.to_str().unwrap()]);
    assert!(inspect_local(&wt, &workflow()).unwrap().locked);
    fs::create_dir(wt.join("nested")).unwrap();
    assert!(inspect_local(&wt.join("nested"), &workflow()).is_err());
}
#[test]
fn remote_credentials_are_not_in_report() {
    let (_temp, _root, wt) = fixture();
    git(
        &wt,
        &[
            "remote",
            "set-url",
            "origin",
            "https://fixture-secret@github.com/a/b.git",
        ],
    );
    let l = inspect_local(&wt, &workflow()).unwrap();
    assert!(l.remote_matches);
    assert!(
        !serde_json::to_string(&l)
            .unwrap()
            .contains("fixture-secret")
    );
    git(
        &wt,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/a/b.git.git",
        ],
    );
    assert!(!inspect_local(&wt, &workflow()).unwrap().remote_matches);
}

struct CleanupRemote {
    head: String,
    merged: bool,
    reachable: bool,
    dependent: bool,
}
#[async_trait::async_trait]
impl issueflow::transport::Transport for CleanupRemote {
    async fn request(
        &self,
        method: http::Method,
        endpoint: &str,
        body: Option<serde_json::Value>,
    ) -> issueflow::error::Result<serde_json::Value> {
        if endpoint == "graphql" {
            assert_eq!(method, http::Method::POST);
            let body = body.unwrap();
            let query = body["query"].as_str().unwrap();
            assert!(query.starts_with("query"), "cleanup must never mutate");
            return Ok(if query.contains("owner:user") {
                json!({"data":{"owner":{"projectV2":{"id":"P1","closed":false,"title":"Board"}}}})
            } else if query.contains("fields(first:") {
                json!({"data":{"node":{"fields":{"nodes":[
                    {"id":"S","name":"Status","options":[{"id":"D","name":"Done"}]},
                    {"id":"T","name":"Work type","options":[]},
                    {"id":"P","name":"Priority","options":[]},
                    {"id":"B","name":"Blocked","options":[]},
                    {"id":"R","name":"Resolution","options":[]}
                ],"pageInfo":{"hasNextPage":false}}}}})
            } else if query.contains("items(first:") {
                json!({"data":{"node":{"items":{"nodes":[{
                    "id":"ITEM1","content":{"id":"I1"},"isArchived":false,
                    "fieldValueByName":{"name":"In review"},"resolution":null,
                    "blocked":{"name":"No"}
                }],"pageInfo":{"hasNextPage":false}}}}})
            } else {
                panic!("unexpected query: {query}");
            });
        }
        assert_eq!(method, http::Method::GET, "cleanup must only read");
        assert!(body.is_none());
        Ok(if endpoint == "repos/a/b/issues/1" {
            json!({"id":1,"node_id":"I1","number":1,"html_url":"https://github.com/a/b/issues/1","state":"open","state_reason":null,"labels":[]})
        } else if endpoint == "repos/a/b/pulls/2" {
            json!({"number":2,"html_url":"https://github.com/a/b/pull/2",
                "state":if self.merged {"closed"} else {"open"},
                "merged":self.merged,"draft":false,
                "head":{"sha":self.head,"ref":"feat/issue-1","repo":{"full_name":"a/b"}},
                "base":{"ref":"main"},"merge_commit_sha":"b".repeat(40),
                "body":"Refs https://github.com/a/b/issues/1"})
        } else if endpoint.starts_with("repos/a/b/compare/") {
            json!({"status":if self.reachable {"ahead"} else {"diverged"},
                "merge_base_commit":{"sha":"b".repeat(40)}})
        } else if endpoint.starts_with("repos/a/b/pulls?") {
            if self.dependent {
                json!([{"id":3,"number":3}])
            } else {
                json!([])
            }
        } else {
            panic!("unexpected endpoint: {endpoint}");
        })
    }
}

#[tokio::test]
async fn cleanup_is_independent_of_acceptance_closure_and_project_sync() {
    use issueflow::{
        config::{Config, Overrides},
        recovery::Recovery,
        workflow_config::DeliveryPolicy,
    };
    let (_temp, _root, wt) = fixture();
    let head = inspect_local(&wt, &workflow()).unwrap().head_sha;
    let remote = CleanupRemote {
        head,
        merged: true,
        reachable: true,
        dependent: false,
    };
    let config =
        Config::resolve(Default::default(), Default::default(), Overrides::default()).unwrap();
    let mut workflow = workflow();
    workflow.github_project_url = Some("https://github.com/users/a/projects/1".into());
    let mut contract = contract();
    contract.pr_url = Some("https://github.com/a/b/pull/2".into());
    for (policy, accepted, phase) in [
        (
            DeliveryPolicy::AcceptanceRequired,
            false,
            "acceptance_pending",
        ),
        (
            DeliveryPolicy::AcceptanceRequired,
            true,
            "reconciliation_needed",
        ),
        (DeliveryPolicy::Merged, false, "reconciliation_needed"),
    ] {
        workflow.delivery_policy = policy;
        let recovery = Recovery {
            config: &config,
            transport: &remote,
            contract: &contract,
            parent: None,
            workflow: &workflow,
        };
        let result = issueflow::cleanup::inspect(&recovery, &wt, true, accepted)
            .await
            .unwrap();
        assert_eq!(result["eligible"], true, "{result}");
        assert_eq!(result["remote_plan"]["phase"], phase);
        assert_eq!(result["remote_snapshot"]["issue_state"], "open");
        assert_eq!(result["remote_snapshot"]["project_status"], "In review");
        assert_eq!(result["deleted"], false);
        assert!(wt.join("README").exists());
    }
}

#[tokio::test]
async fn incomplete_merge_or_dependent_work_still_blocks_cleanup() {
    use issueflow::{
        config::{Config, Overrides},
        recovery::Recovery,
    };
    let (_temp, _root, wt) = fixture();
    let head = inspect_local(&wt, &workflow()).unwrap().head_sha;
    let config =
        Config::resolve(Default::default(), Default::default(), Overrides::default()).unwrap();
    let workflow = workflow();
    let mut contract = contract();
    contract.pr_url = Some("https://github.com/a/b/pull/2".into());
    for (merged, reachable, dependent, expected) in [
        (false, true, false, "remote_delivery_not_complete"),
        (true, false, false, "remote_delivery_not_complete"),
        (true, true, true, "open_dependent_pull_requests"),
    ] {
        let remote = CleanupRemote {
            head: head.clone(),
            merged,
            reachable,
            dependent,
        };
        let recovery = Recovery {
            config: &config,
            transport: &remote,
            contract: &contract,
            parent: None,
            workflow: &workflow,
        };
        let result = issueflow::cleanup::inspect(&recovery, &wt, true, false)
            .await
            .unwrap();
        assert_eq!(result["eligible"], false);
        assert!(
            result["blockers"]
                .as_array()
                .unwrap()
                .contains(&json!(expected)),
            "{result}"
        );
        assert_eq!(result["deleted"], false);
    }
}
