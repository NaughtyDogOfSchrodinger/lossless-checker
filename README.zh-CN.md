# lossless-checker

[English](./README.md)

一个检测**假无损**音频的启发式工具——从有损源（如 320k MP3）转码、再重新封装成 FLAC/ALAC 来冒充
无损的文件；以及**假 Hi-Res**——把 CD/有损素材上采样塞进 96/192 kHz 容器。可对单个文件给出判断，
也可对整个音乐库批量扫描、输出排好序的可疑清单。

- **严格只读**——只分析、只报告，绝不移动、改名或删除任何文件。
- **纯 Rust 解码** FLAC、ALAC、WAV、AIFF、CAF 等，无系统依赖。DSD（`.dsf`/`.dff`）通过**可选的
  ffmpeg 兜底**解码（仅在扫描 DSD 时才需要）。
- **Hi-Res 感知**——可检测上采样、空高频段、以及频谱空洞。
- **并行**——几千个文件的库在现代机器上几分钟即可扫完。

## 原理

一切都基于解码后的 PCM 推断——容器声称的格式、码率、采样率一律不信。工具用
[symphonia](https://github.com/pdeljanov/Symphonia)（DSD 走 ffmpeg）解码音频，对**整首歌**做分段
加窗 FFT（[rustfft](https://github.com/ejmahler/RustFFT)，采用 Blackman-Harris 窗）、对功率谱取
平均，再跑三个检测器。Blackman-Harris 窗的旁瓣极低（约 −92 dB），可避免低频能量泄漏上溢、掩盖有损
截止断崖。

**1. 高频截止（有损转码）。** 有损编码器在固定频率施加低通，留下一道**能量断崖**，其位置暴露原始码率：

| 来源              | 典型截止频率   |
|-------------------|----------------|
| 真无损            | ~19–23 kHz     |
| 256k 转码         | ~18–19 kHz     |
| 128k 转码         | ~16 kHz        |
| 更低码率          | ~12–15 kHz     |

截止定义为"能量仍在频谱自身**峰值** ~65 dB 以内的最高频率"（**峰值相对**阈值）。

**2. 采样率真伪（假 Hi-Res / 上采样）。** 对声称 Hi-Res（> 48 kHz）的文件，真品内容会明显延伸到
CD 上限（~22 kHz）之上。若真实内容只到 ~22 kHz、且 **~26 kHz 以上一片空白**，那就是 CD/有损素材
被上采样塞进 Hi-Res 容器——标记为 🔼 上采样。报告里的*高频延伸*指标（26 kHz 以上能量相对峰值的
dB）让差距一目了然：真 Hi-Res 约在 −30 dB，上采样会跌到 −70 dB 以下。

**3. 频谱空洞（仅供参考）。** AAC/Vorbis 转码可能在截止以下留下凹陷（notch）。工具会检测并报告，
但因其在真实音乐上易误报，**不影响判定**。

两个刻意的设计点：

- **峰值相对，而非底噪相对。** 以响亮的中频峰值（而不是近乎静音的频谱顶端）为基准，意味着即使
  这首歌本身高频就少，硬低通也会显出真正的断崖——管弦/人声类转码在底噪法里会被读成"满频"，
  峰值法能正确揪出。
- **分析整首。** 很多歌以安静的前奏开场，镲片/打击乐等高频要到后面才进来，只取开头会低估截止、
  造成误报。

## 安装

从[最新发布](https://github.com/NaughtyDogOfSchrodinger/lossless-checker/releases/latest)下载预编译的二进制即可，无需安装工具链。按平台选择对应文件：

| 平台                     | 文件                                                   |
|--------------------------|--------------------------------------------------------|
| macOS（Apple 芯片）      | `lossless-checker-<版本>-aarch64-apple-darwin.tar.gz`  |
| macOS（Intel）           | `lossless-checker-<版本>-x86_64-apple-darwin.tar.gz`   |
| Linux（多数发行版）      | `lossless-checker-<版本>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux（静态 / musl）     | `lossless-checker-<版本>-x86_64-unknown-linux-musl.tar.gz` |
| Windows（x64）           | `lossless-checker-<版本>-x86_64-pc-windows-msvc.zip`   |

**macOS / Linux：**

```bash
tar xzf lossless-checker-*.tar.gz
cd lossless-checker-*/
./lossless-checker "path/to/song.flac"
```

在 macOS 上，从网络下载的二进制会被 Gatekeeper 隔离。若提示*"无法打开，因为无法验证开发者"*，
执行一次以下命令清除隔离标记：

```bash
xattr -d com.apple.quarantine ./lossless-checker
```

想在任意目录运行，把它移到 `PATH` 上，例如 `sudo mv lossless-checker /usr/local/bin/`。

**Windows：**

解压后在 PowerShell 或 `cmd` 里运行 `lossless-checker.exe`：

```powershell
.\lossless-checker.exe "path\to\song.flac"
```

若 SmartScreen 提示应用无法识别，点**更多信息 → 仍要运行**。

> `<版本>` 占位符对应发布标签，如 `v0.1.0`。下面[使用](#使用)示例里凡是写 `cargo run --release --`
> 的地方，都可替换成二进制路径（`./lossless-checker`）。

## 构建

若想从源码构建（需要 [Rust 工具链](https://rustup.rs/)）：

```bash
cargo build --release
# 可执行文件在 ./target/release/lossless-checker
```

### 可选：DSD 支持

DSD（`.dsf`/`.dff`）没有纯 Rust 解码器，工具会调用 **ffmpeg** 把它解到 PCM。其他格式都不需要
ffmpeg。仅在你要扫描 DSD 时才需安装：

```bash
# macOS
brew install ffmpeg
# Debian/Ubuntu
sudo apt install ffmpeg
```

若没装 ffmpeg 又指向 DSD 文件，工具会对该文件报一个清晰的错误并跳过，不影响整次扫描的其余部分。

DSD **只按可听频段判定**：它的超声区是噪声整形噪声（不是信号），且解码后留下多少取决于抽取滤波器，
因此 Hi-Res 上采样检测**不**作用于 DSD（否则会把每个 DSD 都误报）。若某 DSD 确实来自低码率有损母带，
仍能通过其可听截止揪出（< 16.5 kHz 判 🚩）。

## 使用

**单文件** —— 输出详细判断：

```bash
cargo run --release -- "path/to/song.flac"
```

> 输出默认中文；加 `--lang en` 切换为英文。机器可读的 JSON 报告不受语言影响。

```
文件: song.flac
格式: FLAC
采样率: 48000 Hz
采样总数: 12582912
奈奎斯特频率: 24000 Hz
估计高频截止: 20795 Hz (86.6% of Nyquist)
频谱空洞: 无明显空洞

判断: ✅ 高频延伸正常，像真无损
```

声称 Hi-Res 但实为上采样的文件会多出一行 `高频延伸` 并给出 🔼 判定：

```
文件: fake96.flac
格式: FLAC
采样率: 96000 Hz
采样总数: 288000
奈奎斯特频率: 48000 Hz
估计高频截止: 24598 Hz (51.2% of Nyquist)
高频延伸(>26kHz): -114.3 dB (相对频谱峰值；越低代表高频越空)
频谱空洞: 无明显空洞

判断: 🔼 声称为 Hi-Res，但真实内容止于 ~CD 频段，疑似上采样/有损转制的假 Hi-Res
```

**整库批量** —— 传一个目录即可递归、并行扫描，输出排好序的报告：

```bash
cargo run --release -- ~/Music --report scan.txt --json scan.json
```

文本报告含：汇总、**按专辑排行**（🚩/🔼 数量降序——整张专辑同档位低截止＝来源八成有损，这是最强
信号）、完整可疑清单（🚩/🔼 在前、按截止频率升序），以及**解码失败清单**（显式列出，绝不静默跳过）：

```
== 汇总 ==
  ✅ 像真无损 (≥19kHz)          2086
  ⚠️  高频收窄 (16.5-19kHz)      515
  🚩 高度可疑 (<16.5kHz)        185
  🔼 假 Hi-Res (上采样)          12
  ✖  解码失败                   0

== 按专辑排行（🚩/🔼 数量降序） ==
  🚩/🔼 15  ⚠️  0  Some Artist - Debut Album (2006)
  ...

== 可疑文件清单（🚩/🔼 在前，各按截止频率升序） ==
   12672 Hz  🚩  Some Artist - Album/03. track.flac
   24598 Hz  🔼  Some Artist - Hi-Res Album/01. track.flac
   17800 Hz  ⚠️  Other Artist - Album/05. track.flac
   ...
```

加 `--json` 还能得到机器可读的输出。Hi-Res 文件会额外带 `hires_ext_db`；每条结果都带
`format_label` 和 `holes` 计数：

```json
{
  "root": "/Users/you/Music",
  "scanned": 2786,
  "summary": { "clean": 2469, "narrowed": 234, "suspect": 83, "upsampled": 12, "error": 0 },
  "results": [
    { "path": "Album/track.flac", "format_label": "FLAC", "sample_rate": 44100, "cutoff_hz": 12672.0, "ratio": 0.5747, "holes": 0, "verdict": "suspect" },
    { "path": "HiRes/track.flac", "format_label": "FLAC", "sample_rate": 96000, "cutoff_hz": 24598.0, "ratio": 0.5125, "hires_ext_db": -114.3, "holes": 0, "verdict": "upsampled" }
  ]
}
```

### 参数

| 参数           | 默认值                  | 说明 |
|----------------|-------------------------|------|
| `--peak-db`    | `65`                    | 默认检测器的峰值相对阈值（低于频谱峰值多少 dB）。已校准，一般仅调试时覆盖。 |
| `--noise-floor`| 关                      | 改用旧的底噪检测法（配合 `--threshold`），保留用于对比。 |
| `--threshold`  | `10.0`                  | 底噪倍数——仅在 `--noise-floor` 时生效。 |
| `--report`     | stdout                  | 把文本报告写到此文件（仅目录扫描）。 |
| `--json`       | —                       | 额外把 JSON 报告写到此文件（仅目录扫描）。 |
| `--ext`        | `flac,wav,m4a,aif,aiff,caf,alac,dsf,dff` | 逗号分隔的扫描扩展名。只扫无损容器 + DSD——扫 mp3 等本身有损的格式对检测假无损没有意义。 |
| `--jobs`       | CPU 核数                | 并行线程数。 |
| `--lang`       | `zh`                    | 日志与报告的语言：`zh`（中文，默认）或 `en`。JSON 报告不受影响。 |

### 判定区间

对 **CD 采样率**（≤ 48 kHz）的文件，判定基于**绝对截止频率（Hz）**，而非"占奈奎斯特的比例"。因为
有损编码器的低通截止是固定 Hz，与容器采样率无关。若用比例会冤枉 48 kHz 文件——它们的真实内容同样
只到 ~21–22 kHz，比例只有 ~88%，却被误判为可疑。

| 截止频率           | 判断 |
|--------------------|------|
| `≥ 19 kHz`         | ✅ 高频延伸正常，像真无损 |
| `16.8 – 19 kHz`    | ⚠️ 高频收窄，可能是高码率有损转码，建议人工复核 |
| `< 16.8 kHz`       | 🚩 明显断崖，高度疑似假无损 |

对 **Hi-Res**（> 48 kHz）的文件，问题变成"真实内容是否真的延伸过了 CD 上限"。当真实内容止于
~28 kHz 以下，**或** 26 kHz 以上一片空白（高频延伸 ≪ −70 dB），即判为 🔼 **上采样**；否则通过为
✅。这正好揪出把 CD 或有损素材上采样塞进 Hi-Res 容器的文件——单凭绝对 Hz 截止会把它们放过。

### 校准

在约 2786 个真实 FLAC **加上已知答案的回转假无损**（把真 FLAC 经 128k/320k MP3 转一圈再转回——
这就是码率已知的标准假无损）上调校：

- **peak-db（65）：** 从 45 扫到 75。太低会把真无损里高频本就弱的曲子也压垮；太高又让 128k 残留
  重新被读成满频。取 65 时，每个 128k 假无损都落在 **16.0–16.7 kHz**（被揪出），真无损则集中在
  **21–22 kHz**。
- **判定区间：** 真无损集中在 19–22 kHz；128k 回转刚好压在 16.8 kHz 以下，故以此为 🚩 线。

## 局限

这是**启发式判断**，不是铁证。下面的「320k 盲区」是最直观的例子——左为真无损，右为 320k MP3
转码，均为 48 kHz，用 Spek 看一眼：

| 真无损 | 320k MP3 转码 |
|--------|----------------|
| ![真无损频谱图——能量自然延伸到顶部](docs/images/spectrogram-genuine-lossless.png) | ![320k 转码频谱图——约 20 kHz 处有硬低通墙](docs/images/spectrogram-fake-320k.png) |

真无损的能量自然延伸到顶部；320k 转码在 **~20 kHz 处有一道硬低通墙**。人眼一眼就能看出这道墙，
但它正好落在大量真无损自然滚降的位置，截止频率指标无法区分，于是把这个假无损读成 ✅——这正是
为什么要对可疑文件用 Spek 亲眼核对。

完整的注意事项：

- **误报：** 古典、原声、人声、氛围、老录音本身高频能量就少，可能落到 ⚠️ 甚至 🚩；间奏
  (interlude)、skit、纯钢琴曲尤其常见。请把**整张专辑**的规律当作真正的信号，而非孤立单曲。
- **漏报——320k 是盲区。** 回转测试显示 128k 假无损能稳定揪出（~16.5 kHz），但 320k MP3 截止在
  ~20 kHz，正好和大量真无损的自然滚降重叠。任何基于截止频率的指标都无法区分，所以 320k 转码通常
  会读成 ✅。本工具揪的是明显的假，不是高码率的那种。
- **Hi-Res 阈值同样是启发式。** 上采样检测假设真 Hi-Res 内容会延伸过 ~28 kHz；个别真品母带若本身
  就滚降到该频率以下（少见，多为老的模拟源）可能被读成 🔼。这些阈值目前是经过推理的默认值，待用
  带标注的 Hi-Res 样本集进一步校准。
- **DSD 需要 ffmpeg**（见[上文](#可选dsd-支持)）；没有它时 DSD 文件会被报错跳过而不参与分析。
- **性能：** 它会完整解码每一首，所以大库要跑几分钟（已用满所有 CPU 核）；单文件检查近乎瞬时。

它的价值在于**从大库里批量揪出高度可疑的文件**。对被标记的文件，建议用 [Spek](https://www.spek.cc/)
等工具看一眼频谱图再下最终结论。

## 许可证

GPL-3.0 —— 见 [LICENSE](./LICENSE)。
