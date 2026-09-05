use crate::{
    config::Platform,
    error::{Error, Result},
    service::{LEGACY_WORKFLOW_STAGE_LABELS, Service, WORKFLOW_STAGE_LABELS},
    target::{Target, encode},
    transport::Transport,
};
use http::Method;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub struct Boards<'a> {
    pub transport: &'a dyn Transport,
    pub target: Target,
}

impl Boards<'_> {
    fn root(&self) -> Result<String> {
        if self.target.platform != Platform::Gitlab || self.target.number.is_some() {
            return Err(Error::new(
                "input",
                "Board commands require a default GitLab project",
            ));
        }
        Ok(format!(
            "projects/{}/boards",
            encode(&self.target.repository)
        ))
    }
    fn service(&self) -> Service<'_> {
        Service {
            transport: self.transport,
            target: self.target.clone(),
        }
    }
    pub async fn list(&self) -> Result<Value> {
        Ok(Value::Array(self.service().pages(&self.root()?).await?))
    }
    pub async fn show(&self, id: u64) -> Result<Value> {
        let value = self
            .transport
            .request(Method::GET, &format!("{}/{id}", self.root()?), None)
            .await?;
        if value["id"].as_u64() != Some(id) || !value["lists"].is_array() {
            return Err(Error::new("response", "Incomplete GitLab board response"));
        }
        Ok(value)
    }
    pub async fn init_workflow(&self, name: &str) -> Result<Value> {
        if name.trim().is_empty() {
            return Err(Error::new("input", "Board name cannot be empty"));
        }
        self.service().setup_labels().await?;
        let labels = self
            .service()
            .pages(&format!(
                "projects/{}/labels",
                encode(&self.target.repository)
            ))
            .await?;
        let mut label_ids = BTreeMap::new();
        for label in labels {
            let label_name = label["name"]
                .as_str()
                .ok_or_else(|| Error::new("response", "GitLab label has no name"))?;
            if WORKFLOW_STAGE_LABELS.contains(&label_name) {
                let id = label["id"]
                    .as_u64()
                    .ok_or_else(|| Error::new("response", "GitLab workflow label has no id"))?;
                if label_ids.insert(label_name.to_string(), id).is_some() {
                    return Err(Error::new("conflict", "Duplicate GitLab workflow labels"));
                }
            }
        }
        if label_ids.len() != WORKFLOW_STAGE_LABELS.len() {
            return Err(Error::new(
                "response",
                "Not all GitLab workflow labels are visible",
            ));
        }
        let boards = self.list().await?;
        let matches: Vec<_> = boards
            .as_array()
            .unwrap()
            .iter()
            .filter(|board| board["name"].as_str() == Some(name))
            .collect();
        if matches.len() > 1 {
            return Err(Error::new(
                "conflict",
                "Multiple GitLab boards have the workflow name",
            ));
        }
        let (board_id, created) = if let Some(board) = matches.first() {
            (
                board["id"]
                    .as_u64()
                    .ok_or_else(|| Error::new("response", "GitLab board has no id"))?,
                false,
            )
        } else {
            let created = self
                .transport
                .request(Method::POST, &self.root()?, Some(json!({"name":name})))
                .await
                .map_err(unknown)?;
            (
                created["id"].as_u64().ok_or_else(|| {
                    unknown(Error::new("response", "Created GitLab board has no id"))
                })?,
                true,
            )
        };
        let lists_root = format!("{}/{board_id}/lists", self.root()?);
        let existing = self.service().pages(&lists_root).await?;
        let mut by_name: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for list in existing {
            if let Some(label) = list["label"]["name"].as_str() {
                by_name.entry(label.to_string()).or_default().push(list);
            }
        }
        if WORKFLOW_STAGE_LABELS
            .iter()
            .any(|name| by_name.get(*name).is_some_and(|lists| lists.len() > 1))
        {
            return Err(Error::new(
                "conflict",
                "A GitLab workflow label appears in multiple board lists",
            ));
        }
        let mut changed = created;
        for label in WORKFLOW_STAGE_LABELS {
            if !by_name.contains_key(label) {
                self.transport
                    .request(
                        Method::POST,
                        &lists_root,
                        Some(json!({"label_id":label_ids[label]})),
                    )
                    .await
                    .map_err(unknown)?;
                changed = true;
            }
        }
        let current = self.service().pages(&lists_root).await?;
        let mut seen = BTreeSet::new();
        for (index, label) in WORKFLOW_STAGE_LABELS.iter().enumerate() {
            let matches: Vec<_> = current
                .iter()
                .filter(|list| list["label"]["name"].as_str() == Some(*label))
                .collect();
            if matches.len() != 1 || !seen.insert(*label) {
                return Err(unknown(Error::new(
                    "conflict",
                    "GitLab board list readback is incomplete or ambiguous",
                )));
            }
            let list = matches[0];
            let id = list["id"]
                .as_u64()
                .ok_or_else(|| Error::new("response", "GitLab board list has no id"))?;
            let position = (index + 1) as u64;
            if list["position"].as_u64() != Some(position) {
                self.transport
                    .request(
                        Method::PUT,
                        &format!("{lists_root}/{id}"),
                        Some(json!({"position":position})),
                    )
                    .await
                    .map_err(unknown)?;
                changed = true;
            }
        }
        let final_board = self.show(board_id).await.map_err(unknown)?;
        let final_lists = self.service().pages(&lists_root).await.map_err(unknown)?;
        for (index, label) in WORKFLOW_STAGE_LABELS.iter().enumerate() {
            if final_lists
                .iter()
                .filter(|list| {
                    list["label"]["name"].as_str() == Some(*label)
                        && list["position"].as_u64() == Some((index + 1) as u64)
                })
                .count()
                != 1
            {
                return Err(unknown(Error::new(
                    "conflict",
                    "GitLab workflow board order could not be verified",
                )));
            }
        }
        let legacy_lists: Vec<_> = final_lists
            .iter()
            .filter(|list| {
                list["label"]["name"]
                    .as_str()
                    .is_some_and(|name| LEGACY_WORKFLOW_STAGE_LABELS.contains(&name))
            })
            .cloned()
            .collect();
        let legacy_cleanup_required = !legacy_lists.is_empty();
        Ok(json!({
            "changed":changed,
            "board":final_board,
            "workflow_lists":final_lists.iter().filter(|list| list["label"]["name"].as_str().is_some_and(|name| WORKFLOW_STAGE_LABELS.contains(&name))).cloned().collect::<Vec<_>>(),
            "legacy_lists":legacy_lists,
            "legacy_cleanup_required":legacy_cleanup_required
        }))
    }
}

fn unknown(mut error: Error) -> Error {
    error.outcome_unknown = true;
    error.message = format!(
        "GitLab board write may have succeeded; inspect before retrying. {}",
        error.message
    );
    error
}
