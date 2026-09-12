//! Offline roadmap initialization, validation and advisory dependency scheduling.
use crate::error::{Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

fn require(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::new("input", message))
    }
}
fn present(s: &Option<String>) -> bool {
    s.as_ref().is_some_and(|s| !s.trim().is_empty())
}
fn key(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}
fn io(error: std::io::Error) -> Error {
    Error::new(
        "input",
        format!("Local roadmap file operation failed: {error}"),
    )
}
fn valid_url(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
    })
}
fn local_file(root: &Path, value: &str) -> Result<PathBuf> {
    let relative = Path::new(value);
    require(
        !value.is_empty()
            && !value.contains('\\')
            && !relative.is_absolute()
            && !relative
                .components()
                .any(|c| matches!(c, Component::ParentDir)),
        "Document must be a contained relative path",
    )?;
    let path = root.join(relative).canonicalize().map_err(io)?;
    require(
        path.starts_with(root.canonicalize().map_err(io)?) && path.is_file(),
        "Document missing or escapes roadmap directory",
    )?;
    Ok(path)
}

#[derive(Deserialize)]
pub struct Plan {
    pub schema_version: u64,
    pub id: String,
    pub revision: u64,
    pub status: String,
    pub base_branch: String,
    pub approved_revision: Option<u64>,
    pub approval_ref: Option<String>,
    pub max_parallel: usize,
    pub sync_pending: bool,
    pub pending_questions: Vec<Question>,
    pub tasks: Vec<Task>,
    pub acceptance: Option<Acceptance>,
}
#[derive(Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    pub tasks: Vec<String>,
}
#[derive(Deserialize)]
pub struct Acceptance {
    pub revision: u64,
    pub delivery_sha: String,
    pub user_confirmation: String,
}
#[derive(Deserialize)]
pub struct Delivery {
    pub target: String,
    pub merge_sha: String,
    pub verified_at: String,
    pub evidence: String,
    pub issue_closed: bool,
}
#[derive(Deserialize)]
pub struct Task {
    pub key: String,
    pub document: String,
    pub status: String,
    pub reviewed_revision: u64,
    pub depends_on: Vec<String>,
    pub resources: Vec<String>,
    pub sync_pending: bool,
    pub issue_url: Option<String>,
    pub branch: Option<String>,
    pub owner: Option<String>,
    pub start_sha: Option<String>,
    pub pr_url: Option<String>,
    pub delivery: Option<Delivery>,
    pub decision_ref: Option<String>,
}
impl Task {
    fn active(&self) -> bool {
        matches!(self.status.as_str(), "running" | "review" | "blocked")
    }
}
impl Plan {
    pub fn load(root: &Path) -> Result<Self> {
        let path = local_file(root, "plan.json")?;
        let value: Self = serde_json::from_str(&fs::read_to_string(path).map_err(io)?)
            .map_err(|e| Error::new("input", format!("Invalid roadmap plan: {e}")))?;
        value.validate(root)?;
        Ok(value)
    }
    pub fn validate(&self, root: &Path) -> Result<Value> {
        require(self.schema_version == 1, "Unsupported schema_version")?;
        require(key(&self.id), "Invalid roadmap id")?;
        require(!self.base_branch.trim().is_empty(), "base_branch required")?;
        require(
            self.revision > 0 && self.max_parallel > 0,
            "revision/max_parallel must be positive",
        )?;
        require(
            matches!(
                self.status.as_str(),
                "draft" | "approved" | "running" | "paused" | "awaiting_acceptance" | "accepted"
            ),
            "Invalid roadmap status",
        )?;
        for file in ["roadmap.md", "decisions.md", "acceptance.md"] {
            local_file(root, file)?;
        }
        if !matches!(self.status.as_str(), "draft" | "paused") {
            require(
                self.approved_revision == Some(self.revision) && present(&self.approval_ref),
                "Current revision needs recorded user approval",
            )?;
            require(!self.tasks.is_empty(), "Non-draft roadmap needs tasks")?;
        }
        let mut index = BTreeMap::new();
        let mut urls = BTreeSet::new();
        let mut branches = BTreeSet::new();
        let mut documents = BTreeSet::new();
        for task in &self.tasks {
            require(key(&task.key), "Invalid task key")?;
            require(
                index.insert(task.key.as_str(), task).is_none(),
                format!("Duplicate task key: {}", task.key),
            )?;
            require(
                matches!(
                    task.status.as_str(),
                    "planned" | "running" | "blocked" | "review" | "merged" | "cancelled"
                ),
                "Invalid task status",
            )?;
            require(
                task.reviewed_revision > 0 && task.reviewed_revision <= self.revision,
                "Invalid reviewed_revision",
            )?;
            for values in [&task.depends_on, &task.resources] {
                require(
                    values.iter().all(|v| !v.trim().is_empty())
                        && values.iter().collect::<BTreeSet<_>>().len() == values.len(),
                    "Empty or duplicate dependency/resource",
                )?;
            }
            require(
                !task.resources.is_empty(),
                "Explicit resources or '*' required",
            )?;
            for scope in &task.resources {
                require(
                    scope == "*"
                        || (!scope.starts_with('/')
                            && !scope.contains(['*', '?', '[', ']', '\\'])
                            && scope
                                .trim_end_matches('/')
                                .split('/')
                                .all(|s| !s.is_empty() && s != "." && s != "..")),
                    "Invalid resource scope; use directory prefixes, not globs",
                )?;
            }
            let doc = local_file(root, &task.document)?;
            require(
                task.document.starts_with("issues/")
                    && task.document.ends_with(".md")
                    && documents.insert(doc),
                "Task document must be a unique issues/*.md path",
            )?;
            if let Some(url) = &task.issue_url {
                require(
                    valid_url(url) && urls.insert(url),
                    "Invalid or duplicate issue URL",
                )?;
            }
            if task.active() || task.status == "merged" {
                require(
                    present(&task.issue_url),
                    "Execution requires published issue_url",
                )?;
                require(
                    present(&task.branch) && task.branch.as_deref() != Some(&self.base_branch),
                    "Independent branch required",
                )?;
                require(
                    branches.insert(task.branch.as_ref().unwrap()),
                    "Duplicate execution branch",
                )?;
            }
            if matches!(task.status.as_str(), "running" | "review") {
                require(
                    present(&task.owner) && present(&task.start_sha),
                    "Execution owner/start_sha required",
                )?;
            }
            if matches!(task.status.as_str(), "review" | "merged") {
                require(
                    task.pr_url.as_deref().is_some_and(valid_url),
                    "PR/MR URL required",
                )?;
            }
            if task.status == "merged" {
                require(
                    task.delivery.as_ref().is_some_and(|d| {
                        d.target == self.base_branch
                            && d.issue_closed
                            && !d.merge_sha.trim().is_empty()
                            && !d.verified_at.trim().is_empty()
                            && !d.evidence.trim().is_empty()
                    }),
                    "Merged task needs verified target/SHA/evidence/issue close",
                )?;
            }
            if task.status == "cancelled" {
                require(
                    present(&task.decision_ref),
                    "Cancellation needs user decision reference",
                )?;
            }
        }
        for task in &self.tasks {
            for dep in &task.depends_on {
                require(
                    dep != &task.key && index.contains_key(dep.as_str()),
                    format!("Missing or self dependency: {dep}"),
                )?;
            }
        }
        let mut remaining: BTreeMap<&str, BTreeSet<&str>> = self
            .tasks
            .iter()
            .map(|t| {
                (
                    t.key.as_str(),
                    t.depends_on.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        while !remaining.is_empty() {
            let leaves: BTreeSet<&str> = remaining
                .iter()
                .filter(|(_, d)| d.is_empty())
                .map(|(k, _)| *k)
                .collect();
            require(!leaves.is_empty(), "Dependency cycle")?;
            remaining.retain(|k, _| !leaves.contains(k));
            for deps in remaining.values_mut() {
                deps.retain(|d| !leaves.contains(d));
            }
        }
        let mut question_ids = BTreeSet::new();
        for q in &self.pending_questions {
            require(
                !q.id.trim().is_empty() && !q.text.trim().is_empty() && question_ids.insert(&q.id),
                "Invalid or duplicate pending question",
            )?;
            require(
                q.tasks.iter().all(|k| index.contains_key(k.as_str())),
                "Invalid question task scope",
            )?;
        }
        if matches!(self.status.as_str(), "awaiting_acceptance" | "accepted") {
            require(
                self.tasks.iter().all(|t| {
                    matches!(t.status.as_str(), "merged" | "cancelled") && !t.sync_pending
                }),
                "Unfinished tasks cannot enter acceptance",
            )?;
            require(
                self.pending_questions.is_empty() && !self.sync_pending,
                "Unresolved questions/sync before acceptance",
            )?;
        }
        if self.status == "accepted" {
            require(
                self.acceptance.as_ref().is_some_and(|a| {
                    a.revision == self.revision
                        && !a.delivery_sha.trim().is_empty()
                        && !a.user_confirmation.trim().is_empty()
                }),
                "Accepted needs user confirmation and delivery SHA",
            )?;
        }
        Ok(json!({"valid":true,"tasks":self.tasks.len(),"revision":self.revision}))
    }
    fn affected(&self, seeds: &[String]) -> Result<BTreeSet<String>> {
        require(
            seeds.iter().all(|s| self.tasks.iter().any(|t| &t.key == s)),
            "Unknown impact seed",
        )?;
        let mut affected: BTreeSet<String> = seeds.iter().cloned().collect();
        loop {
            let more: Vec<String> = self
                .tasks
                .iter()
                .filter(|t| t.depends_on.iter().any(|d| affected.contains(d)))
                .map(|t| t.key.clone())
                .collect();
            let before = affected.len();
            affected.extend(more);
            if affected.len() == before {
                return Ok(affected);
            }
        }
    }
    pub fn impact(&self, seeds: &[String]) -> Result<Value> {
        require(!seeds.is_empty(), "Impact requires task seeds")?;
        let keys = self.affected(seeds)?;
        let affected: Vec<Value> = keys
            .iter()
            .map(|key| {
                let task = self.tasks.iter().find(|t| &t.key == key).unwrap();
                json!({"key":key,"status":task.status})
            })
            .collect();
        Ok(
            json!({"affected":affected,"advisory":"Also inspect shared contracts and non-DAG effects; this command changes nothing."}),
        )
    }
    pub fn ready(&self) -> Result<Value> {
        if !matches!(self.status.as_str(), "approved" | "running") || self.sync_pending {
            return Ok(
                json!({"ready":[],"reason":"roadmap is not executable or synchronization is pending"}),
            );
        }
        let index: BTreeMap<&str, &Task> = self.tasks.iter().map(|t| (t.key.as_str(), t)).collect();
        let mut blocked = BTreeSet::new();
        for q in &self.pending_questions {
            if q.tasks.is_empty() {
                blocked.extend(self.tasks.iter().map(|t| t.key.clone()));
            } else {
                blocked.extend(self.affected(&q.tasks)?);
            }
        }
        let active: Vec<&Task> = self.tasks.iter().filter(|t| t.active()).collect();
        let capacity = self.max_parallel.saturating_sub(active.len());
        let mut occupied: Vec<&[String]> = active.iter().map(|t| t.resources.as_slice()).collect();
        let mut selected = Vec::new();
        let mut excluded = BTreeMap::new();
        for task in self.tasks.iter().filter(|t| t.status == "planned") {
            let reason = if blocked.contains(&task.key) {
                Some("pending user decision or affected dependency")
            } else if task.reviewed_revision != self.revision {
                Some("stale roadmap revision")
            } else if !present(&task.issue_url) || task.sync_pending {
                Some("issue is unpublished or synchronization is pending")
            } else if task
                .depends_on
                .iter()
                .any(|d| index[d.as_str()].status != "merged" || index[d.as_str()].sync_pending)
            {
                Some("dependencies are not verified delivered")
            } else if occupied.iter().any(|r| overlaps(&task.resources, r)) {
                Some("resource conflict with active or selected task")
            } else if selected.len() >= capacity {
                Some("parallel capacity exhausted")
            } else {
                None
            };
            if let Some(reason) = reason {
                excluded.insert(&task.key, reason);
            } else {
                selected.push(&task.key);
                occupied.push(&task.resources);
            }
        }
        Ok(
            json!({"ready":selected,"excluded":excluded,"advisory":"Recheck live platform state and baseline before the single coordinator dispatches."}),
        )
    }
}
fn overlaps(left: &[String], right: &[String]) -> bool {
    left.iter().any(|a| {
        right.iter().any(|b| {
            let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
            a == "*"
                || b == "*"
                || a == b
                || a.starts_with(&format!("{b}/"))
                || b.starts_with(&format!("{a}/"))
        })
    })
}

pub fn init(repo: &Path, id: &str, base: &str, title: &str) -> Result<Value> {
    require(key(id), "Invalid roadmap id")?;
    require(!title.trim().is_empty(), "Roadmap title required")?;
    let repo = repo.canonicalize().map_err(io)?;
    let git = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(io)?;
    require(git.status.success(), "--repo must be a Git repository root")?;
    let top = String::from_utf8(git.stdout)
        .map_err(|_| Error::new("input", "Git root path is not UTF-8"))?;
    require(
        Path::new(top.trim()).canonicalize().map_err(io)? == repo,
        "--repo must be a Git repository root",
    )?;
    let check = Command::new("git")
        .args(["check-ref-format", &format!("refs/heads/{base}")])
        .output()
        .map_err(io)?;
    require(
        check.status.success() && !base.starts_with('-') && base != "HEAD",
        "Invalid base branch",
    )?;
    let parent = repo.join(".agents/roadmaps");
    let root = parent.join(id);
    for p in [repo.join(".agents"), parent.clone(), root.clone()] {
        if let Ok(meta) = fs::symlink_metadata(&p) {
            require(
                !meta.file_type().is_symlink(),
                "Refusing symlinked roadmap directories",
            )?;
        }
    }
    require(
        !root.exists(),
        "Roadmap already exists; resume without overwriting",
    )?;
    fs::create_dir_all(&parent).map_err(io)?;
    fs::create_dir(&root).map_err(io)?;
    fs::create_dir(root.join("issues")).map_err(io)?;
    for (name, template) in [
        ("roadmap.md", include_str!("roadmap_templates/roadmap.md")),
        (
            "decisions.md",
            include_str!("roadmap_templates/decisions.md"),
        ),
        (
            "acceptance.md",
            include_str!("roadmap_templates/acceptance.md"),
        ),
    ] {
        fs::write(
            root.join(name),
            template
                .replace("{{id}}", id)
                .replace("{{title}}", title)
                .replace("{{base}}", base),
        )
        .map_err(io)?;
    }
    let plan = json!({"schema_version":1,"id":id,"revision":1,"status":"draft","base_branch":base,
        "approved_revision":null,"approval_ref":null,"max_parallel":1,"milestone":null,
        "sync_pending":false,"pending_questions":[],"tasks":[]});
    fs::write(
        root.join("plan.json"),
        format!("{}\n", serde_json::to_string_pretty(&plan).unwrap()),
    )
    .map_err(io)?;
    Ok(
        json!({"directory":root,"status":"draft","next":"Fill the documents and obtain review; no remote action was performed."}),
    )
}
