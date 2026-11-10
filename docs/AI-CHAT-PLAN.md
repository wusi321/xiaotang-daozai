# 岛仔 AI 对话页 · 技术可行性与实施规划

> 规划稿 v2（代码 v0.6.63 起实现，v0.6.70 发布）· 需求已确认，已进入实施
> 范围：在灵动岛内新增第 9 页「AI 对话」，接入 OpenAI 兼容接口进行多轮对话。
> 结论：**可行，无阻塞性风险**。启动、聚焦、输入法、流式四条链路在现有代码里都有可复制的实现。

---

## 0. 已确认的决策

| # | 决策 | 结论 |
| --- | --- | --- |
| 1 | 协议 | 只做 **OpenAI 兼容**（`/chat/completions`），覆盖 DeepSeek / Kimi / OpenAI / 硅基流动 / Ollama |
| 2 | 默认服务商 | **DeepSeek**，`https://api.deepseek.com/v1`，`deepseek-chat` |
| 3 | 对话持久化 | **不落盘**，只在内存；重启即清空 |
| 4 | 页面位置 | 追加在「日程」之后，索引 **8**，默认开启 |
| 5 | 承载形式 | 只做岛内页面，不做独立大窗口 |
| 6 | 会话 | 内存内多会话，可切换、可新建、可清空 |
| 7 | 输入尺寸 | 参照日程表单的输入框：高 19–20、字号 11–11.5 |
| 8 | 发送 | 回车发送 + 右侧发送按钮；生成中按钮变「停止」可打断 |
| 9 | 思考过程 | 显示 `reasoning_content` / `reasoning` 增量，默认折叠为一行摘要 |
| 10 | 自动折叠 | 鼠标离开岛 10 秒后收起；收起后继续生成，结果到达时提示 |
| 11 | 表情 | 吉祥物随模型状态变化：思考 / 满意 / 出错 |
| 12 | 密钥 | 设置页填写 Base URL、模型、API Key；Key 存 Windows 凭据管理器 |

---

## 1. 需求拆解

### 1.1 用户目标

1. 在灵动岛里加一个 AI 对话页。
2. 鼠标移到岛上展开，点击后即可输入，光标闪烁，中英文都能打。
3. 与模型多轮对话，能看到「思考过程」。
4. 回车或点按钮发送；生成中可以打断。
5. 有历史会话可回看/切换，有预设问题可选。
6. 鼠标离开 10 秒自动收起，后台继续生成，出结果再提示。
7. 岛仔表情跟着模型状态动。
8. Key 与地址在设置里配。

### 1.2 v1 范围（Must）

- 第 9 页「AI 对话」：开关、排序、翻页、设置入口与其它页一致。
- 点击消息区或输入框 → 窗口取得键盘焦点 → 闪烁光标 → 可输入。
- 回车发送 / 点发送按钮；空内容不发送；流式中按钮变停止，可打断。
- 流式显示回答；有 `reasoning_content` 时先显示思考区（默认折叠一行，可展开）。
- 多轮上下文；消息区可滚动；会话内存内保存，可新建/切换/清空。
- 空会话显示 3 条预设问题，点一下直接发送。
- 鼠标离开 10 秒自动收起；收起后继续生成；生成完成且岛收起时弹一条「AI 回复已就绪」提示，点击回到对话页。
- 吉祥物：生成中思考状、完成时开心、失败时担心。
- 设置页第 13 个分页：服务商预设、Base URL、模型、API Key、系统提示词、温度、最大长度、测试连接、删除密钥。

### 1.3 明确不做（v1）

- 会话落盘、跨重启历史。
- 独立大窗口、Markdown 富文本、代码高亮。
- 图片/文件输入、tools/函数调用、联网检索、语音。
- Anthropic Messages、Gemini generateContent 等非兼容协议。
- 多模型并行、重试队列。

### 1.4 为后续预留

