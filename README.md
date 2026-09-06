# issueflow

A Rust CLI for managing GitHub and GitLab issues. It accesses platform APIs directly through SDKs, without requiring `gh`, `glab`, or Python.

Supports reading, creating, updating, and commenting on issues; managing labels; closing and reopening issues; changing workflow stages; managing native blocking dependencies; maintaining GitHub Projects or GitLab Issue Boards through one `kanban` facade; and delivering GitHub pull requests or same-project GitLab merge requests. GitHub uses `octocrab =0.54.1`. GitLab uses the endpoint/query extension interfaces in `gitlab =0.1804.0` with a controlled HTTP client that supports instance base paths, timeouts, and disabled redirects. Endpoints not covered by the SDK use the same adapter. GitHub workflows use Project fields exclusively; GitLab retains label-based workflow stages.

## Output modes

Direct terminal use renders concise human-readable summaries and tables. Add the global `--verbose` option before or after a subcommand to include safe diagnostic detail. Use global `--json` to force deterministic JSON; stdout redirected to a pipe or file also remains JSON by default, preserving existing automation. Errors and warnings are written to stderr, and output formatting does not change command behavior or exit codes.

Human-readable output does not use ANSI color, so it also respects terminals and automation that set `NO_COLOR`. Configuration remains redacted in every mode; verbose rendering additionally masks fields whose names indicate tokens, authorization values, passwords, secrets, or cookies.

## Build and usage

```sh
cargo build --release
./target/release/issueflow --help
```

The examples below assume the built executable is available as `issueflow` on your `PATH`. You can also use `./target/release/issueflow` from the repository root.

After configuring credentials, start with a read-only diagnostic check. `doctor` verifies the effective platform and repository configuration, endpoint reachability, authentication, repository readability, and Kanban readiness. It never mutates remote state and reports write permission as `unknown` because successful reads do not prove mutation access:

```sh
issueflow --platform github --repository owner/repo doctor --config-file .issue-workflow.json
issueflow --platform gitlab --repository group/repo doctor
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
  "body": "## Background\n…\n\n## Acceptance criteria\n- [ ] …"
}
```

```sh
issueflow --repository owner/repo kanban init https://github.com/users/owner/projects/1
issueflow --platform github --repository owner/repo issue create --file issue.json
issueflow issue update https://github.com/owner/repo/issues/42 --file changes.json --expected-updated-at '2026-09-05T08:00:00Z'
issueflow issue comment https://github.com/owner/repo/issues/42 --file progress.md
```

`changes.json` accepts only `title` and `body`. Only supplied, non-null fields are updated. An empty body clears the description while preserving internal retry markers. Input files are read as UTF-8; `--file -` reads standard input. Creation JSON accepts `title`, `body`, `labels`, and the optional GitLab-only `issue_type`; unknown fields are rejected. `issue_type` can be `issue` or `task` and is rejected for GitHub.

To create a GitLab Task that can become a native child item, set the native type explicitly, read back its returned Work Item URL, and then attach it to the parent:

```json
{
  "title": "Implement the child change",
  "body": "Parent and delivery contract: …",
  "labels": ["workflow::Backlog", "type::chore", "priority::P2"],
  "issue_type": "task"
}
```

```sh
issueflow --platform gitlab --repository group/repo issue create --file task.json --request-id UUID
issueflow issue add-sub-issue https://gitlab.example.com/group/repo/-/issues/2 https://gitlab.example.com/group/repo/-/work_items/3
```

Creation appends an operation UUID comment to the body. The output includes the operation, native issue number, and URL. Use `--request-id UUID` to explicitly identify the same logical request. A retry scans all visible issues: an existing request ID with matching content reuses the issue, while different content or multiple matches produce a conflict. No local registry file is maintained.

**This is not a server-side idempotency guarantee.** Concurrent use of the same UUID, incomplete visibility, removed markers, or temporary listing delays can still cause duplicates. A write timeout reports `outcome_unknown: true` and the request ID; inspect the remote state before resending. Requests are never automatically retried and redirects are not followed. An unparseable success response is also treated as an unknown write outcome.

For an uncertain creation outcome, inspect the original request ID without another write:

```sh
issueflow --platform github --repository owner/repo issue recover-create --request-id UUID
```

The command scans visible open and closed issues and returns `found`, `not_visible`, or `ambiguous`, with matching issue URLs. UUID matching is normalized, so letter case does not affect lookup. `safe_to_retry` is always false: zero visible matches never proves the earlier create failed, one match should be continued by URL, and multiple matches need reconciliation. This command performs no POST, no local registry maintenance, and no automatic retries. It improves recovery visibility without claiming distributed idempotency or fixing GitHub listing delays.

