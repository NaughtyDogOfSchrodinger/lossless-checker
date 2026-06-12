# DSD 真伪检测模块开发文档

> 模块代号：`dsd-checker`
> 归属项目：`lossless-checker`
> 文档版本：v1.0
> 适用范围：在不转 PCM 的前提下，对 DSD（DSF/DFF）文件做"只读不改"的频谱分析，判定其是否为真 DSD 母带、抑或由 PCM/有损源转制而来。

---

## 1. 背景与目标

### 1.1 问题陈述

`lossless-checker` 目前解决的是 **PCM 假无损** 问题：检测 FLAC/ALAC/WAV 等无损容器里是否封装了实际来自 MP3/AAC 的有损音频（破绽为高频截止、频谱空洞）。

DSD 领域存在一个 **镜像问题**：市面上大量 SACD 抓轨（`.dsf` / `.dff`）并非真正的 DSD 母带，而是由 16/44.1 PCM 甚至有损源经 Sigma-Delta 调制"洗"成 DSD 格式的假货。听众无法从文件格式判断真伪，必须从信号特征入手。

### 1.2 核心判据

真 DSD 来自 Sigma-Delta 调制（SDM），具有一个**无法低成本伪造的物理指纹——噪声整形（noise shaping）**：量化噪声被推向超高频，频谱在 50–100 kHz 区间呈显著上扬（约 +18 ~ +24 dB/oct）。

| 来源类型 | 基带（<24 kHz）特征 | 超高频（>50 kHz）特征 |
|---|---|---|
| 真 DSD 母带 | 自然滚降，无人工截止 | 强噪声整形上扬，斜率显著为正 |
| PCM(CD)→DSD | 22.05 kHz 处硬截止残留 | 上扬可能存在但形状/拐点异常 |
| 有损→DSD | 16–20 kHz 截止 + 频谱空洞 | 高频能量异常偏低或人工痕迹 |
| DXD(352.8k)→DSD | 可能有 DXD 拐点（**正常，勿误伤**） | 正常噪声整形 |

### 1.3 模块目标

1. 解析 DSF 与 DFF 容器，流式取出 1-bit DSD 比特流。
2. 在比特流上直接计算特征：密度包络、功率谱、噪声整形斜率、超高频能量占比、基带截止点。
3. 输出结构化判定结果（通过 / 可疑 + 原因）。
4. 提供批量（per-album）聚合与频谱 CSV 导出（用于画对比图、内容创作）。
5. 全程流式、`rayon` 并行，内存占用与文件大小解耦。

### 1.4 非目标

- **不做 DSD→PCM 高保真转换**（那是另一条 DSP 路径，本模块只为分析做廉价近似）。
- **不修改 DSD 数据**（纯只读）。
- 不做实时播放、不做 DoP/Native 封装（属于播放器范畴，非检测器范畴）。

---

## 2. 系统架构

### 2.1 分层结构

```
┌─────────────────────────────────────────────┐
│  CLI 层 (clap)                               │
│  子命令: check-dsd / export-spectrum         │
├─────────────────────────────────────────────┤
│  编排层 (orchestrator)                       │
│  文件发现 / rayon 并行调度 / per-album 聚合  │
├─────────────────────────────────────────────┤
│  分析层 (analysis)                           │
│  DsdAnalyzer: 包络 / 功率谱 / 斜率 / 能量比  │
│  judge: 综合判定逻辑 + 阈值                  │
├─────────────────────────────────────────────┤
│  解析层 (container)                          │
│  DsfReader / DffReader → 统一 DsdStream 接口 │
├─────────────────────────────────────────────┤
│  基础设施层                                  │
│  rustfft / rayon / 错误类型 / 配置           │
└─────────────────────────────────────────────┘
```

### 2.2 数据流

```
文件路径
  → 容器解析（识别 DSF/DFF，读取元数据）
  → 流式读取 block group（按声道去交错）
  → 每块解包为 ±1 序列（注意 DSF=LSB-first, DFF=MSB-first）
  → 喂入 Welch 功率谱累加器（FFT 分块平均）
  → 同时累加密度包络（可选，用于响度/削顶）
  → 全文件读完后：从累加谱计算斜率 / 能量比 / 基带截止
  → judge() 综合判定
  → 单文件结果 → per-album 聚合 → 输出
```

关键约束：**比特流绝不一次性全部展开到内存**。DSD64 单声道每秒约 2.8M 样本，若展开成 `Vec<f32>` 为 11 MB/s，一首 5 分钟曲目单声道即 ~3.3 GB。必须边读边喂、累加即弃。

---

## 3. 模块详细设计