- `chat::Preset` 与 `chat::Config` 分离，新增服务商只加预设。
- `Cmd::ChatSend(ChatRequest)`：界面不感知具体协议与鉴权。
- `Message.role` 四态（system/user/assistant），未来加 tool 只扩枚举。

---

## 2. 现有能力盘点

| 能力 | 现有实现 | 复用方式 |
| --- | --- | --- |
| 岛内文本输入 | `App::input_field`（[app.rs](../src/app.rs) :2770） | `ui.put(rect, TextEdit)` + `Cmd::Focus` |
| 窗口可聚焦 | `native::apply_style(editing)`（[native.rs](../src/native.rs) :273）+ `ViewportCommand::Focus`（[main.rs](../src/main.rs) :598） | `is_editing()` 纳入对话输入 |
| 光标闪烁 | egui `TextEdit` 内置（[builder.rs](../vendor/egui/src/widgets/text_edit/builder.rs) :758），重绘请求取最小合并 | 无需自绘 |
| 输入法 | `filter_todo_input`（[app.rs](../src/app.rs) :1328）+ `foreground_changed` 早退（:1343） | 泛化为 `filter_island_input` |
| 面板内滚动 | 邮件列表/正文嵌套 `Ui` + `ScrollArea`（[pages.rs](../src/pages.rs) :942、:1027） | 消息区照搬 |
| HTTP/代理 | `quota::system_proxy()`（[quota.rs](../src/quota.rs) :329） | 新建 `chat::agent()` |
| 凭据 | `deepseek_quota.rs` 的 `keyring` 封装（[deepseek_quota.rs](../src/deepseek_quota.rs) :41） | 同款，service=`Daozai.AIChat` |
| 后台服务 | 邮件/日历/额度 `Service`（[mail.rs](../src/mail.rs) :783 等） | `chat::Service` 同形态 |
| 吉祥物 | `PetState` + `Expressions`（[pet.rs](../src/pet.rs)、[pet_expressions.rs](../src/pet_expressions.rs)） | 新增 `Thinking` 状态 |
| 提示条 | `App::alert`（[extras.rs](../src/extras.rs) :191） | 生成完成提示 |

---

## 3. 关键技术论证

### 3.1 点击 → 聚焦 → 光标（已验证路径）

链路：点击 → `FrameOut.editing=true` → 去掉 `WS_EX_NOACTIVATE` → `Cmd::Focus` → `ViewportCommand::Focus` → egui `focused` → `TextEdit` 画光标并按 0.5s 请求重绘。

四个必须处理的细节：

1. 失焦（`WindowFocused(false)`）清 `input_focused`，保留草稿。
2. `Esc` 只失焦，不清草稿、不删消息（现有全局 `Esc` 会 `cancel_edit()` 清 `buf`）。
3. 输入法候选窗属其它进程，会触发 `EVENT_SYSTEM_FOREGROUND`；必须复用 IME 帧早退，否则中文打字时岛会收起。
4. 收起后 `editing` 必须回到 false，否则透明窗口长期可激活。

### 3.2 中文输入法

- 组合期屏蔽 `Backspace/Delete/Enter/Esc/方向键` 直传。
- 把 `adding || editing.is_some()` 的判定扩展为 `island_input_active()`（待办 / 任务 / 日程草稿 / 对话输入）。
- 现有 5 个 IME 用例（[ui_tests.rs](../src/ui_tests.rs) :2086–2166）必须继续通过。

### 3.3 页面 8 → 9

新页固定索引 **8**，现有 0–7 与 `CALENDAR_PAGE=7` 不变。

