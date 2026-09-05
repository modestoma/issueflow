# issueflow

Rust 编写的 GitHub / GitLab issue 维护 CLI。通过 SDK 直接访问 API，不依赖 `gh`、`glab` 或 Python。

已实现 issue 读取、创建、更新、评论、标记、关闭／重开、阶段流转和原生阻塞依赖管理。GitHub 使用 `octocrab =0.54.1`，GitLab 使用 `gitlab =0.1804.0` 的 endpoint/query 扩展接口，并提供受控 HTTP 客户端，支持实例基础路径、超时和禁止重定向。SDK 不覆盖的端点通过同一适配器调用。开发本 CLI 期间不使用 issue-workflow 工作流。

## 构建与使用

```sh
cargo build --release
./target/release/issueflow --help
```

配置凭据后，先做只读连通检查；`doctor` 只验证身份，不声称拥有项目写权限：

```sh
issueflow --platform github doctor
issueflow --platform gitlab doctor
issueflow --platform github --repository owner/repo issue list
issueflow issue show https://github.com/owner/repo/issues/42 --comments
issueflow issue comments https://gitlab.example.com/group/repo/-/issues/42
```

详情操作以 URL 的平台、仓库和原生编号为准，覆盖默认目标；主机必须与对应 API 配置匹配，绝不向未配置主机发送凭据。GitHub Enterprise 的网页主机从自定义 API URL 推导。GitLab 支持多级组和实例基础路径。不分配自定义 issue 编号，日期可直接写入标题。

`issue list` 返回全部可见的开放与关闭 issue，排除 GitHub PR。数组 API 完整分页读取，上限 100000 条，超过上限或中途失败不会返回伪装完整的结果。

### 创建与更新

`issue.json` 示例：

```json
{
  "title": "[20260905] 改善错误提示",
  "body": "## 背景\n…\n\n## 验收标准\n- [ ] …",
  "labels": ["type::improvement", "workflow::待复查", "priority::P2"]
}
```

```sh
issueflow --platform github --repository owner/repo setup-labels
issueflow --platform github --repository owner/repo issue create --file issue.json
issueflow issue update https://github.com/owner/repo/issues/42 --file changes.json --expected-updated-at '2026-09-05T08:00:00Z'
issueflow issue comment https://github.com/owner/repo/issues/42 --file progress.md
```

`changes.json` 只接受 `title`、`body`，仅更新出现的非 null 字段；空字符串 body 可清空正文（保留内部重试标记）。正文文件按 UTF-8 读取，`--file -` 从标准输入读取。创建 JSON 只接受 title/body/labels，未知字段报错。

创建时正文附带 operation UUID 注释；输出包含 operation、原生编号、URL。可用 `--request-id UUID` 显式指定同一逻辑请求。重试会查询全部可见 issue：已有相同 request-id 且内容一致时返回原单，内容不同或多个匹配则报冲突。没有本地登记文件。

**这不是服务端幂等保证**：并发使用相同 UUID、权限不完整、删除标记或短暂不可见仍可能重复。写入超时会报告 `outcome_unknown: true` 和 request-id；先核对远端，不盲目重发。所有请求均不自动重试、不跟随重定向。请求成功响应无法解析时也不声称写入未发生。

`--expected-updated-at` 是写入前的过期检查，不是平台原子的 compare-and-swap；检查与写入之间仍存在竞争窗口。应根据最新正文合并变更。

### 标记与状态

```sh
issueflow issue labels https://github.com/owner/repo/issues/42
issueflow issue labels https://github.com/owner/repo/issues/42 --add blocked --remove old-label
issueflow issue transition https://github.com/owner/repo/issues/42 --to in-progress
issueflow issue transition https://github.com/owner/repo/issues/42 --to awaiting-review
issueflow issue close https://github.com/owner/repo/issues/42 --reason completed
issueflow issue reopen https://github.com/owner/repo/issues/42
```

阶段支持 `triage`、`clarification`、`ready`、`in-progress`、`awaiting-review`，映射为中文 `workflow::…` 标记。阶段切换移除旧阶段、保留其他标记，并读回检查。关闭的 issue 必须显式 reopen 才能切换开放阶段。

关闭原因支持 `completed`、`cancelled`、`duplicate`、`invalid`：成功完成对应已完成，其他对应已终止及 resolution 标记。GitHub 非完成原因统一使用原生 `not_planned`，重复的原单 URL 应写入正文／评论。reopen 会清理旧 resolution 并回到待复查。

