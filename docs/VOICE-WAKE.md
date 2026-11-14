# 语音唤醒

喊一句「你好岛崽」就开始说话，说完自动发给模型，回答显示在对话页。唤醒词、声纹、
语音识别全部在本机完成，录音不出这台电脑。

## 它是怎么跑的

```
麦克风 16 kHz
   │
   ├─ sherpa-onnx KWS 每 80 ms 喂一帧（「你好岛崽 / 嗨岛崽 / 哈喽岛崽 / HELLO 岛崽」）
   │     └─ 没命中：不跑识别，只花关键词检测的 CPU
   │
   ├─ 命中后开 3 s 确认窗口：只对这一句跑 silero VAD + SenseVoice
   │
   ├─ 文本复核：句首「你好 / 嗨 / 哈喽 / hello / hi …」+「岛崽」
   │     （含「岛仔 / 导仔 / 刀仔」这类同音误识；KWS 命中时名字后面就是命令）
   │     └─ 唤醒后 8 s 内的话不再需要唤醒词（跟随窗口）
   │
   ├─ 声纹判断：CAM++ 192 维向量与录入的声纹比对，低于阈值的声音直接丢掉
   │
   └─ {"t":"command","text":"…"} → 岛仔填进对话输入框并发送
```

默认是**两级**：KWS 声学模型只负责「可能喊了」，SenseVoice 的文本 + 正则 + 声纹负责
「喊的是什么、是不是你」。这样唤醒词仍然可以改（改 `keywords.txt`，用拼音/phone 写），
但不再需要每句话都跑 SenseVoice。设置页可以切回「识别唤醒」兼容模式，回到以前的行为。

逐帧处理、状态机（`idle / awake`）、KWS 补确认、跟随窗口、声纹录入的细节都在
`voice/daozai_voice.py`；Rust 侧 `src/voice.rs` 只负责起停子进程、读 JSON 事件、
把文字接到对话页。

### 唤醒后的样子

喊中唤醒词后不会立刻展开整块面板，而是先变成一条 **300×44 的长条胶囊**：

1. 胶囊左边是麦克风，中间显示「在听你说…」和当前识别到的文字，右边跟着麦克风电平跳动；
2. 识别完变成「正在想…」，后台请求模型，岛面保持胶囊；
3. 回答完成（或失败）后自动展开到 AI 对话页，直接看到内容；点胶囊也能提前展开；
4. 唤醒后一直没说话，或等不到命令，胶囊会自己收回。

### 连续追问

跟随窗口默认 8 秒，从**命令发出**时开始算。模型回答经常超过 8 秒，等回答完窗口早就过期了，
这就是「回答完再说话没反应」的原因。现在 Rust 侧会通过 `{"cmd":"follow","seconds":N}` 续期：

- 回答期间每 20 秒续一次，每次 120 秒，长回答也不会中途过期；
- 回答完成、停止或失败后保留 30 秒，读完回答直接接着说即可，不用再喊唤醒词；
- 窗口过期后回到 idle，再说话就要重新喊「你好岛崽」。

### 唤醒率

VAD 报出的句子起点常常晚几十毫秒，第一声「你」容易被切掉。脚本现在保留最近 15 秒原始
音频，每句话前多带 **0.5 秒**（`--preroll`）再送识别，这是目前对唤醒率最有效的一处；
如果还是经常叫不醒，可以试：

- 把 `--preroll` 调到 0.8（编辑 `%APPDATA%\Daozai\voice\daozai_voice.py` 或加启动参数）；
- 用 `DAOZAI_VOICE_DEBUG=1` 看识别文本，确认是「没听见」还是「听见了但没匹配上」；
- 换更近的麦克风或降低环境噪声；
- KWS 已经默认开启；如果某个说法总叫不醒，把它按拼音加进
  `%APPDATA%\Daozai\voice\keywords.txt`（见文末）。

## 进程与协议

岛仔在勾选「语音唤醒」后启动一个隐藏窗口的 Python 子进程，父进程退出或取消勾选时它会
被结束。两边用一行一个 JSON 通信：

| 方向 | 内容 |
| --- | --- |
| 岛仔 → 子进程 | `{"cmd":"enroll"}` / `{"cmd":"cancel-enroll"}` / `{"cmd":"reload"}` / `{"cmd":"quit"}` / `{"cmd":"follow","seconds":N}` |
| 子进程 → 岛仔 | `ready`（带 `kws` 布尔值）/ `state` / `level` / `wake` / `command` / `enroll` / `warn` / `error` |

子进程的 stderr 写到 `%APPDATA%\Daozai\voice\helper.log`，出问题时先看它。

## 需要的东西

