# API 等价估算价格目录设计

## 已核实价格

2026-09-05 核实 [GPT-6 Astra 官方模型页](https://developers.openai.com/api/docs/models/gpt-6-astra)：
Standard 文本价格，USD / 1M Token：输入 10、缓存读取 1、缓存写入 12.5、输出 50。
输入超过 272000 Token 时，整次请求输入及缓存价格乘 2、输出乘 1.5。
Batch/Flex 为 Standard 的 50%，Fast 为适用价格的 2 倍。

当前估算入口接入 Standard 价格；没有服务层级时明确按 Standard 等价估算，不能声称真实账单。
输入包含缓存读取和缓存写入：普通输入 = 输入 - 缓存读取 - 缓存写入。
今日 Token 总量仍是输入 + 输出。未知模型不猜价，不把 `gpt-6` 自动映射为 Astra。

## SQLite 数据模型（自动维护阶段实施）

沿用本机 codex-taskbar.db，采用追加价格版本，不覆盖旧价格：

- pricing_catalog：catalog_id、schema_version、published_at、fetched_at、verified_at、source_url、content_sha256、状态。
- model_prices：price_id、catalog_id、provider、model_id、currency、service_tier、effective_from（未知允许 NULL）、observed_at、输入/缓存读取/缓存写入/输出单价、长上下文阈值及各项倍率。单价为每百万 Token 的整数微美元，倍率用分子分母。
- model_aliases：明确的 alias 与 canonical model_id、catalog_id；禁止模糊前缀匹配。
- usage_costs：本机消耗事件标识、price_id、输入/缓存/写入/输出计数、服务层级、估算金额微美元、估算时间、覆盖完整性。
- pricing_sync：最后尝试/成功时间、ETag、失败类别、下次重试时间。

历史事件固定引用当时 price_id，价格同步不重算历史。用户主动重估时另建估算修订，保留原结果。
旧账本只剩聚合 Token 或金额时，标记 legacy/价格版本未知；不能伪造分模型成本。
官方未声明生效时间时仅记录 observed_at，不能把抓取时间当成实际生效时间。

## 自动同步路径

程序启动先读 SQLite，空库导入内置已核实目录，启动不等待网络。
后台每天检查一次版本化价格目录；使用系统代理和 ETag，失败退避并继续使用最后有效目录。
目录由项目流水线核对官方资料后发布，附官方来源、抓取时间、模型规则与校验/签名。
不将官方网页任意改版后的表格直接当成可信新价格；未通过结构/数值/模型规则验证的目录不激活。
完整校验后在单个事务中追加版本并切换当前目录，任一步失败保留旧目录。
新模型无需升级可执行程序：定价匹配与倍率规则必须从数据读取，移除当前 match 常量表。

设置页增加价格目录版本、最后同步时间、来源、立即更新；无价格/缺少计数字段时显示未估算覆盖率。
网络同步和 usage_costs 迁移属于后续实现；本文不表示软件已经具备自动价格更新。

## 验收

缓存不重复计价，272K 边界、cache write、Standard/Fast/Flex 分别验证；价格变更不改变历史金额。
离线启动、错误目录、未知模型、重复事件、跨日累计、并发同步、旧数据库迁移均须覆盖。
