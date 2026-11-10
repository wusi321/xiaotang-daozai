//! 岛内 AI 对话页：消息气泡、思考区、历史会话、输入框与设置分页。
use crate::app::{App, Cmd, Env, CHAT_PAGE};
use crate::chat::{self, Message, Outcome, Role};
use crate::paint::{self, Card, H};
use crate::theme::{self, W};
use eframe::egui::{self, Color32, Pos2, Rect};

/// 一轮正在生成的回答。
pub(crate) struct Streaming { pub started: f64, pub chars: usize, pub reasoning: bool }

pub(crate) struct State {
    pub config: chat::Config,
    /// 会话只在内存里：重启即清空，不写任何文件。
    pub conversations: Vec<chat::Conversation>,
    pub active: usize,
    pub draft: String,
    pub input_focused: bool,
    pub focus_requested: bool,
    pub streaming: Option<Streaming>,
    pub status: String,
    pub status_error: bool,
    pub show_history: bool,
    pub reasoning_open: bool,
    /// 待发送的附件：只保存路径，图片缩略图按需解码并缓存。
    pub attachments: Vec<chat::Attachment>,
    pub thumbs: std::collections::HashMap<std::path::PathBuf, Option<egui::TextureHandle>>,
    pub key_input: String,
    pub testing: bool,
    pub note: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            config: chat::Config::default(),
            conversations: vec![chat::Conversation::default()],
            active: 0,
            draft: String::new(),
            input_focused: false,
            focus_requested: false,
            streaming: None,
            status: String::new(),
            status_error: false,
            show_history: false,
            reasoning_open: false,
            attachments: Vec::new(),
            thumbs: Default::default(),
            key_input: String::new(),
            testing: false,
            note: String::new(),
        }
    }
}

impl State {
    pub(crate) fn load() -> Self { Self { config: chat::Config::load(), ..Default::default() } }
    pub(crate) fn index(&self) -> usize { self.active.min(self.conversations.len().saturating_sub(1)) }
    pub(crate) fn conversation(&self) -> &chat::Conversation { &self.conversations[self.index()] }
    pub(crate) fn conversation_mut(&mut self) -> &mut chat::Conversation { let index = self.index(); &mut self.conversations[index] }
    pub(crate) fn is_streaming(&self) -> bool { self.streaming.is_some() }
}

fn layout_text(ctx: &egui::Context, text: &str, size: f32, weight: W, color: Color32, wrap: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), theme::font(size, weight), color, wrap.max(24.0));
    job.wrap.break_anywhere = true;
    ctx.fonts(|fonts| fonts.layout_job(job))
}

/// 整段文本都是存在的本机文件路径时返回路径（每行一个，支持引号和 file:///）。
fn paths_in(text: &str) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_matches('"').trim_matches('\'');
        if line.is_empty() { continue; }
        let line = match line.strip_prefix("file:///") {
            Some(rest) => std::path::PathBuf::from(rest.replace('/', "\\")),
            None => std::path::PathBuf::from(line),
        };
        if !line.is_file() { return Vec::new(); }
        paths.push(line);
        if paths.len() >= chat::MAX_ATTACHMENTS { break; }
    }
    paths
}

/// 图片附件的小缩略图；解码失败就退回类型角标，不影响发送。
fn load_thumbnail(ctx: &egui::Context, path: &std::path::Path) -> Option<egui::TextureHandle> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 8 * 1024 * 1024 { return None; }
    let image = image::open(path).ok()?.thumbnail(64, 64).to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(ctx.load_texture(format!("chat-thumb-{}", path.display()), egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()), egui::TextureOptions::LINEAR))
}

