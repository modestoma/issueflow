use crate::{
    config::{Config, Platform},
    error::{Error, Result},
    service::Service,
    target::{Target, valid_repository},
    transport::Transport,
};
use http::Method;
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};

pub struct Hierarchy<'a> {
    pub transport: &'a dyn Transport,
    pub parent: Target,
}

pub fn target_from_url(config: &Config, input: &str) -> Result<Target> {
    if let Ok(target) = Target::from_url(config, input) {
        return Ok(target);
    }
    let url =
        url::Url::parse(input).map_err(|_| Error::new("input", "Invalid hierarchy item URL"))?;
    let base = config.gitlab_url.as_ref().ok_or_else(|| {
        Error::new(
            "configuration",
            "GitLab hierarchy requires ISSUEFLOW_GITLAB_URL",
        )
    })?;
    if url.scheme() != "https"
        || url.origin() != base.origin()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::new(
            "input",
            "Hierarchy item URL does not match the configured GitLab host",
        ));
    }
    let path = url.path().strip_prefix(base.path()).ok_or_else(|| {
        Error::new(
            "input",
            "Hierarchy item URL does not match the configured GitLab base path",
        )
    })?;
    let (repository, number) = path
        .trim_end_matches('/')
        .rsplit_once("/-/work_items/")
        .ok_or_else(|| {
            Error::new(
                "input",
                "Expected a GitHub issue or GitLab issue/work-item URL",
            )
        })?;
    if !valid_repository(repository, Platform::Gitlab) {
        return Err(Error::new("input", "Invalid GitLab work-item project path"));
    }
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| Error::new("input", "Invalid GitLab work-item iid"))?;
    Ok(Target {
        platform: Platform::Gitlab,
        repository: repository.into(),
        number: Some(number),
    })
}

