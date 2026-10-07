//! `psutil` (7.x): cpu_percent, virtual_memory, disk_usage, pids — psutil's formulas (usage_percent rounded
//! to one decimal, `used` = total - free for root, `free` = available to users), live values.
use std::sync::Arc;

use super::v::*;

/// psutil's `usage_percent(used, total, round_=1)`
fn usage_percent(used: f64, total: f64) -> f64 {
    if total == 0.0 {
        return 0.0;
    }
    (used / total * 100.0 * 10.0).round() / 10.0
}

fn record(name: &'static str, fields: Vec<(&'static str, V)>) -> V {
    V::native(Native::Record(name, Arc::new(fields)))
}

pub async fn call(name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let arg = |i: usize, n: &str| args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v));
    match name {
        "cpu_percent" => {
            if arg(1, "percpu").is_some_and(|v| super::ops::truthy(v).unwrap_or(false)) {
                return Err(Exc::type_error("py2axum: psutil.cpu_percent(percpu=True) is not supported"));
            }
            let interval = match arg(0, "interval") {
                None | Some(V::None) => None,
                Some(V::Int(i)) => Some(*i as f64),
                Some(V::Float(f)) => Some(*f),
                Some(o) => return Err(Exc::type_error(format!("interval is not a number: {}", o.type_name()))),
            };
            let mut sys = sysinfo::System::new();
            sys.refresh_cpu_usage();
            // psutil sleeps (blocking its event loop); the runtime waits without blocking
            let wait = interval.unwrap_or(0.0).max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.as_secs_f64());
            tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
            sys.refresh_cpu_usage();
            Ok(V::Float((sys.global_cpu_usage() as f64 * 10.0).round() / 10.0))
        }
        "virtual_memory" => {
            let (total, avail, used, free) = memory()?;
            Ok(record("svmem", vec![
                ("total", V::Int(total as i64)),
                ("available", V::Int(avail as i64)),
                ("percent", V::Float(usage_percent(total - avail, total))),
                ("used", V::Int(used as i64)),
                ("free", V::Int(free as i64)),
            ]))
        }
        "disk_usage" => {
            let path = super::pathio::fspath(arg(0, "path").ok_or_else(|| Exc::type_error("disk_usage() missing 1 required positional argument: 'path'"))?)?;
            let c = std::ffi::CString::new(path.clone()).map_err(|_| Exc::value_error("embedded null byte"))?;
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
                return Err(Exc::msg(&FILE_NOT_FOUND_ERROR, format!("[Errno 2] No such file or directory: {}", super::ops::repr(&V::str(&path)).unwrap_or_default())));
            }
            let fr = st.f_frsize as f64;
            let total = st.f_blocks as f64 * fr;
            let avail_root = st.f_bfree as f64 * fr;
            let avail_user = st.f_bavail as f64 * fr;
            let used = total - avail_root;
            Ok(record("sdiskusage", vec![
                ("total", V::Int(total as i64)),
                ("used", V::Int(used as i64)),
                ("free", V::Int(avail_user as i64)),
                ("percent", V::Float(usage_percent(used, used + avail_user))),
            ]))
        }
        "pids" => {
            let mut sys = sysinfo::System::new();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let mut pids: Vec<i64> = sys.processes().keys().map(|p| p.as_u32() as i64).collect();
            pids.sort();
            Ok(V::list(pids.into_iter().map(V::Int).collect()))
        }
        _ => Err(Exc::attr_error(format!("module 'psutil' has no attribute '{name}'"))),
    }
}

/// (total, available, used, free) as psutil computes them on Linux: /proc/meminfo, used = total - available
#[cfg(target_os = "linux")]
fn memory() -> R<(f64, f64, f64, f64)> {
    let text = std::fs::read_to_string("/proc/meminfo").map_err(|e| Exc::msg(&OS_ERROR, e.to_string()))?;
    let get = |k: &str| text.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|n| n.parse::<f64>().ok()).map(|n| n * 1024.0);
    let total = get("MemTotal:").ok_or_else(|| Exc::runtime("py2axum: MemTotal missing from /proc/meminfo"))?;
    let free = get("MemFree:").unwrap_or(0.0);
    let mut avail = get("MemAvailable:").filter(|a| *a > 0.0).ok_or_else(|| Exc::runtime("py2axum: MemAvailable missing from /proc/meminfo"))?;
    if avail > total {
        avail = free;
    }
    Ok((total, avail, total - avail, free))
}

/// macOS (development machines), psutil's formulas: available = inactive + free, used = active + wired,
/// free = free - speculative
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn memory() -> R<(f64, f64, f64, f64)> {
    unsafe {
        let mut total: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = std::ffi::CString::new("hw.memsize").unwrap();
        libc::sysctlbyname(name.as_ptr(), &mut total as *mut u64 as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0);
        let mut stats: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        if libc::host_statistics64(libc::mach_host_self(), libc::HOST_VM_INFO64, &mut stats as *mut _ as *mut i32, &mut count) != 0 {
            return Err(Exc::msg(&OS_ERROR, "host_statistics64 failed"));
        }
        let page = libc::sysconf(libc::_SC_PAGESIZE) as f64;
        let p = |n: u32| n as f64 * page;
        let avail = p(stats.inactive_count) + p(stats.free_count);
        let used = p(stats.active_count) + p(stats.wire_count);
        let free = p(stats.free_count) - p(stats.speculative_count);
        Ok((total as f64, avail, used, free))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn memory() -> R<(f64, f64, f64, f64)> {
    Err(Exc::runtime("py2axum: psutil.virtual_memory() is only supported on Linux and macOS"))
}