/// 消息列表：贴底跟随，用户上滚后保持位置。返回被点击的「思考」折叠行。
fn draw_rows(c: &Card, area: Rect, messages: &[Message], reasoning_open: bool, live: bool, salt: usize) -> Option<usize> {
    let ctx = c.ui.ctx().clone();
    let mut ui = egui::Ui::new(ctx.clone(), egui::Id::new(("chat-list", live, salt)),
        egui::UiBuilder::new().max_rect(area).layer_id(c.painter.layer_id()).style(c.ui.style().clone()));
    ui.set_clip_rect(area.intersect(c.painter.clip_rect()));
    ui.set_opacity(c.painter.opacity());
    if !live { ui.disable(); }
    ui.spacing_mut().item_spacing.y = 6.0 * c.s;
    ui.spacing_mut().scroll.floating = false;
    ui.spacing_mut().scroll.bar_width = 4.0 * c.s;
    ui.visuals_mut().widgets.inactive.bg_fill = Color32::from_gray(72);
    ui.visuals_mut().widgets.hovered.bg_fill = Color32::from_gray(110);
    // 这里的宽度统一用面板局部点；area 是屏幕坐标，需要除掉缩放。
    // 再留 6 点给右侧滚动条，避免长回答贴到滚动条上。
    let s = c.s.max(0.05);
    let width = area.width() / s - 6.0;
    let mut toggle = None;
    egui::ScrollArea::vertical().id_salt(("chat-scroll", salt)).max_height(area.height())
        .auto_shrink([false, false]).stick_to_bottom(true)
        .show(&mut ui, |ui| {
            for (index, message) in messages.iter().enumerate() {
                let user = message.role == Role::User;
                let reasoning = !user && !message.reasoning.is_empty();
                // 只有思考时正文用占位，避免和折叠的思考摘要重复一遍。
                let body = if !message.text.is_empty() { message.text.as_str() }
                    else if reasoning { if message.is_streaming() { "正在思考…" } else { "（没有回答内容）" } }
                    else if message.is_streaming() { "…" } else { "（没有内容）" };
                let ink = if user { theme::WHITE } else { Color32::from_gray(232) };
                let body = layout_text(&ctx, body, 11.5 * s, W::Regular, ink, (width - 22.0) * s);
                let reason = reasoning.then(|| layout_text(&ctx, &message.reasoning, 10.0 * s, W::Regular, Color32::from_gray(150), (width - 22.0) * s));
                let head = if reasoning && !reasoning_open { 14.0 } else { 0.0 };
                let reason_h = if reasoning && reasoning_open { reason.as_ref().map_or(0.0, |galley| galley.size().y / s) + 9.0 } else { 0.0 };
                let body_h = body.size().y / s;
                let h = (head + reason_h + body_h + 13.0).max(24.0);
                let (rect, response) = ui.allocate_exact_size(egui::vec2(width * s, h * s), egui::Sense::click());
                let mut row = Card::new(ui, rect.min, ui.clip_rect());
                row.s = s;
                let bubble_w = ((body.size().x / s) + 22.0).min(width).max(40.0);
                let x = if user { width - bubble_w } else { 0.0 };
                row.fill(x, 0.0, bubble_w, h, 10.0, if user { Color32::from_rgb(38, 84, 138) } else { Color32::from_rgb(34, 34, 38) });
                let mut y = 0.0;
                if head > 0.0 {
                    row.text(x + 11.0, 10.0, &format!("思考 {} 字 · 点击展开", message.reasoning.chars().count()), 9.0, W::Regular, Color32::from_gray(150), H::Left);
                    y += head;
                }
                if reasoning && reasoning_open {
                    if let Some(galley) = &reason {
                        row.text(x + 11.0, y + 9.0, "思考过程", 8.5, W::Regular, Color32::from_gray(122), H::Left);
                        row.painter.galley(row.pos(x + 11.0, y + 12.0), galley.clone(), Color32::from_gray(150));
                        y += reason_h;
                    }
                }
                row.painter.galley(row.pos(x + 11.0, y + 6.0), body.clone(), ink);
                if response.clicked() && reasoning { toggle = Some(index); }
            }
        });
    toggle
}

