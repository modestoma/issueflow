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

const MAX_HIERARCHY_ITEMS: usize = 1000;
const MAX_DIRECT_SUB_ISSUES: usize = 100;
pub const MAX_HIERARCHY_DEPTH: u8 = 8;

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
        || !url.username().is_empty()
        || url.password().is_some()
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
        match self
            .transport
            .request(
                Method::GET,
                &format!("{}/parent", self.parent.endpoint()?),
                None,
            )
            .await
        {
            Ok(value) => Ok(value),
            Err(error) if error.status == Some(404) => Ok(Value::Null),
            Err(error) => Err(error),
        }
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
        let children = service.pages(&self.root()?).await?;
        if children.len() > MAX_DIRECT_SUB_ISSUES {
            return Err(Error::new(
                "conflict",
                "GitHub returned more than 100 direct sub-issues",
            ));
        }
        Ok(Value::Array(children))
    }

    pub async fn sub_issues(&self, recursive: bool, depth: Option<u8>) -> Result<Value> {
        if recursive && self.parent.platform == Platform::Gitlab {
            return Err(Error::new(
                "input",
                "Recursive sub-issue traversal is currently supported only for GitHub",
            ));
        }
        let max_depth = depth.unwrap_or(MAX_HIERARCHY_DEPTH);
        if !(1..=MAX_HIERARCHY_DEPTH).contains(&max_depth) {
            return Err(Error::new("input", "depth must be between 1 and 8"));
        }
        let direct = self.children().await?;
        if !recursive {
            return Ok(Value::Array(
                direct
                    .as_array()
                    .ok_or_else(|| Error::new("response", "Sub-issues response is not an array"))?
                    .iter()
                    .enumerate()
                    .map(|(position, item)| relationship_item(item, 1, position + 1, None))
                    .collect::<Result<Vec<_>>>()?,
            ));
        }

        let mut result = Vec::new();
        let mut seen = BTreeSet::new();
        let root_url = target_url(&self.parent);
        let mut stack = direct
            .as_array()
            .ok_or_else(|| Error::new("response", "Sub-issues response is not an array"))?
            .iter()
            .enumerate()
            .rev()
            .map(|(position, item)| (item.clone(), 1_u8, position + 1, root_url.clone()))
            .collect::<Vec<_>>();
        while let Some((item, item_depth, position, parent_url)) = stack.pop() {
            let child = github_target_from_item(&item)?;
            let key = (child.repository.to_ascii_lowercase(), child.number);
            if !seen.insert(key) {
                return Err(Error::new(
                    "conflict",
                    "Hierarchy changed or repeated an item during traversal",
                ));
            }
            if seen.len() > MAX_HIERARCHY_ITEMS {
                return Err(Error::new(
                    "conflict",
                    "Hierarchy traversal exceeded 1000 items",
                ));
            }
            result.push(relationship_item(
                &item,
                item_depth,
                position,
                Some(&parent_url),
            )?);
            if item_depth < max_depth {
                let child_url = target_url(&child);
                let descendants = Hierarchy {
                    transport: self.transport,
                    parent: child,
                }
                .children()
                .await?;
                for (descendant_position, descendant) in descendants
                    .as_array()
                    .ok_or_else(|| Error::new("response", "Sub-issues response is not an array"))?
                    .iter()
                    .enumerate()
                    .rev()
                {
                    stack.push((
                        descendant.clone(),
                        item_depth + 1,
                        descendant_position + 1,
                        child_url.clone(),
                    ));
                }
            }
        }
        Ok(Value::Array(result))
    }

    pub async fn relationships(&self) -> Result<Value> {
        let parent = self.parent().await?;
        let children = self.children().await?;
        let service = Service {
            transport: self.transport,
            target: self.parent.clone(),
        };
        Ok(json!({
            "parent": parent,
            "sub_issues": children,
            "sub_issues_summary": summary_for(&children)?,
            "blocked_by": service.blocked_by().await?,
            "blocking": service.blocking().await?,
        }))
    }

    pub async fn summary(&self) -> Result<Value> {
        let children = self.children().await?;
        summary_for(&children)
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
            "query":"query($fullPath:ID!,$iid:String!,$after:String){namespace(fullPath:$fullPath){workItem(iid:$iid){id iid title state webUrl workItemType{name} widgets{type ... on WorkItemWidgetHierarchy{parent{id iid title state webUrl workItemType{name}} children(first:100,after:$after){nodes{id iid title state webUrl workItemType{name}} pageInfo{hasNextPage endCursor}}}}}}}",
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
            if seen.len() > MAX_HIERARCHY_ITEMS {
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
                queue.push_back(github_target_from_item(&item)?);
            }
        }
        Ok(())
    }
    pub async fn add_child(&self, child: &Target) -> Result<Value> {
        self.add_child_with_move(child, None).await
    }
    pub async fn add_child_with_move(
        &self,
        child: &Target,
        expected_old_parent: Option<&Target>,
    ) -> Result<Value> {
        if self.parent.platform == Platform::Gitlab {
            if expected_old_parent.is_some() {
                return Err(Error::new(
                    "input",
                    "GitLab parent reassignment is not supported by --move-from",
                ));
            }
            return self.set_gitlab_parent(child, true).await;
        }
        if child.platform != Platform::Github || child == &self.parent {
            return Err(Error::new(
                "input",
                "Parent and child must be distinct GitHub issues",
            ));
        }
        let current_parent = Hierarchy {
            transport: self.transport,
            parent: child.clone(),
        }
        .parent()
        .await?;
        if !current_parent.is_null() {
            let actual = github_target_from_item(&current_parent)?;
            if actual == self.parent {
                return Ok(
                    json!({"changed":false,"parent":current_parent,"children":self.children().await?}),
                );
            }
            if expected_old_parent != Some(&actual) {
                return Err(Error::new(
                    "conflict",
                    "Sub-issue already belongs to another parent; pass --move-from with that parent URL",
                ));
            }
        } else if expected_old_parent.is_some() {
            return Err(Error::new(
                "conflict",
                "Sub-issue has no current parent matching --move-from",
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
        if existing
            .as_array()
            .is_some_and(|items| items.len() >= MAX_DIRECT_SUB_ISSUES)
        {
            return Err(Error::new(
                "conflict",
                "A GitHub issue cannot have more than 100 direct sub-issues",
            ));
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
        let verified_parent = Hierarchy {
            transport: self.transport,
            parent: child.clone(),
        }
        .parent()
        .await
        .map_err(unknown)?;
        if github_target_from_item(&verified_parent).map_err(unknown)? != self.parent {
            return Err(unknown(Error::new(
                "conflict",
                "Sub-issue parent readback differs",
            )));
        }
        if let Some(old_parent) = expected_old_parent {
            let old_children = Hierarchy {
                transport: self.transport,
                parent: old_parent.clone(),
            }
            .children()
            .await
            .map_err(unknown)?;
            if contains_target(&old_children, child).map_err(unknown)? {
                return Err(unknown(Error::new(
                    "conflict",
                    "Old parent still contains the reassigned sub-issue",
                )));
            }
        }
        Ok(json!({"changed":true,"parent":verified_parent,"children":verified}))
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

    pub async fn remove_parent(&self) -> Result<Value> {
        let current = self.parent().await?;
        if current.is_null() {
            return Ok(json!({"changed":false,"parent":Value::Null}));
        }
        let parent = if self.parent.platform == Platform::Github {
            github_target_from_item(&current)?
        } else {
            Target {
                platform: Platform::Gitlab,
                repository: self.parent.repository.clone(),
                number: current["iid"]
                    .as_str()
                    .and_then(|iid| iid.parse().ok())
                    .or_else(|| current["iid"].as_u64()),
            }
        };
        Hierarchy {
            transport: self.transport,
            parent,
        }
        .remove_child(&self.parent)
        .await
    }

    pub async fn move_child(
        &self,
        child: &Target,
        before: Option<&Target>,
        after: Option<&Target>,
    ) -> Result<Value> {
        self.github()
            .map_err(|_| Error::new("input", "Sub-issue ordering is not supported for GitLab"))?;
        if before.is_some() == after.is_some() {
            return Err(Error::new(
                "input",
                "Exactly one of --before or --after is required",
            ));
        }
        let sibling = before.or(after).unwrap();
        if child.platform != Platform::Github
            || sibling.platform != Platform::Github
            || child == sibling
        {
            return Err(Error::new(
                "input",
                "Child and sibling must be distinct GitHub issues",
            ));
        }
        let children = self.children().await?;
        if !contains_target(&children, child)? || !contains_target(&children, sibling)? {
            return Err(Error::new(
                "conflict",
                "Child and sibling must both belong to the requested parent",
            ));
        }
        let child_id = item_id_for_target(&children, child)?;
        let sibling_id = item_id_for_target(&children, sibling)?;
        let mut payload = json!({"sub_issue_id": child_id});
        payload[if before.is_some() {
            "before_id"
        } else {
            "after_id"
        }] = json!(sibling_id);
        self.transport
            .request(
                Method::PATCH,
                &format!("{}/sub_issues/priority", self.parent.endpoint()?),
                Some(payload),
            )
            .await
            .map_err(unknown)?;
        let verified = self.children().await.map_err(unknown)?;
        let child_position = position_of(&verified, child).map_err(unknown)?;
        let sibling_position = position_of(&verified, sibling).map_err(unknown)?;
        if before.is_some() && child_position + 1 != sibling_position
            || after.is_some() && sibling_position + 1 != child_position
        {
            return Err(unknown(Error::new(
                "conflict",
                "Sub-issue order readback differs",
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

pub fn github_target_from_item(item: &Value) -> Result<Target> {
    let url = item["html_url"]
        .as_str()
        .or_else(|| item["url"].as_str())
        .ok_or_else(|| Error::new("response", "GitHub issue has no URL"))?;
    let parsed =
        url::Url::parse(url).map_err(|_| Error::new("response", "Invalid GitHub issue URL"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(Error::new("response", "GitHub issue host is invalid"));
    }
    let parts: Vec<_> = parsed.path().trim_matches('/').split('/').collect();
    if parts.len() != 4 || parts[2] != "issues" {
        return Err(Error::new("response", "Invalid GitHub issue URL"));
    }
    let number = parts[3]
        .parse()
        .map_err(|_| Error::new("response", "Invalid GitHub issue number"))?;
    Ok(Target {
        platform: Platform::Github,
        repository: format!("{}/{}", parts[0], parts[1]),
        number: Some(number),
    })
}

fn target_url(target: &Target) -> String {
    match target.platform {
        Platform::Github => format!(
            "https://github.com/{}/issues/{}",
            target.repository,
            target.number.unwrap_or_default()
        ),
        Platform::Gitlab => String::new(),
    }
}

fn relationship_item(
    item: &Value,
    depth: u8,
    position: usize,
    parent_url: Option<&str>,
) -> Result<Value> {
    let url = item["html_url"]
        .as_str()
        .or_else(|| item["webUrl"].as_str())
        .or_else(|| item["web_url"].as_str())
        .ok_or_else(|| Error::new("response", "Hierarchy item has no URL"))?;
    let id = item["id"]
        .as_u64()
        .map(Value::from)
        .or_else(|| item["id"].as_str().map(Value::from))
        .ok_or_else(|| Error::new("response", "Hierarchy item has no native id"))?;
    let number = item["number"]
        .as_u64()
        .map(Value::from)
        .or_else(|| item["iid"].as_u64().map(Value::from))
        .or_else(|| item["iid"].as_str().map(Value::from))
        .unwrap_or(Value::Null);
    Ok(json!({
        "id": id,
        "number": number,
        "url": url,
        "title": item["title"],
        "state": item["state"],
        "depth": depth,
        "position": position,
        "parent_url": parent_url
    }))
}

fn summary_for(children: &Value) -> Result<Value> {
    let children = children
        .as_array()
        .ok_or_else(|| Error::new("response", "Sub-issues response is not an array"))?;
    let completed = children
        .iter()
        .filter(|item| {
            item["state"]
                .as_str()
                .is_some_and(|state| state.eq_ignore_ascii_case("closed"))
        })
        .count();
    let total = children.len();
    let percent = completed
        .checked_mul(100)
        .and_then(|value| value.checked_div(total))
        .map(Value::from)
        .unwrap_or(Value::Null);
    Ok(json!({
        "completed": completed,
        "total": total,
        "percent": percent,
        "informational": true
    }))
}

fn contains_target(items: &Value, target: &Target) -> Result<bool> {
    Ok(items
        .as_array()
        .ok_or_else(|| Error::new("response", "Sub-issues response is not an array"))?
        .iter()
        .map(github_target_from_item)
        .collect::<Result<Vec<_>>>()?
        .contains(target))
}

fn item_id_for_target(items: &Value, target: &Target) -> Result<u64> {
    items
        .as_array()
        .ok_or_else(|| Error::new("response", "Sub-issues response is not an array"))?
        .iter()
        .find_map(|item| {
            (github_target_from_item(item).ok().as_ref() == Some(target))
                .then(|| item["id"].as_u64())
                .flatten()
        })
        .ok_or_else(|| Error::new("response", "Sub-issue has no native id"))
}

fn position_of(items: &Value, target: &Target) -> Result<usize> {
    items
        .as_array()
        .ok_or_else(|| Error::new("response", "Sub-issues response is not an array"))?
        .iter()
        .position(|item| github_target_from_item(item).ok().as_ref() == Some(target))
        .ok_or_else(|| Error::new("conflict", "Sub-issue disappeared during order readback"))
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
