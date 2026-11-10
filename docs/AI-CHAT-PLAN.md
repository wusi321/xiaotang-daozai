# 岛仔 AI 对话页 · 技术可行性与实施规划

> 版本：规划稿 v1（对应代码 v0.6.63）
> 范围：在灵动岛内新增「AI 对话」页，接入 API Key 与模型进行多轮对话；为后续 AI 能力预留结构。
> 结论先行：**技术上完全可行，没有阻塞性风险**。其中三处非平凡点（键盘聚焦、中文输入法、页面数组扩容）在现有代码里都已有可复制的实现。

---

## 0. 结论摘要

| 维度 | 判断 |
| --- | --- |
| 可行性 | 可行。不引入新运行时、不引入 Electron/Node，沿用 `ureq` 同步 HTTP + 工作线程 |
| 新增依赖 | 无（`ureq` / `serde_json` / `keyring` / `ring` 均已在用） |
| 主要改动 | `model.rs`、`app.rs`、`extras.rs`、`main.rs` + 新增 `chat.rs`、`chat_ui.rs` |
| 侵入最大处 | 页面数组 8 → 9、`page_order` 配置迁移（机械但涉及约 10 处测试） |
| 预估工时 | 5–7 人日（含测试与回归），可拆成 4 个可独立验收的阶段 |
| 隐私影响 | API Key 进 Windows 凭据管理器；对话内容默认只在内存，不落盘、不进日志 |

---

## 1. 需求拆解

### 1.1 原始诉求

1. 接入 AI 密钥，进行对话（类似当前 AI agent 框架的用法）。
2. 新建一个页面/对话框，位于灵动岛内。
3. 鼠标移到岛上展开 → 点击后「定向输入」→ 光标闪烁 → 可输入文字。
4. 与 AI 多轮聊天。
5. 后期接其他 AI 相关功能，现在先把「接入 + 对话」做扎实。

### 1.2 v1 必须做到（Must）

- 岛内新增第 9 个功能页「AI 对话」，可开关、可排序（与现有页面一致）。
- 点击输入区后窗口取得键盘焦点，显示闪烁光标，可直接打字（英文与中文输入法）。
- Enter 发送，空内容不发送；发送后显示用户消息 + AI 流式回复。
- 多轮上下文（携带最近若干轮），可清空对话。
- 设置页可填：服务商预设、Base URL、模型名、API Key、系统提示词；「测试连接」通过后才保存 Key。
- 失败可读：401 / 429 / 5xx / 断网 / 超时，各自中文提示，不泄露 Key。
- 生成中可「停止」；页面切换、收起、失焦时正确收尾，不残留半截状态。

### 1.3 v1 明确不做（Out of scope）

- 多会话管理、会话历史持久化（先单会话，重启后清空）。
- 图片/文件输入、函数调用（tools）、联网检索、语音。
- 独立大窗口聊天（可复用「邮件全文窗口」的模式，放到 v2）。
- 非 OpenAI 兼容协议（Anthropic Messages、Gemini generateContent）。
- Markdown 富文本渲染（先纯文本 + 保留换行，代码块高亮放 v2）。

### 1.4 为后续预留（Design for later）

- `Protocol` 枚举 + 每次请求独立构体，后续加协议只改一处。
- `Cmd::ChatSend(ChatRequest)` 抽象，使「发送」不绑定具体服务商。
- 消息结构带 `role`，天然支持多轮与未来 system/tool 角色。

---

## 2. 现有能力盘点（可直接复用）

