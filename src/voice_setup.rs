//! 语音环境的下载与安装：模型文件、Python 依赖。
//!
//! 设置页里点「下载语音模型」「安装语音依赖」后，真正干活的是这里的后台线程；
//! 界面只负责显示进度与日志。模型和依赖都放在本机，下载源见 [`MODEL_FILES`]。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eframe::egui;

/// 下载清单：相对路径、地址、已知大小（拿不到 Content-Length 时用来显示进度）。
/// 目录名要和 `daozai_voice.py` 的模型发现规则对得上。
pub const MODEL_FILES: [(&str, &str, u64); 4] = [
    (
        "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/model.int8.onnx",
        "https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/model.int8.onnx",
        239_233_841,
    ),
    (
        "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/tokens.txt",
        "https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/tokens.txt",
        315_894,
    ),
    (
        "silero_vad.onnx",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        643_854,
    ),
    (
        "speaker/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
        28_281_164,
    ),
];

/// KWS 模型以 tar.bz2 发布；这里只解出运行时需要的 4 个文件。
/// 国内直连 GitHub release 经常只有几十 KB/s，所以先用 ghproxy 镜像，失败再直连。
const KWS_DIR: &str = "sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20";
const KWS_ARCHIVE: (&str, u64) = (
    "https://ghproxy.net/https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20.tar.bz2",
    32_885_699,
);
const KWS_FILES: [&str; 4] = [
    "encoder-epoch-13-avg-2-chunk-8-left-64.int8.onnx",
    "decoder-epoch-13-avg-2-chunk-8-left-64.onnx",
    "joiner-epoch-13-avg-2-chunk-8-left-64.int8.onnx",
    "tokens.txt",
];

/// 模型总数与总大小，设置页用来提示。
pub fn model_total_bytes() -> u64 {
    MODEL_FILES.iter().map(|(_, _, size)| *size).sum::<u64>() + KWS_ARCHIVE.1
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind { Models, Env }

pub enum Event {
    Log(String),
    Progress { label: String, done: u64, total: u64 },
    Done { kind: Kind, detail: String },
    Failed(String),
    Cancelled,
}

pub struct Service {
    rx: mpsc::Receiver<Event>,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Service {
    pub fn download(ctx: egui::Context, dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let handle = std::thread::Builder::new().name("daozai-voice-models".into()).spawn(move || {
            let result = download_all(&dir, &flag, &tx, &ctx);
            match result {
                Ok(()) => { let _ = tx.send(Event::Done { kind: Kind::Models, detail: dir.display().to_string() }); }
                Err(error) if error == "已取消" => { let _ = tx.send(Event::Cancelled); }
                Err(error) => { let _ = tx.send(Event::Failed(error)); }
            }
            ctx.request_repaint();
        }).ok();
        Self { rx, cancel, handle }
    }

    pub fn install_env(ctx: egui::Context, python: PathBuf, venv: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let handle = std::thread::Builder::new().name("daozai-voice-env".into()).spawn(move || {
            let result = install(&python, &venv, &flag, &tx, &ctx);
            match result {
                Ok(path) => { let _ = tx.send(Event::Done { kind: Kind::Env, detail: path.display().to_string() }); }
                Err(error) if error == "已取消" => { let _ = tx.send(Event::Cancelled); }
                Err(error) => { let _ = tx.send(Event::Failed(error)); }
            }
            ctx.request_repaint();
        }).ok();
        Self { rx, cancel, handle }
    }

    pub fn try_recv(&self) -> Option<Event> { self.rx.try_recv().ok() }
    pub fn cancel(&self) { self.cancel.store(true, Ordering::SeqCst); }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        // 下载线程自己会很快看到取消位；不在这里 join，避免关闭设置时卡住界面。
        let _ = self.handle.take();
    }
}

fn agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .redirects(5);
    let proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"]
        .iter().find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .or_else(crate::quota::system_proxy);
    if let Some(proxy) = proxy.and_then(|value| ureq::Proxy::new(value).ok()) { builder = builder.proxy(proxy); }
    builder.build()
}

