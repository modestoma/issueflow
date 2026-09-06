use std::collections::BTreeSet;

use http::Method;
use serde_json::{Value, json};

use crate::{
    board::Boards,
    config::{Config, Platform},
    error::{Error, Result},
    project::{ProjectTarget, Projects},
    service::{Service, WORKFLOW_STAGE_LABELS},
    target::{Target, encode},
    transport::Transport,
    workflow_config::WorkflowConfig,
};

const REQUIRED_PROJECT_FIELDS: &[(&str, &[&str])] = &[
    (
        "Status",
        &[
            "Backlog",
            "Ready",
            "In progress",
            "In review",
            "Done",
            "Cancelled",
        ],
    ),
    (
        "Work type",
        &[
            "bug",
            "feature",
            "improvement",
            "refactor",
            "docs",
            "chore",
            "research",
        ],
    ),
    ("Priority", &["P0", "P1", "P2", "P3"]),
    ("Blocked", &["No", "Yes"]),
    (
        "Resolution",
        &["Completed", "Cancelled", "Duplicate", "Invalid"],
    ),
];

fn check(name: &str, status: &str, detail: Value) -> Value {
    json!({"name": name, "status": status, "detail": detail})
}

fn github_project_readiness(project: &Value, views: &Value) -> Value {
    let fields = project["fields"].as_array();
    let mut missing = Vec::new();
    if let Some(fields) = fields {
        for (name, required) in REQUIRED_PROJECT_FIELDS {
            let matching: Vec<_> = fields
                .iter()
                .filter(|field| field["name"].as_str() == Some(*name))
                .collect();
            let complete = matching.len() == 1
                && matching[0]["options"].as_array().is_some_and(|options| {
                    required.iter().all(|expected| {
                        options
                            .iter()
                            .filter(|option| option["name"].as_str() == Some(*expected))
                            .count()
                            == 1
                    })
                });
            if !complete {
                missing.push(*name);
            }
        }
    } else {
        missing.extend(REQUIRED_PROJECT_FIELDS.iter().map(|(name, _)| *name));
    }
    let board_ready = views["views"].as_array().is_some_and(|views| {
        views.iter().any(|view| {
            view["layout"] == "BOARD_LAYOUT"
                && view["filter"].as_str().unwrap_or("").is_empty()
                && view["verticalGroupByFields"]["nodes"]
                    .as_array()
                    .is_some_and(|fields| {
                        fields.len() == 1 && fields[0]["name"].as_str() == Some("Status")
                    })
        })
    });
    json!({
        "ready": missing.is_empty() && board_ready && project["closed"] == false,
        "project": project["url"],
        "closed": project["closed"],
        "missing_or_invalid_fields": missing,
        "status_grouped_board": board_ready
    })
}

