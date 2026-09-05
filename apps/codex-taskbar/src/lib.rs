//! 应用装配层的可测试部分。

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use codex_taskbar_application::{
    RateLimitSnapshot, TokenUsageSnapshot,
    coordinator::{MonitorCoordinator, TelemetryUpdate},
    monitor::MonitorState,
};
use codex_taskbar_domain::{
    activity::ActivityState,
    layout::{DisplayItemConfig as LayoutDisplayItemConfig, DisplayItemKind, ordered_visible_items},
    official::{
        OfficialAccount, OfficialAccountKind, OfficialAccountUsage, OfficialCredits, OfficialEndpointStatus,
        OfficialFreshness, OfficialResetCredits, OfficialSnapshot, OfficialSpendControl, OfficialThreadUsage,
        OfficialThreadUsageGroup,
    },
    quota::{Freshness, QuotaPresence, QuotaValue, QuotaWindowState},
    usage::{TokenCounts, UsageSource},
};
use codex_taskbar_platform_windows::render_model::{
    ActivityLampInput, DipRect, FiveHourProgress, ProgressValue, QuotaRingsInput, RenderModel, render_model,
};
use codex_taskbar_platform_windows::{
    ProbeConfig, format_local_unix_time,
    host::{
        NativeApiCostEstimate, NativeApiCostSource, NativeChartSegment, NativeChartTone, NativeDetailRow,
        NativeHostDetails, NativeHostModel, NativeMetricCard, NativeMetricTone,
    },
};
use codex_taskbar_settings::AppConfig;

#[path = "official_runtime.rs"]
pub mod runtime;
mod session_token_fallback;
#[cfg(windows)]
pub mod updater;
#[cfg(windows)]
pub mod webview_preview;

/// 当前 P0 支持的启动模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Run,
    CheckConfig,
    ProbePlan,
    VisualPreview,
    VisualPreviewIdle,
    /// 固定演示：官方帐户没有 5 小时额度时，仅显示 Weekly。
    VisualPreviewWeeklyOnly,
    /// 固定演示：没有 5 小时额度且 Weekly 已耗尽时，显示 Credits 余额。
    VisualPreviewWeeklyCredits,
    /// 详情卡验收：官方未提供 5 小时额度，仅保留 Weekly。
    VisualPreviewDetailsWeeklyOnly,
    /// 详情卡验收：Weekly 耗尽后显示官方余额与重置卡。
    VisualPreviewDetailsWeeklyCredits,
    VisualPreviewDetails,
    VisualPreviewStrip,
    /// 直接使用 WebView2 运行确认稿，作为切换生产任务栏渲染器前的验收入口。
    WebViewPreview,
    /// 打开真实原生设置窗口，供自动化交互验收。
    SettingsPreview,
    Help,
}

/// 视觉预览启动时要主动展示的浮层；生产运行始终由鼠标交互触发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPopup {
    None,
    Details,
    TokenStrip,
}

/// 解析不带程序名的命令行参数。未知或互斥参数返回可直接展示的错误。
pub fn parse_run_mode(args: impl IntoIterator<Item = String>) -> Result<RunMode, String> {
    let mut mode = RunMode::Run;
    for arg in args {
        let candidate = match arg.as_str() {
            "--check-config" => RunMode::CheckConfig,
            "--probe-plan" => RunMode::ProbePlan,
            "--visual-preview" => RunMode::VisualPreview,
            "--visual-preview-idle" => RunMode::VisualPreviewIdle,
            "--visual-preview-weekly-only" => RunMode::VisualPreviewWeeklyOnly,
            "--visual-preview-weekly-credits" => RunMode::VisualPreviewWeeklyCredits,
            "--visual-preview-details-weekly-only" => RunMode::VisualPreviewDetailsWeeklyOnly,
            "--visual-preview-details-weekly-credits" => RunMode::VisualPreviewDetailsWeeklyCredits,
            "--visual-preview-details" => RunMode::VisualPreviewDetails,
            "--visual-preview-strip" => RunMode::VisualPreviewStrip,
            "--webview-preview" => RunMode::WebViewPreview,
            "--settings-preview" => RunMode::SettingsPreview,
            "--help" | "-h" => RunMode::Help,
            _ => return Err(format!("未知参数：{arg}")),
        };
        if mode != RunMode::Run && mode != candidate {
            return Err("一次只能指定一个运行模式".into());
        }
        mode = candidate;
    }
    Ok(mode)
}

/// 应用数据目录。只构造路径，不创建目录或写文件。
#[must_use]
pub fn app_data_dir(local_app_data: Option<&Path>) -> PathBuf {
    local_app_data.map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir).join("CodexTaskbar")
}

/// 把持久化配置映射为 Windows 探针配置；此转换留在装配层，避免 application 反向依赖平台。
#[must_use]
pub fn probe_config(settings: &AppConfig) -> ProbeConfig {
    ProbeConfig {
        target_monitor_device: settings.target_monitor_device.clone(),
        prefer_secondary_monitor: settings.prefer_secondary_monitor,
        preferred_width_px: settings.preferred_width_px(),
        anchor: settings.anchor,
        reserved_offset_px: settings.reserved_offset_px(),
        edge_gap_px: settings.edge_gap_px(),
        embed_in_taskbar: true,
    }
}

/// 生成不包含用户路径和内容数据的配置摘要。
#[must_use]
pub fn redacted_config_summary(settings: &AppConfig) -> String {
    format!(
        "version={} monitor={} anchor={:?} width_px={} gap_px={} offset_px={} items={} log_level={} codex_cli={}",
        settings.version,
        settings.target_monitor_device.as_deref().unwrap_or("auto-secondary"),
        settings.anchor,
        settings.taskbar_width_px,
        settings.safe_spacing_px,
        settings.traffic_monitor_offset_px,
        settings.display_items.iter().filter(|item| item.visible).count(),
        settings.log_level,
        if settings.codex_cli_path.is_some() { "manual" } else { "auto" }
    )
}

/// 仅用于视觉验收的固定预览状态；绝不读取或混入生产数据。
#[must_use]
pub fn visual_preview_state(observed_at_unix_ms: i64) -> MonitorState {
    let mut coordinator = MonitorCoordinator::default();
    // 固定视觉场景也必须走与正式数据相同的“相对时间 + 精确本地时间”展示链路。
    // 这里仅生成验收用的未来时间戳，不参与生产额度计算或写盘。
    let observed_at_unix = observed_at_unix_ms.div_euclid(1_000).max(1);
    coordinator.apply(TelemetryUpdate::RateLimits(RateLimitSnapshot {
        five_hour: Some(QuotaValue::from_used_percent(57.0, Some(300), Some(observed_at_unix + 3 * 60 * 60))),
        weekly: Some(QuotaValue::from_used_percent(32.0, Some(10_080), Some(observed_at_unix + 6 * 24 * 60 * 60))),
        observed_at_unix_ms,
        revision: 1,
    }));
    coordinator.apply(TelemetryUpdate::TokenUsage(Box::new(TokenUsageSnapshot {
        current_thread: Some(TokenCounts {
            input: Some(24_860),
            cached_input: Some(18_420),
            cache_write_input: Some(1_120),
            output: Some(6_611),
            reasoning_output: Some(2_140),
            total: Some(31_471),
        }),
        last_turn: Some(TokenCounts {
            input: Some(1_820),
            cached_input: Some(1_260),
            cache_write_input: Some(220),
            output: Some(480),
            reasoning_output: Some(160),
            total: Some(2_300),
        }),
        model_context_window: Some(200_000),
        // 视觉预览使用固定、脱敏的“今日”聚合；生产仅在 App Server 明确提供
        // 当日桶时填入该字段，不能从当前线程累计反推。
        today: Some(TokenCounts {
            input: Some(20_600),
            cached_input: Some(15_244),
            output: Some(22_200),
            total: Some(42_800),
            ..TokenCounts::default()
        }),
        observed_at_unix_ms,
        source: UsageSource::AppServer,
    })));
    coordinator.apply(TelemetryUpdate::Activity { states: vec![ActivityState::Executing], observed_at_unix_ms });
    coordinator.apply(TelemetryUpdate::Official(Box::new(OfficialSnapshot {
        account: Some(OfficialAccount {
            kind: OfficialAccountKind::ChatGpt,
            masked_identifier: Some("us***@example.com".to_owned()),
            plan_type: Some("plus".to_owned()),
            auth_mode: Some("chatgpt".to_owned()),
            requires_openai_auth: Some(true),
        }),
        credits: Some(OfficialCredits { has_credits: true, unlimited: false, balance: Some("12.50".to_owned()) }),
        individual_limit: Some(OfficialSpendControl {
            limit: Some("50.00".to_owned()),
            used: Some("17.35".to_owned()),
            remaining_percent: Some(65.3),
            resets_at_unix: Some(observed_at_unix + 8 * 24 * 60 * 60),
        }),
        spend_control_reached: Some(false),
        reset_credits: Some(OfficialResetCredits {
            available_count: 2,
            expiry_times_unix: vec![observed_at_unix + 9 * 24 * 60 * 60, observed_at_unix + 21 * 24 * 60 * 60],
            nearest_expiry_unix: Some(observed_at_unix + 9 * 24 * 60 * 60),
        }),
        account_usage: Some(OfficialAccountUsage {
            lifetime_tokens: Some(12_784_320),
            peak_daily_tokens: Some(486_200),
            longest_running_turn_seconds: Some(842),
            current_streak_days: Some(12),
            longest_streak_days: Some(28),
            latest_daily_date: Some("2026-08-22".to_owned()),
            latest_daily_tokens: Some(159_730),
            daily_usage: Vec::new(),
            // 仅用于 --visual-preview-details/strip 的固定脱敏数据；生产状态
            // 由 App Server 提供真实 thread_usage，二者不会互相读取或回退。
            thread_usage: Some(OfficialThreadUsage {
                thread_id: "preview-official-cost-thread".to_owned(),
                estimated_usage_usd_micros: Some(12_345),
                groups: vec![OfficialThreadUsageGroup {
                    model: Some("preview-model".to_owned()),
                    input_tokens: Some(24_860),
                    cached_input_tokens: Some(18_420),
                    net_new_input_tokens: Some(6_440),
                    output_tokens: Some(6_611),
                    total_tokens: Some(31_471),
                    ..OfficialThreadUsageGroup::default()
                }],
                ..OfficialThreadUsage::default()
            }),
        }),
        rate_limit_name: Some("Codex".to_owned()),
        rate_limit_reached_type: None,
        account_status: OfficialEndpointStatus::live(observed_at_unix_ms),
        quota_status: OfficialEndpointStatus::live(observed_at_unix_ms),
        usage_status: OfficialEndpointStatus::live(observed_at_unix_ms),
        freshness: OfficialFreshness::Live,
        observed_at_unix_ms,
    })));
    coordinator.state().clone()
}

/// 性能采样专用的固定空闲状态。
///
/// 它复用视觉预览的全部额度、Token 和账户数据，仅把活动状态切换为 Idle。
/// 原生宿主会在三秒过渡动画后停止 20 FPS 定时器，因此可测量真正的静态空闲
/// CPU，而不会把持续执行呼吸动画误称为空闲。
#[must_use]
pub fn visual_preview_idle_state(observed_at_unix_ms: i64) -> MonitorState {
    let mut state = visual_preview_state(observed_at_unix_ms);
    state.activity = ActivityState::Idle;
    state.activity_entered_at_unix_ms = observed_at_unix_ms;
    state
}

