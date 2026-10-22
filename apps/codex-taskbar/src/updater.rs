//! GitHub Releases 更新检查、下载校验与退出后替换。
//!
//! 网络请求每次读取当前用户代理：静态代理显式绑定，PAC/WPAD 使用自动解析。
//! 只有带 GitHub SHA-256 digest 或官方发布校验清单的 exe 资产才会
//! 被接受，避免把未校验的下载内容交给更新助手执行。

use std::{
    ffi::c_void,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows::{
    Win32::{
        Foundation::{CloseHandle, GlobalFree, HGLOBAL, WAIT_OBJECT_0},
        Networking::WinHttp::{
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_CURRENT_USER_IE_PROXY_CONFIG,
            WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle,
            WinHttpConnect, WinHttpGetIEProxyConfigForCurrentUser, WinHttpOpen, WinHttpOpenRequest,
            WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
            WinHttpSendRequest, WinHttpSetTimeouts,
        },
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    },
    core::PCWSTR,
};

const RELEASE_API_LIMIT: usize = 2 * 1024 * 1024;
const UPDATE_LIMIT: usize = 256 * 1024 * 1024;
const PARALLEL_DOWNLOAD_THRESHOLD: u64 = 8 * 1024 * 1024;
const PARALLEL_DOWNLOAD_PARTS: u64 = 4;
const UPDATE_ASSET_SUFFIX: &str = "-windows-x64.exe";
const DEFAULT_REPOSITORY: &str = "akitten-cn/codex-quota-glance";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: String,
    pub asset_name: String,
    pub download_url: String,
    pub sha256: [u8; 32],
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    Current,
    Available(ReleaseInfo),
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("发布仓库未配置")]
    RepositoryMissing,
    #[error("发布仓库格式无效，应为 owner/repo 或 GitHub 仓库网址")]
    RepositoryInvalid,
    #[error("更新地址无效或不是 HTTPS")]
    InvalidUrl,
    #[error("WinHTTP 请求失败：{0}")]
    Http(String),
    #[error("更新服务返回 HTTP {0}")]
    HttpStatus(u32),
    #[error("GitHub 请求频率受限（HTTP {0}）；请稍后重试，或切换代理节点后再试")]
    RateLimited(u32),
    #[error("GitHub 拒绝访问（HTTP 403）；请检查系统代理规则是否包含 api.github.com 和 GitHub 下载域名")]
    Forbidden,
    #[error("更新响应超过安全大小限制")]
    ResponseTooLarge,
    #[error("更新清单无法解析：{0}")]
    InvalidManifest(String),
    #[error("发布版本没有 Windows x64 单文件资产")]
    AssetMissing,
    #[error("发布资产没有 GitHub SHA-256 digest")]
    DigestMissing,
    #[error("下载文件 SHA-256 校验失败")]
    DigestMismatch,
    #[error("本地文件操作失败：{0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

struct InternetHandle(*mut c_void);

impl Drop for InternetHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

pub fn configured_repository() -> Option<String> {
    Some(repository_from(
        std::env::var("CODEX_TASKBAR_UPDATE_REPOSITORY").ok().as_deref(),
        option_env!("CODEX_TASKBAR_UPDATE_REPOSITORY"),
    ))
}

fn repository_from(runtime: Option<&str>, build: Option<&str>) -> String {
    runtime
        .into_iter()
        .chain(build)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or(DEFAULT_REPOSITORY)
        .to_owned()
}

pub fn check_latest(repository: Option<&str>) -> Result<UpdateStatus, UpdateError> {
    let repository = normalize_repository(repository.ok_or(UpdateError::RepositoryMissing)?)?;
    let url = format!("https://api.github.com/repos/{repository}/releases/latest");
    let bytes = match http_get(&url, RELEASE_API_LIMIT) {
        Ok(bytes) => bytes,
        Err(UpdateError::RateLimited(_) | UpdateError::Forbidden) => {
            tracing::info!(event = "update_checksum_fallback", "GitHub API 受限，读取官方发布校验清单");
            let bytes = http_get(
                &format!("https://github.com/{repository}/releases/latest/download/SHA256SUMS.txt"),
                RELEASE_API_LIMIT,
            )?;
            let release = release_from_checksums(&repository, &bytes)?;
            return Ok(if is_newer(&release.version, env!("CARGO_PKG_VERSION")) {
                UpdateStatus::Available(release)
            } else {
                UpdateStatus::Current
            });
        }
        Err(error) => return Err(error),
    };
    let release: GithubRelease =
        serde_json::from_slice(&bytes).map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
    let version = release.tag_name.trim_start_matches(['v', 'V']).to_owned();
    if !is_newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(UpdateStatus::Current);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name.ends_with(UPDATE_ASSET_SUFFIX))
        .ok_or(UpdateError::AssetMissing)?;
    let digest = asset.digest.as_deref().ok_or(UpdateError::DigestMissing)?;
    let sha256 = parse_sha256_digest(digest).ok_or(UpdateError::DigestMissing)?;
    Ok(UpdateStatus::Available(ReleaseInfo {
        tag: release.tag_name,
        version,
        asset_name: asset.name,
        download_url: asset.browser_download_url,
        sha256,
        notes: release.body.filter(|body| !body.trim().is_empty()),
    }))
}