| 东西 | 说明 |
| --- | --- |
| Python 3.10+ | 带 `sherpa-onnx`、`numpy`、`sounddevice`。装过贾维斯的话直接用它的 `venv` |
| SenseVoice 模型 | `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-*`（约 230 MB） |
| KWS 模型 | `sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20`（int8 运行时约 5.5 MB；下载包约 31 MB） |
| silero VAD | `silero_vad.onnx`（约 0.6 MB） |
| CAM++ 声纹 | `3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx`（约 27 MB） |

脚本本身编在 exe 里，启动时按需写到 `%APPDATA%\Daozai\voice\daozai_voice.py` 并优先使用它，
所以应用内更新（只替换 exe）之后也能拿到与当前版本匹配的脚本。安装目录里的
`voice\daozai_voice.py` 是同一份副本，方便手动运行和排查；`DAOZAI_VOICE_SCRIPT` 可以覆盖。

模型目录按这个顺序找，找到就用：

1. 环境变量 `DAOZAI_VOICE_MODELS`
2. `%LOCALAPPDATA%\Daozai\voice\models`
3. `%LOCALAPPDATA%\Jarvis\models`（贾维斯的安装目录）
4. 脚本旁边的 `models`（通常是 `%APPDATA%\Daozai\voice\models`）
5. `D:\dsh-voice-models`

即使指定了 `DAOZAI_VOICE_MODELS`，脚本也会继续搜索后面的目录：比如 SenseVoice 在贾维斯
目录、KWS 在 `D:\dsh-voice-models`，两边都能找到。

Python 解释器按 `DAOZAI_VOICE_PYTHON` → 贾维斯的 venv → `pythonw.exe` / `python.exe`
的顺序找。声纹存在 `%APPDATA%\Daozai\voice\voiceprint.json`，删掉它等于没录过。

## 调参

都是环境变量，改完重启岛仔生效：

| 变量 | 作用 |
| --- | --- |
| `DAOZAI_VOICE_PYTHON` | 指定 Python |
| `DAOZAI_VOICE_SCRIPT` | 指定 `daozai_voice.py` |
| `DAOZAI_VOICE_MODELS` | 指定模型目录 |
| `DAOZAI_VOICE_DEVICE` | 输入设备编号（`python daozai_voice.py --devices` 可以列出来） |
| `DAOZAI_VOICE_LOOSE=1` | 只说「岛崽」也能唤醒（默认要求带问候语） |
| `DAOZAI_VOICE_KWS=0` | 关掉 KWS 门控，回到每句都识别的兼容模式（等同于设置里关掉「声学唤醒」） |
| `DAOZAI_VOICE_KWS_THRESHOLD` | KWS 触发阈值，默认 0.25；误唤醒多就调到 0.3~0.4 |
| `DAOZAI_VOICE_KWS_SCORE` | KWS 关键词加权，默认 1.0；一般不用改 |
| `DAOZAI_VOICE_DEBUG=1` | 把每句识别结果也发到界面上，调唤醒词时用 |
| `DAOZAI_VOICE_OFF=1` | 完全关掉语音（排查用） |

### 声学唤醒与关键词

设置 → 语音 → **声学唤醒** 控制是否用 KWS 门控（默认开）。关掉后回到「每句都跑识别」，
唤醒词支持任意说法，但空闲 CPU 会回到 3~8%。

关键词在 `%APPDATA%\Daozai\voice\keywords.txt`（`--loose` 时用 `keywords-loose.txt`），
一行一个，格式是「模型 tokens + `@` 原始词」：

```
n ǐ h ǎo d ǎo z ǎi @你好岛崽
h āi d ǎo z ǎi @嗨岛崽
h ā l óu d ǎo z ǎi @哈喽岛崽
HH AH0 L OW1 d ǎo z ǎi @HELLO岛崽
```

- 改词就是用模型 tokens 的拼音/phone 重新拼一行；旧 WenetSpeech 模型没有 `en.phone`，
  不能写 `HH ...`，脚本会自动跳过不认识的行。
- 脚本启动时会把合法行写到同目录的 `keywords.valid.txt` 再用；如果所有行都不合法，
  会打印警告并回退到识别唤醒。
- 默认不要放裸 `岛崽`；实测「导仔这个名字…」会误触发。确实想只说名字，用
  `DAOZAI_VOICE_LOOSE=1` 或加 `keywords-loose.txt`。
- 单独试跑时可以调 `--kws-threshold`（默认 0.25，误唤醒多就调到 0.3~0.4）和
  `--kws-score`（默认 1.0）；设置页暂不暴露这两个数。

