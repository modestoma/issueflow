use crate::{
    config::Platform,
    error::{Error, Result},
    service::Service,
    target::Target,
    transport::Transport,
};
use http::Method;
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};

pub struct Hierarchy<'a> {
    pub transport: &'a dyn Transport,
    pub parent: Target,
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
        let iid = self
            .parent
            .number
            .ok_or_else(|| Error::new("input", "GitLab work item URL requires an iid"))?
            .to_string();
        let response = self.transport.request(Method::POST, "graphql", Some(json!({
            "query":"query($fullPath:ID!,$iid:String!,$after:String){namespace(fullPath:$fullPath){workItem(iid:$iid){id iid title webUrl workItemType{name} widgets{type ... on WorkItemWidgetHierarchy{parent{id iid title webUrl workItemType{name}} children(first:100,after:$after){nodes{id iid title webUrl workItemType{name}} pageInfo{hasNextPage endCursor}}}}}}}",
            "variables":{"fullPath":self.parent.repository,"iid":iid,"after":after}
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
        self.github()?;
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
        self.github()?;
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
}

fn unknown(mut error: Error) -> Error {
    error.outcome_unknown = true;
    error.message = format!(
        "Hierarchy write may have succeeded; inspect before retrying. {}",
        error.message
    );
    error
}