`--expected-updated-at` checks for stale data before writing. It is not an atomic platform compare-and-swap operation; another update can occur between the check and the write. Merge changes against the latest body.

### Issue state and GitLab labels

GitHub close/reopen now change **native state only** and preserve all existing labels. Workflow metadata belongs in the required Project; set Resolution and Status explicitly through `kanban` commands. GitHub `issue transition` rejects workflow label operations and directs callers to Kanban commands. The generic `issue labels` API remains available for explicitly requested legacy label maintenance, not routine GitHub workflow execution. Existing repository labels are not bulk-deleted or automatically migrated.

GitLab retains the label workflow:

```sh
issueflow --platform gitlab --repository group/repo kanban init
issueflow issue transition https://gitlab.example.com/group/repo/-/issues/42 --to in-progress
issueflow issue labels https://gitlab.example.com/group/repo/-/issues/42 --add blocked
issueflow issue close https://gitlab.example.com/group/repo/-/issues/42 --reason completed
issueflow issue reopen https://gitlab.example.com/group/repo/-/issues/42
```

GitLab uses the same six-stage model as GitHub: Backlog, Ready, In progress, In review, Done, and Cancelled. Open transitions support backlog, ready, in-progress, and in-review; close reasons are completed/cancelled/duplicate/invalid, with canonical workflow/resolution label maintenance. Clarification is the orthogonal `needs-clarification` label. Reopening returns to Backlog and clears the previous resolution. `--no-workflow-labels` retains its explicit native-only behavior. No command grants merge or acceptance authorization.

Existing installations can inspect one GitLab issue at a time before migrating legacy labels:

```sh
issueflow issue reconcile-metadata ISSUE_URL
issueflow issue reconcile-metadata ISSUE_URL --apply
```

The command maps the seven legacy Chinese workflow labels to the six canonical stages, preserves clarification separately, maps legacy resolution labels to English values, and derives `blocked` from native blocking relationships. Preview is the default. Apply performs one targeted label update with readback; ambiguous or mixed stages stop without writing. Repository-wide migration and legacy label/list deletion are intentionally not automatic.

## Cross-platform Kanban

`kanban` is the user-facing planning entry point. It routes GitHub Project URLs and configured GitLab project Issue Boards without pretending their data models are identical. The visible command set is `list`, `show`, `create`, `init`, `items`, `add`, `status`, `field`, `repositories`, and `link-repository`.

The old top-level `project`, `board`, and `setup-labels` commands remain executable as hidden compatibility aliases for one release. They preserve their existing behavior and write deprecation warnings to stderr only. New automation should use `kanban`.

### GitLab Issue Boards

Self-hosted GitLab workflows can use a project Issue Board whose columns are backed by the same `workflow::*` labels:

```sh
issueflow --platform gitlab --repository group/repo kanban list
issueflow --platform gitlab --repository group/repo kanban show 3
issueflow --platform gitlab --repository group/repo kanban create --name 'Team Board'
issueflow --platform gitlab --repository group/repo kanban init
issueflow --platform gitlab --repository group/repo kanban init 3
```