/// 固定的“没有 5 小时额度”状态，用于真实 WebView2 任务栏验收。
///
/// 这不是生产分支，也不会读写用户数据；它保证 5 小时层在页面中彻底隐藏，
/// 从而可验证 Weekly 成为唯一额度层时的布局和状态着色。
#[must_use]
pub fn visual_preview_weekly_only_state(observed_at_unix_ms: i64) -> MonitorState {
    let mut state = visual_preview_state(observed_at_unix_ms);
    state.five_hour = QuotaWindowState::from_authoritative(None, &state.five_hour);
    state
}

/// 固定的“Weekly 耗尽后显示 Credits”状态，用于真实 WebView2 任务栏验收。
///
/// Credits 是官方返回的原始单位，只以两位小数呈现，绝不擅自标注为金额。
#[must_use]
pub fn visual_preview_weekly_credits_state(observed_at_unix_ms: i64) -> MonitorState {
    let mut state = visual_preview_weekly_only_state(observed_at_unix_ms);
    let observed_at_unix = observed_at_unix_ms.div_euclid(1_000).max(1);
    state.weekly = QuotaWindowState::from_authoritative(
        Some(QuotaValue::from_used_percent(100.0, Some(10_080), Some(observed_at_unix + 6 * 24 * 60 * 60))),
        &state.weekly,
    );
    state
}

/// 把稳定的应用状态格式化为原生最小详情窗内容。
#[must_use]
pub fn taskbar_host_details(state: &MonitorState) -> NativeHostDetails {
    taskbar_host_details_with_settings(state, &AppConfig::default())
}

/// 按当前显示项配置生成详情与右侧摘要。
#[must_use]
pub fn taskbar_host_details_with_settings(state: &MonitorState, settings: &AppConfig) -> NativeHostDetails {
    official_host_details(state, settings)
}

fn official_host_details(state: &MonitorState, settings: &AppConfig) -> NativeHostDetails {
    let official = state.official.as_ref();
    let account = official.and_then(|snapshot| snapshot.account.as_ref());
    let account_usage = official.and_then(|snapshot| snapshot.account_usage.as_ref());
    let official_thread_usage = state.official_thread_usage();
    let official_cost =
        official_thread_usage.and_then(|usage| usage.estimated_usage_usd_micros).map(|amount_micro_usd| {
            NativeApiCostEstimate { amount_micro_usd, model: None, source: NativeApiCostSource::Official }
        });
    let official_cost_value =
        official_cost.as_ref().map(NativeApiCostEstimate::display_value).unwrap_or_else(|| "--".to_owned());
    // “今日”优先来自 App Server 明确给出的日聚合；官方缺失时才使用结构化
    // SQLite 累计量的本机增量账本。它会在界面中明确标为“本机捕获”，绝不由
    // 当前线程累计或账户历史日桶倒推。
    let today_counts = reliable_today_counts(state);
    let today_value = today_counts.and_then(TokenCounts::display_total).map(format_compact_number);
    let metric_cards =
        [quota_metric_card("5 小时", &state.five_hour, true), quota_metric_card("Weekly", &state.weekly, false)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
    let hero_label = "今日消耗".to_owned();
    let hero_value = today_value.clone().unwrap_or_else(|| "等待捕获".to_owned());
    let hero_hint = if today_counts.is_none() {
        "完成一次新的 Codex 消耗后开始累计".to_owned()
    } else if state.token_usage.today_is_partial {
        format!("本机今日累计 · 缓存命中 {}", cache_hit_percent_label(today_counts))
    } else {
        format!("缓存命中 {}", cache_hit_percent_label(today_counts))
    };

    let mut primary_rows = vec![
        NativeDetailRow::new("账户", official_account_label(account)),
        NativeDetailRow::new(
            "方案 / 认证",
            format!("{} · {}", official_plan_label(account), official_auth_label(account)),
        ),
    ];
    if let Some(credits) = official.and_then(|snapshot| snapshot.credits.as_ref()) {
        // Credits 是官方返回的独立余额单位。它不是美元，也不从订阅额度换算；
        // 仅在任务栏中按耗尽规则显示，详情卡中始终如实呈现。
        primary_rows.push(NativeDetailRow::new("余额", official_credits_label(credits)));
    }
    if let Some(reset) = official.and_then(|snapshot| snapshot.reset_credits.as_ref()) {
        primary_rows.push(NativeDetailRow::new("重置券", reset_credits_label(reset)));
    }
    if let Some(limit) = official.and_then(|snapshot| snapshot.individual_limit.as_ref()) {
        primary_rows.push(NativeDetailRow::new("消费上限", official_spend_label(limit)));
        if let Some(resets_at) = limit.resets_at_unix {
            primary_rows.push(NativeDetailRow::new("消费上限刷新", relative_time_label(resets_at)));
        }
    }
    if official.and_then(|snapshot| snapshot.spend_control_reached).is_some_and(|reached| reached) {
        primary_rows.push(NativeDetailRow::new("消费控制", "已触达上限"));
    } else if let Some(reason) = official.and_then(|snapshot| snapshot.rate_limit_reached_type.as_deref()) {
        primary_rows.push(NativeDetailRow::new("限制状态", rate_limit_reason_label(reason)));
    }

    let mut secondary_rows = if today_counts.is_none() {
        vec![
            NativeDetailRow::new("本机统计", "等待新的累计变化"),
            NativeDetailRow::new("今日来源", "官方未回传 · 本机尚未捕获"),
            NativeDetailRow::new("缓存命中", "等待官方明细"),
        ]
    } else {
        vec![
            NativeDetailRow::new("今日输入", optional_compact(today_counts.and_then(|value| value.input))),
            NativeDetailRow::new("今日缓存", optional_compact(today_counts.and_then(|value| value.cached_input))),
            NativeDetailRow::new("今日输出", optional_compact(today_counts.and_then(|value| value.output))),
            NativeDetailRow::new("缓存命中", cache_hit_percent_label(today_counts)),
            NativeDetailRow::new("今日来源", today_usage_source_label(state)),
        ]
    };
    secondary_rows.push(NativeDetailRow::new("本轮预估", official_cost_value.clone()));
    if let Some(usage) = account_usage {
        secondary_rows.push(NativeDetailRow::section("账户汇总"));
        secondary_rows.push(NativeDetailRow::new("历史总消耗", optional_compact(usage.lifetime_tokens)));
    }
    // 自动上浮是“一次官方更新”的快照，不能混入今日累计、重置时间或当前
    // 线程总量。金额与额度差值需由 runtime 在确认同一更新的相邻快照后覆盖；
    // 普通详情刷新没有该上下文时，保持 `--`。
    let quick_rows = token_popup_rows(state);
    NativeHostDetails {
        title: "Codex 官方账户".to_owned(),
        badge: official_badge(account).to_owned(),
        status: official_freshness_label(official).to_owned(),
        updated: official_age_label(official, state),
        hero_label,
        hero_value,
        hero_hint,
        metric_cards,
        compact_primary_column: true,
        health_rows: Vec::new(),
        primary_heading: "额度与账户".to_owned(),
        secondary_heading: "今日消耗".to_owned(),
        primary_rows,
        secondary_rows,
        quick_rows,
        chart_segments: token_chart_segments(today_counts),
        chart_title: "今日构成".to_owned(),
        trend_points: Vec::new(),
        trend_title: String::new(),
        trend_series: Vec::new(),
        api_cost_estimate: official_cost,
        footer: "余额为官方 Credits 原始单位，非美元；本轮预估仅在官方明确提供时展示，非订阅账单。".to_owned(),
        body: official_details_body(state),
        summary_lines: build_summary_lines(state, settings),
    }
}

/// 为“本次 Token 消耗”自动浮窗构建独立快照。
///
/// 该函数只读官方 App Server 的 `last_turn`。SQLite 后备没有逐轮缓存/输出
/// 细目时不会填充它们。金额与额度下降不是稳定的逐轮数据，弹窗不再展示。
#[must_use]
pub fn token_popup_host_details(state: &MonitorState, settings: &AppConfig) -> NativeHostDetails {
    let mut details = taskbar_host_details_with_settings(state, settings);
    details.quick_rows = token_popup_rows(state);
    details
}

fn token_popup_rows(state: &MonitorState) -> Vec<NativeDetailRow> {
    let last_turn = reliable_last_turn_counts(state);
    vec![
        NativeDetailRow::new("输入", optional_compact(last_turn.and_then(|counts| counts.input))),
        NativeDetailRow::new("缓存输入", optional_compact(last_turn.and_then(|counts| counts.cached_input))),
        NativeDetailRow::new("输出", optional_compact(last_turn.and_then(|counts| counts.output))),
        NativeDetailRow::new("命中率", cache_hit_percent_label(last_turn)),
    ]
}

fn optional_compact(value: Option<u64>) -> String {
    value.map(format_compact_number).unwrap_or_else(|| "--".to_owned())
}

/// OpenAI API 的标准文本价格，单位是“每 1M Token 的微美元”。
///
/// 这里只放入已经有官方价格、且允许按 input/cached input/output 三项估算的
/// 模型。调用方必须显式提供模型名；官方 ChatGPT 订阅或 Codex 账户本身不能
/// 反推出 API 模型，更不能把订阅额度当成账单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfficialApiPricing {
    pub model: &'static str,
    pub input_micro_usd_per_million: u64,
    pub cached_input_micro_usd_per_million: u64,
    pub cache_write_input_micro_usd_per_million: Option<u64>,
    pub output_micro_usd_per_million: u64,
    pub long_context_threshold_tokens: Option<u64>,
}

