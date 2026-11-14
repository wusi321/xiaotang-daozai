# 唤醒词模型：调研与落地规划

> 调研时间：2026-11-14。机器：Ryzen 7 8845H（8C/16T）、15.3 GB 内存、无 NVIDIA GPU。
> 文中“本机实测”都是在这台机器上跑出来的，不是文档抄来的。
>
> 2026-11-14 更新：方案 A（KWS 门控）的 MVP 已落地，见 3.9；下面的“规划”保留为决策记录。

## 0. 结论先行

1. **现在最划算的不是训练，而是给现有链路加一层 sherpa-onnx KWS 门控。**
   用新出的 `sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20`（中英混合、chunk 8 延迟约
   160 ms、int8 运行时约 5.5 MB），关键词写在文本文件里，不训练、不改模型。
   本机实测：连续跑 60 s 约 3.5% 单核，按 16 逻辑核折算约 **0.2% 总 CPU**；唤醒命中比
   句尾晚约 **0.15~0.35 s**。这基本就是之前说的“空闲 1%、延迟 100~300 ms”。
2. **“训练一个类似贾维斯的模型”对中文不是一件小事。**
   openWakeWord 官方明确只支持英文，主干是冻结的英文语音 embedding，只训练最后的小分类头；
   中文的声调、韵母在这个特征空间里几乎没有被表示。第三方实测里，中文唤醒的真人得分只有
   0.0776，而一次空调瞬态能到 0.19——降阈值只会先放大噪声。它适合英文 `hey jarvis`，
   不适合直接拿来做「你好岛崽」。
3. **如果一定要自训，做中文原生的小模型，不要微调 zipformer KWS。**
   推荐 log-mel + BC-ResNet/小 TCN，从零训练；数据配方是 3k~5k 条中文合成正样本 +
   1~5 万条中文负样本 + 200~500 条你自己的真实录音。MVP 一周能跑通，达到上线标准
   大约 2~4 周。主要成本是造数据和评估，不是梯度计算。
4. **推荐节奏：先上 KWS 门控（2~3 天）→ 用真实数据评估 → 不够再自训。**
   KWS 已经能解决 90% 的 CPU 和延迟问题，而且关键词改一行文本就能换；自训模型是
   “完全拥有模型”的长期选项，不是第一天的必需品。

下面把“为什么”和“怎么做”拆开写。

---

## 1. 现状：CPU 到底花在哪

### 1.1 现在的链路

```
麦克风 16 kHz
  ├─ silero VAD 每 32 ms 喂一帧，静音 0.35 s 切句
  ├─ 每句话都跑 SenseVoice（2 线程，int8，约 230 MB）
  ├─ 文本正则判断唤醒词
  ├─ CAM++ 声纹复核
  └─ command → 对话页
```

问题不在 VAD，也不在 Python 空转，而在**每句都跑 SenseVoice**。环境噪声只要让 VAD
切出一句，就会跑一次完整识别。

### 1.2 本机实测

| 项目 | 实测 | 说明 |
| --- | --- | --- |
| SenseVoice 单句 | 3.79 s 音频约 **0.277 s CPU**（2 线程） | 连续说话约 7% 单核 |
| silero VAD | 60 s 静音约 **0.22 s CPU**（0.37% 单核） | 每 32 ms 一帧 |
| 模型加载后纯空闲 | 15 s 约 **0.00 s CPU** | ONNX Runtime 不空转 |
| KWS 连续跑 | 60 s 静音约 **2.08 s CPU**（3.5% 单核） | chunk 8、int8、1 线程 |
| KWS chunk 16 | 60 s 约 **2.2% 单核** | 延迟约 320 ms |

按 16 逻辑核折算，KWS 的 3.5% 单核约等于任务管理器里的 **0.2%**；如果只看 Python
进程的“单核百分比”，它仍然是 3.5%。这两个口径要分清。

**诊断**：你现在看到的空闲 3~8%，最可能不是“脚本空转”，而是麦克风底噪/电视声让 VAD
偶尔切出一句，然后 SenseVoice 跑了一遍。KWS 门控的价值不是让 Python 少转几圈，而是
**让 ASR 只在命中唤醒词后才跑**——噪声再触发 VAD，也不会跑 SenseVoice。

### 1.3 两个 KWS 模型的实测对比

测试音频来自 `D:\dsh-voice-models\wake-tests`（包含真人/合成音的「你好导仔」「嗨岛仔」
「HELLO 岛仔」以及负样本），16 kHz 单声道，80 ms 帧。

