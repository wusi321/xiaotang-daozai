#!/usr/bin/env python3
"""岛仔语音唤醒（本地）。

听麦克风 → KWS 关键词门控 → 静音检测切句 → 本地识别（SenseVoice）→ 文本/声纹复核。
没有命中 KWS 的句子不会跑 ASR，所以空闲时只花关键词检测的 CPU。
模型和推理都在本机，音频不出这台电脑；只有识别出的文字交给岛仔。

协议：父进程（岛仔）启动本脚本，读 stdin 发命令、读 stdout 收事件，都是一行一个 JSON。

  stdin   {"cmd":"enroll"} / {"cmd":"cancel-enroll"} / {"cmd":"reload"} / {"cmd":"quit"}
  stdout  {"t":"ready","asr":true,"speaker":true,"voiceprint":false,"kws":true,"device":"..."}
          {"t":"state","state":"idle|awake|hearing"}
          {"t":"level","rms":0.012}
          {"t":"wake","text":"你好岛崽","source":"kws"}
          {"t":"command","text":"帮我把台灯打开"}
          {"t":"heard","text":"..."}          只有加了 --debug 才会输出
          {"t":"enroll","state":"prompt|done|short|cancelled","index":1,"total":3,"prompt":"..."}
          {"t":"warn","message":"..."} / {"t":"error","message":"..."}

单独调试：
  python daozai_voice.py --wav 测试.wav     跑一段音频（也可以用 --file 目录批量跑）
  python daozai_voice.py --devices          列出输入设备
  python daozai_voice.py --enroll           命令行里录一次声纹
  python daozai_voice.py --no-kws           关掉 KWS，回到每句都跑识别
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import re
import sys
import threading
import time
import wave
from pathlib import Path

import numpy as np

try:
    import sherpa_onnx
except Exception as exc:  # pragma: no cover - 只在缺依赖时触发
    print(json.dumps({"t": "error", "message": f"缺少 sherpa-onnx：{exc}"}, ensure_ascii=False), flush=True)
    raise SystemExit(3)

SR = 16000
FRAME = 512  # 32 ms：silero 要求的窗口
MIN_SILENCE = 0.35
MIN_SPEECH = 0.2
HISTORY = 15.0   # 保留多少秒原始麦克风音频，用来补 VAD 切掉的句首
PREROLL = 0.5    # 每句话前面多带 0.5 秒：VAD 触发时第一个字常被切掉
MAX_SPEECH = 12.0
KWS_CONFIRM = 3.0   # KWS 命中后等 ASR 确认的秒数
KWS_COOLDOWN = 1.5  # 两次 KWS 命中至少隔这么久

# 默认关键词：新模型支持中文拼音 + 英文 phone；旧 WenetSpeech 模型没有 en.phone，
# 会自动去掉 HELLO 那一行。用户可以在 %APPDATA%\Daozai\voice\keywords.txt 里改。
KWS_KEYWORDS_ZH = [
    "n ǐ h ǎo d ǎo z ǎi @你好岛崽",
    "h āi d ǎo z ǎi @嗨岛崽",
    "h ā l óu d ǎo z ǎi @哈喽岛崽",
]
KWS_KEYWORD_HELLO = "HH AH0 L OW1 d ǎo z ǎi @HELLO岛崽"
KWS_KEYWORD_BARE = "d ǎo z ǎi @岛崽"


# ---------------------------------------------------------------- 唤醒词

_GREET = (
    r"(?:你好|您好|尼好|你号|拟好|你豪|泥好|"
    r"哈喽|哈啰|哈罗|嗨|嘿|喂|海|还|害|开|唉|哎|"
    r"hello|hallo|hel+o|helo|hi|hey|hay|哈\s*l[o0u]+|哈l)"
)
_NAME = r"(?:岛|导|到|倒|道|刀|捣|盗)[\s]?(?:崽|仔|栽|载|宰|再|在|灾|哉|子)"
_FILLER = r"(?:嗯+|啊+|哦+|呃+|额+|那个|然后|就是)*"
WAKE_RE = re.compile(rf"^{_FILLER}[\s,，。.!！?？、:：~～]*({_GREET})[\s,，。.!！?？、:：~～]*({_NAME})", re.I)
# 只说名字（“岛仔，……”）也算唤醒；默认关闭，用 --loose 打开
BARE_RE = re.compile(rf"^{_FILLER}[\s,，。.!！?？、:：~～]*({_NAME})", re.I)
# KWS 已经确认“喊了”，但 ASR 可能把“HELLO”听成“哈ELLO”；这时只要文本里出现名字，
# 就把名字后面的内容当命令。
NAME_RE = re.compile(_NAME, re.I)
# 只有问候的一小句（识别把“你好”和“岛仔”切成了两段时用得上）
GREET_ONLY = re.compile(rf"^{_FILLER}[\s,，。.!！?？、:：~～]*({_GREET})[\s,，。.!！?？、:：~～]*$", re.I)
_PUNCT = re.compile(r"[\s\"'`~!@#$%^&*()_+\-=\[\]{};:,.<>/?\\|，。！？、；：“”‘’（）【】《》…—]+")

STOP_WORDS = re.compile(r"^(?:停|停下|停一下|别说了|不用了|算了|好了|可以了|嗯|啊|哦)[。.!！?？]*$")


def normalize(text: str) -> str:
    return _PUNCT.sub("", (text or "").strip())


def split_wake(text: str, loose: bool = False):
    """返回 (是否唤醒, 唤醒词, 唤醒词之后的命令)。"""
    raw = (text or "").strip()
    m = WAKE_RE.match(raw)
    if m:
        return True, m.group(0).strip(), raw[m.end():].strip(" \t,，。.!！?？、:：~～")
    if loose:
        m = BARE_RE.match(raw)
        if m:
            return True, m.group(0).strip(), raw[m.end():].strip(" \t,，。.!！?？、:：~～")
    return False, "", ""


# ---------------------------------------------------------------- 模型路径

def _env(*names):
    for name in names:
        value = os.environ.get(name)
        if value and value.strip():
            return Path(value.strip())
    return None


def _env_float(name: str, default: float) -> float:
    try:
        return float(os.environ.get(name, default))
    except (TypeError, ValueError):
        return default


def _models_candidates():
    yield _env("DAOZAI_VOICE_MODELS", "JARVIS_VOICE_MODELS")
    local = os.environ.get("LOCALAPPDATA")
    if local:
        yield Path(local) / "Daozai" / "voice" / "models"
        yield Path(local) / "Jarvis" / "models"
    yield Path(__file__).resolve().parent / "models"
    yield Path(r"D:\dsh-voice-models")


def _find_sense_voice(root: Path):
    for pattern in ("sherpa-onnx-sense-voice*", "sense-voice*"):
        for path in sorted(root.glob(pattern)):
            if (path / "model.int8.onnx").is_file():
                return path / "model.int8.onnx", path / "tokens.txt"
            if (path / "model.onnx").is_file():
                return path / "model.onnx", path / "tokens.txt"
    return None, None


def _find_speaker(root: Path):
    for pattern in ("speaker/*.onnx", "*.onnx", "*campplus*.onnx"):
        for path in sorted(root.glob(pattern)):
            if "campplus" in path.name.lower() or path.parent.name == "speaker":
                return path
    return None


def _find_vad(root: Path):
    for name in ("silero_vad.onnx", "silero-vad.onnx"):
        path = root / name
        if path.is_file():
            return path
    return None


def _tokens_have_english(tokens: Path) -> bool:
    try:
        for line in tokens.read_text(encoding="utf-8-sig").splitlines():
            parts = line.split()
            if parts and parts[0] == "HH":
                return True
    except Exception:
        pass
    return False


def _find_kws(root: Path):
    """找一个可用的 KWS 模型目录；优先中英模型、chunk-8、int8。"""
    paths = list(root.glob("sherpa-onnx-kws-*"))
    paths.sort(key=lambda item: (
        not ((item / "en.phone").is_file() or _tokens_have_english(item / "tokens.txt")),
        item.name,
    ))
    for path in paths:
        if not path.is_dir() or not (path / "tokens.txt").is_file():
            continue
        encoders = sorted(path.glob("encoder-*.onnx"), key=lambda item: ("chunk-8" not in item.name, item.name))
        for encoder in encoders:
            name = encoder.name
            if name.endswith(".int8.onnx"):
                stem = name[len("encoder-"):-len(".int8.onnx")]
                decoder = path / f"decoder-{stem}.onnx"
                joiner = path / f"joiner-{stem}.int8.onnx"
            elif name.endswith(".onnx"):
                stem = name[len("encoder-"):-len(".onnx")]
                decoder = path / f"decoder-{stem}.onnx"
                joiner = path / f"joiner-{stem}.onnx"
            else:
                continue
            if not decoder.is_file():
                alternate = path / f"decoder-{stem}.int8.onnx"
                if alternate.is_file():
                    decoder = alternate
            if not joiner.is_file():
                alternate = path / f"joiner-{stem}.onnx"
                if alternate.is_file():
                    joiner = alternate
            if decoder.is_file() and joiner.is_file():
                return {
                    "dir": path,
                    "encoder": encoder,
                    "decoder": decoder,
                    "joiner": joiner,
                    "tokens": path / "tokens.txt",
                    "english": (path / "en.phone").is_file() or _tokens_have_english(path / "tokens.txt"),
                }
    return None


def discover_models(explicit: str | None):
    # 显式目录优先，但仍然搜索后备目录：比如用户把 SenseVoice 放在贾维斯目录，
    # 把 KWS 放在 D:\dsh-voice-models，两边都能找到。
    roots = []
    if explicit:
        roots.append(Path(explicit))
    roots.extend(_models_candidates())
    unique = []
    seen = set()
    for root in roots:
        if root is None:
            continue
        try:
            key = str(root.resolve())
        except Exception:
            key = str(root)
        if key in seen:
            continue
        seen.add(key)
        unique.append(root)

    best = {"root": None, "asr": None, "tokens": None, "speaker": None, "vad": None, "kws": None}
    for root in unique:
        if not root.is_dir():
            continue
        if best["root"] is None:
            best["root"] = root
        if best["asr"] is None:
            best["asr"], best["tokens"] = _find_sense_voice(root)
        if best["speaker"] is None:
            best["speaker"] = _find_speaker(root)
        if best["vad"] is None:
            best["vad"] = _find_vad(root)
        if best["kws"] is None:
            best["kws"] = _find_kws(root)
        if best["asr"] and best["speaker"] and best["vad"] and best["kws"]:
            break
    return best



# ---------------------------------------------------------------- 声纹

ENROLL_LINES = [
    "岛仔，帮我看看今天有什么安排。",
    "岛仔，把客厅的灯打开。",
    "岛仔，明天早上八点提醒我。",
]


def profile_path(explicit: str | None) -> Path:
    if explicit:
        return Path(explicit)
    appdata = os.environ.get("APPDATA")
    base = Path(appdata) if appdata else Path.home() / "AppData" / "Roaming"
    return base / "Daozai" / "voice" / "voiceprint.json"


class Speaker:
    """CAM++ 声纹：只认录过的那个声音。"""

    def __init__(self, model: Path | None):
        self.model = model
        self._ex = None
        self._lock = threading.Lock()
        self.profile = None
        self.threshold = 0.45
        self.path = profile_path(None)

    def available(self) -> bool:
        return bool(self.model and self.model.is_file())

    def _extractor(self):
        if self._ex is None:
            cfg = sherpa_onnx.SpeakerEmbeddingExtractorConfig(
                model=str(self.model), num_threads=1, provider="cpu"
            )
            self._ex = sherpa_onnx.SpeakerEmbeddingExtractor(cfg)
        return self._ex

    def embed(self, samples) -> np.ndarray | None:
        samples = np.asarray(samples, dtype=np.float32)
        if not self.available() or len(samples) < int(0.8 * SR):
            return None
        with self._lock:
            ex = self._extractor()
            stream = ex.create_stream()
            stream.accept_waveform(sample_rate=SR, waveform=samples)
            stream.input_finished()
            if not ex.is_ready(stream):
                return None
            vector = np.asarray(ex.compute(stream), dtype=np.float32)
        norm = float(np.linalg.norm(vector))
        return vector / norm if norm > 0 else None

    def load(self, path: Path):
        self.path = path
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            self.profile = {
                "mean": np.asarray(data["mean"], dtype=np.float32),
                "each": [np.asarray(e, dtype=np.float32) for e in data["each"]],
            }
            # 实测 0.55 对 1~2 秒的短句太严，真人复测常落在 0.45~0.55；
            # 上限压到 0.45（贾维斯也做过同样的 0.40 上限修正）。
            self.threshold = min(float(data.get("threshold", 0.45)), 0.45)
        except Exception:
            self.profile = None

    def ready(self) -> bool:
        return self.available() and self.profile is not None

    def score(self, vector) -> float:
        profile = self.profile
        scores = [float(np.dot(vector, profile["mean"]))]
        scores += [float(np.dot(vector, e)) for e in profile["each"]]
        return max(scores)

    def verify(self, samples) -> bool | None:
        if not self.ready():
            return None
        vector = self.embed(samples)
        if vector is None:
            return None
        return self.score(vector) >= self.threshold

    def finish(self, enrolled):
        if len(enrolled) < 2:
            return None
        mean = np.mean(enrolled, axis=0)
        mean /= max(float(np.linalg.norm(mean)), 1e-9)
        self_scores = []
        for index, vector in enumerate(enrolled):
            rest = np.sum([e for i, e in enumerate(enrolled) if i != index], axis=0)
            norm = float(np.linalg.norm(rest))
            if norm > 0:
                self_scores.append(float(np.dot(vector, rest / norm)))
        low = min(self_scores) if self_scores else 0.5
        threshold = float(np.clip(low - 0.10, 0.30, 0.45))
        self.profile = {"mean": mean, "each": list(enrolled)}
        self.threshold = threshold
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(
            json.dumps(
                {
                    "model": self.model.name if self.model else "",
                    "created": time.strftime("%Y-%m-%d %H:%M"),
                    "threshold": round(threshold, 3),
                    "mean": [round(float(x), 6) for x in mean],
                    "each": [[round(float(x), 6) for x in e] for e in enrolled],
                    "self": [round(x, 3) for x in self_scores],
                },
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )
        return threshold


# ---------------------------------------------------------------- 引擎

def default_keywords(english: bool, loose: bool) -> str:
    lines = list(KWS_KEYWORDS_ZH)
    if english:
        lines.append(KWS_KEYWORD_HELLO)
    if loose:
        lines.append(KWS_KEYWORD_BARE)
    return "\n".join(lines) + "\n"


def sanitize_keywords(keywords: Path, tokens: Path) -> tuple[Path, int]:
    """过滤掉模型 tokens 里不存在的词。

    sherpa-onnx 在 keywords 编码失败时会直接退出进程（C++ 层），try/except 拦不住，
    所以必须在构造 KeywordSpotter 之前把非法行剔掉。顺带处理 Windows 的 UTF-8 BOM。
    """
    token_set = set()
    for line in tokens.read_text(encoding="utf-8-sig").splitlines():
        parts = line.split()
        if parts:
            token_set.add(parts[0])
    valid = []
    for raw in keywords.read_text(encoding="utf-8-sig").splitlines():
        line = raw.strip().lstrip("\ufeff")
        if not line or line.startswith("#"):
            continue
        head = line.split("@", 1)[0]
        items = [item for item in head.split() if item and not item.startswith((":", "#"))]
        if not items:
            continue
        if all(item in token_set for item in items):
            valid.append(line)
    out = keywords.with_suffix(".valid.txt")
    out.write_text("\n".join(valid) + ("\n" if valid else ""), encoding="utf-8")
    return out, len(valid)


class Kws:
    """sherpa-onnx 关键词检测：只回答“可能喊了”，不负责识别文字。"""

    def __init__(self, found: dict, keywords: Path, score: float = 1.0, threshold: float = 0.25):
        self.found = found
        self.spotter = sherpa_onnx.KeywordSpotter(
            tokens=str(found["tokens"]),
            encoder=str(found["encoder"]),
            decoder=str(found["decoder"]),
            joiner=str(found["joiner"]),
            keywords_file=str(keywords),
            num_threads=1,
            keywords_score=float(score),
            keywords_threshold=float(threshold),
            num_trailing_blanks=1,
            provider="cpu",
        )
        self.stream = self.spotter.create_stream()

    def reset(self):
        try:
            self.spotter.reset_stream(self.stream)
        except Exception:
            pass

    def feed(self, frame) -> str | None:
        self.stream.accept_waveform(SR, np.asarray(frame, dtype=np.float32))
        hit = None
        while self.spotter.is_ready(self.stream):
            self.spotter.decode_stream(self.stream)
            result = self.spotter.get_result(self.stream)
            if result:
                hit = result
                self.spotter.reset_stream(self.stream)
        return hit


class Engine:
    def __init__(self, args):
        self.args = args
        self.emit_lock = threading.Lock()
        self.models = discover_models(args.models)
        if getattr(args, "kws_model", None):
            found = _find_kws(Path(args.kws_model))
            if found:
                self.models["kws"] = found
        self.speaker_path = Path(args.speaker_model) if args.speaker_model else self.models["speaker"]
        self.speaker = Speaker(self.speaker_path if self.speaker_path else None)
        self.speaker.load(profile_path(args.profile))
        self.follow = float(args.follow)
        self.loose = bool(args.loose)
        self.debug = bool(args.debug)
        self.state = "idle"
        self.awake_until = 0.0
        self.greet_at = 0.0
        self.asr = None
        self.vad = None
        self.kws = None
        self.kws_error = ""
        self.kws_pending = False
        self.kws_hit_at = 0.0
        self.kws_last_hit = 0.0
        self.kws_keyword = ""
        self.kws_announced = False
        self.kws_retro = None
        self.kws_retro_at = 0.0
        self.kws_retro_queue = []
        self.enroll = None
        self.pending_commands = queue.Queue()
        self.last_level = 0.0
        self.silent_since = None
        self.preroll = float(getattr(args, "preroll", PREROLL))
        self.history = np.zeros(0, np.float32)
        self.history_start = 0


    # -- 输出 ---------------------------------------------------------
    def emit(self, payload: dict):
        line = json.dumps(payload, ensure_ascii=False)
        with self.emit_lock:
            try:
                sys.stdout.write(line + "\n")
                sys.stdout.flush()
            except Exception:
                pass

    def set_state(self, state: str):
        if state != self.state:
            self.state = state
            self.emit({"t": "state", "state": state})

    # -- 模型 ---------------------------------------------------------
    def load_models(self):
        if not self.models["asr"]:
            self.emit({"t": "error", "message": "找不到 SenseVoice 识别模型（sherpa-onnx-sense-voice-*）"})
            raise SystemExit(4)
        self.asr = sherpa_onnx.OfflineRecognizer.from_sense_voice(
            model=str(self.models["asr"]),
            tokens=str(self.models["tokens"]),
            num_threads=self.args.threads,
            use_itn=True,
            language="auto",
        )
        if self.models["vad"]:
            cfg = sherpa_onnx.VadModelConfig()
            cfg.silero_vad.model = str(self.models["vad"])
            cfg.silero_vad.threshold = 0.5
            cfg.silero_vad.min_silence_duration = MIN_SILENCE
            cfg.silero_vad.min_speech_duration = MIN_SPEECH
            cfg.silero_vad.max_speech_duration = MAX_SPEECH
            cfg.silero_vad.window_size = FRAME
            cfg.sample_rate = SR
            self.vad = sherpa_onnx.VoiceActivityDetector(cfg, buffer_size_in_seconds=60)
        else:
            self.emit({"t": "error", "message": "找不到 silero_vad.onnx，无法切句"})
            raise SystemExit(4)
        self.load_kws()

    def keywords_path(self, found: dict) -> Path:
        explicit = getattr(self.args, "kws_keywords", None)
        if explicit:
            return Path(explicit)
        base = profile_path(self.args.profile).parent
        base.mkdir(parents=True, exist_ok=True)
        loose = bool(self.args.loose)
        path = base / ("keywords-loose.txt" if loose else "keywords.txt")
        if not path.is_file():
            path.write_text(default_keywords(bool(found.get("english")), loose), encoding="utf-8")
        return path

    def load_kws(self):
        if not getattr(self.args, "kws", True):
            return
        found = self.models.get("kws")
        if not found:
            self.kws_error = "没有找到 KWS 模型，已回退到识别唤醒（可在设置里重新下载语音模型）"
            return
        try:
            keywords = self.keywords_path(found)
            keywords, count = sanitize_keywords(keywords, found["tokens"])
            if count == 0:
                raise ValueError("keywords 里没有模型认识的词")
            self.kws = Kws(
                found,
                keywords,
                score=getattr(self.args, "kws_score", 1.0),
                threshold=getattr(self.args, "kws_threshold", 0.25),
            )
        except Exception as exc:
            self.kws = None
            self.kws_error = f"KWS 加载失败：{exc}；已回退到识别唤醒"

    def transcribe(self, samples) -> str:
        stream = self.asr.create_stream()
        stream.accept_waveform(SR, np.asarray(samples, dtype=np.float32))
        self.asr.decode_stream(stream)
        text = stream.result.text or ""
        return re.sub(r"<\|[^|]*\|>", "", text).strip()

    # -- KWS ---------------------------------------------------------
    def on_kws(self, keyword: str):
        now = time.time()
        if self.kws is None or self.enroll is not None or self.kws_pending:
            return
        if self.state == "awake" and now <= self.awake_until:
            return  # 跟随窗口里已经在听，不再重复唤醒
        if now - self.kws_last_hit < KWS_COOLDOWN:
            return
        self.kws_last_hit = now
        self.kws_pending = True
        self.kws_hit_at = now
        self.kws_keyword = keyword
        self.awake_until = now + KWS_CONFIRM
        self.kws_announced = True
        # VAD 可能刚好在 KWS 命中前 10~300 ms 把唤醒词那一句切走并跳过了；
        # 把最近跳过的那段拿回来补一次识别，避免“明明喊了却没确认”。
        if self.kws_retro is not None and now - self.kws_retro_at < 1.5:
            self.kws_retro_queue.append(self.kws_retro)
        self.kws_retro = None
        self.set_state("awake")
        self.emit({"t": "wake", "text": keyword, "source": "kws"})

    def retract_kws(self, why: str = "", quiet: bool = False):
        self.kws_pending = False
        self.kws_hit_at = 0.0
        # 保留 1.5 秒冷却，避免同一段尾音在流重置后立刻二次命中。
        self.kws_last_hit = time.time()
        self.kws_announced = False
        self.greet_at = 0.0
        self.kws_retro = None
        self.kws_retro_queue.clear()
        if self.kws is not None:
            self.kws.reset()
        if self.debug and why and not quiet:
            self.emit({"t": "heard", "text": "", "note": f"KWS 撤回（{why}）"})
        self.set_state("idle")

    # -- 一句话 -------------------------------------------------------
    def handle_segment(self, samples):
        duration = len(samples) / SR
        if duration < 0.35:
            return
        now = time.time()
        in_follow = self.state == "awake" and not self.kws_pending and now <= self.awake_until
        if self.kws is not None and not (self.enroll is not None or in_follow or self.kws_pending):
            # 没命中 KWS：先不跑 ASR，但把这段留着。KWS 的命中可能比 VAD 收尾晚
            # 几十毫秒，命中时再回头补确认这一段。
            if self.enroll is None and duration <= 3.0:
                self.kws_retro = samples.copy()
                self.kws_retro_at = now
            return
        if self.enroll is not None:
            self.handle_enroll(samples)
            return
        text = self.transcribe(samples)
        self.kws_retro = None
        if self.debug:
            self.emit({"t": "heard", "text": text, "sec": round(duration, 2)})
        if not text:
            # 空识别可能只是这一小段是气声/静音；不要立刻撤回，留给下一句确认。
            return
        now = time.time()
        in_follow = self.state == "awake" and not self.kws_pending and now <= self.awake_until
        woke, phrase, command = split_wake(text, loose=self.loose or in_follow)
        if not woke and self.kws_pending:
            # KWS 已经听到完整的唤醒词；ASR 只用来找“名字”和它后面的命令。
            name = NAME_RE.search(text)
            if name:
                woke, phrase = True, self.kws_keyword or name.group(0)
                command = text[name.end():].strip(" \t,，。.!！?？、:：~～")
        if not woke and not in_follow:
            # 识别有时把“你好”和“岛仔”切成两段：记住刚才那句纯问候，下一句只喊名字也认
            if GREET_ONLY.match(text):
                self.greet_at = now
                if self.kws_pending:
                    self.awake_until = now + KWS_CONFIRM
                return
            if self.greet_at and now - self.greet_at < 3.0:
                match = BARE_RE.match(text)
                if match:
                    woke, phrase = True, "你好" + match.group(1)
                    command = text[match.end():].strip(" \t,，。.!！?？、:：~～")
        if woke:
            self.greet_at = 0.0
            self.kws_pending = False
            self.kws_hit_at = 0.0
            self.kws_retro = None
            self.kws_retro_queue.clear()
            # 唤醒词片段通常只有 1 秒左右，CAM++ 对这么短的片段打分偏低，
            # 容易把刚唤醒的会话直接撤回；改为只在有命令要发送时验证声纹。
            # 跟随窗口里每一句命令仍会单独验证。
            if command and not self.voice_ok(samples, "command"):
                self.kws_announced = False
                self.retract_kws("声纹不符", quiet=True)
                return
            self.set_state("awake")
            self.awake_until = now + self.follow
            if not self.kws_announced:
                self.emit({"t": "wake", "text": phrase})
            self.kws_announced = False
            if command:
                self.deliver(command)
            return
        if self.kws_pending:
            # KWS 已经听到关键词，但 ASR 没复述出名字（口音/远场/切句都可能）。
            # 录过声纹时先把跟随窗口打开，让用户接着说：命令仍会逐句过声纹，
            # 不会因为一句识别不准就把整个会话撤掉。没录声纹时保守撤回，
            # 避免电视声被当成命令。
            if self.speaker.ready():
                self.kws_pending = False
                self.kws_announced = False
                self.set_state("awake")
                self.awake_until = now + self.follow
                if self.args.debug:
                    self.emit({"t": "heard", "text": text, "note": "KWS 已命中，ASR 未复述名字；进入跟随，这句不发送"})
            elif self.debug:
                self.emit({"t": "heard", "text": text, "note": "KWS 候选未确认"})
            return
        if not in_follow:
            return
        if not self.voice_ok(samples, "command"):
            return
        command = normalize(text)
        if not command or STOP_WORDS.match(command):
            return
        self.deliver(command)

    def voice_ok(self, samples, why: str) -> bool:
        if not self.speaker.ready():
            return True  # 没录声纹之前谁都能唤醒（和贾维斯一致）
        verdict = self.speaker.verify(samples[: int(6 * SR)])
        if verdict is None:
            return True
        if not verdict:
            # 不再静默丢弃：设置页状态行会显示原因，方便判断是声纹太严还是识别错了。
            self.emit({"t": "warn", "message": f"声纹不符（{why}）：这句话没有发送；可重新录入或忘掉声纹"})
            if self.args.debug:
                self.emit({"t": "heard", "text": "", "note": f"声纹不符（{why}）"})
        return verdict

    def deliver(self, command: str):
        self.awake_until = time.time() + self.follow
        self.set_state("awake")
        self.emit({"t": "command", "text": command})
        # 回来后继续等下一句（跟随窗口），超时自动回 idle

    # -- 声纹录入 -----------------------------------------------------
    def start_enroll(self):
        if not self.speaker.available():
            self.emit({"t": "error", "message": "没有声纹模型（CAM++），无法录入"})
            return
        self.enroll = {"got": [], "index": 0}
        if self.kws is not None:
            self.kws.reset()
        self.kws_pending = False
        self.kws_announced = False
        self.emit({
            "t": "enroll",
            "state": "prompt",
            "index": 1,
            "total": len(ENROLL_LINES),
            "prompt": ENROLL_LINES[0],
        })

    def handle_enroll(self, samples):
        vector = self.speaker.embed(samples)
        if vector is None:
            self.emit({"t": "enroll", "state": "short", "index": self.enroll["index"] + 1,
                       "total": len(ENROLL_LINES), "prompt": ENROLL_LINES[self.enroll["index"]]})
            return
        self.enroll["got"].append(vector)
        self.enroll["index"] += 1
        index = self.enroll["index"]
        if index < len(ENROLL_LINES):
            self.emit({"t": "enroll", "state": "prompt", "index": index + 1,
                       "total": len(ENROLL_LINES), "prompt": ENROLL_LINES[index]})
            return
        threshold = self.speaker.finish(self.enroll["got"])
        self.enroll = None
        if self.kws is not None:
            self.kws.reset()
        if threshold is None:
            self.emit({"t": "enroll", "state": "failed"})
            return
        self.emit({"t": "enroll", "state": "done", "threshold": round(threshold, 2)})
        self.set_state("idle")

    # -- 麦克风 -------------------------------------------------------
    def feed(self, frame):
        frame = np.asarray(frame, dtype=np.float32)
        rms = float(np.sqrt(np.mean(frame ** 2)) + 1e-9)
        now = time.time()
        if now - self.last_level > 0.5:
            self.last_level = now
            self.emit({"t": "level", "rms": round(rms, 4)})
        if rms > 0.004:
            self.silent_since = None
        elif self.silent_since is None:
            self.silent_since = now
        elif now - self.silent_since > 15 and not self.args.quiet_mic_warning:
            self.silent_since = now
            self.emit({"t": "warn", "message": "麦克风一直没有声音：检查是否被静音，或换一个输入设备"})
        if self.kws is not None and self.enroll is None:
            try:
                hit = self.kws.feed(frame)
            except Exception as exc:
                self.emit({"t": "error", "message": f"KWS 失败：{exc}；已回退到识别唤醒"})
                self.kws = None
                self.kws_pending = False
                self.kws_announced = False
                self.kws_retro = None
                self.kws_retro_queue.clear()
                hit = None
            if hit:
                self.on_kws(hit)
        if self.kws_retro_queue and self.kws_pending:
            retro = self.kws_retro_queue.pop(0)
            try:
                self.handle_segment(retro)
            except Exception as exc:  # 补确认失败不能拖垮主循环
                self.emit({"t": "error", "message": f"KWS 补确认失败：{exc}"})
        self.vad.accept_waveform(frame)
        # 留一段原始音频：VAD 报出的片段从它自己检测到的起点开始，
        # 常常把句首（「你」）切掉，这里补上 preroll 秒再识别。
        self.history = np.concatenate((self.history, frame))
        if len(self.history) > (HISTORY + 1.0) * SR:
            drop = len(self.history) - int(HISTORY * SR)
            self.history = self.history[drop:]
            self.history_start += drop
        while not self.vad.empty():
            segment = self.vad.front
            start = int(segment.start)
            end = start + len(segment.samples)
            begin = max(self.history_start, start - int(self.preroll * SR))
            lo, hi = begin - self.history_start, end - self.history_start
            if 0 <= lo < hi <= len(self.history):
                samples = self.history[lo:hi].copy()
            else:
                samples = np.asarray(segment.samples, dtype=np.float32)
            self.vad.pop()
            try:
                self.handle_segment(samples)
            except Exception as exc:  # 单句失败不能拖垮整个循环
                self.emit({"t": "error", "message": f"处理失败：{exc}"})
        if self.state == "awake" and time.time() > self.awake_until:
            if self.kws_pending:
                self.retract_kws("超时未确认")
            else:
                self.set_state("idle")

    def commands(self):
        while True:
            try:
                line = sys.stdin.readline()
            except Exception:
                line = ""
            if not line:
                # 父进程消失：安静退出
                self.pending_commands.put({"cmd": "quit"})
                return
            try:
                payload = json.loads(line)
            except Exception:
                payload = {"cmd": line.strip()}
            self.pending_commands.put(payload)

    def pump_commands(self):
        while True:
            try:
                payload = self.pending_commands.get_nowait()
            except queue.Empty:
                return
            cmd = str(payload.get("cmd", "")).lower()
            if cmd == "quit":
                raise SystemExit(0)
            if cmd == "enroll":
                self.start_enroll()
            elif cmd in ("cancel-enroll", "cancel"):
                self.enroll = None
                if self.kws is not None:
                    self.kws.reset()
                self.emit({"t": "enroll", "state": "cancelled"})
            elif cmd == "reload":
                self.speaker.load(profile_path(self.args.profile))
                self.emit({"t": "ready", "voiceprint": self.speaker.ready(),
                           "threshold": round(self.speaker.threshold, 2),
                           "kws": self.kws is not None})
            elif cmd == "follow":
                # 父进程在模型回答期间/回答完成后把跟随窗口续上，这样不用再喊唤醒词。
                try:
                    seconds = float(payload.get("seconds", self.follow))
                except (TypeError, ValueError):
                    seconds = self.follow
                self.awake_until = time.time() + max(0.0, seconds)
                if seconds > 0:
                    self.set_state("awake")
                elif self.state == "awake":
                    self.set_state("idle")

    # -- 主循环 -------------------------------------------------------
    def run_mic(self):
        try:
            import sounddevice as sd
        except Exception as exc:
            self.emit({"t": "error", "message": f"缺少 sounddevice：{exc}（pip install sounddevice）"})
            raise SystemExit(5)

        device = self.args.device
        while True:
            try:
                stream = sd.InputStream(
                    device=device,
                    samplerate=SR,
                    blocksize=FRAME,
                    channels=1,
                    dtype="float32",
                )
                stream.start()
                break
            except Exception as exc:
                self.emit({"t": "error", "message": f"打不开麦克风：{exc}"})
                time.sleep(3)
        info = sd.query_devices(device, "input") if device is not None else sd.query_devices(kind="input")
        self.emit({"t": "ready", "asr": True, "speaker": self.speaker.available(),
                   "voiceprint": self.speaker.ready(),
                   "threshold": round(self.speaker.threshold, 2),
                   "kws": self.kws is not None,
                   "device": str(info.get("name", ""))})
        if self.kws_error:
            self.emit({"t": "warn", "message": self.kws_error})
        while True:
            self.pump_commands()
            data, overflowed = stream.read(FRAME)
            if overflowed:
                continue
            self.feed(data[:, 0])

    def run_wav(self, path: Path):
        self.emit({"t": "ready", "asr": True, "speaker": self.speaker.available(),
                   "voiceprint": self.speaker.ready(), "kws": self.kws is not None,
                   "device": str(path)})
        if self.kws_error:
            self.emit({"t": "warn", "message": self.kws_error})
        with wave.open(str(path), "rb") as handle:
            if handle.getframerate() != SR:
                raise SystemExit(f"{path} 不是 {SR}Hz")
            raw = handle.readframes(handle.getnframes())
        samples = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
        for start in range(0, len(samples) - FRAME, FRAME):
            self.feed(samples[start:start + FRAME])
        # 收尾：把 VAD 里剩下的话放出来
        self.vad.flush()
        while not self.vad.empty():
            segment = self.vad.front
            self.vad.pop()
            self.handle_segment(np.asarray(segment.samples, dtype=np.float32))


def main():
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except Exception:
        pass
    parser = argparse.ArgumentParser(description="岛仔本地语音唤醒")
    parser.add_argument("--models", help="模型根目录（含 sense-voice / speaker / silero_vad.onnx）")
    parser.add_argument("--speaker-model", help="CAM++ 声纹模型路径")
    parser.add_argument("--profile", help="声纹文件路径")
    parser.add_argument("--device", type=int, default=None, help="输入设备编号")
    parser.add_argument("--follow", type=float, default=15.0, help="唤醒后多久内不用再喊唤醒词")
    parser.add_argument("--preroll", type=float, default=PREROLL, help="每句话前补多少秒，避免句首被 VAD 切掉")
    parser.add_argument("--threads", type=int, default=2)
    parser.add_argument("--loose", action="store_true", help="只说“岛仔”也能唤醒")
    parser.add_argument("--kws", dest="kws", action="store_true",
                        default=os.environ.get("DAOZAI_VOICE_KWS", "1").strip() != "0",
                        help="用 sherpa-onnx KWS 做门控（默认开）")
    parser.add_argument("--no-kws", dest="kws", action="store_false",
                        help="关掉 KWS，回到每句都跑识别的兼容模式")
    parser.add_argument("--kws-model", help="KWS 模型目录（默认自动查找）")
    parser.add_argument("--kws-keywords", help="keywords.txt 路径")
    parser.add_argument("--kws-score", type=float,
                        default=_env_float("DAOZAI_VOICE_KWS_SCORE", 1.0),
                        help="KWS keywords_score")
    parser.add_argument("--kws-threshold", type=float,
                        default=_env_float("DAOZAI_VOICE_KWS_THRESHOLD", 0.25),
                        help="KWS keywords_threshold")
    parser.add_argument("--debug", action="store_true")
    parser.add_argument("--quiet-mic-warning", action="store_true")
    parser.add_argument("--wav", help="用一段 16k 单声道 wav 代替麦克风（调试用）")
    parser.add_argument("--enroll", action="store_true", help="命令行里录声纹")
    parser.add_argument("--devices", action="store_true", help="列出输入设备")
    args = parser.parse_args()

    if args.devices:
        import sounddevice as sd
        for index, dev in enumerate(sd.query_devices()):
            if dev["max_input_channels"] > 0:
                print(f"[{index}] {dev['name']}  ({sd.query_hostapis(dev['hostapi'])['name']})")
        return

    engine = Engine(args)
    engine.load_models()

    if args.enroll:
        raise SystemExit("请在岛仔的设置里录入声纹（图形界面会提示你读三句话）")

    if args.wav:
        engine.run_wav(Path(args.wav))
        return

    threading.Thread(target=engine.commands, name="stdin", daemon=True).start()
    engine.run_mic()


if __name__ == "__main__":
    main()
