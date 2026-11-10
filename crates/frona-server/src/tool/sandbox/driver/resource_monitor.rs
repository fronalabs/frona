use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use dashmap::DashMap;
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// EWMA factor for CPU samples. Higher = reacts faster to changes,
/// lower = absorbs more transient spikes. 1.0 disables smoothing.
pub const DEFAULT_CPU_SMOOTHING_ALPHA: f64 = 0.1;

#[derive(Debug, Clone)]
pub struct AgentLimits {
    pub cpu_pct: f64,
    pub mem_pct: f64,
}

pub struct TrackedProcess {
    pub agent_id: String,
    pub killed: AtomicBool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub cpu: f64,
    pub mem: f64,
}

impl Usage {
    pub fn new(cpu: f64, mem: f64) -> Self {
        Self { cpu, mem }
    }

    pub fn update(&mut self, sample: Usage, alpha: f64) {
        self.cpu = alpha * sample.cpu + (1.0 - alpha) * self.cpu;
        self.mem = sample.mem;
    }
}

pub struct SystemResourceManager {
    pub max_agent_cpu_pct: f64,
    pub max_agent_memory_pct: f64,
    pub max_total_cpu_pct: f64,
    pub max_total_memory_pct: f64,
    num_cpus: f64,
    agent_limits: DashMap<String, AgentLimits>,
    tracked: DashMap<u32, TrackedProcess>,
    cancel_token: CancellationToken,
}

impl SystemResourceManager {
    pub fn new(
        max_agent_cpu_pct: f64,
        max_agent_memory_pct: f64,
        max_total_cpu_pct: f64,
        max_total_memory_pct: f64,
    ) -> Self {
        let num_cpus = detect_num_cpus();
        Self {
            max_agent_cpu_pct,
            max_agent_memory_pct,
            max_total_cpu_pct,
            max_total_memory_pct,
            num_cpus,
            agent_limits: DashMap::new(),
            tracked: DashMap::new(),
            cancel_token: CancellationToken::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn num_cpus(&self) -> f64 {
        self.num_cpus
    }

    #[cfg(test)]
    pub(crate) fn with_num_cpus(mut self, num_cpus: f64) -> Self {
        assert!(num_cpus > 0.0, "num_cpus must be > 0, got {num_cpus}");
        self.num_cpus = num_cpus;
        self
    }

    pub fn set_agent_limits(&self, agent_id: &str, cpu_pct: Option<f64>, mem_pct: Option<f64>) {
        if cpu_pct.is_some() || mem_pct.is_some() {
            self.agent_limits.insert(
                agent_id.to_string(),
                AgentLimits {
                    cpu_pct: cpu_pct.unwrap_or(self.max_agent_cpu_pct),
                    mem_pct: mem_pct.unwrap_or(self.max_agent_memory_pct),
                },
            );
        }
    }

    pub fn effective_agent_limits(&self, agent_id: &str) -> (f64, f64) {
        match self.agent_limits.get(agent_id) {
            Some(l) => (l.cpu_pct, l.mem_pct),
            None => (self.max_agent_cpu_pct, self.max_agent_memory_pct),
        }
    }

    pub fn register(&self, pid: u32, agent_id: &str) {
        self.tracked.insert(
            pid,
            TrackedProcess {
                agent_id: agent_id.to_string(),
                killed: AtomicBool::new(false),
            },
        );
    }

    pub fn unregister(&self, pid: u32) {
        self.tracked.remove(&pid);
    }

    pub fn is_killed(&self, pid: u32) -> bool {
        self.tracked
            .get(&pid)
            .map(|p| p.killed.load(Relaxed))
            .unwrap_or(false)
    }

    pub fn stop_polling(&self) {
        self.cancel_token.cancel();
    }

    pub fn start_polling(self: &Arc<Self>) -> JoinHandle<()> {
        let mut sys = System::new().expect("initialize system resource monitoring");
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            sys.refresh_memory();
            let mut tracker = UsageTracker::new();
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));

            loop {
                tokio::select! {
                    biased;
                    _ = manager.cancel_token.cancelled() => break,
                    _ = interval.tick() => {
                        tracker.poll_once(&manager, &mut sys);
                    }
                }
            }
        })
    }
}