| 模型 | 关键词 | 结果 | RTF（单核） |
| --- | --- | --- | --- |
| `wenetspeech-3.3M-2024-01-01` chunk 16 | 你好岛崽 | 命中「你好导仔/刀仔」；「嗨岛仔」「HELLO 岛仔」漏 | 0.022~0.025 |
| `zh-en-3M-2025-12-20` chunk 16 | 你好/嗨/哈喽/HELLO/岛崽 | 全部命中；但裸「岛崽」会误触发 | 0.020~0.025 |
| `zh-en-3M-2025-12-20` chunk 8 | 同上 | 全部命中；延迟更低 | 0.027~0.037 |

关键发现：

- **新模型值得换**：它同时支持中文拼音和英文 phone，能写
  `HH AH0 L OW1 d ǎo z ǎi @HELLO岛崽`，旧模型只能中文拼音。
- **裸「岛崽」不要默认开**：测试里「导仔这个名字听起来不错」在 0.8 s 就命中了裸关键词。
  默认只放完整短语；`--loose`（只说名字）再单独加。
- **命中点**：以 1.1 s 左右的「你好岛崽」为例，chunk 8 在 1.28 s 命中，chunk 16 在
  1.44 s 命中。也就是句尾后约 **0.15 s / 0.35 s**，加 80 ms 帧长，和官方标的
  160/320 ms 对得上。

---

## 2. 路线对比

| 路线 | 中文 | 要训练？ | 主要工作 | 空闲 CPU | 延迟 | 许可证/成本 | 适合 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **sherpa-onnx KWS 门控** | ✅ 好 | ❌ | 接模型、写关键词、调阈值 | ~0.2% 总 CPU | 160~320 ms | Apache 2.0（模型卡标注） | **先做这个** |
| **自训 BC-ResNet/TCN** | ✅ 原生 | ✅ | 造数据、训练、评估、导出 | <0.5% 单核 | 80~200 ms | 自己控制；数据许可证要查 | 想完全拥有模型 |
| **openWakeWord** | ⚠️ 官方只支持英文 | ✅ | 合成数据 + 负样本 + 训练头 | ~2.6% 单核 | 80 ms 帧 | 代码 Apache 2.0；预训练模型 CC BY-NC-SA 4.0 | 英文 `hey jarvis` |
| **Porcupine** | ✅ 官方普通话音色 | ❌ | 控制台生成 `.ppn` | 很低 | ~32 ms 帧 | 需 AccessKey；商用授权 | 不想碰训练、能接受商业条款 |
| **微调 zipformer KWS** | ✅ | ✅ | icefall + 大量数据 + 导出 | 低 | 160~320 ms | Apache 2.0 | 不建议，投入产出比最低 |
| **继续 ASR 正则** | ✅ 任意说法 | ❌ | 无 | 3~8% | ~1 s | 无 | 兼容性兜底 |

### 2.1 为什么 openWakeWord 不适合直接做中文

openWakeWord 的官方说明写得很清楚：

- “Currently, only English models are supported”；
- 架构是 mel → **冻结的 Google speech_embedding**（英文说话人识别预训练）→ 小分类头；
- 自训只更新最后的小头，前面的 embedding 不动；
- 官方预训练模型是 **CC BY-NC-SA 4.0（非商用）**，代码才是 Apache 2.0。

