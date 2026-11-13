# 语音唤醒

喊一句「你好岛崽」就开始说话，说完自动发给模型，回答显示在对话页。唤醒词、声纹、
语音识别全部在本机完成，录音不出这台电脑。

## 它是怎么跑的

```
麦克风 16 kHz
   │
   ├─ silero VAD 切句（静音 0.35 s 收尾，一句最长 12 s）
   │
   ├─ SenseVoice 识别（中英日韩粤，自动语种）
   │
   ├─ 唤醒词判断：句首「你好 / 嗨 / 哈喽 / hello / hi …」+「岛崽」（含「岛仔 / 导仔 / 刀仔」这类同音误识）
   │     └─ 唤醒后 8 s 内的话不再需要唤醒词（跟随窗口）
   │
   ├─ 声纹判断：CAM++ 192 维向量与录入的声纹比对，低于阈值的声音直接丢掉
   │
   └─ {"t":"command","text":"…"} → 岛仔填进对话输入框并发送
```

唤醒词**不是**单独的声学模型，而是「识别出的文字 + 正则」。这样做的好处是唤醒词可以
随便改（改成「你好小唐」只是改一行正则），也不依赖预先训练好的关键词模型；代价是每句
话都要跑一次识别，空闲时 CPU 常驻约 3~8%（2 线程，看机器）。

逐帧处理、状态机（`idle / awake`）、跟随窗口、声纹录入的细节都在
`voice/daozai_voice.py`；Rust 侧 `src/voice.rs` 只负责起停子进程、读 JSON 事件、
把文字接到对话页。

### 唤醒后的样子

喊中唤醒词后不会立刻展开整块面板，而是先变成一条 **300×44 的长条胶囊**：

1. 胶囊左边是麦克风，中间显示「在听你说…」和当前识别到的文字，右边跟着麦克风电平跳动；
2. 识别完变成「正在想…」，后台请求模型，岛面保持胶囊；
3. 回答完成（或失败）后自动展开到 AI 对话页，直接看到内容；点胶囊也能提前展开；
4. 唤醒后一直没说话，或等不到命令，胶囊会自己收回。

### 唤醒率

VAD 报出的句子起点常常晚几十毫秒，第一声「你」容易被切掉。脚本现在保留最近 15 秒原始
音频，每句话前多带 **0.5 秒**（`--preroll`）再送识别，这是目前对唤醒率最有效的一处；
如果还是经常叫不醒，可以试：

- 把 `--preroll` 调到 0.8（编辑 `%APPDATA%\Daozai\voice\daozai_voice.py` 或加启动参数）；
- 用 `DAOZAI_VOICE_DEBUG=1` 看识别文本，确认是「没听见」还是「听见了但没匹配上」；
- 换更近的麦克风或降低环境噪声；
- 真正要明显提升，还是加一层 KWS 声学唤醒（见文末）。

## 进程与协议

岛仔在勾选「语音唤醒」后启动一个隐藏窗口的 Python 子进程，父进程退出或取消勾选时它会
被结束。两边用一行一个 JSON 通信：

| 方向 | 内容 |
| --- | --- |
| 岛仔 → 子进程 | `{"cmd":"enroll"}` / `{"cmd":"cancel-enroll"}` / `{"cmd":"reload"}` / `{"cmd":"quit"}` |
| 子进程 → 岛仔 | `ready` / `state` / `level` / `wake` / `command` / `enroll` / `warn` / `error` |

子进程的 stderr 写到 `%APPDATA%\Daozai\voice\helper.log`，出问题时先看它。

## 需要的东西

| 东西 | 说明 |
| --- | --- |
| Python 3.10+ | 带 `sherpa-onnx`、`numpy`、`sounddevice`。装过贾维斯的话直接用它的 `venv` |
| SenseVoice 模型 | `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-*`（约 230 MB） |
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
| `DAOZAI_VOICE_DEBUG=1` | 把每句识别结果也发到界面上，调唤醒词时用 |
| `DAOZAI_VOICE_OFF=1` | 完全关掉语音（排查用） |

跟随窗口、静音时长等常量在 `voice/daozai_voice.py` 顶部（`MIN_SILENCE`、`MAX_SPEECH`、
`ENROLL_LINES`），命令行参数 `--follow` 改跟随窗口。

## 单独试跑

不启动岛仔也能验证麦克风和识别：

```bash
set DAOZAI_VOICE_PYTHON=%LOCALAPPDATA%\Jarvis\venv\Scripts\python.exe
%DAOZAI_VOICE_PYTHON% voice\daozai_voice.py --debug
```

对着麦克风说「你好岛崽，今天天气怎么样」，应该先看到 `{"t":"wake"}` 再看到
`{"t":"command"}`。想用现成音频测，加 `--wav 某段16k单声道.wav`。

## 常见问题

**状态行一直显示「麦克风一直没有声音」**：麦克风被静音、被别的程序独占，或者 Windows
隐私设置里没给桌面程序麦克风权限（设置 → 隐私 → 麦克风 → 允许桌面应用）。用
`python daozai_voice.py --devices` 换个设备编号试试。

**喊了没反应**：先开 `DAOZAI_VOICE_DEBUG=1`，看识别出来的到底是什么。中文「岛崽」常被
识别成「岛仔 / 导仔 / 刀仔」，脚本里的正则都收了；如果听到的是别的写法，往 `_NAME` 里加。

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
- 选择和下载结果保存在 `%APPDATA%\Daozai\voice\config.json`，改完会自动重启语音子进程。

## 如果要做 KWS（声学唤醒）

现在每句话都要跑 SenseVoice，空闲 CPU 约 3~8%、唤醒延迟约 1 秒。想更省电、更快，可以加一层
关键词检测，让 ASR 只在命中后才跑：

**推荐：sherpa-onnx KWS + 现有 ASR 复核**

1. 模型用 `sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01`（中文，约 3 MB）或英文版；
   比 SenseVoice 小两个数量级。
2. 关键词写在 `keywords.txt` 里，用模型 tokens 的拼音/音素表示（例如「你好岛崽」拆成
   `n ǐ h ǎo d ǎo z ǎi`，不同版本 token 集略有差异）。
3. 每 80 ms 喂一帧，`is_ready` → `decode_stream` → `get_result`；命中先发 `{"t":"wake"}`，
   再开 3~5 s 的 VAD 窗口收命令，只对这一句跑 ASR。
4. 命中后仍用现在这套文本正则 + 声纹复核，压误唤醒；KWS 只负责「可能喊了」。
5. 调参：`keywords-score`、`keywords-threshold`、`num-trailing-blanks`；用 50 条正样本
   （不同距离/音量）和 50 条负样本（电视、闲聊）测误唤醒与漏唤醒。

**其它路线**

- **openWakeWord 自训练**：像贾维斯的「hey jarvis」，用 TTS 合成 + 真实录音训练；英文生态成熟，
  中文需要自己造数据和训练。
- **Porcupine（Picovoice）**：中文自定义词、接入快、CPU 低；需要 AccessKey，许可证与开源分发要确认。
- **自训 BC-ResNet / zipformer KWS**：效果最好、可控性最强，但要做数据、训练和导出 ONNX，工作量最大。

建议先上 sherpa KWS 做门控，保留 ASR 复核和声纹；模型与关键词都放本机，沿用设置页的下载与配置方式。
