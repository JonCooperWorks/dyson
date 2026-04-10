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
//     GPU  — system_profiler SPDisplaysDataType
//     CPU  — sysctl machdep.cpu.brand_string + hw.ncpu
//     RAM  — sysctl hw.memsize
//     Disk — statvfs
//
// All probes are best-effort.  Failures produce default values — the
// node still registers, the hub can route on capabilities (tool names).
// ===========================================================================

use std::collections::HashMap;

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
            os: detect_os(),
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

        HardwareInfo {
            cpus,
            gpus,
            ram_bytes,
            disk_free_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// OS detection
// ---------------------------------------------------------------------------

fn detect_os() -> String {
    if cfg!(target_os = "macos") {
        "macos".into()
    } else if cfg!(target_os = "linux") {
        "linux".into()
    } else if cfg!(target_os = "windows") {
        "windows".into()
    } else {
        std::env::consts::OS.to_string()
    }
}

// ---------------------------------------------------------------------------
// GPU detection
// ---------------------------------------------------------------------------

async fn detect_gpus() -> Vec<GpuInfo> {
    // Try NVIDIA first (works on both Linux and macOS with CUDA drivers).
    let nvidia = detect_nvidia_gpus().await;
    if !nvidia.is_empty() {
        return nvidia;
    }

    // On macOS, try system_profiler for Apple Silicon / AMD GPUs.
    #[cfg(target_os = "macos")]
    {
        return detect_macos_gpus().await;
    }

    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

/// Detect NVIDIA GPUs via nvidia-smi.
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_nvidia_smi_output(&stdout)
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
            })
        })
        .collect()
}

/// Detect macOS GPUs via system_profiler.
///
/// On Apple Silicon, the GPU shares unified memory with the CPU.
/// We report the chipset name and total system memory as VRAM since
/// it's all unified.
#[cfg(target_os = "macos")]
async fn detect_macos_gpus() -> Vec<GpuInfo> {
    let output = match tokio::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_macos_gpu_json(&stdout)
}

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
                .unwrap_or("unknown")
                .to_string();

            // Apple Silicon reports VRAM as "_spdisplays_vram" or similar.
            // Try multiple known keys.
            let vram_bytes = extract_macos_vram(gpu);

            Some(GpuInfo {
                model,
                vram_bytes,
                driver: "Apple".into(),
            })
        })
        .collect()
}

/// Extract VRAM from a macOS system_profiler GPU entry.
///
/// Apple Silicon unified memory is reported in various formats
/// across macOS versions. We try known keys and parse the value.
#[cfg(target_os = "macos")]
fn extract_macos_vram(gpu: &serde_json::Value) -> u64 {
    // Try "_spdisplays_vram" (common key).
    for key in ["_spdisplays_vram", "spdisplays_vram", "sppci_vram"] {
        if let Some(vram_str) = gpu.get(key).and_then(|v| v.as_str()) {
            return parse_macos_vram_string(vram_str);
        }
    }
    0
}

/// Parse a macOS VRAM string like "16 GB" or "16384 MB" into bytes.
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

// ---------------------------------------------------------------------------
// CPU detection
// ---------------------------------------------------------------------------

