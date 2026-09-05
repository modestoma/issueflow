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
            if v["errors"].as_array().is_some_and(|errors| {
                errors.iter().any(|v| {
                    v["message"]
                        .as_str()
                        .is_some_and(|m| m.to_ascii_lowercase().contains("reserved"))
                })
            }) {
                e.message = "GitHub rejected a reserved Project field name".into();
            }
            e.outcome_unknown = write;
            return Err(e);
        }
        if !v["data"].is_object() {
            let mut e = response_error();
            if v["errors"].as_array().is_some_and(|errors| {
                errors.iter().any(|v| {
                    v["message"]
                        .as_str()
                        .is_some_and(|m| m.to_ascii_lowercase().contains("reserved"))
                })
            }) {
                e.message = "GitHub rejected a reserved Project field name".into();
            }
            e.outcome_unknown = write;
            return Err(e);
        }
        Ok(v["data"].clone())
    }
    pub async fn owner_projects(&self) -> Result<Value> {
        let q = format!(
            "query($owner:String!,$after:String){{owner:{}(login:$owner){{id projectsV2(first:100,after:$after){{nodes{{id number title url closed}} pageInfo{{hasNextPage endCursor}}}}}}}}",
            self.target.kind
        );
        let mut after = Value::Null;
        let mut ids = BTreeSet::new();
        let mut cursors = BTreeSet::new();
        let mut all = Vec::new();
        for _ in 0..1000 {
            let v = self
                .graphql(&q, json!({"owner":self.target.owner,"after":after}), false)
                .await?;
            let owner = &v["owner"];
            if owner.is_null() {
                return Err(Error::new(
                    "not_found",
                    "Project owner not found or not visible",
                ));
            }
            let owner_id = string(owner, "id")?;
            let c = &owner["projectsV2"];
            for p in c["nodes"].as_array().ok_or_else(response_error)? {
                if !ids.insert(string(p, "id")?.to_owned()) {
                    return Err(Error::new("conflict", "Projects changed during pagination"));
                }
                all.push(p.clone());
            }
            if !c["pageInfo"]["hasNextPage"]
                .as_bool()
                .ok_or_else(response_error)?
            {
                return Ok(json!({"owner_id":owner_id,"projects":all}));
            }
            let cursor = string(&c["pageInfo"], "endCursor")?;
            if !cursors.insert(cursor.to_owned()) {
                return Err(response_error());
            }
            after = json!(cursor);
        }
        Err(Error::new("response", "Project pagination limit exceeded"))
    }
    pub async fn create(&self, title: &str) -> Result<Value> {
        if title.trim().is_empty() {
            return Err(Error::new("input", "Project title cannot be empty"));
        }
        let list = self.owner_projects().await?;
        let matches: Vec<_> = list["projects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["title"].as_str() == Some(title))
            .collect();
        if matches.len() > 1 {
            return Err(Error::new(
                "conflict",
                "Multiple Projects share this title; choose a URL explicitly",
            ));
        }
        let (result, reused) = if let Some(p) = matches.first() {
            if p["closed"] != false {
                return Err(Error::new(
                    "conflict",
                    "Matching Project is closed; choose another title or reopen it explicitly",
                ));
            }
            ((*p).clone(), true)
        } else {
            let v=self.graphql("mutation($owner:ID!,$title:String!){createProjectV2(input:{ownerId:$owner,title:$title}){projectV2{id number title url closed}}}",json!({"owner":string(&list,"owner_id")?,"title":title}),true).await?;
            (v["createProjectV2"]["projectV2"].clone(), false)
        };
        let verify = async {
            let number = result["number"]
                .as_i64()
                .filter(|n| *n > 0 && *n <= i32::MAX as i64)
                .ok_or_else(response_error)? as i32;
            let p = Projects {
                transport: self.transport,
                target: ProjectTarget {
                    owner: self.target.owner.clone(),
                    kind: self.target.kind,
                    number,
                },
            }
            .show()
            .await?;
            if p["id"] != result["id"] || p["title"].as_str() != Some(title) {
                return Err(Error::new("conflict", "Created Project readback differs"));
            }
            Ok(json!({"reused":reused,"project":p}))
        }
        .await;
        if reused {
            verify
        } else {
            verify.map_err(after_write)
        }
    }
    pub async fn init_statuses(&self) -> Result<Value> {
        let desired = [
            ("Backlog", "GRAY"),
            ("Ready", "BLUE"),
            ("In progress", "YELLOW"),
            ("In review", "PURPLE"),
            ("Done", "GREEN"),
            ("Cancelled", "GRAY"),
        ];
        self.ensure_select("Status", &desired, false).await
    }
    async fn ensure_select(
        &self,
        name: &str,
        desired: &[(&str, &str)],
        create: bool,
    ) -> Result<Value> {
        let before = self.show().await?;
        if before["closed"] != false {
            return Err(Error::new("input", "Cannot initialize a closed Project"));
        }
        let fields = before["fields"].as_array().ok_or_else(response_error)?;
        let candidates: Vec<_> = fields
            .iter()
            .filter(|f| f["name"].as_str() == Some(name))
            .collect();
        if candidates.is_empty() && create {
            let options: Vec<_> = desired
                .iter()
                .map(|(name, color)| json!({"name":name,"color":color,"description":""}))
                .collect();
            self.graphql("mutation($project:ID!,$name:String!,$options:[ProjectV2SingleSelectFieldOptionInput!]!){createProjectV2Field(input:{projectId:$project,name:$name,dataType:SINGLE_SELECT,singleSelectOptions:$options}){projectV2Field{... on ProjectV2SingleSelectField{id}}}}",json!({"project":string(&before,"id")?,"name":name,"options":options}),true).await?;
            let verified = self.show().await.map_err(after_write)?;
            let fields = verified["fields"]
                .as_array()
                .ok_or_else(|| after_write(response_error()))?;
            let found: Vec<_> = fields
                .iter()
                .filter(|f| f["name"].as_str() == Some(name))
                .collect();
            if found.len() != 1
                || !found[0]["options"].as_array().is_some_and(|actual| {
                    desired.iter().all(|(name, _)| {
                        actual
                            .iter()
                            .filter(|o| o["name"].as_str() == Some(*name))
                            .count()
                            == 1
                    })
                })
            {
                return Err(after_write(response_error()));
            }
            return Ok(json!({"changed":true,"created":name,"project":verified}));
        }
        if candidates.len() != 1 || !candidates[0]["options"].is_array() {
            return Err(Error::new(
                "input",
                "Expected exactly one single-select field with the requested name",
            ));
        }
        let field = candidates[0];
        let old = field["options"].as_array().unwrap();
        let mut options = Vec::new();
        let mut names = BTreeSet::new();
        for o in old {
            let name = string(o, "name")?;
            if !names.insert(name.to_owned()) {
                return Err(Error::new("conflict", "Ambiguous duplicate Status options"));
            }
            options.push(json!({"id":string(o,"id")?,"name":name,"color":string(o,"color")?,"description":o["description"].as_str().unwrap_or("")}));
        }
        let mut added = Vec::new();
        for &(name, color) in desired {
            if !names.contains(name) {
                options.push(json!({"name":name,"color":color,"description":""}));
                added.push(name);
            }
        }
        if added.is_empty() {
            return Ok(json!({"changed":false,"project":before}));
        }
        let latest = self.show().await?;
        if latest["fields"] != before["fields"] {
            return Err(Error::new(
                "conflict",
                "Project fields changed before initialization; inspect again",
            ));
        }
        self.graphql("mutation($field:ID!,$options:[ProjectV2SingleSelectFieldOptionInput!]!){updateProjectV2Field(input:{fieldId:$field,singleSelectOptions:$options}){projectV2Field{... on ProjectV2SingleSelectField{id}}}}",json!({"field":string(field,"id")?,"options":options}),true).await?;
        let verified = self.show().await.map_err(after_write)?;
        let check = || -> Result<()> {
            let fields = verified["fields"].as_array().ok_or_else(response_error)?;
            let found = fields
                .iter()
                .find(|f| f["id"] == field["id"])
                .ok_or_else(response_error)?;
            let actual = found["options"].as_array().ok_or_else(response_error)?;
            for expected in &options {
                let found: Vec<_> = actual
                    .iter()
                    .filter(|o| o["name"] == expected["name"])
                    .collect();
                if found.len() != 1 {
                    return Err(response_error());
                }
                if let Some(id) = expected.get("id")
                    && (found[0]["id"] != *id
                        || found[0]["color"] != expected["color"]
                        || found[0]["description"].as_str().unwrap_or("")
                            != expected["description"].as_str().unwrap_or(""))
                {
                    return Err(response_error());
                }
            }
            Ok(())
        };
        check().map_err(after_write)?;
        Ok(json!({"changed":true,"added":added,"project":verified}))
    }
    pub async fn init_workflow(&self) -> Result<Value> {
        self.init_statuses().await?;
        let result = async {
            for (name, values) in [
                (
                    "Work type",
                    vec![
                        "bug",
                        "feature",
                        "improvement",
                        "refactor",
                        "docs",
                        "chore",
                        "research",
                    ],
                ),
                ("Priority", vec!["P0", "P1", "P2", "P3"]),
                ("Blocked", vec!["No", "Yes"]),
                (
                    "Resolution",
                    vec!["Completed", "Cancelled", "Duplicate", "Invalid"],
                ),
            ] {
                let options: Vec<_> = values.iter().map(|v| (*v, "GRAY")).collect();
                self.ensure_select(name, &options, true).await?;
            }
            let board = self.ensure_board().await?;
            Ok(json!({"project":self.show().await?,"board":board}))
        }
        .await;
        result.map_err(after_write)
    }
    pub async fn view_list(&self) -> Result<Value> {
        let p = self.show().await?;
        Ok(json!({"project":p["url"],"views":self.views(string(&p,"id")?).await?}))
    }
    async fn views(&self, pid: &str) -> Result<Vec<Value>> {
        let mut after = Value::Null;
        let mut seen = BTreeSet::new();
        let mut cursors = BTreeSet::new();
        let mut all = Vec::new();
        for _ in 0..1000 {
            let v=self.graphql("query($id:ID!,$after:String){node(id:$id){... on ProjectV2{views(first:100,after:$after){nodes{id name layout filter groupByFields(first:100){nodes{... on ProjectV2FieldCommon{id name}} pageInfo{hasNextPage}} verticalGroupByFields(first:100){nodes{... on ProjectV2FieldCommon{id name}} pageInfo{hasNextPage}}} pageInfo{hasNextPage endCursor}}}}}",json!({"id":pid,"after":after}),false).await?;
            let c = &v["node"]["views"];
            for view in c["nodes"].as_array().ok_or_else(response_error)? {
                if !seen.insert(string(view, "id")?.to_string())
                    || view["groupByFields"]["pageInfo"]["hasNextPage"] != false
                    || view["verticalGroupByFields"]["pageInfo"]["hasNextPage"] != false
                {
                    return Err(response_error());
                }
                all.push(view.clone());
            }
            if c["pageInfo"]["hasNextPage"] == false {
                return Ok(all);
            }
            if c["pageInfo"]["hasNextPage"] != true {
                return Err(response_error());
            }
            let cursor = string(&c["pageInfo"], "endCursor")?;
            if !cursors.insert(cursor.to_owned()) {
                return Err(response_error());
            }
            after = json!(cursor);
        }
        Err(response_error())
    }
    pub async fn ensure_board(&self) -> Result<Value> {
        let p = self.show().await?;
        let pid = string(&p, "id")?;
        let fields = p["fields"].as_array().ok_or_else(response_error)?;
        let status = fields
            .iter()
            .find(|f| f["name"] == "Status")
            .ok_or_else(response_error)?;
        let matches = |v: &Value| {
            v["layout"] == "BOARD_LAYOUT"
                && v["filter"].as_str().unwrap_or("").is_empty()
                && v["verticalGroupByFields"]["nodes"]
                    .as_array()
                    .is_some_and(|g| g.len() == 1 && g[0]["id"] == status["id"])
        };
        let views = self.views(pid).await?;
        if let Some(v) = views.iter().find(|v| matches(v)) {
            return Ok(json!({"created":false,"view":v}));
        }
        if views.iter().any(|v| v["name"] == "Issueflow Kanban") {
            return Err(Error::new(
                "conflict",
                "Existing Issueflow Kanban has different grouping/filter; inspect before changing it",
            ));
        }
        let v=self.graphql("mutation($project:ID!){createProjectV2View(input:{projectId:$project,name:\"Issueflow Kanban\",layout:BOARD_LAYOUT}){projectV2View{id}}}",json!({"project":pid}),true).await?;
        let id = v["createProjectV2View"]["projectV2View"]["id"]
            .as_str()
            .ok_or_else(|| after_write(response_error()))?;
        let views = self.views(pid).await.map_err(after_write)?;
        let view = views
            .iter()
            .find(|v| v["id"] == id)
            .ok_or_else(|| after_write(response_error()))?;
        if !matches(view) {
            return Err(after_write(Error::new(
                "response",
                "Board created but Status grouping was not confirmed; inspect view configuration",
            )));
        }
        Ok(json!({"created":true,"view":view}))
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
        self.connection_field(id, field, "Status").await
    }
    async fn connection_field(
        &self,
        id: &str,
        field: &str,
        field_name: &str,
    ) -> Result<Vec<Value>> {
        let selection = if field == "fields" {
            "... on ProjectV2FieldCommon { id name } ... on ProjectV2SingleSelectField { options { id name color description } }"
        } else {
            "id isArchived content { __typename ... on Issue { id url title state } ... on PullRequest { id url title } ... on DraftIssue { id title } } fieldValueByName(name:\"Status\") { ... on ProjectV2ItemFieldSingleSelectValue { name optionId } } resolution:fieldValueByName(name:\"Resolution\") { ... on ProjectV2ItemFieldSingleSelectValue { name optionId } } blocked:fieldValueByName(name:\"Blocked\") { ... on ProjectV2ItemFieldSingleSelectValue { name optionId } }"
        };
        let selection =
            selection.replace("name:\"Status\"", &format!("name:{}", json!(field_name)));
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
        self.field(issue, "Status", name, false).await
    }
    pub async fn field(
        &self,
        issue: &Target,
        field_name: &str,
        name: Option<&str>,
        clear: bool,
    ) -> Result<Value> {
        if clear && name.is_some() {
            return Err(Error::new("input", "Choose either --to or --clear"));
        }
        let p = self.show().await?;
        let pid = string(&p, "id")?;
        let fields = p["fields"].as_array().ok_or_else(response_error)?;
        let matches: Vec<_> = fields
            .iter()
            .filter(|f| f["name"].as_str() == Some(field_name) && f["options"].is_array())
            .collect();
        if matches.len() != 1 {
            return Err(Error::new(
                "input",
                "Project must have exactly one single-select field with the requested name",
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
        let item = Self::membership(
            &self.connection_field(pid, "items", field_name).await?,
            &content,
        )?
        .ok_or_else(|| {
            Error::new(
                "not_found",
                "Issue is not in this Project; use project add first",
            )
        })?;
        if item["isArchived"] == true && (name.is_some() || clear) {
            return Err(Error::new("input", "Project item is archived"));
        }
        if clear {
            if item["fieldValueByName"].is_null() {
                return Ok(json!({"changed":false,"item":item}));
            }
            self.graphql("mutation($project:ID!,$item:ID!,$field:ID!){clearProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field}){projectV2Item{id}}}",json!({"project":pid,"item":string(&item,"id")?,"field":string(field,"id")?}),true).await?;
            let updated = self
                .connection_field(pid, "items", field_name)
                .await
                .and_then(|items| Self::membership(&items, &content)?.ok_or_else(response_error))
                .map_err(after_write)?;
            if !updated["fieldValueByName"].is_null() {
                return Err(after_write(response_error()));
            }
            return Ok(json!({"changed":true,"item":updated}));
        }
        if let Some(option) = option {
            if item["fieldValueByName"]["optionId"].as_str() == Some(&option) {
                return Ok(json!({"changed":false,"item":item}));
            }
            self.graphql("mutation($project:ID!,$item:ID!,$field:ID!,$option:String!){updateProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field,value:{singleSelectOptionId:$option}}){projectV2Item{id}}}",json!({"project":pid,"item":string(&item,"id")?,"field":string(field,"id")?,"option":option}),true).await?;
            let updated = self
                .connection_field(pid, "items", field_name)
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
    if !e
        .message
        .starts_with("Project mutation may have succeeded;")
    {
        e.message = format!("Project mutation may have succeeded; {}", e.message);
    }
    e
}
