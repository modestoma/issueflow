# issueflow

A Rust CLI for managing GitHub and GitLab issues. It accesses platform APIs directly through SDKs, without requiring `gh`, `glab`, or Python.

Supports reading, creating, updating, and commenting on issues; managing labels; closing and reopening issues; changing workflow stages; and managing native blocking dependencies. GitHub uses `octocrab =0.54.1`. GitLab uses the endpoint/query extension interfaces in `gitlab =0.1804.0` with a controlled HTTP client that supports instance base paths, timeouts, and disabled redirects. Endpoints not covered by the SDK use the same adapter. Development of this CLI does not use the issue-workflow workflow.

## Build and usage

```sh
cargo build --release
./target/release/issueflow --help
```

The examples below assume the built executable is available as `issueflow` on your `PATH`. You can also use `./target/release/issueflow` from the repository root.

After configuring credentials, start with a read-only connectivity check. `doctor` verifies authentication only; it does not establish repository write permissions:

```sh
issueflow --platform github doctor
issueflow --platform gitlab doctor
issueflow --platform github --repository owner/repo issue list
issueflow issue show https://github.com/owner/repo/issues/42 --comments
issueflow issue comments https://gitlab.example.com/group/repo/-/issues/42
```

Operations on a specific issue use the platform, repository, and native issue number from its URL, overriding the default target. The host must match the corresponding API configuration; credentials are never sent to unconfigured hosts. The GitHub Enterprise web host is inferred from the custom API URL. GitLab supports nested groups and instance base paths. No custom issue numbers are assigned; dates can be included directly in titles.

`issue list` returns all visible open and closed issues, excluding GitHub pull requests. Array endpoints are paginated up to 100,000 items. Exceeding this limit or encountering a failure during pagination produces an error rather than presenting partial results as complete.

### Create and update

Example `issue.json`:

```json
{
  "title": "[20260905] Improve error messages",
  "body": "## Background\n…\n\n## Acceptance criteria\n- [ ] …",
  "labels": ["type::improvement", "workflow::待复查", "priority::P2"]
}
```

```sh
issueflow --platform github --repository owner/repo setup-labels
issueflow --platform github --repository owner/repo issue create --file issue.json
issueflow issue update https://github.com/owner/repo/issues/42 --file changes.json --expected-updated-at '2026-09-05T08:00:00Z'
issueflow issue comment https://github.com/owner/repo/issues/42 --file progress.md
```

`changes.json` accepts only `title` and `body`. Only supplied, non-null fields are updated. An empty body clears the description while preserving internal retry markers. Input files are read as UTF-8; `--file -` reads standard input. Creation JSON accepts only `title`, `body`, and `labels`; unknown fields are rejected.

Creation appends an operation UUID comment to the body. The output includes the operation, native issue number, and URL. Use `--request-id UUID` to explicitly identify the same logical request. A retry scans all visible issues: an existing request ID with matching content reuses the issue, while different content or multiple matches produce a conflict. No local registry file is maintained.

**This is not a server-side idempotency guarantee.** Concurrent use of the same UUID, incomplete visibility, removed markers, or temporary listing delays can still cause duplicates. A write timeout reports `outcome_unknown: true` and the request ID; inspect the remote state before resending. Requests are never automatically retried and redirects are not followed. An unparseable success response is also treated as an unknown write outcome.

`--expected-updated-at` checks for stale data before writing. It is not an atomic platform compare-and-swap operation; another update can occur between the check and the write. Merge changes against the latest body.

### Labels and state

```sh
issueflow issue labels https://github.com/owner/repo/issues/42
issueflow issue labels https://github.com/owner/repo/issues/42 --add blocked --remove old-label
issueflow issue transition https://github.com/owner/repo/issues/42 --to in-progress
issueflow issue transition https://github.com/owner/repo/issues/42 --to awaiting-review
issueflow issue close https://github.com/owner/repo/issues/42 --reason completed
issueflow issue reopen https://github.com/owner/repo/issues/42
```

The supported stages are `triage`, `clarification`, `ready`, `in-progress`, and `awaiting-review`. They map to Chinese workflow labels used by the current implementation:

| Stage | Label |
| --- | --- |
| `triage` | `workflow::待复查` |
| `clarification` | `workflow::待明确` |
| `ready` | `workflow::就绪` |
| `in-progress` | `workflow::开发中` |
| `awaiting-review` | `workflow::待验收` |

A transition removes the previous workflow label, preserves other labels, and reads back the result for verification. A closed issue must be explicitly reopened before transitioning to an open stage.

