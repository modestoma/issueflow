use crate::{
    config::Platform,
    error::{Error, Result},
    service::Service,
    target::{Target, encode},
    transport::Transport,
};
use http::Method;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub title: Option<String>,
    pub description: Option<String>,
}

pub struct Milestones<'a> {
    pub transport: &'a dyn Transport,
    pub target: Target,
}

fn uncertain(mut error: Error) -> Error {
    error.outcome_unknown = true;
    error
}
fn snapshot(value: &Value) -> Value {
    json!({"id":value["id"],"number":value["number"],"title":value["title"],
        "description":value["description"],"state":value["state"],"updated_at":value["updated_at"]})
}
impl Milestones<'_> {
    fn github(&self) -> bool {
        self.target.platform == Platform::Github
    }
    fn prefix(&self) -> Result<String> {
        if self.target.number.is_some() {
            return Err(Error::new(
                "input",
                "Milestones require a repository target",
            ));
        }
        Ok(if self.github() {
            format!("repos/{}", self.target.repository)
        } else {
            format!("projects/{}", encode(&self.target.repository))
        })
    }
    fn root(&self) -> Result<String> {
        Ok(format!("{}/milestones", self.prefix()?))
    }
    fn endpoint(&self, id: u64) -> Result<String> {
        if id == 0 {
            return Err(Error::new("input", "Milestone ID must be positive"));
        }
        Ok(format!("{}/{id}", self.root()?))
    }
    fn identity(&self, value: &Value) -> Result<u64> {
        let field = if self.github() { "number" } else { "id" };
        if value["id"].as_u64().filter(|id| *id > 0).is_none() {
            return Err(Error::new("response", "Missing milestone identity"));
        }
        value[field]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(|| Error::new("response", "Missing milestone identifier"))
    }
    pub async fn list(&self) -> Result<Value> {
        let root = self.root()?;
        let endpoint = if self.github() {
            format!("{root}?state=all")
        } else {
            root
        };
        let service = Service {
            transport: self.transport,
            target: self.target.clone(),
        };
        let values = service.pages(&endpoint).await?;
        for v in &values {
            self.identity(v)?;
        }
        Ok(Value::Array(values))
    }
    pub async fn show(&self, id: u64) -> Result<Value> {
        let value = self
            .transport
            .request(Method::GET, &self.endpoint(id)?, None)
            .await?;
        if self.identity(&value)? != id {
            return Err(Error::new("response", "Milestone identity mismatch"));
        }
        Ok(value)
    }
    fn body(input: Input, create: bool) -> Result<Value> {
        let mut body = serde_json::Map::new();
        if let Some(title) = input.title {
            if title.trim().is_empty() {
                return Err(Error::new("input", "Milestone title cannot be empty"));
            }
            body.insert("title".into(), title.into());
        }
        if let Some(description) = input.description {
            body.insert("description".into(), description.into());
        }
        if body.is_empty() || (create && !body.contains_key("title")) {
            return Err(Error::new(
                "input",
                "Creation requires title; updates require title or description",
            ));
        }
        Ok(Value::Object(body))
    }
    pub async fn create(&self, input: Input, request_id: &str, apply: bool) -> Result<Value> {
        let uuid = uuid::Uuid::parse_str(request_id)
            .map_err(|_| Error::new("input", "request-id must be UUID"))?;
        let marker = format!("<!-- roadmap-operation:{uuid} -->");
        let mut body = Self::body(input, true)?;
        body["description"] =
            format!("{}\n\n{marker}", body["description"].as_str().unwrap_or("")).into();
        let all = self.list().await?;
        let found: Vec<_> = all
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["description"].as_str().unwrap_or("").contains(&marker))
            .collect();
        if found.len() > 1 {
            return Err(Error::new(
                "conflict",
                "Multiple milestones match request-id",
            ));
        }
        if let Some(value) = found.first() {
            return Ok(json!({"reused":true,"milestone":value}));
        }
        let endpoint = self.root()?;
        if !apply {
            return Ok(json!({"preview":true,"method":"POST","endpoint":endpoint,"body":body}));
        }
        let value = self
            .transport
            .request(Method::POST, &endpoint, Some(body.clone()))
            .await?;
        let id = self.identity(&value).map_err(uncertain)?;
        let actual = self.show(id).await.map_err(uncertain)?;
        Self::verify_body(&actual, &body).map_err(uncertain)?;
        Ok(json!({"applied":true,"milestone":actual}))
    }
    fn verify_body(actual: &Value, body: &Value) -> Result<()> {
        if body
            .as_object()
            .unwrap()
            .iter()
            .any(|(k, v)| actual.get(k) != Some(v))
        {
            return Err(Error::new("conflict", "Milestone write readback mismatch"));
        }
        Ok(())
    }
    pub async fn update(
        &self,
        id: u64,
        input: Input,
        expected: &Value,
        apply: bool,
    ) -> Result<Value> {
        let mut body = Self::body(input, false)?;
        let current = self.show(id).await?;
        if snapshot(&current) != snapshot(expected) {
            return Err(Error::new(
                "conflict",
                "Milestone changed; merge latest content before updating",
            ));
        }
        if let Some(description) = body["description"].as_str() {
            let mut description = description.to_string();
            let old = current["description"].as_str().unwrap_or("");
            for rest in old.split("<!-- roadmap-operation:").skip(1) {
                if let Some((id, _)) = rest.split_once(" -->")
                    && uuid::Uuid::parse_str(id).is_ok()
                {
                    let marker = format!("<!-- roadmap-operation:{id} -->");
                    if !description.contains(&marker) {
                        description.push_str(&format!("\n\n{marker}"));
                    }
                }
            }
            body["description"] = description.into();
        }
        self.write(id, body, apply, false).await
    }
    pub async fn close(
        &self,
        id: u64,
        expected: &Value,
        accepted_ref: &str,
        apply: bool,
    ) -> Result<Value> {
        if accepted_ref.trim().is_empty() {
            return Err(Error::new("input", "User acceptance reference required"));
        }
        let current = self.show(id).await?;
        if snapshot(&current) != snapshot(expected) {
            return Err(Error::new("conflict", "Milestone changed before close"));
        }
        let body = if self.github() {
            json!({"state":"closed"})
        } else {
            json!({"state_event":"close"})
        };
        self.write(id, body, apply, true).await
    }
    async fn write(&self, id: u64, body: Value, apply: bool, closing: bool) -> Result<Value> {
        let method = if self.github() {
            Method::PATCH
        } else {
            Method::PUT
        };
        let endpoint = self.endpoint(id)?;
        if !apply {
            return Ok(
                json!({"preview":true,"method":method.as_str(),"endpoint":endpoint,"body":body}),
            );
        }
        self.transport
            .request(method, &endpoint, Some(body.clone()))
            .await?;
        let actual = self.show(id).await.map_err(uncertain)?;
        let verification = if closing {
            json!({"state":"closed"})
        } else {
            body
        };
        Self::verify_body(&actual, &verification).map_err(uncertain)?;
        Ok(json!({"applied":true,"milestone":actual}))
    }
    pub async fn bind(&self, id: u64, issue: u64, apply: bool) -> Result<Value> {
        if issue == 0 {
            return Err(Error::new("input", "Issue number must be positive"));
        }
        let current = self.show(id).await?;
        let endpoint = format!("{}/issues/{issue}", self.prefix()?);
        let value = self.transport.request(Method::GET, &endpoint, None).await?;
        let field = if self.github() { "number" } else { "iid" };
        if value[field].as_u64() != Some(issue) || value.get("pull_request").is_some() {
            return Err(Error::new(
                "response",
                "Expected an Issue in the selected repository",
            ));
        }
        if !value["milestone"].is_null() {
            if value["milestone"]["id"] != current["id"] {
                return Err(Error::new(
                    "conflict",
                    "Issue already belongs to another milestone",
                ));
            }
            return Ok(json!({"already_bound":true,"issue":value}));
        }
        let body = if self.github() {
            json!({"milestone":id})
        } else {
            json!({"milestone_id":id})
        };
        let method = if self.github() {
            Method::PATCH
        } else {
            Method::PUT
        };
        if !apply {
            return Ok(
                json!({"preview":true,"method":method.as_str(),"endpoint":endpoint,"body":body}),
            );
        }
        self.transport
            .request(method, &endpoint, Some(body))
            .await?;
        let actual = self
            .transport
            .request(Method::GET, &endpoint, None)
            .await
            .map_err(uncertain)?;
        if actual[field].as_u64() != Some(issue) || actual["milestone"]["id"] != current["id"] {
            return Err(uncertain(Error::new(
                "conflict",
                "Issue milestone readback mismatch",
            )));
        }
        Ok(json!({"applied":true,"issue":actual}))
    }
}
