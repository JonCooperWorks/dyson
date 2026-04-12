// ===========================================================================
// HardwareProbe — detect local hardware capabilities.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Probes the local machine for OS, GPU, CPU, RAM, and disk information.
//   Produces a `NodeManifest` that gets sent to the swarm hub during
//   registration so the hub can route tasks to the right hardware.
//
// Platform support:
//
//   Linux:
//     GPU  — nvidia-smi
//     CPU  — /proc/cpuinfo
//     RAM  — /proc/meminfo
//     Disk — statvfs
//
//   macOS:
//     GPU  — nvidia-smi, then system_profiler SPDisplaysDataType
//     CPU  — sysctl machdep.cpu.brand_string + hw.ncpu
//     RAM  — sysctl hw.memsize
//     Disk — statvfs
//
// All probes are best-effort.  Failures produce default values — the
// node still registers, the hub can route on capabilities (tool names).
// ===========================================================================

use crate::swarm::types::{CpuInfo, GpuInfo, HardwareInfo, NodeManifest, NodeStatus};

// ---------------------------------------------------------------------------
// HardwareProbe
// ---------------------------------------------------------------------------

pub struct HardwareProbe;

impl HardwareProbe {
    /// Run a full hardware probe and build a manifest.
    pub async fn run(node_name: &str, tool_names: Vec<String>) -> NodeManifest {
        let (gpus, cpus, ram_bytes, disk_free_bytes) = tokio::join!(
            detect_gpus(),
            detect_cpus(),
            detect_ram(),
            detect_disk_free("."),
        );

        NodeManifest {
            node_name: node_name.to_string(),
            os: std::env::consts::OS.to_string(),
            hardware: HardwareInfo {
                cpus,
                gpus,
                ram_bytes,
                disk_free_bytes,
            },
            capabilities: tool_names,
            status: NodeStatus::Idle,
        }
    }

    /// Quick status check (for heartbeats).
    pub async fn quick_status(working_dir: &str) -> HardwareInfo {
        let (gpus, cpus, ram_bytes, disk_free_bytes) = tokio::join!(
            detect_gpus(),
            detect_cpus(),
            detect_ram(),
            detect_disk_free(working_dir),
        );

        HardwareInfo { cpus, gpus, ram_bytes, disk_free_bytes }
    }
}

// ===========================================================================
// Linux
// ===========================================================================

#[cfg(target_os = "linux")]
async fn detect_gpus() -> Vec<GpuInfo> {
    // Prefer the rich info from nvidia-smi when it's available — it gives
    // us a proper model string, VRAM, and driver version.  If no NVIDIA
    // hardware is present, fall back to `lspci` so we still catch AMD,
    // Intel, and integrated GPUs.  Both paths are best-effort: if neither
    // tool is on PATH the node still registers, the hub just sees 0 GPUs.
    let nvidia = detect_nvidia_gpus().await;
    if !nvidia.is_empty() {
        return nvidia;
    }
    detect_linux_lspci_gpus().await
}

#[cfg(target_os = "linux")]
async fn detect_linux_lspci_gpus() -> Vec<GpuInfo> {
    let output = match tokio::process::Command::new("lspci")
        .args(["-mm", "-nn"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    parse_lspci_output(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `lspci -mm -nn` output and extract display-class devices.
///
/// `-mm` emits machine-readable fields: each line is a quoted, space-separated
/// list like:
///
///   01:00.0 "VGA compatible controller [0300]" "NVIDIA Corporation [10de]" \
///       "GA102 [GeForce RTX 3090] [2204]" -ra1 "NVIDIA Corporation [10de]" \
///       "GA102 [2204]"
///
/// We care about classes `03xx` (Display controllers): VGA (0300), XGA (0301),
/// 3D (0302), other display (0380).  lspci doesn't expose VRAM in this mode,
/// so we emit model + vendor and leave vram_bytes=0 / cores=None.
#[cfg(target_os = "linux")]
fn parse_lspci_output(output: &str) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();

    for line in output.lines() {
        let fields = match lspci_split_fields(line) {
            Some(f) if f.len() >= 4 => f,
            _ => continue,
        };

        // fields[0] is the slot (e.g. "01:00.0"), fields[1] is the class
        // string with the class code in brackets, fields[2] is the vendor,
        // fields[3] is the device.
        let class = &fields[1];
        if !lspci_is_display_class(class) {
            continue;
        }

        let vendor = lspci_strip_id(&fields[2]);
        let device = lspci_strip_id(&fields[3]);
        let model = if vendor.is_empty() {
            device
        } else {
            format!("{vendor} {device}")
        };

        gpus.push(GpuInfo {
            model,
            vram_bytes: 0,
            driver: "unknown".into(),
            cores: None,
        });
    }

    gpus
}

/// Split a single `lspci -mm` line into its quoted fields.  The first
/// token (PCI slot) is unquoted; subsequent fields are double-quoted and
/// may contain spaces.
#[cfg(target_os = "linux")]
fn lspci_split_fields(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut fields: Vec<String> = Vec::new();
    let mut chars = line.chars().peekable();

    // First field: slot, unquoted, up to whitespace.
    let mut slot = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            break;
        }
        slot.push(c);
        chars.next();
    }
    fields.push(slot);

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut buf = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                buf.push(c);
            }
            fields.push(buf);
        } else {
            // Unquoted token (e.g. `-ra1` revision); skip to next whitespace.
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                chars.next();
            }
        }
    }

    Some(fields)
}

