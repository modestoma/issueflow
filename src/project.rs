use crate::{
    config::{Config, Platform},
    error::{Error, Result},
    target::Target,
    transport::Transport,
};
use http::Method;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use url::Url;

#[derive(Debug)]
pub struct ProjectTarget {
    pub owner: String,
    pub kind: &'static str,
    pub number: i32,
}
impl ProjectTarget {
    pub fn parse(config: &Config, input: &str) -> Result<Self> {
        // Enterprise GraphQL uses a different API base; do not guess its routing.
        if config.github_api_url.as_str() != "https://api.github.com/" {
            return Err(Error::new(
                "configuration",
                "Projects currently supports github.com only",
            ));
        }
        let u = Url::parse(input).map_err(|_| Error::new("input", "Invalid Project URL"))?;
        let parts: Vec<_> = u.path().trim_end_matches('/').split('/').skip(1).collect();
        if u.scheme() != "https"
            || u.host_str() != Some("github.com")
            || u.port().is_some()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || parts.len() != 4
            || !matches!(parts[0], "users" | "orgs")
            || parts[2] != "projects"
            || parts[1].is_empty()
            || !parts[1]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(Error::new(
                "input",
                "Use a canonical https://github.com/users/OWNER/projects/N or /orgs/OWNER/projects/N URL",
            ));
        }
        let number = parts[3]
            .parse::<i32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::new("input", "Invalid Project number"))?;
        Ok(Self {
            owner: parts[1].into(),
            kind: if parts[0] == "users" {
                "user"
            } else {
                "organization"
            },
            number,
        })
    }
}
fn response_error() -> Error {
    Error::new("response", "Incomplete or malformed Projects response")
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(response_error)
}