/// 返回一个明确支持的官方 API 模型价格；未知模型返回 `None`，禁止猜价。
#[must_use]
pub fn official_api_pricing(model: &str) -> Option<OfficialApiPricing> {
    match model.trim().to_ascii_lowercase().as_str() {
        // https://developers.openai.com/api/docs/models/gpt-6-astra
        // 2026-09-05 核实的 Standard 文本价格；不是订阅实际扣款。
        "gpt-6-astra" => Some(OfficialApiPricing {
            model: "gpt-6-astra",
            input_micro_usd_per_million: 10_000_000,
            cached_input_micro_usd_per_million: 1_000_000,
            cache_write_input_micro_usd_per_million: Some(12_500_000),
            output_micro_usd_per_million: 50_000_000,
            long_context_threshold_tokens: Some(272_000),
        }),
        "gpt-5" | "gpt-5-chat-latest" => Some(OfficialApiPricing {
            model: "gpt-5",
            input_micro_usd_per_million: 1_250_000,
            cached_input_micro_usd_per_million: 125_000,
            cache_write_input_micro_usd_per_million: None,
            output_micro_usd_per_million: 10_000_000,
            long_context_threshold_tokens: None,
        }),
        "gpt-5-mini" => Some(OfficialApiPricing {
            model: "gpt-5-mini",
            input_micro_usd_per_million: 250_000,
            cached_input_micro_usd_per_million: 25_000,
            cache_write_input_micro_usd_per_million: None,
            output_micro_usd_per_million: 2_000_000,
            long_context_threshold_tokens: None,
        }),
        "gpt-5-nano" => Some(OfficialApiPricing {
            model: "gpt-5-nano",
            input_micro_usd_per_million: 50_000,
            cached_input_micro_usd_per_million: 5_000,
            cache_write_input_micro_usd_per_million: None,
            output_micro_usd_per_million: 400_000,
            long_context_threshold_tokens: None,
        }),
        "gpt-5.2" | "gpt-5.2-chat-latest" => Some(OfficialApiPricing {
            model: "gpt-5.2",
            input_micro_usd_per_million: 1_750_000,
            cached_input_micro_usd_per_million: 175_000,
            cache_write_input_micro_usd_per_million: None,
            output_micro_usd_per_million: 14_000_000,
            long_context_threshold_tokens: None,
        }),
        // Codex 专用模型价格同样来自官方 API pricing；不要把 ChatGPT Plus
        // 的额度折算成这一价格，只有显式选择该模型时才可调用此分支。
        "gpt-5.3-codex" => Some(OfficialApiPricing {
            model: "gpt-5.3-codex",
            input_micro_usd_per_million: 1_750_000,
            cached_input_micro_usd_per_million: 175_000,
            cache_write_input_micro_usd_per_million: None,
            output_micro_usd_per_million: 14_000_000,
            long_context_threshold_tokens: None,
        }),
        "gpt-5.4" => Some(OfficialApiPricing {
            model: "gpt-5.4",
            input_micro_usd_per_million: 2_500_000,
            cached_input_micro_usd_per_million: 250_000,
            cache_write_input_micro_usd_per_million: None,
            output_micro_usd_per_million: 15_000_000,
            long_context_threshold_tokens: Some(272_000),
        }),
        "gpt-5.4-mini" => Some(OfficialApiPricing {
            model: "gpt-5.4-mini",
            input_micro_usd_per_million: 750_000,
            cached_input_micro_usd_per_million: 75_000,
            cache_write_input_micro_usd_per_million: None,
            output_micro_usd_per_million: 4_500_000,
            long_context_threshold_tokens: None,
        }),
        "gpt-5.5" => Some(OfficialApiPricing {
            model: "gpt-5.5",
            input_micro_usd_per_million: 5_000_000,
            cached_input_micro_usd_per_million: 500_000,
            cache_write_input_micro_usd_per_million: None,
            output_micro_usd_per_million: 30_000_000,
            long_context_threshold_tokens: Some(272_000),
        }),
        "gpt-5.6" | "gpt-5.6-sol" => Some(OfficialApiPricing {
            model: "gpt-5.6-sol",
            input_micro_usd_per_million: 4_000_000,
            cached_input_micro_usd_per_million: 400_000,
            cache_write_input_micro_usd_per_million: Some(5_000_000),
            output_micro_usd_per_million: 20_000_000,
            long_context_threshold_tokens: Some(272_000),
        }),
        "gpt-5.6-terra" => Some(OfficialApiPricing {
            model: "gpt-5.6-terra",
            input_micro_usd_per_million: 2_000_000,
            cached_input_micro_usd_per_million: 200_000,
            cache_write_input_micro_usd_per_million: Some(2_500_000),
            output_micro_usd_per_million: 12_000_000,
            long_context_threshold_tokens: Some(272_000),
        }),
        "gpt-5.6-luna" => Some(OfficialApiPricing {
            model: "gpt-5.6-luna",
            input_micro_usd_per_million: 200_000,
            cached_input_micro_usd_per_million: 20_000,
            cache_write_input_micro_usd_per_million: Some(250_000),
            output_micro_usd_per_million: 1_200_000,
            long_context_threshold_tokens: Some(272_000),
        }),
        _ => None,
    }
}

/// 用明确模型的官方价格估算一组 Token 的 API 等价美元费用。
///
/// `input` 已包含缓存输入，因此先扣除 `cached_input`，避免缓存 Token 二次计费。
/// 三项任一缺失，或模型价格未知时返回 `None`；这比用零填充缺失字段更安全。
#[must_use]
pub fn official_api_equivalent_cost(counts: &TokenCounts, model: Option<&str>) -> Option<String> {
    let (amount_micro_usd, canonical_model) = official_api_equivalent_cost_micro_usd(counts, model)?;
    Some(format_micro_usd(u128::from(amount_micro_usd), canonical_model))
}

/// 返回可累计、可画趋势的整数微美元金额和规范模型名。结果只是按官方 API
/// 标准文本价格计算的等价值，不代表 ChatGPT/Codex 订阅实际扣款。
#[must_use]
pub fn official_api_equivalent_cost_micro_usd(
    counts: &TokenCounts,
    model: Option<&str>,
) -> Option<(u64, &'static str)> {
    let pricing = official_api_pricing(model?)?;
    let input = u128::from(counts.input?);
    let cached = u128::from(counts.cached_input?).min(input);
    let cache_write = u128::from(counts.cache_write_input.unwrap_or(0)).min(input.saturating_sub(cached));
    let uncached = input.saturating_sub(cached).saturating_sub(cache_write);
    let output = u128::from(counts.output?);
    let million = 1_000_000_u128;
    let long_context = pricing.long_context_threshold_tokens.is_some_and(|threshold| input > u128::from(threshold));
    let input_multiplier = if long_context { 2_u128 } else { 1 };
    // 长上下文输出价为 1.5 倍，使用整数分子/分母避免浮点累计误差。
    let output_multiplier_numerator = if long_context { 3_u128 } else { 2 };
    let cache_write_rate =
        pricing.cache_write_input_micro_usd_per_million.unwrap_or(pricing.input_micro_usd_per_million);
    let input_cost = (uncached * u128::from(pricing.input_micro_usd_per_million)
        + cached * u128::from(pricing.cached_input_micro_usd_per_million)
        + cache_write * u128::from(cache_write_rate))
        * input_multiplier;
    let output_cost = output * u128::from(pricing.output_micro_usd_per_million) * output_multiplier_numerator / 2;
    let total_micro_usd = (input_cost + output_cost) / million;
    u64::try_from(total_micro_usd).ok().map(|amount| (amount, pricing.model))
}

fn format_micro_usd(micro_usd: u128, model: &str) -> String {
    let dollars = micro_usd / 1_000_000;
    let fraction = micro_usd % 1_000_000;
    // 保留六位小数，既不会把小额请求显示成 0.00，也方便用户核对价格表。
    format!("US${dollars}.{fraction:06}（{model} API 等价估算）")
}

fn official_account_label(account: Option<&OfficialAccount>) -> String {
    let Some(account) = account else { return "等待账户信息".to_owned() };
    account.masked_identifier.clone().unwrap_or_else(|| match account.kind {
        OfficialAccountKind::ChatGpt => "ChatGPT 账户".to_owned(),
        OfficialAccountKind::ApiKey => "API Key".to_owned(),
        OfficialAccountKind::AmazonBedrock => "Amazon Bedrock".to_owned(),
        OfficialAccountKind::Unknown if account.requires_openai_auth == Some(true) => "需要登录".to_owned(),
        OfficialAccountKind::Unknown => "账户类型未知".to_owned(),
    })
}

fn official_plan_label(account: Option<&OfficialAccount>) -> &str {
    account.and_then(|value| value.plan_type.as_deref()).map(plan_label).unwrap_or("--")
}

fn official_badge(account: Option<&OfficialAccount>) -> &'static str {
    match account.map(|value| value.kind) {
        Some(OfficialAccountKind::ChatGpt) => "官方登录",
        Some(OfficialAccountKind::ApiKey) => "API Key",
        Some(OfficialAccountKind::AmazonBedrock) => "Bedrock",
        Some(OfficialAccountKind::Unknown) | None => "等待身份",
    }
}

fn official_auth_label(account: Option<&OfficialAccount>) -> &'static str {
    let Some(account) = account else { return "--" };
    if let Some(mode) = account.auth_mode.as_deref() {
        return match mode.to_ascii_lowercase().as_str() {
            "chatgpt" => "ChatGPT OAuth",
            "apikey" | "api_key" => "API Key",
            "amazonbedrock" | "amazon_bedrock" => "Amazon Bedrock",
            _ => "其他认证",
        };
    }
    match account.kind {
        OfficialAccountKind::ChatGpt => "ChatGPT OAuth",
        OfficialAccountKind::ApiKey => "API Key",
        OfficialAccountKind::AmazonBedrock => "Amazon Bedrock",
        OfficialAccountKind::Unknown => "--",
    }
}

fn plan_label(value: &str) -> &str {
    match value.to_ascii_lowercase().as_str() {
        "free" => "Free",
        "plus" => "Plus",
        "pro" => "Pro",
        "team" => "Team",
        "business" => "Business",
        "enterprise" => "Enterprise",
        "edu" => "Edu",
        _ => value,
    }
}

fn official_credits_label(credits: &OfficialCredits) -> String {
    if credits.unlimited {
        "无限".to_owned()
    } else if !credits.has_credits {
        "未启用".to_owned()
    } else {
        credits.balance.as_deref().map_or_else(
            || "可用，余额未提供".to_owned(),
            // Credits 并非货币；这里只做显示精度归一化，绝不换算成美元。
            format_credits_balance,
        )
    }
}

fn should_show_official_credits(state: &MonitorState) -> bool {
    let Some(credits) = state.official.as_ref().and_then(|snapshot| snapshot.credits.as_ref()) else {
        return false;
    };
    if !credits.has_credits || credits.unlimited || credits.balance.is_none() {
        return false;
    }

    let is_explicitly_exhausted = |window: &QuotaWindowState| {
        window.presence == QuotaPresence::Present
            && window.freshness == Freshness::Fresh
            && window.value.as_ref().is_some_and(|value| value.remaining_percent <= 0.0)
    };

    match state.five_hour.presence {
        QuotaPresence::Present => is_explicitly_exhausted(&state.five_hour),
        QuotaPresence::Absent => is_explicitly_exhausted(&state.weekly),
        QuotaPresence::Unknown => false,
    }
}

fn official_spend_label(limit: &OfficialSpendControl) -> String {
    match (&limit.used, &limit.limit, limit.remaining_percent) {
        (Some(used), Some(total), Some(remaining)) => format!("{used} / {total} · 余 {remaining:.0}%"),
        (_, _, Some(remaining)) => format!("剩余 {remaining:.0}%"),
        (Some(used), Some(total), None) => format!("{used} / {total}"),
        _ => "--".to_owned(),
    }
}

