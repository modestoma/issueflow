use crate::{
    config::Platform,
    error::{Error, Result},
    pull::Pulls,
    service::Service,
    target::Target,
    transport::Transport,
};
use http::Method;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn bad() -> Error {
    Error::new("response", "Incomplete PR verification evidence")
}
pub async fn inspect(transport: &dyn Transport, target: Target) -> Result<Value> {
    let pulls = Pulls {
        transport,
        target: target.clone(),
    };
    let before = pulls.show().await?;
    if target.platform == Platform::Gitlab {
        return inspect_gitlab(transport, target, pulls, before).await;
    }
    let sha = before["head"]["sha"]
        .as_str()
        .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(bad)?;
    let root = format!("repos/{}/commits/{sha}", target.repository);
    let mut checks = Vec::new();
    let mut ids = BTreeSet::new();
    let mut complete = false;
    for page in 1..=1000 {
        let v = transport
            .request(
                Method::GET,
                &format!("{root}/check-runs?filter=latest&per_page=100&page={page}"),
                None,
            )
            .await?;
        let nodes = v["check_runs"].as_array().ok_or_else(bad)?;
        for n in nodes {
            let id = n["id"].as_u64().ok_or_else(bad)?;
            if !ids.insert(id) {
                return Err(Error::new("conflict", "Checks changed during pagination"));
            }
            checks.push(n.clone());
        }
        if nodes.len() < 100 {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(Error::new("response", "Check pagination limit exceeded"));
    }
    let service = Service {
        transport,
        target: target.clone(),
    };
    let statuses = service.pages(&format!("{root}/statuses")).await?;
    let reviews = service
        .pages(&format!(
            "repos/{}/pulls/{}/reviews",
            target.repository,
            target.number.ok_or_else(bad)?
        ))
        .await?;
    let after = pulls.show().await?;
    if before["head"]["sha"] != after["head"]["sha"]
        || before["base"]["ref"] != after["base"]["ref"]
        || before["base"]["sha"] != after["base"]["sha"]
        || before["base"]["repo"]["id"] != after["base"]["repo"]["id"]
    {
        return Err(Error::new(
            "conflict",
            "PR head or base changed while reading checks; inspect again",
        ));
    }
    summarize(sha, &checks, &statuses, &reviews)
}
async fn inspect_gitlab(
    transport: &dyn Transport,
    target: Target,
    pulls: Pulls<'_>,
    before: Value,
) -> Result<Value> {
    let sha = before["diff_refs"]["head_sha"]
        .as_str()
        .or_else(|| before["sha"].as_str())
        .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(bad)?;
    let iid = target.number.ok_or_else(bad)?;
    let root = format!(
        "projects/{}/merge_requests/{iid}",
        crate::target::encode(&target.repository)
    );
    let service = Service {
        transport,
        target: target.clone(),
    };
    let pipelines = service.pages(&format!("{root}/pipelines")).await?;
    let approvals = transport
        .request(Method::GET, &format!("{root}/approvals"), None)
        .await?;
    let after = pulls.show().await?;
    for key in [
        "source_branch",
        "target_branch",
        "source_project_id",
        "target_project_id",
    ] {
        if before[key] != after[key] {
            return Err(Error::new(
                "conflict",
                "MR head or base changed while reading checks; inspect again",
            ));
        }
    }
    let after_sha = after["diff_refs"]["head_sha"]
        .as_str()
        .or_else(|| after["sha"].as_str());
    if after_sha != Some(sha) {
        return Err(Error::new(
            "conflict",
            "MR head or base changed while reading checks; inspect again",
        ));
    }
    let mut passed = 0;
    let mut failed = 0;
    let mut pending = 0;
    let mut neutral = 0;
    for pipeline in &pipelines {
        match pipeline["status"].as_str().ok_or_else(bad)? {
            "success" => passed += 1,
            "failed" | "canceled" => failed += 1,
            "skipped" | "manual" => neutral += 1,
            _ => pending += 1,
        }
    }
    let observed = if pipelines.is_empty() {
        "absent"
    } else if failed > 0 {
        "failed"
    } else if pending > 0 {
        "pending"
    } else if neutral > 0 {
        "non_failing"
    } else {
        "passed"
    };
    Ok(json!({
        "head_sha": sha,
        "observed_checks": observed,
        "counts":{"passed":passed,"failed":failed,"pending":pending,"neutral_or_skipped":neutral},
        "pipelines": pipelines,
        "approvals": approvals,
        "merge_authorized": false,
        "policy_evaluated": false,
        "note":"Observed GitLab evidence only. Missing pipelines are not a pass; approval data does not grant merge authorization. Protected branch rules, server policy, and human acceptance still apply."
    }))
}
pub fn summarize(
    sha: &str,
    checks: &[Value],
    statuses: &[Value],
    reviews: &[Value],
) -> Result<Value> {
    let mut failures = 0;
    let mut pending = 0;
    let mut neutral = 0;
    let mut success = 0;
    for c in checks {
        let status = c["status"].as_str().ok_or_else(bad)?;
        if status != "completed" {
            pending += 1;
            continue;
        }
        match c["conclusion"].as_str() {
            Some("success") => success += 1,
            Some("neutral" | "skipped") => neutral += 1,
            Some(
                "failure" | "cancelled" | "timed_out" | "action_required" | "startup_failure"
                | "stale",
            ) => failures += 1,
            _ => pending += 1,
        }
    }
    // A context can have many historical statuses; only its highest-ID report is current.
    let mut latest: BTreeMap<String, Value> = BTreeMap::new();
    for s in statuses {
        let context = s["context"].as_str().ok_or_else(bad)?;
        let id = s["id"].as_u64().ok_or_else(bad)?;
        if latest
            .get(context)
            .is_none_or(|v| v["id"].as_u64().unwrap_or(0) < id)
        {
            latest.insert(context.into(), s.clone());
        }
    }
    for s in latest.values() {
        match s["state"].as_str() {
            Some("success") => success += 1,
            Some("failure" | "error") => failures += 1,
            _ => pending += 1,
        }
    }
    let observed = if checks.is_empty() && latest.is_empty() {
        "absent"
    } else if failures > 0 {
        "failed"
    } else if pending > 0 {
        "pending"
    } else if neutral > 0 {
        "non_failing"
    } else {
        "passed"
    };
    let tagged: Vec<_> = reviews
        .iter()
        .map(|r| json!({"review":r,"matches_head":r["commit_id"].as_str()==Some(sha)}))
        .collect();
    Ok(
        json!({"head_sha":sha,"observed_checks":observed,"counts":{"passed":success,"failed":failures,"pending":pending,"neutral_or_skipped":neutral},"check_runs":checks,"latest_statuses":latest.into_values().collect::<Vec<_>>(),"reviews":tagged,"merge_authorized":false,"policy_evaluated":false,"note":"Observed evidence only. Missing checks are not a pass; review entries are historical, not an effective approval decision. Repository rules and human acceptance still apply."}),
    )
}