`kanban create` reuses a unique exact-name board or creates it and reads it back. `kanban init` uses the default name `Issueflow Workflow`; override it with `--name`, or provide an explicit positive board ID to initialize that existing board. Initialization ensures all workflow labels, adds only missing label lists, orders the six canonical workflow columns, and reads the final board and lists back. Repeating a completed initialization is a read-only no-op. Ambiguous boards or duplicate workflow lists stop without deletion. Legacy columns are reported as `legacy_lists` with `legacy_cleanup_required=true`; they are not silently deleted. Multi-step writes are not transactional, so an unknown outcome must be inspected and resumed with the same name or board ID rather than creating another board. These project-level label lists are the cross-tier compatibility target; the command does not depend on Premium-only native status lists or manage group boards. GitLab does not provide parity for Project items, arbitrary fields, repository links, or Project Status through this facade; those subcommands fail before API access. See the [GitLab Issue Boards guide](https://docs.gitlab.com/user/project/issue_board/) and [Boards API](https://docs.gitlab.com/api/boards/).

### Dependencies

```sh
issueflow issue blocked-by https://github.com/owner/repo/issues/42
issueflow issue blocking https://github.com/owner/repo/issues/42
issueflow issue add-dependency https://github.com/owner/repo/issues/42 https://github.com/owner/repo/issues/40
issueflow issue remove-dependency https://github.com/owner/repo/issues/42 https://github.com/owner/repo/issues/40
```

The first issue is **blocked by the second issue**. Before adding a dependency, the CLI traverses reachable blockers and checks for cycles, up to 1,000 nodes. GitHub uses the dependencies API; GitLab uses issue links. Dependencies may cross repositories but must remain on the same platform and instance. Unsupported versions or insufficient permissions produce API errors; ordinary related-to links are never presented as blocking relationships. Native GitLab blocking relationships depend on the instance license.

Both dependency directions retain native relationship data: `blocked-by` lists prerequisites of the current issue and `blocking` lists issues that depend on it. `dependencies` remains a compatibility spelling of `blocked-by`. The CLI does not automatically unblock development or close issues, since a closed issue may have been cancelled. Checking and adding a dependency are not transactional across users; the remote state remains authoritative. Parent/Sub-issue hierarchy is independent of blocking and does not automatically authorize work or delivery.

### GitHub Projects

Projects v2 commands support user-owned and organization-owned projects on **github.com**. GitHub Enterprise Projects routing is not supported in this version; custom GitHub API bases are rejected by these commands. Existing issue commands retain their previous platform support.

Use a canonical Project URL, not a board view URL with `/views/N` or query parameters:

```sh
issueflow --no-env-file kanban show https://github.com/users/modestoma/projects/1
issueflow --no-env-file kanban items https://github.com/users/modestoma/projects/1
issueflow --no-env-file kanban add https://github.com/users/modestoma/projects/1 https://github.com/modestoma/issueflow/issues/4
issueflow --no-env-file kanban status https://github.com/users/modestoma/projects/1 https://github.com/modestoma/issueflow/issues/4
issueflow --no-env-file kanban status https://github.com/users/modestoma/projects/1 https://github.com/modestoma/issueflow/issues/4 --to 'In progress'
```

For organization projects, use `https://github.com/orgs/OWNER/projects/N`. Project and issue URLs are explicit. Ordinary Project operations do not require default platform/repository configuration; `kanban init` additionally requires a repository so it can establish and verify the native link. These commands use `ISSUEFLOW_GITHUB_TOKEN` and the same configuration precedence as other commands. `--no-env-file` ensures authentication comes from the process environment.

`show` returns metadata and field definitions, including Status option names and IDs. `items` lists all visible Project items with their Status, including draft issues and pull requests; hidden content may be null. Fields and items use cursor pagination, with repeated IDs/cursors and incomplete results treated as errors. The limit is 1,000 pages per connection.

`add` accepts an existing GitHub issue, checks membership, and reuses an existing item. `status` without `--to` reads its current Status. With `--to`, it resolves an exact, case-sensitive option name from the single-select field named `Status`. Missing or ambiguous fields/options are rejected. The issue must already belong to the Project; setting Status does not implicitly add it. Archived items cannot have Status changed through this command. Mutations are read back, and already-matching Status values do not trigger another mutation.

For a board using these stages, the recommended workflow mapping is:

| Workflow meaning | Project Status |
| --- | --- |
| Captured, not yet taken up | Backlog |
| Taken up and preparing to process | Ready |
| Active investigation, clarification, implementation or verification | In progress |
| PR created or verified updates pushed for review | In review |
| PR merged into the contracted target; native closure is assessed separately | Done |

Use Cancelled for termination and Resolution to distinguish Cancelled, Duplicate and Invalid; do not interpret every closed issue as Done. Routine status changes never create fields or options.

GitHub workflows require a configured Project before work begins. If none exists, create/reuse one, run `kanban init`, verify the Board and fields, and save the confirmed URL. Do not fall back to labels when configuration or permissions are missing. Native issue Open/Closed and blocking dependencies remain separate from Project metadata.

**Project Status and issue state are separate.** The CLI does not send issue-close mutations when changing Status. However, GitHub Project automations can close issues or overwrite Status when items change. Inspect the Project's Workflows settings before using live status writes; disable automatic closure if it would bypass human acceptance. Moving a card does not launch Codex or authorize merging or deployment.

### Project onboarding

```sh
issueflow --platform github kanban list --owner modestoma
issueflow --platform github kanban create --owner modestoma --name 'My workflow'
issueflow --repository modestoma/issueflow kanban init https://github.com/users/modestoma/projects/2
```

Use `--owner-type organization` for an organization. `list` reads all visible Projects for the explicit owner. `create` reuses a unique open Project with the exact title, rejects ambiguous/closed matches, or creates a new Project and verifies it by its returned ID/number. No mutation is automatically retried. Title lookup is best-effort reuse, not an atomic idempotency guarantee; after an unknown outcome inspect the owner's Project list before any new create attempt.

GitHub `kanban init` requires an explicit `--repository`. It reuses or creates the native Project-to-repository link with readback, then initializes the following single-select fields while preserving existing option IDs, names, colors and descriptions:

| Field | Options |
| --- | --- |
| Status | Backlog, Ready, In progress, In review, Done, Cancelled |
| Work type | bug, feature, improvement, refactor, docs, chore, research |
| Priority | P0, P1, P2, P3 |
| Blocked | No, Yes |
| Resolution | Completed, Cancelled, Duplicate, Invalid; leave unset until resolved |

GitHub reserves the field name `Type`, so the custom field is named **Work type**. Existing defaults/options are retained. A Board view with Status as its vertical column grouping is reused, or an `Issueflow Kanban` view is created and read back. Unrelated views are not overwritten. An existing dedicated view with incompatible grouping/filter produces an error. Project automations remain unchanged.

The API updates complete option lists; the final read-before-write check is not an atomic lock against concurrent edits. Initialization can partially succeed, so inspect the `kanban show` result and initialization readback before retrying. The hidden compatibility alias retains the old Status-only initialization command for one release.

```sh
issueflow kanban field PROJECT_URL ISSUE_URL --name 'Work type' --to feature
issueflow kanban field PROJECT_URL ISSUE_URL --name Priority --to P2
issueflow kanban field PROJECT_URL ISSUE_URL --name Blocked --to Yes
issueflow kanban field PROJECT_URL ISSUE_URL --name Resolution --to Completed
issueflow kanban field PROJECT_URL ISSUE_URL --name Resolution --clear
```

Omitting `--to` and `--clear` reads the field. Names/options match exactly; missing/ambiguous fields fail before writing. Add the issue first. Clear Resolution on reopening, reconsider Blocked and set the actual workflow stage. The CLI does not read legacy labels to infer these values or bulk-remove them. Migrate existing issue metadata deliberately before switching its source of truth.


After successful readback, record the selected URL as `github_project_url` in the repository's secret-free `.issue-workflow.json` and run `config validate`. Do not save guessed URLs or create a new Project merely because an existing one is inaccessible. The CLI does not automatically overwrite local configuration or provision every repository; these commands are explicit onboarding actions.

### Projects authentication and failure handling

Issue write permissions alone are insufficient for Projects. For personal Projects, GitHub's GraphQL guide documents classic PAT scopes `read:project` for queries or `project` for queries and mutations. Retain the repository access needed for the issues being added (including `repo` for private repositories when using a classic PAT). An organization may instead use an appropriately authorized GitHub App. See [GitHub's Projects API guide](https://docs.github.com/en/issues/planning-and-tracking-with-projects/automating-your-project/using-the-api-to-manage-projects) for token support and permissions. Tokens belong in the environment or private configuration files, never command arguments.

HTTP-200 responses containing GraphQL errors are failures, even when partial data is present. Known insufficient-scope/forbidden errors return the permission exit code; raw error messages are not printed. Queries use POST but are classified as read-only for unknown-outcome reporting. Failed mutations or failed readback can report `outcome_unknown: true`; inspect Project items/Status before another write. There are no automatic mutation retries, background synchronization, or cross-user transactions.

Live validation against the configured personal Project passed metadata/options and item reads, existing membership reuse, invalid-option rejection, Status changes through Ready / In progress / In review, and matching-Status no-op behavior. The issue remained open. Creating a new Project membership has local test coverage but has not been exercised against a live account. Organization-owned Project URL parsing is tested; its GraphQL lookup has not been live-validated.

## Pull requests, merge requests, and branch delivery

One issue can own a long-lived development branch with multiple commits. Git manages worktrees, branches, commits, and pushes; issueflow manages GitHub PRs and same-project GitLab MRs. Branch names can use `feat/`, `fix/`, `refactor/`, `docs/`, or project conventions. A child issue's PR/MR targets its parent integration branch, not necessarily `main`.

```sh
issueflow --platform github --repository owner/repo pr list --head feat/issue-5-example --base feat/issue-1-parent
issueflow pr create https://github.com/owner/repo/issues/5 --file pr.json
issueflow pr show https://github.com/owner/repo/pull/8
issueflow pr show https://gitlab.example.com/group/repo/-/merge_requests/8
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

`pr list` defaults to open PRs with complete pagination; `--state closed`, `--state merged`, and `--state all` support historical lookup. Creation reuses an existing open PR for the same head/base only when its body contains the same issue reference; it does not overwrite the existing PR. Reusing a PR does not update its title/body/draft state. Push further commits to the same branch to update its diff. If creation has an unknown outcome, inspect open PRs before retrying; a momentarily empty list is not proof of failure.

Inspect the evidence for the current PR head before requesting merge approval:

```sh
issueflow pr checks https://github.com/owner/repo/pull/8
```

The command reads paginated check runs, latest commit statuses per context, and review history, then rereads the PR to reject head/base changes during inspection. `observed_checks` is `absent`, `pending`, `failed`, `non_failing` (neutral/skipped), or `passed`. No checks is not a pass. Reviews are annotated with whether their commit matches the current head; they are historical records, not an effective approval count. Required branch/ruleset policies and human acceptance are not evaluated, and `merge_authorized` is always false. API permission failures remain errors, never empty evidence. The SHA is evidence provenance, not a persistent authorization token.

Update a PR selectively or mark a Draft ready for review:

```sh
issueflow pr update https://github.com/owner/repo/pull/8 --file pr-changes.json --expected-head-sha FULL_40_CHARACTER_SHA
issueflow pr ready https://github.com/owner/repo/pull/8 --expected-head-sha FULL_40_CHARACTER_SHA
```

Update JSON accepts only optional `title` and `body`. Omitted/null fields are preserved, blank titles and empty updates are rejected, and existing standalone `Refs https://...` lines are retained when replacing the body. No base/head retargeting is performed. Both operations reject closed PRs or stale head expectations and read back their results. This is a read-before-write check, not an atomic lock; concurrent metadata edits may still race. `ready` is a no-op if already ready and currently supports github.com only. Neither command merges or closes issues; GraphQL mutation errors may have unknown outcomes.

After explicit review and merge authorization:

```sh
issueflow pr merge https://github.com/owner/repo/pull/8 --expected-base feat/issue-1-parent --expected-head-sha FULL_40_CHARACTER_SHA --method merge
issueflow issue close https://github.com/owner/repo/issues/5 --reason completed --no-workflow-labels
```

Merge requires an open, non-draft PR with the expected target and full head SHA. The SHA is also passed to GitHub's merge endpoint; draft, stale expectations, unconfirmed merge responses, and failed readback are errors. Methods are `merge` (default), `squash`, and `rebase`. GitHub permissions and branch protections still apply. Target-branch validation is a read-before-write check, not an atomic lock against concurrent retargeting. This implementation handles ordinary PRs, not GitHub native stacked-PR batch merges or merge queues.

Merging does not invoke issue closure, delete branches, or set Project Status. Verify delivery to the intended base before explicitly closing the issue. A child is complete after acceptance into the parent integration branch; the parent still requires overall acceptance and its own PR into the original target. GitHub closure and reopening are native-only by default; maintain Project fields separately. Multi-step delivery has no cross-API transaction and must be reconciled from remote state after partial failure.

For GitLab, `pr list/show/create/update/ready/checks/merge` use the configured instance URL, nested project path, and MR iid. Creation is limited to source and target branches in the same project. `ready` removes a `Draft:` or `WIP:` title prefix. Checks return MR pipelines and the native approvals response as observed evidence; they never grant merge authorization. GitLab merge supports merge and squash, but not the GitHub-style rebase method. Protected branches, approval rules, pipeline requirements, and server policy remain authoritative.

Current commands do not evaluate every repository merge policy. Review required checks using available repository evidence or the PR/MR page before granting merge approval. [GitHub PR API reference](https://docs.github.com/en/rest/pulls/pulls), [GitLab Merge Requests API reference](https://docs.gitlab.com/api/merge_requests/).

### Native Parent and Sub-issue relationships

```sh
issueflow issue relationships ISSUE_OR_WORK_ITEM_URL
issueflow issue parent ISSUE_OR_WORK_ITEM_URL
issueflow issue sub-issues ISSUE_OR_WORK_ITEM_URL
issueflow issue sub-issues GITHUB_ISSUE_URL --recursive --depth 8
issueflow issue add-sub-issue PARENT_URL CHILD_URL
issueflow issue add-sub-issue NEW_PARENT_URL CHILD_URL --move-from EXPECTED_OLD_PARENT_URL
issueflow issue remove-sub-issue PARENT_URL CHILD_URL
issueflow issue remove-parent CHILD_URL
issueflow issue move-sub-issue PARENT_URL CHILD_URL --before SIBLING_URL
issueflow issue move-sub-issue PARENT_URL CHILD_URL --after SIBLING_URL
issueflow --platform github --repository owner/repo issue create --file issue.json --parent PARENT_URL
```

`relationships` returns separate `parent`, ordered `sub_issues`, informational `sub_issues_summary`, `blocked_by`, and `blocking` values; it never infers one relationship kind from another. Direct Sub-issue reads are the inexpensive default. GitHub recursive reads are bounded to eight levels and 1,000 returned items, preserve native ids, URLs, state, depth, parent URL, and sibling position, and reject repeated nodes or more than 100 direct children instead of presenting partial data.

GitHub uses native Sub-issues REST endpoints. Adds traverse up to 1,000 descendants before writing to reject cycles and read back mutations. An existing different parent is rejected unless `--move-from` exactly matches the current parent; explicit reassignment verifies the child under the new parent and absent from the old parent. Create-with-parent sends GitHub's native parent field as part of the create request, and both ordinary idempotent reuse and `recover-create --parent PARENT_URL` verify the actual parent. Sibling ordering requires exactly one `--before` or `--after`, validates both items against the requested parent before writing, and reads back their final adjacency.

GitLab uses the versionless Work Item GraphQL hierarchy widget through the configured instance `/api/graphql` endpoint. The project-level adapter supports the reliable `Issue -> Task` combination only: create the Task with GitLab `issue_type: "task"`, then use its returned `/work_items/` URL. Both items must be distinct and in the same project, their native Work Item types are read before mutation, an existing different parent causes a conflict, and `workItemUpdate.hierarchyWidget.parentId` is verified by rereading the child. Recursive traversal, explicit reassignment, create-with-parent, arbitrary `Issue -> Issue`, group Epic mapping, and hierarchy reordering are not claimed for GitLab and fail before mutation. Native hierarchy and progress never imply a blocking dependency, Project transition, closure, or delivery acceptance. The hidden `hierarchy parent/children/add-child/remove-child` aliases remain for one compatibility release and warn on stderr. [GitHub Sub-issues API](https://docs.github.com/en/rest/issues/sub-issues), [GitHub issue dependencies API](https://docs.github.com/en/rest/issues/issue-dependencies), [GitLab child items](https://docs.gitlab.com/user/work_items/child_items/), and [GitLab Issues API](https://docs.gitlab.com/api/issues/).

### Validate parent/child branch contracts

```sh
issueflow delivery validate-contract --file child.json --parent-file parent.json
```

A contract is a small JSON artifact that can be kept in the issue body and materialized temporarily for validation; no local issue registry is required:

```json
{
  "schema_version": 1,
  "issue_url": "https://github.com/owner/repo/issues/12",
  "parent_issue_url": "https://github.com/owner/repo/issues/10",
  "source_branch": "feat/issue-10-parent",
  "branch": "feat/issue-12-child",
  "pr_target": "feat/issue-10-parent",
  "pr_url": null
}
```

The parent file uses the same schema with its own issue URL/branch and original target; a root uses `parent_issue_url: null`. Validation requires source and PR target to agree, distinct development/target branches, same-repository issue/PR URLs, and a child's target to equal the supplied parent's branch. A child without its parent file is not considered validated. Unknown fields are rejected. This runs offline and never loads credentials or mutates branches.

The result explicitly sets `remote_verified: false`: local validation does not prove branches exist, inspect actual PR targets, grant merge permission, or validate a full ancestor graph. Read the actual PR before merging and compare its head/base with the contract. The parent still needs overall acceptance after its children deliver.

## Recover interrupted delivery

```sh
issueflow --no-env-file delivery inspect --file child.json --parent-file parent.json --config-file .issue-workflow.json
issueflow --no-env-file delivery reconcile --file child.json --parent-file parent.json --config-file .issue-workflow.json
issueflow --no-env-file delivery reconcile --file child.json --parent-file parent.json --config-file .issue-workflow.json --apply --expected-head-sha FULL_40_CHARACTER_SHA
```

Both `inspect` and `reconcile` are **read-only by default**. They validate the branch contract and workflow configuration, resolve an explicit PR/MR URL or search all states for the contracted head/base, verify the issue reference, and inspect native workflow state. GitHub reads required Project fields; GitLab reads its single workflow stage plus blocking/resolution labels. Multiple matching historical PRs/MRs require an explicit `pr_url`; none is reported as `no_pr`, never treated as permission to create one.

A merged PR is considered delivered only when its merge commit is reachable from the contracted target according to GitHub's compare API. A missing/deleted target or unreadable evidence prevents automatic completion. Returned phases include `no_pr`, `in_progress`, `in_review`, `delivery_pending`, `acceptance_pending`, `manual_review`, `reconciliation_needed`, and `complete`.

Verified target delivery permits Project membership and Done repair even while human acceptance is pending. In that case, `reconcile --apply --expected-head-sha SHA` applies only those Project actions, returns `acceptance_pending`, and preserves native issue state and Resolution. Repeated runs are a no-op until acceptance is confirmed; `human_acceptance_required` blocks closure rather than Done synchronization.

Under `delivery_policy: "merged"`, verified target delivery allows closure planning. Under `acceptance_required`, the caller must explicitly pass `--accepted` only after human acceptance has been confirmed; a green check or merged PR is insufficient. Reuse an existing confirmed acceptance decision when resuming, rather than asking the user repeatedly. Configuration and these flags do not grant merge permission.

Only explicit `--apply` performs missing actions, guarded by the expected PR head: add missing Project membership, set Project Status to Done, and/or close the native issue while preserving labels. The workflow configuration must supply a Project URL and initialized metadata fields; missing configuration yields setup_required, never label fallback. Recovery reads Blocked and Resolution from Project fields, sets Resolution=Completed when completing delivery, and never changes labels. Missing/ambiguous Done options, archived items, terminated issues, unknown closure reasons, or unmerged PRs block writes. Terminated issues are never reopened or relabeled as completed.

Evidence is refreshed before each action and after completion. Repeating a completed recovery is a no-op. The command never merges a PR, creates an issue/PR, modifies local Git state, or posts comments. Partial failures report confirmed completed steps and unknown outcomes; inspect again instead of replaying a merge or a stale plan. These checks and cross-API writes are not atomic: concurrent retargeting, force pushes, acceptance changes or Project automations still require reconciliation. PR/issue references and caller-supplied acceptance are not a distributed lock or cryptographic proof of approval.

## Inspect worktree cleanup eligibility

```sh
issueflow --no-env-file delivery cleanup-check --file child.json --parent-file parent.json --config-file .issue-workflow.json --worktree /absolute/path/to/worktree
```

This command never deletes files, worktrees, or branches. It combines recovery evidence with local Git inspection and open PRs targeting the candidate branch. The exact registered worktree root, expected branch and repository remote must match. Main worktrees, locked worktrees, tracked modifications, untracked files, ignored files, incomplete remote delivery, or a local HEAD different from the reviewed PR head prevent eligibility. Ignored files include `.env` and generated `target/` output; the tool does not silently assume they are disposable. Git inspection disables optional index locks and filesystem monitor hooks.

Cleanup eligibility uses verified PR merge and merge-commit reachability from the contracted target independently of the recovery plan. Pending human acceptance, an open issue, or unfinished Project field synchronization do not by themselves block cleanup. `remote_plan` still reports those remaining workflow steps; `--accepted` affects that plan, not cleanup eligibility. Keep a worktree if it is still needed for acceptance. Unreadable remote evidence continues to fail the check.

Open PRs cannot reveal unpublished child work. After separately reviewing child issues, branches and worktrees, `--confirm-no-dependent-work` records that dependency check for this invocation; it does not override any detected open dependent PR. Omission keeps the result ineligible. A completed squash/rebase delivery can still be recognized by remote merge ancestry and matching reviewed head without requiring the original head commit to be an ancestor of the target.

`eligible: true` means the recorded checks passed at inspection time, not that deletion was authorized. Recheck immediately before a separately authorized cleanup, and never force-remove a worktree merely because it belongs to a closed issue. Unknown/failed Git or API evidence fails the check. Remote credentials are not printed, and no cleanup is executed by this CLI version.

## Output and errors

Interactive terminals receive concise human-readable summaries and tables; `--verbose` adds safe detail. Redirected stdout and explicit `--json` output remain deterministic JSON. Errors and deprecation warnings use stderr, with errors remaining JSON when stderr is not a terminal or `--json` is set. Normalized issue results contain `platform`, `id`, `number`, `url`, `title`, `body`, `state`, `issue_type`, `labels`, `created_at`, and `updated_at`. `id` is the platform-wide ID; `number` is the repository's native issue number (GitLab `iid`); `issue_type` is `issue` for GitHub and the lower-case native Issue/Task type for GitLab when the API provides it. Comments and dependencies retain platform-native JSON.

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
issueflow config validate --file .issue-workflow.json
```

These commands run offline before `.env` or environment configuration is loaded. `capabilities` reports an installed GitHub/GitLab support matrix, explicit limitations, and a versioned command/option schema derived from the installed parser. It does not report the configured platform and does not imply remote permissions. Use `config show` for effective local configuration and `doctor` for live read-only diagnostics.

Workflow validation checks a secret-free schema with `schema_version: 1`, `platform` set to `github` or `gitlab`, the exact host, full repository path, `remote`, `base_branch`, `timezone: "Asia/Shanghai"`, and explicit `permissions`. GitHub requires its normal Project setup before recovery can proceed; GitLab rejects `github_project_url` and uses labels. Unknown fields (including token fields) are rejected; configured commands are never executed. Runtime recovery also requires the workflow host to match the configured API host, and validation never grants authorization.

`delivery_policy` is `merged` when approved merge into the contracted target completes delivery, or `acceptance_required` when deployment/device/business acceptance remains necessary. Omission defaults to `acceptance_required`. Both policies still require user merge approval. `permissions` retains independent local_commit/push flags and optional pull_request/draft_pr_mr flags; a legacy Draft permission is not promoted to general PR permission.

### Delivery and closure policy

| Policy | Meaning of an approved merge | When the issue may close |
| --- | --- | --- |
| `merged` | Delivery into this issue's contracted PR target completes the code task | After merged state and target delivery are verified, unless additional acceptance was explicitly required |
| `acceptance_required` | Code has landed but delivery may remain incomplete | Only after the additional deployment, device, or business acceptance has been confirmed |

A policy never grants merge permission. User approval applies to the specific reviewed PR/head; approving a child PR does not approve its parent. A child delivers to its parent integration branch, while the parent requires overall acceptance and delivery to the original target. Without an explicit policy, keep the conservative `acceptance_required` default; do not infer acceptance from a closed PR or green CI.

For each authorized merge, finish one issue before moving to the next: verify merged state and target, evaluate its delivery condition, update Project/issue state if eligible, then read both back. If delivery remains pending, keep the issue open and state the concrete remaining check. If an API step fails, inspect actual state and retry only the missing authorized step; do not repeat the merge or claim the whole batch is complete.

Project stage timing is part of the workflow: record Ready once prerequisites are satisfied, set and verify In progress **before implementation**, and enter In review only after a PR and verification are ready. Historical missing transitions are acknowledged, not backfilled by rewinding the current stage.

Project URLs are validated against the supported canonical github.com user/org format without contacting the server. Credential configuration remains separate; no `.env` is needed when the process already has `ISSUEFLOW_GITHUB_TOKEN`.

## Configuration

Precedence, from highest to lowest: **explicit CLI flags > process environment variables > `.env` > built-in defaults**.

By default, only `.env` in the current working directory is loaded; parent directories are not searched. Use `--env-file PATH` to select a different file (replacing the default, not layering on top), or `--no-env-file` to disable file loading. A missing default `.env` is allowed. An explicitly selected file that is missing, unreadable, or malformed causes an error. Loading configuration does not mutate the process environment or create local configuration registry files.

```sh
cp .env.example .env
cargo run -- config show
cargo run -- --env-file /path/to/project.env config show
cargo run -- --no-env-file --timeout-seconds 60 config show
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

`config show` writes structured output that reports only whether tokens are configured, never their contents. `config validate` is offline and validates the secret-free project workflow file. Errors go to stderr; configuration errors use exit code `2`. `.env` and `.env.*` are ignored by Git, with an exception for `.env.example`. Never place real credentials in examples or command-line arguments.

For one compatibility release, bare `config` behaves like `config show`, and the old `workflow` paths continue to route to `delivery` or `config validate`. These aliases are hidden from normal help and emit deprecation warnings only on stderr, so JSON stdout remains parseable.

## Development validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Automated tests cover configuration, credential redaction, URL handling, pagination, write mappings, duplicate detection, state changes, and dependency rules. Local HTTP servers verify both SDKs' authentication headers, base paths, JSON handling, empty `204` responses, redirects, and rate limiting. These tests do not write to real repositories.

Live GitHub validation passed the main issue maintenance operations but reproduced duplicate creation when immediately retrying the same request ID. Live validation of issue and MR operations against Jihu GitLab v18.4.6-jh is still pending.

### Native Project repository links

After creating or selecting the workflow Project, establish its native repository link before saving onboarding configuration:

```sh
issueflow --no-env-file kanban link-repository PROJECT_URL OWNER/REPOSITORY
issueflow --no-env-file kanban repositories PROJECT_URL
```

The link makes the Project accessible from the repository's Projects entry. It is separate from adding an issue to a Project and from recording `github_project_url` locally. Existing links are reused without mutation; other repository links are preserved. Both commands paginate links. Linking reads back the association and reports unknown outcomes without retrying mutations. A failed link does not undo successful Project creation: inspect and resume using the existing Project URL. Project creation remains an explicit owner-scoped operation. The API is documented in [GitHub's Projects reference](https://docs.github.com/en/graphql/reference/projects#linkprojectv2torepository).