fn reset_credits_label(reset: &OfficialResetCredits) -> String {
    let mut expiry_times = reset.expiry_times_unix.clone();
    if expiry_times.is_empty() {
        expiry_times.extend(reset.nearest_expiry_unix);
    }
    expiry_times.sort_unstable();
    expiry_times.dedup();

    (0..reset.available_count)
        .map(|index| match expiry_times.get(index as usize) {
            Some(expiry) => format!("第 {} 张 · 到期 {}", index + 1, exact_reset_time_label(*expiry)),
            None => format!("第 {} 张 · 官方未提供到期时间", index + 1),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn relative_time_label(timestamp_unix: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default();
    let remaining = timestamp_unix.saturating_sub(now);
    if remaining <= 0 {
        "等待刷新".to_owned()
    } else if remaining < 3_600 {
        format!("{} 分钟后", (remaining + 59) / 60)
    } else if remaining < 86_400 {
        format!("{} 小时后", (remaining + 3_599) / 3_600)
    } else {
        format!("{} 天后", (remaining + 86_399) / 86_400)
    }
}

/// 详情卡的额度卡与重置券同时给出倒计时和准确本地时间，便于用户判断，不需要
/// 自行把 UTC 或“约几小时后”换算。Windows API 不可用时仍保留安全的相对时间。
fn exact_reset_time_label(timestamp_unix: i64) -> String {
    let relative = relative_time_label(timestamp_unix);
    format_local_unix_time(timestamp_unix).map_or(relative.clone(), |local| format!("{relative} · {local}"))
}

fn quota_metric_card(label: &str, window: &QuotaWindowState, expected_five_hour: bool) -> Option<NativeMetricCard> {
    if window.presence == QuotaPresence::Absent {
        return None;
    }
    let value = window.value.as_ref().or(window.last_known.as_ref());
    let remaining = value.map(|value| value.remaining_percent);
    let display = remaining.map_or_else(|| "--".to_owned(), |value| format!("{}% 剩余", value.round() as i32));
    let reset = value.and_then(|value| value.resets_at_unix).map(exact_reset_time_label).unwrap_or_else(|| {
        if expected_five_hour { "未提供重置时间".to_owned() } else { "等待重置时间".to_owned() }
    });
    let live = window.freshness == Freshness::Fresh && window.value.is_some();
    let detail = if live { reset } else { format!("缓存 · {reset}") };
    let tone = if !live {
        NativeMetricTone::Neutral
    } else {
        match remaining.unwrap_or(100.0) {
            value if value <= 10.0 => NativeMetricTone::Critical,
            value if value <= 25.0 => NativeMetricTone::Warning,
            _ => NativeMetricTone::Positive,
        }
    };
    let card = NativeMetricCard::new(label, display, detail, tone);
    Some(remaining.map_or(card.clone(), |value| card.with_progress(value.clamp(0.0, 100.0).round() as u8)))
}

/// 把缓存输入从输入中扣除后生成互斥 Token 段，避免占比图重复计算缓存命中。
fn token_chart_segments(counts: Option<&TokenCounts>) -> Vec<NativeChartSegment> {
    let Some(counts) = counts else { return Vec::new() };
    let input = counts.input.unwrap_or(0);
    let cached = counts.cached_input.unwrap_or(0).min(input);
    let uncached = input.saturating_sub(cached);
    let output = counts.output.unwrap_or(0);
    [
        NativeChartSegment::new("普通输入", uncached, NativeChartTone::Input),
        NativeChartSegment::new("缓存输入", cached, NativeChartTone::Cached),
        NativeChartSegment::new("输出", output, NativeChartTone::Output),
    ]
    .into_iter()
    .filter(|segment| segment.value > 0)
    .collect()
}

fn cache_hit_percent_label(counts: Option<&TokenCounts>) -> String {
    let Some(counts) = counts else { return "--".to_owned() };
    let Some(input) = counts.input.filter(|value| *value > 0) else { return "--".to_owned() };
    let cached = counts.cached_input.unwrap_or(0).min(input);
    format!("{}%", ((cached as f64 / input as f64) * 100.0).round() as u64)
}

fn official_freshness_label(official: Option<&OfficialSnapshot>) -> &'static str {
    let Some(snapshot) = official else { return "等待官方数据" };
    let statuses =
        [snapshot.account_status.freshness, snapshot.quota_status.freshness, snapshot.usage_status.freshness];
    if statuses.iter().all(|status| *status == OfficialFreshness::Live) {
        "官方数据实时"
    } else if statuses.contains(&OfficialFreshness::Live) {
        "官方数据部分实时"
    } else if statuses.contains(&OfficialFreshness::Cached) {
        "官方缓存"
    } else {
        "官方数据不可用"
    }
}

fn official_age_label(official: Option<&OfficialSnapshot>, state: &MonitorState) -> String {
    official
        .and_then(|snapshot| snapshot.quota_status.observed_at_unix_ms.or(snapshot.account_status.observed_at_unix_ms))
        .map(age_label)
        .unwrap_or_else(|| {
            if is_reliable_live_data(state) { "额度刚刚更新".to_owned() } else { "等待官方数据".to_owned() }
        })
}

fn rate_limit_reason_label(reason: &str) -> &'static str {
    match reason {
        "rate_limit_reached" => "额度窗口已用尽",
        "workspace_owner_credits_depleted" => "工作区所有者 Credits 已用尽",
        "workspace_member_credits_depleted" => "成员 Credits 已用尽",
        "workspace_owner_usage_limit_reached" => "工作区消费上限已触达",
        "workspace_member_usage_limit_reached" => "成员消费上限已触达",
        _ => "额度受限",
    }
}

fn official_details_body(state: &MonitorState) -> String {
    let mut lines = vec![
        format!("官方数据: {}", official_freshness_label(state.official.as_ref())),
        format!("今日消耗: {}", optional_compact(reliable_today_counts(state).and_then(TokenCounts::display_total))),
        format!("Weekly 额度: {}", quota_summary(&state.weekly, false)),
    ];
    if state.five_hour.presence == QuotaPresence::Present && state.five_hour.value.is_some() {
        lines.push(format!("5h 额度: {}", quota_summary(&state.five_hour, true)));
    }
    lines.join("\r\n")
}

fn age_label(observed_at_unix_ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default();
    let seconds = now.saturating_sub(observed_at_unix_ms).max(0) / 1_000;
    match seconds {
        0..=5 => "刚刚更新".to_owned(),
        6..=59 => format!("{seconds} 秒前"),
        _ => format!("{} 分钟前", seconds / 60),
    }
}

fn build_summary_lines(state: &MonitorState, settings: &AppConfig) -> [Option<String>; 2] {
    // V2 海浪胶囊只承载可靠且能在 40 DIP 高度读清的四项：今日、缓存、条件
    // Credits 与额度百分比（由渲染器右侧单独绘制）。活动状态已映射为波色，
    // 当前线程 Token、API/数据源与“未知状态”均不再占任务栏文字空间。
    let selected = fitted_display_items(state, settings);
    let today = display_item_is_selected(&selected, DisplayItemKind::TodayTokens)
        .then(|| summary_item(DisplayItemKind::TodayTokens, state, settings).unwrap_or_else(|| "今日 --".to_owned()));
    let cache = display_item_is_selected(&selected, DisplayItemKind::CacheHitRate)
        .then(|| summary_item(DisplayItemKind::CacheHitRate, state, settings).unwrap_or_else(|| "缓存 --".to_owned()));
    let mut secondary = cache.unwrap_or_default();
    if should_show_official_credits(state) {
        let balance = state
            .official
            .as_ref()
            .and_then(|snapshot| snapshot.credits.as_ref())
            .and_then(|credits| credits.balance.as_deref())
            .expect("credits were validated before display");
        secondary.push_str(" · Credits ");
        secondary.push_str(&format_credits_balance(balance));
    }
    [today, (!secondary.is_empty()).then_some(secondary)]
}

/// Credits 是官方后端的独立余额单位，非货币。可解析数值固定两位，其他文本安全保留。
fn format_credits_balance(balance: &str) -> String {
    balance
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| balance.trim().chars().take(16).collect())
}

/// 按用户顺序选择可见项目；总最小宽度超过任务栏预算时，先移除
/// `keep_priority` 较小的项目，同优先级从末尾开始折叠。视觉组件和文字摘要
/// 使用同一结果，避免设置窗口显示“已隐藏”而灯/圆环仍被固定绘制。
fn fitted_display_items(state: &MonitorState, settings: &AppConfig) -> Vec<LayoutDisplayItemConfig> {
    let layout_items = settings
        .display_items
        .iter()
        .map(|item| LayoutDisplayItemConfig {
            kind: item.kind,
            visible: item.visible,
            order: item.order,
            min_width_px: item.min_width_px,
            keep_priority: item.keep_priority,
        })
        .collect::<Vec<_>>();
    let mut selected = ordered_visible_items(&layout_items)
        .into_iter()
        .filter(|item| {
            matches!(item.kind, DisplayItemKind::ActivityLight | DisplayItemKind::QuotaRings)
                || summary_item(item.kind, state, settings).is_some()
        })
        .cloned()
        .collect::<Vec<_>>();
    let budget = settings.taskbar_width_px.max(1);
    while selected.len() > 1 && selected.iter().map(|item| u32::from(item.min_width_px)).sum::<u32>() > budget {
        let remove = selected
            .iter()
            .enumerate()
            .min_by(|(left_index, left), (right_index, right)| {
                left.keep_priority.cmp(&right.keep_priority).then_with(|| right_index.cmp(left_index))
            })
            .map(|(index, _)| index)
            .unwrap_or(selected.len() - 1);
        selected.remove(remove);
    }
    selected
}

fn display_item_is_selected(items: &[LayoutDisplayItemConfig], kind: DisplayItemKind) -> bool {
    items.iter().any(|item| item.kind == kind)
}

fn summary_item(kind: DisplayItemKind, state: &MonitorState, _settings: &AppConfig) -> Option<String> {
    match kind {
        DisplayItemKind::ActivityLight => Some(activity_label_zh(state.activity).to_owned()),
        DisplayItemKind::QuotaRings => None,
        DisplayItemKind::ResetCountdown => {
            // Unknown/Absent 都没有可用于倒计时的可靠 5h 值；返回 None 会让
            // 配置驱动布局自动隐藏该项，避免把“未知”渲染成伪造的 0 或旧值。
            (state.five_hour.presence == QuotaPresence::Present && state.five_hour.value.is_some())
                .then(|| format!("5h {}", quota_inline_summary(&state.five_hour, true)))
        }
        DisplayItemKind::CurrentThreadTokens => state
            .token_usage
            .current_thread
            .as_ref()
            .and_then(TokenCounts::display_total)
            .map(|total| format!("任务 {}", format_compact_number(total))),
        DisplayItemKind::TodayTokens => reliable_today_counts(state)
            .and_then(TokenCounts::display_total)
            .map(|total| format!("今日 {}", format_compact_number(total))),
        DisplayItemKind::InputTokens => reliable_current_counts(state)
            .and_then(|counts| counts.input)
            .map(|input| format!("输入 {}", format_compact_number(input))),
        DisplayItemKind::OutputTokens => reliable_current_counts(state)
            .and_then(|counts| counts.output)
            .map(|output| format!("输出 {}", format_compact_number(output))),
        DisplayItemKind::CacheHitRate => reliable_current_counts(state).and_then(cache_hit_rate_summary),
        DisplayItemKind::DataFreshness => is_reliable_live_data(state).then_some("实时".to_owned()),
    }
}

fn reliable_current_counts(state: &MonitorState) -> Option<&TokenCounts> {
    state.token_usage.fresh.then_some(())?;
    state.token_usage.current_thread.as_ref()
}

fn reliable_last_turn_counts(state: &MonitorState) -> Option<&TokenCounts> {
    if state.token_usage.last_turn_source != UsageSource::SessionLogFallback {
        state.token_usage.fresh.then_some(())?;
    }
    state.token_usage.last_turn.as_ref()
}

fn reliable_today_counts(state: &MonitorState) -> Option<&TokenCounts> {
    match state.token_usage.today_source {
        UsageSource::AppServer if state.token_usage.fresh => {}
        UsageSource::SqliteFallback => {}
        _ => return None,
    }
    state.token_usage.today.as_ref()
}

