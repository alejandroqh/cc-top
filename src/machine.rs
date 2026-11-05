//! This machine: load per core, memory, and network traffic, sampled at each refresh.

use std::time::Instant;

use sysinfo::{Networks, System};

pub struct Machine {
    sys: Option<System>, // None where sysinfo can't read this OS
    nets: Option<Networks>,
    sampled: Instant,
    pub cpus: Vec<f64>,     // % busy per core since the last sample
    pub memory: (u64, u64), // used and total bytes
    pub up: Vec<u64>,       // bytes sent per second, one per sample, oldest first
    pub down: Vec<u64>,     // bytes received per second
}

impl Machine {
    pub fn new() -> Self {
        Machine {
            sys: System::new().ok(),
            nets: Networks::new_with_refreshed_list().ok(),
            sampled: Instant::now(),
            cpus: vec![],
            memory: (0, 0),
            up: vec![],
            down: vec![],
        }
    }

    /// Takes a sample, keeping the last `keep` network ones.
    pub fn refresh(&mut self, keep: usize) {
        if let Some(sys) = &mut self.sys {
            sys.refresh_cpu_usage(); // since the last call: the first one reads 0
            sys.refresh_memory();
            self.cpus = sys.cpus().iter().map(|c| f64::from(c.usage())).collect();
            self.memory = (sys.used_memory(), sys.total_memory());
        }
        let secs = self.sampled.elapsed().as_secs_f64().max(0.001); // a click refreshes early
        self.sampled = Instant::now();
        let Some(nets) = &mut self.nets else { return };
        nets.refresh(true);
        // Physical interfaces only: VPN tunnels and container bridges carry traffic that also
        // crosses one of these, and loopback never leaves the machine.
        let (mut up, mut down) = (0, 0);
        for (name, n) in nets.iter() {
            if ["en", "eth", "wl"]
                .iter()
                .any(|p| name.to_string_lossy().starts_with(p))
            {
                up += n.transmitted();
                down += n.received();
            }
        }
        for (history, bytes) in [(&mut self.up, up), (&mut self.down, down)] {
            history.push((bytes as f64 / secs) as u64);
            let extra = history.len().saturating_sub(keep);
            history.drain(..extra);
        }
    }
}