| 能力 | 现有实现 | 复用方式 |
| --- | --- | --- |
| 岛内文本输入 | `App::input_field`（[app.rs](../src/app.rs) :2770） | 同款 `ui.put(rect, TextEdit)` + 首次 `request_focus()` + `Cmd::Focus` |
| 让窗口可被聚焦 | `native::apply_style(editing)`（[native.rs](../src/native.rs) :273）去掉 `WS_EX_NOACTIVATE`；`Cmd::Focus` → `ViewportCommand::Focus`（[main.rs](../src/main.rs) :598） | `is_editing()` 纳入对话输入状态即可 |
| 光标闪烁 | egui `TextEdit` 内置：viewport 有焦点时按 `text_cursor.blink` 绘制，并自行 `request_repaint_after`（[builder.rs](../vendor/egui/src/widgets/text_edit/builder.rs) :758、[visuals.rs](../vendor/egui/src/text_selection/visuals.rs) :120） | 无需自绘，只要保证焦点与重绘 |
| 中文输入法保护 | `filter_todo_input`（[app.rs](../src/app.rs) :1328）+ `foreground_changed()` 的 `todo_ime_frame` 早退（:1343） | 必须泛化，否则输入法候选窗会触发「前台切走」把岛收起 |
| 面板内滚动列表 | 邮件列表/正文的嵌套 `egui::Ui` + `ScrollArea`（[pages.rs](../src/pages.rs) :942、:1027） | 消息区直接照搬该模式 |
| 独立大窗口 | 邮件全文窗口 `show_viewport_immediate`（[mail_reader.rs](../src/mail_reader.rs)） | v2 聊天大窗口的模板 |
| HTTP + 代理 + 超时 | `quota::system_proxy()` / `http_agent()`（[quota.rs](../src/quota.rs) :329） | 抽一个 `chat_agent()`：连接/读取超时分开，**不设整体超时**（流式会被截断） |
| 系统凭据存取 | `deepseek_quota.rs` 的 `keyring` 封装（[deepseek_quota.rs](../src/deepseek_quota.rs) :41） | 同款 `Entry::new("Daozai.AIChat", slot)` |
| 后台服务 + 事件回灌 | 邮件/日历/额度 `Service` + `mpsc`（[mail.rs](../src/mail.rs) :783、[calendar.rs](../src/calendar.rs) :939、[quota.rs](../src/quota.rs) :275） | `chat::Service` 同一形态 |
| 无窗口测试 | `ui_tests.rs` 的 `H` 注入事件（[ui_tests.rs](../src/ui_tests.rs) :565） | 直接测聚焦、打字、发送、渲染 |
| 页面高度可变 | `expanded_height()`（[app.rs](../src/app.rs) :1485） | 加一支 `CHAT_H` |

---

## 3. 关键技术论证

### 3.1 点击 → 聚焦 → 光标闪烁（最高风险项，但已有验证）

现有待办输入在正式版中可用，对话输入走完全相同的链路：

1. 用户点击输入区 → GDI 命中区命中（窗口此刻仍是 `NOACTIVATE`，`WM_MOUSEACTIVATE` 返回 `MA_NOACTIVATE`，点击照样送达 egui）。
2. 该帧设置 `chat.input_focused = true`，`FrameOut.editing = true`。
3. 同一帧 `native.update(..., editing=true)` 去掉 `WS_EX_NOACTIVATE`（样式立即生效，不重画非客户区）。
4. 下一帧 `Cmd::Focus` → `ViewportCommand::Focus` → 窗口激活。
5. egui 收到 `WindowFocused(true)`，`TextEdit` 画光标并按 0.5s 周期请求重绘；本程序 `schedule_repaint` 只追加更晚的期限，不会压掉更早的重绘请求。
6. 键盘与 IME 事件进入 ROOT viewport，`filter_todo_input` 泛化版做输入法保护。

必须同时处理的 4 个细节：

- **失焦**：收到 `WindowFocused(false)` 时清 `input_focused`，但**保留草稿**（与待办「空白草稿自动退出」的策略不同）。
- **Esc**：全局 `Esc` 目前直接 `cancel_edit()`，会清空 `buf`。对话要单独分支：只让输入框失焦，不清草稿、不删消息。
- **前台切走**：`foreground_changed()` 会 `hover_lock=true` 收起岛。输入法候选窗属于其他进程，会触发该事件——必须复用 `todo_ime_frame` 的早退逻辑，否则**中文打字打到一半岛就收了**。
- **收起时**：`native.update` 的 `editing` 参数目前不看 `hidden`（[main.rs](../src/main.rs) :491）。失焦回调要保证收起后 `editing=false`，避免窗口长时间处于可激活态。

