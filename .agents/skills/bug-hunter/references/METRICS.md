# Bug Hunter 指标定义

## 1. 核心指标

- `Resolution Rate`：被采纳建议 / 总建议。
- `False Positive Rate`：被拒绝建议 / 总建议。
- `Accepted Count`：过阈值问题数。
- `Debate Count`：争议桶数量。
- `Gate Block Rate`：因门禁失败导致的流水线失败次数 / 总运行次数。
- `Fix Lead Time`：从报告产生到问题被修复的时间。

## 2. 角色维度指标

按 `agent` 统计：

- 角色采纳率
- 角色拒绝率
- 角色建议总量

用途：判断哪些角色噪声高、哪些角色召回高。

## 3. 趋势观察建议

至少按周比较：

- Resolution Rate 趋势
- False Positive Rate 趋势
- `critical` / `major` 的命中与误报趋势
- Gate Block Rate 趋势（避免过度阻塞）
- Fix Lead Time 趋势（评估“贴近开发”的真实收益）

## 4. 阈值调优建议

- 若误报偏高：提高 `--threshold`（例如 `0.60 -> 0.65`）。
- 若漏报偏高：降低 `--threshold`（例如 `0.60 -> 0.55`）。
- 优先保持 `critical` 召回，其次再优化 minor 噪声。
- 门禁建议渐进收紧：`critical -> major -> minor`，每次至少观察 1 周再调整。

## 5. 权重更新节奏

- 小团队：每 1 周更新一次权重。
- 高频迭代项目：每 20-30 次决策更新一次。
- 避免每轮都大幅调整，防止模型振荡。