/// Return true if an lspci class string describes a display controller
/// (PCI class 0x03xx).  The class field looks like `VGA compatible
/// controller [0300]` or `3D controller [0302]`.
#[cfg(target_os = "linux")]
fn lspci_is_display_class(class: &str) -> bool {
    if let (Some(start), Some(end)) = (class.rfind('['), class.rfind(']'))
        && end > start + 1 {
            let code = &class[start + 1..end];
            return code.starts_with("03");
        }
    // Fall back to string match if the class code isn't bracketed.
    let lower = class.to_ascii_lowercase();
    lower.contains("vga") || lower.contains("3d controller") || lower.contains("display")
}

/// Strip the trailing ` [xxxx]` PCI id from a vendor or device string.
#[cfg(target_os = "linux")]
fn lspci_strip_id(field: &str) -> String {
    match field.rfind(" [") {
        Some(idx) => field[..idx].trim().to_string(),
        None => field.trim().to_string(),
    }
}

#[cfg(target_os = "linux")]
async fn detect_cpus() -> Vec<CpuInfo> {
    match tokio::fs::read_to_string("/proc/cpuinfo").await {
        Ok(content) => parse_proc_cpuinfo(&content),
        Err(_) => vec![fallback_cpu()],
    }
}

#[cfg(target_os = "linux")]
async fn detect_ram() -> u64 {
    match tokio::fs::read_to_string("/proc/meminfo").await {
        Ok(content) => parse_proc_meminfo(&content),
        Err(_) => 0,
    }
}

// ===========================================================================
// macOS
// ===========================================================================

#[cfg(target_os = "macos")]
async fn detect_gpus() -> Vec<GpuInfo> {
    let nvidia = detect_nvidia_gpus().await;
    if !nvidia.is_empty() {
        return nvidia;
    }

    // Try PATH first, then the absolute path.  GUI-launched processes on
    // macOS often lack `/usr/sbin` in PATH, which caused GPU detection to
    // silently return an empty vec.
    let stdout = match run_macos_tool(
        &["system_profiler", "/usr/sbin/system_profiler"],
        &["SPDisplaysDataType", "-json"],
    )
    .await
    {
        Some(out) => out,
        None => return Vec::new(),
    };

    parse_macos_gpu_json(&stdout)
}

#[cfg(target_os = "macos")]
async fn detect_cpus() -> Vec<CpuInfo> {
    let model = run_sysctl("machdep.cpu.brand_string").await;
    let logical = run_sysctl("hw.logicalcpu").await.and_then(|s| s.parse::<u32>().ok());
    let ncpu = run_sysctl("hw.ncpu").await.and_then(|s| s.parse::<u32>().ok());

    let cores = logical.or(ncpu).unwrap_or_else(|| fallback_cpu().cores);
    let model = model.unwrap_or_else(|| "unknown".into());

    let physical_cores = run_sysctl("hw.physicalcpu")
        .await
        .and_then(|s| s.parse::<u32>().ok());

    vec![CpuInfo { model, cores, physical_cores }]
}

