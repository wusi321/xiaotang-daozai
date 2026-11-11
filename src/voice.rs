//! 语音唤醒：本地唤醒词 + 声纹 + 识别，识别出的文字自动送进 AI 对话。
//!
//! 重活（麦克风、静音切句、SenseVoice 识别、CAM++ 声纹）交给同目录下的
//! `voice/daozai_voice.py` 子进程，它只用本机的模型，音频不出这台电脑。这里负责：
//! 起停子进程、读它输出的 JSON 事件、把识别结果接到对话页，以及给设置页一份状态。
//!
//! 之所以用子进程而不是在 Rust 里链 sherpa-onnx：语音栈（sherpa-onnx + 模型）
//! 已经是 Python 侧现成的，编进主程序会让安装包多出几百 MB，还得重写整套推理。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;

use crate::app::{App, Cmd, CHAT_PAGE};

/// 别弹黑框（Windows）。
#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x08000000);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

/// 子进程吐出来的事件。
pub(crate) enum Event {
    Ready { device: String, voiceprint: bool, threshold: f32, speaker: bool },
    State(String),
    Level(f32),
    Wake(String),
    Command(String),
    Heard { text: String, note: String },
    Enroll { state: String, index: u32, total: u32, prompt: String, threshold: f32 },
    Warn(String),
    Error(String),
    Exited,
}

/// 设置页和状态行要显示的东西。
#[derive(Clone, Debug)]
pub(crate) struct Status {
    pub starting: bool,
    pub running: bool,
    pub device: String,
    pub state: String,
    pub level: f32,
    pub voiceprint: bool,
    pub speaker: bool,
    pub threshold: f32,
    pub last: String,
    pub message: String,
    pub enroll: Option<(u32, u32, String)>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            starting: false,
            running: false,
            device: String::new(),
            state: "idle".into(),
            level: 0.0,
            voiceprint: false,
            speaker: true,
            threshold: 0.0,
            last: String::new(),
            message: String::new(),
            enroll: None,
        }
    }
}

impl Status {
    /// 设置页那一行灰色小字。
    pub(crate) fn describe(&self) -> String {
        if let Some((index, total, prompt)) = &self.enroll {
            return format!("请读第 {index}/{total} 句：{prompt}");
        }
        if !self.starting && !self.running {
            return if self.message.is_empty() {
                "语音服务未启动".into()
            } else {
                self.message.clone()
            };
        }
        if !self.running {
            return "正在启动语音服务…（第一次加载模型要几秒）".into();
        }
        let state = match self.state.as_str() {
            "awake" => "在听你说",
            _ => "等待唤醒",
        };
        let voice = if !self.speaker {
            "没有声纹模型".to_string()
        } else if self.voiceprint {
            format!("只认你的声音（{:.2}）", self.threshold)
        } else {
            "还没录声纹，谁喊都认".to_string()
        };
        let device = if self.device.is_empty() { "麦克风".into() } else { self.device.clone() };
        let extra = if self.message.is_empty() { String::new() } else { format!(" · {}", self.message) };
        format!("{device} · {state} · {voice} · 音量 {:.0}%{extra}", self.level * 100.0)
    }
}

/// 语音运行环境：手工指定的 Python / 模型目录 / 输入设备；留空时自动检测。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub python: Option<PathBuf>,
    pub models: Option<PathBuf>,
    pub device: Option<i32>,
}

impl Config {
    fn path() -> PathBuf { crate::model::config_dir().join("voice").join("config.json") }

    pub fn load() -> Self {
        let Ok(raw) = std::fs::read(Self::path()) else { return Self::default(); };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw) else { return Self::default(); };
        let path_of = |key: &str| value.get(key).and_then(|v| v.as_str()).filter(|text| !text.trim().is_empty()).map(PathBuf::from);
        Self {
            python: path_of("python"),
            models: path_of("models"),
            device: value.get("device").and_then(|v| v.as_i64()).map(|v| v as i32),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
        let json = serde_json::json!({
            "python": self.python.as_ref().map(|path| path.display().to_string()),
            "models": self.models.as_ref().map(|path| path.display().to_string()),
            "device": self.device,
        });
        if let Ok(raw) = serde_json::to_vec_pretty(&json) { let _ = crate::model::atomic_write(&path, &raw); }
    }