### 3.2 中文输入法

- `Event::Ime(Preedit/Commit)` 与 `Event::Text` 两路都要处理；`TextEdit` 已经处理，我们只做事件过滤。
- 输入法组合期间必须屏蔽 `Backspace/Delete/Enter/Esc/方向键` 的直传（现有 `filter_todo_input` :1340 的保留列表），否则「选词退格删正文、选词回车直接发送」。
- 泛化方案：把 `adding || editing.is_some()` 的判定改为 `island_input_active()`（待办 / 任务编辑 / 日程草稿 / 对话输入），`todo_ime_active` 改名 `island_ime_active`，测试同步改名。
- 回归：现有 5 个 IME 用例（[ui_tests.rs](../src/ui_tests.rs) :2086–2166）必须继续通过。

### 3.3 页面从 8 扩到 9（唯一「侵入式」改动）

策略：**新页固定为索引 8（追加在「日程」之后）**，现有 0–7 的语义、`CALENDAR_PAGE=7`、所有硬编码页号全部不变。这是把改动面压到最小的关键决定。

触点清单：

| 位置 | 改动 |
| --- | --- |
| [app.rs](../src/app.rs) :52 | `PAGE_NAMES: [&str; 9]`，追加 `"AI 对话"`；新增 `pub(crate) const CHAT_PAGE: usize = 8;` |
| :609 | `dots: [Spring; 9]`，初始化 `std::array::from_fn` 已是按 9 生成 |
| :1485 | `expanded_height()` 增加 `if self.page==CHAT_PAGE {CHAT_H}` |
| :2386 | 设置页映射 `[1,2,3,5,4,7,11,8]` → 末尾加 `12` |
| :2421 | `page_title` 增加 `8 => ("AI", "对话", theme::SOFT_GREEN)` |
| :2480 | `render_page` 增加 `CHAT_PAGE => self.page_chat(c, env, cmds)` |
| :2489 | 在 `CALENDAR_PAGE =>` 之后、`_ =>` 之前插入 `CHAT_PAGE => self.page_chat(c, env, cmds)` |
| [model.rs](../src/model.rs) :201 | `visible_pages: [bool; 9]`、`page_order: [usize; 9]` |
| :245 | 默认值 `[true;9]`、`[0,1,2,3,4,5,6,7,8]` |
| :298 | `page_order` 迁移：长度 8 且为 0..8 全排列 → 末尾补 8；长度 9 且全排列 → 采用；否则保持默认 |
| :308 | `visible_pages` 接受长度 6..=9；短于 9 时尾部沿用「任一为真」的旧规则（现有逻辑天然把新页设为可见） |
| :344 | 保底逻辑不变（至少一页可见） |
| [extras.rs](../src/extras.rs) :1051 | `SETTINGS_GROUPS` 的 AI 组 `(11,11)` → `(11,12)`，组名可改「AI 对话与额度」 |
| :1132 | `titles` 追加 `"AI 对话"`；`titles[self.settings_tab.min(11)]` → `.min(12)` |
| :1156 附近 | `match self.settings_tab` 增加 `12 => self.chat_settings(ui)` |
| :1169 | 页面说明 `descriptions` 增加第 9 条 |
| [ui_tests.rs](../src/ui_tests.rs) | 约 10 处 `[true;8]` / 8 元 `page_order` / `0..8` 循环改为 9，并补 8 长度迁移用例 |

迁移兼容性：老用户 `settings.txt` 里是 6/7/8 位 `visible_pages` 与 8 项 `page_order`，上述规则保证「原开关不丢、新页默认开启」。与当初新增「日程页」的处理方式一致（现有测试 :1896 已固化该语义）。