/// Owned by the polling task. Holds all EWMA-smoothed usage state for both
/// per-agent and global enforcement. There is exactly one writer (the
/// polling task) and one reader (also the polling task) for these fields,
/// so no synchronization primitives are needed inside the tracker.
pub struct UsageTracker {
    alpha: f64,
    agent_usage: HashMap<String, Usage>,
    global_usage: Usage,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self {
            alpha: DEFAULT_CPU_SMOOTHING_ALPHA,
            agent_usage: HashMap::new(),
            global_usage: Usage::default(),
        }
    }

    pub fn with_alpha(mut self, alpha: f64) -> Self {
        assert!(
            alpha > 0.0 && alpha <= 1.0,
            "smoothing alpha must be in (0, 1], got {alpha}"
        );
        self.alpha = alpha;
        self
    }

    pub fn alpha(&self) -> f64 {
        self.alpha
    }

    pub fn global_usage(&self) -> Usage {
        self.global_usage
    }

    #[cfg(test)]
    pub(crate) fn agent_usage_value(&self, agent_id: &str) -> Option<Usage> {
        self.agent_usage.get(agent_id).copied()
    }

    fn update_agent_usage(&mut self, agent_id: &str, sample: Usage) -> Usage {
        let entry = self.agent_usage.entry(agent_id.to_string()).or_default();
        entry.update(sample, self.alpha);
        *entry
    }

    fn reset(&mut self) {
        self.agent_usage.clear();
        self.global_usage = Usage::default();
    }

    pub fn poll_once(&mut self, mgr: &SystemResourceManager, sys: &mut System) {
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());

        if mgr.tracked.is_empty() {
            // No tracked processes: reset smoothed usage so a future burst
            // starts from a clean slate rather than inheriting stale history.
            self.reset();
            return;
        }

        let total_memory = sys.total_memory();

        let mut children_map: HashMap<Pid, Vec<Pid>> = HashMap::new();
        for (pid, process) in sys.processes() {
            if let Some(parent) = process.parent() {
                children_map.entry(parent).or_default().push(*pid);
            }
        }

        let mut per_pid: HashMap<u32, (f64, u64)> = HashMap::new();
        let mut dead_pids: Vec<u32> = Vec::new();

        for entry in mgr.tracked.iter() {
            let pid = *entry.key();
            let sysinfo_pid = Pid::from_u32(pid);

            if sys.process(sysinfo_pid).is_none() {
                dead_pids.push(pid);
                continue;
            }

            if total_memory > 0 {
                let (cpu, mem_bytes) = collect_tree_usage(sysinfo_pid, &children_map, |pid| {
                    sys.process(pid).map(|p| (p.cpu_usage() as f64, p.memory()))
                });
                let normalized_cpu = cpu / mgr.num_cpus;
                per_pid.insert(pid, (normalized_cpu, mem_bytes));
            }
        }

        for pid in &dead_pids {
            mgr.tracked.remove(pid);
        }

        if total_memory == 0 {
            return;
        }

        let mut pid_usage: Vec<(u32, String, f64, f64, u64)> = Vec::new();
        for entry in mgr.tracked.iter() {
            let pid = *entry.key();
            let tracked = entry.value();
            if tracked.killed.load(Relaxed) {
                continue;
            }
            if let Some(&(cpu, mem_bytes)) = per_pid.get(&pid) {
                let mem_pct = (mem_bytes as f64 / total_memory as f64) * 100.0;
                pid_usage.push((pid, tracked.agent_id.clone(), cpu, mem_pct, mem_bytes));
            }
        }

        {
            let live_agents: std::collections::HashSet<String> = pid_usage
                .iter()
                .map(|(_, aid, _, _, _)| aid.clone())
                .collect();
            self.agent_usage.retain(|aid, _| live_agents.contains(aid));
        }

        self.enforce_limits(mgr, &pid_usage);
    }

    /// Single enforcement pass with two phases:
    ///
    /// 1. Per-agent: for each agent, sum its live PIDs' usage, blend
    ///    (CPU smoothed, memory replaced) into the per-agent stored value,
    ///    kill at most one PID if the result exceeds the agent's limit
    ///    (mem-priority). Simultaneously accumulate the raw global total
    ///    so phase 2 doesn't need a second iteration.
    /// 2. Global: blend the accumulated raw total into the global stored
    ///    value (CPU smoothed, memory replaced). Kill at most one PID if
    ///    the result exceeds the global limit (mem-priority).
    fn enforce_limits(
        &mut self,
        mgr: &SystemResourceManager,
        pid_usage: &[(u32, String, f64, f64, u64)],
    ) {
        let mut agent_ids: Vec<String> =
            pid_usage.iter().map(|(_, a, _, _, _)| a.clone()).collect();
        agent_ids.sort();
        agent_ids.dedup();

        // Phase 1 — per-agent enforcement, accumulating raw global total.
        let mut raw_global = Usage::default();
        for agent_id in &agent_ids {
            let (max_cpu, max_mem) = mgr.effective_agent_limits(agent_id);

            let mut agent_total = Usage::default();
            let mut largest_mem_pid: Option<(u32, u64)> = None;
            let mut largest_cpu_pid: Option<(u32, f64)> = None;

            for &(pid, ref aid, cpu, mem_pct, mem_bytes) in pid_usage {
                if aid != agent_id {
                    continue;
                }
                if mgr
                    .tracked
                    .get(&pid)
                    .is_some_and(|t| t.killed.load(Relaxed))
                {
                    continue;
                }
                agent_total.cpu += cpu;
                agent_total.mem += mem_pct;
                raw_global.cpu += cpu;
                raw_global.mem += mem_pct;
                if largest_mem_pid.is_none_or(|(_, prev)| mem_bytes > prev) {
                    largest_mem_pid = Some((pid, mem_bytes));
                }
                if largest_cpu_pid.is_none_or(|(_, prev)| cpu > prev) {
                    largest_cpu_pid = Some((pid, cpu));
                }
            }

            // CPU is EWMA-blended so a single bursty tick (e.g. process
            // spawn) doesn't trigger a kill — sustained pressure does.
            // Memory is replaced raw; it grows monotonically so any
            // crossing of the limit should be acted on immediately.
            let smoothed = self.update_agent_usage(agent_id, agent_total);

            // Memory takes priority over CPU. Kill at most one PID per
            // agent per tick; subsequent polls re-evaluate.
            if smoothed.mem > max_mem
                && let Some((pid, _)) = largest_mem_pid
            {
                tracing::warn!(
                    pid, agent = %agent_id,
                    "Killing process: agent memory smoothed {:.1}% > {max_mem:.1}% (raw {:.1}%)",
                    smoothed.mem, agent_total.mem,
                );
                if let Some(entry) = mgr.tracked.get(&pid) {
                    entry.killed.store(true, Relaxed);
                }
                kill_process(pid);
            } else if smoothed.cpu > max_cpu
                && let Some((pid, _)) = largest_cpu_pid
            {
                tracing::warn!(
                    pid, agent = %agent_id,
                    "Killing process: agent CPU smoothed {:.1}% > {max_cpu:.1}% (raw {:.1}%)",
                    smoothed.cpu, agent_total.cpu,
                );
                if let Some(entry) = mgr.tracked.get(&pid) {
                    entry.killed.store(true, Relaxed);
                }
                kill_process(pid);
            }
        }

        // Phase 2 — global enforcement.
        self.global_usage.update(raw_global, self.alpha);

        if self.global_usage.cpu <= mgr.max_total_cpu_pct
            && self.global_usage.mem <= mgr.max_total_memory_pct
        {
            return;
        }

        let exceeded_cpu = self.global_usage.cpu > mgr.max_total_cpu_pct;
        let reason = if exceeded_cpu {
            format!(
                "tracked CPU smoothed {:.1}% > {:.1}% (raw {:.1}%)",
                self.global_usage.cpu, mgr.max_total_cpu_pct, raw_global.cpu,
            )
        } else {
            format!(
                "tracked memory smoothed {:.1}% > {:.1}% (raw {:.1}%)",
                self.global_usage.mem, mgr.max_total_memory_pct, raw_global.mem,
            )
        };

        let mut candidates: Vec<(u32, String, f64, u64)> = pid_usage
            .iter()
            .filter(|(pid, _, _, _, _)| {
                !mgr.tracked.get(pid).is_some_and(|t| t.killed.load(Relaxed))
            })
            .map(|&(pid, ref aid, cpu, _, mem_bytes)| (pid, aid.clone(), cpu, mem_bytes))
            .collect();

        if exceeded_cpu {
            candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        } else {
            candidates.sort_by_key(|c| std::cmp::Reverse(c.3));
        }

        if let Some((pid, agent_id, _, _)) = candidates.into_iter().next() {
            tracing::warn!(pid, agent = %agent_id, "Killing process: {reason}");
            if let Some(entry) = mgr.tracked.get(&pid) {
                entry.killed.store(true, Relaxed);
            }
            kill_process(pid);
        }
    }
}