跟随窗口、静音时长等常量在 `voice/daozai_voice.py` 顶部（`MIN_SILENCE`、`MAX_SPEECH`、
`ENROLL_LINES`），命令行参数 `--follow` 改跟随窗口。

## 单独试跑

不启动岛仔也能验证麦克风和识别：

```bash
set DAOZAI_VOICE_PYTHON=%LOCALAPPDATA%\Jarvis\venv\Scripts\python.exe
%DAOZAI_VOICE_PYTHON% voice\daozai_voice.py --debug
```

对着麦克风说「你好岛崽，今天天气怎么样」，应该先看到 `{"t":"wake","source":"kws"}` 再看到
`{"t":"command"}`。想用现成音频测，加 `--wav 某段16k单声道.wav`；想对比旧行为，加
`--no-kws`。

## 常见问题

**状态行一直显示「麦克风一直没有声音」**：麦克风被静音、被别的程序独占，或者 Windows
隐私设置里没给桌面程序麦克风权限（设置 → 隐私 → 麦克风 → 允许桌面应用）。用
`python daozai_voice.py --devices` 换个设备编号试试。

**喊了没反应**：先开 `DAOZAI_VOICE_DEBUG=1`，看有没有 `wake` 和 `heard`。如果连
`wake` 都没有，是 KWS 没听到：确认 `keywords.txt` 里有这个说法、模型目录里有 KWS，
或者临时切到「识别唤醒」对比。如果 `wake` 有了但没 `command`，是文本复核没过：
中文「岛崽」常被识别成「岛仔 / 导仔 / 刀仔」，脚本里的正则都收了；别的写法往 `_NAME` 里加。

**KWS 总误唤醒**：先把裸「岛崽」从 `keywords.txt` 去掉；再把 `--kws-threshold` 从
0.25 调到 0.3~0.4；最后确认声纹已经录入。

**总是被别人的声音唤醒**：还没录入声纹，或者阈值太松。重新录一次（三句话尽量用平常的
音量和距离），或把 `voiceprint.json` 里的 `threshold` 调高 0.05。

**录不进去**：每句话至少要说 1 秒以上，太短会被判为听不清；状态行会提示重说。

## 环境配置（参考贾维斯）

设置页里的「下载语音模型 / 安装语音依赖」沿用了贾维斯 `app/install.py` 的思路：

- **Python 顺序**：`DAOZAI_VOICE_PYTHON` → 岛仔自己的 venv（`%LOCALAPPDATA%\Daozai\voice\venv`）→
  贾维斯 venv（`%LOCALAPPDATA%\Jarvis\venv`，装过贾维斯就直接用）→ 贾维斯自带 Python
  （`%LOCALAPPDATA%\Jarvis\python`）→ PATH。
- **依赖源顺序**：清华 → 阿里云 → PyPI，任一成功即停。
- **模型**：优先复用 `%LOCALAPPDATA%\Jarvis\models`；没有时才下载到
  `%LOCALAPPDATA%\Daozai\voice\models`，先 hf-mirror、失败换 huggingface，支持 `.part` 断点续传。
  KWS 模型从 ghproxy 镜像下 31 MB 的 tar.bz2（失败再直连 GitHub），只解出 int8/chunk-8
  的运行时文件（约 5.5 MB）。
- 选择和下载结果保存在 `%APPDATA%\Daozai\voice\config.json`，改完会自动重启语音子进程。

## KWS 门控（已实现）

2026-11 已实现：默认先用 sherpa-onnx KWS 听关键词，命中后才跑 SenseVoice；没命中不跑
识别。完整的调研、路线对比和自训方案见 [KWS-PLAN.md](KWS-PLAN.md)。

- 模型：`sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20`（中英混合，chunk 8 延迟约
  160 ms，int8 运行时约 5.5 MB），支持「你好岛崽 / 嗨岛崽 / 哈喽岛崽 / HELLO 岛崽」。
  旧 WenetSpeech 3.3M 模型只有中文拼音，不能写 `HH ...` 这种英文 phone。
- 每 80 ms 喂帧，命中立刻发 `wake`；VAD 如果刚好在命中前把这一句切走，脚本会把最近
  跳过的一段拿回来补一次识别（`KWS 补确认`）。
- 命中后仍用文本正则 + 声纹复核；KWS 只负责「可能喊了」。ASR 只在命中窗口/跟随窗口里跑。
- 本机实测：KWS 连续跑约 3.5% 单核（16 线程机器上约 0.2% 总 CPU），唤醒延迟
  160~320 ms；比每句都跑 SenseVoice 省得多。
- 如果 KWS 在你的环境仍不够，再考虑自训 BC-ResNet/TCN；openWakeWord 官方只支持英文，
  不建议直接用于中文。