fn today_usage_source_label(state: &MonitorState) -> &'static str {
    match state.token_usage.today_source {
        UsageSource::AppServer if state.token_usage.fresh => "官方实时",
        UsageSource::SqliteFallback => "本机捕获（非完整日账单）",
        _ => "等待官方或本机捕获",
    }
}

fn cache_hit_rate_summary(counts: &TokenCounts) -> Option<String> {
    let input = counts.input?;
    let cached_input = counts.cached_input?;
    if input == 0 {
        return None;
    }
    let percent = ((cached_input as f32 / input as f32) * 100.0).round() as i32;
    Some(format!("命中 {percent}%"))
}

fn is_reliable_live_data(state: &MonitorState) -> bool {
    state.token_usage.fresh
        || state.weekly.freshness == Freshness::Fresh
        || state.five_hour.freshness == Freshness::Fresh
}

fn activity_label_zh(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Unknown => "未知状态",
        ActivityState::Idle => "空闲",
        ActivityState::Thinking => "思考中",
        ActivityState::Executing => "执行中",
        ActivityState::WaitingForUser => "等待操作",
        ActivityState::Reviewing => "检查中",
        ActivityState::Completed => "已完成",
        ActivityState::Failed => "失败",
    }
}

fn quota_summary(window: &QuotaWindowState, expected_five_hour: bool) -> String {
    match window.presence {
        QuotaPresence::Present => {
            if window.freshness == Freshness::Fresh {
                if let Some(value) = &window.value {
                    return format!("剩余 {}%", value.remaining_percent.round() as i32);
                }
            }
            if let Some(last_known) = &window.last_known {
                return format!("暂无实时数据，上次为 {}%", last_known.remaining_percent.round() as i32);
            }
            "暂无实时数据".to_owned()
        }
        QuotaPresence::Absent => {
            if expected_five_hour {
                "当前账户未提供 5h 窗口".to_owned()
            } else {
                "当前账户未提供该窗口".to_owned()
            }
        }
        QuotaPresence::Unknown => {
            if let Some(last_known) = &window.last_known {
                format!("未知，上次为 {}%", last_known.remaining_percent.round() as i32)
            } else {
                "未知".to_owned()
            }
        }
    }
}

fn quota_inline_summary(window: &QuotaWindowState, expected_five_hour: bool) -> String {
    match window.presence {
        QuotaPresence::Present => {
            if window.freshness == Freshness::Fresh {
                if let Some(value) = &window.value {
                    return format!("{}%", value.remaining_percent.round() as i32);
                }
            }
            if let Some(last_known) = &window.last_known {
                return format!("上次 {}%", last_known.remaining_percent.round() as i32);
            }
            "未知".to_owned()
        }
        QuotaPresence::Absent => {
            if expected_five_hour {
                "未提供".to_owned()
            } else {
                "缺失".to_owned()
            }
        }
        QuotaPresence::Unknown => {
            if let Some(last_known) = &window.last_known {
                format!("上次 {}%", last_known.remaining_percent.round() as i32)
            } else {
                "未知".to_owned()
            }
        }
    }
}

fn format_compact_number(value: u64) -> String {
    // 统一保留一位小数，并在 999.5K/999.5M 处向上进位，避免出现
    // “1000K”或图表、详情和任务栏摘要在边界值处不一致。
    if value >= 1_000_000 {
        return format_compact_scaled(value, 1_000_000, 'M');
    }
    if value >= 1_000 {
        if value >= 999_500 {
            return "1M".to_owned();
        }
        return format_compact_scaled(value, 1_000, 'K');
    }
    value.to_string()
}

fn format_compact_scaled(value: u64, unit: u64, suffix: char) -> String {
    let tenths = ((value % unit).saturating_mul(10).saturating_add(unit / 2)) / unit;
    let mut major = value / unit;
    if tenths >= 10 {
        major = major.saturating_add(1);
        return format!("{major}{suffix}");
    }
    if tenths == 0 { format!("{major}{suffix}") } else { format!("{major}.{tenths}{suffix}") }
}

/// 把稳定的应用状态映射为平台绘制模型。
#[must_use]
pub fn taskbar_render_model(
    state: &MonitorState,
    surface_width_dip: f32,
    surface_height_dip: f32,
    now_ms: u64,
) -> RenderModel {
    let host = taskbar_host_model(state, &AppConfig::default(), surface_width_dip, surface_height_dip);
    let entered_at_ms = u64::try_from(state.activity_entered_at_unix_ms).unwrap_or(0);
    let mut model =
        render_model(host.quota, host.lamp_bounds, ActivityLampInput { state: host.activity, entered_at_ms, now_ms });
    model.show_quota = host.show_quota;
    model.show_lamp = host.show_lamp;
    model.summary_left = host.summary_left;
    if !model.show_quota {
        model.animation.next_frame_at_ms = None;
    }
    model
}

/// 把应用状态映射为原生窗口宿主接受的语义快照。
#[must_use]
pub fn taskbar_host_model(
    state: &MonitorState,
    settings: &AppConfig,
    surface_width_dip: f32,
    surface_height_dip: f32,
) -> NativeHostModel {
    let height = surface_height_dip.max(1.0);
    let selected = fitted_display_items(state, settings);
    let _legacy_lamp_selected = display_item_is_selected(&selected, DisplayItemKind::ActivityLight);
    let show_quota = display_item_is_selected(&selected, DisplayItemKind::QuotaRings);
    // V2 取消独立状态灯和圆环。整个任务栏宿主就是一个可点击的不透明海浪胶囊，
    // Weekly/5h 波面与所有摘要文字都在相同水域内。
    let wave_margin = 2.0_f32.min(height / 4.0);
    let weekly = state
        .weekly
        .value
        .as_ref()
        .filter(|_| state.weekly.is_taskbar_visible())
        .map_or(ProgressValue::Unavailable, |value| ProgressValue::Known {
            remaining_percent: value.remaining_percent,
        });
    let five_hour = match state.five_hour.presence {
        QuotaPresence::Absent => FiveHourProgress::Absent,
        QuotaPresence::Present if state.five_hour.is_taskbar_visible() => {
            state.five_hour.value.as_ref().map_or(FiveHourProgress::Unknown, |value| {
                FiveHourProgress::Present(ProgressValue::Known { remaining_percent: value.remaining_percent })
            })
        }
        QuotaPresence::Present | QuotaPresence::Unknown => FiveHourProgress::Unknown,
    };
    NativeHostModel {
        quota: QuotaRingsInput {
            bounds: DipRect {
                left: wave_margin,
                top: wave_margin,
                right: (surface_width_dip - wave_margin).max(wave_margin + 1.0),
                bottom: (height - wave_margin).max(wave_margin + 1.0),
            },
            weekly,
            five_hour,
        },
        // 保留该字段仅为旧模型兼容；原生渲染器不再画独立灯。
        lamp_bounds: DipRect { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 },
        activity: state.activity,
        show_quota,
        show_lamp: false,
        summary_left: 0.0,
        reduce_motion: settings.reduce_motion,
        taskbar_background_opacity: settings.taskbar_background_opacity(),
    }
}

#[cfg(test)]
mod tests {
    use codex_taskbar_domain::{activity::ActivityState, official::OfficialThreadUsage, quota::QuotaValue};
    use codex_taskbar_settings::{DisplayItemKind, TaskbarAnchor};

    use super::*;

    #[test]
    fn command_line_modes_are_exclusive() {
        assert_eq!(parse_run_mode(["--check-config".into()]), Ok(RunMode::CheckConfig));
        assert_eq!(parse_run_mode(["--visual-preview".into()]), Ok(RunMode::VisualPreview));
        assert_eq!(parse_run_mode(["--visual-preview-idle".into()]), Ok(RunMode::VisualPreviewIdle));
        assert_eq!(parse_run_mode(["--visual-preview-weekly-only".into()]), Ok(RunMode::VisualPreviewWeeklyOnly));
        assert_eq!(parse_run_mode(["--visual-preview-weekly-credits".into()]), Ok(RunMode::VisualPreviewWeeklyCredits));
        assert_eq!(
            parse_run_mode(["--visual-preview-details-weekly-only".into()]),
            Ok(RunMode::VisualPreviewDetailsWeeklyOnly)
        );
        assert_eq!(
            parse_run_mode(["--visual-preview-details-weekly-credits".into()]),
            Ok(RunMode::VisualPreviewDetailsWeeklyCredits)
        );
        assert!(parse_run_mode(["--check-config".into(), "--probe-plan".into()]).is_err());
        assert!(parse_run_mode(["--unknown".into()]).is_err());
    }

    #[test]
    fn settings_map_to_safe_embedded_probe() {
        let settings = AppConfig {
            anchor: TaskbarAnchor::Left,
            taskbar_width_px: 400,
            safe_spacing_px: 12,
            traffic_monitor_offset_px: 24,
            ..AppConfig::default()
        };
        let probe = probe_config(&settings);
        assert_eq!(probe.anchor, TaskbarAnchor::Left);
        assert_eq!(probe.preferred_width_px, 400);
        assert!(probe.embed_in_taskbar);
    }

    #[test]
    fn config_summary_never_contains_manual_cli_path() {
        let settings =
            AppConfig { codex_cli_path: Some(r"C:\Users\person\private\codex.exe".to_owned()), ..AppConfig::default() };
        let summary = redacted_config_summary(&settings);
        assert!(summary.contains("codex_cli=manual"));
        assert!(!summary.contains("person"));
        assert!(!summary.contains("private"));
    }

    #[test]
    fn visual_preview_state_is_fixed_and_fresh() {
        let state = visual_preview_state(123_456);
        assert_eq!(state.activity, ActivityState::Executing);
        assert_eq!(state.weekly.value.as_ref().map(|value| value.remaining_percent.round() as i32), Some(68));
        assert_eq!(state.five_hour.value.as_ref().map(|value| value.remaining_percent.round() as i32), Some(43));
        assert_eq!(state.token_usage.current_thread.as_ref().and_then(TokenCounts::display_total), Some(31_471));
        let thread_usage = state.official_thread_usage().expect("preview official cost snapshot");
        assert_eq!(thread_usage.thread_id, "preview-official-cost-thread");
        assert_eq!(thread_usage.estimated_usage_usd_micros, Some(12_345));
        assert_eq!(thread_usage.groups[0].model.as_deref(), Some("preview-model"));
    }

    #[test]
    fn detail_visual_preview_includes_quota_and_reset_card_exact_times() {
        let details = taskbar_host_details(&visual_preview_state(1_700_000_000_000));
        let five = details.metric_cards.iter().find(|card| card.label == "5 小时").expect("5 小时验收卡");
        let weekly = details.metric_cards.iter().find(|card| card.label == "Weekly").expect("7 天验收卡");
        let reset_card = details.primary_rows.iter().find(|row| row.label == "重置券").expect("重置卡验收行");

        assert!(!five.detail.contains("未提供"));
        assert!(!weekly.detail.contains("等待重置时间"));
        assert!(five.detail.contains('·'));
        assert!(weekly.detail.contains('·'));
        assert!(reset_card.value.contains("第 1 张 · 到期"));
        assert!(reset_card.value.contains("第 2 张 · 到期"));
        assert!(reset_card.value.contains("到期"));
        assert!(reset_card.value.contains('·'));
    }

