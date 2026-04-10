// ===========================================================================
// HardwareProbe — detect local hardware capabilities.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Probes the local machine for GPU, CPU, RAM, and disk information.
//   Produces a `NodeManifest` that gets sent to the swarm hub during
//   registration so the hub can route tasks to the right hardware.
//
// Detection methods:
//
//   GPU  — `nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader`
//          Falls back to empty list if nvidia-smi is not installed.
//
//   CPU  — Parse `/proc/cpuinfo` on Linux.
//          Falls back to `num_cpus` count with "unknown" model.
//
//   RAM  — Parse `/proc/meminfo` on Linux (MemTotal line).
//          Falls back to 0.
//
//   Disk — `statvfs` on the working directory.
//          Falls back to 0.
//
// All probes are best-effort.  Failures are logged and produce
// default values — the node still registers, it just has incomplete
// hardware info.  The hub can still route tasks based on capabilities
// (tool names) even without hardware details.
// ===========================================================================

use std::collections::HashMap;

use crate::swarm::types::{CpuInfo, GpuInfo, HardwareInfo, NodeManifest, NodeStatus};

// ---------------------------------------------------------------------------
// HardwareProbe
// ---------------------------------------------------------------------------

/// Detect local hardware and build a `NodeManifest`.
pub struct HardwareProbe;

impl HardwareProbe {
    /// Run a full hardware probe and build a manifest.
    ///
    /// `node_name` is the human-readable name from config (or hostname).
    /// `tool_names` is the list of tools loaded on this node's agent.
    pub async fn run(node_name: &str, tool_names: Vec<String>) -> NodeManifest {
        let (gpus, cpus, ram_bytes, disk_free_bytes) = tokio::join!(
            detect_gpus(),
            detect_cpus(),
            detect_ram(),
            detect_disk_free("."),
        );

        NodeManifest {
            node_name: node_name.to_string(),
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

    /// Quick status check (for heartbeats).  Only checks dynamic values.
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
// GPU detection
// ---------------------------------------------------------------------------

/// Detect NVIDIA GPUs via nvidia-smi.
async fn detect_gpus() -> Vec<GpuInfo> {
    let output = match tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,driver_version",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                stderr = String::from_utf8_lossy(&o.stderr).as_ref(),
                "nvidia-smi returned non-zero"
            );
            return Vec::new();
        }
        Err(e) => {
            tracing::debug!(error = %e, "nvidia-smi not found or failed to run");
            return Vec::new();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_nvidia_smi_output(&stdout)
}

/// Parse nvidia-smi CSV output into `GpuInfo` structs.
///
/// Expected format (one line per GPU):
///   `NVIDIA GeForce RTX 4090, 24564, 560.35.03`
///   (name, memory.total in MiB, driver_version)
fn parse_nvidia_smi_output(output: &str) -> Vec<GpuInfo> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(3, ',').map(|s| s.trim()).collect();
            if parts.len() < 3 {
                tracing::debug!(line, "unexpected nvidia-smi line format");
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

// ---------------------------------------------------------------------------
// CPU detection
// ---------------------------------------------------------------------------

/// Detect CPUs by reading /proc/cpuinfo (Linux) or falling back to core count.
async fn detect_cpus() -> Vec<CpuInfo> {
    match tokio::fs::read_to_string("/proc/cpuinfo").await {
        Ok(content) => parse_proc_cpuinfo(&content),
        Err(_) => {
            // Fallback: just report the number of logical cores.
            vec![CpuInfo {
                model: "unknown".into(),
                cores: std::thread::available_parallelism()
                    .map(|n| n.get() as u32)
                    .unwrap_or(1),
            }]
        }
    }
}

/// Parse /proc/cpuinfo into deduplicated CPU entries.
///
/// /proc/cpuinfo lists one block per logical core.  We group by model
/// name and count cores per model.
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

// ---------------------------------------------------------------------------
// RAM detection
// ---------------------------------------------------------------------------

/// Detect total RAM by reading /proc/meminfo (Linux).
async fn detect_ram() -> u64 {
    match tokio::fs::read_to_string("/proc/meminfo").await {
        Ok(content) => parse_proc_meminfo(&content),
        Err(_) => 0,
    }
}

/// Parse /proc/meminfo for MemTotal.
///
/// Expected format: `MemTotal:       32768000 kB`
fn parse_proc_meminfo(content: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let rest = rest.trim();
            // Value is in kB, convert to bytes.
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

/// Detect free disk space on the filesystem containing `path`.
async fn detect_disk_free(path: &str) -> u64 {
    // Use statvfs on Unix.
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        let gpus = parse_nvidia_smi_output("");
        assert!(gpus.is_empty());
    }

    #[test]
    fn parse_nvidia_smi_malformed() {
        let output = "this is not csv\n";
        let gpus = parse_nvidia_smi_output(output);
        assert!(gpus.is_empty());
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
        let ram = parse_proc_meminfo("SwapTotal:  8192 kB\n");
        assert_eq!(ram, 0);
    }

    #[test]
    fn parse_meminfo_empty() {
        let ram = parse_proc_meminfo("");
        assert_eq!(ram, 0);
    }
}