### 3.4 面板高度与几何

- 建议 `CHAT_H = 236`，与「邮件正文」页同高（[app.rs](../src/app.rs) :1486）。理由：
  - 画布 `WIN_H=270` 不变，`region` 公式 `H*1.08+16 ≈ 271`，只裁掉约 1px 阴影，与邮件页现状一致；
  - 左右停靠的 `SIDE_HALF_EXTENT=150` 已按「展开面板最高约 236」预留（[side_dock.rs](../src/side_dock.rs) :19），三停靠都成立；
  - 不必改 `WIN_H`，也就不必同步 `composition.rs` 的 960×270 底板、录制器画布、大量测试常量。
- 如果后续想要更高的聊天区（≥ 250），再按一次「小改」处理：`WIN_H 270 → 300`，同步 `composition.rs:219`、`daozai-record` 的 `CH=250`、预览测试的固定尺寸。**不建议 v1 就做**。
- 布局（面板局部坐标，419 宽）：

| 区域 | 矩形 | 说明 |
| --- | --- | --- |
| 标题 | y 基线 25 | `AI` + `对话`，与其它页一致；右上设置齿轮复用 |
| 消息区 | (36, 34) 347 × 152 | 嵌套 Ui + ScrollArea，`stick_to_bottom` |
| 输入条 | (36, 190) 347 × 28 | 圆角 8 底，文本框 + 右侧发送/停止圆钮 |
| 状态行 | y 基线 224 | 「正在回复 · 12 字」「已停止」「连接失败」 |
| 页码豆 | y = 236 − 7.2 | 复用 `nav()` 计算 |
| 左右翻页箭头 | x 8..34 / 385..411，y=68 | x 不与内容重叠，保持现有位置 |

### 3.5 网络与流式

- 用 `ureq` 同步请求 + 独立线程，**不引入 tokio/reqwest**，与全项目一致，二进制体积可控。
- `Agent` 构造：`timeout_connect(10s)` + `timeout_read(30s)`，**不设 `.timeout()`**（整体超时会切断长回答）。代理沿用 `quota::system_proxy()` + `HTTPS_PROXY` 等环境变量。
- 流式：`POST {base}/chat/completions`，`stream: true`，从 `response.into_reader()` 逐块读，按 `\n` 切行：
  - 行缓冲必须是 `Vec<u8>`（**不能是 String**），否则 UTF-8 多字节被切在两块之间会乱码；
  - `data: ` 前缀后 JSON 解析，取 `choices[0].delta.content`；
  - `data: [DONE]` 结束；空行忽略；`\r\n` 兼容。
- 非流式兜底：若服务端忽略 `stream`（返回普通 JSON），整包解析后一次性给出一条消息。
- 取消：`Arc<AtomicBool>`，worker 每读到一块检查一次，置位即跳出并 drop response（关闭连接）。「停止」后保留已生成内容并标记「已停止」。
- 错误映射（与 `deepseek_quota::response_error` 同风格）：
  - 401/403 → 「API Key 无效或无权限」
  - 404 → 「Base URL 或模型名不正确」
  - 429 → 「请求过于频繁，请稍后重试」（读取 `Retry-After`）
  - 5xx → 「服务暂不可用」
  - 连接失败/超时 → 「无法连接，请检查网络或代理」
- 限制：单次回答上限 32k 字符；请求历史上限 24k 字符（超出丢最早轮次，保留 system）；单次请求最大 1 个在飞。

### 3.6 凭据与隐私

- API Key 只进 Windows 凭据管理器（`keyring`，service = `Daozai.AIChat`，slot = `sha256(provider|base_url)` 的前缀），配置文件只存 provider/base_url/model/别名。与 DeepSeek 余额的实现完全一致。
- `chat.rs` 不写任何日志；错误信息不回显 Key；`Debug` 实现不打印 Key（Key 只以局部 `String` 短暂存在，用完即丢弃，不额外引入加密 crate）。
- 对话内容默认**只在内存**，不写 `%APPDATA%`，不进诊断文件。设置页明确写一行说明。
- URL 校验：只允许 `https://`；`http://` 仅允许 `127.0.0.1` / `localhost`（本地 Ollama、LM Studio）。非 http(s) 直接拒绝。
- 与 README 的隐私口径一致：开启对话后才会访问用户填写的服务商地址。