### 3.1 目录结构（建议并入 `lossless-checker`）

```
src/
├── main.rs                  # CLI 入口，clap 定义
├── lib.rs                   # 库导出
├── pcm/                     # 现有 PCM 假无损检测（保持不变）
│   └── ...
├── dsd/
│   ├── mod.rs               # 模块导出
│   ├── container/
│   │   ├── mod.rs
│   │   ├── dsf.rs           # DSF 解析器
│   │   ├── dff.rs           # DFF 解析器
│   │   └── stream.rs        # 统一 DsdStream trait
│   ├── analysis/
│   │   ├── mod.rs
│   │   ├── analyzer.rs      # DsdAnalyzer
│   │   ├── spectrum.rs      # Welch 功率谱
│   │   ├── envelope.rs      # 密度包络
│   │   └── metrics.rs       # 斜率 / 能量比 / 截止检测
│   ├── judge.rs             # 综合判定 + 阈值配置
│   └── report.rs            # 单文件 / 专辑结果结构
└── common/
    ├── error.rs
    └── config.rs
```

### 3.2 容器解析层

#### 3.2.1 统一抽象 `DsdStream`

两种容器格式差异较大，用 trait 抹平，分析层只面对统一接口。

```rust
/// DSD 流的统一元数据
#[derive(Debug, Clone)]
pub struct DsdMeta {
    pub format: DsdContainer,   // Dsf | Dff
    pub channels: u32,
    pub sample_rate: u64,       // 2_822_400 = DSD64, 5_644_800 = DSD128 ...
    pub bit_order: BitOrder,    // Lsb (DSF) | Msb (DFF)
    pub total_samples_per_channel: Option<u64>, // DFF 可能未知
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DsdContainer { Dsf, Dff }

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BitOrder { Lsb, Msb }

/// 一个 block group：所有声道各一块，已去交错
/// channels[c] = 第 c 声道本块的原始 DSD 字节
pub struct BlockGroup {
    pub channels: Vec<Vec<u8>>,
}

/// 统一流式读取接口
pub trait DsdStream {
    fn meta(&self) -> &DsdMeta;
    /// 读下一组（所有声道各一块）；EOF 返回 Ok(None)
    fn next_block_group(&mut self) -> Result<Option<BlockGroup>, DsdError>;
}
```

#### 3.2.2 DSF 解析规格

DSF（Sony）chunk 结构，**全部小端**：

| Chunk | 字段 | 字节 | 说明 |
|---|---|---|---|
| DSD | magic `"DSD "` | 4 | |
| | chunk size | 8 | =28 |
| | total file size | 8 | |
| | metadata pointer | 8 | 指向 ID3，0 表示无 |
| fmt | magic `"fmt "` | 4 | |
| | chunk size | 8 | =52 |
| | format version | 4 | =1 |
| | format id | 4 | 0=DSD raw |
| | channel type | 4 | |
| | channel num | 4 | |
| | sampling freq | 4 | 2822400 / 5644800 ... |
| | bits per sample | 4 | =1 |
| | sample count | 8 | 每声道样本数 |
| | block size per ch | 4 | 通常 4096 |
| | reserved | 4 | |
| data | magic `"data"` | 4 | |
| | chunk size | 8 | |
| | sample data | … | block 交错 |

**关键陷阱：**
- **位序 = LSB-first**：字节内 bit0 是最早的样本。
- **交错方式 = 大块分离**：`[ch0 整块 4096B][ch1 整块 4096B]...` 然后下一组。**不是逐字节交错**。
- 最后一组可能未填满 `block_size`，尾部用 0 填充（静音），分析时影响极小可忽略，但精确实现应按 `sample_count` 裁剪。

#### 3.2.3 DFF 解析规格

DFF（DSDIFF，Philips）采用 **IFF 风格、大端**的嵌套 chunk：

- 顶层 `FRM8` 容器，内含 `FVER`、`PROP`、`DSD ` / `DST ` 等。
- 采样率、声道在 `PROP` chunk 的子块（`FS  `、`CHNL`）里。
- **位序 = MSB-first**：字节内 bit7 是最早的样本。
- **交错方式 = 逐字节按声道交错**（与 DSF 不同）：`[ch0 1B][ch1 1B][ch0 1B][ch1 1B]...`。
- 注意 `DST ` 表示 **DST 无损压缩** 的 DSD，需要先解压才能分析——v1.0 可先**只支持未压缩 `DSD ` chunk**，遇到 `DST ` 报"暂不支持"。

