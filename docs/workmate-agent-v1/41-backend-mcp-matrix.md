# M7 出口条件：各后端在空白名单下的 MCP 隔离与如实上报

- 日期：2026-10-22（UTC+8）
- 代码：Core `suoak/AionCore` 分支 `feat/m7-e1-gemini-acp-mcp`（基于 origin/main `a5b6af8a`，含 #150 / #152 / #153 / #154）；UI 抽屉见 AionUi #180
- gemini 实测版本：`@google/gemini-cli@0.65.0`（npm 最新稳定版；Core 与 UI 均未锁定 gemini 版本，见 §5）
- 图例：✅ 满足 · ⚠️ 如实标注为外部/未知（按出口条件可接受） · 🔶 未验证

## 1. 出口条件（用户已确定）

1. **aionrs、Claude、Gemini**：白名单为空的 managed 会话（`extra.mcp_strict=true`）**真正不挂任何 MCP**。
2. **Codex、Antigravity、其他 ACP 后端**：在 `tool_surface` 里**如实标出外部 MCP**；能探测实际加载的标为 `external`，探测不到的明确标出隔离状态（`mcp_isolation=ambient` + `mcp_discovery=unavailable`，即「可能加载本机 MCP / 实际加载情况未知」），不允许静默挂载、不允许空列表冒充「没有」。

## 2. 最终状态（managed 会话、白名单为空）

| 后端 | 运行时路由 | WorkMate 注入 | 本机/工作区 MCP | `tool_surface` | 出口条件 |
|---|---|---|---|---|---|
| **aionrs** | 进程内 | 0 | 不加载（全局与工作区 `.aionrs.toml` 的 MCP 被清空） | `aionrs_registry`，`strict` | ✅ |
| **Claude Code** | DirectCli | 0 | 不加载（`--mcp-config '{"mcpServers":{}}' --strict-mcp-config`，#152） | `claude_init`，`strict`，工具来自 `system/init` | ✅ |
| **Gemini** | 通用 ACP | 0 | **不加载**（本 PR：`--allowed-mcp-server-names __csbu_workmate_no_mcp__ --extensions none`，已实测） | `launch_config`，`strict`，`mcp_discovery=unavailable`，`mcp_servers`=注入的 server | ✅ |
| **Codex** | DirectCli | 0 | 会加载 `~/.codex/config.toml` 与受信任项目配置（无 strict 开关） | `codex_mcp_status`，`ambient`，外部 server `external=true` 并列入 `external_mcp_servers`（#154） | ⚠️ 如实标注 |
| **Antigravity** | Antigravity | 0（无 MCP 通道） | 总是读 `~/.gemini/config/mcp_config.json` | `launch_config`，`ambient`，`mcp_discovery=unavailable`（本 PR，此前为 `null`） | ⚠️ 如实标注 |
| **其他 ACP**（qwen、codebuddy、auggie、copilot、自定义） | 通用 ACP | 0 | 取决于各 CLI，未验证 🔶 | `launch_config`，`ambient`，`mcp_discovery=unavailable`（本 PR，此前为 `null`） | ⚠️ 如实标注 |

Legacy / unmanaged 会话（没有 `mcp_strict` 标记）：所有后端启动参数不变；ACP 与 antigravity 现在多上报一个 `ambient` + `unavailable` 的 surface，旧 Core 返回 `null` 时 UI 显示「未上报」的行为不受影响。

## 3. Gemini 实现

- 位置：`crates/aionui-ai-agent/src/factory/acp_assembler.rs::apply_gemini_mcp_isolation_args`，在 `assemble_acp_params` 里、`resolve_mcp_servers` 之后调用，因此 allowlist 用的是**最终**下发给 `session/new` 的 server 名（含团队 MCP `aionui-team`）。
- 条件：`metadata.backend == "gemini"` 且 `config.mcp_strict`。qwen 等 gemini-cli 分支**不加**这组参数（未实测，避免未知参数导致启动失败）。
- 参数：`--allowed-mcp-server-names <name1,name2,…>`；名单为空时传哨兵 `__csbu_workmate_no_mcp__`；再加 `--extensions none`（扩展可携带自己的 MCP，也一并关掉）。未建模的 MCP 传输类型不进 allowlist（fail closed）。
- 为什么 allowlist 必须包含注入的名字：gemini-cli 的 `allowedMcpServers` 同样过滤经 ACP `session/new` 注入的 server。
- 上报：`AcpAgentManager::tool_surface()` → `tool_surface_from_params()`；`AgentInstance::Acp` 不再返回 `None`。

## 4. Gemini 实测（`/workspace/gemini-probe`）