    #[test]
    fn weekly_only_preview_hides_five_hour_and_only_exposes_credits_after_weekly_is_exhausted() {
        let weekly_only = visual_preview_weekly_only_state(123_456);
        assert!(weekly_only.five_hour.value.is_none());
        assert!(!should_show_official_credits(&weekly_only));

        let weekly_credits = visual_preview_weekly_credits_state(123_456);
        assert!(weekly_credits.five_hour.value.is_none());
        assert_eq!(weekly_credits.weekly.value.as_ref().map(|value| value.remaining_percent), Some(0.0));
        assert!(should_show_official_credits(&weekly_credits));
    }

    #[test]
    fn compact_token_numbers_round_at_unit_boundary() {
        assert_eq!(format_compact_number(999), "999");
        assert_eq!(format_compact_number(999_499), "999.5K");
        assert_eq!(format_compact_number(999_500), "1M");
        assert_eq!(format_compact_number(1_999_500), "2M");
    }

    #[test]
    fn official_api_cost_requires_explicit_known_model_and_complete_counts() {
        let counts = TokenCounts {
            input: Some(1_000_000),
            cached_input: Some(250_000),
            output: Some(100_000),
            ..TokenCounts::default()
        };
        // gpt-5: 0.75M * $1.25 + 0.25M * $0.125 + 0.1M * $10 = $1.96875。
        assert_eq!(
            official_api_equivalent_cost(&counts, Some("gpt-5")),
            Some("US$1.968750（gpt-5 API 等价估算）".to_owned())
        );
        assert_eq!(official_api_equivalent_cost(&counts, None), None);
        assert_eq!(official_api_equivalent_cost(&counts, Some("unknown-model")), None);
        assert_eq!(
            official_api_equivalent_cost(&TokenCounts { output: Some(1), ..TokenCounts::default() }, Some("gpt-5")),
            None
        );
    }

    #[test]
    fn gpt_5_6_prices_use_cached_input_and_long_context_rules_from_official_docs() {
        let standard = TokenCounts {
            input: Some(100_000),
            cached_input: Some(40_000),
            output: Some(10_000),
            ..TokenCounts::default()
        };
        // Terra: 60K*$2 + 40K*$0.20 + 10K*$12 = $0.248。
        assert_eq!(
            official_api_equivalent_cost_micro_usd(&standard, Some("gpt-5.6-terra")),
            Some((248_000, "gpt-5.6-terra"))
        );

        let long = TokenCounts {
            input: Some(1_000_000),
            cached_input: Some(250_000),
            output: Some(100_000),
            ..TokenCounts::default()
        };
        // Sol 超过 272K：输入整体 2 倍、输出 1.5 倍，共 $9.20。
        assert_eq!(
            official_api_equivalent_cost_micro_usd(&long, Some("gpt-5.6-sol")),
            Some((9_200_000, "gpt-5.6-sol"))
        );
        assert_eq!(official_api_pricing("gpt-5.6").map(|pricing| pricing.model), Some("gpt-5.6-sol"));
    }

    #[test]
    fn gpt_6_astra_standard_cache_write_and_long_context_costs() {
        let standard = TokenCounts {
            input: Some(100_000),
            cached_input: Some(40_000),
            cache_write_input: Some(10_000),
            output: Some(10_000),
            ..TokenCounts::default()
        };
        // 50K 普通输入 + 40K 缓存读取 + 10K 缓存写入 + 10K 输出 = $1.165。
        assert_eq!(
            official_api_equivalent_cost_micro_usd(&standard, Some("gpt-6-astra")),
            Some((1_165_000, "gpt-6-astra"))
        );
        let long = TokenCounts {
            input: Some(1_000_000),
            cached_input: Some(250_000),
            output: Some(100_000),
            ..TokenCounts::default()
        };
        assert_eq!(
            official_api_equivalent_cost_micro_usd(&long, Some("gpt-6-astra")),
            Some((23_000_000, "gpt-6-astra"))
        );
        assert_eq!(official_api_pricing("gpt-6"), None);
    }

    #[test]
    fn idle_visual_preview_keeps_data_but_stops_continuous_activity() {
        let executing = visual_preview_state(123_456);
        let idle = visual_preview_idle_state(123_456);
        assert_eq!(idle.activity, ActivityState::Idle);
        assert_eq!(idle.activity_entered_at_unix_ms, 123_456);
        assert_eq!(idle.five_hour, executing.five_hour);
        assert_eq!(idle.weekly, executing.weekly);
        assert_eq!(idle.token_usage, executing.token_usage);
        assert_eq!(idle.official, executing.official);
    }

    #[cfg(any())]
    mod retired_new_api_tests {
        use super::*;

        #[test]
        fn new_api_visual_preview_uses_only_fixed_fake_configuration() {
            let settings = new_api_visual_preview_settings();
            assert!(settings.new_api.is_configured());
            assert_eq!(settings.new_api.base_url, "https://preview.invalid");
            assert_eq!(settings.new_api.api_key, "visual-preview-placeholder");
            assert!(settings.new_api.access_token.is_empty());
            assert!(settings.new_api.new_api_user.is_empty());
            assert!(settings.codex_cli_path.is_none());
        }

        #[test]
        fn new_api_visual_preview_state_contains_provider_without_official_quota() {
            let state = new_api_visual_preview_state(123_456);
            let provider = state.provider.as_ref().expect("fixed provider snapshot");
            assert_eq!(state.activity, ActivityState::Executing);
            assert!(state.official.is_none());
            assert_eq!(state.weekly.presence, QuotaPresence::Unknown);
            assert_eq!(state.five_hour.presence, QuotaPresence::Unknown);
            assert_eq!(provider.health, ProviderHealth::Available);
            assert_eq!(provider.display_name, "New API · 演示环境");
            assert_eq!(provider.available_cny_micros, Some(7_460_000));
            assert_eq!(provider.today_estimated_cny_micros, Some(130_000));
            assert_eq!(provider.today_local_tokens.as_ref().and_then(ProviderTokenUsage::display_total), Some(619_000));
            assert_eq!(provider.pricing_version.as_deref(), Some("preview-2026.08"));
            assert!(provider.account.is_none());
            assert!(provider.key_quota.is_none());
            assert!(provider.today.is_none());
            assert_eq!(provider.observed_at_unix_ms, 123_456);
        }

        #[test]
        fn new_api_summary_uses_balance_today_amount_tokens_and_cache_rate_only() {
            let mut state = new_api_visual_preview_state(123_456);
            let provider = state.provider.as_mut().expect("fixed provider snapshot");
            provider.available_cny_micros = Some(7_460_000);
            provider.today_estimated_cny_micros = Some(130_000);
            provider.today_local_tokens = Some(ProviderTokenUsage {
                input: Some(619_000),
                cached_input: Some(449_394),
                output: Some(8_000),
                total: Some(619_000),
                ..ProviderTokenUsage::default()
            });

            let summary = build_new_api_summary(&state);

            assert!(summary.contains("¥7.46"));
            assert!(summary.contains("今日 ¥0.13"));
            assert!(summary.contains("619K"));
            assert!(summary.contains("命中 72.6%"));
            assert!(!summary.contains("未知状态"));
            assert!(!summary.contains("当前任务"));
            assert!(!summary.contains("API 可用"));
        }

        #[test]
        fn new_api_display_never_reuses_official_quota_or_current_thread_tokens() {
            let mut state = new_api_visual_preview_state(123_456);
            state.five_hour.presence = QuotaPresence::Present;
            state.five_hour.value = Some(QuotaValue::from_used_percent(50.0, Some(300), None));
            state.token_usage.current_thread = Some(TokenCounts { total: Some(9_999_999), ..TokenCounts::default() });
            let provider = state.provider.as_mut().expect("fixed provider snapshot");
            provider.available_cny_micros = Some(7_460_000);
            provider.today_estimated_cny_micros = Some(130_000);
            provider.today_local_tokens =
                Some(ProviderTokenUsage { total: Some(619_000), ..ProviderTokenUsage::default() });

            let details = taskbar_host_details_with_settings(&state, &new_api_visual_preview_settings());

            assert!(!details.body.contains("5 小时"));
            assert!(!details.body.contains("当前任务"));
            assert!(!details.summary_lines.iter().flatten().any(|line| line.contains("5h") || line.contains("任务")));
        }

        #[test]
        fn configured_new_api_does_not_override_an_explicit_official_source_selection() {
            let state = visual_preview_state(123_456);
            let details = taskbar_host_details_with_settings(&state, &new_api_visual_preview_settings());

            assert_eq!(details.title, "Codex 官方账户");
        }

        #[test]
        fn unmatched_api_key_never_falls_back_to_official_quota_details() {
            let mut state = visual_preview_state(123_456);
            state.source_selection = codex_taskbar_domain::source::SourceSelection::ApiUnmatched;

            let details = taskbar_host_details_with_settings(&state, &new_api_visual_preview_settings());

            assert_eq!(details.title, "New API Key 未匹配");
            assert!(!details.body.contains("Weekly 额度"));
        }

        #[test]
        fn new_api_details_restore_account_key_and_pricing_context_without_key_progress_ring() {
            let mut state = new_api_visual_preview_state(123_456);
            let provider = state.provider.as_mut().expect("fixed provider snapshot");
            provider.account = Some(codex_taskbar_domain::provider::ProviderAccount {
                display_name: Some("kitten".to_owned()),
                masked_identifier: Some("ki***@example.com".to_owned()),
                group: Some("Codex-Plus".to_owned()),
                request_count: Some(12_900),
                ..codex_taskbar_domain::provider::ProviderAccount::default()
            });
            provider.key_quota = Some(codex_taskbar_domain::provider::ProviderKeyQuota {
                name: Some("codex plus".to_owned()),
                unlimited: true,
                ..codex_taskbar_domain::provider::ProviderKeyQuota::default()
            });
            let details = taskbar_host_details_with_settings(&state, &new_api_visual_preview_settings());

            assert!(details.metric_cards.is_empty());
            assert_eq!(details.primary_rows[0].label, "余额");
            assert_eq!(details.primary_rows[0].value, "¥7.46");
            assert!(details.primary_rows.iter().any(|row| row.label == "账户" && row.value == "kitten"));
            assert!(details.primary_rows.iter().any(|row| row.label == "分组" && row.value == "Codex-Plus"));
            assert!(details.primary_rows.iter().any(|row| row.label == "当前 Key" && row.value == "codex plus"));
            assert!(details.primary_rows.iter().any(|row| row.label == "价格版本"));
            assert!(
                !details.primary_rows.iter().any(|row| row.label.contains("已授予") || row.label.contains("已使用"))
            );
        }

        #[test]
        fn new_api_details_omit_today_token_row_and_ignore_disabled_key_quota_health() {
            let mut state = new_api_visual_preview_state(123_456);
            state.provider.as_mut().expect("provider").key_quota_health = ProviderHealth::Disabled;
            let details = taskbar_host_details_with_settings(&state, &new_api_visual_preview_settings());

            assert!(!details.secondary_rows.iter().any(|row| row.label == "今日 Token"));
            assert_eq!(details.health_rows[1].value, "无");
            assert!(!details.body.contains("API 未启用"));
        }