impl App {
    pub(crate) fn page_chat(&mut self, c: &Card, _env: &Env, cmds: &mut Vec<Cmd>) {
        let history = c.hit(286.0, 5.0, 50.0, 20.0, "chat-history").on_hover_text("历史会话（只保存在本次运行）");
        c.text(311.0, 19.0, "历史", 10.0, W::Regular, if history.hovered() { theme::WHITE } else { theme::GRAY }, H::Center);
        if history.clicked() { self.chat.show_history = !self.chat.show_history; self.chat.input_focused = false; }
        let fresh = c.hit(340.0, 5.0, 26.0, 20.0, "chat-new").on_hover_text("新对话");
        c.text(353.0, 19.0, "＋", 13.0, W::Semibold, if fresh.hovered() { theme::WHITE } else { theme::GRAY }, H::Center);
        if fresh.clicked() { self.chat_new(); }

        if self.chat.show_history {
            self.draw_chat_history(c);
        } else if self.chat.conversation().messages.is_empty() && !self.chat.is_streaming() {
            self.draw_chat_empty(c, cmds);
        } else {
            self.draw_chat_messages(c, cmds);
        }

        if self.chat.show_history { return; }
        self.draw_chat_attachments(c);
        let (status, error) = self.chat_status();
        if self.drag_hot {
            c.text(36.0, 190.0, "松开即可添加为附件（只记录路径）", 8.5, W::Regular, theme::BLUE, H::Left);
        } else if !status.is_empty() {
            c.text(36.0, 190.0, &c.fit(&status, 8.5, W::Regular, 288.0), 8.5, W::Regular, if error { theme::RED } else { theme::GRAY }, H::Left);
        }

        // 输入条：背景与「点这里开始输入」的感应区；真正的文本框在 chat_inputs 里叠在上面。
        c.fill(36.0, 196.0, 347.0, 30.0, 9.0, theme::BTN);
        let clip = c.hit(38.0, 199.0, 22.0, 24.0, "chat-attach").on_hover_text("添加图片或文件（也可拖入、粘贴本机路径）");
        c.text(49.0, 216.0, "＋", 15.0, W::Regular, if clip.hovered() { theme::WHITE } else { theme::GRAY }, H::Center);
        if clip.clicked() { cmds.push(Cmd::ChatAttach); }
        let focus = c.hit(62.0, 197.0, 296.0, 28.0, "chat-focus");
        if focus.clicked() { self.chat.focus_requested = true; }
        let button = c.hit(362.0, 199.0, 24.0, 24.0, "chat-send").on_hover_text(if self.chat.is_streaming() { "停止生成" } else { "发送（回车）" });
        let (hover, press) = c.hover_t(&button);
        let streaming = self.chat.is_streaming();
        let base = if streaming { Color32::from_rgb(122, 62, 62) } else { Color32::from_rgb(44, 104, 168) };
        let tint = if streaming { Color32::from_rgb(150, 78, 78) } else { Color32::from_rgb(64, 128, 198) };
        c.circle(374.0, 211.0, 11.0 - press * 0.6, paint::mix(base, tint, hover));
        if streaming {
            c.fill(370.0, 207.0, 8.0, 8.0, 1.5, theme::WHITE);
        } else {
            c.poly(&[(378.5, 211.0), (371.0, 206.6), (371.0, 215.4)], theme::WHITE);
        }
        if button.clicked() {
            if streaming {
                cmds.push(Cmd::ChatCancel);
                self.chat.status = "正在停止…".into();
                self.chat.status_error = false;
            } else {
                self.chat_send(cmds);
            }
        }
    }

    fn draw_chat_empty(&mut self, c: &Card, cmds: &mut Vec<Cmd>) {
        if self.chat.config.slot.is_none() {
            c.text(36.0, 58.0, "还没连接 AI", 12.0, W::Semibold, theme::WHITE, H::Left);
            let hint = c.fit("到 设置 → AI 对话 填写接口地址、模型和 API Key", 10.0, W::Regular, 330.0);
            c.text(36.0, 75.0, &hint, 10.0, W::Regular, theme::GRAY, H::Left);
            let (go, gc) = paint::capsule(c, 36.0, 88.0, 92.0, 26.0, "chat-setup", theme::BTN, theme::BTN_HOVER);
            gc.centered_text(82.0, 101.0, "去设置", 10.0, W::Semibold, theme::WHITE, H::Center);
            if go.clicked() { self.open_settings(); self.settings_tab = 12; }
            return;
        }
        let presets = ["用一句话解释什么是依赖注入", "给我三个提高专注力的小技巧", "帮我想一个周末计划"];
        for (index, text) in presets.iter().enumerate() {
            let id = ["chat-preset-0", "chat-preset-1", "chat-preset-2"][index];
            let y = 46.0 + index as f32 * 32.0;
            let (hit, chip) = paint::capsule(c, 36.0, y, 347.0, 26.0, id, theme::BTN, theme::BTN_HOVER);
            chip.text(50.0, y + 17.0, &chip.fit(*text, 10.5, W::Regular, 318.0), 10.5, W::Regular, Color32::from_gray(220), H::Left);
            if hit.clicked() { self.chat.draft = (*text).to_string(); self.chat_send(cmds); }
        }
        c.text(36.0, 148.0, "回答显示在这里；生成中可随时停止。", 9.0, W::Regular, theme::DIM, H::Left);
    }