#[cfg(target_os = "macos")]
async fn detect_ram() -> u64 {
    run_sysctl("hw.memsize")
        .await
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| {
            // Fall back to the libc sysctlbyname in case the CLI isn't on PATH.
            sysctl_memsize_via_libc()
        })
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
async fn run_sysctl(key: &str) -> Option<String> {
    run_macos_tool(&["sysctl", "/usr/sbin/sysctl"], &["-n", key]).await
}

/// Invoke a macOS system tool, trying the bare name (PATH) first and then
/// the absolute path.  Processes launched outside a login shell on macOS
/// often don't have `/usr/sbin` on PATH, so `sysctl` and `system_profiler`
/// would fail with "command not found" — which previously cascaded into
/// silently-wrong hardware reports (CPU=1, GPU=[]).
#[cfg(target_os = "macos")]
async fn run_macos_tool(programs: &[&str], args: &[&str]) -> Option<String> {
    for program in programs {
        let output = match tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
        {
            Ok(o) => o,
            Err(_) => continue,
        };

        if !output.status.success() {
            continue;
        }

        return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    None
}

#[cfg(target_os = "macos")]
fn sysctl_memsize_via_libc() -> Option<u64> {
    use std::ffi::CString;
    use std::mem;

    let name = CString::new("hw.memsize").ok()?;
    let mut value: u64 = 0;
    let mut size = mem::size_of::<u64>();

    let ret = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut u64 as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };

    if ret == 0 { Some(value) } else { None }
}

// ===========================================================================
// Fallback (other platforms)
// ===========================================================================

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn detect_gpus() -> Vec<GpuInfo> {
    detect_nvidia_gpus().await
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn detect_cpus() -> Vec<CpuInfo> {
    vec![fallback_cpu()]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn detect_ram() -> u64 {
    0
}

// ===========================================================================
// Shared helpers
// ===========================================================================

fn fallback_cpu() -> CpuInfo {
    CpuInfo {
        model: "unknown".into(),
        cores: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1),
        physical_cores: None,
    }
}

/// Detect NVIDIA GPUs via nvidia-smi (available on all platforms).
async fn detect_nvidia_gpus() -> Vec<GpuInfo> {
    let output = match tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,driver_version",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    parse_nvidia_smi_output(&String::from_utf8_lossy(&output.stdout))
}

fn parse_nvidia_smi_output(output: &str) -> Vec<GpuInfo> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(3, ',').map(|s| s.trim()).collect();
            if parts.len() < 3 {
                return None;
            }
            let vram_mib: u64 = parts[1].parse().unwrap_or(0);
            Some(GpuInfo {
                model: parts[0].to_string(),
                vram_bytes: vram_mib * 1024 * 1024,
                driver: parts[2].to_string(),
                // nvidia-smi does not report a directly comparable "GPU cores"
                // number — CUDA cores are a different concept, so leave None.
                cores: None,
            })
        })
        .collect()
}

// Linux-only parsers (used by the linux detect_cpus/detect_ram)
#[cfg(target_os = "linux")]
fn parse_proc_cpuinfo(content: &str) -> Vec<CpuInfo> {
    use std::collections::{HashMap, HashSet};

    // Accumulate per-model logical-core counts and the set of unique
    // (physical_id, core_id) pairs we've seen for each model.  The latter
    // gives us the physical-core count after walking the file.
    #[derive(Default)]
    struct ModelAccum {
        logical: u32,
        physical_cores: HashSet<(i32, i32)>,
    }

    let mut models: HashMap<String, ModelAccum> = HashMap::new();

    // /proc/cpuinfo is a sequence of records separated by blank lines; each
    // record is the per-logical-processor state.  Walk record by record so
    // we can join `model name` with its sibling `physical id` and `core id`.
    for record in content.split("\n\n") {
        let mut model: Option<String> = None;
        let mut physical_id: Option<i32> = None;
        let mut core_id: Option<i32> = None;

        for line in record.lines() {
            let (key, value) = match line.split_once(':') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => continue,
            };
            match key {
                "model name" => model = Some(value.to_string()),
                "physical id" => physical_id = value.parse::<i32>().ok(),
                "core id" => core_id = value.parse::<i32>().ok(),
                _ => {}
            }
        }

        let Some(model) = model else { continue };
        let entry = models.entry(model).or_default();
        entry.logical += 1;
        if let (Some(p), Some(c)) = (physical_id, core_id) {
            entry.physical_cores.insert((p, c));
        }
    }

    if models.is_empty() {
        return vec![fallback_cpu()];
    }

    models
        .into_iter()
        .map(|(model, acc)| CpuInfo {
            model,
            cores: acc.logical,
            physical_cores: if acc.physical_cores.is_empty() {
                None
            } else {
                Some(acc.physical_cores.len() as u32)
            },
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_proc_meminfo(content: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let rest = rest.trim();
            if let Some(kb_str) = rest.strip_suffix("kB").or(rest.strip_suffix("KB"))
                && let Ok(kb) = kb_str.trim().parse::<u64>() {
                    return kb * 1024;
                }
        }
    }
    0
}