impl Hierarchy<'_> {
    fn github(&self) -> Result<()> {
        if self.parent.platform != Platform::Github {
            return Err(Error::new(
                "input",
                "GitLab hierarchy requires Work Item GraphQL support",
            ));
        }
        Ok(())
    }
    fn root(&self) -> Result<String> {
        self.github()?;
        Ok(format!("{}/sub_issues", self.parent.endpoint()?))
    }
    pub async fn parent(&self) -> Result<Value> {
        if self.parent.platform == Platform::Gitlab {
            let item = self.gitlab_item(None).await?;
            return Ok(item["widgets"]
                .as_array()
                .and_then(|widgets| widgets.iter().find(|widget| widget["type"] == "HIERARCHY"))
                .map(|widget| widget["parent"].clone())
                .unwrap_or(Value::Null));
        }
        self.transport
            .request(
                Method::GET,
                &format!("{}/parent", self.parent.endpoint()?),
                None,
            )
            .await
    }
    pub async fn children(&self) -> Result<Value> {
        if self.parent.platform == Platform::Gitlab {
            let mut children = Vec::new();
            let mut cursor: Option<String> = None;
            let mut seen = BTreeSet::new();
            for _ in 0..1000 {
                let item = self.gitlab_item(cursor.as_deref()).await?;
                let widget = item["widgets"]
                    .as_array()
                    .and_then(|widgets| widgets.iter().find(|widget| widget["type"] == "HIERARCHY"))
                    .ok_or_else(|| {
                        Error::new("response", "GitLab work item has no hierarchy widget")
                    })?;
                let connection = &widget["children"];
                for child in connection["nodes"].as_array().ok_or_else(|| {
                    Error::new("response", "GitLab hierarchy children are incomplete")
                })? {
                    let id = child["id"]
                        .as_str()
                        .ok_or_else(|| Error::new("response", "GitLab child has no id"))?;
                    if !seen.insert(id.to_string()) {
                        return Err(Error::new(
                            "conflict",
                            "GitLab hierarchy changed during pagination",
                        ));
                    }
                    children.push(child.clone());
                }
                if connection["pageInfo"]["hasNextPage"] != true {
                    return Ok(Value::Array(children));
                }
                cursor = Some(
                    connection["pageInfo"]["endCursor"]
                        .as_str()
                        .ok_or_else(|| {
                            Error::new("response", "GitLab hierarchy cursor is missing")
                        })?
                        .to_string(),
                );
            }
            return Err(Error::new(
                "response",
                "GitLab hierarchy pagination limit exceeded",
            ));
        }
        let service = Service {
            transport: self.transport,
            target: self.parent.clone(),
        };
        Ok(Value::Array(service.pages(&self.root()?).await?))
    }
    async fn gitlab_item(&self, after: Option<&str>) -> Result<Value> {
        Self::gitlab_item_for(self.transport, &self.parent, after).await
    }
    async fn gitlab_item_for(
        transport: &dyn Transport,
        target: &Target,
        after: Option<&str>,
    ) -> Result<Value> {
        let iid = target
            .number
            .ok_or_else(|| Error::new("input", "GitLab work item URL requires an iid"))?
            .to_string();
        let response = transport.request(Method::POST, "graphql", Some(json!({
            "query":"query($fullPath:ID!,$iid:String!,$after:String){namespace(fullPath:$fullPath){workItem(iid:$iid){id iid title webUrl workItemType{name} widgets{type ... on WorkItemWidgetHierarchy{parent{id iid title webUrl workItemType{name}} children(first:100,after:$after){nodes{id iid title webUrl workItemType{name}} pageInfo{hasNextPage endCursor}}}}}}}",
            "variables":{"fullPath":target.repository,"iid":iid,"after":after}
        }))).await?;
        if response
            .get("errors")
            .is_some_and(|errors| !errors.as_array().is_some_and(|items| items.is_empty()))
        {
            return Err(Error::new(
                "graphql",
                "GitLab Work Item hierarchy query failed",
            ));
        }
        let item = &response["data"]["namespace"]["workItem"];
        if !item["id"].is_string()
            || !item["workItemType"]["name"].is_string()
            || !item["widgets"].is_array()
        {
            return Err(Error::new(
                "response",
                "Incomplete GitLab work item response",
            ));
        }
        Ok(item.clone())
    }
    async fn ensure_no_cycle(&self, child: &Target) -> Result<()> {
        let mut queue = VecDeque::from([child.clone()]);
        let mut seen = BTreeSet::new();
        while let Some(current) = queue.pop_front() {
            let key = (current.repository.to_ascii_lowercase(), current.number);
            if !seen.insert(key) {
                continue;
            }
            if seen.len() > 1000 {
                return Err(Error::new(
                    "conflict",
                    "Hierarchy traversal exceeded 1000 items",
                ));
            }
            if current == self.parent {
                return Err(Error::new(
                    "conflict",
                    "Native hierarchy would create a cycle",
                ));
            }
            let service = Service {
                transport: self.transport,
                target: current.clone(),
            };
            for item in service
                .pages(&format!("{}/sub_issues", current.endpoint()?))
                .await?
            {
                let url = item["html_url"]
                    .as_str()
                    .ok_or_else(|| Error::new("response", "Sub-issue has no URL"))?;
                // Native GitHub sub-issues can cross repositories on the same host.
                let parts: Vec<_> = url::Url::parse(url)
                    .map_err(|_| Error::new("response", "Invalid sub-issue URL"))?
                    .path()
                    .trim_matches('/')
                    .split('/')
                    .map(str::to_string)
                    .collect();
                if parts.len() != 4 || parts[2] != "issues" {
                    return Err(Error::new("response", "Invalid sub-issue URL"));
                }
                let number = parts[3]
                    .parse()
                    .map_err(|_| Error::new("response", "Invalid sub-issue number"))?;
                queue.push_back(Target {
                    platform: Platform::Github,
                    repository: format!("{}/{}", parts[0], parts[1]),
                    number: Some(number),
                });
            }
        }
        Ok(())
    }
    pub async fn add_child(&self, child: &Target) -> Result<Value> {
        if self.parent.platform == Platform::Gitlab {
            return self.set_gitlab_parent(child, true).await;
        }
        if child.platform != Platform::Github || child == &self.parent {
            return Err(Error::new(
                "input",
                "Parent and child must be distinct GitHub issues",
            ));
        }
        self.ensure_no_cycle(child).await?;
        let child_issue = Service {
            transport: self.transport,
            target: child.clone(),
        }
        .raw_issue()
        .await?;
        let child_id = child_issue["id"]
            .as_u64()
            .ok_or_else(|| Error::new("response", "Child issue has no id"))?;
        let existing = self.children().await?;
        if existing
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"].as_u64() == Some(child_id))
        {
            return Ok(json!({"changed":false,"children":existing}));
        }
        self.transport
            .request(
                Method::POST,
                &self.root()?,
                Some(json!({"sub_issue_id":child_id})),
            )
            .await
            .map_err(unknown)?;
        let verified = self.children().await.map_err(unknown)?;
        if !verified
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"].as_u64() == Some(child_id))
        {
            return Err(unknown(Error::new(
                "conflict",
                "Sub-issue add could not be verified",
            )));
        }
        Ok(json!({"changed":true,"children":verified}))
    }
    pub async fn remove_child(&self, child: &Target) -> Result<Value> {
        if self.parent.platform == Platform::Gitlab {
            return self.set_gitlab_parent(child, false).await;
        }
        if child.platform != Platform::Github {
            return Err(Error::new("input", "Child must be a GitHub issue"));
        }
        let child_issue = Service {
            transport: self.transport,
            target: child.clone(),
        }
        .raw_issue()
        .await?;
        let child_id = child_issue["id"]
            .as_u64()
            .ok_or_else(|| Error::new("response", "Child issue has no id"))?;
        let existing = self.children().await?;
        if !existing
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"].as_u64() == Some(child_id))
        {
            return Ok(json!({"changed":false,"children":existing}));
        }
        self.transport
            .request(
                Method::DELETE,
                &format!("{}/sub_issue", self.parent.endpoint()?),
                Some(json!({"sub_issue_id":child_id})),
            )
            .await
            .map_err(unknown)?;
        let verified = self.children().await.map_err(unknown)?;
        if verified
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"].as_u64() == Some(child_id))
        {
            return Err(unknown(Error::new(
                "conflict",
                "Sub-issue removal could not be verified",
            )));
        }
        Ok(json!({"changed":true,"children":verified}))
    }
    async fn set_gitlab_parent(&self, child: &Target, add: bool) -> Result<Value> {
        if child.platform != Platform::Gitlab
            || child.repository != self.parent.repository
            || child == &self.parent
        {
            return Err(Error::new(
                "input",
                "GitLab parent and child must be distinct work items in one project",
            ));
        }
        let parent = Self::gitlab_item_for(self.transport, &self.parent, None).await?;
        let before = Self::gitlab_item_for(self.transport, child, None).await?;
        let parent_type = parent["workItemType"]["name"]
            .as_str()
            .ok_or_else(|| Error::new("response", "GitLab parent type is missing"))?;
        let child_type = before["workItemType"]["name"]
            .as_str()
            .ok_or_else(|| Error::new("response", "GitLab child type is missing"))?;
        if (parent_type, child_type) != ("Issue", "Task") {
            return Err(Error::new(
                "input",
                "This project-level adapter supports only GitLab Issue to Task hierarchy",
            ));
        }
        let parent_id = parent["id"]
            .as_str()
            .ok_or_else(|| Error::new("response", "GitLab parent id is missing"))?;
        let child_id = before["id"]
            .as_str()
            .ok_or_else(|| Error::new("response", "GitLab child id is missing"))?;
        let current_parent = hierarchy_parent(&before);
        if add && current_parent.and_then(|item| item["id"].as_str()) == Some(parent_id)
            || !add && current_parent.is_none()
        {
            return Ok(json!({"changed":false,"child":before}));
        }
        if add && current_parent.is_some() {
            return Err(Error::new(
                "conflict",
                "GitLab child already belongs to another parent",
            ));
        }
        if !add && current_parent.and_then(|item| item["id"].as_str()) != Some(parent_id) {
            return Err(Error::new(
                "conflict",
                "GitLab child does not belong to the requested parent",
            ));
        }
        let response = self.transport.request(Method::POST, "graphql", Some(json!({
            "query":"mutation($input:WorkItemUpdateInput!){workItemUpdate(input:$input){workItem{id} errors}}",
            "variables":{"input":{"id":child_id,"hierarchyWidget":{"parentId":if add { Value::String(parent_id.into()) } else { Value::Null }}}}
        }))).await.map_err(unknown)?;
        if response
            .get("errors")
            .is_some_and(|errors| !errors.as_array().is_some_and(|items| items.is_empty()))
            || !response["data"]["workItemUpdate"]["errors"]
                .as_array()
                .is_some_and(|items| items.is_empty())
            || response["data"]["workItemUpdate"]["workItem"]["id"].as_str() != Some(child_id)
        {
            return Err(unknown(Error::new(
                "graphql",
                "GitLab hierarchy mutation was not confirmed",
            )));
        }
        let after = Self::gitlab_item_for(self.transport, child, None)
            .await
            .map_err(unknown)?;
        let actual = hierarchy_parent(&after).and_then(|item| item["id"].as_str());
        if actual != if add { Some(parent_id) } else { None } {
            return Err(unknown(Error::new(
                "conflict",
                "GitLab hierarchy readback differs",
            )));
        }
        Ok(json!({"changed":true,"child":after}))
    }
}

fn hierarchy_parent(item: &Value) -> Option<&Value> {
    item["widgets"]
        .as_array()?
        .iter()
        .find(|widget| widget["type"] == "HIERARCHY")?
        .get("parent")
        .filter(|parent| !parent.is_null())
}

fn unknown(mut error: Error) -> Error {
    error.outcome_unknown = true;
    error.message = format!(
        "Hierarchy write may have succeeded; inspect before retrying. {}",
        error.message
    );
    error
}