fn alternate_source(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("https://hf-mirror.com/") {
        Some(format!("https://huggingface.co/{rest}"))
    } else if let Some(rest) = url.strip_prefix("https://huggingface.co/") {
        Some(format!("https://hf-mirror.com/{rest}"))
    } else if url.starts_with("https://github.com/") {
        Some(format!("https://ghproxy.net/{url}"))
    } else {
        None
    }
}

fn download_all(dir: &Path, cancel: &AtomicBool, tx: &mpsc::Sender<Event>, ctx: &egui::Context) -> Result<(), String> {
    let agent = agent();
    for (index, (relative, url, expected)) in MODEL_FILES.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) { return Err("已取消".into()); }
        let destination = dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        let existing = std::fs::metadata(&destination).map(|meta| meta.len()).unwrap_or(0);
        if existing > 0 && (*expected == 0 || existing == *expected) {
            let _ = tx.send(Event::Log(format!("已存在，跳过：{relative}")));
            continue;
        }
        if existing > 0 {
            // 之前下到一半或文件损坏：删掉重来（进度条按新文件算）。
            let _ = std::fs::remove_file(&destination);
        }
        if let Some(parent) = destination.parent() { std::fs::create_dir_all(parent).map_err(|error| format!("无法创建目录：{error}"))?; }
        let _ = tx.send(Event::Log(format!("下载 {}/{}：{relative}", index + 1, MODEL_FILES.len())));
        let temporary = destination.with_extension("part");
        let mut result = download_one(&agent, url, &temporary, *expected, relative, cancel, tx, ctx);
        if result.is_err() {
            if let Some(alternate) = alternate_source(url) {
                let _ = tx.send(Event::Log(format!("换一个下载源：{alternate}")));
                result = download_one(&agent, &alternate, &temporary, *expected, relative, cancel, tx, ctx);
            }
        }
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        // 目标已存在时（上次中断留下的完整文件）先删掉，再改名。
        let _ = std::fs::remove_file(&destination);
        std::fs::rename(&temporary, &destination).map_err(|error| format!("保存文件失败：{error}"))?;
    }
    if !kws_ready(dir) {
        download_kws(&agent, dir, cancel, tx, ctx)?;
    }
    let _ = tx.send(Event::Log("模型下载完成".into()));
    Ok(())
}

fn kws_ready(dir: &Path) -> bool {
    let target = dir.join(KWS_DIR);
    KWS_FILES.iter().all(|name| target.join(name).is_file())
}

fn download_kws(
    agent: &ureq::Agent, dir: &Path, cancel: &AtomicBool,
    tx: &mpsc::Sender<Event>, ctx: &egui::Context,
) -> Result<(), String> {
    let target = dir.join(KWS_DIR);
    std::fs::create_dir_all(&target).map_err(|error| format!("无法创建目录：{error}"))?;
    let temporary = dir.join(".kws.tar.bz2.part");
    let _ = tx.send(Event::Log("下载 KWS 模型（约 31 MB，只保留 int8/chunk-8 运行时文件）…".into()));
    let mut result = download_one(
        agent, KWS_ARCHIVE.0, &temporary, KWS_ARCHIVE.1,
        "sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20.tar.bz2", cancel, tx, ctx,
    );
    if result.is_err() {
        let alternate = KWS_ARCHIVE.0.strip_prefix("https://ghproxy.net/").unwrap_or(KWS_ARCHIVE.0);
        let _ = tx.send(Event::Log(format!("换一个下载源：{alternate}")));
        result = download_one(
            agent, alternate, &temporary, KWS_ARCHIVE.1,
            "sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20.tar.bz2", cancel, tx, ctx,
        );
    }
    if let Err(error) = result {
        // 保留 .part：下次点「下载」会从断点续传，而不是从 0 开始。
        return Err(error);
    }

    let unpacked = extract_kws(&temporary, &target)?;
    let _ = std::fs::remove_file(&temporary);
    if unpacked < KWS_FILES.len() {
        return Err(format!("KWS 压缩包不完整：只解出 {unpacked}/{} 个文件", KWS_FILES.len()));
    }
    let _ = tx.send(Event::Log("KWS 模型已就绪".into()));
    Ok(())
}

