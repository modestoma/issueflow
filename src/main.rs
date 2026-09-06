use std::{collections::HashMap, io::IsTerminal, path::PathBuf, process::ExitCode};

use clap::{CommandFactory, Parser, Subcommand};
use issueflow::config::{Config, Overrides, read_env_file};
use issueflow::{
    error::{Error, Result},
    service::{CloseReason, Service, Stage},
    target::Target,
    transport::SdkTransport,
};
use serde_json::{Value, json};
use std::io::Read;

#[derive(Parser)]
#[command(version, about = "GitHub / GitLab issue maintenance CLI")]
struct Cli {
    /// Show additional safe diagnostic detail in human-readable output
    #[arg(long, global = true)]
    verbose: bool,
    /// Always emit deterministic JSON, including in an interactive terminal
    #[arg(long, global = true)]
    json: bool,
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
    /// Show installed GitHub and GitLab support; does not inspect configuration or permissions
    Capabilities,
    /// Read and maintain native parent/child relationships
    #[command(subcommand, hide = true)]
    Hierarchy(HierarchyCommand),
    /// Read and initialize GitLab project issue boards
    #[command(subcommand, hide = true)]
    Board(BoardCommand),
    /// Validate effective or secret-free project configuration
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Inspect and reconcile issue delivery state
    #[command(subcommand)]
    Delivery(DeliveryCommand),
    /// Create, inspect and explicitly merge GitHub PRs and same-project GitLab MRs
    #[command(subcommand)]
    Pr(PullCommand),
    /// Read and manage GitHub Projects v2
    #[command(subcommand, hide = true)]
    Project(ProjectCommand),
    /// Manage GitHub Projects and GitLab Issue Boards through one Kanban workflow
    #[command(subcommand)]
    Kanban(KanbanCommand),
    /// Run read-only configuration, API, repository, and Kanban diagnostics
    Doctor {
        /// Secret-free project workflow configuration used to locate GitHub Projects
        #[arg(long)]
        config_file: Option<PathBuf>,
        /// Expected GitLab workflow board name
        #[arg(long, default_value = "Issueflow Workflow")]
        board_name: String,
    },
    /// Create missing workflow labels in the default GitLab repository
    #[command(hide = true)]
    SetupLabels,
    /// Read and maintain issues using their full platform URLs
    #[command(subcommand)]
    Issue(IssueCommand),
    /// Deprecated compatibility alias for delivery and configuration validation
    #[command(subcommand, hide = true)]
    Workflow(DeliveryCommand),
}