### 3.7 线程模型与重绘

沿用「Cmd 出 / Event 入」：

```
App::frame
  └─ 点击发送 → pending.push(Cmd::ChatSend(ChatRequest{messages, config}))
main.rs 执行 Cmd
  └─ 读取 keyring 中的 Key → chat::Service::send(...) → 起线程
       └─ 每块 delta：tx.send(Event::Delta) + ctx.request_repaint()
main.rs 每帧
  └─ while let Some(ev) = chat.try_recv() { app.chat_event(ev) }
```

- `App` 不碰网络、不读内存凭据，符合「界面与平台无关」的既有约束。
- 重绘由 worker 主动 `ctx.request_repaint()` 驱动，不受静态 1s 节流影响。
- 测试中 `App` 只产出 `Cmd::ChatSend`，不触网；事件用 `app.chat_event(...)` 直接注入。

### 3.8 渲染与性能

- 每条消息缓存 `Arc<egui::Galley>`（按宽度/字号/文本哈希失效），流式时只重建最后一条；否则 60fps × 几十条消息的文本重排会吃掉预算。
- 只渲染最近 40 条（更早的折叠为「更早的 12 条…」按钮，v1 可只保留 40 条上限）。
- 滚动：`ScrollArea::vertical().stick_to_bottom(true)`；用户手动上滚时暂停跟随（记录 `follow` 标志，滚回底部恢复）。
- 面板是小面积绘制，`Card` 裁剪已保证不会溢出到岛外；毛玻璃区域只认岛体轮廓，不需要为新页加 `GlassShape`。

---

## 4. 设计草案

### 4.1 数据结构（`src/chat.rs`）

```rust
pub enum Protocol { OpenAiCompatible }          // v1 仅一种，预留扩展

pub struct Config {
    pub provider: Preset,        // DeepSeek / Kimi / OpenAI / 自定义
    pub base_url: String,        // 形如 https://api.deepseek.com/v1
    pub model: String,
    pub system: String,
    pub temperature: f32,        // 0..=2
    pub max_tokens: u32,
    pub alias: String,
    pub slot: Option<String>,    // 凭据槽位（存 Key 时才写入）
}

pub enum Role { System, User, Assistant }

pub struct Message { pub role: Role, pub text: String, pub at: f64 }

pub struct ChatRequest { pub config: Config, pub messages: Vec<Message> }

pub enum Event {
    Delta(String),
    Done,
    Error(String),
}

pub struct Service { /* tx 请求、rx 事件、cancel: Arc<AtomicBool>、join 句柄 */ }
impl Service {
    pub fn start(ctx: egui::Context) -> Self;
    pub fn send(&self, key: &str, req: ChatRequest) -> Result<(), String>;
    pub fn cancel(&self);
    pub fn try_recv(&self) -> Option<Event>;
}
```

凭据函数（仅 Windows）：

```rust
fn credential(slot: &str) -> Result<keyring::Entry, String>;
pub fn save_key(slot: &str, key: &str) -> Result<(), String>;
pub fn load_key(slot: &str) -> Result<String, String>;
pub fn forget_key(slot: &str);
```

### 4.2 Cmd / Event 扩展（`src/app.rs`）

```rust
pub enum Cmd {
    // ...
    ChatSend(chat::ChatRequest),   // 需要在 main.rs 里读 Key 后真正发出
    ChatTest(chat::Config),        // 设置页「测试连接」
    ChatCancel,                    // 停止生成
}
```

`ChatRequest` 需 `Clone + Debug + PartialEq` 以满足 `Cmd` 的 derive；`Message.text` 参与 `PartialEq` 没问题（仅内存比较）。