| 位置 | 改动 |
| --- | --- |
| [app.rs](../src/app.rs) :52 | `PAGE_NAMES: [&str; 9]`，`CHAT_PAGE=8` |
| :609 | `dots: [Spring; 9]` |
| :1279、:2390 | 页面→设置页映射数组各加一项 `12` |
| :1485 | `expanded_height()` 加 `CHAT_PAGE => CHAT_H` |
| :2421 | `page_title` 加 `8 => ("AI","对话",SOFT_GREEN)` |
| :2480 | `render_page` 加 `CHAT_PAGE` 分支 |
| [model.rs](../src/model.rs) :201 | 两个数组改 9；`page_order` 迁移（8 项补 8）；`visible_pages` 接受 6..=9 |
| [extras.rs](../src/extras.rs) :1051 | AI 组范围 `(11,11)` → `(11,12)` |
| :1132 | `titles` 加 `"AI 对话"`，`.min(11)` → `.min(12)` |
| :1156 | `match settings_tab` 加 `12 => chat_settings(ui)` |
| [ui_tests.rs](../src/ui_tests.rs) | `[true;8]`、8 项 order、`0..8` 循环改 9 |

### 3.4 面板高度与布局

取 `CHAT_H = 248`，不改 `WIN_H=270`。`region = H*1.08+16 ≈ 284`，底部约 14px 极淡阴影被画布裁掉，内容与页码豆都在画布内；三停靠均成立（侧边预留按 236+ 设计）。

面板局部坐标（419 宽）：

| 区域 | 矩形 / 位置 | 说明 |
| --- | --- | --- |
| 标题 | 基线 y=25 | `AI` + `对话`；右上「历史」「新对话」「设置」 |
| 消息区 | (36, 32) 347 × 150 | 嵌套 Ui + ScrollArea，贴底跟随 |
| 状态行 | 基线 y=190 | 模型名 / 正在思考 / 错误（红） |
| 输入条 | (36, 196) 347 × 30 | 圆角 8 底；文本框 (44, 201) 300 × 19；右侧圆钮 r=11 |
| 页码豆 | y = 240.8 | 复用 `nav()` |

### 3.5 网络与流式

- `ureq` + 独立线程，不引入 async 运行时。
- `Agent`：`timeout_connect(10s)`、`timeout_read(30s)`，**不设整体超时**；代理沿用 `quota::system_proxy()` 与环境变量。
- SSE：字节级行缓冲（`Vec<u8>`，避免 UTF-8 被切块），`data:` 前缀、`[DONE]` 结束；解析 `choices[0].delta.content` 与 `delta.reasoning_content`（兼容 `reasoning`）。
- 取消：`Arc<AtomicBool>`，每块检查；停止后保留已生成内容并标记「已停止」。
- 兜底：服务端忽略 `stream` 时按普通 JSON 整包解析。
- 限制：回答 ≤ 64k 字符；请求历史 ≤ 24k 字符；单次 300s 上限；单请求在飞。
- 错误映射：401/403、404、429（读 `Retry-After`）、5xx、连接/超时，均中文提示且不含 Key。

### 3.6 凭据与隐私

- Key 只进 Windows 凭据管理器（service `Daozai.AIChat`，slot = `sha256(preset|base_url)` 前 32 位）。
- `ai-chat.json` 只存 preset / base_url / model / system / 温度 / 长度 / slot 引用。
- 对话只在内存；不写日志、不进诊断文件；错误信息不回显 Key。
- URL 只允许 `https://`，`http://` 仅允许 `127.0.0.1` / `localhost`。

### 3.7 线程模型

```
点击发送 → Cmd::ChatSend(ChatRequest)
main.rs → 读 keyring Key → chat::Service::send(key, req)（后台线程）
worker → 每块 Event::Delta → tx.send + ctx.request_repaint()
main.rs 每帧 → chat.try_recv() → App::chat_event(...)
```

`App` 不联网、不读凭据；UI 测试只断言 `Cmd`，不触网。

### 3.8 自动折叠与后台生成