Supported close reasons are `completed`, `cancelled`, `duplicate`, and `invalid`. Completion sets `workflow::已完成`; other reasons set `workflow::已终止` with a corresponding resolution label (`resolution::取消`, `resolution::重复`, or `resolution::失效`). On GitHub, all non-completion reasons use the native `not_planned` state reason. For duplicates, include the original issue URL in the body or a comment. Reopening clears old resolution labels and returns the issue to triage.

The CLI executes explicit commands. It does not determine whether a human has accepted the work, and it does not automatically merge or release anything. Use `close --reason completed` after human acceptance and delivery requirements are met. Label changes and closing involve multiple requests without a shared transaction. Partial success is reported as an error; reconcile against the remote state. `setup-labels` creates missing labels while preserving existing colors and unrelated labels. The general `labels` command can manage labels directly; use `transition`, `close`, and `reopen` to keep workflow stages and native issue state consistent.

### Dependencies

```sh
issueflow issue dependencies https://github.com/owner/repo/issues/42
issueflow issue add-dependency https://github.com/owner/repo/issues/42 https://github.com/owner/repo/issues/40
issueflow issue remove-dependency https://github.com/owner/repo/issues/42 https://github.com/owner/repo/issues/40
```

The first issue is **blocked by the second issue**. Before adding a dependency, the CLI traverses reachable blockers and checks for cycles, up to 1,000 nodes. GitHub uses the dependencies API; GitLab uses issue links. Dependencies may cross repositories but must remain on the same platform and instance. Unsupported versions or insufficient permissions produce API errors; ordinary related-to links are never presented as blocking relationships. Native GitLab blocking relationships depend on the instance license.

Dependency lists retain native relationship data. The CLI does not automatically unblock development or close issues, since a closed issue may have been cancelled. Checking and adding a dependency are not transactional across users; the remote state remains authoritative. Native parent-child hierarchy APIs, GitLab MR operations, and worktree automation are outside this version's scope. Parent/child issue URLs and branch contracts can be maintained in issue bodies.

## GitHub Projects

Projects v2 commands support user-owned and organization-owned projects on **github.com**. GitHub Enterprise Projects routing is not supported in this version; custom GitHub API bases are rejected by these commands. Existing issue commands retain their previous platform support.

Use a canonical Project URL, not a board view URL with `/views/N` or query parameters:

```sh
issueflow --no-env-file project show https://github.com/users/modestoma/projects/1
issueflow --no-env-file project items https://github.com/users/modestoma/projects/1
issueflow --no-env-file project add https://github.com/users/modestoma/projects/1 https://github.com/modestoma/issueflow/issues/4
issueflow --no-env-file project status https://github.com/users/modestoma/projects/1 https://github.com/modestoma/issueflow/issues/4
issueflow --no-env-file project status https://github.com/users/modestoma/projects/1 https://github.com/modestoma/issueflow/issues/4 --to 'In progress'
```

For organization projects, use `https://github.com/orgs/OWNER/projects/N`. Project and issue URLs are explicit; these commands do not require default platform/repository configuration. They use `ISSUEFLOW_GITHUB_TOKEN` and the same configuration precedence as other commands. `--no-env-file` ensures authentication comes from the process environment.

`show` returns metadata and field definitions, including Status option names and IDs. `items` lists all visible Project items with their Status, including draft issues and pull requests; hidden content may be null. Fields and items use cursor pagination, with repeated IDs/cursors and incomplete results treated as errors. The limit is 1,000 pages per connection.

`add` accepts an existing GitHub issue, checks membership, and reuses an existing item. `status` without `--to` reads its current Status. With `--to`, it resolves an exact, case-sensitive option name from the single-select field named `Status`. Missing or ambiguous fields/options are rejected. The issue must already belong to the Project; setting Status does not implicitly add it. Archived items cannot have Status changed through this command. Mutations are read back, and already-matching Status values do not trigger another mutation.

For a board using these stages, the recommended workflow mapping is:

| Workflow meaning | Project Status |
| --- | --- |
| Captured, being reviewed, or awaiting clarification | Backlog |
| Clarified and ready to start, with prerequisites available | Ready |
| Implementation and verification in progress | In progress |
| Awaiting review or human acceptance | In review |
| Accepted and delivered | Done |

Use an independently configured option such as `Cancelled` for termination, if available; do not interpret every closed issue as Done. The CLI does not create fields or options. Keep type, priority, and blocked labels independently.

Projects-backed GitHub workflows should use `project status` as the stage store instead of calling the label-based `issue transition`. `issue transition` remains label-based. For Project-backed workflows, close/reopen with `--no-workflow-labels` to change only native issue state and preserve all labels; update Project Status separately. This option also leaves resolution labels unchanged, so reconcile termination reasons explicitly when needed. Without the flag, existing close/reopen label behavior remains unchanged.