fn extract_kws(archive: &Path, target: &Path) -> Result<usize, String> {
    let file = std::fs::File::open(archive).map_err(|error| format!("打不开下载文件：{error}"))?;
    let decoder = bzip2::read::BzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(|error| format!("读取 KWS 压缩包失败：{error}"))?;
    let mut unpacked = 0usize;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("读取 KWS 压缩包失败：{error}"))?;
        let path = entry.path().map_err(|error| format!("读取 KWS 文件名失败：{error}"))?;
        let Some(name) = path.file_name().and_then(|name| name.to_str()).map(str::to_owned) else { continue; };
        if !KWS_FILES.contains(&name.as_str()) { continue; }
        entry.unpack(target.join(&name)).map_err(|error| format!("解压 {name} 失败：{error}"))?;
        unpacked += 1;
    }
    Ok(unpacked)
}

#[allow(clippy::too_many_arguments)]
fn download_one(
    agent: &ureq::Agent, url: &str, temporary: &Path, expected: u64, label: &str,
    cancel: &AtomicBool, tx: &mpsc::Sender<Event>, ctx: &egui::Context,
) -> Result<(), String> {
    let mut have = std::fs::metadata(temporary).map(|meta| meta.len()).unwrap_or(0);
    if expected > 0 && have == expected {
        let _ = tx.send(Event::Progress { label: label.to_string(), done: have, total: have });
        return Ok(());
    }
    let mut request = agent.get(url);
    if have > 0 { request = request.set("Range", &format!("bytes={have}-")); }
    let response = request.call().map_err(|error| match error {
        ureq::Error::Status(status, _) => format!("下载失败（{status}）：{label}"),
        other => format!("下载失败：{label}（{other}）"),
    })?;
    let resumed = response.status() == 206 && have > 0;
    if !resumed { have = 0; }
    let length = response.header("Content-Length").and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(0);
    let total = if length > 0 { have + length } else { expected };
    let mut reader = BufReader::new(response.into_reader());
    let mut file = std::fs::OpenOptions::new()
        .create(true).write(true).append(resumed).truncate(!resumed)
        .open(temporary).map_err(|error| format!("无法写入文件：{error}"))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut done: u64 = have;
    let mut last = Instant::now();
    let _ = tx.send(Event::Progress { label: label.to_string(), done, total });
    loop {
        if cancel.load(Ordering::SeqCst) { return Err("已取消".into()); }
        let read = reader.read(&mut buffer).map_err(|error| format!("下载中断：{error}"))?;
        if read == 0 { break; }
        file.write_all(&buffer[..read]).map_err(|error| format!("写入失败：{error}"))?;
        done += read as u64;
        if last.elapsed() >= Duration::from_millis(200) {
            last = Instant::now();
            let _ = tx.send(Event::Progress { label: label.to_string(), done, total });
            ctx.request_repaint();
        }
    }
    file.flush().ok();
    if expected > 0 && done != expected {
        return Err(format!("下载不完整：{label}（{done}/{expected} 字节）"));
    }
    let _ = tx.send(Event::Progress { label: label.to_string(), done: total.max(done), total: total.max(done) });
    Ok(())
}

fn install(python: &Path, venv: &Path, cancel: &AtomicBool, tx: &mpsc::Sender<Event>, ctx: &egui::Context) -> Result<PathBuf, String> {
    if !python.is_file() { return Err(format!("找不到 Python：{}", python.display())); }
    let venv_python = venv.join("Scripts").join("python.exe");
    if !venv_python.is_file() {
        let _ = tx.send(Event::Log(format!("创建虚拟环境：{}", venv.display())));
        run(python, &["-m", "venv", &venv.display().to_string()], cancel, tx, ctx)?;
    } else {
        let _ = tx.send(Event::Log("虚拟环境已存在，更新依赖".into()));
    }
    if !venv_python.is_file() { return Err("虚拟环境创建失败：找不到 Scripts\\python.exe".into()); }
    if cancel.load(Ordering::SeqCst) { return Err("已取消".into()); }
    let _ = tx.send(Event::Log("升级 pip…".into()));
    let _ = run(&venv_python, &["-m", "pip", "install", "--upgrade", "pip", "--disable-pip-version-check"], cancel, tx, ctx);
    // 参考贾维斯的顺序：清华 → 阿里云 → PyPI；每个源失败就换下一个。
    const MIRRORS: [&str; 3] = [
        "https://pypi.tuna.tsinghua.edu.cn/simple",
        "https://mirrors.aliyun.com/pypi/simple",
        "https://pypi.org/simple",
    ];
    let mut installed = false;
    let mut last_error = String::new();
    for mirror in MIRRORS {
        if cancel.load(Ordering::SeqCst) { return Err("已取消".into()); }
        let _ = tx.send(Event::Log(format!("安装 sherpa-onnx / numpy / sounddevice（{mirror}）…")));
        let args = ["-m", "pip", "install", "--disable-pip-version-check", "--timeout", "30", "-i", mirror, "sherpa-onnx", "numpy", "sounddevice"];
        match run(&venv_python, &args, cancel, tx, ctx) {
            Ok(()) => { installed = true; break; }
            Err(error) => {
                last_error = error.clone();
                let _ = tx.send(Event::Log(format!("这个源没装成：{error}")));
            }
        }
    }
    if !installed { return Err(format!("依赖安装失败：{last_error}")); }
    let _ = tx.send(Event::Log("语音依赖安装完成".into()));
    Ok(venv_python)
}