- `morph()` 的离开延迟按页取：对话页且（输入聚焦或生成中）时 `10.0s`，其余 `0.12s`。
- `want` 里的「编辑中保持展开」条款排除对话页，否则永远不会收起。
- 收起时 `cancel_edit()` 只清输入聚焦，不清草稿与消息；`Service` 线程不受影响。
- 生成完成时若岛已收起，`alert("AI 回复已就绪", 摘要, CHAT_PAGE, false)`，点击提示回到对话页。

### 3.9 吉祥物状态

- 新增 `PetState::Thinking`：眼睛看向左上、轻微上浮、右上角三个脉动小点。
- `pet_state()` 优先级：反应 > 唤醒 > **生成中 > 专注** > 担心 > 音乐 > 好奇 > 睡眠。
- 完成 `react(Happy)`（复用现有 12 组表情），失败 `react(Worried)`。
- 吉祥物只在收起态可见，因此正好对应「后台思考」。

### 3.10 渲染与性能

- 消息气泡用 `layout_job` 折行；egui 自带 galley 缓存，同文本不重复排版；流式只让最后一条变化。
- 只渲染最近 60 条；会话最多 12 个。
- `ScrollArea::stick_to_bottom(true)`，用户上滚时仍可查看。

---

## 4. 设计

### 4.1 数据结构（`src/chat.rs`）

```rust
pub enum Preset { DeepSeek, Kimi, OpenAi, Custom }      // 预设 base_url / model

pub struct Config { preset, base_url, model, system, temperature, max_tokens, slot: Option<String> }

pub enum Role { System, User, Assistant }
pub enum Outcome { Streaming, Ok, Stopped, Failed(String) }
pub struct Message { role, text, reasoning, outcome }
pub struct Conversation { title, messages }

pub struct ChatRequest { config: Config, messages: Vec<Message> }

pub enum Event {
    Delta { reasoning: bool, text: String },
    Done { ms: u64 },
    Stopped,
    Error(String),
    Test(Result<String, String>),   // 设置页测试连接
}

pub struct Service { /* jobs, events, cancel + 后台线程 */ }
impl Service { fn start(ctx); fn send(key,&ChatRequest); fn test(config,key); fn cancel(); fn try_recv(); }
```

凭据：`save_key / load_key / forget_key`（仅 Windows，非 Windows 返回明确错误）。

### 4.2 `Cmd` 扩展

```rust
ChatSend(chat::ChatRequest),
ChatCancel,
ChatTest(chat::Config, String),   // Key 只在内存里传给工作线程
ChatForget,                       // 删除密钥
```

### 4.3 请求体

```json
{"model":"deepseek-chat","stream":true,"temperature":0.7,"max_tokens":1024,
 "messages":[{"role":"system","content":"你是岛仔…"},{"role":"user","content":"…"}]}
```

### 4.4 页面交互

| 操作 | 结果 |
| --- | --- |
| 点消息区 / 输入框 | 聚焦，窗口激活，光标闪烁 |
| 回车 / 发送按钮 | 追加用户消息 + 助手占位，发出请求 |
| 生成中 | 按钮变停止方块；状态行显示「正在思考 / 正在回复 · N 字」 |
| 点停止 | 取消，保留已生成文本，标记「已停止」 |
| 点「历史」 | 消息区切换为会话列表，点击切换；「新对话」新建 |
| 空会话 | 3 条预设问题芯片 |
| Esc / 失焦 | 只失焦，保留草稿 |
| 离开 10s | 收起；生成继续 |
| 生成完成且收起 | 提示条「AI 回复已就绪」 |

### 4.5 设置项

服务商预设（自动填 URL/模型）、Base URL、模型、API Key（密码框）、测试连接并保存密钥、删除密钥、系统提示词、温度、最大长度、清空当前会话。

---

## 5. 实施计划（本轮）

