use std::{collections::HashMap, path::PathBuf, process::ExitCode};

use clap::{CommandFactory, Parser, Subcommand};
use issueflow::config::{Config, Overrides, read_env_file};
use issueflow::{
    error::{Error, Result},
    service::{CloseReason, Service, Stage},
    target::Target,
    transport::{SdkTransport, Transport},
};
use serde_json::{Value, json};
use std::io::Read;

#[derive(Parser)]
#[command(version, about = "GitHub / GitLab issue maintenance CLI")]
struct Cli {
    /// Read this env file instead of .env in the current directory
    #[arg(long, global = true, conflicts_with = "no_env_file")]
    env_file: Option<PathBuf>,
    /// Ignore .env; process environment and explicit options still apply
    #[arg(long, global = true)]
    no_env_file: bool,
    #[command(flatten)]
    overrides: Overrides,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect installed command capabilities without loading credentials
    Capabilities,
    /// Read and maintain native parent/child relationships
    #[command(subcommand)]
    Hierarchy(HierarchyCommand),
    /// Read and initialize GitLab project issue boards
    #[command(subcommand)]
    Board(BoardCommand),
    /// Validate a secret-free GitHub workflow configuration (offline)
    #[command(subcommand)]
    Workflow(WorkflowCommand),
    /// Create, inspect and explicitly merge GitHub pull requests
    #[command(subcommand)]
    Pr(PullCommand),
    /// Read and manage GitHub Projects v2
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Show effective configuration as JSON; credentials are never included
    Config,
    /// Check API authentication for the configured platform (read-only)
    Doctor,
    /// Create missing workflow labels in the default GitLab repository
    SetupLabels,
    /// Read and maintain issues using their full platform URLs
    #[command(subcommand)]
    Issue(IssueCommand),
}

#[derive(Subcommand)]
enum HierarchyCommand {
    Parent {
        issue_url: String,
    },
    Children {
        issue_url: String,
    },
    AddChild {
        parent_url: String,
        child_url: String,
    },
    RemoveChild {
        parent_url: String,
        child_url: String,
    },
}

#[derive(Subcommand)]
enum BoardCommand {
    List,
    Show {
        id: u64,
    },
    InitWorkflow {
        #[arg(long, default_value = "Issueflow Workflow")]
        name: String,
    },
}

#[derive(Subcommand)]
enum WorkflowCommand {
    /// Read-only worktree cleanup eligibility; never deletes anything
    CleanupCheck {
        #[command(flatten)]
        args: RecoveryArgs,
        #[arg(long)]
        worktree: PathBuf,
        /// Assert no unpublished child issues/branches still depend on this branch
        #[arg(long)]
        confirm_no_dependent_work: bool,
    },
    /// Read remote delivery state and propose missing recovery steps
    Inspect(RecoveryArgs),
    /// Inspect by default; explicitly apply only missing eligible steps
    Reconcile {
        #[command(flatten)]
        args: RecoveryArgs,
        #[arg(long, requires = "expected_head_sha")]
        apply: bool,
        #[arg(long, requires = "apply")]
        expected_head_sha: Option<String>,
    },
    ValidateContract {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        parent_file: Option<PathBuf>,
    },
    Validate {
        #[arg(long, default_value = ".issue-workflow.json")]
        file: PathBuf,
    },
}

#[derive(clap::Args)]
struct RecoveryArgs {
    #[arg(long)]
    file: PathBuf,
    #[arg(long)]
    parent_file: Option<PathBuf>,
    #[arg(long, default_value = ".issue-workflow.json")]
    config_file: PathBuf,
    /// Assert human acceptance was explicitly confirmed; never inferred from CI
    #[arg(long)]
    accepted: bool,
}

fn command_schema(c: &clap::Command) -> Value {
    json!({"name":c.get_name(),"options":c.get_arguments().filter_map(|a|a.get_long()).collect::<Vec<_>>(),"subcommands":c.get_subcommands().map(command_schema).collect::<Vec<_>>()})
}
fn finish(result: Result<Value>) -> ExitCode {
    match result {
        Ok(v) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).expect("JSON serialization")
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", json!({"error":e}));
            ExitCode::from(e.exit_code())
        }
    }
}