// 官方发布工作流生成的 HTTPS 校验清单；固定资产和版本，仍强制验证摘要。
fn release_from_checksums(repository: &str, bytes: &[u8]) -> Result<ReleaseInfo, UpdateError> {
    let text = std::str::from_utf8(bytes).map_err(|_| UpdateError::InvalidManifest("校验清单编码无效".into()))?;
    let mut found = None;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 {
            continue;
        }
        let name = fields[1].trim_start_matches('*');
        let Some(version) = name.strip_prefix("codex-taskbar-").and_then(|name| name.strip_suffix(UPDATE_ASSET_SUFFIX))
        else {
            continue;
        };
        let parts: Vec<_> = version.split('.').collect();
        if parts.len() != 3
            || parts.iter().any(|part| {
                part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()) || part.parse::<u64>().is_err()
            })
        {
            return Err(UpdateError::InvalidManifest("校验清单版本无效".into()));
        }
        if found.is_some() {
            return Err(UpdateError::InvalidManifest("校验清单包含重复资产".into()));
        }
        let sha256 = parse_sha256_digest(&format!("sha256:{}", fields[0])).ok_or(UpdateError::DigestMissing)?;
        let tag = format!("v{version}");
        found = Some(ReleaseInfo {
            version: version.into(),
            download_url: format!("https://github.com/{repository}/releases/download/{tag}/{name}"),
            tag,
            asset_name: name.into(),
            sha256,
            notes: None,
        });
    }
    found.ok_or(UpdateError::AssetMissing)
}

pub fn download_and_stage(
    release: &ReleaseInfo,
    data_root: &Path,
    adaptive_chunk_download: bool,
) -> Result<PathBuf, UpdateError> {
    let bytes = download_asset(&release.download_url, UPDATE_LIMIT, adaptive_chunk_download)?;
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != release.sha256 {
        return Err(UpdateError::DigestMismatch);
    }
    let updates = data_root.join("updates");
    std::fs::create_dir_all(&updates)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let staged = updates.join(format!("codex-taskbar-{}-{stamp}.exe", release.version));
    std::fs::write(&staged, bytes)?;
    Ok(staged)
}

pub fn launch_update_helper(staged: &Path) -> Result<(), UpdateError> {
    let target = std::env::current_exe()?;
    Command::new(staged).arg("--apply-update").arg(&target).arg(std::process::id().to_string()).spawn()?;
    Ok(())
}

/// 由已校验的新版临时 exe 执行。等待旧进程退出后替换目标，再启动目标版本。
pub fn apply_staged_update(target: &Path, old_pid: u32) -> Result<(), UpdateError> {
    wait_for_process_exit(old_pid);
    let staged = std::env::current_exe()?;
    install_staged_update(&staged, target, |target| {
        Command::new(target).arg("--cleanup-update").arg(&staged).spawn().map(|_| ())
    })?;
    Ok(())
}

fn install_staged_update(
    staged: &Path,
    target: &Path,
    launch: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let pending = target.with_extension(format!("exe.pending-{stamp}"));
    let backup = target.with_extension(format!("exe.backup-{stamp}"));
    // 先准备完整的新文件，再移动旧版；复制失败不能碰旧版。
    if let Err(error) =
        std::fs::copy(staged, &pending).and_then(|_| std::fs::OpenOptions::new().write(true).open(&pending)?.sync_all())
    {
        let _ = std::fs::remove_file(&pending);
        return Err(error);
    }
    let had_target = target.exists();
    if had_target && let Err(error) = std::fs::rename(target, &backup) {
        let _ = std::fs::remove_file(&pending);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&pending, target).and_then(|_| launch(target)) {
        let _ = std::fs::remove_file(target);
        if had_target {
            std::fs::rename(&backup, target)?;
        }
        let _ = std::fs::remove_file(&pending);
        return Err(error);
    }
    // spawn 成功不等于新版健康；保留旧版备份供启动后故障恢复。
    Ok(())
}