### 4.3 请求体（OpenAI 兼容）

```json
{
  "model": "deepseek-chat",
  "messages": [
    {"role": "system", "content": "你是岛仔，回答简洁，默认中文。"},
    {"role": "user", "content": "帮我把这句话改得更客气一点"},
    {"role": "assistant", "content": "……"}
  ],
  "stream": true,
  "temperature": 0.7,
  "max_tokens": 1024
}
```

响应（SSE）：`data: {"choices":[{"delta":{"content":"你"}}]}` … `data: [DONE]`。

### 4.4 页面状态

```rust
pub(crate) struct ChatState {
    pub messages: Vec<chat::Message>,
    pub draft: String,
    pub input_focused: bool,
    pub streaming: Option<Streaming>,   // started_at, chars
    pub error: String,
    pub follow: bool,
    pub clear_confirm: bool,
    pub form: ChatForm,                 // 设置页表单（含临时 Key 输入）
    pub note: String,                   // 测试连接结果
    pub testing: bool,
}
```

### 4.5 状态机

```
空会话 --点击输入--> 聚焦(typing) --Enter--> 发送中(streaming) --Done--> 完成
                                            |--Error--> 失败(可重试)
                                            |--停止--> 已停止(保留内容)
聚焦 --Esc/失焦/切页/切走--> 空闲(保留草稿)
```

### 4.6 设置项（新增第 13 个分页「AI 对话」）

| 项 | 控件 | 说明 |
| --- | --- | --- |
| 服务商 | 下拉预设 | DeepSeek（默认）/ Kimi / OpenAI / 自定义 |
| Base URL | 单行输入 | 选预设时自动填，可改 |
| 模型 | 单行输入 + 预设建议 | 如 `deepseek-chat`、`kimi-k2`、`gpt-4o-mini` |
| API Key | 密码框 | 只在内存；「测试并保存」通过后进凭据管理器 |
| 测试连接 | 按钮 | 发一条 `max_tokens=8` 的最小请求，显示耗时与模型回显 |
| 系统提示词 | 多行 | 默认「你是岛仔，回答简洁，默认中文。」 |
| 温度 / 最大长度 | 滑条/步进 | 0.0–2.0 / 256–4096 |
| 清除对话 | 按钮 | 二次确认，只清内存 |

---

## 5. 实施计划（4 个阶段，每阶段可独立验收）

### 阶段 0：准备与探针（0.5 人日）

- 建 `chat::Config` 与预设表、`ai-chat.json` 读写、凭据封装（含非 Windows 桩）。
- 用一个 `#[ignore]` 的真实 Key 诊断用例验证：SSE 能逐块到达、取消能断开（手动跑，不進 CI）。
- 验收：`cargo test` 全绿；诊断用例输出分段计数与首字延迟。

### 阶段 1：页面与输入骨架（1.5 人日）

- 第 9 页注册（3.3 全部触点 + 测试更新 + 迁移用例）。
- 页面布局、空状态、消息气泡、输入框、发送/停止按钮（先本地回显，不联网）。
- 聚焦链路：`is_editing` 纳入、Esc 特判、失焦保留草稿、IME 过滤泛化、`foreground_changed` 保护。
- 验收：鼠标悬停展开 → 点输入区 → 光标闪烁 → 中英文都能输入 → Enter 后出现「你」的气泡和占位回复；切页/收起/Esc 行为正确；DPI 80%/100%/140% 与三停靠截图正常。

### 阶段 2：网络、流式与会话（2.5 人日）

- `chat::Service`、SSE 解析、取消、错误映射、代理、超时策略。
- `Cmd::ChatSend/ChatCancel` 与 `chat_event` 接入；流式渲染与「停止」。
- 多轮上下文裁剪；重试（429 一次退避）。
- 验收：真实 DeepSeek Key 端到端；200 字回答首字 < 1.5s；停止在 200ms 内生效；401/404/断网提示正确；连续 5 轮上下文连贯。