    pub fn resolved_python(&self) -> Option<PathBuf> {
        self.python.clone().filter(|path| path.is_file()).or_else(find_python)
    }

    pub fn resolved_models(&self) -> Option<PathBuf> {
        self.models.clone().filter(|path| path.is_dir()).or_else(find_models)
    }

    /// 一键安装依赖时用的虚拟环境；和 `find_python` 的自动检测路径保持一致。
    pub fn venv_dir() -> PathBuf {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default()
            .join("Daozai").join("voice").join("venv")
    }

    /// 下载模型的默认目录；Python 侧会优先在这里找。
    pub fn default_models_dir() -> PathBuf {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default()
            .join("Daozai").join("voice").join("models")
    }
}

/// 目录里是否有语音需要的三类模型（SenseVoice / silero VAD / CAM++ 声纹）。
pub fn has_models(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false; };
    let (mut sense, mut vad, mut speaker) = (false, false, false);
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if path.is_dir() && name.starts_with("sherpa-onnx-sense-voice") && path.join("model.int8.onnx").is_file() { sense = true; }
        if name == "silero_vad.onnx" { vad = true; }
        if name == "speaker" && std::fs::read_dir(&path).map(|items| items.flatten().any(|item| item.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".onnx"))).unwrap_or(false) {
            speaker = true;
        }
    }
    sense && vad && speaker
}

fn find_models() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DAOZAI_VOICE_MODELS") {
        let path = PathBuf::from(path);
        if path.is_dir() { return Some(path); }
    }
    let mut candidates = vec![Config::default_models_dir()];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Jarvis").join("models"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() { candidates.push(dir.join("voice").join("models")); }
    }
    candidates.push(PathBuf::from(r"D:\dsh-voice-models"));
    candidates.into_iter().find(|path| has_models(path))
}

/// 常驻的语音子进程。
pub(crate) struct Service {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Event>,
}

impl Service {
    pub(crate) fn start(ctx: egui::Context, config: &Config) -> Result<Self, String> {
        let python = config.resolved_python().ok_or_else(|| {
            "找不到 Python：到 设置 → 常规 → 语音 里选择 Python 或安装语音依赖".to_string()
        })?;
        let script = find_script()
            .ok_or_else(|| "找不到 voice/daozai_voice.py（语音脚本没跟程序放在一起）".to_string())?;
        let profile = crate::model::config_dir().join("voice").join("voiceprint.json");

        let mut command = Command::new(&python);
        command
            .arg(&script)
            .arg("--profile")
            .arg(&profile)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUNBUFFERED", "1");
        if std::env::var_os("DAOZAI_VOICE_DEBUG").is_some() {
            command.arg("--debug");
        }
        if let Some(models) = config.resolved_models() {
            command.arg("--models").arg(models);
        } else if let Some(models) = std::env::var_os("DAOZAI_VOICE_MODELS") {
            command.arg("--models").arg(models);
        }
        if let Some(device) = config.device {
            command.arg("--device").arg(device.to_string());
        } else if let Some(device) = std::env::var_os("DAOZAI_VOICE_DEVICE") {
            command.arg("--device").arg(device);
        }
        if std::env::var_os("DAOZAI_VOICE_LOOSE").is_some() {
            command.arg("--loose");
        }
        hide_console(&mut command);

        let mut child = command.spawn().map_err(|error| format!("启动语音子进程失败：{error}"))?;
        let stdout = child.stdout.take().ok_or("语音子进程没有输出")?;
        let stdin = child.stdin.take();
        if let Some(stderr) = child.stderr.take() {
            log_stderr(stderr);
        }

        let (tx, events) = std::sync::mpsc::channel();
        spawn_reader(stdout, tx, ctx);
        Ok(Self { child, stdin, events })
    }

