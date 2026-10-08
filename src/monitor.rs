//! System monitor readings, straight from /proc and /sys: no helper process,
//! one read per process, and buffers reused from one sample to the next.
use std::{collections::HashMap, fs, io::Read, path::PathBuf, time::Instant};

#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: i32,
    pub name: String,
    /// Share of one core since the last sample, 1.0 = a full core.
    pub cpu: f32,
    /// Resident memory in bytes.
    pub memory: u64,
    pub threads: u32,
}
#[derive(Clone, Debug, Default)]
pub struct Sample {
    /// Busy share of all cores, then of each, since the last sample.
    pub cpu: f32,
    pub cores: Vec<f32>,
    /// Package temperature in °C.
    pub temperature: Option<f32>,
    pub load: [f32; 3],
    pub uptime: u64,
    pub memory_total: u64,
    pub memory_used: u64,
    pub memory_cached: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// Bytes per second over every interface but loopback.
    pub received: u64,
    pub sent: u64,
    /// Bytes per second over every disk.
    pub read: u64,
    pub written: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    pub procs: Vec<Proc>,
}

/// Keeps the previous counters, so each sample reports rates since the last.
pub struct Sampler {
    at: Instant,
    cpu: Vec<(u64, u64)>,
    procs: HashMap<i32, u64>,
    net: (u64, u64),
    disk: (u64, u64),
    temperature: Option<PathBuf>,
    tick: f32,
    page: u64,
    text: String,
}
impl Default for Sampler {
    fn default() -> Self {
        // SAFETY: sysconf only reads system constants.
        let (tick, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        Self {
            at: Instant::now(),
            cpu: vec![],
            procs: HashMap::new(),
            net: (0, 0),
            disk: (0, 0),
            temperature: temperature_path(),
            tick: tick.max(1) as f32,
            page: page.max(1) as u64,
            text: String::with_capacity(4096),
        }
    }
}
impl Sampler {
    /// Reads the system; the first call only sets the counters rates start from.
    pub fn sample(&mut self) -> Sample {
        let elapsed = self.at.elapsed().as_secs_f32().max(0.001);
        self.at = Instant::now();
        let mut s = Sample::default();
        if self.read("/proc/stat") {
            let times = cpu_times(&self.text);
            let busy = |(b, t): (u64, u64), (pb, pt): (u64, u64)| {
                let total = t.saturating_sub(pt);
                if total == 0 {
                    0.
                } else {
                    b.saturating_sub(pb) as f32 / total as f32
                }
            };
            if times.len() == self.cpu.len() {
                let mut shares = times.iter().zip(&self.cpu).map(|(&n, &p)| busy(n, p));
                s.cpu = shares.next().unwrap_or(0.);
                s.cores = shares.collect();
            } else {
                s.cores = vec![0.; times.len().saturating_sub(1)];
            }
            self.cpu = times;
        }
        if self.read("/proc/meminfo") {
            let m = meminfo(&self.text);
            let get = |k: &str| m.get(k).copied().unwrap_or(0) * 1024;
            s.memory_total = get("MemTotal");
            s.memory_used = s.memory_total.saturating_sub(get("MemAvailable"));
            s.memory_cached = get("Cached") + get("Buffers");
            s.swap_total = get("SwapTotal");
            s.swap_used = s.swap_total.saturating_sub(get("SwapFree"));
        }
        if self.read("/proc/loadavg") {
            for (slot, v) in s.load.iter_mut().zip(self.text.split_whitespace()) {
                *slot = v.parse().unwrap_or(0.);
            }
        }
        if self.read("/proc/uptime") {
            s.uptime = self
                .text
                .split('.')
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
        let rate = |now: u64, before: u64| {
            if before == 0 {
                0
            } else {
                (now.saturating_sub(before) as f32 / elapsed) as u64
            }
        };
        if self.read("/proc/net/dev") {
            let net = net_bytes(&self.text);
            (s.received, s.sent) = (rate(net.0, self.net.0), rate(net.1, self.net.1));
            self.net = net;
        }
        if self.read("/proc/diskstats") {
            let disk = disk_bytes(&self.text, |name| {
                !["loop", "ram", "zram", "dm-", "md"]
                    .iter()
                    .any(|p| name.starts_with(p))
                    && fs::exists(format!("/sys/block/{name}")).unwrap_or(false)
            });
            (s.read, s.written) = (rate(disk.0, self.disk.0), rate(disk.1, self.disk.1));
            self.disk = disk;
        }
        (s.disk_total, s.disk_used) = disk_usage("/");
        if let Some(path) = &self.temperature {
            s.temperature = fs::read_to_string(path)
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .map(|v| v / 1000.);
        }
        s.procs = self.procs(elapsed);
        s
    }
    fn procs(&mut self, elapsed: f32) -> Vec<Proc> {
        let mut procs = Vec::with_capacity(self.procs.len() + 16);
        let mut seen = HashMap::with_capacity(self.procs.len() + 16);
        let Ok(dir) = fs::read_dir("/proc") else {
            return procs;
        };
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                continue;
            };
            // Gone between listing and reading: skip it.
            if !self.read(&format!("/proc/{pid}/stat")) {
                continue;
            }
            let Some(stat) = parse_stat(&self.text) else {
                continue;
            };
            let cpu = self
                .procs
                .get(&pid)
                .map(|&before| stat.ticks.saturating_sub(before) as f32 / self.tick / elapsed)
                .unwrap_or(0.);
            seen.insert(pid, stat.ticks);
            procs.push(Proc {
                pid,
                name: stat.name.to_string(),
                cpu,
                memory: stat.rss * self.page,
                threads: stat.threads,
            });
        }
        self.procs = seen;
        procs
    }
    /// Reads `path` into the reused buffer; false if it could not.
    fn read(&mut self, path: &str) -> bool {
        self.text.clear();
        fs::File::open(path)
            .and_then(|mut f| f.read_to_string(&mut self.text))
            .is_ok()
    }
}