        #[test]
        fn new_api_details_chart_uses_today_token_scope() {
            let state = new_api_visual_preview_state(123_456);
            let settings = new_api_visual_preview_settings();
            let details = taskbar_host_details_with_settings(&state, &settings);

            assert_eq!(details.chart_title, "今日本机 Token 构成");
            assert_eq!(
                details
                    .chart_segments
                    .iter()
                    .map(|segment| (segment.label.as_str(), segment.value))
                    .collect::<Vec<_>>(),
                [("普通输入", 167_414), ("缓存输入", 443_586), ("输出", 8_000)]
            );
            // Codex 当前线程输入 38,420 不能进入 New API 本机账本构成图。
            assert!(!details.chart_segments.iter().any(|segment| segment.value == 38_420));
        }

        #[test]
        fn new_api_details_expose_only_profile_scoped_local_ledger_trend() {
            let state = new_api_visual_preview_state(123_456);
            let details = taskbar_host_details_with_settings(&state, &new_api_visual_preview_settings());

            assert_eq!(details.trend_title, "今日本机 Token 趋势");
            assert_eq!(details.trend_points.len(), 5);
            assert_eq!(details.trend_points[0].label, "09:00");
            assert_eq!(details.trend_points[4].value, 116_000);
        }

        #[test]
        fn new_api_details_explain_when_no_local_token_turn_has_been_captured() {
            let mut state = new_api_visual_preview_state(123_456);
            state.provider.as_mut().expect("provider").today_local_tokens = None;

            let details = taskbar_host_details_with_settings(&state, &new_api_visual_preview_settings());

            assert_eq!(details.hero_value, "--");
            assert!(details.body.contains("尚未捕获到本机用量"));
            assert!(details.trend_points.is_empty());
        }

        #[test]
        fn new_api_details_expose_sanitized_sync_and_error_status() {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("system time").as_millis() as i64;
            let mut state = new_api_visual_preview_state(now);
            // Key 配额已不属于当前采集范围；用仍在采集的今日日志端点验证真实限流。
            state.provider.as_mut().expect("provider").today_health = ProviderHealth::RateLimited;
            let settings = new_api_visual_preview_settings();
            let details = taskbar_host_details_with_settings(&state, &settings);

            assert_eq!(
                details.health_rows.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(),
                ["同步状态", "脱敏错误"]
            );
            assert_eq!(details.health_rows[0].value, "使用最近校验缓存");
            assert_eq!(details.health_rows[1].value, "API 限流");
            assert_eq!(details.status, "使用最近校验缓存");
            assert!(!details.body.contains("Codex 本地"));
        }

        #[test]
        fn new_api_details_make_cached_failure_explicit_when_no_endpoint_is_live() {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("system time").as_millis() as i64;
            let mut state = new_api_visual_preview_state(now);
            let provider = state.provider.as_mut().expect("provider");
            provider.account_health = ProviderHealth::AuthenticationFailed;
            provider.key_quota_health = ProviderHealth::RateLimited;
            provider.today_health = ProviderHealth::NetworkUnavailable;

            let details = taskbar_host_details_with_settings(&state, &new_api_visual_preview_settings());
            assert_eq!(details.status, "使用最近校验缓存");
            assert_eq!(details.health_rows[1].value, "API 认证失败");
            assert!(details.body.contains("错误状态: API 认证失败"));
            assert_eq!(details.updated, "刚刚更新");
        }

        #[test]
        fn new_api_taskbar_hides_quota_ring_and_ignores_legacy_freshness_item() {
            let mut state = new_api_visual_preview_state(123_456);
            let settings = new_api_visual_preview_settings();
            state.provider.as_mut().expect("provider").key_quota_health = ProviderHealth::RateLimited;
            let model = taskbar_host_model(&state, &settings, 320.0, 40.0);
            let details = taskbar_host_details_with_settings(&state, &settings);

            assert!(model.show_lamp);
            assert!(!model.show_quota);
            assert_eq!(details.summary_lines[1], None);
            assert!(!details.summary_lines[0].as_deref().unwrap_or_default().contains("API"));
        }

        #[test]
        fn new_api_quick_rows_use_local_ledger_without_key_fallback() {
            let mut state = new_api_visual_preview_state(123_456);
            let provider = state.provider.as_mut().expect("provider");
            provider.key_quota = None;
            provider.usage = None;
            let settings = new_api_visual_preview_settings();
            let details = taskbar_host_details_with_settings(&state, &settings);

            assert_eq!(
                details.quick_rows.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(),
                ["余额", "今日", "Token", "命中"]
            );
            assert_eq!(details.quick_rows[0].value, "¥7.46");
            assert_eq!(details.quick_rows[1].value, "¥0.13");
            assert_eq!(details.quick_rows[2].value, "619K");
            assert!(details.metric_cards.is_empty());
        }
    }