#[derive(Subcommand)]
enum KanbanCommand {
    /// List Kanban containers for the configured platform
    List {
        /// GitHub user or organization; required for GitHub
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, value_enum, default_value = "user")]
        owner_type: ProjectOwner,
    },
    /// Show a GitHub Project URL or a GitLab board ID
    Show { target: String },
    /// Create or reuse a unique Kanban container
    Create {
        /// GitHub user or organization; required for GitHub
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, value_enum, default_value = "user")]
        owner_type: ProjectOwner,
        #[arg(long)]
        name: String,
    },
    /// Initialize the canonical workflow; GitLab may omit the ID to create/reuse by name
    Init {
        target: Option<String>,
        #[arg(long, default_value = "Issueflow Workflow")]
        name: String,
    },
    /// List items in a GitHub Project
    Items { target: String },
    /// Add an existing issue to a GitHub Project
    Add { target: String, issue_url: String },
    /// Read or set a GitHub Project Status
    Status {
        target: String,
        issue_url: String,
        #[arg(long)]
        to: Option<String>,
    },
    /// Read, set, or clear a GitHub Project single-select field
    Field {
        target: String,
        issue_url: String,
        #[arg(long)]
        name: String,
        #[arg(long, conflicts_with = "clear")]
        to: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// List repositories linked to a GitHub Project
    Repositories { target: String },
    /// Link a GitHub Project to an explicit repository
    LinkRepository { target: String, repository: String },
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
enum ConfigCommand {
    /// Show effective configuration; credentials are always redacted
    Show,
    /// Validate a secret-free project workflow configuration without loading credentials
    Validate {
        #[arg(long, default_value = ".issue-workflow.json")]
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum DeliveryCommand {
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
    /// Deprecated compatibility alias for config validate
    #[command(hide = true)]
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
    json!({"name":c.get_name(),"options":c.get_arguments().filter_map(|a|a.get_long()).collect::<Vec<_>>(),"subcommands":c.get_subcommands().filter(|subcommand| !subcommand.is_hide_set()).map(command_schema).collect::<Vec<_>>()})
}

fn capabilities() -> Value {
    let schema = command_schema(&Cli::command());
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "capability_schema_version": 2,
        "scope": "installed_support",
        "configured_platform": Value::Null,
        "remote_permissions_checked": false,
        "platforms": {
            "github": {
                "issues": "supported",
                "sub_issues": "supported",
                "dependencies": "supported",
                "pull_requests": "supported",
                "kanban": "GitHub Projects v2",
                "delivery_recovery": "supported"
            },
            "gitlab": {
                "issues": "supported",
                "sub_issues": "same-project Issue to Task",
                "dependencies": "supported",
                "merge_requests": "same-project",
                "kanban": "GitLab Issue Boards",
                "delivery_recovery": "supported"
            }
        },
        "limitations": [
            "This command reports installed support, not the selected platform or live permissions.",
            "GitHub Projects currently supports github.com only.",
            "GitLab hierarchy is limited to same-project Issue to Task relationships.",
            "GitLab merge requests must use branches in the same project."
        ],
        "cli_schema": schema.clone(),
        "cli": schema
    })
}
#[derive(Clone, Copy)]
struct OutputOptions {
    json: bool,
    verbose: bool,
}

fn finish(result: Result<Value>, options: OutputOptions) -> ExitCode {
    match result {
        Ok(v) => {
            if issueflow::output::use_json(options.json, std::io::stdout().is_terminal()) {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&v).expect("JSON serialization")
                );
            } else {
                println!("{}", issueflow::output::render_success(&v, options.verbose));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if issueflow::output::use_json(options.json, std::io::stderr().is_terminal()) {
                eprintln!("{}", json!({"error":e}));
            } else {
                eprintln!("{}", issueflow::output::render_error(&e, options.verbose));
            }
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
        /// Verify that the recovered GitHub issue belongs to this parent
        #[arg(long)]
        parent: Option<String>,
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
        /// Create a native GitHub Sub-issue under this parent
        #[arg(long)]
        parent: Option<String>,
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
    /// Read the native parent issue
    Parent { url: String },
    /// List native Sub-issues, optionally traversing a bounded tree
    SubIssues {
        url: String,
        #[arg(long)]
        recursive: bool,
        #[arg(long, requires = "recursive", value_parser = clap::value_parser!(u8).range(1..=8))]
        depth: Option<u8>,
    },
    /// Read parent, Sub-issues and both dependency directions
    Relationships { url: String },
    /// Add an existing native Sub-issue
    AddSubIssue {
        parent_url: String,
        child_url: String,
        #[arg(long)]
        move_from: Option<String>,
    },
    /// Remove a native Sub-issue from the requested parent
    RemoveSubIssue {
        parent_url: String,
        child_url: String,
    },
    /// Remove the current native parent; no parent is a no-op
    RemoveParent { url: String },
    /// Reprioritize a GitHub Sub-issue relative to one sibling
    MoveSubIssue {
        parent_url: String,
        child_url: String,
        #[arg(long, conflicts_with = "after", required_unless_present = "after")]
        before: Option<String>,
        #[arg(long, conflicts_with = "before", required_unless_present = "before")]
        after: Option<String>,
    },
    /// List issues that block this issue
    BlockedBy { url: String },
    /// List issues that this issue blocks
    Blocking { url: String },
    /// Deprecated spelling of blocked-by
    #[command(hide = true)]
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
            | Self::Parent { url }
            | Self::SubIssues { url, .. }
            | Self::Relationships { url }
            | Self::RemoveParent { url }
            | Self::BlockedBy { url }
            | Self::Blocking { url }
            | Self::Dependencies { url }
            | Self::AddDependency { url, .. }
            | Self::RemoveDependency { url, .. } => Some(url),
            Self::AddSubIssue { parent_url, .. }
            | Self::RemoveSubIssue { parent_url, .. }
            | Self::MoveSubIssue { parent_url, .. } => Some(parent_url),
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
    if matches!(command, Command::SetupLabels) {
        eprintln!("warning: `setup-labels` is deprecated; use `kanban init` instead");
    }
    if let Command::Config { command } = command {
        debug_assert!(matches!(command, None | Some(ConfigCommand::Show)));
        return Ok(config.redacted());
    }
    if let Command::Doctor {
        config_file,
        board_name,
    } = command
    {
        let target = Target::defaults(&config)?;
        let transport = SdkTransport::new(&config, target.platform)?;
        let workflow_path = config_file.or_else(|| {
            let default = PathBuf::from(".issue-workflow.json");
            default.exists().then_some(default)
        });
        let workflow = workflow_path
            .as_ref()
            .map(|path| input::<issueflow::workflow_config::WorkflowConfig>(path))
            .transpose()?;
        if let Some(workflow) = &workflow {
            workflow.validate()?;
        }
        return issueflow::doctor::inspect(
            &config,
            &transport,
            target,
            workflow.as_ref(),
            &board_name,
        )
        .await;
    }
    if let Command::Hierarchy(command) = command {
        eprintln!(
            "warning: `hierarchy` is deprecated; use the corresponding `issue` Sub-issue command"
        );
        let parent_url = match &command {
            HierarchyCommand::Parent { issue_url } | HierarchyCommand::Children { issue_url } => {
                issue_url
            }
            HierarchyCommand::AddChild { parent_url, .. }
            | HierarchyCommand::RemoveChild { parent_url, .. } => parent_url,
        };
        let parent = issueflow::hierarchy::target_from_url(&config, parent_url)?;
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
                    .add_child(&issueflow::hierarchy::target_from_url(&config, &child_url)?)
                    .await
            }
            HierarchyCommand::RemoveChild { child_url, .. } => {
                hierarchy
                    .remove_child(&issueflow::hierarchy::target_from_url(&config, &child_url)?)
                    .await
            }
        };
    }
    if let Command::Kanban(command) = command {
        use issueflow::{
            board::Boards,
            config::Platform,
            project::{ProjectTarget, Projects},
        };

        let github_target = |target: &str, operation: &str| -> Result<ProjectTarget> {
            if config.platform == Some(Platform::Gitlab) && !target.starts_with("https://") {
                return Err(Error::new(
                    "input",
                    format!("kanban {operation} is not supported for GitLab Issue Boards"),
                ));
            }
            ProjectTarget::parse(&config, target)
        };
        let gitlab_board = |target: &str| -> Result<u64> {
            if config.platform != Some(Platform::Gitlab) {
                return Err(Error::new(
                    "configuration",
                    "A numeric Kanban target requires --platform gitlab and a repository",
                ));
            }
            target
                .parse::<u64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| Error::new("input", "GitLab board ID must be a positive integer"))
        };
        return match command {
            KanbanCommand::List { owner, owner_type } => match config.platform {
                Some(Platform::Github) => {
                    let owner = owner.ok_or_else(|| {
                        Error::new("input", "GitHub kanban list requires --owner")
                    })?;
                    let target = ProjectTarget::parse(
                        &config,
                        &format!(
                            "https://github.com/{}/{owner}/projects/1",
                            owner_type.path()
                        ),
                    )?;
                    let transport = SdkTransport::new(&config, Platform::Github)?;
                    Projects {
                        transport: &transport,
                        target,
                    }
                    .owner_projects()
                    .await
                }
                Some(Platform::Gitlab) => {
                    if owner.is_some() {
                        return Err(Error::new(
                            "input",
                            "GitLab kanban list does not accept --owner",
                        ));
                    }
                    let target = Target::defaults(&config)?;
                    let transport = SdkTransport::new(&config, Platform::Gitlab)?;
                    Boards {
                        transport: &transport,
                        target,
                    }
                    .list()
                    .await
                }
                None => Err(Error::new(
                    "configuration",
                    "kanban list requires --platform github or --platform gitlab",
                )),
            },
            KanbanCommand::Create {
                owner,
                owner_type,
                name,
            } => match config.platform {
                Some(Platform::Github) => {
                    let owner = owner.ok_or_else(|| {
                        Error::new("input", "GitHub kanban create requires --owner")
                    })?;
                    let target = ProjectTarget::parse(
                        &config,
                        &format!(
                            "https://github.com/{}/{owner}/projects/1",
                            owner_type.path()
                        ),
                    )?;
                    let transport = SdkTransport::new(&config, Platform::Github)?;
                    Projects {
                        transport: &transport,
                        target,
                    }
                    .create(&name)
                    .await
                }
                Some(Platform::Gitlab) => {
                    if owner.is_some() {
                        return Err(Error::new(
                            "input",
                            "GitLab kanban create does not accept --owner",
                        ));
                    }
                    let target = Target::defaults(&config)?;
                    let transport = SdkTransport::new(&config, Platform::Gitlab)?;
                    Boards {
                        transport: &transport,
                        target,
                    }
                    .create(&name)
                    .await
                }
                None => Err(Error::new(
                    "configuration",
                    "kanban create requires --platform github or --platform gitlab",
                )),
            },
            KanbanCommand::Show { target } => {
                if target.starts_with("https://") {
                    let project = ProjectTarget::parse(&config, &target)?;
                    let transport = SdkTransport::new(&config, Platform::Github)?;
                    Projects {
                        transport: &transport,
                        target: project,
                    }
                    .show()
                    .await
                } else {
                    let id = gitlab_board(&target)?;
                    let board_target = Target::defaults(&config)?;
                    let transport = SdkTransport::new(&config, Platform::Gitlab)?;
                    Boards {
                        transport: &transport,
                        target: board_target,
                    }
                    .show(id)
                    .await
                }
            }
            KanbanCommand::Init { target, name } => match target {
                Some(target) if target.starts_with("https://") => {
                    let project = ProjectTarget::parse(&config, &target)?;
                    let repository = config.repository.clone().ok_or_else(|| {
                        Error::new(
                            "configuration",
                            "GitHub kanban init requires --repository for native linkage verification",
                        )
                    })?;
                    let transport = SdkTransport::new(&config, Platform::Github)?;
                    let projects = Projects {
                        transport: &transport,
                        target: project,
                    };
                    let repository_link = projects.link_repository(&repository).await?;
                    let workflow = projects.init_workflow().await?;
                    Ok(json!({
                        "platform":"github",
                        "repository_link":repository_link,
                        "workflow":workflow
                    }))
                }
                Some(target) => {
                    let id = gitlab_board(&target)?;
                    let board_target = Target::defaults(&config)?;
                    let transport = SdkTransport::new(&config, Platform::Gitlab)?;
                    Boards {
                        transport: &transport,
                        target: board_target,
                    }
                    .init_workflow_target(&name, Some(id))
                    .await
                }
                None if config.platform == Some(Platform::Gitlab) => {
                    let board_target = Target::defaults(&config)?;
                    let transport = SdkTransport::new(&config, Platform::Gitlab)?;
                    Boards {
                        transport: &transport,
                        target: board_target,
                    }
                    .init_workflow(&name)
                    .await
                }
                None => Err(Error::new(
                    "input",
                    "GitHub kanban init requires a canonical Project URL",
                )),
            },
            KanbanCommand::Items { target } => {
                let project = github_target(&target, "items")?;
                let transport = SdkTransport::new(&config, Platform::Github)?;
                Projects {
                    transport: &transport,
                    target: project,
                }
                .items()
                .await
            }
            KanbanCommand::Add { target, issue_url } => {
                let project = github_target(&target, "add")?;
                let issue = Target::from_url(&config, &issue_url)?;
                if issue.platform != Platform::Github {
                    return Err(Error::new(
                        "input",
                        "GitHub kanban add requires a GitHub issue",
                    ));
                }
                let transport = SdkTransport::new(&config, Platform::Github)?;
                Projects {
                    transport: &transport,
                    target: project,
                }
                .add(&issue)
                .await
            }
            KanbanCommand::Status {
                target,
                issue_url,
                to,
            } => {
                let project = github_target(&target, "status")?;
                let issue = Target::from_url(&config, &issue_url)?;
                if issue.platform != Platform::Github {
                    return Err(Error::new(
                        "input",
                        "GitHub kanban status requires a GitHub issue",
                    ));
                }
                let transport = SdkTransport::new(&config, Platform::Github)?;
                Projects {
                    transport: &transport,
                    target: project,
                }
                .status(&issue, to.as_deref())
                .await
            }
            KanbanCommand::Field {
                target,
                issue_url,
                name,
                to,
                clear,
            } => {
                let project = github_target(&target, "field")?;
                let issue = Target::from_url(&config, &issue_url)?;
                if issue.platform != Platform::Github {
                    return Err(Error::new(
                        "input",
                        "GitHub kanban field requires a GitHub issue",
                    ));
                }
                let transport = SdkTransport::new(&config, Platform::Github)?;
                Projects {
                    transport: &transport,
                    target: project,
                }
                .field(&issue, &name, to.as_deref(), clear)
                .await
            }
            KanbanCommand::Repositories { target } => {
                let project = github_target(&target, "repositories")?;
                let transport = SdkTransport::new(&config, Platform::Github)?;
                Projects {
                    transport: &transport,
                    target: project,
                }
                .repositories()
                .await
            }
            KanbanCommand::LinkRepository { target, repository } => {
                let project = github_target(&target, "link-repository")?;
                let transport = SdkTransport::new(&config, Platform::Github)?;
                Projects {
                    transport: &transport,
                    target: project,
                }
                .link_repository(&repository)
                .await
            }
        };
    }
    if let Command::Board(command) = command {
        eprintln!("warning: `board` is deprecated; use `kanban` instead");
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
    if let Command::Delivery(command) | Command::Workflow(command) = command {
        let (args, apply, expected, cleanup) = match command {
            DeliveryCommand::Inspect(args) => (args, false, None, None),
            DeliveryCommand::CleanupCheck {
                args,
                worktree,
                confirm_no_dependent_work,
            } => (
                args,
                false,
                None,
                Some((worktree, confirm_no_dependent_work)),
            ),
            DeliveryCommand::Reconcile {
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
        eprintln!("warning: `project` is deprecated; use `kanban` instead");
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
            IssueCommand::RecoverCreate { request_id, parent } => {
                let parent = parent
                    .as_deref()
                    .map(|url| issueflow::hierarchy::target_from_url(&config, url))
                    .transpose()?;
                service
                    .recover_create_with_parent(&request_id, parent.as_ref())
                    .await
            }
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
            IssueCommand::Create {
                file,
                request_id,
                parent,
            } => {
                let parent = parent
                    .as_deref()
                    .map(|url| issueflow::hierarchy::target_from_url(&config, url))
                    .transpose()?;
                service
                    .create_with_parent(
                        input(&file)?,
                        &request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        parent.as_ref(),
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
            IssueCommand::Parent { .. } => {
                issueflow::hierarchy::Hierarchy {
                    transport: &transport,
                    parent: service.target.clone(),
                }
                .parent()
                .await
            }
            IssueCommand::SubIssues {
                recursive, depth, ..
            } => {
                issueflow::hierarchy::Hierarchy {
                    transport: &transport,
                    parent: service.target.clone(),
                }
                .sub_issues(recursive, depth)
                .await
            }
            IssueCommand::Relationships { .. } => {
                issueflow::hierarchy::Hierarchy {
                    transport: &transport,
                    parent: service.target.clone(),
                }
                .relationships()
                .await
            }
            IssueCommand::AddSubIssue {
                child_url,
                move_from,
                ..
            } => {
                let child = issueflow::hierarchy::target_from_url(&config, &child_url)?;
                let old_parent = move_from
                    .as_deref()
                    .map(|url| issueflow::hierarchy::target_from_url(&config, url))
                    .transpose()?;
                issueflow::hierarchy::Hierarchy {
                    transport: &transport,
                    parent: service.target.clone(),
                }
                .add_child_with_move(&child, old_parent.as_ref())
                .await
            }
            IssueCommand::RemoveSubIssue { child_url, .. } => {
                let child = issueflow::hierarchy::target_from_url(&config, &child_url)?;
                issueflow::hierarchy::Hierarchy {
                    transport: &transport,
                    parent: service.target.clone(),
                }
                .remove_child(&child)
                .await
            }
            IssueCommand::RemoveParent { .. } => {
                issueflow::hierarchy::Hierarchy {
                    transport: &transport,
                    parent: service.target.clone(),
                }
                .remove_parent()
                .await
            }
            IssueCommand::MoveSubIssue {
                child_url,
                before,
                after,
                ..
            } => {
                let child = issueflow::hierarchy::target_from_url(&config, &child_url)?;
                let before = before
                    .as_deref()
                    .map(|url| issueflow::hierarchy::target_from_url(&config, url))
                    .transpose()?;
                let after = after
                    .as_deref()
                    .map(|url| issueflow::hierarchy::target_from_url(&config, url))
                    .transpose()?;
                issueflow::hierarchy::Hierarchy {
                    transport: &transport,
                    parent: service.target.clone(),
                }
                .move_child(&child, before.as_ref(), after.as_ref())
                .await
            }
            IssueCommand::BlockedBy { .. } | IssueCommand::Dependencies { .. } => {
                service.blocked_by().await
            }
            IssueCommand::Blocking { .. } => service.blocking().await,
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
    let output = OutputOptions {
        json: cli.json,
        verbose: cli.verbose,
    };
    match &cli.command {
        Command::Workflow(_) => {
            eprintln!("warning: 'workflow' is deprecated; use 'delivery', or 'config validate'")
        }
        Command::Config { command: None } => {
            eprintln!("warning: bare 'config' is deprecated; use 'config show'")
        }
        _ => {}
    }
    match &cli.command {
        Command::Capabilities => {
            return finish(Ok(capabilities()), output);
        }
        Command::Delivery(DeliveryCommand::ValidateContract { file, parent_file })
        | Command::Workflow(DeliveryCommand::ValidateContract { file, parent_file }) => {
            let result = (|| {
                let c = input::<issueflow::branch_contract::BranchContract>(file)?;
                let parent = parent_file
                    .as_ref()
                    .map(|p| input::<issueflow::branch_contract::BranchContract>(p))
                    .transpose()?;
                c.validate(parent.as_ref())
            })();
            return finish(result, output);
        }
        Command::Config {
            command: Some(ConfigCommand::Validate { file }),
        } => {
            return finish(
                input::<issueflow::workflow_config::WorkflowConfig>(file)
                    .and_then(|v| v.validate()),
                output,
            );
        }
        Command::Workflow(DeliveryCommand::Validate { file }) => {
            return finish(
                input::<issueflow::workflow_config::WorkflowConfig>(file)
                    .and_then(|v| v.validate()),
                output,
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
    finish(result, output)
}
