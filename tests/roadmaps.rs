use issueflow::roadmap::{self, Plan};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};
use tempfile::TempDir;
struct Fixture {
    repo: TempDir,
    root: PathBuf,
    data: Value,
}
impl Fixture {
    fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(repo.path())
                .status()
                .unwrap()
                .success()
        );
        roadmap::init(repo.path(), "demo", "develop", "Demo").unwrap();
        let root = repo.path().join(".agents/roadmaps/demo");
        let data =
            serde_json::from_str(&fs::read_to_string(root.join("plan.json")).unwrap()).unwrap();
        Self { repo, root, data }
    }
    fn task(&mut self, key: &str, deps: &[&str], resources: &[&str]) {
        fs::write(self.root.join(format!("issues/{key}.md")), "# Task\n").unwrap();
        let number = self.data["tasks"].as_array().unwrap().len() + 1;
        self.data["tasks"].as_array_mut().unwrap().push(json!({"key":key,"document":format!("issues/{key}.md"),"status":"planned","reviewed_revision":1,"depends_on":deps,"resources":resources,"sync_pending":false,"issue_url":format!("https://github.com/o/r/issues/{number}")}));
    }
    fn approve(&mut self) {
        self.data["status"] = "approved".into();
        self.data["approved_revision"] = 1.into();
        self.data["approval_ref"] = "decisions.md#approval".into();
        self.data["max_parallel"] = 3.into();
    }
    fn merged(&mut self, n: usize) {
        let t = &mut self.data["tasks"][n];
        t["status"] = "merged".into();
        t["branch"] = format!("codex/task-{n}").into();
        t["pr_url"] = "https://github.com/o/r/pull/20".into();
        t["delivery"] = json!({"target":"develop","merge_sha":"a".repeat(40),"verified_at":"2026-09-12T12:00:00Z","issue_closed":true,"evidence":"issues/task.md"});
    }
    fn save(&self) {
        fs::write(
            self.root.join("plan.json"),
            serde_json::to_vec(&self.data).unwrap(),
        )
        .unwrap();
    }
    fn load(&self) -> issueflow::error::Result<Plan> {
        self.save();
        Plan::load(&self.root)
    }
    fn ready(&self) -> Value {
        self.load().unwrap().ready().unwrap()["ready"].clone()
    }
}
#[test]
fn draft_initializes_and_refuses_overwrite() {
    let f = Fixture::new();
    assert_eq!(f.ready(), json!([]));
    assert!(roadmap::init(f.repo.path(), "demo", "main", "Overwrite").is_err());
    assert!(
        fs::read_to_string(f.root.join("roadmap.md"))
            .unwrap()
            .contains("Demo")
    );
}
#[test]
fn dependencies_release_only_after_verified_merge() {
    let mut f = Fixture::new();
    f.task("a", &[], &["src/a"]);
    f.task("b", &["a"], &["src/b"]);
    f.task("c", &["a"], &["src/c"]);
    f.task("d", &["b", "c"], &["src/d"]);
    f.approve();
    assert_eq!(f.ready(), json!(["a"]));
    f.merged(0);
    assert_eq!(f.ready(), json!(["b", "c"]));
    f.merged(1);
    assert_eq!(f.ready(), json!(["c"]));
    f.merged(2);
    assert_eq!(f.ready(), json!(["d"]));
}
#[test]
fn cancelled_dependency_does_not_release() {
    let mut f = Fixture::new();
    f.task("a", &[], &["src/a"]);
    f.task("b", &["a"], &["src/b"]);
    f.approve();
    f.data["tasks"][0]["status"] = "cancelled".into();
    assert!(f.load().is_err());
    f.data["tasks"][0]["decision_ref"] = "decisions.md#cancel".into();
    assert_eq!(f.ready(), json!([]));
}
#[test]
fn cycles_missing_dependencies_and_duplicates_fail() {
    let mut f = Fixture::new();
    f.task("a", &["b"], &["src/a"]);
    f.task("b", &["a"], &["src/b"]);
    assert!(f.load().err().unwrap().message.contains("cycle"));
    f.data["tasks"][0]["depends_on"] = json!(["missing"]);
    assert!(f.load().is_err());
    f.data["tasks"][0]["depends_on"] = json!([]);
    f.data["tasks"][1]["key"] = "a".into();
    assert!(f.load().is_err());
}
#[test]
fn resource_prefixes_capacity_and_active_work() {
    let mut f = Fixture::new();
    f.task("a", &[], &["src/api"]);
    f.task("b", &[], &["src/api/types.rs"]);
    f.task("c", &[], &["src/ui"]);
    f.approve();
    assert_eq!(f.ready(), json!(["a", "c"]));
    let a = &mut f.data["tasks"][0];
    a["status"] = "running".into();
    a["owner"] = "worker-a".into();
    a["start_sha"] = "a".repeat(40).into();
    a["branch"] = "codex/a".into();
    f.data["max_parallel"] = 2.into();
    assert_eq!(f.ready(), json!(["c"]));
    f.data["tasks"][0]["resources"] = json!(["*"]);
    assert_eq!(f.ready(), json!([]));
}
#[test]
fn questions_block_transitive_dependents_only() {
    let mut f = Fixture::new();
    f.task("a", &[], &["src/a"]);
    f.task("b", &["a"], &["src/b"]);
    f.task("c", &[], &["src/c"]);
    f.approve();
    f.data["pending_questions"] = json!([{"id":"q1","text":"Which approach?","tasks":["a"]}]);
    assert_eq!(f.ready(), json!(["c"]));
    let p = f.load().unwrap();
    assert_eq!(
        p.impact(&["a".into()]).unwrap()["affected"],
        json!([{"key":"a","status":"planned"},{"key":"b","status":"planned"}])
    );
    assert!(p.impact(&["missing".into()]).is_err());
    f.data["pending_questions"][0]["tasks"] = json!([]);
    assert_eq!(f.ready(), json!([]));
}
#[test]
fn approval_revision_and_sync_are_required() {
    let mut f = Fixture::new();
    f.task("a", &[], &["src/a"]);
    f.approve();
    f.data["revision"] = 2.into();
    assert!(f.load().is_err());
    f.data["approved_revision"] = 2.into();
    assert_eq!(f.ready(), json!([]));
    f.data["tasks"][0]["reviewed_revision"] = 2.into();
    assert_eq!(f.ready(), json!(["a"]));
    f.data["tasks"][0]["sync_pending"] = true.into();
    assert_eq!(f.ready(), json!([]));
    f.data["tasks"][0]["sync_pending"] = false.into();
    f.data["sync_pending"] = true.into();
    assert_eq!(f.ready(), json!([]));
}
#[test]
fn merged_and_accepted_require_evidence() {
    let mut f = Fixture::new();
    f.task("a", &[], &["src/a"]);
    f.approve();
    f.merged(0);
    f.data["tasks"][0]["delivery"]["target"] = "main".into();
    assert!(f.load().is_err());
    f.data["tasks"][0]["delivery"]["target"] = "develop".into();
    f.data["status"] = "accepted".into();
    assert!(f.load().is_err());
    f.data["acceptance"] =
        json!({"revision":1,"delivery_sha":"a".repeat(40),"user_confirmation":"User accepted"});
    assert!(f.load().is_ok());
}
#[test]
fn rejects_escape_symlink_and_alias_resources() {
    let mut f = Fixture::new();
    assert!(roadmap::init(f.repo.path(), "../escape", "main", "No").is_err());
    assert!(roadmap::init(f.repo.path(), "bad", "@{-1}", "No").is_err());
    f.task("a", &[], &["src/./api"]);
    assert!(f.load().is_err());
    f.data["tasks"][0]["resources"] = json!(["src/api"]);
    f.data["tasks"][0]["document"] = "../outside.md".into();
    assert!(f.load().is_err());
    #[cfg(unix)]
    {
        let outside = f.repo.path().join("outside.md");
        fs::write(&outside, "Outside").unwrap();
        std::os::unix::fs::symlink(outside, f.root.join("issues/link.md")).unwrap();
        f.data["tasks"][0]["document"] = "issues/link.md".into();
        assert!(f.load().is_err());
    }
}
#[test]
fn rejects_boolean_revision_and_missing_required_fields() {
    let mut f = Fixture::new();
    f.data["revision"] = true.into();
    assert!(f.load().is_err());
    f.data["revision"] = 1.into();
    f.data.as_object_mut().unwrap().remove("sync_pending");
    assert!(f.load().is_err());
}
#[test]
fn cli_offline_without_env_config_and_embedded_templates() {
    let temp = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .arg(temp.path())
        .status()
        .unwrap();
    let binary = env!("CARGO_BIN_EXE_issueflow");
    let output = Command::new(binary)
        .env("ISSUEFLOW_PLATFORM", "invalid")
        .args([
            "--json",
            "--env-file",
            "/missing/env",
            "roadmap",
            "init",
            "--repo",
        ])
        .arg(temp.path())
        .args([
            "--id",
            "offline",
            "--base",
            "develop",
            "--title",
            "$(not-a-command)",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let root = temp.path().join(".agents/roadmaps/offline");
    for verb in ["validate", "ready"] {
        let out = Command::new(binary)
            .args(["--json", "--env-file", "/missing/env", "roadmap", verb])
            .arg(&root)
            .env("ISSUEFLOW_GITLAB_URL", "invalid")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        fs::read_to_string(root.join("roadmap.md"))
            .unwrap()
            .contains("$(not-a-command)")
    );
}
