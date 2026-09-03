# 本地 SQLite 数据模型

主数据库位于 `%LOCALAPPDATA%\CodexTaskbar\codex-taskbar.db`，采用 WAL 与事务提交。
数据库不保存 Prompt、回复、线程标题、完整线程标识或登录凭据。

## `settings_snapshot`

设置属于低频整体提交的聚合对象，因此以单行版本化快照保存：

- `id`：固定为 1；
- `schema_version`：配置结构版本；
- `payload_json`：完整设置快照，便于新增字段时前向兼容；
- `updated_at_unix`：事务提交时间。

旧 `settings.json` 仅在数据库尚无设置行时导入一次，之后不再写入或读取。

## `usage_daily`

按本地日期保存 Token 与 API 等价估算聚合：`day_key`、`total_tokens`、
`input_tokens`、`cached_input_tokens`、`output_tokens`、`estimated_api_cost_micro_usd`。

其中 `input_tokens` 已包含 `cached_input_tokens`，所以：

`total_tokens = input_tokens + output_tokens`

## `usage_hourly`

以 `(day_key, hour)` 为复合主键保存与日表相同的小时聚合字段，用于今日趋势与点位交互。
`hour` 受 0–23 约束，并通过外键关联 `usage_daily`。

旧 `local-usage-ledger.json` 在新数据库没有用量行时导入一次。凡具备输入、输出明细的旧记录，
迁移时都会重新计算日/小时总量，从而移除历史版本可能重复计入的缓存 Token。
