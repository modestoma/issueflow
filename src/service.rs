use crate::{
    config::Platform,
    error::{Error, Result},
    target::{Target, encode},
    transport::Transport,
};
use clap::ValueEnum;
use http::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Copy, ValueEnum)]
pub enum Stage {
    Triage,
    Clarification,
    Ready,
    InProgress,
    AwaitingReview,
}
impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Triage => "workflow::待复查",
            Self::Clarification => "workflow::待明确",
            Self::Ready => "workflow::就绪",
            Self::InProgress => "workflow::开发中",
            Self::AwaitingReview => "workflow::待验收",
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
pub enum CloseReason {
    Completed,
    Cancelled,
    Duplicate,
    Invalid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInput {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateInput {
    pub title: Option<String>,
    pub body: Option<String>,
}

pub struct Service<'a> {
    pub transport: &'a dyn Transport,
    pub target: Target,
}
impl Service<'_> {
    fn gh(&self) -> bool {
        self.target.platform == Platform::Github
    }
    fn edit_method(&self) -> Method {
        if self.gh() {
            Method::PATCH
        } else {
            Method::PUT
        }
    }
    pub async fn raw_issue(&self) -> Result<Value> {
        let issue = self
            .transport
            .request(Method::GET, &self.target.endpoint()?, None)
            .await?;
        normalize(&issue, self.target.platform)?;
        Ok(issue)
    }
    pub async fn show(&self) -> Result<Value> {
        normalize(&self.raw_issue().await?, self.target.platform)
    }
    pub(crate) async fn pages(&self, endpoint: &str) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        let mut ids = BTreeSet::new();
        for page in 1..=1000 {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            let response = self
                .transport
                .request(
                    Method::GET,
                    &format!("{endpoint}{separator}per_page=100&page={page}"),
                    None,
                )
                .await?;
            let items = response
                .as_array()
                .ok_or_else(|| Error::new("response", "API 分页没有返回数组"))?;
            for item in items {
                let id = item
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new("response", "分页项目缺少 id"))?;
                if !ids.insert(id) {
                    return Err(Error::new(
                        "conflict",
                        "分页期间数据发生变化或重复，请重新读取",
                    ));
                }
                all.push(item.clone());
            }
            if items.len() < 100 {
                return Ok(all);
            }
        }
        Err(Error::new(
            "response",
            "分页超过 100000 条上限；未返回不完整结果",
        ))
    }
    async fn raw_list(&self) -> Result<Vec<Value>> {
        let query = if self.gh() {
            "state=all&sort=created&direction=asc"
        } else {
            "state=all&scope=all&order_by=created_at&sort=asc"
        };
        Ok(self
            .pages(&format!("{}?{query}", self.target.collection()))
            .await?
            .into_iter()
            .filter(|v| v.get("pull_request").is_none())
            .collect())
    }
    pub async fn list(&self) -> Result<Value> {
        Ok(Value::Array(
            self.raw_list()
                .await?
                .iter()
                .map(|v| normalize(v, self.target.platform))
                .collect::<Result<Vec<_>>>()?,
        ))
    }
    pub async fn comments(&self) -> Result<Value> {
        self.raw_issue().await?;
        let suffix = if self.gh() { "comments" } else { "notes" };
        Ok(Value::Array(
            self.pages(&format!("{}/{suffix}", self.target.endpoint()?))
                .await?,
        ))
    }
    pub async fn comment(&self, body: String) -> Result<Value> {
        if body.trim().is_empty() {
            return Err(Error::new("input", "评论不能为空"));
        }
        self.raw_issue().await?;
        let suffix = if self.gh() { "comments" } else { "notes" };
        self.transport
            .request(
                Method::POST,
                &format!("{}/{suffix}", self.target.endpoint()?),
                Some(json!({"body": body})),
            )
            .await
    }
    /// Inspect a prior create operation without ever submitting another create request.
    pub async fn recover_create(&self, operation: &str) -> Result<Value> {
        let id = uuid::Uuid::parse_str(operation)
            .map_err(|_| Error::new("input", "request-id must be a UUID"))?;
        let issues = self.raw_list().await?;
        let matches = issues
            .iter()
            .filter(|v| {
                v[if self.gh() { "body" } else { "description" }]
                    .as_str()
                    .unwrap_or("")
                    .lines()
                    .any(|line| {
                        line.trim()
                            .strip_prefix("<!-- issueflow-operation: ")
                            .and_then(|s| s.strip_suffix(" -->"))
                            .and_then(|s| uuid::Uuid::parse_str(s).ok())
                            == Some(id)
                    })
            })
            .map(|v| normalize(v, self.target.platform))
            .collect::<Result<Vec<_>>>()?;
        let status = match matches.len() {
            0 => "not_visible",
            1 => "found",
            _ => "ambiguous",
        };
        Ok(
            json!({"operation":id.to_string(),"status":status,"matches":matches,"safe_to_retry":false,"note":"Read-only inspection. No match does not prove the create failed; visibility may lag. Do not retry create to verify its outcome."}),
        )
    }
    pub async fn create(&self, mut input: CreateInput, operation: &str) -> Result<Value> {
        uuid::Uuid::parse_str(operation)
            .map_err(|_| Error::new("input", "request-id 必须为 UUID"))?;
        if input.title.trim().is_empty() {
            return Err(Error::new("input", "issue 标题不能为空"));
        }
        validate_labels(&input.labels)?;
        validate_categories(&input.labels)?;
        if input.body.contains("issueflow-operation:") {
            return Err(Error::new(
                "input",
                "正文中不能自行设置 issueflow-operation 标记",
            ));
        }
        let marker = format!("<!-- issueflow-operation: {operation} -->");
        let existing = self.raw_list().await?;
        let matches: Vec<_> = existing
            .iter()
            .filter(|v| {
                v[if self.gh() { "body" } else { "description" }]
                    .as_str()
                    .is_some_and(|s| s.contains(&marker))
            })
            .collect();
        if matches.len() > 1 {
            return Err(Error::new(
                "conflict",
                "相同 request-id 对应多个 issue，请人工核对",
            ));
        }
        if let Some(issue) = matches.first() {
            let old_body = issue[if self.gh() { "body" } else { "description" }]
                .as_str()
                .unwrap_or("");
            if issue["title"].as_str() != Some(input.title.as_str())
                || old_body.replace(&marker, "").trim_end() != input.body.trim_end()
            {
                return Err(Error::new(
                    "conflict",
                    "request-id 已存在，但标题或正文不同；未覆盖已有 issue",
                ));
            }
            return Ok(
                json!({"operation": operation, "reused": true, "issue": normalize(issue, self.target.platform)?}),
            );
        }
        input.body.push_str(&format!("\n\n{marker}"));
        let mut payload = json!({"title": input.title});
        payload[if self.gh() { "body" } else { "description" }] = json!(input.body);
        payload["labels"] = if self.gh() {
            json!(input.labels)
        } else {
            json!(input.labels.join(","))
        };
        let response = self
            .transport
            .request(Method::POST, &self.target.collection(), Some(payload))
            .await
            .map_err(|mut e| {
                e.message.push_str(&format!("；request-id={operation}"));
                e
            })?;
        let issue = normalize(&response, self.target.platform).map_err(|mut e| {
            e.outcome_unknown = true;
            e.message.push_str(&format!("；request-id={operation}"));
            e
        })?;
        Ok(json!({"operation": operation, "reused": false, "issue": issue}))
    }
    pub async fn update(&self, input: UpdateInput, expected: Option<&str>) -> Result<Value> {
        if input.title.is_none() && input.body.is_none() {
            return Err(Error::new("input", "没有更新字段"));
        }
        if input.title.as_ref().is_some_and(|t| t.trim().is_empty()) {
            return Err(Error::new("input", "标题不能为空"));
        }
        let current = self.raw_issue().await?;
        if expected.is_some_and(|e| current["updated_at"].as_str() != Some(e)) {
            return Err(Error::new(
                "conflict",
                "issue 已被更新，请重新读取并合并修改",
            ));
        }
        let mut payload = json!({});
        if let Some(title) = input.title {
            payload["title"] = json!(title);
        }
        if let Some(mut body) = input.body {
            // Preserve recovery markers when replacing the visible description.
            let old = current[if self.gh() { "body" } else { "description" }]
                .as_str()
                .unwrap_or("");
            for line in old
                .lines()
                .filter(|l| l.starts_with("<!-- issueflow-operation:") && l.ends_with(" -->"))
            {
                if !body.contains(line) {
                    body.push_str(&format!("\n\n{line}"));
                }
            }
            payload[if self.gh() { "body" } else { "description" }] = json!(body);
        }
        let value = self
            .transport
            .request(self.edit_method(), &self.target.endpoint()?, Some(payload))
            .await?;
        normalize(&value, self.target.platform)
    }
    pub async fn labels(&self, add: Vec<String>, remove: Vec<String>) -> Result<Value> {
        validate_labels(&add)?;
        validate_labels(&remove)?;
        if add.iter().any(|label| remove.contains(label)) {
            return Err(Error::new("input", "同一标记不能同时添加和删除"));
        }
        let current = self.raw_issue().await?;
        if add.is_empty() && remove.is_empty() {
            return Ok(json!(labels(&current)?));
        }
        let mut desired: BTreeSet<_> = labels(&current)?.into_iter().collect();
        for label in &remove {
            desired.remove(label);
        }
        desired.extend(add.iter().cloned());
        validate_categories(&desired.into_iter().collect::<Vec<_>>())?;
        let endpoint = self.target.endpoint()?;
        if self.gh() {
            if !add.is_empty() {
                self.transport
                    .request(
                        Method::POST,
                        &format!("{endpoint}/labels"),
                        Some(json!({"labels": add})),
                    )
                    .await?;
            }
            let existing = labels(&current)?;
            for label in &remove {
                if existing.contains(label) {
                    self.transport
                        .request(
                            Method::DELETE,
                            &format!("{endpoint}/labels/{}", encode(label)),
                            None,
                        )
                        .await
                        .map_err(partial)?;
                }
            }
        } else {
            self.transport
                .request(
                    Method::PUT,
                    &endpoint,
                    Some(json!({"add_labels": add.join(","), "remove_labels": remove.join(",")})),
                )
                .await?;
        }
        let issue = self.raw_issue().await.map_err(partial)?;
        let actual = labels(&issue)?;
        if !add.iter().all(|s| actual.contains(s)) || remove.iter().any(|s| actual.contains(s)) {
            return Err(Error::new(
                "conflict",
                "标记读回与更新目标不一致，可能发生并发修改",
            ));
        }
        normalize(&issue, self.target.platform)
    }
    async fn set_stage(
        &self,
        stage: &str,
        extra_add: Vec<String>,
        extra_remove: Vec<String>,
    ) -> Result<Value> {
        let current = self.raw_issue().await?;
        let mut remove: Vec<_> = labels(&current)?
            .into_iter()
            .filter(|s| s.starts_with("workflow::") && s != stage)
            .collect();
        remove.extend(extra_remove);
        let mut add = vec![stage.to_string()];
        add.extend(extra_add);
        let result = self.labels(add, remove).await?;
        let actual = labels(&result)?;
        if actual
            .iter()
            .filter(|s| s.starts_with("workflow::"))
            .count()
            != 1
        {
            return Err(Error::new(
                "conflict",
                "出现多个阶段标记，需重新核对远端状态",
            ));
        }
        Ok(result)
    }
    pub async fn transition(&self, stage: Stage) -> Result<Value> {
        if self.gh() {
            return Err(Error::new(
                "input",
                "GitHub workflow stages use project status; configure a Project first",
            ));
        }
        let current = self.raw_issue().await?;
        if current["state"] == "closed" {
            return Err(Error::new("input", "issue 已关闭，请先明确执行 reopen"));
        }
        self.set_stage(stage.label(), vec![], vec![]).await
    }
    /// Change native state without touching any labels (for Project-backed workflows).
    pub async fn native_state(&self, reason: Option<CloseReason>) -> Result<Value> {
        self.raw_issue().await?;
        let payload = if self.gh() {
            match reason {
                Some(CloseReason::Completed) => {
                    json!({"state":"closed","state_reason":"completed"})
                }
                Some(_) => json!({"state":"closed","state_reason":"not_planned"}),
                None => json!({"state":"open"}),
            }
        } else {
            json!({"state_event":if reason.is_some(){"close"}else{"reopen"}})
        };
        self.transport
            .request(self.edit_method(), &self.target.endpoint()?, Some(payload))
            .await?;
        let result = self.show().await.map_err(partial)?;
        let expected = if reason.is_some() { "closed" } else { "open" };
        if result["state"] != expected {
            return Err(partial(Error::new(
                "conflict",
                "Native state readback differs",
            )));
        }
        Ok(result)
    }
    pub async fn close(&self, reason: CloseReason) -> Result<Value> {
        if self.gh() {
            return self.native_state(Some(reason)).await;
        }
        let current = self.raw_issue().await?;
        let old_resolutions: Vec<_> = labels(&current)?
            .into_iter()
            .filter(|s| s.starts_with("resolution::"))
            .collect();
        let resolution = match reason {
            CloseReason::Completed => None,
            CloseReason::Cancelled => Some("resolution::取消"),
            CloseReason::Duplicate => Some("resolution::重复"),
            CloseReason::Invalid => Some("resolution::失效"),
        };
        let removes = old_resolutions
            .into_iter()
            .filter(|s| Some(s.as_str()) != resolution)
            .collect();
        self.set_stage(
            if resolution.is_none() {
                "workflow::已完成"
            } else {
                "workflow::已终止"
            },
            resolution.into_iter().map(String::from).collect(),
            removes,
        )
        .await?;
        let payload = if self.gh() {
            json!({"state":"closed", "state_reason": if matches!(reason, CloseReason::Completed) { "completed" } else { "not_planned" }})
        } else {
            json!({"state_event":"close"})
        };
        let result = self
            .transport
            .request(self.edit_method(), &self.target.endpoint()?, Some(payload))
            .await
            .map_err(partial)?;
        normalize(&result, self.target.platform)
    }
    pub async fn reopen(&self) -> Result<Value> {
        if self.gh() {
            return self.native_state(None).await;
        }
        self.raw_issue().await?;
        let payload = if self.gh() {
            json!({"state":"open"})
        } else {
            json!({"state_event":"reopen"})
        };
        self.transport
            .request(self.edit_method(), &self.target.endpoint()?, Some(payload))
            .await?;
        let current = self.raw_issue().await.map_err(partial)?;
        let remove = labels(&current)?
            .into_iter()
            .filter(|s| s.starts_with("resolution::"))
            .collect();
        self.set_stage("workflow::待复查", vec![], remove)
            .await
            .map_err(partial)
    }
    pub async fn setup_labels(&self) -> Result<Value> {
        if self.gh() {
            return Err(Error::new(
                "input",
                "GitHub workflow metadata uses project init-workflow, not labels",
            ));
        }
        let endpoint = match self.target.platform {
            Platform::Github => format!("repos/{}/labels", self.target.repository),
            Platform::Gitlab => format!("projects/{}/labels", encode(&self.target.repository)),
        };
        let existing = self.pages(&endpoint).await?;
        let mut created = Vec::new();
        for name in [
            "workflow::待复查",
            "workflow::待明确",
            "workflow::就绪",
            "workflow::开发中",
            "workflow::待验收",
            "workflow::已完成",
            "workflow::已终止",
            "blocked",
            "resolution::取消",
            "resolution::重复",
            "resolution::失效",
            "type::bug",
            "type::feature",
            "type::improvement",
            "type::refactor",
            "type::docs",
            "type::chore",
            "type::research",
            "priority::P0",
            "priority::P1",
            "priority::P2",
            "priority::P3",
        ] {
            if existing.iter().any(|l| l["name"].as_str() == Some(name)) {
                continue;
            }
            self.transport.request(Method::POST, &endpoint, Some(json!({"name": name, "color": if self.gh() { "64748b" } else { "#64748b" }}))).await.map_err(partial)?;
            created.push(name);
        }
        Ok(json!({"created": created}))
    }
    pub async fn dependencies(&self) -> Result<Value> {
        let current = self.raw_issue().await?;
        let suffix = if self.gh() {
            "dependencies/blocked_by"
        } else {
            "links"
        };
        let items = self
            .pages(&format!("{}/{suffix}", self.target.endpoint()?))
            .await?;
        let url_key = if self.gh() { "html_url" } else { "web_url" };
        let origin = url::Url::parse(
            current[url_key]
                .as_str()
                .ok_or_else(|| Error::new("response", "issue 缺少 URL"))?,
        )
        .map_err(|_| Error::new("response", "issue URL 无效"))?
        .origin();
        for item in &items {
            let url = item[url_key]
                .as_str()
                .and_then(|s| url::Url::parse(s).ok())
                .ok_or_else(|| Error::new("response", "关联项缺少有效 URL"))?;
            if url.origin() != origin {
                return Err(Error::new("response", "关联项主机不一致，未跟随外部链接"));
            }
        }
        Ok(json!(
            items
                .into_iter()
                .filter(|v| self.gh() || v["link_type"] == "is_blocked_by")
                .collect::<Vec<_>>()
        ))
    }
    pub async fn add_dependency(&self, blocker: &Target) -> Result<Value> {
        if blocker.platform != self.target.platform {
            return Err(Error::new(
                "input",
                "跨平台依赖请在正文中关联 URL；原生关系不支持",
            ));
        }
        if blocker == &self.target {
            return Err(Error::new("input", "issue 不能依赖自身"));
        }
        // Walk the reachable blockers before adding an edge; never treat closed as complete.
        let mut queue = vec![blocker.clone()];
        let mut visited = BTreeSet::new();
        while let Some(node) = queue.pop() {
            if node == self.target {
                return Err(Error::new("conflict", "该依赖会形成循环"));
            }
            if !visited.insert((node.repository.clone(), node.number)) {
                continue;
            }
            if visited.len() > 1000 {
                return Err(Error::new("input", "依赖图超过检查上限，未修改关系"));
            }
            let service = Service {
                transport: self.transport,
                target: node,
            };
            let deps = service.dependencies().await?;
            for dep in deps
                .as_array()
                .ok_or_else(|| Error::new("response", "依赖响应格式无效"))?
            {
                queue.push(target_from_dependency(dep, self.target.platform)?);
            }
        }
        self.raw_issue().await?;
        let service = Service {
            transport: self.transport,
            target: blocker.clone(),
        };
        let issue = service.raw_issue().await?;
        let suffix = if self.gh() {
            "dependencies/blocked_by"
        } else {
            "links"
        };
        let body = if self.gh() {
            json!({"issue_id": issue["id"]})
        } else {
            json!({"target_project_id": blocker.repository, "target_issue_iid": blocker.number, "link_type":"is_blocked_by"})
        };
        self.transport
            .request(
                Method::POST,
                &format!("{}/{suffix}", self.target.endpoint()?),
                Some(body),
            )
            .await?;
        self.dependencies().await.map_err(partial)
    }

    pub async fn remove_dependency(&self, blocker: &Target) -> Result<Value> {
        if blocker.platform != self.target.platform {
            return Err(Error::new("input", "原生依赖不能跨平台"));
        }
        let dependencies = self.dependencies().await?;
        let mut found = None;
        for value in dependencies
            .as_array()
            .ok_or_else(|| Error::new("response", "依赖列表格式无效"))?
        {
            if target_from_dependency(value, self.target.platform)? == *blocker {
                found = Some(
                    value[if self.gh() { "id" } else { "issue_link_id" }]
                        .as_u64()
                        .ok_or_else(|| Error::new("response", "缺少原生依赖关系编号"))?,
                );
                break;
            }
        }
        let Some(id) = found else {
            return Ok(json!({"removed": false, "dependencies": dependencies}));
        };
        let suffix = if self.gh() {
            "dependencies/blocked_by"
        } else {
            "links"
        };
        self.transport
            .request(
                Method::DELETE,
                &format!("{}/{suffix}/{id}", self.target.endpoint()?),
                None,
            )
            .await?;
        Ok(json!({"removed": true, "dependencies": self.dependencies().await.map_err(partial)?}))
    }
}

