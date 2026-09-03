//! POSIX 本地时区转换，线程安全且保留夏令时规则。
#[cfg(unix)]
fn local(timestamp: i64) -> Option<libc::tm> {
    let timestamp = libc::time_t::try_from(timestamp).ok()?;
    let mut result = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: 时间输入和输出缓冲区有效；不使用共享静态缓冲区。
    if unsafe { libc::localtime_r(&timestamp, result.as_mut_ptr()) }.is_null() {
        return None;
    }
    Some(unsafe { result.assume_init() })
}

pub fn format_local_unix_time(timestamp: i64) -> Option<String> {
    if timestamp <= 0 {
        return None;
    }
    #[cfg(unix)]
    {
        let t = local(timestamp)?;
        Some(format!("{:04}-{:02}-{:02} {:02}:{:02}", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub fn local_usage_clock() -> (i32, u8) {
    #[cfg(unix)]
    {
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        if let Some(t) = local(now) {
            return ((t.tm_year + 1900) * 10000 + (t.tm_mon + 1) * 100 + t.tm_mday, t.tm_hour as u8);
        }
    }
    (19700101, 0)
}

pub fn local_usage_clock_at(timestamp: i64) -> Option<(i32, u8)> {
    #[cfg(unix)]
    {
        let t = local(timestamp)?;
        Some(((t.tm_year + 1900) * 10000 + (t.tm_mon + 1) * 100 + t.tm_mday, t.tm_hour as u8))
    }
    #[cfg(not(unix))]
    {
        let _ = timestamp;
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_clock_and_precise_timestamp_are_available() {
        assert!(super::local_usage_clock().0 >= 20200101);
        assert_eq!(super::format_local_unix_time(1704067200).unwrap().len(), 16);
        assert_eq!(super::format_local_unix_time(0), None);
    }
}