pub fn cleanup_staged_update(staged: &Path) {
    for _ in 0..20 {
        match std::fs::remove_file(staged) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn wait_for_process_exit(pid: u32) {
    if let Ok(process) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        let _ = unsafe { WaitForSingleObject(process, 30_000) } == WAIT_OBJECT_0;
        let _ = unsafe { CloseHandle(process) };
    } else {
        thread::sleep(Duration::from_millis(500));
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u32,
    content_length: Option<u64>,
    rate_limit_remaining: Option<u32>,
    bytes: Vec<u8>,
}

fn response_error(response: &HttpResponse) -> UpdateError {
    let message = serde_json::from_slice::<serde_json::Value>(&response.bytes)
        .ok()
        .and_then(|value| value["message"].as_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    if matches!(response.status, 403 | 429)
        && (response.status == 429
            || response.rate_limit_remaining == Some(0)
            || message.contains("rate limit")
            || message.contains("abuse detection"))
    {
        UpdateError::RateLimited(response.status)
    } else if response.status == 403 {
        UpdateError::Forbidden
    } else {
        UpdateError::HttpStatus(response.status)
    }
}

fn http_get(url: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
    let response = http_request(url, "GET", None, limit)?;
    if !(200..300).contains(&response.status) {
        return Err(response_error(&response));
    }
    Ok(response.bytes)
}

fn download_asset(url: &str, limit: usize, adaptive: bool) -> Result<Vec<u8>, UpdateError> {
    if adaptive
        && let Ok(head) = http_request(url, "HEAD", None, 0)
        && (200..300).contains(&head.status)
        && let Some(length) = head.content_length
        && (PARALLEL_DOWNLOAD_THRESHOLD..=limit as u64).contains(&length)
        && let Ok(bytes) = parallel_range_get(url, length, limit)
    {
        return Ok(bytes);
    }
    http_get(url, limit)
}

fn parallel_range_get(url: &str, length: u64, limit: usize) -> Result<Vec<u8>, UpdateError> {
    let part_size = length.div_ceil(PARALLEL_DOWNLOAD_PARTS);
    let parts = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for part in 0..PARALLEL_DOWNLOAD_PARTS {
            let start = part.saturating_mul(part_size);
            if start >= length {
                break;
            }
            let end = (start + part_size - 1).min(length - 1);
            let header = format!("Range: bytes={start}-{end}\r\n");
            workers.push((
                start,
                end,
                scope.spawn(move || http_request(url, "GET", Some(&header), (end - start + 1) as usize)),
            ));
        }
        workers
            .into_iter()
            .map(|(start, end, worker)| {
                let response = worker.join().map_err(|_| UpdateError::Http("分块下载线程异常结束".into()))??;
                let expected = (end - start + 1) as usize;
                if response.status != 206 || response.bytes.len() != expected {
                    return Err(UpdateError::Http("服务器未按 Range 返回完整分块".into()));
                }
                Ok((start, response.bytes))
            })
            .collect::<Result<Vec<_>, UpdateError>>()
    })?;
    let mut parts = parts;
    parts.sort_by_key(|(start, _)| *start);
    let mut output = Vec::with_capacity(length as usize);
    for (_, bytes) in parts {
        output.extend_from_slice(&bytes);
    }
    if output.len() != length as usize || output.len() > limit {
        return Err(UpdateError::Http("分块合并后的安装包长度不匹配".into()));
    }
    Ok(output)
}

#[derive(Default)]
struct ProxySettings(WINHTTP_CURRENT_USER_IE_PROXY_CONFIG);

impl ProxySettings {
    fn current() -> Self {
        let mut settings = Self::default();
        if unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut settings.0) }.is_err() {
            tracing::warn!(event = "update_proxy_read_failed", "无法读取用户代理，使用 WinHTTP 自动代理");
        }
        settings
    }

    fn uses_static(&self) -> bool {
        !self.0.lpszProxy.is_null() && self.0.lpszAutoConfigUrl.is_null()
    }
}

impl Drop for ProxySettings {
    fn drop(&mut self) {
        for pointer in [self.0.lpszProxy, self.0.lpszProxyBypass, self.0.lpszAutoConfigUrl] {
            if !pointer.is_null() {
                let _ = unsafe { GlobalFree(Some(HGLOBAL(pointer.0.cast()))) };
            }
        }
    }
}