/// 跑一条命令，把输出逐行写进日志；返回非零退出码时带最后几行报错。
fn run(program: &Path, args: &[&str], cancel: &AtomicBool, tx: &mpsc::Sender<Event>, ctx: &egui::Context) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(program);
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|error| format!("启动失败：{error}"))?;
    let stderr = child.stderr.take();
    let logger = {
        let tx = tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(stderr) = stderr {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let _ = tx.send(Event::Log(line));
                    ctx.request_repaint();
                }
            }
        })
    };
    let mut tail: Vec<String> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if cancel.load(Ordering::SeqCst) { let _ = child.kill(); break; }
            if line.trim().is_empty() { continue; }
            tail.push(line.clone());
            if tail.len() > 6 { tail.remove(0); }
            let _ = tx.send(Event::Log(line));
            ctx.request_repaint();
        }
    }
    let status = child.wait().map_err(|error| format!("等待进程失败：{error}"))?;
    let _ = logger.join();
    if cancel.load(Ordering::SeqCst) { return Err("已取消".into()); }
    if !status.success() {
        return Err(tail.last().cloned().unwrap_or_else(|| format!("退出码 {:?}", status.code())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_matches_what_the_helper_looks_for() {
        let names: Vec<&str> = MODEL_FILES.iter().map(|(name, _, _)| *name).collect();
        assert!(names.iter().any(|name| name.starts_with("sherpa-onnx-sense-voice") && name.ends_with("model.int8.onnx")));
        assert!(names.iter().any(|name| name.ends_with("tokens.txt")));
        assert!(names.contains(&"silero_vad.onnx"));
        assert!(names.iter().any(|name| name.starts_with("speaker/") && name.contains("campplus")));
        assert!(model_total_bytes() > 200 * 1024 * 1024, "模型总量应包含 SenseVoice 的 200 MB 以上");
        for (name, url, _) in MODEL_FILES {
            assert!(url.starts_with("https://"), "{name} 的下载地址必须是 https");
            assert!(!name.contains('\\'), "清单里统一用 / 分隔");
        }
        assert!(KWS_ARCHIVE.0.starts_with("https://"));
        assert!(KWS_FILES.iter().all(|name| name.ends_with(".onnx") || *name == "tokens.txt"));
    }

    #[test]
    fn kws_archive_extracts_only_runtime_files() {
        let root = std::env::temp_dir().join(format!("daozai-kws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let archive = root.join("kws.tar.bz2");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let encoder = bzip2::write::BzEncoder::new(file, bzip2::Compression::best());
            let mut builder = tar::Builder::new(encoder);
            for name in KWS_FILES.iter().copied().chain(["extra.txt"]) {
                let data = b"dummy";
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, format!("{KWS_DIR}/{name}"), &data[..]).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        let target = root.join("out");
        std::fs::create_dir_all(&target).unwrap();
        let count = extract_kws(&archive, &target).unwrap();
        assert_eq!(count, KWS_FILES.len());
        for name in KWS_FILES {
            assert!(target.join(name).is_file(), "missing {name}");
        }
        assert!(!target.join("extra.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