pub struct Projects<'a> {
    pub transport: &'a dyn Transport,
    pub target: ProjectTarget,
}
impl Projects<'_> {
    async fn graphql(&self, query: &str, variables: Value, write: bool) -> Result<Value> {
        let result = self
            .transport
            .request(
                Method::POST,
                "graphql",
                Some(json!({"query":query,"variables":variables})),
            )
            .await;
        let v = result.map_err(|mut e| {
            if !write {
                e.outcome_unknown = false;
            }
            e
        })?;
        if v.get("errors")
            .is_some_and(|e| !e.as_array().is_some_and(|a| a.is_empty()))
        {
            let denied = v["errors"].as_array().is_some_and(|errors| {
                errors.iter().any(|e| {
                    matches!(
                        e["type"].as_str(),
                        Some("INSUFFICIENT_SCOPES" | "FORBIDDEN")
                    )
                })
            });
            let mut e = if denied {
                Error::new(
                    "permission",
                    "Token lacks access to GitHub Projects; configure a supported token with Projects access",
                )
            } else {
                Error::new(
                    "graphql",
                    "GitHub Projects GraphQL request failed; check project visibility, schema compatibility and token permissions",
                )
            };
            e.outcome_unknown = write;
            return Err(e);
        }
        if !v["data"].is_object() {
            let mut e = response_error();
            e.outcome_unknown = write;
            return Err(e);
        }
        Ok(v["data"].clone())
    }
    pub async fn show(&self) -> Result<Value> {
        let q = format!(
            "query($owner:String!,$number:Int!){{owner:{}(login:$owner){{projectV2(number:$number){{id title url closed}}}}}}",
            self.target.kind
        );
        let v = self
            .graphql(
                &q,
                json!({"owner":self.target.owner,"number":self.target.number}),
                false,
            )
            .await?;
        let mut project = v["owner"]["projectV2"].clone();
        if project.is_null() {
            return Err(Error::new(
                "not_found",
                "Project not found or not visible to this token",
            ));
        }
        let id = string(&project, "id")?.to_owned();
        project["fields"] = json!(self.connection(&id, "fields").await?);
        Ok(project)
    }
    async fn connection(&self, id: &str, field: &str) -> Result<Vec<Value>> {
        let selection = if field == "fields" {
            "... on ProjectV2FieldCommon { id name } ... on ProjectV2SingleSelectField { options { id name } }"
        } else {
            "id isArchived content { __typename ... on Issue { id url title state } ... on PullRequest { id url title } ... on DraftIssue { id title } } fieldValueByName(name:\"Status\") { ... on ProjectV2ItemFieldSingleSelectValue { name optionId } }"
        };
        let q = format!(
            "query($id:ID!,$after:String){{node(id:$id){{... on ProjectV2 {{{field}(first:100,after:$after){{nodes{{{selection}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
        );
        let mut after = Value::Null;
        let mut seen = BTreeSet::new();
        let mut cursors = BTreeSet::new();
        let mut all = Vec::new();
        for _ in 0..1000 {
            let v = self
                .graphql(&q, json!({"id":id,"after":after}), false)
                .await?;
            let c = &v["node"][field];
            for item in c["nodes"].as_array().ok_or_else(response_error)? {
                if !seen.insert(string(item, "id")?.to_owned()) {
                    return Err(Error::new(
                        "conflict",
                        "Project changed during pagination; read again",
                    ));
                }
                all.push(item.clone());
            }
            let page = &c["pageInfo"];
            if !page["hasNextPage"].as_bool().ok_or_else(response_error)? {
                return Ok(all);
            }
            let cursor = string(page, "endCursor")?;
            if !cursors.insert(cursor.to_owned()) {
                return Err(response_error());
            }
            after = json!(cursor);
        }
        Err(Error::new("response", "Project pagination limit exceeded"))
    }
    pub async fn items(&self) -> Result<Value> {
        let p = self.show().await?;
        Ok(json!({"project":p,"items":self.connection(string(&p,"id")?,"items").await?}))
    }
    async fn issue_id(&self, issue: &Target) -> Result<String> {
        if issue.platform != Platform::Github {
            return Err(Error::new("input", "Projects requires a GitHub issue"));
        }
        let (owner, repo) = issue
            .repository
            .split_once('/')
            .ok_or_else(response_error)?;
        let number = issue
            .number
            .filter(|n| *n <= i32::MAX as u64)
            .ok_or_else(|| Error::new("input", "Invalid issue number"))?;
        let v=self.graphql("query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){issue(number:$number){id}}}",json!({"owner":owner,"repo":repo,"number":number}),false).await?;
        string(&v["repository"]["issue"], "id")
            .map(String::from)
            .map_err(|_| Error::new("not_found", "Issue not found or not visible"))
    }
    fn membership(items: &[Value], content: &str) -> Result<Option<Value>> {
        let matches: Vec<_> = items
            .iter()
            .filter(|i| i["content"]["id"].as_str() == Some(content))
            .collect();
        if matches.len() > 1 {
            return Err(Error::new(
                "conflict",
                "Multiple Project items match the issue",
            ));
        }
        Ok(matches.first().map(|v| (*v).clone()))
    }
    pub async fn add(&self, issue: &Target) -> Result<Value> {
        let p = self.show().await?;
        let pid = string(&p, "id")?;
        let content = self.issue_id(issue).await?;
        if let Some(item) = Self::membership(&self.connection(pid, "items").await?, &content)? {
            return Ok(json!({"reused":true,"item":item}));
        }
        self.graphql("mutation($project:ID!,$content:ID!){addProjectV2ItemById(input:{projectId:$project,contentId:$content}){item{id}}}",json!({"project":pid,"content":content}),true).await?;
        let item = self
            .connection(pid, "items")
            .await
            .and_then(|items| Self::membership(&items, &content)?.ok_or_else(response_error))
            .map_err(after_write)?;
        Ok(json!({"reused":false,"item":item}))
    }
    pub async fn status(&self, issue: &Target, name: Option<&str>) -> Result<Value> {
        let p = self.show().await?;
        let pid = string(&p, "id")?;
        let fields = p["fields"].as_array().ok_or_else(response_error)?;
        let matches: Vec<_> = fields
            .iter()
            .filter(|f| f["name"] == "Status" && f["options"].is_array())
            .collect();
        if matches.len() != 1 {
            return Err(Error::new(
                "input",
                "Project must have exactly one single-select Status field",
            ));
        }
        let field = matches[0];
        // Resolve exact option before any mutation. No implicit creation or stage aliases.
        let option = if let Some(name) = name {
            let choices: Vec<_> = field["options"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|o| o["name"].as_str() == Some(name))
                .collect();
            if choices.len() != 1 {
                return Err(Error::new(
                    "input",
                    "Status option missing or ambiguous; use project show to inspect exact names",
                ));
            }
            Some(string(choices[0], "id")?.to_owned())
        } else {
            None
        };
        let content = self.issue_id(issue).await?;
        let item = Self::membership(&self.connection(pid, "items").await?, &content)?.ok_or_else(
            || {
                Error::new(
                    "not_found",
                    "Issue is not in this Project; use project add first",
                )
            },
        )?;
        if item["isArchived"] == true && name.is_some() {
            return Err(Error::new("input", "Project item is archived"));
        }
        if let Some(option) = option {
            if item["fieldValueByName"]["optionId"].as_str() == Some(&option) {
                return Ok(json!({"changed":false,"item":item}));
            }
            self.graphql("mutation($project:ID!,$item:ID!,$field:ID!,$option:String!){updateProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field,value:{singleSelectOptionId:$option}}){projectV2Item{id}}}",json!({"project":pid,"item":string(&item,"id")?,"field":string(field,"id")?,"option":option}),true).await?;
            let updated = self
                .connection(pid, "items")
                .await
                .and_then(|items| Self::membership(&items, &content)?.ok_or_else(response_error))
                .map_err(after_write)?;
            if updated["fieldValueByName"]["optionId"].as_str() != Some(&option) {
                return Err(after_write(Error::new(
                    "conflict",
                    "Status readback differs; inspect Project before retrying",
                )));
            }
            Ok(json!({"changed":true,"item":updated}))
        } else {
            Ok(json!({"item":item}))
        }
    }
}
fn after_write(mut e: Error) -> Error {
    e.outcome_unknown = true;
    e.message = format!("Project mutation may have succeeded; {}", e.message);
    e
}