### 阶段 3：设置与打磨（1.5 人日）

- 第 13 个设置分页、测试连接、保存/清除 Key、系统提示词、温度/长度、清空对话。
- 空状态引导（未配置 → 「去设置」）、消息复制、耗时显示、超长文本折叠。
- 验收：全新用户从空状态到成功对话不超过 3 步；Key 只出现在密码框与凭据管理器。

### 阶段 4：回归与文档（1 人日）

- 全量 `cargo test --locked --all-targets --features record -- --test-threads=1`。
- 手动回归矩阵（见 §6.3）。
- README 功能段、`docs/USAGE.md`、`CHANGELOG.md`；如发版再动版本号。

**合计 5–7 人日。** 若只求最快可用：阶段 0+1+2 的核心部分约 3.5 人日即可跑通，设置页可先用 `ai-chat.json` 手填。

---

## 6. 测试计划

### 6.1 单元测试（`chat.rs` 内）

- SSE 解析：单块完整、跨块断行、`\r\n`、多事件同块、`[DONE]`、空 `data:`、非法 JSON 跳过、**UTF-8 汉字跨块**。
- 请求体：角色顺序、system 注入、历史裁剪不破坏成对轮次、`stream` 字段。
- 错误映射：401/403/404/429/500/连接失败文案互不混淆，且不含 Key。
- 配置：预设切换填写、URL 校验（拒绝 `http://` 公网、允许 `http://127.0.0.1`）、JSON 往返。
- 凭据：非 Windows 返回明确错误；slot 稳定（同配置同槽）。

### 6.2 UI 测试（`ui_tests.rs`）

- `show_page(8)` 可达；隐藏后导航跳过；`page_order` 9 项持久化与迁移。
- 点击输入区 → `is_editing()==true` 且产生 `Cmd::Focus`；`Esc` 只失焦不清草稿。
- 注入 `Event::Text` 后 `draft` 正确；Enter 产生 `Cmd::ChatSend`，空草稿不产生。
- `chat_event(Delta)` 后最后一条 assistant 文本增长；`Done` 清 streaming；`Error` 显示文案。
- IME 组合期 Backspace/Enter 不透传（沿用现有 5 个用例 + 1 个对话用例）。
- 页面高度：`expanded_height()==CHAT_H`，命中区包含输入条，页码豆不与输入条重叠。

### 6.3 手动回归矩阵

| 维度 | 取值 |
| --- | --- |
| 停靠 | 顶部 / 左侧 / 右侧 |
| 皮肤 | 经典 / 奶油 / 毛玻璃·浅 / 毛玻璃·深 / 悬浮云 |
| 尺寸 | 80% / 100% / 140% |
| 形态 | 刘海 / 胶囊；极简开/关 |
| 输入 | 英文 / 中文输入法（微软拼音，含候选窗翻页）/ 粘贴多行 |
| 焦点 | 点输入、点岛外、Alt+Tab、点任务栏、全屏应用、锁屏恢复 |
| 网络 | 正常 / 断网 / 超时 / 401 / 429 |
| 长内容 | 200 字回答、2000 字回答、连续 10 轮 |

---

## 7. 回归风险清单

