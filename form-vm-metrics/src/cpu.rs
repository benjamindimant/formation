use sysinfo::{ProcessesToUpdate, System};
use serde::{Serialize, Deserialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)] 
pub struct CpuMetrics {
    usage_pct: i64,
    process_count: usize
}

impl CpuMetrics {
    pub fn usage_pct(&self) -> i64 {
        self.usage_pct
    }
    
    pub fn process_count(&self) -> usize {
        self.process_count
    }
}

pub async fn collect_cpu(sys: &mut System) -> CpuMetrics {
    sys.refresh_cpu_all();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();
    sys.refresh_cpu_usage();

    let usage = (sys.global_cpu_usage() * 100.0) as i64;
    let count = sys.processes().len();

    CpuMetrics {
        usage_pct: usage,
        process_count: count
    }
}