fn target_from_dependency(value: &Value, platform: Platform) -> Result<Target> {
    if platform == Platform::Gitlab
        && let Some(full) = value["references"]["full"].as_str()
    {
        let (repository, number) = full
            .rsplit_once('#')
            .ok_or_else(|| Error::new("response", "依赖 reference 无效"))?;
        if !crate::target::valid_repository(repository, platform) {
            return Err(Error::new("response", "依赖仓库无效"));
        }
        return Ok(Target {
            platform,
            repository: repository.into(),
            number: Some(
                number
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| Error::new("response", "依赖编号无效"))?,
            ),
        });
    }
    let raw = value[if platform == Platform::Github {
        "html_url"
    } else {
        "web_url"
    }]
    .as_str()
    .ok_or_else(|| Error::new("response", "依赖缺少 URL"))?;
    let url = url::Url::parse(raw).map_err(|_| Error::new("response", "依赖 URL 无效"))?;
    let path = url.path().trim_matches('/');
    let separator = if platform == Platform::Github {
        "/issues/"
    } else {
        "/-/issues/"
    };
    let (repo, id) = path
        .rsplit_once(separator)
        .ok_or_else(|| Error::new("response", "无法解析依赖路径"))?;
    if !crate::target::valid_repository(repo, platform) {
        return Err(Error::new("response", "依赖仓库路径无效"));
    }
    Ok(Target {
        platform,
        repository: repo.into(),
        number: Some(
            id.parse()
                .map_err(|_| Error::new("response", "依赖编号无效"))?,
        ),
    })
}
fn partial(mut error: Error) -> Error {
    error
        .message
        .push_str("；此前步骤可能已成功，请读回核对后再继续");
    error.outcome_unknown = true;
    error
}
fn validate_labels(values: &[String]) -> Result<()> {
    if values
        .iter()
        .any(|v| v.trim().is_empty() || v.contains(',') || v.contains('\n'))
    {
        return Err(Error::new("input", "标记不能为空或包含逗号、换行"));
    }
    Ok(())
}
fn validate_categories(values: &[String]) -> Result<()> {
    for prefix in ["workflow::", "type::", "priority::", "resolution::"] {
        if values
            .iter()
            .filter(|s| s.starts_with(prefix))
            .collect::<BTreeSet<_>>()
            .len()
            > 1
        {
            return Err(Error::new(
                "input",
                format!("{prefix} 类别只能保留一个标记"),
            ));
        }
    }
    Ok(())
}
pub fn labels(value: &Value) -> Result<Vec<String>> {
    value["labels"]
        .as_array()
        .ok_or_else(|| Error::new("response", "issue 缺少 labels 数组"))?
        .iter()
        .map(|v| {
            v.as_str()
                .or_else(|| v["name"].as_str())
                .map(String::from)
                .ok_or_else(|| Error::new("response", "标记格式无效"))
        })
        .collect()
}
pub fn normalize(value: &Value, platform: Platform) -> Result<Value> {
    if value.get("pull_request").is_some() {
        return Err(Error::new("input", "链接指向 PR，不能当作 issue 修改"));
    }
    let gh = platform == Platform::Github;
    let number = value[if gh { "number" } else { "iid" }]
        .as_u64()
        .ok_or_else(|| Error::new("response", "API 缺少原生 issue 编号"))?;
    let url = value[if gh { "html_url" } else { "web_url" }]
        .as_str()
        .ok_or_else(|| Error::new("response", "API 缺少 issue URL"))?;
    Ok(
        json!({"platform": platform, "id": value["id"], "number": number, "url": url, "title": value["title"], "body": value[if gh { "body" } else { "description" }].as_str().unwrap_or(""), "state": if value["state"] == "closed" { "closed" } else { "open" }, "labels": labels(value)?, "created_at": value["created_at"], "updated_at": value["updated_at"]}),
    )
}
