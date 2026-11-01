//! On-demand host metrics shared by all admin transports and Gateway instances.
//! All sysinfo refreshes and the (possibly contended) collector lock run on Tokio's
//! blocking pool. No background sampler or per-request System is created.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Serialize;
use sysinfo::{
    DiskRefreshKind, Disks, Networks, Pid, ProcessRefreshKind, ProcessesToUpdate, System,
};

use super::AdminService;

const MIN_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const CACHE_TTL: Duration = Duration::from_secs(2);

static COLLECTOR: LazyLock<Mutex<SystemStatusCollector>> =
    LazyLock::new(|| Mutex::new(SystemStatusCollector::new()));

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SystemStatus {
    /// Unix epoch milliseconds at collection, unchanged when served from cache.
    pub sampled_at: i64,
    /// Actual monotonic time between samples; null on the first sample.
    pub sample_interval_seconds: Option<f64>,
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub kernel_version: Option<String>,
    pub architecture: String,
    pub uptime_seconds: u64,
    pub cpu: CpuStatus,
    pub memory: MemoryStatus,
    pub disks: Vec<DiskStatus>,
    /// Includes loopback interfaces; consumers choose which interfaces to aggregate.
    pub networks: Vec<NetworkStatus>,
    /// The running Nyro server/desktop process, not all host processes.
    pub process: ProcessStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CpuStatus {
    pub brand: String,
    pub logical_cores: usize,
    pub physical_cores: Option<usize>,
    /// Host-wide usage in 0..=100; null until two CPU samples are available.
    pub usage_percent: Option<f32>,
    /// Mean of the available, nonzero logical-CPU frequencies.
    pub frequency_mhz: Option<f64>,
    /// Per-logical-core usage in 0..=100. Empty until warmed up (or unavailable),
    /// since the number[] contract cannot represent null entries.
    pub cores: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryStatus {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiskStatus {
    pub name: String,
    pub mount_point: String,
    pub file_system: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub removable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NetworkStatus {
    pub name: String,
    /// OS lifetime byte counters, not sysinfo's since-last-refresh deltas.
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
    /// Null for the first sample, a new interface, or a counter reset.
    pub receive_bytes_per_second: Option<f64>,
    pub transmit_bytes_per_second: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcessStatus {
    pub pid: u32,
    pub memory_bytes: u64,
    /// Normalized by logical core count: 100 means all host logical CPUs busy.
    /// sysinfo's raw process usage may reach 100 * logical_cores. Null until
    /// warmed up, including when the process was missing in the previous sample.
    pub cpu_percent: Option<f32>,
}

impl AdminService {
    /// Shared, two-second cached host metrics for both HTTP and desktop IPC.
    pub async fn get_system_status(&self) -> anyhow::Result<SystemStatus> {
        tokio::task::spawn_blocking(|| {
            let mut collector = COLLECTOR
                .lock()
                .map_err(|_| anyhow::anyhow!("System status collector lock is poisoned"))?;
            Ok(collector.get_status())
        })
        .await
        .context("System status collection task failed")?
    }
}

struct CachedSample {
    at: Instant,
    status: SystemStatus,
}

struct SystemStatusCollector {
    system: System,
    disks: Disks,
    networks: Networks,
    cached: Option<CachedSample>,
    previous_networks: HashMap<String, (u64, u64)>,
    previous_process_start: Option<u64>,
}

impl SystemStatusCollector {
    fn new() -> Self {
        Self {
            system: System::new(),
            disks: Disks::new(),
            networks: Networks::new(),
            cached: None,
            previous_networks: HashMap::new(),
            previous_process_start: None,
        }
    }

    fn get_status(&mut self) -> SystemStatus {
        if let Some(cached) = &self.cached
            && cache_is_fresh(cached.at, Instant::now())
        {
            return cached.status.clone();
        }
        self.sample()
    }

    fn sample(&mut self) -> SystemStatus {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();
        let pid = Pid::from_u32(std::process::id());
        // Do not enumerate every process or collect environment/command-line data.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        self.disks
            .refresh_specifics(true, DiskRefreshKind::nothing().with_storage());
        self.networks.refresh(true);

        let now = Instant::now();
        let interval = sample_interval(self.cached.as_ref().map(|sample| sample.at), now);
        let cpus = self.system.cpus();
        let logical_cores = cpus.len();
        let cpu_ready = sysinfo::IS_SUPPORTED_SYSTEM
            && interval.is_some()
            && logical_cores > 0
            && self
                .cached
                .as_ref()
                .is_some_and(|sample| sample.status.cpu.logical_cores == logical_cores);
        let frequencies: Vec<_> = cpus
            .iter()
            .map(|cpu| cpu.frequency())
            .filter(|mhz| *mhz > 0)
            .collect();
        let frequency_mhz = (!frequencies.is_empty()).then(|| {
            frequencies.iter().map(|mhz| *mhz as f64).sum::<f64>() / frequencies.len() as f64
        });

        let process = self.system.process(pid);
        let process_start = process.map(|process| process.start_time());
        let process_ready =
            cpu_ready && process_start.is_some() && process_start == self.previous_process_start;
        let process_status = ProcessStatus {
            pid: pid.as_u32(),
            memory_bytes: process.map_or(0, |process| process.memory()),
            cpu_percent: process
                .filter(|_| process_ready)
                .and_then(|process| normalized_process_cpu(process.cpu_usage(), logical_cores)),
        };
        self.previous_process_start = process_start;

        let mut disks: Vec<_> = self
            .disks
            .iter()
            .map(|disk| DiskStatus {
                name: disk.name().to_string_lossy().into_owned(),
                mount_point: disk.mount_point().to_string_lossy().into_owned(),
                file_system: disk.file_system().to_string_lossy().into_owned(),
                total_bytes: disk.total_space(),
                available_bytes: disk.available_space(),
                removable: disk.is_removable(),
            })
            .collect();
        disks.sort_by(|a, b| a.mount_point.cmp(&b.mount_point).then(a.name.cmp(&b.name)));

        let mut current_networks = HashMap::new();
        let mut networks: Vec<_> = self
            .networks
            .iter()
            .map(|(name, network)| {
                let received = network.total_received();
                let transmitted = network.total_transmitted();
                let previous = self.previous_networks.get(name);
                current_networks.insert(name.clone(), (received, transmitted));
                NetworkStatus {
                    name: name.clone(),
                    received_bytes: received,
                    transmitted_bytes: transmitted,
                    receive_bytes_per_second: byte_rate(
                        received,
                        previous.map(|totals| totals.0),
                        interval,
                    ),
                    transmit_bytes_per_second: byte_rate(
                        transmitted,
                        previous.map(|totals| totals.1),
                        interval,
                    ),
                }
            })
            .collect();
        networks.sort_by(|a, b| a.name.cmp(&b.name));
        // Replace rather than extend: an interface absent for a sample must warm
        // up again when it returns, rather than reporting its whole lifetime as a burst.
        self.previous_networks = current_networks;

        let status = SystemStatus {
            sampled_at: chrono::Utc::now().timestamp_millis(),
            sample_interval_seconds: interval.map(|interval| interval.as_secs_f64()),
            hostname: System::host_name(),
            os: System::long_os_version(),
            kernel_version: System::kernel_version(),
            architecture: System::cpu_arch(),
            uptime_seconds: System::uptime(),
            cpu: CpuStatus {
                brand: cpus
                    .first()
                    .map(|cpu| cpu.brand())
                    .filter(|brand| !brand.trim().is_empty())
                    .unwrap_or("Unknown")
                    .to_owned(),
                logical_cores,
                physical_cores: self.system.physical_core_count(),
                usage_percent: cpu_ready
                    .then(|| percent(self.system.global_cpu_usage()))
                    .flatten(),
                frequency_mhz,
                cores: if cpu_ready {
                    cpus.iter()
                        .map(|cpu| percent(cpu.cpu_usage()))
                        .collect::<Option<Vec<_>>>()
                        .unwrap_or_default()
                } else {
                    Vec::new()
                },
            },
            memory: MemoryStatus {
                total_bytes: self.system.total_memory(),
                used_bytes: self.system.used_memory(),
                available_bytes: self.system.available_memory(),
                swap_total_bytes: self.system.total_swap(),
                swap_used_bytes: self.system.used_swap(),
            },
            disks,
            networks,
            process: process_status,
        };
        self.cached = Some(CachedSample {
            at: now,
            status: status.clone(),
        });
        status
    }
}

fn cache_is_fresh(sampled_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(sampled_at) < CACHE_TTL
}

fn sample_interval(previous: Option<Instant>, now: Instant) -> Option<Duration> {
    let elapsed = now.checked_duration_since(previous?)?;
    (elapsed >= MIN_SAMPLE_INTERVAL).then_some(elapsed)
}

fn byte_rate(current: u64, previous: Option<u64>, interval: Option<Duration>) -> Option<f64> {
    let interval = interval.filter(|interval| *interval >= MIN_SAMPLE_INTERVAL)?;
    // A decreasing counter means reset/wrap, not zero traffic or a huge burst.
    let delta = current.checked_sub(previous?)?;
    Some(delta as f64 / interval.as_secs_f64())
}

fn percent(value: f32) -> Option<f32> {
    value.is_finite().then(|| value.clamp(0.0, 100.0))
}

fn normalized_process_cpu(raw_usage: f32, logical_cores: usize) -> Option<f32> {
    if logical_cores == 0 {
        return None;
    }
    percent(raw_usage / logical_cores as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_status_first_sample_is_unwarmed_and_cached() {
        let mut collector = SystemStatusCollector::new();
        let first = collector.get_status();
        assert!(first.sampled_at > 0);
        assert_eq!(first.sample_interval_seconds, None);
        assert_eq!(first.cpu.usage_percent, None);
        assert!(first.cpu.cores.is_empty());
        assert_eq!(first.process.cpu_percent, None);
        assert_eq!(first.process.pid, std::process::id());
        assert!(!first.architecture.is_empty());
        assert!(first.networks.iter().all(|network| {
            network.receive_bytes_per_second.is_none()
                && network.transmit_bytes_per_second.is_none()
        }));
        let cached = collector.get_status();
        assert_eq!(first, cached);
        let json = serde_json::to_value(first).unwrap();
        assert!(json["sample_interval_seconds"].is_null());
        assert!(json["cpu"]["usage_percent"].is_null());
        assert!(json["process"]["cpu_percent"].is_null());
        assert_eq!(json["cpu"]["cores"], serde_json::json!([]));
    }

    #[test]
    fn system_status_cache_and_sampling_interval_boundaries() {
        assert!(CACHE_TTL >= MIN_SAMPLE_INTERVAL);
        let start = Instant::now();
        assert!(cache_is_fresh(start, start));
        assert!(cache_is_fresh(
            start,
            start + CACHE_TTL - Duration::from_nanos(1)
        ));
        assert!(!cache_is_fresh(start, start + CACHE_TTL));
        assert_eq!(sample_interval(None, start), None);
        assert_eq!(sample_interval(Some(start), start), None);
        assert_eq!(
            sample_interval(Some(start + Duration::from_secs(1)), start),
            None
        );
        assert_eq!(
            sample_interval(
                Some(start),
                start + MIN_SAMPLE_INTERVAL - Duration::from_nanos(1)
            ),
            None
        );
        assert_eq!(
            sample_interval(Some(start), start + MIN_SAMPLE_INTERVAL),
            Some(MIN_SAMPLE_INTERVAL)
        );
        assert_eq!(
            sample_interval(Some(start), start + Duration::from_millis(2750)),
            Some(Duration::from_millis(2750))
        );
    }

    #[test]
    fn system_status_rates_use_real_elapsed_time_and_handle_counter_boundaries() {
        let elapsed = Some(Duration::from_millis(2500));
        assert_eq!(byte_rate(1_100, Some(100), elapsed), Some(400.0));
        assert_eq!(byte_rate(100, Some(100), elapsed), Some(0.0));
        assert_eq!(byte_rate(u64::MAX, Some(u64::MAX - 5), elapsed), Some(2.0));
        // First/new/reappearing interface, reset/wrap and unprimed intervals.
        assert_eq!(byte_rate(1_000_000, None, elapsed), None);
        assert_eq!(byte_rate(50, Some(100), elapsed), None);
        assert_eq!(byte_rate(0, Some(u64::MAX), elapsed), None);
        assert_eq!(byte_rate(100, Some(0), None), None);
        assert_eq!(byte_rate(100, Some(0), Some(Duration::ZERO)), None);
        assert_eq!(
            byte_rate(100, Some(0), Some(Duration::from_millis(999))),
            None
        );
        assert_eq!(
            byte_rate(100, Some(0), Some(Duration::from_secs(1))),
            Some(100.0)
        );
    }

    #[test]
    fn system_status_process_cpu_is_normalized_and_invalid_values_are_unavailable() {
        assert_eq!(normalized_process_cpu(200.0, 4), Some(50.0));
        assert_eq!(normalized_process_cpu(400.0, 4), Some(100.0));
        assert_eq!(normalized_process_cpu(0.0, 4), Some(0.0));
        assert_eq!(normalized_process_cpu(500.0, 4), Some(100.0));
        assert_eq!(normalized_process_cpu(10.0, 0), None);
        assert_eq!(normalized_process_cpu(f32::NAN, 4), None);
        assert_eq!(percent(f32::INFINITY), None);
        assert_eq!(percent(-5.0), Some(0.0));
    }
}