async fn detect_cpus() -> Vec<CpuInfo> {
    // Linux: /proc/cpuinfo
    if let Ok(content) = tokio::fs::read_to_string("/proc/cpuinfo").await {
        return parse_proc_cpuinfo(&content);
    }

    // macOS: sysctl
    #[cfg(target_os = "macos")]
    {
        return detect_macos_cpus().await;
    }

    #[cfg(not(target_os = "macos"))]
    {
        vec![CpuInfo {
            model: "unknown".into(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
        }]
    }
}

fn parse_proc_cpuinfo(content: &str) -> Vec<CpuInfo> {
    let mut model_counts: HashMap<String, u32> = HashMap::new();

    for line in content.lines() {
        if let Some(model) = line.strip_prefix("model name") {
            if let Some(value) = model.split_once(':').map(|(_, v)| v.trim().to_string()) {
                *model_counts.entry(value).or_insert(0) += 1;
            }
        }
    }

    if model_counts.is_empty() {
        return vec![CpuInfo {
            model: "unknown".into(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
        }];
    }

    model_counts
        .into_iter()
        .map(|(model, cores)| CpuInfo { model, cores })
        .collect()
}

#[cfg(target_os = "macos")]
async fn detect_macos_cpus() -> Vec<CpuInfo> {
    let model = run_sysctl("machdep.cpu.brand_string")
        .await
        .unwrap_or_else(|| "unknown".into());

    let cores = run_sysctl("hw.ncpu")
        .await
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1);

    vec![CpuInfo { model, cores }]
}

// ---------------------------------------------------------------------------
// RAM detection
// ---------------------------------------------------------------------------

async fn detect_ram() -> u64 {
    // Linux: /proc/meminfo
    if let Ok(content) = tokio::fs::read_to_string("/proc/meminfo").await {
        return parse_proc_meminfo(&content);
    }

    // macOS: sysctl hw.memsize
    #[cfg(target_os = "macos")]
    {
        return run_sysctl("hw.memsize")
            .await
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
    }

    #[cfg(not(target_os = "macos"))]
    0
}

fn parse_proc_meminfo(content: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let rest = rest.trim();
            if let Some(kb_str) = rest.strip_suffix("kB").or(rest.strip_suffix("KB")) {
                if let Ok(kb) = kb_str.trim().parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Disk detection
// ---------------------------------------------------------------------------

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

    // SAFETY: statvfs writes into the provided buffer.  We pass a valid
    // C string path and an aligned MaybeUninit buffer.
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };

    if ret != 0 {
        return 0;
    }

    // SAFETY: statvfs returned 0, so the buffer is initialized.
    let stat = unsafe { stat.assume_init() };
    stat.f_bavail as u64 * stat.f_frsize as u64
}

// ---------------------------------------------------------------------------
// macOS sysctl helper
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
async fn run_sysctl(key: &str) -> Option<String> {
    let output = tokio::process::Command::new("sysctl")
        .args(["-n", key])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_os_returns_known_value() {
        let os = detect_os();
        assert!(
            ["linux", "macos", "windows"].contains(&os.as_str())
                || !os.is_empty(),
            "unexpected OS: {os}"
        );
    }

    #[test]
    fn parse_nvidia_smi_single_gpu() {
        let output = "NVIDIA GeForce RTX 4090, 24564, 560.35.03\n";
        let gpus = parse_nvidia_smi_output(output);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].model, "NVIDIA GeForce RTX 4090");
        assert_eq!(gpus[0].vram_bytes, 24564 * 1024 * 1024);
        assert_eq!(gpus[0].driver, "560.35.03");
    }

    #[test]
    fn parse_nvidia_smi_multiple_gpus() {
        let output = "\
NVIDIA A100-SXM4-80GB, 81920, 535.129.03
NVIDIA A100-SXM4-80GB, 81920, 535.129.03
";
        let gpus = parse_nvidia_smi_output(output);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].model, "NVIDIA A100-SXM4-80GB");
        assert_eq!(gpus[1].vram_bytes, 81920 * 1024 * 1024);
    }

    #[test]
    fn parse_nvidia_smi_empty() {
        assert!(parse_nvidia_smi_output("").is_empty());
    }

    #[test]
    fn parse_nvidia_smi_malformed() {
        assert!(parse_nvidia_smi_output("this is not csv\n").is_empty());
    }

    #[test]
    fn parse_cpuinfo_single_model() {
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
    }

    #[test]
    fn parse_cpuinfo_mixed_models() {
        let content = "\
processor\t: 0
model name\t: Intel Core i7
processor\t: 1
model name\t: Intel Core i7
processor\t: 2
model name\t: ARM Cortex-A78
";
        let cpus = parse_proc_cpuinfo(content);
        assert_eq!(cpus.len(), 2);

        let intel = cpus.iter().find(|c| c.model.contains("Intel")).unwrap();
        assert_eq!(intel.cores, 2);

        let arm = cpus.iter().find(|c| c.model.contains("ARM")).unwrap();
        assert_eq!(arm.cores, 1);
    }

    #[test]
    fn parse_cpuinfo_empty() {
        let cpus = parse_proc_cpuinfo("");
        assert_eq!(cpus.len(), 1);
        assert_eq!(cpus[0].model, "unknown");
    }

    #[test]
    fn parse_meminfo_normal() {
        let content = "\
MemTotal:       65536000 kB
MemFree:        32000000 kB
MemAvailable:   48000000 kB
";
        let ram = parse_proc_meminfo(content);
        assert_eq!(ram, 65536000 * 1024);
    }

    #[test]
    fn parse_meminfo_missing() {
        assert_eq!(parse_proc_meminfo("SwapTotal:  8192 kB\n"), 0);
    }

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
                "_spdisplays_vram": "96 GB"
            }]
        }"#;
        let gpus = parse_macos_gpu_json(json);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].model, "Apple M2 Max");
        assert_eq!(gpus[0].vram_bytes, 96 * 1024 * 1024 * 1024);
        assert_eq!(gpus[0].driver, "Apple");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_macos_gpu_json_empty() {
        assert!(parse_macos_gpu_json("{}").is_empty());
        assert!(parse_macos_gpu_json("invalid").is_empty());
    }
}