方法：探针 stdio MCP（`fake-mcp.js`，启动即写日志）分别配置在用户 `~/.gemini/settings.json`（user-probe）、工作区 `.gemini/settings.json`（workspace-probe）、扩展 `~/.gemini/extensions/probe-ext`（ext-probe），并通过 ACP `session/new` 注入 `wm-injected`；用 `HOME` 隔离的临时目录、假 `GEMINI_API_KEY` 驱动 `gemini --acp` 完成 `initialize` + `session/new`（不需要登录即可走到 MCP 加载）。脚本：`run-matrix.sh`。

| 场景 | 启动参数 | 实际启动的探针 MCP |
|---|---|---|
| 基线（= 现状 / unmanaged） | `--acp` | ext-probe、user-probe、wm-injected、workspace-probe（session/new 之前进程启动时就已拉起） |
| **managed，空白名单（Core 新行为）** | `--acp --allowed-mcp-server-names __csbu_workmate_no_mcp__ --extensions none` | **无** |
| **managed，注入 1 个（Core 新行为）** | `--acp --allowed-mcp-server-names wm-injected --extensions none` | **仅 wm-injected** |
| managed，注入 2 个（逗号分隔） | `--allowed-mcp-server-names wm-injected,user-probe --extensions none` | 仅 wm-injected、user-probe |
| 仅 allowlist | `--allowed-mcp-server-names wm-injected` | 仅 wm-injected（扩展 MCP 也被挡） |
| 仅 `--extensions none` | `--extensions none` | user-probe、wm-injected、workspace-probe（不够） |

附带发现：gemini-cli 默认开启 folder trust 时，未受信任的工作区**连注入的 MCP 也不启动**（日志提示跳过项目 hooks/agents）。实测矩阵是在 `security.folderTrust.enabled=false` 下跑的（最坏情况：工作区配置会生效）。这意味着在未受信任目录里，managed gemini 会话可能用不上 WorkMate 注入的 MCP——是可用性问题而不是泄漏，后续如需可在 E3 里处理。

**结论**：managed gemini 会话在空白名单下实际 MCP 数为 0；非空白名单时只加载 WorkMate 注入的 server；unmanaged 行为不变。

## 5. Wire 形状（向后兼容）

`ConversationToolSurface` 新增：
- `source` 新取值 `"launch_config"`：后端不上报工具，surface 只反映启动配置。
- `mcp_discovery: "reported" | "unavailable"`：默认 `reported`，**为默认值时不序列化**，所以 claude/codex/aionrs 的 JSON 与之前完全一致；旧 JSON（没有该字段）反序列化为 `reported`。
- `mcp_isolation` 仍只有 `strict` / `ambient` 两个值（UI #180 已按此渲染：strict 绿色「仅托管 MCP」，其余橙色「可能加载本机 MCP」）。

UI 兼容性：#180 对未知 `source` / 多出的字段不会报错；launch-config surface 的工具列表为空，抽屉会显示「共 0 项」，因此另开一个小 AionUi PR：`mcp_discovery=unavailable` 时显示「该后端不上报实际加载的工具与 MCP，以下仅为 CSBU WorkMate 注入的配置」，并补齐 TS 类型。

## 6. 测试

- `acp_assembler::tests::managed_gemini_with_empty_allowlist_launches_with_no_mcp_at_all`：断言启动参数 `--acp --allowed-mcp-server-names __csbu_workmate_no_mcp__ --extensions none`。
- `…::managed_gemini_allowlists_exactly_the_injected_servers_including_team`：allowlist = 最终注入名单（含 `aionui-team`）。
- `…::unmanaged_gemini_launch_args_are_unchanged`：legacy/unmanaged 参数仍为 `--acp`。
- `…::non_gemini_acp_backends_are_not_given_gemini_flags_even_when_strict`：qwen/codebuddy/claude-acp 参数不变。
- `…::acp_tool_surface_is_strict_only_for_managed_gemini`：ACP surface 的隔离与发现状态。
- `session_agent::…::tool_surface_reports_isolation_and_discovery_per_backend`：claude strict/reported、codex ambient/reported/external、antigravity ambient/unavailable，以及 wire 形状。
- `antigravity::conn::tests::tool_inventory_is_an_honest_ambient_launch_config`。
- `aionui-api-types`：旧 JSON（无 `mcp_discovery`）兼容、`launch_config_surface_wire_shape`。
- 本地：`cargo test -p aionui-api-types -p aionui-session -p aionui-ai-agent -p aionui-conversation --lib` 全部通过；`cargo clippy --workspace --all-targets` 无新增告警。