fn gitlab_board_readiness(
    labels: &[Value],
    boards: &Value,
    board: Option<&Value>,
    board_name: &str,
) -> Value {
    let visible: BTreeSet<_> = labels
        .iter()
        .filter_map(|label| label["name"].as_str())
        .collect();
    let missing_labels: Vec<_> = WORKFLOW_STAGE_LABELS
        .iter()
        .filter(|name| !visible.contains(**name))
        .copied()
        .collect();
    let matching_boards = boards.as_array().map_or(0, |items| {
        items
            .iter()
            .filter(|item| item["name"].as_str() == Some(board_name))
            .count()
    });
    let lists: BTreeSet<_> = board
        .and_then(|value| value["lists"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|list| list["label"]["name"].as_str())
        .collect();
    let missing_lists: Vec<_> = WORKFLOW_STAGE_LABELS
        .iter()
        .filter(|name| !lists.contains(**name))
        .copied()
        .collect();
    json!({
        "ready": missing_labels.is_empty() && matching_boards == 1 && missing_lists.is_empty(),
        "board_name": board_name,
        "matching_boards": matching_boards,
        "missing_labels": missing_labels,
        "missing_lists": missing_lists
    })
}

pub async fn inspect(
    config: &Config,
    transport: &dyn Transport,
    target: Target,
    workflow: Option<&WorkflowConfig>,
    board_name: &str,
) -> Result<Value> {
    let platform = target.platform;
    let endpoint = match platform {
        Platform::Github => config.github_api_url.as_str(),
        Platform::Gitlab => config
            .gitlab_url
            .as_ref()
            .map(|url| url.as_str())
            .ok_or_else(|| Error::new("configuration", "缺少 ISSUEFLOW_GITLAB_URL"))?,
    };
    let identity = transport.request(Method::GET, "user", None).await?;
    let username = identity["login"]
        .as_str()
        .or_else(|| identity["username"].as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::new("response", "Authentication response has no user identity"))?;
    let repository = transport
        .request(
            Method::GET,
            &match platform {
                Platform::Github => format!("repos/{}", target.repository),
                Platform::Gitlab => format!("projects/{}", encode(&target.repository)),
            },
            None,
        )
        .await?;
    let actual_repository = match platform {
        Platform::Github => repository["full_name"].as_str(),
        Platform::Gitlab => repository["path_with_namespace"].as_str(),
    };
    if !actual_repository.is_some_and(|value| value.eq_ignore_ascii_case(&target.repository)) {
        return Err(Error::new(
            "response",
            "Repository response does not match configured repository",
        ));
    }

    let kanban = match platform {
        Platform::Github => match workflow {
            None => {
                json!({"ready":false,"state":"not_configured","reason":"No workflow configuration was supplied or found"})
            }
            Some(workflow) => {
                if workflow.platform()? != platform
                    || !workflow.repository.eq_ignore_ascii_case(&target.repository)
                {
                    return Err(Error::new(
                        "configuration",
                        "Workflow configuration does not match the selected platform and repository",
                    ));
                }
                match workflow.github_project_url.as_deref() {
                    None => {
                        json!({"ready":false,"state":"not_configured","reason":"github_project_url is not configured"})
                    }
                    Some(project_url) => {
                        let projects = Projects {
                            transport,
                            target: ProjectTarget::parse(config, project_url)?,
                        };
                        let project = projects.show().await?;
                        let views = projects.view_list().await?;
                        github_project_readiness(&project, &views)
                    }
                }
            }
        },
        Platform::Gitlab => {
            if board_name.trim().is_empty() {
                return Err(Error::new("input", "Board name cannot be empty"));
            }
            let service = Service {
                transport,
                target: target.clone(),
            };
            let labels = service
                .pages(&format!("projects/{}/labels", encode(&target.repository)))
                .await?;
            let boards_service = Boards {
                transport,
                target: target.clone(),
            };
            let boards = boards_service.list().await?;
            let matches: Vec<_> = boards
                .as_array()
                .into_iter()
                .flatten()
                .filter(|board| board["name"].as_str() == Some(board_name))
                .collect();
            let board = if matches.len() == 1 {
                let id = matches[0]["id"]
                    .as_u64()
                    .ok_or_else(|| Error::new("response", "GitLab board has no id"))?;
                Some(boards_service.show(id).await?)
            } else {
                None
            };
            gitlab_board_readiness(&labels, &boards, board.as_ref(), board_name)
        }
    };
    let healthy = kanban["ready"] == true;
    Ok(json!({
        "healthy": healthy,
        "platform": platform,
        "repository": target.repository,
        "endpoint": endpoint,
        "checks": [
            check("configuration", "passed", json!({"platform":platform,"repository":actual_repository})),
            check("endpoint", "passed", json!({"url":endpoint,"reachable":true})),
            check("authentication", "passed", json!({"identity":username})),
            check("repository_access", "passed", json!({"repository":actual_repository,"read":true})),
            check("kanban_readiness", if healthy {"passed"} else {"incomplete"}, kanban)
        ],
        "permissions": {
            "read": "verified",
            "write": "unknown",
            "note": "Read-only diagnostics do not prove mutation permissions."
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{HashMap, VecDeque},
        sync::Mutex,
    };

    use async_trait::async_trait;

    struct Mock {
        replies: Mutex<VecDeque<Value>>,
        calls: Mutex<Vec<(Method, String)>>,
    }

    #[async_trait]
    impl Transport for Mock {
        async fn request(
            &self,
            method: Method,
            endpoint: &str,
            _body: Option<Value>,
        ) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((method, endpoint.to_owned()));
            Ok(self.replies.lock().unwrap().pop_front().unwrap())
        }
    }

    #[test]
    fn github_readiness_requires_all_fields_and_status_board() {
        let fields: Vec<_> = REQUIRED_PROJECT_FIELDS
            .iter()
            .enumerate()
            .map(|(index, (name, options))| {
                json!({"id":format!("F{index}"),"name":name,"options":options.iter().map(|name|json!({"name":name})).collect::<Vec<_>>()})
            })
            .collect();
        let project =
            json!({"url":"https://github.com/users/a/projects/1","closed":false,"fields":fields});
        let views = json!({"views":[{"layout":"BOARD_LAYOUT","filter":"","verticalGroupByFields":{"nodes":[{"name":"Status"}]}}]});
        assert_eq!(github_project_readiness(&project, &views)["ready"], true);
    }

    #[test]
    fn gitlab_readiness_reports_missing_lists() {
        let labels: Vec<_> = WORKFLOW_STAGE_LABELS
            .iter()
            .map(|name| json!({"name":name}))
            .collect();
        let boards = json!([{"id":1,"name":"Issueflow Workflow"}]);
        let board = json!({"lists":[]});
        let result = gitlab_board_readiness(&labels, &boards, Some(&board), "Issueflow Workflow");
        assert_eq!(result["ready"], false);
        assert_eq!(result["missing_lists"].as_array().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn inspect_verifies_identity_and_repository_but_keeps_write_unknown() {
        let config = Config::resolve(
            HashMap::new(),
            [
                ("ISSUEFLOW_PLATFORM".into(), "github".into()),
                ("ISSUEFLOW_REPOSITORY".into(), "owner/repo".into()),
            ]
            .into(),
            Default::default(),
        )
        .unwrap();
        let mock = Mock {
            replies: Mutex::new(
                vec![
                    json!({"login":"octocat"}),
                    json!({"full_name":"owner/repo"}),
                ]
                .into(),
            ),
            calls: Mutex::new(Vec::new()),
        };
        let result = inspect(
            &config,
            &mock,
            Target::defaults(&config).unwrap(),
            None,
            "Issueflow Workflow",
        )
        .await
        .unwrap();
        assert_eq!(result["healthy"], false);
        assert_eq!(result["permissions"]["write"], "unknown");
        assert_eq!(result["checks"][4]["status"], "incomplete");
        assert_eq!(
            *mock.calls.lock().unwrap(),
            vec![
                (Method::GET, "user".into()),
                (Method::GET, "repos/owner/repo".into())
            ]
        );
    }
}