    /// 给子进程发一条命令（enroll / cancel-enroll / reload）。
    pub(crate) fn command(&mut self, text: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{{\"cmd\":\"{text}\"}}");
            let _ = stdin.flush();
        }
    }

    pub(crate) fn drain(&self, out: &mut Vec<Event>) {
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{{\"cmd\":\"quit\"}}");
            let _ = stdin.flush();
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_reader(stdout: std::process::ChildStdout, tx: Sender<Event>, ctx: egui::Context) {
    std::thread::Builder::new()
        .name("daozai-voice".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Some(event) = parse(&line) else { continue };
                let repaint = !matches!(event, Event::Level(_));
                if tx.send(event).is_err() {
                    return;
                }
                if repaint {
                    ctx.request_repaint();
                }
            }
            let _ = tx.send(Event::Exited);
            ctx.request_repaint();
        })
        .ok();
}

fn log_stderr(stderr: std::process::ChildStderr) {
    let path = crate::model::config_dir().join("voice").join("helper.log");
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    std::thread::spawn(move || {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(file) = file.as_mut() {
                let _ = writeln!(file, "{line}");
            }
        }
    });
}

fn text_of(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn float_of(value: &serde_json::Value, key: &str) -> f32 {
    value.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32
}

fn bool_of(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn uint_of(value: &serde_json::Value, key: &str) -> u32 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as u32
}

fn parse(line: &str) -> Option<Event> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None; // 子进程偶尔会混进别的东西，忽略
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = text_of(&value, "t");
    Some(match kind.as_str() {
        "ready" => Event::Ready {
            device: text_of(&value, "device"),
            voiceprint: bool_of(&value, "voiceprint"),
            threshold: float_of(&value, "threshold"),
            speaker: value.get("speaker").map(|v| v.as_bool().unwrap_or(true)).unwrap_or(true),
        },
        "state" => Event::State(text_of(&value, "state")),
        "level" => Event::Level(float_of(&value, "rms")),
        "wake" => Event::Wake(text_of(&value, "text")),
        "command" => Event::Command(text_of(&value, "text")),
        "heard" => Event::Heard { text: text_of(&value, "text"), note: text_of(&value, "note") },
        "enroll" => Event::Enroll {
            state: text_of(&value, "state"),
            index: uint_of(&value, "index"),
            total: uint_of(&value, "total"),
            prompt: text_of(&value, "prompt"),
            threshold: float_of(&value, "threshold"),
        },
        "warn" => Event::Warn(text_of(&value, "message")),
        "error" => Event::Error(text_of(&value, "message")),
        _ => return None,
    })
}