    #[test]
    fn authoritative_missing_five_hour_hides_inner_ring() {
        let mut state = MonitorState::default();
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: None,
            weekly: Some(QuotaValue::from_used_percent(25.0, Some(10_080), None)),
            observed_at_unix_ms: 100,
            revision: 1,
        });
        state.update_activity_at([ActivityState::Idle], 100);

        let model = taskbar_render_model(&state, 300.0, 40.0, 200);
        assert!(model.rings.five_hour.is_none());
        assert!(model.rings.weekly.sweep_angle.is_some());
        assert!(model.animation.next_frame_at_ms.is_some());
    }

    #[test]
    fn stale_quota_never_draws_a_known_progress_arc() {
        let mut state = MonitorState::default();
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(QuotaValue::from_used_percent(25.0, Some(300), None)),
            weekly: Some(QuotaValue::from_used_percent(25.0, Some(10_080), None)),
            observed_at_unix_ms: 100,
            revision: 1,
        });
        state.mark_rate_limits_unavailable();
        let model = taskbar_render_model(&state, 300.0, 40.0, 200);
        assert!(model.rings.weekly.sweep_angle.is_none());
        assert!(model.rings.five_hour.is_none());
    }

    #[test]
    fn details_report_absent_five_hour_without_reviving_it() {
        let mut state = MonitorState::default();
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: None,
            weekly: Some(QuotaValue::from_used_percent(40.0, Some(10_080), None)),
            observed_at_unix_ms: 100,
            revision: 1,
        });
        state.update_activity_at([ActivityState::WaitingForUser], 100);

        let details = taskbar_host_details(&state);
        assert!(details.body.contains("今日消耗: --"));
        assert!(!details.body.contains("5h 额度:"));
        assert!(details.body.contains("剩余 60%"));
    }

    #[test]
    fn official_credits_stay_hidden_while_five_hour_quota_has_remaining_capacity() {
        let state = visual_preview_state(123_456);
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "12.50"));
        assert!(details.summary_lines.iter().flatten().all(|line| !line.contains("Credits")));
    }

    #[test]
    fn official_credits_show_when_five_hour_quota_is_exhausted() {
        let mut state = visual_preview_state(123_456);
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(QuotaValue::from_used_percent(100.0, Some(300), None)),
            weekly: Some(QuotaValue::from_used_percent(32.0, Some(10_080), None)),
            observed_at_unix_ms: 123_457,
            revision: 2,
        });
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "12.50"));
        assert!(details.summary_lines.iter().flatten().any(|line| line.contains("Credits 12.50")));
    }

    #[test]
    fn official_credits_show_when_five_hour_is_authoritatively_absent_and_weekly_is_exhausted() {
        let mut state = visual_preview_state(123_456);
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: None,
            weekly: Some(QuotaValue::from_used_percent(100.0, Some(10_080), None)),
            observed_at_unix_ms: 123_457,
            revision: 2,
        });
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "12.50"));
        assert!(details.summary_lines.iter().flatten().any(|line| line.contains("Credits 12.50")));
    }

    #[test]
    fn official_credits_stay_hidden_when_both_quota_windows_are_authoritatively_absent() {
        let mut state = visual_preview_state(123_456);
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: None,
            weekly: None,
            observed_at_unix_ms: 123_457,
            revision: 2,
        });
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "12.50"));
        assert!(details.summary_lines.iter().flatten().all(|line| !line.contains("Credits")));
    }

    #[test]
    fn official_credits_stay_hidden_when_weekly_quota_is_stale() {
        let mut state = visual_preview_state(123_456);
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: None,
            weekly: Some(QuotaValue::from_used_percent(100.0, Some(10_080), None)),
            observed_at_unix_ms: 123_457,
            revision: 2,
        });
        state.weekly.freshness = Freshness::Stale;
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "12.50"));
        assert!(details.summary_lines.iter().flatten().all(|line| !line.contains("Credits")));
    }

    #[test]
    fn official_credits_stay_hidden_when_credits_are_not_enabled() {
        let mut state = visual_preview_state(123_456);
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(QuotaValue::from_used_percent(100.0, Some(300), None)),
            weekly: Some(QuotaValue::from_used_percent(32.0, Some(10_080), None)),
            observed_at_unix_ms: 123_457,
            revision: 2,
        });
        state
            .official
            .as_mut()
            .expect("preview official snapshot")
            .credits
            .as_mut()
            .expect("preview credits")
            .has_credits = false;
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "未启用"));
        assert!(details.summary_lines.iter().flatten().all(|line| !line.contains("Credits")));
    }

    #[test]
    fn official_credits_stay_hidden_when_balance_is_not_provided() {
        let mut state = visual_preview_state(123_456);
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(QuotaValue::from_used_percent(100.0, Some(300), None)),
            weekly: Some(QuotaValue::from_used_percent(32.0, Some(10_080), None)),
            observed_at_unix_ms: 123_457,
            revision: 2,
        });
        state
            .official
            .as_mut()
            .expect("preview official snapshot")
            .credits
            .as_mut()
            .expect("preview credits")
            .balance = None;
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "可用，余额未提供"));
        assert!(details.summary_lines.iter().flatten().all(|line| !line.contains("Credits")));
    }

    #[test]
    fn official_credits_stay_hidden_when_quota_state_is_unknown() {
        let mut state = visual_preview_state(123_456);
        state.mark_rate_limits_unavailable();
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "12.50"));
        assert!(details.summary_lines.iter().flatten().all(|line| !line.contains("Credits")));
    }

    #[test]
    fn official_credits_stay_hidden_when_the_account_is_unlimited() {
        let mut state = visual_preview_state(123_456);
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(QuotaValue::from_used_percent(100.0, Some(300), None)),
            weekly: Some(QuotaValue::from_used_percent(32.0, Some(10_080), None)),
            observed_at_unix_ms: 123_457,
            revision: 2,
        });
        state
            .official
            .as_mut()
            .expect("preview official snapshot")
            .credits
            .as_mut()
            .expect("preview credits")
            .unlimited = true;
        let details = taskbar_host_details(&state);

        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "无限"));
        assert!(details.summary_lines.iter().flatten().all(|line| !line.contains("Credits")));
    }

    #[test]
    fn details_fall_back_to_last_known_values_when_live_data_is_missing() {
        let mut state = MonitorState::default();
        state.apply_authoritative(RateLimitSnapshot {
            five_hour: Some(QuotaValue::from_used_percent(25.0, Some(300), None)),
            weekly: Some(QuotaValue::from_used_percent(40.0, Some(10_080), None)),
            observed_at_unix_ms: 100,
            revision: 1,
        });
        state.mark_rate_limits_unavailable();
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(321), ..TokenCounts::default() }),
            last_turn: None,
            model_context_window: None,
            today: None,
            observed_at_unix_ms: 100,
            source: UsageSource::AppServer,
        });
        state.mark_token_usage_unavailable();

        let details = taskbar_host_details(&state);
        assert!(details.body.contains("今日消耗: --"));
        assert!(!details.body.contains("5h 额度:"));
        assert!(details.body.contains("上次为 60%"));
        assert!(details.quick_rows.iter().all(|row| row.value == "--"));
    }

    #[test]
    fn official_quick_rows_use_only_the_official_last_turn_with_stable_labels() {
        let mut state = MonitorState::default();
        state.token_usage.current_thread = Some(TokenCounts {
            input: Some(1_000),
            cached_input: Some(900),
            output: Some(200),
            total: Some(1_200),
            ..TokenCounts::default()
        });
        state.token_usage.last_turn = Some(TokenCounts {
            input: Some(10),
            cached_input: Some(4),
            output: Some(6),
            total: Some(16),
            ..TokenCounts::default()
        });
        state.token_usage.fresh = true;

        let details = taskbar_host_details(&state);
        assert_eq!(
            details.quick_rows.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(),
            ["输入", "缓存输入", "输出", "命中率"]
        );
        assert_eq!(details.quick_rows[0].value, "10");
        assert_eq!(details.quick_rows[1].value, "4");
        assert_eq!(details.quick_rows[2].value, "6");
        assert_eq!(details.quick_rows[3].value, "40%");
        // 快览绝不展示当前线程累计值（1.2K），只展示本次官方 last_turn。
        assert!(!details.quick_rows.iter().any(|row| row.value == "1.2K"));
    }

    #[test]
    fn official_quick_rows_are_unknown_without_last_turn() {
        let mut state = MonitorState::default();
        state.token_usage.current_thread = Some(TokenCounts {
            input: Some(100),
            cached_input: Some(50),
            output: Some(20),
            total: Some(120),
            ..TokenCounts::default()
        });
        state.token_usage.fresh = true;

        let details = taskbar_host_details(&state);
        assert_eq!(
            details.quick_rows.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(),
            ["输入", "缓存输入", "输出", "命中率"]
        );
        assert!(details.quick_rows.iter().all(|row| row.value == "--"));
    }

    #[test]
    fn official_details_use_compact_account_column_and_expanded_token_dashboard() {
        let state = visual_preview_state(123_456);
        let details = taskbar_host_details(&state);
        assert!(details.compact_primary_column);
        assert_eq!(details.primary_heading, "额度与账户");
        assert_eq!(details.secondary_heading, "今日消耗");
        assert_eq!(details.hero_label, "今日消耗");
        assert_eq!(details.hero_value, "42.8K");
        assert_eq!(details.hero_hint, "缓存命中 74%");
        assert!(details.secondary_rows.iter().any(|row| row.label == "今日输入" && row.value == "20.6K"));
        assert!(details.secondary_rows.iter().any(|row| row.label == "今日缓存" && row.value == "15.2K"));
        assert!(details.secondary_rows.iter().any(|row| row.label == "历史总消耗" && row.value == "12.8M"));
        assert!(
            details.secondary_rows.iter().all(|row| !row.label.contains("推理") && !row.label.contains("当前任务"))
        );
        assert!(details.trend_points.is_empty());
        assert!(details.primary_rows.iter().any(|row| row.label == "余额" && row.value == "12.50"));
        assert!(details.primary_rows.iter().any(|row| row.label == "重置券"));
    }

    #[test]
    fn official_details_prefer_server_usd_estimate_and_mark_it_non_bill() {
        let mut state = visual_preview_state(123_456);
        state
            .official
            .as_mut()
            .expect("official preview")
            .account_usage
            .as_mut()
            .expect("account usage")
            .thread_usage =
            Some(OfficialThreadUsage { estimated_usage_usd_micros: Some(1_250), ..OfficialThreadUsage::default() });
        let details = taskbar_host_details(&state);
        assert_eq!(details.api_cost_estimate.as_ref().map(|estimate| estimate.amount_micro_usd), Some(1_250));
        assert_eq!(
            details.secondary_rows.iter().find(|row| row.label == "本轮预估").map(|row| row.value.as_str()),
            Some("US$0.001250（官方估算 · 非账单）")
        );
        assert!(details.quick_rows.iter().all(|row| row.label != "预计金额" && row.label != "额度消耗"));
    }

    #[test]
    fn token_popup_details_contains_only_reliable_turn_token_metrics() {
        let state = visual_preview_state(123_456);
        let details = token_popup_host_details(&state, &AppConfig::default());
        assert_eq!(details.quick_rows[0].label, "输入");
        assert_eq!(details.quick_rows[0].value, "1.8K");
        assert_eq!(details.quick_rows[1].value, "1.3K");
        assert_eq!(details.quick_rows[2].value, "480");
        assert_eq!(details.quick_rows[3].value, "69%");
        assert_eq!(details.quick_rows.len(), 4);
    }

    #[test]
    fn recent_turn_metrics_are_exclusive_to_the_consumption_popup() {
        let mut state = MonitorState::default();
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: None,
            last_turn: Some(TokenCounts {
                input: Some(12_400),
                cached_input: Some(8_200),
                output: Some(2_700),
                total: Some(15_100),
                ..TokenCounts::default()
            }),
            model_context_window: None,
            today: None,
            observed_at_unix_ms: 100,
            source: UsageSource::SessionLogFallback,
        });
        state.apply_token_usage(TokenUsageSnapshot {
            current_thread: Some(TokenCounts { total: Some(42_800), ..TokenCounts::default() }),
            last_turn: None,
            model_context_window: Some(200_000),
            today: None,
            observed_at_unix_ms: 200,
            source: UsageSource::AppServer,
        });
        // 最近一轮是弹窗口径，绝不能在详情卡中冒充今日累计。
        state.mark_token_usage_unavailable();

        let details = taskbar_host_details(&state);
        assert!(details.secondary_rows.iter().all(|row| !row.label.starts_with("最近")));
        let popup = token_popup_host_details(&state, &AppConfig::default());
        assert_eq!(popup.quick_rows[0].value, "12.4K");
        assert_eq!(popup.quick_rows[1].value, "8.2K");
        assert_eq!(popup.quick_rows[2].value, "2.7K");
        assert_eq!(popup.quick_rows[3].value, "66%");
    }

    #[test]
    fn official_chart_segments_subtract_cached_input_and_never_double_count() {
        let counts = TokenCounts {
            input: Some(100),
            cached_input: Some(70),
            output: Some(25),
            total: Some(125),
            ..TokenCounts::default()
        };
        let segments = token_chart_segments(Some(&counts));
        assert_eq!(
            segments.iter().map(|segment| (segment.label.as_str(), segment.value)).collect::<Vec<_>>(),
            [("普通输入", 30), ("缓存输入", 70), ("输出", 25)]
        );
        assert_eq!(segments.iter().map(|segment| segment.value).sum::<u64>(), 125);
    }

    #[test]
    fn default_summary_uses_today_and_cache_without_status_text() {
        let state = visual_preview_state(123_456);
        let details = taskbar_host_details_with_settings(&state, &AppConfig::default());
        assert_eq!(details.summary_lines, [Some("今日 42.8K".to_owned()), Some("命中 74%".to_owned())]);
    }

    #[test]
    fn hidden_summary_items_do_not_leave_stale_text_in_the_wave_capsule() {
        let state = visual_preview_state(123_456);
        let mut settings = AppConfig::default();
        for kind in [DisplayItemKind::TodayTokens, DisplayItemKind::CacheHitRate] {
            settings.display_items.iter_mut().find(|item| item.kind == kind).expect("display item").visible = false;
        }
        let details = taskbar_host_details_with_settings(&state, &settings);
        assert_eq!(details.summary_lines, [None, None]);
    }

    #[test]
    fn hiding_already_disabled_current_thread_token_keeps_compact_summary() {
        let state = visual_preview_state(123_456);
        let mut settings = AppConfig::default();
        settings
            .display_items
            .iter_mut()
            .find(|item| item.kind == DisplayItemKind::CurrentThreadTokens)
            .expect("token item")
            .visible = false;

        let details = taskbar_host_details_with_settings(&state, &settings);
        assert_eq!(details.summary_lines, [Some("今日 42.8K".to_owned()), Some("命中 74%".to_owned())]);
    }

    #[test]
    fn moving_hidden_current_thread_token_does_not_change_summary_layout() {
        let state = visual_preview_state(123_456);
        let mut settings = AppConfig::default();
        settings
            .display_items
            .iter_mut()
            .find(|item| item.kind == DisplayItemKind::ResetCountdown)
            .expect("reset item")
            .order = 2;
        settings
            .display_items
            .iter_mut()
            .find(|item| item.kind == DisplayItemKind::CurrentThreadTokens)
            .expect("token item")
            .order = 3;

        let details = taskbar_host_details_with_settings(&state, &settings);
        assert_eq!(details.summary_lines, [Some("今日 42.8K".to_owned()), Some("命中 74%".to_owned())]);
    }

    #[test]
    fn host_model_uses_one_shared_wave_viewport_without_a_status_lamp() {
        let model = taskbar_host_model(&MonitorState::default(), &AppConfig::default(), 320.0, 40.0);
        assert_eq!(model.lamp_bounds.width(), 0.0);
        assert!((model.quota.bounds.width() - 316.0).abs() < f32::EPSILON);
        assert!((model.quota.bounds.left - 2.0).abs() < f32::EPSILON);
        assert!((model.quota.bounds.top - 2.0).abs() < f32::EPSILON);
        assert!(!model.show_lamp);
        assert!(model.show_quota);
    }

    #[test]
    fn hiding_visual_items_removes_their_rendering_and_transparent_space() {
        let mut settings = AppConfig::default();
        for item in &mut settings.display_items {
            if matches!(item.kind, DisplayItemKind::ActivityLight | DisplayItemKind::QuotaRings) {
                item.visible = false;
            }
        }
        let model = taskbar_host_model(&MonitorState::default(), &settings, 320.0, 40.0);
        assert!(!model.show_lamp);
        assert!(!model.show_quota);
        assert_eq!(model.summary_left, 0.0);
    }

    #[test]
    fn legacy_lamp_order_cannot_split_the_shared_wave_viewport() {
        let mut settings = AppConfig::default();
        settings.display_items.iter_mut().find(|item| item.kind == DisplayItemKind::QuotaRings).expect("rings").order =
            0;
        settings
            .display_items
            .iter_mut()
            .find(|item| item.kind == DisplayItemKind::ActivityLight)
            .expect("lamp")
            .order = 1;
        let model = taskbar_host_model(&MonitorState::default(), &settings, 320.0, 40.0);
        assert_eq!(model.quota.bounds.left, 2.0);
        assert_eq!(model.lamp_bounds.width(), 0.0);
    }

    #[test]
    fn narrow_width_folds_lower_priority_items_before_lamp_and_rings() {
        let settings = AppConfig { taskbar_width_px: 96, ..AppConfig::default() }.normalize();
        let selected = fitted_display_items(&MonitorState::default(), &settings);
        assert!(display_item_is_selected(&selected, DisplayItemKind::ActivityLight));
        assert!(display_item_is_selected(&selected, DisplayItemKind::QuotaRings));
        assert!(selected.iter().map(|item| u32::from(item.min_width_px)).sum::<u32>() <= 96);
    }
}
