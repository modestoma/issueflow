use crate::{
    branch_contract::BranchContract,
    error::{Error, Result},
    pull::Pulls,
    recovery::Recovery,
    target::Target,
    workflow_config::WorkflowConfig,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
#[derive(Serialize, Debug)]
pub struct LocalState {
    pub path: String,
    pub branch: String,
    pub head_sha: String,
    pub primary: bool,
    pub locked: bool,
    pub remote_matches: bool,
    pub tracked_changes: bool,
    pub untracked_files: bool,
    pub ignored_paths: Vec<String>,
}
fn git(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(path)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|_| Error::new("local_git", "Unable to run Git for worktree inspection"))?;
    if !output.status.success() {
        return Err(Error::new(
            "local_git",
            "Git inspection failed; verify path, remote and worktree registration",
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| {
        Error::new(
            "local_git",
            "Non-UTF-8 Git output requires manual inspection",
        )
    })
}
fn remote_matches(remote: &str, host: &str, repo: &str) -> bool {
    let remote = remote.trim();
    let (actual_host, path) = if let Ok(url) = url::Url::parse(remote) {
        if !matches!(url.scheme(), "ssh" | "https") {
            return false;
        }
        (
            url.host_str().unwrap_or("").to_string(),
            url.path().to_string(),
        )
    } else if let Some((left, right)) = remote.split_once(':') {
        (
            left.rsplit('@').next().unwrap_or("").to_string(),
            right.to_string(),
        )
    } else {
        return false;
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    actual_host.eq_ignore_ascii_case(host) && path.eq_ignore_ascii_case(repo)
}
pub fn inspect_local(path: &Path, workflow: &WorkflowConfig) -> Result<LocalState> {
    workflow.validate()?;
    let path = path
        .canonicalize()
        .map_err(|_| Error::new("input", "Worktree path does not exist"))?;
    let root = git(&path, &["rev-parse", "--show-toplevel"])?;
    let root = Path::new(root.trim())
        .canonicalize()
        .map_err(|_| Error::new("local_git", "Cannot resolve worktree root"))?;
    if root != path {
        return Err(Error::new(
            "input",
            "Pass the exact worktree root, not a subdirectory",
        ));
    }
    let listing = git(&path, &["worktree", "list", "--porcelain", "-z"])?;
    let records: Vec<_> = listing
        .split("\0\0")
        .filter(|r| r.starts_with("worktree "))
        .collect();
    let same = |record: &str| {
        record
            .split('\0')
            .next()
            .and_then(|v| v.strip_prefix("worktree "))
            .and_then(|v| Path::new(v).canonicalize().ok())
            .is_some_and(|v| v == path)
    };
    let entry = records
        .iter()
        .position(|r| same(r))
        .ok_or_else(|| Error::new("input", "Directory is not a registered Git worktree"))?;
    let branch = git(&path, &["branch", "--show-current"])?;
    let head = git(&path, &["rev-parse", "--verify", "HEAD"])?;
    let remote = git(&path, &["remote", "get-url", &workflow.remote])?;
    let status = git(
        &path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    let mut tracked = false;
    let mut untracked = false;
    let mut ignored = Vec::new();
    // Rename/copy status records have one extra NUL-delimited source path.
    let mut skip_source = false;
    for record in status.split('\0').filter(|r| !r.is_empty()) {
        if skip_source {
            skip_source = false;
            continue;
        }
        if let Some(name) = record.strip_prefix("!! ") {
            ignored.push(name.to_string());
        } else if record.starts_with("?? ") {
            untracked = true;
        } else {
            tracked = true;
            skip_source = record.chars().take(2).any(|c| matches!(c, 'R' | 'C'));
        }
    }
    Ok(LocalState {
        path: path.to_string_lossy().into(),
        branch: branch.trim().into(),
        head_sha: head.trim().into(),
        primary: entry == 0,
        locked: records[entry]
            .split('\0')
            .any(|v| v == "locked" || v.starts_with("locked ")),
        remote_matches: remote_matches(&remote, &workflow.host, &workflow.repository),
        tracked_changes: tracked,
        untracked_files: untracked,
        ignored_paths: ignored,
    })
}
pub fn blockers(
    local: &LocalState,
    contract: &BranchContract,
    reviewed_sha: Option<&str>,
    remote_complete: bool,
    dependent_prs: usize,
    confirmed: bool,
) -> Vec<&'static str> {
    let mut b = Vec::new();
    if local.primary {
        b.push("primary_worktree");
    }
    if local.locked {
        b.push("locked_worktree");
    }
    if !local.remote_matches {
        b.push("remote_repository_mismatch");
    }
    if local.branch != contract.branch {
        b.push("branch_mismatch_or_detached_head");
    }
    if local.tracked_changes {
        b.push("tracked_changes");
    }
    if local.untracked_files {
        b.push("untracked_files");
    }
    if !local.ignored_paths.is_empty() {
        b.push("ignored_files_require_review");
    }
    if reviewed_sha != Some(local.head_sha.as_str()) {
        b.push("local_head_differs_from_reviewed_pr");
    }
    if !remote_complete {
        b.push("remote_delivery_not_complete");
    }
    if dependent_prs > 0 {
        b.push("open_dependent_pull_requests");
    }
    if !confirmed {
        b.push("dependent_work_not_confirmed");
    }
    b
}
pub async fn inspect(
    recovery: &Recovery<'_>,
    path: &Path,
    confirmed: bool,
    accepted: bool,
) -> Result<Value> {
    recovery.contract.validate(recovery.parent)?;
    let local = inspect_local(path, recovery.workflow)?;
    if !local.remote_matches || local.branch != recovery.contract.branch {
        return Ok(
            json!({"eligible":false,"local":local,"blockers":["local_contract_or_repository_mismatch"],"deleted":false,"note":"Remote evidence not requested for a mismatched local checkout."}),
        );
    }
    let (snapshot, plan) = recovery.inspect(accepted).await?;
    let target = Target::from_url(recovery.config, &recovery.contract.issue_url)?;
    let dependent = Pulls {
        transport: recovery.transport,
        target,
    }
    .list(None, Some(&recovery.contract.branch))
    .await?;
    let count = dependent
        .as_array()
        .ok_or_else(|| Error::new("response", "Invalid dependent PR list"))?
        .len();
    let reasons = blockers(
        &local,
        recovery.contract,
        snapshot.head_sha.as_deref(),
        plan.phase == "complete",
        count,
        confirmed,
    );
    Ok(
        json!({"eligible":reasons.is_empty(),"blockers":reasons,"local":local,"remote_snapshot":snapshot,"remote_plan":plan,"open_dependent_prs":dependent,"deleted":false,"note":"Read-only eligibility report, never a deletion command or authorization. Open PRs do not reveal unpublished child work; confirm child issue/worktree dependencies separately. Ignored files including build output and credentials require review. Recheck immediately before any explicitly authorized cleanup."}),
    )
}