fn http_request(url: &str, method: &str, headers: Option<&str>, limit: usize) -> Result<HttpResponse, UpdateError> {
    http_request_with_proxy(url, method, headers, limit, &ProxySettings::current())
}

fn http_request_with_proxy(
    url: &str,
    method: &str,
    headers: Option<&str>,
    limit: usize,
    proxy: &ProxySettings,
) -> Result<HttpResponse, UpdateError> {
    let (host, port, path) = parse_https_url(url)?;
    let agent = wide("Codex-Taskbar-Updater/1.0");
    let host_wide = wide(&host);
    let path_wide = wide(&path);
    let method_wide = wide(method);
    // 自动代理可能在代理失败后回退直连。用户明确配置静态代理时绑定该代理，
    // 也让设置变更在下一次检查、HEAD 和各下载分块上立即生效。
    let mode = if proxy.uses_static() {
        "system_static"
    } else if !proxy.0.lpszAutoConfigUrl.is_null() {
        "system_pac"
    } else {
        "system_auto"
    };
    tracing::info!(event = "update_request_started", proxy_mode = mode, method, "开始更新请求");
    let session = InternetHandle(unsafe {
        if proxy.uses_static() {
            WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_NAMED_PROXY,
                PCWSTR(proxy.0.lpszProxy.0),
                PCWSTR(proxy.0.lpszProxyBypass.0),
                0,
            )
        } else {
            WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0)
        }
    });
    if session.0.is_null() {
        return Err(UpdateError::Http("无法创建 WinHTTP 会话".into()));
    }
    unsafe { WinHttpSetTimeouts(session.0, 10_000, 15_000, 30_000, 30_000) }
        .map_err(|error| UpdateError::Http(error.to_string()))?;
    let connection = InternetHandle(unsafe { WinHttpConnect(session.0, PCWSTR(host_wide.as_ptr()), port, 0) });
    if connection.0.is_null() {
        return Err(UpdateError::Http("无法连接更新服务器".into()));
    }
    let request = InternetHandle(unsafe {
        WinHttpOpenRequest(
            connection.0,
            PCWSTR(method_wide.as_ptr()),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    });
    if request.0.is_null() {
        return Err(UpdateError::Http("无法创建更新请求".into()));
    }
    let header_wide = headers.map(|headers| headers.encode_utf16().collect::<Vec<_>>());
    unsafe {
        WinHttpSendRequest(request.0, header_wide.as_deref(), None, 0, 0, 0)
            .map_err(|error| UpdateError::Http(error.to_string()))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|error| UpdateError::Http(error.to_string()))?;
    }
    let mut status = 0_u32;
    let mut status_size = std::mem::size_of::<u32>() as u32;
    let mut index = 0_u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut status_size,
            &mut index,
        )
        .map_err(|error| UpdateError::Http(error.to_string()))?;
    }
    let mut content_length_value = 0_u32;
    let mut content_length_size = std::mem::size_of::<u32>() as u32;
    let mut content_length_index = 0_u32;
    let content_length = unsafe {
        WinHttpQueryHeaders(
            request.0,
            windows::Win32::Networking::WinHttp::WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut content_length_value as *mut u32).cast()),
            &mut content_length_size,
            &mut content_length_index,
        )
    }
    .ok()
    .map(|()| u64::from(content_length_value));
    let rate_header = wide("X-RateLimit-Remaining");
    let mut rate_value = 0_u32;
    let mut rate_size = std::mem::size_of::<u32>() as u32;
    let rate_limit_remaining = unsafe {
        WinHttpQueryHeaders(
            request.0,
            windows::Win32::Networking::WinHttp::WINHTTP_QUERY_CUSTOM | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR(rate_header.as_ptr()),
            Some((&mut rate_value as *mut u32).cast()),
            &mut rate_size,
            std::ptr::null_mut(),
        )
    }
    .ok()
    .map(|()| rate_value);
    tracing::info!(event = "update_response", proxy_mode = mode, status, rate_limit_remaining, "收到更新响应");
    let mut output = Vec::new();
    loop {
        let mut available = 0_u32;
        unsafe { WinHttpQueryDataAvailable(request.0, &mut available) }
            .map_err(|error| UpdateError::Http(error.to_string()))?;
        if available == 0 {
            break;
        }
        if output.len().saturating_add(available as usize) > limit {
            return Err(UpdateError::ResponseTooLarge);
        }
        let start = output.len();
        output.resize(start + available as usize, 0);
        let mut read = 0_u32;
        unsafe { WinHttpReadData(request.0, output[start..].as_mut_ptr().cast(), available, &mut read) }
            .map_err(|error| UpdateError::Http(error.to_string()))?;
        output.truncate(start + read as usize);
    }
    Ok(HttpResponse { status, content_length, rate_limit_remaining, bytes: output })
}