impl Default for UsageTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk the tree rooted at `root` via `children_map`, summing
/// `(cpu, memory_bytes)` returned by `lookup` at each visited PID.
///
/// PIDs whose `lookup` returns `None` are skipped (no contribution) but
/// their children are still traversed. This separates the traversal logic
/// from the data source (sysinfo in production, hand-built maps in tests).
fn collect_tree_usage<F>(root: Pid, children_map: &HashMap<Pid, Vec<Pid>>, lookup: F) -> (f64, u64)
where
    F: Fn(Pid) -> Option<(f64, u64)>,
{
    let mut total_cpu = 0.0f64;
    let mut total_mem = 0u64;
    let mut stack = vec![root];

    while let Some(pid) = stack.pop() {
        if let Some((cpu, mem)) = lookup(pid) {
            total_cpu += cpu;
            total_mem += mem;
        }
        if let Some(children) = children_map.get(&pid) {
            stack.extend(children);
        }
    }

    (total_cpu, total_mem)
}

fn detect_num_cpus() -> f64 {
    let mut sys = System::new().expect("initialize CPU detection for resource limits");
    sys.refresh_cpu_all();
    sys.cpus().len().max(1) as f64
}

fn kill_process(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        tracing::warn!("Process kill not supported on this platform");
    }
}

pub fn log_system_resources() {
    use sysinfo::System;
    let cpus = System::physical_core_count().unwrap_or(0);
    let mem_gb = effective_total_memory() as f64 / 1_073_741_824.0;
    tracing::info!("System resources: {cpus} CPUs, {mem_gb:.1} GB memory");
}