中文的声调是音位，英文 embedding 没有为它训练过；只换分类头，等于让一个没听过中文的
特征提取器去区分「岛崽」和「导仔」。第三方横评里，中文唤醒的真人得分只有 0.0776，
而一次空调瞬态能到 0.19——降阈值只会先放大噪声（[实测记录](https://juejin.cn/post/7706345830357712931)）。
社区里确实有人用中文 TTS 硬训，但效果不稳定，而且需要自己解决中文 Piper/Edge-TTS
数据、中文负样本和评估。**可以花一天做“可行性实验”，但不要把它当主路线。**

> 参考：[openWakeWord PyPI 说明](https://pypi.org/project/openwakeword/)、
> [ORBIS 的训练指南](https://orbis.protolabs.studio/docs/how-to/train-a-wake-word)
> （英文，免费 Colab 约 1 小时，几千条合成正样本）。

### 2.2 为什么 Porcupine 不是首选

Porcupine 官方支持 **Chinese (Mandarin)**，自定义词在 Console 生成 `.ppn`，推理完全
本地、CPU 很低，接入比训练快得多。但：

- 需要 **AccessKey**，运行时初始化要联网校验/账号；
- 商用和再分发要按 Picovoice 的授权走；2026 年有第三方报道称个人长期免费档已取消，
  以官网为准；
- `.ppn` 是专有格式，不能转 ONNX，也不能自己改模型；
- 对“岛仔”这种开源/MIT 项目，引入商业 SDK 的许可证问题比技术问题更麻烦。

如果只是自己用、能接受账号和条款，它是“今天就要中文唤醒”的最快方案；如果要随应用
分发，优先 KWS 或自训。

### 2.3 为什么不微调 zipformer KWS

`sherpa-onnx` 的 KWS 模型本身是 icefall 训练的小型 zipformer transducer，训练配方在
[icefall PR #1428](https://github.com/k2-fsa/icefall/pull/1428)，WenetSpeech L（约 1 万
小时）级别。它不是“拿几十条录音微调一下”的模型；要微调得先搭 icefall、准备对齐数据、
多卡训练、再导出 ONNX。工作量远大于从零训一个 BC-ResNet，而且收益只是“关键词更像
你的口音”——这件事交给后面的声纹和 ASR 复核更划算。

---

## 3. 推荐方案 A：sherpa-onnx KWS 门控（先做）

### 3.1 模型选择

| 项 | 建议 |
| --- | --- |
| 模型 | `sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20` |
| 变体 | `chunk-8`（延迟 160 ms）优先；CPU 想再省一点用 `chunk-16`（320 ms） |
| 精度 | `encoder`/`joiner` 用 int8，`decoder` 用 fp32（官方建议） |
| 体积 | 运行时 5 个文件约 5.5 MB；完整 tar.bz2 约 31 MB（含 fp32、chunk16、测试音频） |
| 许可证 | k2-fsa 系列模型卡标注 Apache 2.0；新模型发布页没附 README，落地前再核对一次 |
| 回退 | 旧 `wenetspeech-3.3M-2024-01-01` 也能用，但只支持中文、不支持 `HELLO` |

### 3.2 关键词文件

默认只放完整短语（`keywords.txt`）：

```
n ǐ h ǎo d ǎo z ǎi @你好岛崽
h āi d ǎo z ǎi @嗨岛崽
h ā l óu d ǎo z ǎi @哈喽岛崽
HH AH0 L OW1 d ǎo z ǎi @HELLO岛崽
```

`--loose` 模式再追加：

```
d ǎo z ǎi @岛崽
```

注意：

- `@` 后面是原始词，不能有空格；英文短语里的空格用 `_`（例如 `@LIGHT_UP`），中英混写
  不要加空格，例如 `@HELLO岛崽`。
- 新模型用 `phone+ppinyin`，英文 phone 来自 `en.phone`；旧模型只能用 `ppinyin`。
- 生成方式（需要 `click`）：

```bash
sherpa-onnx-cli text2token \
  --tokens tokens.txt \
  --tokens-type phone+ppinyin \
  --lexicon en.phone \
  keywords_raw.txt keywords.txt
```

- 阈值先用默认 `keywords_score=1.0`、`keywords_threshold=0.25`、
  `num_trailing_blanks=1`；有误唤醒再往上调 threshold（0.3~0.4），漏唤醒则往下调
  或加同音写法。

### 3.3 接入后的状态机

目标：**KWS 只负责“可能喊了”，ASR 仍负责“喊的是什么”，声纹仍负责“是不是你”。**

```
idle
  │  KWS 命中（80 ms 帧，160~320 ms）
  ▼
awake（立刻发 wake；3 s 内等 ASR 确认，确认后按 8 s 跟随窗口）
  │  VAD 切出这一句 → 只对这一句跑 SenseVoice
  ├─ 文本正则命中 → 声纹通过 → command
  ├─ 文本正则不命中 → 判定 KWS 误报，回 idle（不发送）
  └─ 3 s 内没有语音 → 回 idle
```

关键点：

1. **KWS 连续跑最简单**，不要只在 VAD 触发后跑。VAD 会把句首「你」切掉；如果为了省
   CPU 改成 VAD 触发，必须把 `--preroll` 那段缓冲也喂给 KWS，否则唤醒词开头会丢。
2. **ASR 只在 `awake` / `kws 确认窗口` / 跟随窗口里跑**。其他 VAD 段直接丢，不转写。
3. **保留滚动缓冲**：KWS 可能在句子中间命中，VAD 段结束时把整段（含 `--preroll`）
   送 ASR，保证命令开头不丢。
4. **KWS 命中立刻发 `wake`**，让胶囊马上出现；如果 ASR 没确认，1~3 s 内发 `idle`
   收回，不发送命令。
5. **声纹复核照旧**，而且建议默认开启录入；KWS 误报 + 别人声音会被声纹挡住。

### 3.4 代码改动（Python 侧）

`voice/daozai_voice.py` 里加一个 `Kws` 小类，`Engine` 持有它：

```python
class Kws:
    def __init__(self, model_dir: Path, keywords: Path, chunk: int = 8):
        self.kws = sherpa_onnx.KeywordSpotter(
            tokens=str(model_dir / "tokens.txt"),
            encoder=str(model_dir / f"encoder-epoch-13-avg-2-chunk-{chunk}-left-64.int8.onnx"),
            decoder=str(model_dir / f"decoder-epoch-13-avg-2-chunk-{chunk}-left-64.onnx"),
            joiner=str(model_dir / f"joiner-epoch-13-avg-2-chunk-{chunk}-left-64.int8.onnx"),
            keywords_file=str(keywords),
            num_threads=1,
            keywords_score=1.0,
            keywords_threshold=0.25,
            num_trailing_blanks=1,
            provider="cpu",
        )
        self.stream = self.kws.create_stream()

    def feed(self, frame) -> str | None:
        self.stream.accept_waveform(SR, frame)
        hit = None
        while self.kws.is_ready(self.stream):
            self.kws.decode_stream(self.stream)
            result = self.kws.get_result(self.stream)
            if result:
                hit = result
                self.kws.reset_stream(self.stream)
        return hit
```

`Engine.feed()` 里：

```python
hit = self.kws.feed(frame) if self.kws else None
if hit and not self.enroll:
    self.kws_pending = True
    self.kws_hit_at = time.time()
    self.kws_keyword = hit
    self.set_state("awake")
    self.awake_until = self.kws_hit_at + 3.0   # 等 ASR 确认
    self.emit({"t": "wake", "text": hit, "source": "kws"})
```

`handle_segment()` 开头加门控：

```python
now = time.time()
in_follow = self.state == "awake" and not self.kws_pending and now <= self.awake_until
if not (self.enroll or in_follow or self.kws_pending or now <= self.kws_hit_at + 3.0):
    return  # 没命中 KWS，不跑 ASR
```

ASR 之后：

```python
woke, phrase, command = split_wake(text, loose=self.loose or in_follow)
if self.kws_pending and not woke:
    self.kws_pending = False
    self.kws_hit_at = 0.0
    self.set_state("idle")      # KWS 误报，撤回
    return
if woke:
    self.kws_pending = False
    self.kws_hit_at = 0.0
    self.awake_until = now + self.follow
    if not self.kws_announced:
        self.emit({"t": "wake", "text": phrase})
        self.kws_announced = True
    # ... 后面的声纹、command 逻辑不变
```

注意：`state == "awake"` 在 KWS 命中后立刻为真，**不能**用它判断“已经确认唤醒”，
否则误报永远撤不回。要单独用 `kws_pending` 区分“KWS 候选”和“ASR 已确认”。
`kws_pending` 在确认/撤回时清掉，`kws_announced` 在回到 idle 时重置。

另外，`transcribe()` 返回空文本时也要撤回，否则 KWS 误报会让胶囊一直挂到 3 s 超时。

### 3.5 代码改动（Rust 侧）

- `Event` 增加 `Wake { text, source }` 或保持原样；`source` 只用于调试，不影响 UI。
- `Status::describe()` 增加“声学唤醒 / 识别唤醒”的状态文案。
- 设置页增加“唤醒引擎”二选一：
  - **声学唤醒（省电）**：KWS，默认；
  - **识别唤醒（兼容）**：现在的 ASR 正则，作为兜底。
- `has_models()` 仍只要求 SenseVoice + VAD + speaker；KWS 单独用
  `has_kws()` 判断，缺 KWS 时自动回退，不阻塞启动。
- `voice_setup.rs` 的 `MODEL_FILES` 增加 KWS 文件；如果只下 tar.bz2，需要加
  `bzip2` 解压，或者把 5 个运行时文件单独托管到项目 release（约 5.5 MB，推荐）。

### 3.6 下载与安装

- 推荐把 **5 个运行时文件**（encoder int8 / decoder / joiner int8 / tokens / keywords）
  放到项目自己的 GitHub release 或 CDN，单文件下载约 5.5 MB，现有下载器不用改架构。
- 如果直接用官方 tar.bz2（31 MB），需要：
  - 加 `bzip2` crate 解压到 `sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20/`；
  - 或者下载后调用系统 `tar -xjf`（Windows 11 自带 bsdtar，但不保证所有机器都有）。
- 本次实现走的是 tar.bz2 + `bzip2`/`tar` 解压、只保留 4 个运行时文件（见 3.9）；以后
  想减小下载量，再把它们单独托管成 5.5 MB 的单文件清单。
- 模型目录发现规则和现有模型一致，放在 `%LOCALAPPDATA%\Daozai\voice\models` 下即可。
- 设置页文案：模型总量从约 270 MB 变成约 275 MB（单文件方案）或 300 MB（tar 方案）。

### 3.7 验收指标

| 指标 | 目标 |
| --- | --- |
| 唤醒率 | 「你好岛崽」≥ 95%（1 m 内，安静） |
| 误唤醒 | 连续 2 h 电视/闲聊 < 1 次（配合声纹后） |
| 唤醒延迟 | KWS 命中到胶囊出现 < 400 ms（chunk 8） |
| 空闲 CPU | 60 s 静音 < 1% 总 CPU（16 线程机器） |
| 回归 | 现有 `voice-tests`、`cargo test` 全过；跟随窗口、声纹、录入不退化 |

### 3.8 工期

- 模型/keywords/探针：0.5 天（已经跑通）；
- Python 门控 + 状态机：0.5~1 天；
- Rust 事件/设置/下载：0.5~1 天；
- 评估 + 调阈值 + 文档：0.5~1 天。

**合计 2~3 天**，比自训快一个数量级。

### 3.9 实现状态（2026-11-14）

- `voice/daozai_voice.py`：KWS 加载/自动发现/关键词清洗/连续门控/`kws_pending` 状态机/
  VAD 抢先切句的补确认/`--kws`、`--no-kws`、`--kws-keywords`、`--kws-threshold` 参数；
  KWS 缺失或关键词非法时自动回退到识别唤醒，并输出 `warn`。
- `src/model.rs`：新增 `voice_kws` 设置（默认开，持久化）。
- `src/voice.rs`：用 `DAOZAI_VOICE_KWS=0` 关掉门控（兼容旧脚本）、`ready.kws` 解析、
  状态行显示「声学唤醒 / 识别唤醒」、`has_kws` / `find_kws` / `voice_kws_ready`。
- `src/extras.rs`：设置页「声学唤醒」开关与模型状态提示。
- `src/voice_setup.rs`：下载 31 MB tar.bz2、bzip2 解压、只保留 int8/chunk-8 运行时文件，
  优先 ghproxy 镜像，失败再直连 GitHub。
- 已用 `wake-tests`（你好/嗨/HELLO/无唤醒/近音词）验证；下一步是用真实麦克风跑
  50 正 + 50 负 + 2 h 连续负样本，再决定阈值。

---

## 4. 方案 B：自训一个中文唤醒模型（可选）

如果 KWS 在你的真实环境里不够（比如电视声误唤醒多、口音漏唤醒多），再走这条。
目标不是“再做一个 sherpa”，而是“用你自己的数据，做一个更贴合你麦克风、你的口音、
你的房间的小模型”。

### 4.1 不要从 openWakeWord 开始

一句话：**它的特征提取器是英文的，且冻结。** 你可以花一天做个小实验：

1. 用中文 TTS 生成 1k 条「你好岛崽」，加 5k 条中文负样本；
2. 用 openWakeWord 的 mel + embedding 提特征，只训分类头；
3. 在 50 条真实录音上评估。

如果分数集中在 0.0x、ROC 起不来，就停——这不是数据不够，是特征空间不对。
这个实验的价值是“用一天确认此路不通”，不要投两周。

### 4.2 模型结构：log-mel + BC-ResNet / 小 TCN

推荐从零训练，不依赖任何英文预训练：

```
16 kHz 单声道
  → 25 ms 窗 / 10 ms  hop，64 维 log-mel
  → 1.0 s 上下文（101 帧）
  → BC-ResNet-1/2（4 个 broadcast block，通道 16/24/32/48）
  → 全局平均池化 → FC 1 → sigmoid
```

- 参数量：约 5 万~15 万；ONNX int8 约 0.5~1.5 MB。
- 推理：每 80 ms 跑一次 1.0 s 窗口，输出一个分数；连续 2~3 帧过阈值才触发，
  冷却 1.5 s。
- 延迟：约 80~200 ms（比 zipformer KWS 更低）。
- 训练：PyTorch，BCE + label smoothing，AdamW，batch 128，30~50 epoch，
  cosine schedule，SpecAugment（时间/频率掩码）。
- 类别不平衡：正:负 ≈ 1:5~1:10，`pos_weight` 3~5。

为什么不是“再大一点”的模型：唤醒词是常驻任务，模型越小、越省电越好；单短语任务
不需要 3M 参数的 zipformer，BC-ResNet 在 Speech Commands 上已经够用。真正的难点
在数据，不在模型容量。

### 4.3 数据配方

这是整个项目 80% 的工作量。

| 数据 | 数量 | 来源/做法 |
| --- | --- | --- |
| 合成正样本 | 3k~5k | 10~20 个中文 TTS 音色 × 5 种语速/音高；文本覆盖「你好岛崽 / 嗨岛崽 / 哈喽岛崽 / 喂岛崽」；前后留 0.2~0.5 s 静音 |
| 真实正样本 | 200~500 | 你自己 + 家人/朋友，0.5 m / 2 m / 4 m，安静/有电视声，各录 10~20 遍；这是最有价值的数据 |
| 普通负样本 | 20k~50k | 中文播客、TV、会议、有声书、音乐、噪声；1.2~2.0 s 切片 |
| 硬负样本 | 2k~5k | 「你好小唐 / 你好岛风 / 导演 / 导仔 / 刀仔 / 小岛 / 海岛 / 岛崽这个名字」等近音词；从长音频里用初版模型挖高分片段 |
| 增广 | 3~5 倍 | 随机增益、语速 0.85~1.15、RIR 混响、噪声 5~20 dB SNR、高低通、轻微削波 |

数据来源建议：

- 中文语音：AISHELL-1/2/3、THCHS-30、Common Voice zh-CN、MAGICDATA、WenetSpeech
  （每个语料的许可证不同，商用分发前逐个核对）；
- 噪声/音乐：MUSAN、DEMAND、DNS Challenge；
- RIR：OpenSLR RIRs；
- TTS：Piper 中文音色、Edge-TTS 多音色、CosyVoice2 / GPT-SoVITS（想要更自然/
  多音色时）。注意 `vits-piper-zh_CN-huayan-medium` 的 MODEL_CARD 写的是
  **License: Unknown**，自己用没问题，随应用分发要谨慎。

**划分**：按说话人/来源分训练/验证/测试，不能随机切同一段音频的相邻窗口，否则
测试集泄漏，指标会虚高。

### 4.4 训练与算力

- 本机 8845H（8C/16T）+ 15 GB：小模型完全能训；预计算 log-mel 后，瓶颈主要在
  数据生成/增广，不在梯度。一次完整实验预计几小时到一天。
- AMD 780M 核显在 Windows 上没有可用的 PyTorch CUDA/ROCm 训练路径，不要指望它；
  要么 CPU，要么用免费/国内 GPU 平台（Colab、Kaggle、AutoDL、恒源云等）。
- 如果数据在 5k 正 + 20k 负以内，CPU 训练是现实选项；要反复跑几十组实验、或者负样本
  到几万条以上，再用 GPU 省时间。

### 4.5 评估

见第 5 节。自训模型必须和 KWS、现有 ASR 正则在**同一批音频**上对比，否则没有意义。

### 4.6 导出与接入

```python
torch.onnx.export(model, dummy, "wake.onnx", opset_version=17,
                  input_names=["mel"], output_names=["score"],
                  dynamic_axes={"mel": {0: "batch", 1: "time"}})
```

- 用 `onnxruntime` 做动态 int8 量化；
- 用 20 条真实音频对比 PyTorch 和 ONNX 的分数，误差 < 1e-3；
- Python 侧和 KWS 用同一个接口：`score(frame) -> float`，命中逻辑、状态机、
  ASR 复核、声纹全部复用；
- Rust 侧可以继续走 Python 子进程，也可以以后用 `ort`/`sherpa-onnx` 的 Rust
  binding 把 KWS 搬进主进程（[sherpa-onnx Rust KeywordSpotter](https://docs.rs/sherpa-onnx/latest/sherpa_onnx/struct.KeywordSpotter.html)）。

### 4.7 工期

| 阶段 | 时间 | 产出 |
| --- | --- | --- |
| 数据管线 + 1k 条冒烟 | 2~3 天 | 能生成、能训练、能导出 |
| MVP（3k 正 + 10k 负） | 1 周 | 在 50 条真实录音上跑出第一组 FRR/FA |
| 迭代（硬负挖掘、增广、调阈值） | 2~4 周 | 达到 FRR ≤5%、FA ≤0.5/h |
| 接入 + 评估 + 发布 | 3~5 天 | 和 KWS 做 A/B，决定默认用哪个 |

**注意**：这是“第一次跑通”到“上线”的差距。不要用“Colab 一小时”来估算中文自训的
总工期；那一小时只覆盖“英文、有现成 Piper 音色、有现成负样本”的情况。

### 4.8 许可证

- 代码/模型：自己写的部分可以 MIT/Apache；训练数据、TTS 音色、预训练模型各有各的
  许可证。
- openWakeWord 的预训练模型是 **CC BY-NC-SA 4.0**，不能商用分发；如果你只是拿它的
  代码/流程，自己训的模型另算，但训练数据仍要合规。
- Piper 中文音色、Edge-TTS、CosyVoice 的商用条款要逐个确认；`huayan` 的
  MODEL_CARD 明确写 `License: Unknown`，别直接打包进商业安装包。
- sherpa-onnx KWS 模型是 Apache 2.0，这是它相对省心的最大优势。

---

## 5. 评估协议（两种方案共用）

### 5.1 测试集

| 类别 | 数量 | 条件 |
| --- | --- | --- |
| 正样本 | 50 条 | 不同距离（0.5/2/4 m）、不同音量、安静/电视声 |
| 负样本 | 50 条 | 电视、闲聊、音乐、播客；其中 10 条含「岛/崽/导/仔」近音词 |
| 连续负样本 | 2~5 h | 电视/播客/会议，统计误唤醒/小时 |
| 边界样本 | 20 条 | 「你好小唐」「你好岛风」「导演」「岛崽这个名字」「你好岛」（被切掉） |

### 5.2 指标

- **FRR**（漏唤醒率）：正样本中未触发的比例；
- **FA/h**（误唤醒率）：连续负样本中每小时触发次数；
- **延迟**：从唤醒词最后一个音节结束到 `wake` 事件的时间（p50/p95）；
- **CPU**：60 s 静音 + 60 s 连续语音的进程 CPU；
- **模型体积**：ONNX 文件大小。

### 5.3 验收线

| 指标 | 可上线 | 优秀 |
| --- | --- | --- |
| FRR | ≤ 10% | ≤ 5% |
| FA/h | ≤ 2 | ≤ 0.5 |
| 延迟 p95 | ≤ 500 ms | ≤ 300 ms |
| 空闲 CPU | ≤ 1% 总 CPU | ≤ 0.3% |
| 模型 | ≤ 5 MB | ≤ 2 MB |

先用验证集调阈值，再在测试集上只报一次；不要拿测试集反复调参。

---

## 6. 风险与未决问题

1. **KWS 误唤醒**：电视里出现「你好」+ 类似「岛崽」的音，可能命中。缓解：完整短语
   + 声纹 + ASR 正则二次确认；默认不开放裸「岛崽」。
2. **KWS 漏唤醒**：口音、远场、轻声。缓解：加同音关键词、降 threshold、保留
   “识别唤醒”兼容模式；必要时再自训。
3. **ASR 复核的延迟**：KWS 命中后还要等 VAD 切句 + SenseVoice，命令文字会晚 0.5~1 s。
   但胶囊已经出现，用户感知的“唤醒”是快的；如果要求命令也快，只能上流式 ASR，
   那是另一个项目。
4. **KWS 命中但 ASR 没复述出名字**：当前是保守策略——不把这句话当命令发出去，避免
   KWS 误报时把电视声发进对话。若真实使用中这种情况多，可以改成“KWS + 声纹通过就直接
   收命令”，代价是误唤醒会上升；先用 debug 日志统计发生率再决定。
5. **下载体积/网络**：官方 tar.bz2 31 MB；实现已优先 ghproxy 镜像、失败再直连 GitHub，
   下载后只保留 5.5 MB 运行时文件。如果镜像也不可用，用户可以手动放模型或先用兼容模式。
6. **许可证**：KWS Apache 2.0 最省心；Porcupine 商业授权；openWakeWord 预训练模型
   非商用；自训模型的 TTS/语料许可证要逐个查。
7. **“改唤醒词”体验**：KWS 改词要写拼音，普通用户不会。第一版可以只支持内置
   几个短语；以后再做“输入中文 → 自动转拼音”的小工具。
8. **Python 子进程**：KWS 不解决“后台挂着 py 脚本”的观感；如果目标是彻底去 Python，
   需要把 KWS + ASR 都搬到 Rust（sherpa-onnx 有 Rust binding），那是另一个工程量，
   和 CPU 优化不是一回事。

---

## 7. 建议的下一步

> 2026-11-14：第 1 步的 MVP 已经落地（见 3.9）；剩下的第 1 步是真实麦克风评估和调阈值。

按投入产出排序：

1. **先做 KWS 门控（推荐，2~3 天）**：把新模型和 keywords 接进 `daozai_voice.py`，
   ASR 只在命中后跑；设置页加“声学唤醒/识别唤醒”开关；用现有 `wake-tests` +
   2 h 负样本验收。预计空闲 CPU 从 3~8% 降到 1% 以内，唤醒延迟 160~320 ms。
2. **收集真实数据（并行，1~2 周）**：每次唤醒/误唤醒都留 3 s 音频（脱敏、只本地），
   攒 200~500 条「你好岛崽」和 2~5 h 负样本。这些数据无论走哪条路线都用得上。
3. **再决定是否自训（2~4 周）**：如果 KWS 在你的客厅/电视环境下 FA/h 仍然高，或者
   你明确想“模型完全属于自己”，再启动 BC-ResNet/TCN 项目；否则 KWS 就是终点。

如果现在只做一件事：**做第 1 步**。它不会浪费——即使以后自训，KWS 仍然可以当
“第一级门控”或“兼容模式”。

---

## 附录 A：本机实测记录

### A.1 探针脚本

- `D:\dsh-voice-models\kws-probe\probe_kws_zh_en.py`：跑新模型 + 多关键词，输出命中
  时间与 RTF；
- `D:\dsh-voice-models\kws-probe\probe_cpu.py`：分别测 SenseVoice/VAD/KWS 的 CPU；
- 关键词文件：`D:\dsh-voice-models\kws-probe\keywords-zh-en.txt`。

### A.2 关键输出

```
=== chunk=8 score=1.0 thr=0.25 ===
hello_wake   3.73s hits=[{'kw': 'HELLO岛崽', 'at': 1.28}] rtf=0.0335
mention      3.17s hits=[{'kw': '岛崽', 'at': 0.8}]     rtf=0.0315   # 裸关键词误报
no_wake      3.36s hits=[]                              rtf=0.0347
other_voice  3.44s hits=[{'kw': '你好岛崽', 'at': 1.28}] rtf=0.0286
wake_cmd     3.79s hits=[{'kw': '你好岛崽', 'at': 1.28}] rtf=0.0296
wake_hi      1.60s hits=[{'kw': '嗨岛崽', 'at': 1.12}]   rtf=0.0275
wake_only    1.80s hits=[{'kw': '你好岛崽', 'at': 1.28}] rtf=0.0273
```

```
SenseVoice 3.79s: 0.277s CPU/次
VAD 60s 静音:    0.22s CPU
KWS 60s 静音:    2.08s CPU（chunk 8）
```

## 附录 B：参考资料

- [sherpa-onnx Keyword Spotting 文档](https://k2-fsa.github.io/sherpa/onnx/kws/index.html)
  —— keywords 文件格式、`keywords_score` / `keywords_threshold` / `num_trailing_blanks`。
- [sherpa-onnx KWS 预训练模型](https://k2-fsa.github.io/sherpa/onnx/kws/pretrained_models/index.html)
  —— 2025-12-20 中英模型、chunk 8/16 延迟、下载与 `text2token` 用法。
- [openWakeWord（PyPI）](https://pypi.org/project/openwakeword/)
  —— 英文限定、冻结 embedding、合成数据训练、预训练模型 CC BY-NC-SA 4.0。
- [ORBIS：Train your own wake word](https://orbis.protolabs.studio/docs/how-to/train-a-wake-word)
  —— openWakeWord 英文自训的实际流程与“免费 Colab 1 小时”的量级参考。
- [Picovoice Porcupine](https://github.com/Picovoice/porcupine)
  —— 官方语言列表含 Chinese (Mandarin)、AccessKey、`.ppn` 自定义词。
- [第三方横评：三个唤醒引擎](https://juejin.cn/post/7706345830357712931)
  —— 中文场景下 openWakeWord/Porcupine/Vosk 的实测记录，可作为“openWakeWord 中文
  分数塌陷”的旁证；作者结论偏 Porcupine，读的时候注意其商业倾向。
- [sherpa-onnx Rust KeywordSpotter](https://docs.rs/sherpa-onnx/latest/sherpa_onnx/struct.KeywordSpotter.html)
  —— 以后想把 KWS 搬进 Rust 主进程时用。
