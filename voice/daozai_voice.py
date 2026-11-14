#!/usr/bin/env python3
"""岛仔语音唤醒（本地）。

听麦克风 → 静音检测切句 → 本地识别（SenseVoice）→ 唤醒词/声纹判断 → 输出文字。
模型和推理都在本机，音频不出这台电脑；只有识别出的文字交给岛仔。

协议：父进程（岛仔）启动本脚本，读 stdin 发命令、读 stdout 收事件，都是一行一个 JSON。

  stdin   {"cmd":"enroll"} / {"cmd":"cancel-enroll"} / {"cmd":"reload"} / {"cmd":"quit"}
  stdout  {"t":"ready","asr":true,"speaker":true,"voiceprint":false,"device":"..."}
          {"t":"state","state":"idle|awake|hearing"}
          {"t":"level","rms":0.012}
          {"t":"wake","text":"你好岛仔"}
          {"t":"command","text":"帮我把台灯打开"}
          {"t":"heard","text":"..."}          只有加了 --debug 才会输出
          {"t":"enroll","state":"prompt|done|short|cancelled","index":1,"total":3,"prompt":"..."}
          {"t":"error","message":"..."}

单独调试：
  python daozai_voice.py --wav 测试.wav     跑一段音频（也可以用 --file 目录批量跑）
  python daozai_voice.py --devices          列出输入设备
  python daozai_voice.py --enroll           命令行里录一次声纹
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


def discover_models(explicit: str | None):
    roots = [Path(explicit)] if explicit else list(_models_candidates())
    best = {"root": None, "asr": None, "tokens": None, "speaker": None, "vad": None}
    for root in roots:
        if root is None or not root.is_dir():
            continue
        if best["root"] is None:
            best["root"] = root
        if best["asr"] is None:
            best["asr"], best["tokens"] = _find_sense_voice(root)
        if best["speaker"] is None:
            best["speaker"] = _find_speaker(root)
        if best["vad"] is None:
            best["vad"] = _find_vad(root)
        if best["asr"] and best["speaker"] and best["vad"]:
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
            self.threshold = float(data.get("threshold", 0.45))
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
        threshold = float(np.clip(low - 0.10, 0.30, 0.55))
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

class Engine:
    def __init__(self, args):
        self.args = args
        self.emit_lock = threading.Lock()
        self.models = discover_models(args.models)
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

    def transcribe(self, samples) -> str:
        stream = self.asr.create_stream()
        stream.accept_waveform(SR, np.asarray(samples, dtype=np.float32))
        self.asr.decode_stream(stream)
        text = stream.result.text or ""
        return re.sub(r"<\|[^|]*\|>", "", text).strip()

    # -- 一句话 -------------------------------------------------------
    def handle_segment(self, samples):
        duration = len(samples) / SR
        if duration < 0.35:
            return
        if self.enroll is not None:
            self.handle_enroll(samples)
            return
        text = self.transcribe(samples)
        if self.debug:
            self.emit({"t": "heard", "text": text, "sec": round(duration, 2)})
        if not text:
            return
        now = time.time()
        in_follow = self.state == "awake" and now <= self.awake_until
        woke, phrase, command = split_wake(text, loose=self.loose or in_follow)
        if not woke and not in_follow:
            # 识别有时把“你好”和“岛仔”切成两段：记住刚才那句纯问候，下一句只喊名字也认
            if GREET_ONLY.match(text):
                self.greet_at = now
                return
            if self.greet_at and now - self.greet_at < 3.0:
                match = BARE_RE.match(text)
                if match:
                    woke, phrase = True, "你好" + match.group(1)
                    command = text[match.end():].strip(" \t,，。.!！?？、:：~～")
        if woke:
            self.greet_at = 0.0
            if not self.voice_ok(samples, "wake"):
                if self.args.debug:
                    self.emit({"t": "heard", "text": text, "note": "声纹不符"})
                return
            self.set_state("awake")
            self.awake_until = now + self.follow
            self.emit({"t": "wake", "text": phrase})
            if command:
                self.deliver(command)
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
        if not verdict and self.args.debug:
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
                self.emit({"t": "enroll", "state": "cancelled"})
            elif cmd == "reload":
                self.speaker.load(profile_path(self.args.profile))
                self.emit({"t": "ready", "voiceprint": self.speaker.ready(),
                           "threshold": round(self.speaker.threshold, 2)})
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
                   "device": str(info.get("name", ""))})
        while True:
            self.pump_commands()
            data, overflowed = stream.read(FRAME)
            if overflowed:
                continue
            self.feed(data[:, 0])

    def run_wav(self, path: Path):
        self.emit({"t": "ready", "asr": True, "speaker": self.speaker.available(),
                   "voiceprint": self.speaker.ready(), "device": str(path)})
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
    parser.add_argument("--follow", type=float, default=8.0, help="唤醒后多久内不用再喊唤醒词")
    parser.add_argument("--preroll", type=float, default=PREROLL, help="每句话前补多少秒，避免句首被 VAD 切掉")
    parser.add_argument("--threads", type=int, default=2)
    parser.add_argument("--loose", action="store_true", help="只说“岛仔”也能唤醒")
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
