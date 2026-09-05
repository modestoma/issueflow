# GitHub 真实联调记录 — 2026-09-05

目标仓库：https://github.com/modestoma/issueflow

使用本地 release 版 issueflow 和 `.env` 配置的 Token，通过 CLI 直接调用 GitHub API。测试脚本仅负责顺序调用 CLI 和断言 JSON 输出，没有通过其他客户端执行 API 操作。未提交或推送代码。

## 结果

主要维护功能通过；即时重复创建检查未通过，不能把本次验收表述为全部功能通过。

通过的操作：

- doctor 认证，issue list/show，中文标题及正文创建。
- 初始化 22 个工作流标记；再次初始化没有新增标记。
- 正文更新并保留 operation 标记，过期 updated_at 拒绝写入。
- 中文 Markdown 评论写入、读取。
- 添加／移除 blocked 标记。
- 待明确、就绪、开发中、待验收阶段切换；保留 type 和 priority 标记。
- GitHub 原生阻塞依赖添加、读取、移除；拒绝反向循环依赖。
- 按取消原因关闭、重开并清除 resolution、恢复待复查。
- 两条测试记录最终按 completed 关闭；分别读回依赖列表为空；列表读回均为 closed。
- 同一 operation 已出现两个匹配项后，再次创建返回 conflict，未继续新增。

## 即时重复创建检查失败

首次创建：https://github.com/modestoma/issueflow/issues/1

紧接着使用相同 request-id 和相同正文再次创建，返回 reused=false，并创建了 https://github.com/modestoma/issueflow/issues/2 。两条记录的 created_at 相隔一秒。后续列表查询确认两条正文均含相同 operation 标记。

结合 create 实现判断，第二次创建前的列表查重未看到首条记录，表现与 GitHub 列表短暂不可见一致；本次未采集该次 GET 的原始 HTTP 响应，不能进一步确定服务端缓存或一致性机制。

已确认的边界：列表扫描加正文 UUID 只能提供尽力查重，不能保证即时重试或并发创建幂等。此次未将延时或重复 GET 包装成可靠性修复，也未更改实现。

建议后续：成功创建后由调用方保留返回的 URL，继续通过 URL 操作；写入结果不明时先核对远端，避免立即重新创建。若需要严格幂等，应单独设计持久化操作结果和并发协调机制，不能仅依赖列表查重。

## 最终远端状态

- #1 和 #2 均已关闭，带 workflow::已完成、type::chore、priority::P3。
- #2 已改名为“B 前置依赖（即时重试产生）”，用于完成依赖联调。
- 两条记录间的测试依赖已移除。
- 初始化的 22 个工作流标记保留，供项目后续使用。
- 测试正文与评论保留作为验收记录。

本轮仅验证 GitHub。公司极狐 GitLab v18.4.6-jh 尚未联调；本轮也未用超过 100 条真实 issue 验证分页，分页仍以本地测试为依据。
