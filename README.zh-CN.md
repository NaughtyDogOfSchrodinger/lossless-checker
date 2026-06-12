# lossless-checker

[English](./README.md)

一个检测**假无损**音频的启发式工具——从有损源（如 320k MP3）转码、再重新封装成 FLAC/ALAC 来冒充
无损的文件；以及**假 Hi-Res**——把 CD/有损素材上采样塞进 96/192 kHz 容器。可对单个文件给出判断，
也可对整个音乐库批量扫描、输出排好序的可疑清单。

- **严格只读**——只分析、只报告，绝不移动、改名或删除任何文件。
- **纯 Rust、零系统依赖**——FLAC/ALAC/WAV/AIFF/CAF 走 symphonia;DSD（`.dsf`/`.dff`）走原生
  [`check-dsd`](#dsd-真伪检测-check-dsd) 子命令。
- **Hi-Res 感知**——可检测上采样、空高频段、以及频谱空洞。
- **并行**——几千个文件的库在现代机器上几分钟即可扫完。

## 原理

一切都基于解码后的 PCM 推断——容器声称的格式、码率、采样率一律不信。工具用
[symphonia](https://github.com/pdeljanov/Symphonia) 解码音频，对**整首歌**做分段
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
./lossless-checker check "path/to/song.flac"
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
.\lossless-checker.exe check "path\to\song.flac"
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

## 使用

工具分两个子命令：

- **`check`** —— PCM 假无损检测（FLAC/ALAC/WAV/…）。本节全部内容。
- **`check-dsd`** —— 原生 **DSD 真伪检测**（自解析 `.dsf`/`.dff`，零系统依赖）。见下文
  [DSD 真伪检测](#dsd-真伪检测-check-dsd)。

**单文件** —— 输出详细判断：

```bash
cargo run --release -- check "path/to/song.flac"
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
cargo run --release -- check ~/Music --report scan.txt --json scan.json
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
| `--ext`        | `flac,wav,m4a,aif,aiff,caf,alac` | 逗号分隔的扫描扩展名。只扫无损/PCM 容器——扫 mp3 等本身有损的格式没有意义,DSD 走 `check-dsd`。 |
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

## DSD 真伪检测（`check-dsd`）

DSD（`.dsf`/`.dff`）独立成子命令——对 DSD 要问的不是"是否有损转码",而是
**"这是真 DSD 母带,还是 PCM/CD 洗成的 DSD?"**：

```bash
cargo run --release -- check-dsd ~/DSD --album-summary
```

`check-dsd` **零系统依赖**。它自己解析 `.dsf`/`.dff` 容器，取出原始 1-bit 流，直接计算其全频段功率谱
（到 DSD 奈奎斯特，DSD64 约 1.41 MHz）。这暴露了转制者难以低成本伪造的物理指纹：**噪声整形**
——真 DSD 母带把 Sigma-Delta 量化噪声推向 50–100 kHz，频谱在那里显著上扬（正斜率）。由 PCM/有损
转制的假 DSD 缺这段上扬，且基带常残留 CD/有损截止。整条比特流流式处理，内存与文件大小解耦。

每个文件按三项指标判 **Pass** / **Suspicious** / **Unsupported**：噪声整形斜率（dB/oct）、超高频
能量占比（>50 kHz）、基带截止（只看 ≤24 kHz，故 DXD 工作流的 176 kHz 拐点不会被误判为 CD 墙）。
**斜率为主信号**,并门控对基带截止的解读:斜率确证真 SDM 时,基带截止视为母带自然滚降,只有
<16.5 kHz 的硬性低截才仍定罪——2020 年人声/原声母带滚降到 ~19 kHz **不会**被标记;只有当斜率平坦
时,中频段截止才作为 PCM/有损来源的佐证。

此外,**22.05 kHz 处的陡峭数字硬墙**(`cd_wall` flag)即便斜率达标也定罪:这是 CD→DSD 重调制后仍
残留的数字 ADC 指纹,与模拟母带的温和滚降不同。(实测中它揪出了一张 1993 年 CD 时代专辑——其 DSD
复刻带有专辑级的 22.05 kHz 硬墙;而 1977 年模拟母带 DSD 与 2020 年原生 DSD 都没有这种台阶。)

```
== 汇总 ==
  ✅ 通过        124
  🚩 可疑        8
  ⛔ 暂不支持    3
  ✖  解析失败    0
```

> **状态：** v1.0。解析 **DSF** 与 **DFF**（未压缩）；**DST 压缩**的 DSD 报 `Unsupported`。
> 阈值（`--min-slope 6.0`、`--min-hf-ratio 0.05`）是*起步*经验值——边界判定请先用
> 真/假/DXD 样本标定后再采信。运行 `check-dsd --help` 查看全部参数（`--fft-size`、`--slope-lo/-hi`、
> `--hf-threshold`、`--format json`、`-v`）。

> **性能：** 每一帧都做 FFT（不抽帧），但单文件内的 FFT 计算会**跨 CPU 核并行**,结果与单线程逐帧
> 完全一致（逐比特相同）。文件越大提速依旧保持,整库还会跨文件并行。现代多核机器上的单文件实测：
>
> | 速率 | 文件大小 | 单线程 | 并行 | 加速 |
> |------|---------|--------|------|------|
> | DSD64  | 186 MB | 3.3 s  | 0.87 s | 3.8× |
> | DSD256 | 894 MB | 12.6 s | 4.1 s  | 3.1× |

**导出频谱**用于画图（真假对比图，也是标定的依据）：

```bash
cargo run --release -- export-spectrum "track.dsf" -o track.csv   # 或 --channel 0
```

输出 `frequency_hz,power_db` 两列（默认写到 `<文件>.spectrum.csv`，所有声道混合）。喂给
gnuplot/matplotlib/Excel 即可：真 DSD 有基带、然后 50 kHz 以上陡峭的噪声整形上扬；PCM/有损转制的
假货超声区平坦（且基带常带 CD/有损截止）。

阈值与标定方法（含首个已验证的真 DSD 参照样本）见 [`docs/calibration.md`](docs/calibration.md)。

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
- **漏报——高码率/透明编码是盲区。** 回转测试显示 128k 假无损能稳定揪出（~16.5 kHz），但截止断崖
  这套启发式只对施加「低且硬」低通的编码器有效，对下面这些无能为力：
  - **320k MP3**——截止在 ~20 kHz，和大量真无损的自然滚降重叠。
  - **高码率 AAC（256k+）**——软低通约 19–20 kHz，落在同一重叠区；AAC 在截止以下的凹陷虽会被
    *报告*（检测器 3），却从不参与判定——在自然空洞上误报太多。
  - **Opus**——全频带码率下根本没有低通墙，设计上就是透明的，没有断崖可找，每次都读成 ✅。

  任何基于截止频率的指标都无法把它们和真无损区分开。本工具揪的是明显的低码率假，不是透明的那种。
- **Hi-Res 阈值同样是启发式。** 上采样检测假设真 Hi-Res 内容会延伸过 ~28 kHz；个别真品母带若本身
  就滚降到该频率以下（少见，多为老的模拟源）可能被读成 🔼。这些阈值目前是经过推理的默认值，待用
  带标注的 Hi-Res 样本集进一步校准。
- **DSD 走 [`check-dsd`](#dsd-真伪检测-check-dsd)**,不走 `check`——它直接对 1-bit 流做原生分析
  （无 ffmpeg、不转 PCM）。
- **性能：** 它会完整解码每一首，所以大库要跑几分钟（已用满所有 CPU 核）；单文件检查近乎瞬时。

它的价值在于**从大库里批量揪出高度可疑的文件**。对被标记的文件，建议用 [Spek](https://www.spek.cc/)
等工具看一眼频谱图再下最终结论。

这些局限的改进计划记录在 [ROADMAP.md](./ROADMAP.md)。

## 许可证

GPL-3.0 —— 见 [LICENSE](./LICENSE)。