fn parse_https_url(url: &str) -> Result<(String, u16, String), UpdateError> {
    let rest = url.strip_prefix("https://").ok_or(UpdateError::InvalidUrl)?;
    let (authority, path) = rest.split_once('/').map_or((rest, "/"), |(host, path)| (host, path));
    if authority.is_empty() || authority.contains(['@', '\\']) {
        return Err(UpdateError::InvalidUrl);
    }
    let (host, port) = authority
        .rsplit_once(':')
        .map(|(host, port)| port.parse::<u16>().map(|port| (host, port)))
        .transpose()
        .map_err(|_| UpdateError::InvalidUrl)?
        .unwrap_or((authority, 443));
    if host.is_empty() {
        return Err(UpdateError::InvalidUrl);
    }
    Ok((host.to_owned(), port, if path == "/" { path.to_owned() } else { format!("/{path}") }))
}

fn normalize_repository(value: &str) -> Result<String, UpdateError> {
    let value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    let value =
        value.strip_prefix("https://github.com/").or_else(|| value.strip_prefix("http://github.com/")).unwrap_or(value);
    let mut parts = value.split('/');
    let owner = parts.next().filter(|part| !part.is_empty()).ok_or(UpdateError::RepositoryInvalid)?;
    let repo = parts.next().filter(|part| !part.is_empty()).ok_or(UpdateError::RepositoryInvalid)?;
    if parts.next().is_some()
        || !owner.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || !repo.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(UpdateError::RepositoryInvalid);
    }
    Ok(format!("{owner}/{repo}"))
}