fn find_python() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DAOZAI_VOICE_PYTHON") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut candidates = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        // 真正的运行环境优先用已经装好依赖的 venv；贾维斯自带的 base Python 只作兜底。
        let daozai = local.join("Daozai").join("voice").join("venv");
        candidates.push(daozai.join("Scripts").join("pythonw.exe"));
        candidates.push(daozai.join("Scripts").join("python.exe"));
        let venv = local.join("Jarvis").join("venv").join("Scripts");
        candidates.push(venv.join("pythonw.exe"));
        candidates.push(venv.join("python.exe"));
        let python = local.join("Jarvis").join("python");
        candidates.push(python.join("pythonw.exe"));
        candidates.push(python.join("python.exe"));
    }
    for name in ["pythonw.exe", "python.exe"] {
        if let Ok(path) = which(name) {
            candidates.push(path);
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn which(name: &str) -> Result<PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

fn find_script() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DAOZAI_VOICE_SCRIPT") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    // 程序自带一份脚本，每次启动按需写到数据目录：这样应用内更新（只替换 exe）之后
    // 也能拿到与当前版本匹配的脚本，不依赖安装包是否覆盖了 voice/ 目录。
    if let Some(path) = ensure_embedded_script() {
        return Some(path);
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
            if let Some(up) = dir.parent() {
                roots.push(up.to_path_buf());
            }
        }
    }
    // 装在别处时，脚本也可以放在数据目录里：%APPDATA%\Daozai\voice\
    let data = crate::model::config_dir();
    roots.push(data.join("voice"));
    roots.push(data.clone());
    // 只有调试版才认源码目录；发布版不能把构建机的路径编进 exe（打包脚本会拒绝）
    #[cfg(debug_assertions)]
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    for root in roots {
        for relative in ["voice/daozai_voice.py", "daozai_voice.py", "../voice/daozai_voice.py"] {
            let candidate = root.join(relative);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 把编进 exe 的脚本写到 `%APPDATA%\Daozai\voice\`；内容有变化（升级）时覆盖。
fn ensure_embedded_script() -> Option<PathBuf> {
    const SCRIPT: &str = include_str!("../voice/daozai_voice.py");
    let dir = crate::model::config_dir().join("voice");
    let path = dir.join("daozai_voice.py");
    let stale = std::fs::read_to_string(&path).map(|old| old != SCRIPT).unwrap_or(true);
    if stale {
        let _ = std::fs::create_dir_all(&dir);
        if crate::model::atomic_write(&path, SCRIPT.as_bytes()).is_err() {
            return path.is_file().then_some(path);
        }
    }
    path.is_file().then_some(path)
}

// ---------------------------------------------------------------- 与 App 的接线

impl App {
    /// 每帧调用：按开关起停子进程，把识别出的文字送进对话页。
    pub(crate) fn voice_tick(&mut self, ctx: &egui::Context, cmds: &mut Vec<Cmd>) {
        self.voice_setup_tick(ctx);
        if std::env::var_os("DAOZAI_VOICE_OFF").is_some() {
            return;
        }
        if self.settings.voice_wake {
            if self.voice.is_none() && self.t >= self.voice_retry_at {
                self.voice_status = Status { starting: true, ..Status::default() };
                match Service::start(ctx.clone(), &self.voice_config) {
                    Ok(service) => self.voice = Some(service),
                    Err(error) => {
                        self.voice_status = Status { message: error, ..Status::default() };
                        // 不自动关掉开关：用户可能正在下载模型或安装依赖；5 秒后再试。
                        self.voice_retry_at = self.t + 5.0;
                    }
                }
            }
        } else if self.voice.is_some() {
            self.voice = None; // Drop 里会结束子进程
            self.voice_status = Status::default();
        }

        let mut events = Vec::new();
        if let Some(service) = self.voice.as_ref() {
            service.drain(&mut events);
        }
        for event in events {
            self.voice_event(event, cmds);
        }

        // 上一句还在生成时收到的语音，等它答完再发
        if let Some(text) = self.voice_pending.clone() {
            if !self.chat.is_streaming() {
                self.voice_pending = None;
                self.voice_send(text, cmds);
            }
        }
    }

    fn voice_setup_tick(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(setup) = self.voice_setup.as_ref() {
            while let Some(event) = setup.try_recv() { events.push(event); }
        }
        if events.is_empty() { return; }
        for event in events {
            match event {
                crate::voice_setup::Event::Log(line) => {
                    self.voice_log.push(line);
                    if self.voice_log.len() > 80 { let extra = self.voice_log.len() - 80; self.voice_log.drain(..extra); }
                }
                crate::voice_setup::Event::Progress { label, done, total } => {
                    self.voice_progress = Some((label, done, total));
                }
                crate::voice_setup::Event::Done { kind, detail } => {
                    self.voice_setup = None;
                    self.voice_progress = None;
                    match kind {
                        crate::voice_setup::Kind::Models => {
                            self.voice_config.models = Some(PathBuf::from(&detail));
                            self.voice_config.save();
                            self.voice_log.push(format!("模型已就绪：{detail}"));
                        }
                        crate::voice_setup::Kind::Env => {
                            self.voice_config.python = Some(PathBuf::from(&detail));
                            self.voice_config.save();
                            self.voice_log.push(format!("语音依赖装好了：{detail}"));
                        }
                    }
                    self.voice_restart();
                }
                crate::voice_setup::Event::Failed(error) => {
                    self.voice_setup = None;
                    self.voice_progress = None;
                    self.voice_log.push(format!("失败：{error}"));
                }
                crate::voice_setup::Event::Cancelled => {
                    self.voice_setup = None;
                    self.voice_progress = None;
                    self.voice_log.push("已取消".into());
                }
            }
        }
        ctx.request_repaint();
    }

    /// 用当前配置重新起语音子进程（选择路径、装完依赖后调用）。
    fn voice_restart(&mut self) {
        self.voice = None;
        self.voice_retry_at = 0.0;
        self.voice_status = Status::default();
    }

    pub fn voice_set_python(&mut self, path: PathBuf) {
        self.voice_config.python = Some(path);
        self.voice_config.save();
        self.voice_restart();
    }

    pub fn voice_set_models(&mut self, path: PathBuf) {
        self.voice_config.models = Some(path);
        self.voice_config.save();
        self.voice_restart();
    }

    /// 设置页「下载语音模型」。
    pub(crate) fn voice_download_models(&mut self, ctx: &egui::Context) {
        if self.voice_setup.is_some() { return; }
        if self.voice_models_ready() {
            self.voice_log = vec!["已经找到可用的语音模型，不用重复下载".into()];
            return;
        }
        let dir = self.voice_config.models.clone().filter(|path| path.is_dir())
            .unwrap_or_else(Config::default_models_dir);
        self.voice_log = vec![format!("下载到：{}", dir.display())];
        self.voice_progress = Some((String::new(), 0, crate::voice_setup::model_total_bytes()));
        self.voice_setup = Some(crate::voice_setup::Service::download(ctx.clone(), dir));
    }

    /// 设置页「安装语音依赖」：建 venv 并装 sherpa-onnx / numpy / sounddevice。
    pub(crate) fn voice_install_env(&mut self, ctx: &egui::Context) {
        if self.voice_setup.is_some() { return; }
        let Some(python) = self.voice_config.resolved_python() else {
            self.voice_log = vec!["先选择一个 Python 解释器（需要 Python 3.10+）".into()];
            return;
        };
        let venv = Config::venv_dir();
        self.voice_log = vec![format!("使用 Python：{}", python.display()), format!("安装到：{}", venv.display())];
        self.voice_setup = Some(crate::voice_setup::Service::install_env(ctx.clone(), python, venv));
    }

    pub(crate) fn voice_cancel_setup(&mut self) {
        if let Some(setup) = self.voice_setup.as_ref() { setup.cancel(); }
    }

    /// 设置页显示的两行环境信息：Python 与模型目录。
    pub(crate) fn voice_env_summary(&self) -> (String, String) {
        let python = self.voice_config.resolved_python().map(|path| path.display().to_string())
            .unwrap_or_else(|| "未找到（可安装语音依赖或选择 python.exe）".into());
        let models = self.voice_config.resolved_models().map(|path| path.display().to_string())
            .unwrap_or_else(|| "未找到（可下载约 270 MB 模型）".into());
        (python, models)
    }

    pub(crate) fn voice_models_ready(&self) -> bool {
        self.voice_config.resolved_models().is_some_and(|path| has_models(&path))
    }

    fn voice_event(&mut self, event: Event, cmds: &mut Vec<Cmd>) {
        match event {
            Event::Ready { device, voiceprint, threshold, speaker } => {
                let status = &mut self.voice_status;
                status.starting = false;
                status.running = true;
                // reload 的 ready 不带设备名，别把已经显示出来的麦克风名字清掉。
                if !device.is_empty() { status.device = device; }
                status.voiceprint = voiceprint;
                status.threshold = threshold;
                status.speaker = speaker;
                status.message.clear();
            }
            Event::State(state) => {
                if state == "awake" && self.voice_status.state != "awake" {
                    self.pet.react(crate::pet::PetState::Curious);
                }
                self.voice_status.state = state;
            }
            Event::Level(level) => self.voice_status.level = level,
            Event::Wake(text) => {
                self.voice_status.last = text;
                self.voice_status.message.clear();
                self.pet.react(crate::pet::PetState::Curious);
            }
            Event::Command(text) => {
                self.voice_status.last = text.clone();
                self.voice_status.message.clear();
                self.voice_send(text, cmds);
            }
            Event::Heard { text, note } => {
                if !note.is_empty() {
                    self.voice_status.message = note;
                } else if !text.is_empty() {
                    self.voice_status.message = text;
                }
            }
            Event::Enroll { state, index, total, prompt, threshold } => match state.as_str() {
                "prompt" => {
                    self.voice_status.enroll = Some((index, total, prompt));
                    self.voice_status.message.clear();
                }
                "short" => {
                    self.voice_status.enroll = Some((index, total, prompt));
                    self.voice_status.message = "没听清，再说一次".into();
                }
                "done" => {
                    self.voice_status.enroll = None;
                    self.voice_status.voiceprint = true;
                    self.voice_status.threshold = threshold;
                    self.voice_status.message = format!("声纹已保存（阈值 {threshold:.2}）");
                }
                "cancelled" => {
                    self.voice_status.enroll = None;
                    self.voice_status.message = "已取消录入".into();
                }
                "failed" => {
                    self.voice_status.enroll = None;
                    self.voice_status.message = "录入失败：句子太短或模型不可用".into();
                }
                _ => {}
            },
            Event::Warn(message) | Event::Error(message) => {
                self.voice_status.message = message;
            }
            Event::Exited => {
                self.voice_status.running = false;
                self.voice_status.starting = false;
                if self.voice_status.message.is_empty() {
                    self.voice_status.message = "语音服务已退出".into();
                }
                // 环境类错误（缺 Python/依赖/模型）重试也没用，先关掉开关并保留原因；
                // 其它意外退出隔 5 秒再拉起，避免疯狂重启。
                let fatal = ["缺少", "找不到", "依赖", "模型", "Python"].iter()
                    .any(|word| self.voice_status.message.contains(word));
                if fatal {
                    self.settings.voice_wake = false;
                    self.settings.save();
                } else {
                    self.voice_retry_at = self.t + 5.0;
                }
                self.voice = None;
                // 子进程意外退出（模型缺失、Python 崩了）时把开关关掉：
                // 否则下一帧就会再拉一次，变成疯狂重启。
                if self.settings.voice_wake {
                    self.settings.voice_wake = false;
                    self.settings.save();
                }
            }
        }
    }

    /// 把一句话当成用户输入发出去。
    pub(crate) fn voice_send(&mut self, text: String, cmds: &mut Vec<Cmd>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.chat.is_streaming() {
            self.voice_pending = Some(text);
            return;
        }
        self.chat.draft = text;
        if self.settings.visible_pages.get(CHAT_PAGE).copied().unwrap_or(true) && self.page != CHAT_PAGE {
            self.show_page(CHAT_PAGE);
        }
        self.chat_send(cmds);
        self.pet.react(crate::pet::PetState::Thinking);
    }

    /// 设置页的「录入我的声音」。
    pub(crate) fn voice_enroll(&mut self) {
        if let Some(service) = self.voice.as_mut() {
            service.command("enroll");
            self.voice_status.message = "正在录入：跟着提示读三句话".into();
        } else {
            self.voice_status.message = "先打开语音唤醒再录入".into();
        }
    }

    /// 设置页的「忘掉我的声音」。
    pub(crate) fn voice_forget(&mut self) {
        let path = crate::model::config_dir().join("voice").join("voiceprint.json");
        let _ = std::fs::remove_file(&path);
        if let Some(service) = self.voice.as_mut() {
            service.command("reload");
        }
        self.voice_status.voiceprint = false;
        self.voice_status.threshold = 0.0;
        self.voice_status.enroll = None;
        self.voice_status.message = "声纹已删除：现在谁喊都能唤醒".into();
    }

    pub(crate) fn voice_cancel_enroll(&mut self) {
        if let Some(service) = self.voice.as_mut() {
            service.command("cancel-enroll");
        }
        self.voice_status.enroll = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_events_parse_into_what_the_ui_needs() {
        let ready = parse(r#"{"t":"ready","asr":true,"speaker":true,"voiceprint":false,"device":"阵列麦克风"}"#);
        match ready {
            Some(Event::Ready { device, voiceprint, threshold, speaker }) => {
                assert_eq!(device, "阵列麦克风");
                assert!(!voiceprint);
                assert_eq!(threshold, 0.0);
                assert!(speaker);
            }
            _ => panic!("ready 事件没解析出来"),
        }

        match parse(r#"{"t":"command","text":"帮我把台灯打开"}"#) {
            Some(Event::Command(text)) => assert_eq!(text, "帮我把台灯打开"),
            _ => panic!("command 事件没解析出来"),
        }

        match parse(r#"{"t":"enroll","state":"prompt","index":2,"total":3,"prompt":"岛仔，把客厅的灯打开。"}"#) {
            Some(Event::Enroll { state, index, total, prompt, .. }) => {
                assert_eq!(state, "prompt");
                assert_eq!((index, total), (2, 3));
                assert!(prompt.starts_with("岛仔"));
            }
            _ => panic!("enroll 事件没解析出来"),
        }

        assert!(parse(r#"{"t":"level","rms":0.03}"#).is_some());
        // 子进程混进来的非 JSON 行要被静默丢掉，不能让界面报错
        assert!(parse("ImportError: no module named sherpa_onnx").is_none());
        assert!(parse(r#"{"t":"不认识的类型"}"#).is_none());
    }

    #[test]
    fn status_line_tells_starting_running_and_enrolling_apart() {
        assert!(Status::default().describe().contains("未启动"));

        let started = Status { starting: false, running: true, device: "麦克风".into(), ..Status::default() };
        assert!(started.describe().contains("等待唤醒"));
        assert!(started.describe().contains("谁喊都认"));

        let enrolled = Status { voiceprint: true, threshold: 0.47, ..started };
        assert!(enrolled.describe().contains("只认你的声音"));

        let awoken = Status { state: "awake".into(), ..enrolled };
        assert!(awoken.describe().contains("在听你说"));

        let enrolling = Status {
            enroll: Some((2, 3, "岛仔，把客厅的灯打开。".into())),
            ..Status::default()
        };
        assert!(enrolling.describe().contains("2/3"));
    }

    /// 真的把子进程拉起来，等它报 ready。需要本机有 Python + 模型，平时跳过：
    /// `cargo test --lib voice::tests::spawns_the_helper -- --ignored --nocapture`
    #[test]
    #[ignore = "需要本机 Python 与语音模型"]
    fn spawns_the_helper_and_reads_its_ready_event() {
        let mut service = Service::start(egui::Context::default(), &Config::default()).expect("启动语音子进程");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut ready = false;
        while !ready && std::time::Instant::now() < deadline {
            let mut events = Vec::new();
            service.drain(&mut events);
            for event in events {
                match event {
                    Event::Ready { device, .. } => {
                        println!("ready: {device}");
                        ready = true;
                    }
                    Event::Error(message) => panic!("子进程报错：{message}"),
                    Event::Exited => panic!("子进程还没 ready 就退出了，看 %APPDATA%\\Daozai\\voice\\helper.log"),
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert!(ready, "等了 120 秒也没有等到 ready");
        drop(service); // Drop 里会发 quit 并结束子进程
    }
}