> **实务建议**：若不想自己写两套解析器，可直接用 `symphonia` 解析容器拿到 DSD packet，本模块只做分析层。考虑到 `lossless-checker` 已依赖 `symphonia`，这是最省事的路径。但 `symphonia` 对 DFF/DST 的支持需实测确认；自写解析器可控性更高。**推荐：DSF 自写（简单可靠），DFF 优先尝试 symphonia。**

### 3.3 分析层

#### 3.3.1 比特解包

```rust
/// 把一块 DSD 字节按位序展开为 ±1.0
/// bit_order 决定字节内的位顺序
#[inline]
pub fn unpack_block(bytes: &[u8], order: BitOrder, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(bytes.len() * 8);
    match order {
        BitOrder::Lsb => {
            for &b in bytes {
                for i in 0..8 {
                    out.push(if (b >> i) & 1 == 1 { 1.0 } else { -1.0 });
                }
            }
        }
        BitOrder::Msb => {
            for &b in bytes {
                for i in (0..8).rev() {
                    out.push(if (b >> i) & 1 == 1 { 1.0 } else { -1.0 });
                }
            }
        }
    }
}
```

#### 3.3.2 功率谱（Welch 平均）

**算法规格：**
- 方法：Welch（分块加窗 FFT，幅度平方平均）。
- 窗函数：Hann。
- `fft_size`：默认 65536（DSD64 下频率分辨率约 43 Hz/bin，足以分辨 30–100 kHz 区间斜率）。
- 重叠：默认无重叠（hop = fft_size）；要更平滑可设 50% 重叠。
- 输出：`Vec<(freq_hz, power_db)>`，长度 `fft_size/2`。
- **流式累加**：维护一个 `fft_size` 的滑动缓冲，跨 block group 边界连续填充，攒满一帧就 FFT 并累加进 `accum[]`，永不保存全部样本。

```rust
pub struct WelchAccumulator {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    fft_size: usize,
    fill: Vec<f32>,        // 当前帧填充缓冲
    fill_len: usize,
    accum: Vec<f64>,       // 长度 fft_size/2
    blocks: usize,
}

impl WelchAccumulator {
    pub fn new(fft_size: usize) -> Self { /* 建 planner, 算 Hann 窗 */ }

    /// 持续喂入 ±1 样本，内部自动攒帧、FFT、累加
    pub fn feed(&mut self, samples: &[f32]) {
        for &s in samples {
            self.fill[self.fill_len] = s;
            self.fill_len += 1;
            if self.fill_len == self.fft_size {
                self.process_frame();
                self.fill_len = 0;
            }
        }
    }

    fn process_frame(&mut self) { /* 加窗 → FFT → norm_sqr 累加 */ }

    /// 收尾，输出 (freq, dB)
    pub fn finalize(self, sample_rate: f64) -> Vec<(f64, f64)> { /* ... */ }
}
```

> **并行注意**：`rustfft` 的 planner 产出的 `Fft` 是 `Send + Sync`（`Arc<dyn Fft>`），可跨线程共享；但每个文件用独立的 `WelchAccumulator` 实例（各自持有 fill 缓冲与 accum），天然适合 `rayon` 按文件并行。

#### 3.3.3 密度包络（可选）

滑动窗口求 ±1 均值，近似瞬时幅度。用途：响度估计、削顶检测、动态范围。默认窗口 64（对应一次 ~44 kHz 等效带宽的粗解调），输出可抽取以降数据量。非真伪判定的必需项，列为可选特征。

#### 3.3.4 指标计算

```rust
/// 噪声整形斜率：在 [f_lo, f_hi] 对 (log2 freq, dB) 做最小二乘
/// 返回 dB/oct。真 DSD 期望 +18 ~ +24
pub fn noise_shaping_slope(spectrum: &[(f64, f64)], f_lo: f64, f_hi: f64) -> f64;

/// 超高频能量占比 = (Σ power[f>thr]) / (Σ power[all])，线性域
pub fn hf_energy_ratio(spectrum: &[(f64, f64)], threshold_hz: f64) -> f64;

/// 基带截止检测：在 [0, max_hz] 内寻找陡降拐点
/// 复用 lossless-checker 现有 PCM rolloff 逻辑
/// 返回 Some(cutoff_hz) 或 None
pub fn detect_baseband_cutoff(spectrum: &[(f64, f64)], max_hz: f64) -> Option<f64>;
```

**斜率拟合细节**：以 `log2(freq)` 为自变量，斜率单位自然为 dB/oct。拟合区间默认 30 kHz–100 kHz（DSD64 下 100 kHz 远低于 1.41 MHz 奈奎斯特，安全）。DSD128 可放宽到 30 kHz–200 kHz。

### 3.4 判定层