/// (busy, total) jiffies: the whole machine first, then each core.
fn cpu_times(stat: &str) -> Vec<(u64, u64)> {
    stat.lines()
        .take_while(|l| l.starts_with("cpu"))
        .map(|l| {
            let v: Vec<u64> = l
                .split_whitespace()
                .skip(1)
                .filter_map(|n| n.parse().ok())
                .collect();
            let total: u64 = v.iter().take(8).sum();
            // idle and iowait.
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            (total.saturating_sub(idle), total)
        })
        .collect()
}
/// Fields in kB.
fn meminfo(text: &str) -> HashMap<&str, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k, v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}
/// Bytes received and sent by every interface but loopback.
fn net_bytes(text: &str) -> (u64, u64) {
    text.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            if name.trim() == "lo" {
                return None;
            }
            let v: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|n| n.parse().ok())
                .collect();
            Some((*v.first()?, *v.get(8)?))
        })
        .fold((0, 0), |(r, s), (a, b)| (r + a, s + b))
}
/// Bytes read and written by the disks `keep` accepts.
fn disk_bytes(text: &str, keep: impl Fn(&str) -> bool) -> (u64, u64) {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let name = *f.get(2)?;
            if !keep(name) {
                return None;
            }
            let sectors = |i: usize| f.get(i)?.parse::<u64>().ok();
            Some((sectors(5)? * 512, sectors(9)? * 512))
        })
        .fold((0, 0), |(r, w), (a, b)| (r + a, w + b))
}
/// (total, used) bytes of the filesystem holding `path`.
fn disk_usage(path: &str) -> (u64, u64) {
    let Ok(path) = std::ffi::CString::new(path) else {
        return (0, 0);
    };
    // SAFETY: statvfs fills the zeroed struct it is given.
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut v) } != 0 {
        return (0, 0);
    }
    let block = v.f_frsize as u64;
    let total = v.f_blocks as u64 * block;
    (total, total.saturating_sub(v.f_bfree as u64 * block))
}
struct Stat<'a> {
    name: &'a str,
    ticks: u64,
    threads: u32,
    rss: u64,
}
/// `/proc/<pid>/stat`: the name sits in parentheses and may hold spaces and
/// parentheses itself, so the fields are counted from the last `)`.
fn parse_stat(text: &str) -> Option<Stat<'_>> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?;
    // Fields from the state (3rd) on.
    let f: Vec<&str> = text.get(close + 2..)?.split_whitespace().collect();
    let n = |i: usize| f.get(i - 3)?.parse::<u64>().ok();
    Some(Stat {
        name,
        ticks: n(14)? + n(15)?,
        threads: n(20)? as u32,
        rss: n(24)?,
    })
}
/// The CPU package sensor, found once.
fn temperature_path() -> Option<PathBuf> {
    let hwmon = fs::read_dir("/sys/class/hwmon").ok()?;
    hwmon.flatten().map(|e| e.path()).find_map(|dir| {
        let name = fs::read_to_string(dir.join("name")).ok()?;
        ["coretemp", "k10temp", "zenpower", "cpu_thermal"]
            .contains(&name.trim())
            .then(|| dir.join("temp1_input"))
            .filter(|p| p.exists())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_names_may_hold_spaces_and_parentheses() {
        let line = "42 (Web (Content) x) S 1 42 42 0 -1 4194560 100 0 0 0 30 12 0 0 20 0 7 0 900 1000 250 18446744073709551615";
        let s = parse_stat(line).unwrap();
        assert_eq!(s.name, "Web (Content) x");
        assert_eq!((s.ticks, s.threads, s.rss), (42, 7, 250));
    }
    #[test]
    fn counters_parse() {
        let stat = "cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 50 0 50 350 50 0 0 0 0 0\nintr 1\n";
        assert_eq!(cpu_times(stat), vec![(200, 1000), (100, 500)]);
        let dev = "h\nh\n    lo: 9 0 0 0 0 0 0 0 9 0\n  wlan0: 100 0 0 0 0 0 0 0 40 0\n";
        assert_eq!(net_bytes(dev), (100, 40));
        let disks = " 259 0 nvme0n1 1 0 10 0 1 0 4 0\n 259 1 nvme0n1p1 1 0 99 0 1 0 99 0\n";
        assert_eq!(disk_bytes(disks, |n| n == "nvme0n1"), (5120, 2048));
    }
    #[test]
    fn samples_this_machine() {
        let mut sampler = Sampler::default();
        sampler.sample();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let s = sampler.sample();
        assert!(s.memory_total > 0 && !s.cores.is_empty() && !s.procs.is_empty());
        assert!((0. ..=1.).contains(&s.cpu));
    }
}