| 阶段 | 内容 | 交付 |
| --- | --- | --- |
| A | `chat.rs`：配置、预设、凭据、SSE、Service、错误映射、单测 | 可单测的网络层 |
| B | 第 9 页注册、布局、消息/思考/历史/预设、输入与聚焦、IME 泛化 | 可打字的岛内对话 UI（本地回显） |
| C | `Cmd`/事件接入、流式、停止、自动折叠、吉祥物状态、完成提示 | 端到端可用 |
| D | 设置第 13 页、密钥与测试连接 | 用户可自助配置 |
| E | 8→9 测试更新、新增对话用例、全量回归 | CI 绿 |

---

## 6. 测试计划

- 单测：SSE 分块/断行/CRLF/UTF-8 跨块/`[DONE]`/非流式兜底；请求体角色与裁剪；错误映射；URL 校验；slot 稳定。
- UI：第 9 页可达、隐藏后导航跳过；点输入区产生 `Cmd::Focus`；输入文字；回车产生 `Cmd::ChatSend`；`Delta` 增长、`Done` 收尾、`Error` 展示；Esc 只失焦；离开 10s 才收起。
- 手动：三停靠 × 五皮肤 × 80/100/140% × 中英文输入 × 断网/401/429 × 长回答。

---

## 7. 回归风险

| 风险 | 对策 |
| --- | --- |
| 页面数组漏改 | 全量 `cargo test`；`page_order` 全排列校验 |
| 老配置迁移丢开关 | 6/7/8 位旧值迁移用例 |
| 输入法把岛收起 | IME 帧早退 |
| 收起时清掉草稿 | 收起只失焦，不清 `chat.draft` |
| 窗口长期可激活 | 收起/失焦强制 `editing=false` |
| 流式卡死 | `timeout_read` + 总时长上限 + 停止 |
| Key 泄露 | 不进配置文件/日志/错误文案 |

---

*本文件随实现更新。*

---

## 8. 实现记录（初版已完成）

| 项 | 实际 |
| --- | --- |
| 新页 | 索引 8「AI 对话」，`CHAT_H = 248`，默认开启，追加在「日程」之后 |
| 设置 | 第 13 个分页「AI 对话」，归入「AI 对话与额度」分组 |
| 新增文件 | `src/chat.rs`（配置、预设、凭据、SSE、Service、10 个单测）<br>`src/chat_ui.rs`（页面、气泡、思考区、历史会话、设置分页） |
| 改动文件 | `app.rs`、`model.rs`、`extras.rs`、`main.rs`、`lib.rs`、`pet.rs`、`ui_tests.rs`、`README.md`、`docs/USAGE.md`、`docs/PRIVACY.md`、`CHANGELOG.md` |
| 新增 Cmd | `ChatSend` / `ChatCancel` / `ChatTest` / `ChatForget` |
| 吉祥物 | 新增 `PetState::Thinking`；生成中思考、完成 `react(Happy)`、失败 `react(Worried)` |
| 自动收起 | 对话页离开统一 10 秒；收起只清输入焦点，草稿与消息、后台生成本身都不受影响 |
| 完成提示 | 岛收起时 `alert("AI 回复已就绪", 摘要, CHAT_PAGE)`，点击提示回到对话页 |
| 测试 | 10 个单元测试 + 4 个 UI 用例全绿（`cargo test -- --test-threads=1 chat`） |

实现中与规划不同、值得记下来的三处：

1. **`leave_delay` 按页判断**，而不是按「输入聚焦/生成中」判断。否则按 `Esc` 失焦后离开计时会退回 0.12 秒，与「离开 10 秒才收起」的预期不符。
2. **上下文预算改为只统计「保留下来的消息」**。第一版把丢弃的前缀也累计进预算，导致历史很长时会把所有消息丢光，只剩 system。
3. **输入框失焦时主动 `surrender_focus`**，避免收起后 egui 仍保留旧焦点，出现「没激活却有光标」的错觉。

后续可做：思考区按消息分别展开、Markdown 与代码块、独立大窗口、会话持久化（默认仍关闭）、Anthropic 原生协议。