    fn draw_chat_messages(&mut self, c: &Card, _cmds: &mut Vec<Cmd>) {
        let index = self.chat.index();
        let live = c.live;
        let reasoning_open = self.chat.reasoning_open;
        let height = if self.chat.attachments.is_empty() { 150.0 } else { 118.0 };
        let area = c.rect(36.0, 32.0, 347.0, height);
        let toggle = {
            let messages = &self.chat.conversations[index].messages;
            draw_rows(c, area, messages, reasoning_open, live, index)
        };
        if toggle.is_some() { self.chat.reasoning_open = !self.chat.reasoning_open; }
    }

    /// 附件条：只显示本机路径的缩略图/类型，并提供一个「允许读取内容」的开关。
    fn draw_chat_attachments(&mut self, c: &Card) {
        if self.chat.attachments.is_empty() { return; }
        let mut remove = None;
        for index in 0..self.chat.attachments.len().min(chat::MAX_ATTACHMENTS) {
            let attachment = self.chat.attachments[index].clone();
            let (x, y) = (38.0 + index as f32 * 62.0, 152.0);
            c.fill(x, y, 60.0, 28.0, 6.0, theme::CARD);
            match self.thumbnail(c.ui.ctx(), &attachment) {
                Some(texture) => {
                    c.painter.image(texture.id(), c.rect(x + 2.0, y + 4.0, 20.0, 20.0), Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
                }
                None => {
                    c.fill(x + 2.0, y + 4.0, 20.0, 20.0, 4.0, Color32::from_gray(46));
                    let (label, color) = if attachment.kind == chat::AttachmentKind::Image { ("图", theme::SOFT_GREEN) } else { ("文", theme::GRAY) };
                    c.centered_text(x + 12.0, y + 14.0, label, 9.0, W::Semibold, color, H::Center);
                }
            }
            c.text(x + 25.0, y + 12.0, &c.fit(&attachment.name(), 7.5, W::Regular, 32.0), 7.5, W::Regular, Color32::from_gray(225), H::Left);
            c.text(x + 25.0, y + 23.0, attachment.kind.label(), 7.0, W::Regular, theme::DIM, H::Left);
            let close = c.hit(x + 47.0, y + 1.0, 12.0, 12.0, ("chat-detach", index)).on_hover_text(format!("移除 {}", attachment.path.display()));
            c.text(x + 53.0, y + 9.0, "×", 9.0, W::Regular, if close.hovered() { theme::WHITE } else { theme::GRAY }, H::Center);
            if close.clicked() { remove = Some(index); }
        }
        if let Some(index) = remove {
            self.chat.attachments.remove(index);
            self.chat.thumbs.clear();
        }
        let on = self.chat.config.read_attachments;
        let toggle = c.hit(356.0, 155.0, 22.0, 22.0, "chat-read-attachments")
            .on_hover_text(if on { "发送时会读取附件内容（图片 Base64、文本内联）；点击改为只发送路径" } else { "只发送路径；点击允许岛仔读取图片与文本内容" });
        c.circle(367.0, 166.0, 10.0, if on { Color32::from_rgb(38, 84, 138) } else { Color32::from_gray(52) });
        c.centered_text(367.0, 166.0, "读", 8.5, W::Semibold, if on { theme::WHITE } else { theme::GRAY }, H::Center);
        if toggle.clicked() {
            self.chat.config.read_attachments = !on;
            self.chat.config.save();
            self.chat.status = if self.chat.config.read_attachments { "发送时会读取附件内容".into() } else { "改为只发送附件路径".into() };
            self.chat.status_error = false;
        }
    }

    fn thumbnail(&mut self, ctx: &egui::Context, attachment: &chat::Attachment) -> Option<egui::TextureHandle> {
        if attachment.kind != chat::AttachmentKind::Image { return None; }
        if let Some(cached) = self.chat.thumbs.get(&attachment.path) { return cached.clone(); }
        let texture = load_thumbnail(ctx, &attachment.path);
        self.chat.thumbs.insert(attachment.path.clone(), texture.clone());
        texture
    }

    /// 加附件：只接受真实文件，按路径去重，最多 5 个。
    pub fn chat_attach(&mut self, files: Vec<std::path::PathBuf>) {
        for path in files {
            if self.chat.attachments.len() >= chat::MAX_ATTACHMENTS { break; }
            if !path.is_file() { continue; }
            if self.chat.attachments.iter().any(|attachment| attachment.path == path) { continue; }
            self.chat.attachments.push(chat::Attachment::of(path));
        }
        if !self.chat.attachments.is_empty() {
            self.chat.status = format!("{} 个附件 · {}", self.chat.attachments.len(), if self.chat.config.read_attachments { "发送时读取内容" } else { "只发送路径" });
            self.chat.status_error = false;
        }
    }

    /// 整段输入都是存在的本机路径时，转成附件而不是发出去。
    fn attach_paths_from_text(&mut self, text: &str) -> bool {
        let paths = paths_in(text);
        if paths.is_empty() { return false; }
        self.chat_attach(paths);
        self.chat.draft.clear();
        self.chat.input_focused = true;
        true
    }

    fn draw_chat_history(&mut self, c: &Card) {
        c.text(36.0, 44.0, "会话只保存在本次运行的内存里", 9.0, W::Regular, theme::DIM, H::Left);
        c.text(383.0, 44.0, &format!("{} 个", self.chat.conversations.len()), 9.0, W::Regular, theme::DIM, H::Right);
        let rows: Vec<(usize, String, usize)> = self.chat.conversations.iter().enumerate().rev()
            .map(|(index, conversation)| (index, if conversation.title.is_empty() { "新对话".into() } else { conversation.title.clone() }, conversation.messages.len()))
            .take(5).collect();
        for (row, (index, title, count)) in rows.iter().enumerate() {
            let y = 52.0 + row as f32 * 26.0;
            let selected = *index == self.chat.index();
            if selected { c.fill(36.0, y, 347.0, 24.0, 6.0, theme::BTN); }
            let hit = c.hit(36.0, y, 347.0, 24.0, ("chat-conv", *index));
            c.text(46.0, y + 16.0, &c.fit(title, 11.0, W::Regular, 250.0), 11.0, W::Regular, theme::WHITE, H::Left);
            c.text(374.0, y + 16.0, &format!("{count} 条"), 9.0, W::Regular, theme::GRAY, H::Right);
            if hit.clicked() && !self.chat.is_streaming() {
                self.chat.active = *index;
                self.chat.show_history = false;
                self.chat.status.clear();
                self.chat.status_error = false;
            }
        }
        let clear = c.hit(36.0, 196.0, 96.0, 26.0, "chat-clear-all");
        c.text(84.0, 213.0, "清空全部", 9.5, W::Regular, if clear.hovered() { theme::RED } else { theme::GRAY }, H::Center);
        if clear.clicked() && !self.chat.is_streaming() {
            self.chat.conversations = vec![chat::Conversation::default()];
            self.chat.active = 0;
            self.chat.show_history = false;
            self.chat.status.clear();
            self.chat.status_error = false;
        }
        let back = c.hit(300.0, 196.0, 74.0, 26.0, "chat-history-back");
        c.text(337.0, 213.0, "返回对话", 9.5, W::Regular, if back.hovered() { theme::WHITE } else { theme::GRAY }, H::Center);
        if back.clicked() { self.chat.show_history = false; }
    }

    fn chat_status(&self) -> (String, bool) {
        if self.chat.status_error && !self.chat.status.is_empty() { return (self.chat.status.clone(), true); }
        if let Some(streaming) = &self.chat.streaming {
            let elapsed = (self.t - streaming.started).max(0.0);
            return (if streaming.reasoning { format!("正在思考 · {elapsed:.1}s") } else { format!("正在回复 · {} 字 · {elapsed:.1}s", streaming.chars) }, false);
        }
        if !self.chat.status.is_empty() { return (self.chat.status.clone(), false); }
        let model = self.chat.config.model.trim();
        (if model.is_empty() { self.chat.config.preset.label().to_string() } else { format!("{} · {model}", self.chat.config.preset.label()) }, false)
    }

    pub(crate) fn chat_new(&mut self) {
        if self.chat.is_streaming() { self.chat.status = "先停止正在生成的回答".into(); self.chat.status_error = false; return; }
        if self.chat.conversation().is_empty() {
            self.chat.show_history = false;
            self.chat.draft.clear();
            return;
        }
        self.chat.conversations.push(chat::Conversation::default());
        if self.chat.conversations.len() > 12 { self.chat.conversations.remove(0); }
        self.chat.active = self.chat.conversations.len() - 1;
        self.chat.show_history = false;
        self.chat.reasoning_open = false;
        self.chat.draft.clear();
        self.chat.status.clear();
        self.chat.status_error = false;
    }

    pub(crate) fn chat_send(&mut self, cmds: &mut Vec<Cmd>) {
        if self.chat.is_streaming() { return; }
        let text = self.chat.draft.trim().to_string();
        if text.is_empty() && self.chat.attachments.is_empty() { return; }
        if self.chat.config.slot.is_none() {
            self.chat.status = "还没有保存 API Key：请到 设置 → AI 对话 里连接".into();
            self.chat.status_error = true;
            return;
        }
        let attachments = std::mem::take(&mut self.chat.attachments);
        let body_text = if text.is_empty() { "请看看这些附件。".to_string() } else { text.clone() };
        let body_text = format!("{}{}", body_text, chat::path_summary(&attachments));
        let conversation = self.chat.conversation_mut();
        if conversation.title.is_empty() { conversation.title = chat::Conversation::title_of(if text.is_empty() { "附件" } else { text.as_str() }); }
        conversation.messages.push(Message::user_with(body_text, attachments));
        conversation.messages.push(Message::assistant());
        let messages = conversation.messages.clone();
        let request = chat::ChatRequest {
            config: self.chat.config.clone(),
            messages,
            read_attachments: self.chat.config.read_attachments,
        };
        self.chat.draft.clear();
        self.chat.thumbs.clear();
        self.chat.status.clear();
        self.chat.status_error = false;
        self.chat.reasoning_open = false;
        self.chat.streaming = Some(Streaming { started: self.t, chars: 0, reasoning: false });
        cmds.push(Cmd::ChatSend(request));
    }

    pub fn chat_event(&mut self, event: chat::Event) {
        match event {
            chat::Event::Delta { reasoning, text } => {
                if self.chat.streaming.is_none() { return; }
                if let Some(streaming) = self.chat.streaming.as_mut() {
                    if reasoning { streaming.reasoning = true; }
                    streaming.chars += text.chars().count();
                }
                let index = self.chat.index();
                if let Some(message) = self.chat.conversations[index].messages.last_mut().filter(|message| message.role == Role::Assistant) {
                    if reasoning { message.reasoning.push_str(&text); } else { message.text.push_str(&text); }
                }
            }
            chat::Event::Done { ms } => {
                let chars = self.chat.streaming.as_ref().map_or(0, |streaming| streaming.chars);
                self.chat_finish(Outcome::Ok);
                self.chat.streaming = None;
                self.chat.status = format!("用时 {:.1}s · {chars} 字", ms as f64 / 1000.0);
                self.chat.status_error = false;
                self.pet.react(crate::pet::PetState::Happy);
                if !self.open {
                    let index = self.chat.index();
                    let text = self.chat.conversations[index].messages.last().map(|message| message.display().chars().take(40).collect::<String>()).unwrap_or_default();
                    self.alert("AI 回复已就绪", text, CHAT_PAGE, false);
                }
            }
            chat::Event::Stopped => {
                self.chat_finish(Outcome::Stopped);
                self.chat.streaming = None;
                self.chat.status = "已停止生成".into();
                self.chat.status_error = false;
            }
            chat::Event::Error(error) => {
                self.chat_finish(Outcome::Failed(error.clone()));
                self.chat.streaming = None;
                self.chat.status = error;
                self.chat.status_error = true;
                self.pet.react(crate::pet::PetState::Worried);
            }
            chat::Event::Test(Ok(ok)) => {
                self.chat.testing = false;
                self.chat.config.slot = Some(ok.slot);
                self.chat.config.save();
                self.chat.key_input.clear();
                self.chat.note = if ok.reply.trim().is_empty() { "连接成功，模型已响应".into() } else { format!("连接成功：{}", ok.reply.trim()) };
                self.chat.status = "AI 服务已连接".into();
                self.chat.status_error = false;
            }
            chat::Event::Test(Err(error)) => {
                self.chat.testing = false;
                self.chat.note = error;
            }
        }
    }

    fn chat_finish(&mut self, outcome: Outcome) {
        let index = self.chat.index();
        if let Some(message) = self.chat.conversations[index].messages.last_mut().filter(|message| message.role == Role::Assistant) {
            message.outcome = outcome;
        }
    }

    /// 真正的文本框：只覆盖输入条内部，位于「点这里聚焦」的感应区之上。
    pub(crate) fn chat_inputs(&mut self, ui: &mut egui::Ui, origin: Pos2, cmds: &mut Vec<Cmd>) {
        let rect = Rect::from_min_size(origin + egui::vec2(68.0, 201.0), egui::vec2(272.0, 19.0));
        let mut draft = std::mem::take(&mut self.chat.draft);
        let hint = if self.chat.config.slot.is_some() { "问点什么，回车发送；可拖入或粘贴路径" } else { "先到设置里填写 API Key" };
        let te = egui::TextEdit::singleline(&mut draft)
            .id(egui::Id::new("chat-input"))
            .frame(false)
            .margin(egui::Margin::ZERO)
            .text_color(theme::WHITE)
            .font(theme::font(11.5, W::Regular))
            .hint_text(egui::RichText::new(hint).color(theme::DIM).font(theme::font(11.0, W::Regular)))
            .desired_width(rect.width());
        let resp = ui.put(rect, te);
        self.chat.draft = draft;
        if self.chat.focus_requested {
            self.chat.focus_requested = false;
            self.chat.input_focused = true;
            resp.request_focus();
            cmds.push(Cmd::Focus);
        }
        if (resp.clicked() || resp.gained_focus()) && !self.chat.input_focused {
            self.chat.input_focused = true;
            cmds.push(Cmd::Focus);
        }
        // 收起时 egui 可能还留着旧焦点；这里以岛上的聚焦状态为准，避免出现「没激活却有光标」。
        if !self.chat.input_focused && resp.has_focus() { resp.surrender_focus(); }
        let lost_window = ui.input(|i| i.events.iter().any(|event| matches!(event, egui::Event::WindowFocused(false))));
        if lost_window { self.chat.input_focused = false; }
        // 粘贴进来的内容如果整段都是本机路径，直接转成附件，不当作消息发出。
        if self.chat.input_focused {
            let pasted: Vec<String> = ui.input(|i| i.events.iter().filter_map(|event| match event {
                egui::Event::Paste(text) => Some(text.clone()),
                _ => None,
            }).collect());
            for text in pasted {
                if self.chat.draft.trim() == text.trim() && self.attach_paths_from_text(&text) {
                    resp.request_focus();
                    break;
                }
            }
        }
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        if enter && self.chat.input_focused {
            let text = self.chat.draft.trim().to_string();
            if !self.attach_paths_from_text(&text) {
                self.chat_send(cmds);
            }
            resp.request_focus();
            self.chat.input_focused = true;
        } else if resp.lost_focus() && !enter {
            self.chat.input_focused = false;
        }
    }

    // ---------------------------------------------------------------- 设置

    pub(crate) fn chat_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("接入 OpenAI 兼容的对话接口。密钥只保存在 Windows 凭据管理器，对话只在内存里。").size(12.0));
        crate::extras::settings_label(ui, "服务商");
        crate::extras::settings_card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for preset in chat::Preset::ALL {
                    let selected = self.chat.config.preset == preset;
                    let button = egui::Button::new(egui::RichText::new(preset.label()).size(12.0))
                        .fill(if selected { egui::Color32::from_rgb(45, 84, 126) } else { egui::Color32::TRANSPARENT })
                        .stroke(egui::Stroke::NONE).corner_radius(6.0);
                    if ui.add_sized([86.0, 26.0], button).clicked() && !selected {
                        self.chat.config.apply_preset(preset);
                        self.chat.config.save();
                        self.chat.note.clear();
                    }
                }
            });
            ui.add_space(4.0);
            ui.label(egui::RichText::new("接口地址（Base URL）").size(11.0));
            ui.add(egui::TextEdit::singleline(&mut self.chat.config.base_url).hint_text("https://api.deepseek.com/v1").desired_width(ui.available_width()));
            ui.add_space(2.0);
            ui.label(egui::RichText::new("模型").size(11.0));
            ui.add(egui::TextEdit::singleline(&mut self.chat.config.model).hint_text("deepseek-chat").desired_width(ui.available_width()));
            if ui.button("保存地址与模型").clicked() {
                match self.chat.config.validate() {
                    Ok(()) => {
                        // 地址或服务商变了，旧槽位里的密钥就不再对应，要求重新测试。
                        let expected = chat::key_slot(&self.chat.config);
                        if self.chat.config.slot.as_deref() != Some(expected.as_str()) { self.chat.config.slot = None; }
                        // 先记下是否有密钥，再保存（保存会写入 slot 字段）。
                        let connected = self.chat.config.slot.is_some();
                        self.chat.config.save();
                        self.chat.note = if connected { "设置已保存".into() } else { "设置已保存；地址变了，请重新测试连接".into() };
                    }
                    Err(error) => self.chat.note = error,
                }
            }
        });
        crate::extras::settings_label(ui, "密钥");
        crate::extras::settings_card(ui, |ui| {
            ui.label(egui::RichText::new(if self.chat.config.slot.is_some() { "已保存密钥（Windows 凭据管理器）" } else { "尚未保存密钥" }).size(11.5));
            ui.add(egui::TextEdit::singleline(&mut self.chat.key_input).password(true).hint_text("sk-…").desired_width(ui.available_width()));
            ui.horizontal(|ui| {
                let can_test = !self.chat.testing && !self.chat.key_input.trim().is_empty();
                if ui.add_enabled(can_test, egui::Button::new(if self.chat.testing { "连接中…" } else { "测试连接并保存" })).clicked() {
                    self.chat.testing = true;
                    self.chat.note.clear();
                    self.pending.push(Cmd::ChatTest(self.chat.config.clone(), self.chat.key_input.trim().to_string()));
                }
                if self.chat.config.slot.is_some() && ui.button("删除密钥").clicked() {
                    if let Some(slot) = self.chat.config.slot.take() {
                        self.chat.config.save();
                        self.chat.note = "已删除保存的密钥".into();
                        self.pending.push(Cmd::ChatForget(slot));
                    }
                }
            });
            if !self.chat.note.is_empty() { ui.label(egui::RichText::new(&self.chat.note).size(11.5)); }
        });
        crate::extras::settings_label(ui, "对话");
        crate::extras::settings_card(ui, |ui| {
            let mut read = self.chat.config.read_attachments;
            if ui.checkbox(&mut read, "允许读取附件内容（图片按 Base64、文本内联）").changed() {
                self.chat.config.read_attachments = read;
                self.chat.config.save();
            }
            ui.label(egui::RichText::new("关闭时只把附件的本机路径告诉模型；开启后每次发送才读取一次，不缓存内容。").size(10.5));
            ui.label(egui::RichText::new("系统提示词").size(11.0));
            ui.add(egui::TextEdit::multiline(&mut self.chat.config.system).desired_rows(3).desired_width(ui.available_width()));
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("随机度").size(11.0));
                ui.add(egui::Slider::new(&mut self.chat.config.temperature, 0.0..=2.0).fixed_decimals(1));
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("最长回答").size(11.0));
                ui.add(egui::DragValue::new(&mut self.chat.config.max_tokens).range(64..=8192).speed(16.0));
            });
            ui.horizontal(|ui| {
                if ui.button("保存设置").clicked() { self.chat.config.save(); self.chat.note = "设置已保存".into(); }
                if ui.button("清空当前对话").clicked() && !self.chat.is_streaming() {
                    let conversation = self.chat.conversation_mut();
                    conversation.messages.clear();
                    conversation.title.clear();
                    self.chat.status.clear();
                    self.chat.status_error = false;
                }
            });
        });
    }
}