```rust
#[derive(Debug, Clone)]
pub struct DsdThresholds {
    pub min_noise_shaping_slope: f64, // 默认 6.0 dB/oct
    pub min_hf_ratio: f64,            // 默认 0.05
    pub cd_cutoff_hz: f64,            // 22_050
    pub cd_cutoff_tol_hz: f64,        // 1_000
    pub lossy_cutoff_max_hz: f64,     // 20_000
    pub slope_fit_lo_hz: f64,         // 30_000
    pub slope_fit_hi_hz: f64,         // 100_000
    pub hf_threshold_hz: f64,         // 50_000
}

impl Default for DsdThresholds { /* 上述默认值 */ }

#[derive(Debug, Clone, serde::Serialize)]
pub struct DsdVerdict {
    pub file: String,
    pub container: String,
    pub sample_rate: u64,
    pub channels: u32,
    pub noise_shaping_slope: f64,
    pub hf_ratio: f64,
    pub baseband_cutoff_hz: Option<f64>,
    pub flags: Vec<String>,
    pub status: VerdictStatus,   // Pass | Suspicious | Unsupported
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub enum VerdictStatus { Pass, Suspicious, Unsupported }
```

**判定规则（v1.0）：**

1. `slope < min_noise_shaping_slope` → flag「缺乏自然噪声整形上扬（疑似 PCM 转制）」
2. `hf_ratio < min_hf_ratio` → flag「超高频能量异常偏低」
3. 基带截止：
   - `|cutoff - 22050| < tol` → flag「22 kHz 截止（疑似 CD/PCM 来源）」
   - `cutoff < lossy_cutoff_max` → flag「低位截止（疑似有损来源）」
   - 其余可疑截止 → 普通 flag
4. 任一 flag 命中 → `Suspicious`；全部通过 → `Pass`。
5. **误伤防护**：176 kHz 附近的拐点是 DXD 工作流正常特征，**不计入** CD/有损判定。基带截止只对 ≤24 kHz 区间敏感。

### 3.5 编排与聚合层

- **文件发现**：递归扫描目录，匹配 `.dsf` / `.dff`（大小写不敏感）。
- **并行**：`rayon` 的 `par_iter()` 按文件并行，每文件独立 `WelchAccumulator`。
- **per-album 聚合**：按父目录分组（沿用 `lossless-checker` 既有约定），统计每专辑 Pass/Suspicious 比例，输出专辑级结论。
- **进度**：可选 `indicatif` 进度条。

---

## 4. CLI 接口规格

```
lossless-checker check-dsd [OPTIONS] <PATHS>...

参数:
  <PATHS>...                  文件或目录（可多个）

选项:
  -r, --recursive             递归扫描子目录
      --fft-size <N>          FFT 尺寸 [默认: 65536]
      --slope-lo <HZ>         斜率拟合下限 [默认: 30000]
      --slope-hi <HZ>         斜率拟合上限 [默认: 100000]
      --min-slope <DB>        噪声整形斜率阈值 [默认: 6.0]
      --hf-threshold <HZ>     超高频能量统计下限 [默认: 50000]
      --min-hf-ratio <R>      超高频能量占比阈值 [默认: 0.05]
  -j, --jobs <N>              并行线程数 [默认: CPU 核数]
      --format <FMT>          输出格式: text | json [默认: text]
      --album-summary         输出 per-album 聚合
  -v, --verbose               打印每文件详细指标
```

```
lossless-checker export-spectrum [OPTIONS] <FILE>

把单文件功率谱导出为 CSV，供画频谱对比图。

选项:
      --fft-size <N>          [默认: 65536]
  -o, --output <PATH>         CSV 输出路径 [默认: <file>.spectrum.csv]
      --channel <N>           指定声道 [默认: 0，或 mix 混合]
```

CSV 列格式：`frequency_hz,power_db`，便于直接喂给 gnuplot / Python matplotlib / Excel 作图，用于内容创作中的"真假 DSD 频谱对比"。

---

## 5. 性能与资源

| 项目 | 约束 / 目标 |
|---|---|
| 内存 | 与文件大小**解耦**；峰值 ≈ FFT 缓冲 + 单 block group。单文件应远低于 100 MB。 |
| 禁止 | 一次性 unpack 整曲到 `Vec<f32>`（会到 GB 级）。 |
| 并行粒度 | 按文件（rayon par_iter）；声道在文件内串行喂同一谱或各自累加。 |
| FFT planner | `Arc<dyn Fft>` 可共享；累加器每文件独立。 |
| 解包热点 | `unpack_block` 是内循环，`#[inline]`；可考虑 256 项查表（每字节 → 预存 8 个 ±1）进一步提速。 |
| I/O | 用 `BufReader`；按 block_size 对齐读取。 |