CLI 执行显式命令，不判断人是否验收，也不自动合并或发布。`close --reason completed` 应在人工验收和交付条件满足后调用。标记与关闭涉及多个请求，不具备跨请求事务；部分成功会明确报错，按远端现状修复。`setup-labels` 仅创建缺失标记，保留现有颜色和无关标记。通用 labels 操作可直接管理标签；推荐用 transition/close/reopen 保持工作阶段与基础状态一致。

### 依赖

```sh
issueflow issue dependencies https://github.com/owner/repo/issues/42
issueflow issue add-dependency https://github.com/owner/repo/issues/42 https://github.com/owner/repo/issues/40
issueflow issue remove-dependency https://github.com/owner/repo/issues/42 https://github.com/owner/repo/issues/40
```

方向为第一个 issue **被第二个 issue 阻塞**。添加前遍历可达前置关系并检查循环，最多 1000 个节点。GitHub 使用 dependencies API；GitLab 使用 issue links。平台和实例必须一致，可跨项目；不支持的版本或权限返回 API 错误，不把普通 related-to 冒充阻塞。GitLab 原生阻塞关系取决于实例许可证。

依赖列表输出原生关系信息；不自动解锁开发或关闭 issue，因为 Closed 可能意味着取消。检查与添加之间不具备跨用户事务，远端仍是最终依据。父子层级、正文依赖降级、PR/MR 和 worktree 自动化不在本版 CLI 范围内。

## 输出与错误

成功输出 JSON 到 stdout，错误 JSON 到 stderr。issue 结果统一包含 platform、id、number、url、title、body、state、labels、created_at、updated_at；id 是平台全局 ID，number 是仓库原生编号（GitLab iid）。评论与依赖保留平台原始 JSON。

退出码：0 成功，2 输入／配置错误，3 认证／权限错误，4 找不到资源，5 冲突，1 其他 API／网络错误。服务端错误正文不会直接输出，避免意外回显凭据；HTTP 状态保留在 status 中。Clap 的命令用法错误使用标准 CLI 帮助格式。GitHub 的 403 也可能是次级限流，需要结合平台状态判断。

## 配置

优先级由高到低：**显式命令参数 > 进程环境变量 > `.env` > 内置默认值**。

默认只读取当前工作目录的 `.env`，不向父目录搜索。使用 `--env-file PATH` 指定另一份文件（替代默认文件，不叠加），或 `--no-env-file` 禁用文件加载。默认 `.env` 不存在可继续，明确指定的文件缺失、文件不可读或格式错误则退出。不会修改进程环境或创建本地配置维护文件。

```sh
cp .env.example .env
cargo run -- config
cargo run -- --env-file /path/to/project.env config
cargo run -- --no-env-file --timeout-seconds 60 config
```

`.env` 支持 dotenvy 的引号、注释和变量展开；含字面 `$` 的值用单引号。重复的 `ISSUEFLOW_` 文件配置项报错。进程变量的空值仍覆盖文件值：空 Token 表示未配置，不回退使用文件里的凭据。

| 环境变量 | 默认值 / 用途 |
| --- | --- |
| `ISSUEFLOW_GITHUB_TOKEN` | 可选 GitHub Token，只从环境或文件读取 |
| `ISSUEFLOW_GITHUB_API_URL` | `https://api.github.com` |
| `ISSUEFLOW_GITLAB_TOKEN` | 可选 GitLab Token，只从环境或文件读取 |
| `ISSUEFLOW_GITLAB_URL` | 公司实例地址，例如 `https://gitlab.example.com` |
| `ISSUEFLOW_PLATFORM` | 可选 `github` 或 `gitlab` |
| `ISSUEFLOW_REPOSITORY` | 可选完整 `owner/repo` 或 `group/subgroup/project` |
| `ISSUEFLOW_TIMEOUT_SECONDS` | `30`，允许 1–300 秒 |

对应非凭据参数：`--platform`、`--repository`、`--github-api-url`、`--gitlab-url`、`--timeout-seconds`。URL 必须为 HTTPS，不接受内嵌凭据、查询参数或片段；支持实例基础路径。所有配置统一校验，但查看配置不要求提供 Token。

`config` 输出 JSON，仅报告 Token 是否配置，不输出其内容；错误写入 stderr，配置错误退出码为 2。`.env` 和 `.env.*` 已忽略，仅 `.env.example` 可提交。不要把真实凭据写入示例或命令行参数。

## 开发验证

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

测试覆盖配置、凭据隐藏、链接、分页、写入映射、幂等查重、状态和依赖规则，并使用本地 HTTP 服务验证两个 SDK 的认证头、基础路径、JSON、204、重定向和限流行为。测试不写真实项目。真实 GitHub 和极狐 GitLab v18.4.6-jh 尚需单独验收。