#[derive(Subcommand)]
enum PullCommand {
    /// Inspect checks and review evidence for the current PR head
    Checks {
        url: String,
    },
    Update {
        url: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_head_sha: String,
    },
    Ready {
        url: String,
        #[arg(long)]
        expected_head_sha: String,
    },
    List {
        #[arg(long, value_enum, default_value = "open")]
        state: issueflow::pull::PullState,
        #[arg(long)]
        head: Option<String>,
        #[arg(long)]
        base: Option<String>,
    },
    Show {
        url: String,
    },
    Create {
        issue_url: String,
        #[arg(long)]
        file: PathBuf,
    },
    Merge {
        url: String,
        #[arg(long)]
        expected_head_sha: String,
        #[arg(long)]
        expected_base: String,
        #[arg(long, value_enum, default_value = "merge")]
        method: issueflow::pull::MergeMethod,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ProjectOwner {
    User,
    Organization,
}
impl ProjectOwner {
    fn path(self) -> &'static str {
        match self {
            Self::User => "users",
            Self::Organization => "orgs",
        }
    }
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// List native repository links
    Repositories {
        url: String,
    },
    /// Link this Project to an explicit owner/repository, with readback
    LinkRepository {
        url: String,
        repository: String,
    },
    Views {
        url: String,
    },
    InitWorkflow {
        url: String,
    },
    Field {
        url: String,
        issue_url: String,
        #[arg(long)]
        name: String,
        #[arg(long, conflicts_with = "clear")]
        to: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// List Projects belonging to an explicit GitHub owner
    List {
        #[arg(long)]
        owner: String,
        #[arg(long, value_enum, default_value = "user")]
        owner_type: ProjectOwner,
    },
    /// Create a Project or reuse a unique matching title; never retries mutations
    Create {
        #[arg(long)]
        owner: String,
        #[arg(long, value_enum, default_value = "user")]
        owner_type: ProjectOwner,
        #[arg(long)]
        title: String,
    },
    /// Add missing workflow Status options, preserving existing option IDs
    InitStatuses {
        url: String,
    },
    /// Read Project metadata and field options
    Show {
        url: String,
    },
    /// List all visible Project items and their Status
    Items {
        url: String,
    },
    /// Add an existing issue, reusing existing membership
    Add {
        url: String,
        issue_url: String,
    },
    /// Read Status, or set an exact option name (does not itself close issues)
    Status {
        url: String,
        issue_url: String,
        #[arg(long)]
        to: Option<String>,
    },
}

#[derive(Subcommand)]
enum IssueCommand {
    /// Read-only recovery lookup; never submits a create request
    RecoverCreate {
        #[arg(long)]
        request_id: String,
    },
    /// Preview or apply canonical GitLab workflow metadata migration
    ReconcileMetadata {
        url: String,
        #[arg(long)]
        apply: bool,
    },
    /// List all visible open and closed issues in the default repository
    List,
    /// Read an issue, optionally including all comments
    Show {
        url: String,
        #[arg(long)]
        comments: bool,
    },
    /// Read all issue comments
    Comments { url: String },
    /// Create from a JSON file; use - for stdin
    Create {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Update title/body from JSON, optionally checking the last update time
    Update {
        url: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_updated_at: Option<String>,
    },
    /// Publish a UTF-8 Markdown comment from a file or stdin
    Comment {
        url: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Read or add/remove labels without replacing unrelated labels
    Labels {
        url: String,
        #[arg(long)]
        add: Vec<String>,
        #[arg(long)]
        remove: Vec<String>,
    },
    /// Change the label-based workflow stage of an open GitLab issue
    Transition {
        url: String,
        #[arg(long, value_enum)]
        to: Stage,
    },
    /// Explicitly close with a completion or termination reason
    Close {
        url: String,
        #[arg(long, value_enum)]
        reason: CloseReason,
        /// Only change native issue state; preserve labels
        #[arg(long)]
        no_workflow_labels: bool,
    },
    /// Reopen an issue; GitLab also restores the Backlog stage
    Reopen {
        url: String,
        #[arg(long)]
        no_workflow_labels: bool,
    },
    /// List native blocking dependencies
    Dependencies { url: String },
    /// Mark the first issue as blocked by the second, checking for cycles
    AddDependency { url: String, blocker_url: String },
    /// Remove a native blocking relationship
    RemoveDependency { url: String, blocker_url: String },
}

impl IssueCommand {
    fn url(&self) -> Option<&str> {
        match self {
            Self::List | Self::Create { .. } | Self::RecoverCreate { .. } => None,
            Self::Show { url, .. }
            | Self::ReconcileMetadata { url, .. }
            | Self::Comments { url }
            | Self::Update { url, .. }
            | Self::Comment { url, .. }
            | Self::Labels { url, .. }
            | Self::Transition { url, .. }
            | Self::Close { url, .. }
            | Self::Reopen { url, .. }
            | Self::Dependencies { url }
            | Self::AddDependency { url, .. }
            | Self::RemoveDependency { url, .. } => Some(url),
        }
    }
}

fn read_file(path: &std::path::Path) -> Result<String> {
    if path == std::path::Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|_| Error::new("input", "无法读取标准输入"))?;
        Ok(text)
    } else {
        std::fs::read_to_string(path).map_err(|_| Error::new("input", "无法读取输入文件"))
    }
}
fn input<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<T> {
    serde_json::from_str(&read_file(path)?)
        .map_err(|_| Error::new("input", "输入 JSON 格式或字段无效"))
}

async fn execute(command: Command, config: Config) -> Result<Value> {
    if matches!(command, Command::Config) {
        return Ok(config.redacted());
    }
    if matches!(command, Command::Doctor) {
        let platform = config
            .platform
            .ok_or_else(|| Error::new("configuration", "doctor 需要指定平台"))?;
        let transport = SdkTransport::new(&config, platform)?;
        let user = transport.request(http::Method::GET, "user", None).await?;
        return Ok(
            json!({"platform": platform, "authenticated": true, "user": user["username"].as_str().or_else(|| user["login"].as_str())}),
        );
    }
    if let Command::Hierarchy(command) = command {
        let parent_url = match &command {
            HierarchyCommand::Parent { issue_url } | HierarchyCommand::Children { issue_url } => {
                issue_url
            }
            HierarchyCommand::AddChild { parent_url, .. }
            | HierarchyCommand::RemoveChild { parent_url, .. } => parent_url,
        };
        let parent = Target::from_url(&config, parent_url)?;
        let transport = SdkTransport::new(&config, parent.platform)?;
        let hierarchy = issueflow::hierarchy::Hierarchy {
            transport: &transport,
            parent,
        };
        return match command {
            HierarchyCommand::Parent { .. } => hierarchy.parent().await,
            HierarchyCommand::Children { .. } => hierarchy.children().await,
            HierarchyCommand::AddChild { child_url, .. } => {
                hierarchy
                    .add_child(&Target::from_url(&config, &child_url)?)
                    .await
            }
            HierarchyCommand::RemoveChild { child_url, .. } => {
                hierarchy
                    .remove_child(&Target::from_url(&config, &child_url)?)
                    .await
            }
        };
    }
    if let Command::Board(command) = command {
        let target = Target::defaults(&config)?;
        let transport = SdkTransport::new(&config, target.platform)?;
        let boards = issueflow::board::Boards {
            transport: &transport,
            target,
        };
        return match command {
            BoardCommand::List => boards.list().await,
            BoardCommand::Show { id } => boards.show(id).await,
            BoardCommand::InitWorkflow { name } => boards.init_workflow(&name).await,
        };
    }
    if let Command::Workflow(command) = command {
        let (args, apply, expected, cleanup) = match command {
            WorkflowCommand::Inspect(args) => (args, false, None, None),
            WorkflowCommand::CleanupCheck {
                args,
                worktree,
                confirm_no_dependent_work,
            } => (
                args,
                false,
                None,
                Some((worktree, confirm_no_dependent_work)),
            ),
            WorkflowCommand::Reconcile {
                args,
                apply,
                expected_head_sha,
            } => (args, apply, expected_head_sha, None),
            _ => unreachable!("offline workflow command handled before configuration"),
        };
        let contract = input::<issueflow::branch_contract::BranchContract>(&args.file)?;
        let parent = args
            .parent_file
            .as_ref()
            .map(|p| input::<issueflow::branch_contract::BranchContract>(p))
            .transpose()?;
        let workflow = input::<issueflow::workflow_config::WorkflowConfig>(&args.config_file)?;
        workflow.validate()?;
        contract.validate(parent.as_ref())?;
        let platform = workflow.platform()?;
        match platform {
            issueflow::config::Platform::Github
                if workflow.host != "github.com"
                    || config.github_api_url.as_str() != "https://api.github.com/" =>
            {
                return Err(Error::new(
                    "configuration",
                    "Workflow host does not match the configured GitHub API",
                ));
            }
            issueflow::config::Platform::Gitlab => {
                let host = config
                    .gitlab_url
                    .as_ref()
                    .and_then(url::Url::host_str)
                    .ok_or_else(|| {
                        Error::new(
                            "configuration",
                            "GitLab workflow requires --gitlab-url or ISSUEFLOW_GITLAB_URL",
                        )
                    })?;
                if !host.eq_ignore_ascii_case(&workflow.host) {
                    return Err(Error::new(
                        "configuration",
                        "Workflow host does not match the configured GitLab API",
                    ));
                }
            }
            _ => {}
        }
        let transport = SdkTransport::new(&config, platform)?;
        let recovery = issueflow::recovery::Recovery {
            config: &config,
            transport: &transport,
            contract: &contract,
            parent: parent.as_ref(),
            workflow: &workflow,
        };
        if let Some((path, confirmed)) = cleanup {
            return issueflow::cleanup::inspect(&recovery, &path, confirmed, args.accepted).await;
        }
        return recovery
            .reconcile(args.accepted, apply, expected.as_deref())
            .await;
    }
    if let Command::Pr(command) = command {
        use issueflow::pull::{Pulls, target_from_url};
        let target = match &command {
            PullCommand::List { .. } => Target::defaults(&config)?,
            PullCommand::Create { issue_url, .. } => Target::from_url(&config, issue_url)?,
            PullCommand::Show { url }
            | PullCommand::Merge { url, .. }
            | PullCommand::Update { url, .. }
            | PullCommand::Ready { url, .. }
            | PullCommand::Checks { url } => target_from_url(&config, url)?,
        };
        if matches!(command, PullCommand::Ready { .. })
            && target.platform == issueflow::config::Platform::Github
            && config.github_api_url.as_str() != "https://api.github.com/"
        {
            return Err(Error::new(
                "configuration",
                "PR ready currently supports github.com only",
            ));
        }
        let transport = SdkTransport::new(&config, target.platform)?;
        let service = Pulls {
            transport: &transport,
            target,
        };
        return match command {
            PullCommand::List { head, base, state } => {
                service
                    .list_state(head.as_deref(), base.as_deref(), state)
                    .await
            }
            PullCommand::Show { .. } => service.show().await,
            PullCommand::Checks { .. } => {
                issueflow::pull_checks::inspect(&transport, service.target).await
            }
            PullCommand::Update {
                file,
                expected_head_sha,
                ..
            } => service.update(input(&file)?, &expected_head_sha).await,
            PullCommand::Ready {
                expected_head_sha, ..
            } => service.ready(&expected_head_sha).await,
            PullCommand::Create { issue_url, file } => {
                service.create(input(&file)?, &issue_url).await
            }
            PullCommand::Merge {
                expected_head_sha,
                expected_base,
                method,
                ..
            } => {
                service
                    .merge(&expected_head_sha, &expected_base, method)
                    .await
            }
        };
    }
    if let Command::Project(command) = command {
        use issueflow::project::{ProjectTarget, Projects};
        let target = match &command {
            ProjectCommand::List { owner, owner_type }
            | ProjectCommand::Create {
                owner, owner_type, ..
            } => ProjectTarget::parse(
                &config,
                &format!(
                    "https://github.com/{}/{}/projects/1",
                    owner_type.path(),
                    owner
                ),
            )?,
            ProjectCommand::Repositories { url }
            | ProjectCommand::LinkRepository { url, .. }
            | ProjectCommand::Show { url }
            | ProjectCommand::Items { url }
            | ProjectCommand::InitStatuses { url }
            | ProjectCommand::InitWorkflow { url }
            | ProjectCommand::Views { url }
            | ProjectCommand::Field { url, .. }
            | ProjectCommand::Add { url, .. }
            | ProjectCommand::Status { url, .. } => ProjectTarget::parse(&config, url)?,
        };
        let issue = match &command {
            ProjectCommand::Add { issue_url, .. }
            | ProjectCommand::Status { issue_url, .. }
            | ProjectCommand::Field { issue_url, .. } => {
                let t = Target::from_url(&config, issue_url)?;
                if t.platform != issueflow::config::Platform::Github {
                    return Err(Error::new("input", "Projects requires a GitHub issue"));
                }
                Some(t)
            }
            _ => None,
        };
        let transport = SdkTransport::new(&config, issueflow::config::Platform::Github)?;
        let service = Projects {
            transport: &transport,
            target,
        };
        return match command {
            ProjectCommand::Repositories { .. } => service.repositories().await,
            ProjectCommand::LinkRepository { repository, .. } => {
                service.link_repository(&repository).await
            }
            ProjectCommand::Show { .. } => service.show().await,
            ProjectCommand::List { .. } => service.owner_projects().await,
            ProjectCommand::Create { title, .. } => service.create(&title).await,
            ProjectCommand::InitStatuses { .. } => service.init_statuses().await,
            ProjectCommand::InitWorkflow { .. } => service.init_workflow().await,
            ProjectCommand::Views { .. } => service.view_list().await,
            ProjectCommand::Field {
                name, to, clear, ..
            } => {
                service
                    .field(issue.as_ref().unwrap(), &name, to.as_deref(), clear)
                    .await
            }
            ProjectCommand::Items { .. } => service.items().await,
            ProjectCommand::Add { .. } => service.add(issue.as_ref().unwrap()).await,
            ProjectCommand::Status { to, .. } => {
                service.status(issue.as_ref().unwrap(), to.as_deref()).await
            }
        };
    }
    let target = match &command {
        Command::Issue(issue) => match issue.url() {
            Some(url) => Target::from_url(&config, url)?,
            None => Target::defaults(&config)?,
        },
        _ => Target::defaults(&config)?,
    };
    let transport = SdkTransport::new(&config, target.platform)?;
    let service = Service {
        transport: &transport,
        target,
    };
    match command {
        Command::SetupLabels => service.setup_labels().await,
        Command::Issue(command) => match command {
            IssueCommand::List => service.list().await,
            IssueCommand::RecoverCreate { request_id } => service.recover_create(&request_id).await,
            IssueCommand::ReconcileMetadata { apply, .. } => {
                service.reconcile_metadata(apply).await
            }
            IssueCommand::Show { comments, .. } => {
                let issue = service.show().await?;
                if comments {
                    Ok(json!({"issue": issue, "comments": service.comments().await?}))
                } else {
                    Ok(issue)
                }
            }
            IssueCommand::Comments { .. } => service.comments().await,
            IssueCommand::Create { file, request_id } => {
                service
                    .create(
                        input(&file)?,
                        &request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    )
                    .await
            }
            IssueCommand::Update {
                file,
                expected_updated_at,
                ..
            } => {
                service
                    .update(input(&file)?, expected_updated_at.as_deref())
                    .await
            }
            IssueCommand::Comment { file, .. } => service.comment(read_file(&file)?).await,
            IssueCommand::Labels { add, remove, .. } => service.labels(add, remove).await,
            IssueCommand::Transition { to, .. } => service.transition(to).await,
            IssueCommand::Close {
                reason,
                no_workflow_labels,
                ..
            } => {
                if no_workflow_labels {
                    service.native_state(Some(reason)).await
                } else {
                    service.close(reason).await
                }
            }
            IssueCommand::Reopen {
                no_workflow_labels, ..
            } => {
                if no_workflow_labels {
                    service.native_state(None).await
                } else {
                    service.reopen().await
                }
            }
            IssueCommand::Dependencies { .. } => service.dependencies().await,
            IssueCommand::AddDependency { blocker_url, .. } => {
                service
                    .add_dependency(&Target::from_url(&config, &blocker_url)?)
                    .await
            }
            IssueCommand::RemoveDependency { blocker_url, .. } => {
                service
                    .remove_dependency(&Target::from_url(&config, &blocker_url)?)
                    .await
            }
        },
        _ => unreachable!(),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Command::Capabilities => {
            return finish(Ok(
                json!({"version":env!("CARGO_PKG_VERSION"),"capability_schema_version":1,"cli":command_schema(&Cli::command())}),
            ));
        }
        Command::Workflow(WorkflowCommand::ValidateContract { file, parent_file }) => {
            let result = (|| {
                let c = input::<issueflow::branch_contract::BranchContract>(file)?;
                let parent = parent_file
                    .as_ref()
                    .map(|p| input::<issueflow::branch_contract::BranchContract>(p))
                    .transpose()?;
                c.validate(parent.as_ref())
            })();
            return finish(result);
        }
        Command::Workflow(WorkflowCommand::Validate { file }) => {
            return finish(
                input::<issueflow::workflow_config::WorkflowConfig>(file)
                    .and_then(|v| v.validate()),
            );
        }
        _ => {}
    }

    let result = (|| {
        let file = if cli.no_env_file {
            HashMap::new()
        } else {
            read_env_file(
                &cli.env_file
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(".env")),
                cli.env_file.is_some(),
            )?
        };
        let environment = std::env::vars_os()
            .filter_map(|(key, value)| {
                let key = key.into_string().ok()?;
                if !key.starts_with("ISSUEFLOW_") {
                    return None;
                }
                Some((key, value))
            })
            .map(|(key, value)| {
                value
                    .into_string()
                    .map(|value| (key, value))
                    .map_err(|_| issueflow::config::ConfigError("环境配置必须为 UTF-8".into()))
            })
            .collect::<std::result::Result<HashMap<_, _>, _>>()?;
        Config::resolve(file, environment, cli.overrides)
    })();
    let result = match result {
        Ok(config) => execute(cli.command, config).await,
        Err(error) => Err(Error::from(error)),
    };
    match result {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).expect("JSON value serialization")
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}", serde_json::json!({"error": error}));
            ExitCode::from(error.exit_code())
        }
    }
}