**查表优化（可选进阶）**：预生成 `[[f32; 8]; 256]`，每个字节值直接查出对应的 8 个 ±1 样本，`unpack_block` 内层从位运算变为 `memcpy`，显著降低解包开销。

---

## 6. 测试与标定

### 6.1 单元测试

- **容器解析**：用最小合法 DSF/DFF 头部字节构造 fixture，断言元数据字段正确。覆盖：错误 magic、截断文件、非 1-bit、未支持的 DST。
- **位序**：构造已知字节（如 `0b00000001`），断言 LSB-first 与 MSB-first 解包结果相反。
- **斜率拟合**：喂入合成的已知斜率直线（freq vs dB），断言拟合结果误差 < 0.1 dB/oct。
- **能量比**：合成纯高频 / 纯低频谱，断言占比为 ~1 / ~0。

### 6.2 集成测试

- 合成一段真 SDM 信号（用第三诉求里的 `SdmModulator` 把正弦/噪声调制成 DSD），断言被判 `Pass`。
- 取 16/44.1 正弦 → 上采样 → SDM 调制成假 DSD，断言被判 `Suspicious` 且命中 22 kHz 截止 flag。

### 6.3 阈值标定流程（关键，必须做）

文档给出的阈值（`slope=6.0`、`hf_ratio=0.05`）是**起步经验值**，必须用真实样本标定：

1. **采集真品集**：从公认发烧厂牌的原生 DSD 录音（如 2L、Channel Classics、Native DSD 等以真 DSD 录制著称的来源）取样本。
2. **采集假品集**：已知由 CD/有损转制的 DSD（论坛上被验真过的假货、或自己用工具转制的对照样本）。
3. 对两组跑 `export-spectrum`，画分布直方图：噪声整形斜率分布、hf_ratio 分布。
4. 找到两组分布的分界点，设为阈值；保留一定 margin 降低误判。
5. **DXD 来源单独验证**：确认 DXD→DSD 不被误伤（其 SDM 是真的，斜率正常）。
6. 把标定结果与样本来源记录进 `docs/calibration.md`。

> 标定产出的"真假 DSD 频谱对比图"本身就是极具传播力的内容素材，可直接用于 Xiaohongshu / 什么值得买 的 DSD 续作。

---

## 7. 已知局限与边界情况

| 情况 | 处理 |
|---|---|
| DFF + DST 压缩 | v1.0 不支持，报 `Unsupported`；v2 再加 DST 解压。 |
| DXD(352.8k)→DSD | SDM 真实，斜率正常，**不应误伤**；基带截止只看 ≤24 kHz。 |
| 极短文件 | FFT 帧数不足，斜率不可靠；设最小时长门槛，不足则标注低置信。 |
| 多声道（5.1） | 逐声道分析或下混；判定取最可疑声道。 |
| DSD128/256 | 奈奎斯特更高，斜率拟合上限可放宽；阈值需分采样率标定。 |
| 母带本身高频滚降 | 真 DSD 也可能有温和滚降，单凭基带不能定罪，**必须结合噪声整形斜率综合判断**。 |

---

## 8. 路线图

**v1.0（本文档范围）**
- DSF 自写解析 + DFF（symphonia 或自写，未压缩）
- Welch 功率谱 + 噪声整形斜率 + 能量比 + 基带截止
- `check-dsd` / `export-spectrum` 两个子命令
- rayon 并行 + per-album 聚合
- 起步阈值 + 标定流程文档

**v1.1**
- 查表解包优化
- 密度包络衍生指标（响度、削顶、动态范围）
- 多声道智能下混判定

**v2.0**
- DST 解压支持
- 按采样率自适应阈值（DSD64/128/256 分档）
- 置信度评分（不只二元 Pass/Suspicious）
- 与 PCM 检测器结果统一报告

---

## 9. 集成检查清单

- [ ] `dsd` 模块并入 `lossless-checker`，复用现有 error / config / rayon 设施
- [ ] 基带截止检测复用 PCM 侧 rolloff 逻辑，避免重复实现
- [ ] CLI 在现有 clap 结构下新增 `check-dsd` / `export-spectrum` 子命令
- [ ] 输出结构 `#[derive(Serialize)]`，与现有 JSON 报告格式对齐
- [ ] per-album 聚合复用现有按目录分组约定
- [ ] 文档 `docs/dsd-detection.md`（本文件）入库
- [ ] 标定结果 `docs/calibration.md` 入库
- [ ] 真/假/DXD 三类样本的集成测试入 CI