| # | 风险 | 影响 | 对策 |
| --- | --- | --- | --- |
| 1 | 页面数组扩容漏改 | 启动 panic 或页面错位 | 用 `cargo test` 全量兜底；`page_order` 校验保留全排列检查 |
| 2 | 老配置迁移丢开关 | 用户页面消失 | 迁移用例覆盖 6/7/8 位旧值；与日程页历史行为一致 |
| 3 | 输入法候选窗触发收起 | 中文打一半岛没了 | 复用 `todo_ime_frame`，`foreground_changed` 早退 |
| 4 | Esc 清空对话草稿 | 误删输入 | Esc 分支特判，只失焦 |
| 5 | 窗口长时间可激活 | 点击穿透失效、误激活 | 失焦/收起时强制 `editing=false`；`FrameOut.editing` 纳入 `!hidden` 判断 |
| 6 | 流式无整体超时导致卡死 | 一直「正在回复」 | `timeout_read` + 单次总时长上限（如 180s）+ 可停止 |
| 7 | 大回答每帧重排文本 | 掉帧、CPU 高 | 缓存 Galley、只重排流式最后一条、消息上限 |
| 8 | Key 泄露进日志/截图 | 安全事故 | 不打印、不落盘、密码框默认遮蔽、错误文案脱敏 |
| 9 | 非 Windows 编译失败 | CI 红 | `chat.rs` 凭据与 `keyring` 全部 `cfg(windows)`，与 `deepseek_quota` 同构 |
| 10 | 录制器/预览测试尺寸假设 | 截图测试失败 | `CHAT_H` 不超出 `WIN_H`，不改画布常量 |

---

## 8. 需要你拍板的 6 个决定

| # | 问题 | 我的建议 |
| --- | --- | --- |
| 1 | v1 只做 OpenAI 兼容协议，还是同时做 Anthropic 原生？ | **只做 OpenAI 兼容**（覆盖 DeepSeek/Kimi/OpenAI/硅基流动/Ollama 等），Anthropic 放 v2 |
| 2 | 默认预设选谁？ | **DeepSeek**（已有 Key 体系与余额页，模型 `deepseek-chat`） |
| 3 | 对话是否持久化到本机？ | **不持久化**（v1 只存内存）；以后要历史再加开关 |
| 4 | 新页放在哪、默认是否开启？ | **追加在「日程」之后（索引 8）+ 默认开启**，与当年新增日程页一致 |
| 5 | 是否要独立大窗口聊天？ | **v1 只做岛内**，v2 复用邮件全文窗口模式做「展开大窗」 |
| 6 | 单会话还是多会话？ | **单会话 + 清空**；多会话等有持久化后再做 |

---

## 9. 改动文件清单（预估）

| 文件 | 类型 | 内容 |
| --- | --- | --- |
| `src/chat.rs` | 新增 | 配置、预设、凭据、SSE、Service、错误映射、单测（约 700–900 行） |
| `src/chat_ui.rs` | 新增 | 岛内页面、消息渲染、输入框、设置分页（约 500–700 行） |
| `src/app.rs` | 改 | 第 9 页注册、`ChatState`、`Cmd` 扩展、`is_editing`、Esc/失焦/IME、`chat_event` |
| `src/model.rs` | 改 | 9 项数组、`page_order` 迁移 |
| `src/extras.rs` | 改 | 第 13 个设置分页、页面说明 |
| `src/main.rs` | 改 | `chat::Service` 装配、`Cmd::ChatSend/ChatTest/ChatCancel` 处理、事件回灌 |
| `src/lib.rs` | 改 | 注册 `mod chat; mod chat_ui;` |
| `src/ui_tests.rs` | 改 | 8→9 相关约 10 处 + 新增对话用例约 8–10 个 |
| `src/bin/daozai-record/main.rs` | 改（可选） | 演示场景加一屏假对话，便于出图 |
| `docs/USAGE.md`、`README.md`、`CHANGELOG.md` | 改 | 使用说明、功能段、更新记录 |

---

## 10. 附：为什么不用另外三条路

| 备选方案 | 否决理由 |
| --- | --- |
| 岛内嵌 WebView 指向网页版 | 破坏「无 Electron/无 WebView」的定位；无法命中穿透与岛体轮廓；内存暴涨 |
| 独立聊天子进程 / 本地服务 | 多一个进程与端口，安装、更新、崩溃恢复都变复杂；现有 Service 线程模型够用 |
| 引入 async 运行时（tokio + reqwest） | 依赖体积与安全面变大，与现有同步 HTTP 风格冲突；流式用同步 reader 完全可行 |

---

*本文件为规划稿，未改动任何实现代码。确认后再按阶段执行。*
