//! GitHub Releases 更新检查、下载校验与退出后替换。
//!
//! 网络请求使用 WinHTTP 的 automatic proxy 模式，因此会遵循 Windows 的
//! 系统代理与 PAC/WPAD。只有带 GitHub 生成的 SHA-256 digest 的 exe 资产才会
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
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        Networking::WinHttp::{
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
            WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
            WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
            WinHttpSendRequest,
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
    std::env::var("CODEX_TASKBAR_UPDATE_REPOSITORY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| option_env!("CODEX_TASKBAR_UPDATE_REPOSITORY").map(str::to_owned))
}

pub fn check_latest(repository: Option<&str>) -> Result<UpdateStatus, UpdateError> {
    let repository = normalize_repository(repository.ok_or(UpdateError::RepositoryMissing)?)?;
    let url = format!("https://api.github.com/repos/{repository}/releases/latest");
    let bytes = http_get(&url, RELEASE_API_LIMIT)?;
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
    let backup = target.with_extension("exe.old");
    let _ = std::fs::remove_file(&backup);
    if target.exists() {
        std::fs::rename(target, &backup)?;
    }
    if let Err(error) = std::fs::copy(&staged, target) {
        let _ = std::fs::rename(&backup, target);
        return Err(UpdateError::Io(error));
    }
    let _ = std::fs::remove_file(&backup);
    Command::new(target).arg("--cleanup-update").arg(&staged).spawn()?;
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
    bytes: Vec<u8>,
}

fn http_get(url: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
    let response = http_request(url, "GET", None, limit)?;
    if !(200..300).contains(&response.status) {
        return Err(UpdateError::HttpStatus(response.status));
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

fn http_request(url: &str, method: &str, headers: Option<&str>, limit: usize) -> Result<HttpResponse, UpdateError> {
    let (host, port, path) = parse_https_url(url)?;
    let agent = wide("Codex-Taskbar-Updater/1.0");
    let host_wide = wide(&host);
    let path_wide = wide(&path);
    let method_wide = wide(method);
    let session = InternetHandle(unsafe {
        WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0)
    });
    if session.0.is_null() {
        return Err(UpdateError::Http("无法创建 WinHTTP 会话".into()));
    }
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
    Ok(HttpResponse { status, content_length, bytes: output })
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
    if hex.len() != 64 {
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
}
