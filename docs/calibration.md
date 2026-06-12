# DSD 真伪检测 — 阈值标定

> 本文记录 `check-dsd` 各阈值的标定流程、起步默认值与已采集的参照样本。
> 阈值是**经验值**，必须用真/假/DXD 样本验证后才可信。判定逻辑与指标定义见
> [`dsd-detection-design.md`](../dsd-detection-design.md)。

## 1. 指标回顾

`check-dsd` 对每个文件计算三项指标，全部来自原始 1-bit 流的全频段 Welch 功率谱：

| 指标 | 含义 | 真 DSD 期望 |
|---|---|---|
| `noise_shaping_slope` (dB/oct) | 30–100 kHz 对 (log2 f, dB) 最小二乘斜率 | 显著正（实测真品 +10 ~ +29） |
| `hf_ratio` | >50 kHz 线性能量占总能量比 | 高（噪声整形把能量推到超声区） |
| `baseband_cutoff_hz` | ≤24 kHz 基带内的陡降拐点 | 无，或自然滚降 |

## 2. 判定逻辑：斜率为主，门控基带截止

噪声整形斜率是**主信号**——转制者难以低成本伪造。基带截止的解读由斜率门控：

- **斜率达标**（`slope ≥ min_noise_shaping_slope`，确证真 SDM）：基带截止多为母带自然滚降，
  **不单独定罪**。只有接近 22.05 kHz 的 **CD 硬墙**（CD→DSD 重调制后仍残留）或 **<16.5 kHz 的硬性
  低截**才仍标记。
- **斜率平坦**：文件已缺 DSD 指纹，基带截止作为 PCM/有损来源的**佐证**，严格解读（任何
  `< lossy_cutoff_max_hz` 或近 CD 墙都标记）。

> 这条门控是用真实样本（见 §4）发现并修正的：纯按基带截止定罪会把人声/原声母带的自然滚降误判为
> 假货。

## 3. 起步默认阈值

| 阈值 | 默认 | CLI | 说明 |
|---|---|---|---|
| `min_noise_shaping_slope` | 6.0 dB/oct | `--min-slope` | 低于此视为缺噪声整形 |
| `min_hf_ratio` | 0.05 | `--min-hf-ratio` | 低于此视为超高频能量异常偏低 |
| `hf_threshold_hz` | 50 000 | `--hf-threshold` | 超高频能量统计下限 |
| `slope_fit_lo_hz` | 30 000 | `--slope-lo` | 斜率拟合下限 |
| `slope_fit_hi_hz` | 100 000 | `--slope-hi` | 斜率拟合上限（DSD128 可放宽到 200 k） |
| `cd_cutoff_hz` / `cd_cutoff_tol_hz` | 22 050 / 1 000 | — | CD 硬墙位置与容差 |
| `lossy_cutoff_max_hz` | 20 000 | — | 平斜率分支：低于此视为有损低截 |
| `hard_lossy_cutoff_hz` | 16 500 | — | 斜率达标时仍定罪的硬性低截线 |
| `baseband_max_hz` | 24 000 | — | 基带截止只看 ≤ 此频率（避开 DXD 176 k 拐点） |

## 4. 标定流程

1. **采集真品集**：以真 DSD 录制著称的发烧厂牌原生录音（2L、Channel Classics、Native DSD 等）。
2. **采集假品集**：已被验真为 CD/有损转制的 DSD；或自己用工具把 CD/有损转成 DSD 作对照。
3. 对两组逐文件跑 `check-dsd -v`（或 `export-spectrum` 导出频谱画分布直方图）：记录
   `noise_shaping_slope`、`hf_ratio`、`baseband_cutoff_hz` 的分布。
4. 找两组分布的分界点设阈值，留一定 margin 降低误判。
5. **DXD 单独验证**：确认 DXD(352.8k)→DSD 不被误伤——其 SDM 真实、斜率正常，基带可能有 176 k 拐点
   （已被 `baseband_max_hz` 排除）。
6. 把样本来源与结论记入本文件 §5。

> 用 `export-spectrum` 导出的真假对比 CSV 本身就是极具传播力的内容素材。

## 5. 参照样本记录

### 5.1 真品参照 — 藤田惠美《心の時間》(2020) [DSD128]

第一张验证过的真品。10 轨 DSF（DSD128，5.6448 MHz），3.6 GB，流式分析 ~12 s（多核）。

- **噪声整形斜率全部强正：+9.7 ~ +29.3 dB/oct** —— 确证真 Sigma-Delta 调制（假货此处≈0）。
- `hf_ratio` ≈ 0.99（DSD 典型）。
- 基带：6 轨在 17.9–23.2 kHz 有自然滚降，4 轨延伸到 ~24 k。**专辑内分布混合**（非全专辑统一卡
  22.05 k 硬墙），符合"逐曲不同年代/模拟母带"，而非统一洗版。
- **判定（门控逻辑后）：10/10 Pass。**

> 关键发现：早期判定逻辑（基带截止独立定罪）把这 6 轨误判为可疑。据此引入"斜率达标时只在 CD 墙/
> <16.5 k 才定罪"的门控，并新增 `hard_lossy_cutoff_hz`。本专辑可作为「真 DSD + 自然滚降」的回归参照。

| 轨道 | slope (dB/oct) | hf_ratio | 基带截止 | 判定 |
|---|---|---|---|---|
| 01 Sexy | +16.2 | 0.996 | 19466 Hz | ✅ |
| 02 Kimi wo Nosete | +21.5 | 0.997 | 19638 Hz | ✅ |
| 03 Shiroi Buranko | +11.7 | 0.991 | 无 | ✅ |
| 04 Kaerenai Futari | +14.3 | 0.995 | 23170 Hz | ✅ |
| 05 Paredo | +9.7 | 0.992 | 无 | ✅ |
| 06 Ken to Mary | +13.6 | 0.994 | 无 | ✅ |
| 07 Suiyo no Bara | +24.2 | 0.997 | 19466 Hz | ✅ |
| 08 Ano Koro no Mama | +29.3 | 0.996 | 19380 Hz | ✅ |
| 09 Cherry | +11.0 | 0.990 | 无 | ✅ |
| 10 Present | +21.9 | 0.995 | 17916 Hz | ✅ |

### 5.2 待补

- [ ] 已知假品（CD/有损→DSD）样本：测斜率是否真的偏平、基带是否卡 22.05 k。
- [ ] DXD→DSD 样本：确认不被误伤。
- [ ] 按采样率分档（DSD64 / 128 / 256）的斜率分布，必要时分档设阈值。

## 6. 合成对照（CI）

无真实样本时，集成测试用合成信号锚定判定方向（`src/dsd/run.rs` tests）：

- **真**：26 kHz 带限源 → 2 阶误差反馈 SDM → 有噪声整形、无基带截止 → Pass。
- **假**：同源裸符号量化（无整形）→ 平斜率 → WeakNoiseShaping → Suspicious。

"CD→DSD 带 22 k 硬墙但仍有斜率"的精确场景由 `src/dsd/judge.rs` 单测覆盖。