// macOS-only parsers
#[cfg(target_os = "macos")]
fn parse_macos_gpu_json(json_str: &str) -> Vec<GpuInfo> {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let displays = match parsed.get("SPDisplaysDataType").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };

    displays
        .iter()
        .filter_map(|gpu| {
            let model = gpu
                .get("sppci_model")
                .and_then(|v| v.as_str())
                .or_else(|| gpu.get("_name").and_then(|v| v.as_str()))
                .unwrap_or("unknown")
                .to_string();

            let vram_bytes = extract_macos_vram(gpu);
            let cores = extract_macos_gpu_cores(gpu);

            Some(GpuInfo {
                model,
                vram_bytes,
                driver: "Apple".into(),
                cores,
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn extract_macos_vram(gpu: &serde_json::Value) -> u64 {
    for key in ["_spdisplays_vram", "spdisplays_vram", "sppci_vram"] {
        if let Some(vram_str) = gpu.get(key).and_then(|v| v.as_str()) {
            return parse_macos_vram_string(vram_str);
        }
    }
    0
}

/// Pull the GPU core count out of a `system_profiler SPDisplaysDataType`
/// entry.  Apple Silicon GPUs expose this as `sppci_cores` (usually a
/// string like `"38"`, sometimes a JSON number).  Intel/AMD discrete GPUs
/// don't set this field — return None.
#[cfg(target_os = "macos")]
fn extract_macos_gpu_cores(gpu: &serde_json::Value) -> Option<u32> {
    let value = gpu.get("sppci_cores")?;
    if let Some(n) = value.as_u64() {
        return u32::try_from(n).ok();
    }
    value.as_str()?.trim().parse::<u32>().ok()
}

#[cfg(target_os = "macos")]
fn parse_macos_vram_string(s: &str) -> u64 {
    let s = s.trim();
    if let Some(gb) = s.strip_suffix("GB").or(s.strip_suffix("gb")) {
        gb.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024
    } else if let Some(mb) = s.strip_suffix("MB").or(s.strip_suffix("mb")) {
        mb.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024
    } else {
        s.parse::<u64>().unwrap_or(0)
    }
}

/// Disk free via statvfs (Unix: both Linux and macOS).
async fn detect_disk_free(path: &str) -> u64 {
    #[cfg(unix)]
    {
        disk_free_statvfs(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

#[cfg(unix)]
fn disk_free_statvfs(path: &str) -> u64 {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let c_path = match CString::new(path) {
        Ok(p) => p,
        Err(_) => return 0,
    };

    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };

    if ret != 0 {
        return 0;
    }

    let stat = unsafe { stat.assume_init() };
    stat.f_bavail as u64 * stat.f_frsize as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_is_not_empty() {
        assert!(!std::env::consts::OS.is_empty());
    }

    #[test]
    fn parse_nvidia_smi_single_gpu() {
        let output = "NVIDIA GeForce RTX 4090, 24564, 560.35.03\n";
        let gpus = parse_nvidia_smi_output(output);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].model, "NVIDIA GeForce RTX 4090");
        assert_eq!(gpus[0].vram_bytes, 24564 * 1024 * 1024);
        assert_eq!(gpus[0].driver, "560.35.03");
        assert_eq!(gpus[0].cores, None);
    }

    #[test]
    fn parse_nvidia_smi_multiple_gpus() {
        let output = "\
NVIDIA A100-SXM4-80GB, 81920, 535.129.03
NVIDIA A100-SXM4-80GB, 81920, 535.129.03
";
        let gpus = parse_nvidia_smi_output(output);
        assert_eq!(gpus.len(), 2);
    }

    #[test]
    fn parse_nvidia_smi_empty() {
        assert!(parse_nvidia_smi_output("").is_empty());
    }

    #[test]
    fn parse_nvidia_smi_malformed() {
        assert!(parse_nvidia_smi_output("this is not csv\n").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_cpuinfo_single_model_no_topology() {
        // Older/minimal /proc/cpuinfo with no physical id / core id fields.
        let content = "\
processor\t: 0
model name\t: AMD Ryzen 9 7950X 16-Core Processor
cpu MHz\t\t: 4500.000

processor\t: 1
model name\t: AMD Ryzen 9 7950X 16-Core Processor
cpu MHz\t\t: 4500.000
";
        let cpus = parse_proc_cpuinfo(content);
        assert_eq!(cpus.len(), 1);
        assert_eq!(cpus[0].model, "AMD Ryzen 9 7950X 16-Core Processor");
        assert_eq!(cpus[0].cores, 2);
        assert_eq!(cpus[0].physical_cores, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_cpuinfo_with_smt_topology() {
        // 4 logical processors, 2 physical cores, one socket — classic
        // hyperthreaded configuration.  physical_cores should be 2, cores 4.
        let content = "\
processor\t: 0
physical id\t: 0
core id\t\t: 0
model name\t: Intel(R) Core(TM) i5-8250U CPU @ 1.60GHz

processor\t: 1
physical id\t: 0
core id\t\t: 1
model name\t: Intel(R) Core(TM) i5-8250U CPU @ 1.60GHz

processor\t: 2
physical id\t: 0
core id\t\t: 0
model name\t: Intel(R) Core(TM) i5-8250U CPU @ 1.60GHz

processor\t: 3
physical id\t: 0
core id\t\t: 1
model name\t: Intel(R) Core(TM) i5-8250U CPU @ 1.60GHz
";
        let cpus = parse_proc_cpuinfo(content);
        assert_eq!(cpus.len(), 1);
        assert_eq!(cpus[0].cores, 4);
        assert_eq!(cpus[0].physical_cores, Some(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_cpuinfo_multi_socket_topology() {
        // Two sockets, one core per socket, no SMT — distinct physical ids
        // with the same core id should still count as 2 physical cores.
        let content = "\
processor\t: 0
physical id\t: 0
core id\t\t: 0
model name\t: Xeon

processor\t: 1
physical id\t: 1
core id\t\t: 0
model name\t: Xeon
";
        let cpus = parse_proc_cpuinfo(content);
        assert_eq!(cpus.len(), 1);
        assert_eq!(cpus[0].cores, 2);
        assert_eq!(cpus[0].physical_cores, Some(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_cpuinfo_mixed_models() {
        // Heterogeneous machine (e.g. big.LITTLE) — two models, each
        // reported as its own CpuInfo entry.
        let content = "\
processor\t: 0
physical id\t: 0
core id\t\t: 0
model name\t: Intel Core i7

processor\t: 1
physical id\t: 0
core id\t\t: 1
model name\t: Intel Core i7

processor\t: 2
physical id\t: 1
core id\t\t: 0
model name\t: ARM Cortex-A78
";
        let cpus = parse_proc_cpuinfo(content);
        assert_eq!(cpus.len(), 2);
        let intel = cpus.iter().find(|c| c.model.contains("Intel")).unwrap();
        assert_eq!(intel.cores, 2);
        assert_eq!(intel.physical_cores, Some(2));
        let arm = cpus.iter().find(|c| c.model.contains("ARM")).unwrap();
        assert_eq!(arm.cores, 1);
        assert_eq!(arm.physical_cores, Some(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_cpuinfo_empty() {
        let cpus = parse_proc_cpuinfo("");
        assert_eq!(cpus.len(), 1);
        assert_eq!(cpus[0].model, "unknown");
        assert_eq!(cpus[0].physical_cores, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_meminfo_normal() {
        assert_eq!(
            parse_proc_meminfo("MemTotal:       65536000 kB\nMemFree:  32000000 kB\n"),
            65536000 * 1024,
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_meminfo_missing() {
        assert_eq!(parse_proc_meminfo("SwapTotal:  8192 kB\n"), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_meminfo_empty() {
        assert_eq!(parse_proc_meminfo(""), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_macos_vram_gb() {
        assert_eq!(parse_macos_vram_string("16 GB"), 16 * 1024 * 1024 * 1024);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_macos_vram_mb() {
        assert_eq!(parse_macos_vram_string("8192 MB"), 8192 * 1024 * 1024);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_macos_gpu_json_apple_silicon() {
        let json = r#"{
            "SPDisplaysDataType": [{
                "sppci_model": "Apple M2 Max",
                "sppci_cores": "38",
                "_spdisplays_vram": "96 GB"
            }]
        }"#;
        let gpus = parse_macos_gpu_json(json);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].model, "Apple M2 Max");
        assert_eq!(gpus[0].vram_bytes, 96 * 1024 * 1024 * 1024);
        assert_eq!(gpus[0].cores, Some(38));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_macos_gpu_json_apple_silicon_numeric_cores() {
        // Some system_profiler versions emit sppci_cores as a JSON number.
        let json = r#"{
            "SPDisplaysDataType": [{
                "sppci_model": "Apple M1",
                "sppci_cores": 8,
                "_spdisplays_vram": "8 GB"
            }]
        }"#;
        let gpus = parse_macos_gpu_json(json);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].cores, Some(8));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_macos_gpu_json_no_cores_field() {
        // Discrete / Intel GPUs don't report sppci_cores — should be None,
        // not a misleading 0 or 1.
        let json = r#"{
            "SPDisplaysDataType": [{
                "sppci_model": "Intel Iris Plus Graphics",
                "_spdisplays_vram": "1536 MB"
            }]
        }"#;
        let gpus = parse_macos_gpu_json(json);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].cores, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_macos_gpu_json_empty() {
        assert!(parse_macos_gpu_json("{}").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_lspci_amd_gpu() {
        // `lspci -mm -nn` line for a discrete AMD GPU.
        let line = "0a:00.0 \"VGA compatible controller [0300]\" \
\"Advanced Micro Devices, Inc. [AMD/ATI] [1002]\" \
\"Navi 21 [Radeon RX 6800 XT] [73bf]\" -rc1 \
\"Sapphire Technology Limited [1da2]\" \"Navi 21 [9471]\"";
        let gpus = parse_lspci_output(line);
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].model.contains("Advanced Micro Devices"));
        assert!(gpus[0].model.contains("Navi 21"));
        assert_eq!(gpus[0].vram_bytes, 0);
        assert_eq!(gpus[0].cores, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_lspci_intel_igpu() {
        let line = "00:02.0 \"VGA compatible controller [0300]\" \
\"Intel Corporation [8086]\" \
\"Alder Lake-S GT1 [UHD Graphics 730] [4680]\" -r0c \
\"ASUSTeK Computer Inc. [1043]\" \"Alder Lake-S GT1 [8882]\"";
        let gpus = parse_lspci_output(line);
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].model.contains("Intel Corporation"));
        assert!(gpus[0].model.contains("UHD Graphics"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_lspci_3d_controller_class() {
        // Datacenter NVIDIA cards show up as "3D controller [0302]", not VGA.
        let line = "81:00.0 \"3D controller [0302]\" \"NVIDIA Corporation [10de]\" \
\"GA100 [A100 SXM4 80GB] [20b2]\" -ra1 \
\"NVIDIA Corporation [10de]\" \"GA100 [1463]\"";
        let gpus = parse_lspci_output(line);
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].model.contains("NVIDIA Corporation"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_lspci_skips_non_display_devices() {
        let content = "\
00:00.0 \"Host bridge [0600]\" \"Intel Corporation [8086]\" \"Skylake Host [1904]\"
00:02.0 \"VGA compatible controller [0300]\" \"Intel Corporation [8086]\" \"Iris Plus [9b41]\"
00:1f.3 \"Audio device [0403]\" \"Intel Corporation [8086]\" \"HDA [9dc8]\"
";
        let gpus = parse_lspci_output(content);
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].model.contains("Iris Plus"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_lspci_empty() {
        assert!(parse_lspci_output("").is_empty());
    }
}