fn parse_sha256_digest(value: &str) -> Option<[u8; 32]> {
    let hex = value.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0_u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(value: &str) -> Option<Vec<u64>> {
        value.split('.').map(|part| part.split(['-', '+']).next()?.parse().ok()).collect()
    }
    match (parts(candidate), parts(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => candidate != current,
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_build_has_repository_and_overrides_remain_supported() {
        assert_eq!(repository_from(None, None), DEFAULT_REPOSITORY);
        assert_eq!(repository_from(Some(" "), Some("")), DEFAULT_REPOSITORY);
        assert_eq!(repository_from(Some("custom/repo"), Some("build/repo")), "custom/repo");
        assert_eq!(repository_from(None, Some("build/repo")), "build/repo");
    }

    #[test]
    fn distinguishes_github_rate_limits_from_proxy_denials_without_exposing_body() {
        let mut response = HttpResponse {
            status: 403,
            content_length: None,
            rate_limit_remaining: None,
            bytes: br#"{"message":"API rate limit exceeded for private-ip"}"#.to_vec(),
        };
        assert!(matches!(response_error(&response), UpdateError::RateLimited(403)));
        assert!(!response_error(&response).to_string().contains("private-ip"));
        response.bytes = b"<html>Forbidden</html>".to_vec();
        assert!(matches!(response_error(&response), UpdateError::Forbidden));
        response.rate_limit_remaining = Some(0);
        assert!(matches!(response_error(&response), UpdateError::RateLimited(403)));
        response.status = 404;
        assert!(matches!(response_error(&response), UpdateError::HttpStatus(404)));
    }

    #[test]
    #[ignore = "需要外网；只读取公开发布清单，不下载或安装"]
    fn external_latest_release_uses_current_system_proxy() {
        let status = check_latest(configured_repository().as_deref()).unwrap();
        println!("公开更新检查成功：{status:?}");
    }

    #[test]
    #[ignore = "需要外网；下载官方资产到内存并校验，不执行安装包"]
    fn external_release_download_matches_official_checksum() {
        let bytes = http_get(
            &format!("https://github.com/{DEFAULT_REPOSITORY}/releases/latest/download/SHA256SUMS.txt"),
            RELEASE_API_LIMIT,
        )
        .unwrap();
        let release = release_from_checksums(DEFAULT_REPOSITORY, &bytes).unwrap();
        let bytes = download_asset(&release.download_url, UPDATE_LIMIT, true).unwrap();
        assert_eq!(<[u8; 32]>::from(Sha256::digest(&bytes)), release.sha256);
        println!("官方安装包下载及校验成功：{}，{} 字节（未执行）", release.asset_name, bytes.len());
    }

    #[test]
    fn official_checksum_fallback_pins_asset_and_rejects_ambiguous_or_malicious_names() {
        let line = format!("{}  codex-taskbar-0.2.6-windows-x64.exe\n", "ab".repeat(32));
        let release = release_from_checksums(DEFAULT_REPOSITORY, line.as_bytes()).unwrap();
        assert_eq!(release.sha256, [0xab; 32]);
        assert!(release.download_url.ends_with("/releases/download/v0.2.6/codex-taskbar-0.2.6-windows-x64.exe"));
        assert!(release_from_checksums(DEFAULT_REPOSITORY, line.repeat(2).as_bytes()).is_err());
        assert!(release_from_checksums(DEFAULT_REPOSITORY, line.replace("0.2.6", "../../evil").as_bytes()).is_err());
        assert!(release_from_checksums(DEFAULT_REPOSITORY, line.replace("ab", "zz").as_bytes()).is_err());
    }

    #[test]
    fn normalizes_supported_repository_inputs() {
        assert_eq!(normalize_repository("owner/repo").unwrap(), "owner/repo");
        assert_eq!(normalize_repository("https://github.com/owner/repo.git").unwrap(), "owner/repo");
        assert!(normalize_repository("https://example.com/owner/repo").is_err());
    }

    #[test]
    fn compares_release_versions_without_prerelease_suffix() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn parses_only_sha256_digest() {
        let value = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(parse_sha256_digest(&value), Some([0xab; 32]));
        assert_eq!(parse_sha256_digest("sha512:abcd"), None);
        assert_eq!(parse_sha256_digest(&format!("sha256:{}", "中".repeat(20) + "abcd")), None);
    }

    #[test]
    fn parallel_segments_cover_the_file_without_overlap() {
        for length in [PARALLEL_DOWNLOAD_THRESHOLD, PARALLEL_DOWNLOAD_THRESHOLD + 3, 33_554_433] {
            let size = length.div_ceil(PARALLEL_DOWNLOAD_PARTS);
            let ranges = (0..PARALLEL_DOWNLOAD_PARTS)
                .filter_map(|part| {
                    let start = part * size;
                    (start < length).then_some((start, (start + size - 1).min(length - 1)))
                })
                .collect::<Vec<_>>();
            assert_eq!(ranges.first().map(|range| range.0), Some(0));
            assert_eq!(ranges.last().map(|range| range.1), Some(length - 1));
            for pair in ranges.windows(2) {
                assert_eq!(pair[0].1 + 1, pair[1].0);
            }
        }
    }

    /// 手动/CI 网络验收：传入公开、支持 Range 的 HTTPS 资产，验证四段合并后
    /// 的摘要。默认忽略，避免普通单元测试依赖外网。
    #[test]
    #[ignore = "需要 CODEX_TASKBAR_TEST_ASSET_URL/LENGTH/SHA256 与外网"]
    fn external_parallel_download_matches_expected_sha256() {
        let url = std::env::var("CODEX_TASKBAR_TEST_ASSET_URL").expect("缺少测试资产 URL");
        let length = std::env::var("CODEX_TASKBAR_TEST_ASSET_LENGTH")
            .expect("缺少测试资产长度")
            .parse::<u64>()
            .expect("测试资产长度无效");
        let expected = std::env::var("CODEX_TASKBAR_TEST_ASSET_SHA256").expect("缺少测试资产 SHA-256");
        let bytes = parallel_range_get(&url, length, UPDATE_LIMIT).expect("四路 Range 下载失败");
        let actual = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        assert_eq!(actual, expected);
    }
    #[test]
    fn failed_launch_restores_old_executable_and_failed_copy_never_moves_it() {
        let root = std::env::temp_dir().join(format!(
            "taskbar-update-review-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("installed.exe");
        let staged = root.join("new.exe");
        std::fs::write(&target, b"old").unwrap();
        assert!(install_staged_update(&staged, &target, |_| Ok(())).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        std::fs::write(&staged, b"new").unwrap();
        assert!(
            install_staged_update(&staged, &target, |_| Err(std::io::Error::other("injected spawn failure"))).is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        install_staged_update(&staged, &target, |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(
            std::fs::read_dir(&root)
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains("backup-"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