fn effective_total_memory() -> u64 {
    let mut sys = System::new().expect("initialize system memory detection");
    sys.refresh_memory();
    sys.cgroup_limits()
        .map(|cg| cg.total_memory)
        .unwrap_or_else(|| sys.total_memory())
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;

    #[test]
    fn test_register_and_unregister() {
        let manager = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0);
        manager.register(1234, "agent_1");
        assert!(manager.tracked.contains_key(&1234));
        manager.unregister(1234);
        assert!(!manager.tracked.contains_key(&1234));
    }

    #[test]
    fn test_is_killed_default_false() {
        let manager = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0);
        manager.register(1234, "agent_1");
        assert!(!manager.is_killed(1234));
    }

    #[test]
    fn test_is_killed_unregistered() {
        let manager = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0);
        assert!(!manager.is_killed(9999));
    }

    #[test]
    fn test_set_agent_limits() {
        let manager = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0);
        manager.set_agent_limits("agent_1", Some(50.0), Some(60.0));

        let limits = manager.agent_limits.get("agent_1").unwrap();
        assert!((limits.cpu_pct - 50.0).abs() < f64::EPSILON);
        assert!((limits.mem_pct - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_set_agent_limits_partial() {
        let manager = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0);
        manager.set_agent_limits("agent_1", Some(50.0), None);

        let limits = manager.agent_limits.get("agent_1").unwrap();
        assert!((limits.cpu_pct - 50.0).abs() < f64::EPSILON);
        assert!((limits.mem_pct - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_set_agent_limits_no_overrides() {
        let manager = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0);
        manager.set_agent_limits("agent_1", None, None);
        assert!(!manager.agent_limits.contains_key("agent_1"));
    }

    #[tokio::test]
    async fn test_stop_polling() {
        let manager = Arc::new(SystemResourceManager::new(80.0, 80.0, 90.0, 90.0));
        let handle = manager.start_polling();
        manager.stop_polling();
        handle.await.unwrap();
    }

    #[test]
    fn test_poll_once_auto_cleans_dead_pids() {
        let manager = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0);
        manager.register(999_999_999, "agent_1");
        assert!(manager.tracked.contains_key(&999_999_999));

        let mut tracker = UsageTracker::new();
        let mut sys = System::new().unwrap();
        tracker.poll_once(&manager, &mut sys);

        assert!(!manager.tracked.contains_key(&999_999_999));
    }

    #[test]
    fn with_num_cpus_overrides_normalization_factor() {
        let m = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0).with_num_cpus(8.0);
        assert_abs_diff_eq!(m.num_cpus(), 8.0, epsilon = 1e-9);
    }

    #[test]
    #[should_panic(expected = "num_cpus must be > 0")]
    fn with_num_cpus_zero_panics() {
        let _ = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0).with_num_cpus(0.0);
    }

    #[test]
    fn usage_smooth_blends_cpu_replaces_mem() {
        let mut u = Usage::new(0.0, 0.0);
        u.update(Usage::new(100.0, 50.0), 0.3);
        // 0.3·100 + 0.7·0 = 30 (cpu smoothed); mem replaced with sample.
        assert_abs_diff_eq!(u.cpu, 30.0, epsilon = 1e-9);
        assert_abs_diff_eq!(u.mem, 50.0, epsilon = 1e-9);

        u.update(Usage::new(100.0, 50.0), 0.3);
        // 0.3·100 + 0.7·30 = 51 (cpu); mem still 50.
        assert_abs_diff_eq!(u.cpu, 51.0, epsilon = 1e-9);
        assert_abs_diff_eq!(u.mem, 50.0, epsilon = 1e-9);
    }

    #[test]
    fn usage_smooth_alpha_one_replaces_state() {
        let mut u = Usage::new(80.0, 60.0);
        u.update(Usage::new(10.0, 5.0), 1.0);
        assert_abs_diff_eq!(u.cpu, 10.0, epsilon = 1e-9);
        assert_abs_diff_eq!(u.mem, 5.0, epsilon = 1e-9);
    }

    #[test]
    fn usage_smooth_with_zero_sample_decays_cpu_zeros_mem() {
        let mut u = Usage::new(100.0, 100.0);
        // Five consecutive zero samples at alpha=0.3:
        //   cpu_n = 0.7^n · 100  (geometric decay)
        //   mem_n = 0            (replaced on first sample)
        for _ in 0..5 {
            u.update(Usage::default(), 0.3);
        }
        let expected_cpu = 100.0 * 0.7f64.powi(5); // ≈ 16.807
        assert_abs_diff_eq!(u.cpu, expected_cpu, epsilon = 1e-9);
        assert_abs_diff_eq!(u.mem, 0.0, epsilon = 1e-9);
    }

    #[test]
    fn tracker_defaults_to_default_smoothing_alpha() {
        let t = UsageTracker::new();
        assert_abs_diff_eq!(t.alpha(), DEFAULT_CPU_SMOOTHING_ALPHA, epsilon = 1e-9);
    }

    #[test]
    fn with_alpha_overrides_smoothing_factor() {
        let t = UsageTracker::new().with_alpha(0.5);
        assert_abs_diff_eq!(t.alpha(), 0.5, epsilon = 1e-9);
    }

    #[test]
    #[should_panic(expected = "smoothing alpha must be in (0, 1]")]
    fn with_alpha_zero_panics() {
        let _ = UsageTracker::new().with_alpha(0.0);
    }

    #[test]
    #[should_panic(expected = "smoothing alpha must be in (0, 1]")]
    fn with_alpha_above_one_panics() {
        let _ = UsageTracker::new().with_alpha(1.5);
    }

    fn children_map(pairs: &[(u32, u32)]) -> HashMap<Pid, Vec<Pid>> {
        let mut m: HashMap<Pid, Vec<Pid>> = HashMap::new();
        for &(parent, child) in pairs {
            m.entry(Pid::from_u32(parent))
                .or_default()
                .push(Pid::from_u32(child));
        }
        m
    }

    fn lookup_from(rows: &[(u32, f64, u64)]) -> impl Fn(Pid) -> Option<(f64, u64)> + '_ {
        let data: HashMap<u32, (f64, u64)> = rows.iter().map(|&(p, c, m)| (p, (c, m))).collect();
        move |pid| {
            let id = pid.as_u32();
            data.get(&id).copied()
        }
    }

    #[test]
    fn collect_tree_usage_returns_zero_when_root_missing_and_no_children() {
        let children = HashMap::new();
        let (cpu, mem) = collect_tree_usage(Pid::from_u32(42), &children, |_| None);
        assert_abs_diff_eq!(cpu, 0.0, epsilon = 1e-9);
        assert_eq!(mem, 0);
    }

    #[test]
    fn collect_tree_usage_returns_only_root_when_no_children() {
        let children = HashMap::new();
        let (cpu, mem) =
            collect_tree_usage(Pid::from_u32(1), &children, lookup_from(&[(1, 50.0, 1024)]));
        assert_abs_diff_eq!(cpu, 50.0, epsilon = 1e-9);
        assert_eq!(mem, 1024);
    }

    #[test]
    fn collect_tree_usage_sums_parent_and_single_child() {
        let children = children_map(&[(1, 2)]);
        let (cpu, mem) = collect_tree_usage(
            Pid::from_u32(1),
            &children,
            lookup_from(&[(1, 30.0, 1000), (2, 70.0, 2000)]),
        );
        assert_abs_diff_eq!(cpu, 100.0, epsilon = 1e-9);
        assert_eq!(mem, 3000);
    }

    #[test]
    fn collect_tree_usage_sums_multiple_children_under_one_parent() {
        let children = children_map(&[(1, 2), (1, 3), (1, 4)]);
        let (cpu, mem) = collect_tree_usage(
            Pid::from_u32(1),
            &children,
            lookup_from(&[
                (1, 10.0, 100),
                (2, 20.0, 200),
                (3, 30.0, 300),
                (4, 40.0, 400),
            ]),
        );
        assert_abs_diff_eq!(cpu, 100.0, epsilon = 1e-9);
        assert_eq!(mem, 1000);
    }

    #[test]
    fn collect_tree_usage_walks_deep_chain() {
        // 1 -> 2 -> 3 -> 4
        let children = children_map(&[(1, 2), (2, 3), (3, 4)]);
        let (cpu, mem) = collect_tree_usage(
            Pid::from_u32(1),
            &children,
            lookup_from(&[(1, 1.0, 10), (2, 2.0, 20), (3, 4.0, 40), (4, 8.0, 80)]),
        );
        assert_abs_diff_eq!(cpu, 15.0, epsilon = 1e-9);
        assert_eq!(mem, 150);
    }

    #[test]
    fn collect_tree_usage_skips_missing_nodes_but_still_visits_descendants() {
        let children = children_map(&[(1, 2)]);
        let (cpu, mem) =
            collect_tree_usage(Pid::from_u32(1), &children, lookup_from(&[(2, 25.0, 500)]));
        assert_abs_diff_eq!(cpu, 25.0, epsilon = 1e-9);
        assert_eq!(mem, 500);
    }

    #[test]
    fn collect_tree_usage_only_visits_descendants_of_root() {
        let children = children_map(&[(1, 2), (99, 100), (99, 101)]);
        let (cpu, mem) = collect_tree_usage(
            Pid::from_u32(1),
            &children,
            lookup_from(&[
                (1, 1.0, 10),
                (2, 2.0, 20),
                (99, 1000.0, 1_000_000),
                (100, 500.0, 500_000),
                (101, 500.0, 500_000),
            ]),
        );
        assert_abs_diff_eq!(cpu, 3.0, epsilon = 1e-9);
        assert_eq!(mem, 30);
    }

    #[test]
    fn collect_tree_usage_sums_branching_subtree() {
        //         1
        //        / \
        //       2   3
        //      / \
        //     4   5
        let children = children_map(&[(1, 2), (1, 3), (2, 4), (2, 5)]);
        let (cpu, mem) = collect_tree_usage(
            Pid::from_u32(1),
            &children,
            lookup_from(&[
                (1, 1.0, 10),
                (2, 2.0, 20),
                (3, 3.0, 30),
                (4, 4.0, 40),
                (5, 5.0, 50),
            ]),
        );
        assert_abs_diff_eq!(cpu, 15.0, epsilon = 1e-9);
        assert_eq!(mem, 150);
    }

    fn spawn_sleep() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        unsafe {
            std::process::Command::new("sleep")
                .arg("60")
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()
                .expect("failed to spawn sleep process")
        }
    }

    fn assert_process_dead(child: &mut std::process::Child) {
        let status = child.wait().expect("failed to wait on child");
        assert!(!status.success(), "process should have been killed");
    }

    #[test]
    fn test_enforce_agent_limits_below_threshold_no_kill() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        // global silenced via 999.0 so phase 2 can't fire.
        let manager = SystemResourceManager::new(80.0, 80.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_1");

        let usage = vec![
            (p1, "agent_1".into(), 30.0, 20.0, 1000u64),
            (p2, "agent_1".into(), 40.0, 25.0, 2000),
        ];

        tracker.enforce_limits(&manager, &usage);

        assert!(!manager.is_killed(p1));
        assert!(!manager.is_killed(p2));

        let _ = c1.kill();
        let _ = c2.kill();
        let _ = c1.wait();
        let _ = c2.wait();
    }

    #[test]
    fn test_enforce_agent_limits_cpu_exceeded_kills_largest() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(80.0, 80.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new().with_alpha(1.0);
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_1");

        // Total CPU = 50 + 40 = 90 > 80 threshold
        let usage = vec![
            (p1, "agent_1".into(), 50.0, 10.0, 1000u64),
            (p2, "agent_1".into(), 40.0, 10.0, 2000),
        ];

        tracker.enforce_limits(&manager, &usage);

        assert!(manager.is_killed(p1));
        assert!(!manager.is_killed(p2));
        assert_process_dead(&mut c1);

        let _ = c2.kill();
        let _ = c2.wait();
    }

    #[test]
    fn test_enforce_agent_limits_memory_exceeded_kills_largest() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(80.0, 80.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new().with_alpha(1.0);
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_1");

        // Total mem = 50 + 40 = 90 > 80 threshold
        let usage = vec![
            (p1, "agent_1".into(), 10.0, 40.0, 4000u64),
            (p2, "agent_1".into(), 10.0, 50.0, 5000),
        ];

        tracker.enforce_limits(&manager, &usage);

        assert!(manager.is_killed(p2));
        assert!(!manager.is_killed(p1));
        assert_process_dead(&mut c2);

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn test_enforce_agent_limits_respects_custom_limits() {
        let mut c1 = spawn_sleep();
        let p1 = c1.id();

        let manager = SystemResourceManager::new(80.0, 80.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new().with_alpha(1.0);
        manager.set_agent_limits("agent_1", Some(30.0), None);
        manager.register(p1, "agent_1");

        // CPU 35 > custom limit 30
        let usage = vec![(p1, "agent_1".into(), 35.0, 10.0, 1000u64)];

        tracker.enforce_limits(&manager, &usage);

        assert!(manager.is_killed(p1));
        assert_process_dead(&mut c1);
    }

    #[test]
    fn test_enforce_agent_limits_isolates_agents() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(80.0, 80.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_2");

        let usage = vec![
            (p1, "agent_1".into(), 70.0, 10.0, 1000u64),
            (p2, "agent_2".into(), 70.0, 10.0, 1000),
        ];

        tracker.enforce_limits(&manager, &usage);

        assert!(!manager.is_killed(p1));
        assert!(!manager.is_killed(p2));

        let _ = c1.kill();
        let _ = c2.kill();
        let _ = c1.wait();
        let _ = c2.wait();
    }

    #[test]
    fn test_enforce_global_limits_below_threshold_no_kill() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(999.0, 999.0, 90.0, 90.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_2");

        let usage = vec![
            (p1, "agent_1".into(), 40.0, 30.0, 3000u64),
            (p2, "agent_2".into(), 40.0, 30.0, 3000),
        ];

        tracker.enforce_limits(&manager, &usage);

        assert!(!manager.is_killed(p1));
        assert!(!manager.is_killed(p2));

        let _ = c1.kill();
        let _ = c2.kill();
        let _ = c1.wait();
        let _ = c2.wait();
    }

    #[test]
    fn test_enforce_global_limits_cpu_exceeded_kills_largest() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(999.0, 999.0, 90.0, 90.0);
        let mut tracker = UsageTracker::new().with_alpha(1.0);
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_2");

        // Total tracked CPU = 60 + 40 = 100 > 90 threshold
        let usage = vec![
            (p1, "agent_1".into(), 60.0, 10.0, 1000u64),
            (p2, "agent_2".into(), 40.0, 10.0, 1000),
        ];

        tracker.enforce_limits(&manager, &usage);

        assert!(manager.is_killed(p1));
        assert!(!manager.is_killed(p2));
        assert_process_dead(&mut c1);

        let _ = c2.kill();
        let _ = c2.wait();
    }

    #[test]
    fn test_enforce_global_limits_memory_exceeded_kills_largest() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(999.0, 999.0, 90.0, 90.0);
        let mut tracker = UsageTracker::new().with_alpha(1.0);
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_2");

        // Total tracked mem = 50 + 50 = 100 > 90 threshold
        let usage = vec![
            (p1, "agent_1".into(), 10.0, 50.0, 5000u64),
            (p2, "agent_2".into(), 10.0, 50.0, 6000),
        ];

        tracker.enforce_limits(&manager, &usage);

        assert!(manager.is_killed(p2));
        assert!(!manager.is_killed(p1));
        assert_process_dead(&mut c2);

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn test_enforce_global_limits_skips_already_killed() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(999.0, 999.0, 90.0, 90.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_2");

        manager
            .tracked
            .get(&p1)
            .unwrap()
            .killed
            .store(true, Relaxed);

        // Only pid 2 counts: CPU 50 < 90 threshold
        let usage = vec![
            (p1, "agent_1".into(), 60.0, 10.0, 1000u64),
            (p2, "agent_2".into(), 50.0, 10.0, 1000),
        ];

        tracker.enforce_limits(&manager, &usage);

        assert!(!manager.is_killed(p2));

        let _ = c1.kill();
        let _ = c2.kill();
        let _ = c1.wait();
        let _ = c2.wait();
    }

    #[test]
    fn test_enforce_global_limits_uses_tracked_not_system_cpu() {
        let mut c1 = spawn_sleep();
        let p1 = c1.id();

        let manager = SystemResourceManager::new(80.0, 80.0, 10.0, 10.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");

        // Tracked CPU is only 5% — under the 10% global threshold
        let usage = vec![(p1, "agent_1".into(), 5.0, 5.0, 500u64)];

        tracker.enforce_limits(&manager, &usage);

        assert!(!manager.is_killed(p1));

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn smoothing_absorbs_single_spike_on_agent() {
        // Brief, very high tick sandwiched between low ticks. With alpha=0.1
        // the EWMA peaks well below the 95% threshold and never kills.
        let mut c1 = spawn_sleep();
        let p1 = c1.id();
        let manager = SystemResourceManager::new(95.0, 95.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");

        let low = vec![(p1, "agent_1".into(), 5.0, 5.0, 1000u64)];
        let spike = vec![(p1, "agent_1".into(), 150.0, 5.0, 1000u64)];

        for _ in 0..5 {
            tracker.enforce_limits(&manager, &low);
        }
        tracker.enforce_limits(&manager, &spike);
        // Pre-spike EWMA was ≈ 5; after spike: 0.1·150 + 0.9·5 ≈ 19.5 < 95
        assert!(!manager.is_killed(p1));

        for _ in 0..10 {
            tracker.enforce_limits(&manager, &low);
        }
        assert!(!manager.is_killed(p1));

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn sustained_pressure_eventually_kills_agent() {
        // Sustained 110% with threshold 95% and alpha=0.1:
        //   ewma_n = 110·(1 − 0.9ⁿ)
        // Crosses 95 when 0.9ⁿ < 15/110 ≈ 0.136 → n ≥ 19.
        let mut c1 = spawn_sleep();
        let p1 = c1.id();
        let manager = SystemResourceManager::new(95.0, 95.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");

        let high = vec![(p1, "agent_1".into(), 110.0, 5.0, 1000u64)];

        let mut killed_at = None;
        for tick in 1..=40 {
            tracker.enforce_limits(&manager, &high);
            if manager.is_killed(p1) {
                killed_at = Some(tick);
                break;
            }
        }
        let n = killed_at.expect("expected agent to be killed under sustained CPU");
        assert!(
            (17..=22).contains(&n),
            "expected kill around tick 19, got {n}"
        );
        assert_process_dead(&mut c1);
    }

    #[test]
    fn agent_smoothed_usage_persists_across_ticks() {
        let mut c1 = spawn_sleep();
        let p1 = c1.id();
        let manager = SystemResourceManager::new(95.0, 95.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");

        let usage = vec![(p1, "agent_1".into(), 50.0, 30.0, 1000u64)];

        let mut expected = Usage::default();
        for _ in 0..4 {
            tracker.enforce_limits(&manager, &usage);
            expected.update(Usage::new(50.0, 30.0), DEFAULT_CPU_SMOOTHING_ALPHA);
        }
        let stored = tracker
            .agent_usage_value("agent_1")
            .expect("agent_usage present");
        assert_abs_diff_eq!(stored.cpu, expected.cpu, epsilon = 1e-9);
        assert_abs_diff_eq!(stored.mem, expected.mem, epsilon = 1e-9);

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn agents_smoothed_usage_is_isolated() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();
        let manager = SystemResourceManager::new(95.0, 95.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_2");

        let usage = vec![
            (p1, "agent_1".into(), 200.0, 5.0, 1000u64),
            (p2, "agent_2".into(), 1.0, 1.0, 1000u64),
        ];
        tracker.enforce_limits(&manager, &usage);

        let a1 = tracker.agent_usage_value("agent_1").unwrap();
        let a2 = tracker.agent_usage_value("agent_2").unwrap();
        assert_abs_diff_eq!(a1.cpu, 20.0, epsilon = 1e-9); // 0.1·200
        assert_abs_diff_eq!(a2.cpu, 0.1, epsilon = 1e-9); // 0.1·1
        assert!(!manager.is_killed(p1));
        assert!(!manager.is_killed(p2));

        let _ = c1.kill();
        let _ = c2.kill();
        let _ = c1.wait();
        let _ = c2.wait();
    }

    #[test]
    fn smoothing_absorbs_single_spike_on_global() {
        let mut c1 = spawn_sleep();
        let p1 = c1.id();
        let manager = SystemResourceManager::new(999.0, 999.0, 95.0, 95.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");

        let low = vec![(p1, "agent_1".into(), 5.0, 5.0, 1000u64)];
        let spike = vec![(p1, "agent_1".into(), 150.0, 5.0, 1000u64)];

        for _ in 0..5 {
            tracker.enforce_limits(&manager, &low);
        }
        tracker.enforce_limits(&manager, &spike);
        assert!(!manager.is_killed(p1));

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn global_smoothed_usage_persists_across_ticks() {
        let mut c1 = spawn_sleep();
        let p1 = c1.id();
        let manager = SystemResourceManager::new(999.0, 999.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");

        let usage = vec![(p1, "agent_1".into(), 50.0, 30.0, 1000u64)];

        let mut expected = Usage::default();
        for _ in 0..4 {
            tracker.enforce_limits(&manager, &usage);
            expected.update(Usage::new(50.0, 30.0), DEFAULT_CPU_SMOOTHING_ALPHA);
        }
        let stored = tracker.global_usage();
        assert_abs_diff_eq!(stored.cpu, expected.cpu, epsilon = 1e-9);
        assert_abs_diff_eq!(stored.mem, expected.mem, epsilon = 1e-9);

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn tracker_reset_clears_smoothed_usage() {
        let manager = SystemResourceManager::new(80.0, 80.0, 90.0, 90.0);
        let mut tracker = UsageTracker::new();
        tracker.global_usage = Usage::new(80.0, 60.0);
        tracker
            .agent_usage
            .insert("agent_1".into(), Usage::new(70.0, 40.0));

        let mut sys = System::new().unwrap();
        tracker.poll_once(&manager, &mut sys);

        assert_eq!(tracker.global_usage(), Usage::default());
        assert!(tracker.agent_usage_value("agent_1").is_none());
    }

    #[test]
    fn mem_kill_takes_priority_over_cpu_kill() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(50.0, 50.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new().with_alpha(1.0);
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_1");

        // p1 is the largest CPU; p2 is the largest mem. Mem priority → p2 dies.
        let usage = vec![
            (p1, "agent_1".into(), 70.0, 30.0, 100u64),
            (p2, "agent_1".into(), 30.0, 35.0, 5000u64),
        ];
        tracker.enforce_limits(&manager, &usage);

        assert!(manager.is_killed(p2));
        assert!(!manager.is_killed(p1));
        assert_process_dead(&mut c2);

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn killed_pid_excluded_from_smoothed_total() {
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(95.0, 95.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new();
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_1");

        manager
            .tracked
            .get(&p1)
            .unwrap()
            .killed
            .store(true, Relaxed);

        let usage = vec![
            (p1, "agent_1".into(), 200.0, 80.0, 100u64),
            (p2, "agent_1".into(), 10.0, 5.0, 100u64),
        ];
        tracker.enforce_limits(&manager, &usage);

        let stored = tracker.agent_usage_value("agent_1").unwrap();
        // Only p2's contribution: cpu = 0.1·10 = 1.0 (smoothed); mem = 5 (raw).
        assert_abs_diff_eq!(stored.cpu, 1.0, epsilon = 1e-9);
        assert_abs_diff_eq!(stored.mem, 5.0, epsilon = 1e-9);

        let _ = c1.kill();
        let _ = c2.kill();
        let _ = c1.wait();
        let _ = c2.wait();
    }

    #[test]
    fn enforce_limits_runs_both_phases_in_one_call() {
        // Two agents, each individually over its per-agent limit. Combined,
        // they also exceed the global limit. With alpha=1.0 we expect three
        // kills in one call: phase 1 kills one PID per agent, phase 2 kills
        // one more (the largest remaining contributor).
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let mut c3 = spawn_sleep();
        let mut c4 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();
        let p3 = c3.id();
        let p4 = c4.id();

        let manager = SystemResourceManager::new(50.0, 99.0, 80.0, 999.0);
        let mut tracker = UsageTracker::new().with_alpha(1.0);
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_1");
        manager.register(p3, "agent_2");
        manager.register(p4, "agent_2");

        // agent_1: 40 + 30 = 70 > 50 (per-agent CPU limit) → kill largest (p1)
        // agent_2: 35 + 25 = 60 > 50                       → kill largest (p3)
        // Raw global CPU = 40+30+35+25 = 130 > 80 → kill largest remaining (p2)
        let usage = vec![
            (p1, "agent_1".into(), 40.0, 10.0, 1000u64),
            (p2, "agent_1".into(), 30.0, 10.0, 1000u64),
            (p3, "agent_2".into(), 35.0, 10.0, 1000u64),
            (p4, "agent_2".into(), 25.0, 10.0, 1000u64),
        ];
        tracker.enforce_limits(&manager, &usage);

        assert!(manager.is_killed(p1), "agent_1 largest");
        assert!(manager.is_killed(p3), "agent_2 largest");
        assert!(manager.is_killed(p2), "phase 2 largest remaining");
        assert!(!manager.is_killed(p4));
        assert_process_dead(&mut c1);
        assert_process_dead(&mut c2);
        assert_process_dead(&mut c3);

        let _ = c4.kill();
        let _ = c4.wait();
    }

    #[test]
    fn enforce_limits_phase2_skips_pids_killed_in_phase1() {
        // The agent's largest PID is also the global's largest PID. Phase 1
        // kills it for breaching the per-agent limit; phase 2 must skip it
        // when picking its own victim.
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();

        let manager = SystemResourceManager::new(50.0, 99.0, 60.0, 999.0);
        let mut tracker = UsageTracker::new().with_alpha(1.0);
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_1");

        // agent_1 total CPU = 60 + 20 = 80 > 50 → kill largest (p1)
        // Phase 2 raw global = 80 > 60 → must NOT pick p1 (already killed),
        //   pick the next largest CPU contributor (p2).
        let usage = vec![
            (p1, "agent_1".into(), 60.0, 10.0, 1000u64),
            (p2, "agent_1".into(), 20.0, 10.0, 1000u64),
        ];
        tracker.enforce_limits(&manager, &usage);

        assert!(manager.is_killed(p1));
        assert!(manager.is_killed(p2));
        assert_process_dead(&mut c1);
        assert_process_dead(&mut c2);
    }

    #[test]
    fn enforce_limits_global_uses_smoothed_not_raw() {
        // A single tick where the raw global total exceeds the global limit,
        // but the smoothed global stays below. No phase 2 kill expected.
        let mut c1 = spawn_sleep();
        let p1 = c1.id();

        let manager = SystemResourceManager::new(999.0, 999.0, 50.0, 999.0);
        let mut tracker = UsageTracker::new(); // default alpha = 0.1
        manager.register(p1, "agent_1");

        // raw_cpu = 150 > 50 limit, but smoothed = 0.1·150 + 0.9·0 = 15 < 50
        let usage = vec![(p1, "agent_1".into(), 150.0, 10.0, 1000u64)];
        tracker.enforce_limits(&manager, &usage);

        assert!(!manager.is_killed(p1), "smoothed global stays below limit");
        let g = tracker.global_usage();
        assert_abs_diff_eq!(g.cpu, 15.0, epsilon = 1e-9);

        let _ = c1.kill();
        let _ = c1.wait();
    }

    #[test]
    fn enforce_limits_accumulates_global_across_agents() {
        // Multiple agents each contributing partial usage. After one call,
        // tracker.global_usage() must equal smoothed(Σ raw, alpha) — i.e.
        // the raw global accumulator added each agent's contribution.
        let mut c1 = spawn_sleep();
        let mut c2 = spawn_sleep();
        let mut c3 = spawn_sleep();
        let p1 = c1.id();
        let p2 = c2.id();
        let p3 = c3.id();

        let manager = SystemResourceManager::new(999.0, 999.0, 999.0, 999.0);
        let mut tracker = UsageTracker::new(); // default alpha = 0.1
        manager.register(p1, "agent_1");
        manager.register(p2, "agent_2");
        manager.register(p3, "agent_3");

        let usage = vec![
            (p1, "agent_1".into(), 10.0, 5.0, 100u64),
            (p2, "agent_2".into(), 20.0, 7.0, 200u64),
            (p3, "agent_3".into(), 30.0, 8.0, 300u64),
        ];
        tracker.enforce_limits(&manager, &usage);

        // raw_cpu = 60, raw_mem = 20. cpu smoothed: 0.1·60 = 6. mem raw: 20.
        let g = tracker.global_usage();
        assert_abs_diff_eq!(g.cpu, 6.0, epsilon = 1e-9);
        assert_abs_diff_eq!(g.mem, 20.0, epsilon = 1e-9);

        let _ = c1.kill();
        let _ = c2.kill();
        let _ = c3.kill();
        let _ = c1.wait();
        let _ = c2.wait();
        let _ = c3.wait();
    }
}