**Project Status and issue state are separate.** The CLI does not send issue-close mutations when changing Status. However, GitHub Project automations can close issues or overwrite Status when items change. Inspect the Project's Workflows settings before using live status writes; disable automatic closure if it would bypass human acceptance. Moving a card does not launch Codex or authorize merging or deployment.

### Projects authentication and failure handling

Issue write permissions alone are insufficient for Projects. For personal Projects, GitHub's GraphQL guide documents classic PAT scopes `read:project` for queries or `project` for queries and mutations. Retain the repository access needed for the issues being added (including `repo` for private repositories when using a classic PAT). An organization may instead use an appropriately authorized GitHub App. See [GitHub's Projects API guide](https://docs.github.com/en/issues/planning-and-tracking-with-projects/automating-your-project/using-the-api-to-manage-projects) for token support and permissions. Tokens belong in the environment or private configuration files, never command arguments.

HTTP-200 responses containing GraphQL errors are failures, even when partial data is present. Known insufficient-scope/forbidden errors return the permission exit code; raw error messages are not printed. Queries use POST but are classified as read-only for unknown-outcome reporting. Failed mutations or failed readback can report `outcome_unknown: true`; inspect Project items/Status before another write. There are no automatic mutation retries, background synchronization, or cross-user transactions.

Live validation against the configured personal Project passed metadata/options and item reads, existing membership reuse, invalid-option rejection, Status changes through Ready / In progress / In review, and matching-Status no-op behavior. The issue remained open. Creating a new Project membership has local test coverage but has not been exercised against a live account. Organization-owned Project URL parsing is tested; its GraphQL lookup has not been live-validated.

## GitHub pull requests and branch delivery

One issue can own a long-lived development branch with multiple commits. Git manages worktrees, branches, commits, and pushes; issueflow manages GitHub PRs. Branch names can use `feat/`, `fix/`, `refactor/`, `docs/`, or project conventions. A child issue's PR targets its parent integration branch, not necessarily `main`.

```sh
issueflow --platform github --repository owner/repo pr list --head feat/issue-5-example --base feat/issue-1-parent
issueflow pr create https://github.com/owner/repo/issues/5 --file pr.json
issueflow pr show https://github.com/owner/repo/pull/8
```

Example `pr.json` (UTF-8; `--file -` also accepts stdin):

```json
{
  "title": "Add the child feature",
  "body": "Describe the resulting behavior and validation.",
  "head": "feat/issue-5-example",
  "base": "feat/issue-1-parent",
  "draft": false
}
```

Push the head branch first. Head and base must be distinct branches in the same repository; fork heads are not supported in this version. Unknown JSON fields are rejected. The CLI appends `Refs <issue URL>` rather than an automatic closing keyword; do not include `Fixes` or `Closes` directives yourself if closure must wait for acceptance. PR output retains GitHub's native fields, including `html_url`, `head.sha`, and `base.ref`.

`pr list` returns open PRs with complete pagination. Creation reuses an existing open PR for the same head/base only when its body contains the same issue reference; it does not overwrite the existing PR. Reusing a PR does not update its title/body/draft state. Push further commits to the same branch to update its diff. If creation has an unknown outcome, inspect open PRs before retrying; a momentarily empty list is not proof of failure.

After explicit review and merge authorization:

```sh
issueflow pr merge https://github.com/owner/repo/pull/8 --expected-base feat/issue-1-parent --expected-head-sha FULL_40_CHARACTER_SHA --method merge
issueflow issue close https://github.com/owner/repo/issues/5 --reason completed --no-workflow-labels
```

Merge requires an open, non-draft PR with the expected target and full head SHA. The SHA is also passed to GitHub's merge endpoint; draft, stale expectations, unconfirmed merge responses, and failed readback are errors. Methods are `merge` (default), `squash`, and `rebase`. GitHub permissions and branch protections still apply. Target-branch validation is a read-before-write check, not an atomic lock against concurrent retargeting. This implementation handles ordinary PRs, not GitHub native stacked-PR batch merges or merge queues.

Merging does not invoke issue closure, delete branches, or set Project Status. Verify delivery to the intended base before explicitly closing the issue. A child is complete after acceptance into the parent integration branch; the parent still requires overall acceptance and its own PR into the original target. Project-backed closure uses `--no-workflow-labels`; reopening has the same flag. Multi-step delivery has no cross-API transaction and must be reconciled from remote state after partial failure.

Current commands do not edit PR metadata, read all review/check-run details, create GitLab MRs, or manage native sub-issue relationships. Review required checks using available repository evidence or the PR page before granting merge approval. [GitHub PR API reference](https://docs.github.com/en/rest/pulls/pulls).

## Output and errors

Successful commands write JSON to stdout; errors write JSON to stderr. Normalized issue results contain `platform`, `id`, `number`, `url`, `title`, `body`, `state`, `labels`, `created_at`, and `updated_at`. `id` is the platform-wide ID; `number` is the repository's native issue number (GitLab `iid`). Comments and dependencies retain platform-native JSON.

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Other API or network error |
| `2` | Input or configuration error |
| `3` | Authentication or permission error |
| `4` | Resource not found |
| `5` | Conflict |

Raw server error bodies are not printed, to avoid accidentally exposing credentials. The HTTP status is retained in `status`. Clap command usage errors use the standard CLI help format. A GitHub `403` can also indicate secondary rate limiting and requires interpretation in context.

## Workflow configuration and capability discovery

```sh
issueflow capabilities
issueflow workflow validate --file .issue-workflow.json
```

These commands run offline before `.env` or environment configuration is loaded. `capabilities` derives its command/option tree from the installed CLI parser, so different builds can be distinguished even when their package versions match. It does not imply remote permissions.

Workflow validation checks a GitHub-only, secret-free schema with `schema_version: 1`, `platform: "github"`, `host: "github.com"`, full `repository`, `remote`, `base_branch`, `timezone: "Asia/Shanghai"`, and explicit `permissions`. Optional fields include `proposer`, `verification_commands`, `manual_acceptance`, `delivery_condition`, `github_project_url`, and `branch_prefixes`. Unknown fields (including token fields) are rejected; configured commands are never executed. Validation neither changes API routing nor grants authorization. Existing API commands still use their normal explicit flags/environment configuration.

`delivery_policy` is `merged` when approved merge into the contracted target completes delivery, or `acceptance_required` when deployment/device/business acceptance remains necessary. Omission defaults to `acceptance_required`. Both policies still require user merge approval. `permissions` retains independent local_commit/push flags and optional pull_request/draft_pr_mr flags; a legacy Draft permission is not promoted to general PR permission.

Project URLs are validated against the supported canonical github.com user/org format without contacting the server. Credential configuration remains separate; no `.env` is needed when the process already has `ISSUEFLOW_GITHUB_TOKEN`.

## Configuration

Precedence, from highest to lowest: **explicit CLI flags > process environment variables > `.env` > built-in defaults**.

By default, only `.env` in the current working directory is loaded; parent directories are not searched. Use `--env-file PATH` to select a different file (replacing the default, not layering on top), or `--no-env-file` to disable file loading. A missing default `.env` is allowed. An explicitly selected file that is missing, unreadable, or malformed causes an error. Loading configuration does not mutate the process environment or create local configuration registry files.

```sh
cp .env.example .env
cargo run -- config
cargo run -- --env-file /path/to/project.env config
cargo run -- --no-env-file --timeout-seconds 60 config
```

`.env` supports dotenvy quoting, comments, and variable expansion. Use single quotes for values containing a literal `$`. Duplicate `ISSUEFLOW_` entries in the file are rejected. Empty process environment values still override file values: an empty token is treated as unconfigured and does not fall back to credentials in the file.

| Environment variable | Default / purpose |
| --- | --- |
| `ISSUEFLOW_GITHUB_TOKEN` | Optional GitHub token, read only from the environment or file |
| `ISSUEFLOW_GITHUB_API_URL` | `https://api.github.com` |
| `ISSUEFLOW_GITLAB_TOKEN` | Optional GitLab token, read only from the environment or file |
| `ISSUEFLOW_GITLAB_URL` | Instance URL, such as `https://gitlab.example.com` |
| `ISSUEFLOW_PLATFORM` | Optional `github` or `gitlab` |
| `ISSUEFLOW_REPOSITORY` | Optional full `owner/repo` or `group/subgroup/project` path |
| `ISSUEFLOW_TIMEOUT_SECONDS` | `30`; allowed range: 1–300 seconds |

Corresponding non-credential flags are `--platform`, `--repository`, `--github-api-url`, `--gitlab-url`, and `--timeout-seconds`. URLs must use HTTPS and cannot contain embedded credentials, query parameters, or fragments. Instance base paths are supported. All configuration is validated, but inspecting configuration does not require a token.

`config` writes JSON that reports only whether tokens are configured, never their contents. Errors go to stderr; configuration errors use exit code `2`. `.env` and `.env.*` are ignored by Git, with an exception for `.env.example`. Never place real credentials in examples or command-line arguments.

## Development validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Automated tests cover configuration, credential redaction, URL handling, pagination, write mappings, duplicate detection, state changes, and dependency rules. Local HTTP servers verify both SDKs' authentication headers, base paths, JSON handling, empty `204` responses, redirects, and rate limiting. These tests do not write to real repositories.

Live GitHub validation passed the main issue maintenance operations but reproduced duplicate creation when immediately retrying the same request ID. Live validation against Jihu GitLab v18.4.6-jh is still pending.
