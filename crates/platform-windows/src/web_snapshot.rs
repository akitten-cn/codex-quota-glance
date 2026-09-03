//! Windows 与 macOS 共用的详情卡 JSON 投影；不包含原始会话或认证信息。
use crate::host::{NativeApiCostEstimate, NativeHostDetails};

pub fn details_web_snapshot(details: &NativeHostDetails) -> String {
    let rows = |rows: &[crate::host::NativeDetailRow]| {
        rows.iter()
            .filter(|row| matches!(row.kind, crate::host::NativeDetailRowKind::Value))
            .map(|row| serde_json::json!({"label": row.label, "value": row.value}))
            .collect::<Vec<_>>()
    };
    let metrics = details
        .metric_cards
        .iter()
        .map(|card| serde_json::json!({"label":card.label,"value":card.value,"detail":card.detail,"progress":card.progress_percent}))
        .collect::<Vec<_>>();
    let trend = details
        .trend_points
        .iter()
        .map(|point| serde_json::json!({"label":point.label,"value":point.value}))
        .collect::<Vec<_>>();
    let trend_series = details
        .trend_series
        .iter()
        .map(|series| serde_json::json!({
            "id": series.id,
            "title": series.title,
            "unit": series.unit,
            "empty_message": series.empty_message,
            "points": series.points.iter().map(|point| serde_json::json!({"label":point.label,"value":point.value})).collect::<Vec<_>>(),
        }))
        .collect::<Vec<_>>();
    // 指标卡只有一列窄宽，保留短美元数值；“官方估算、非订阅账单”的解释
    // 已由卡片脚注和详情页底部文案承载，避免把用户可读金额拆成多行。
    let compact_estimate = details.api_cost_estimate.as_ref().map(NativeApiCostEstimate::compact_display_value);
    serde_json::json!({
        "schema_version": 1,
        "kind": "details",
        "title": details.title,
        "badge": details.badge,
        "status": details.status,
        "updated": details.updated,
        "hero": {"label":details.hero_label,"value":details.hero_value,"hint":details.hero_hint},
        "metric_cards": metrics,
        "primary_rows": rows(&details.primary_rows),
        "secondary_rows": rows(&details.secondary_rows),
        "estimate": compact_estimate,
        "trend_points": trend,
        "trend_title": details.trend_title,
        "trend_series": trend_series,
        "footer": details.footer,
    })
    .to_string()
}
