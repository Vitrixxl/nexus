//! System monitor readings, straight from /proc and /sys (and nvidia-smi for an
//! NVIDIA card that is awake): no resident helper, one read per file, and the
//! previous sample's buffers refilled rather than reallocated.
use std::{
    collections::HashMap,
    fmt::Write as _,
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub name: String,
    /// Share of one core since the last sample, 1.0 = a full core.
    pub cpu: f32,
    /// Resident memory in bytes.
    pub memory: u64,
    pub threads: u32,
}
#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub name: String,
    /// Busy share, 0..1.
    pub busy: Option<f32>,
    /// Current and highest clock in MHz.
    pub clock: Option<(u32, u32)>,
    /// Video memory used and total, in bytes.
    pub memory: Option<(u64, u64)>,
    pub temperature: Option<f32>,
    pub watts: Option<f32>,
    /// Powered down by the kernel: not read, so as not to wake it.
    pub asleep: bool,
}
/// A disk's throughput, in bytes per second.
#[derive(Clone, Debug, Default)]
pub struct Device {
    pub name: String,
    pub read: u64,
    pub written: u64,
}
#[derive(Clone, Debug, Default)]
pub struct Mount {
    pub path: String,
    pub total: u64,
    pub used: u64,
}
/// A network interface: rates in bytes per second, totals since boot.
#[derive(Clone, Debug, Default)]
pub struct Link {
    pub name: String,
    pub received: u64,
    pub sent: u64,
    pub total_received: u64,
    pub total_sent: u64,
}
#[derive(Clone, Debug, Default)]
pub struct Battery {
    pub percent: f32,
    /// As the kernel says it: Charging, Discharging, Full, Not charging.
    pub status: String,
    /// Draw (discharging) or intake (charging), in watts.
    pub watts: f32,
    /// Seconds until empty, or until full when charging.
    pub seconds: Option<u64>,
    /// Full charge against the design capacity, 0..1.
    pub health: Option<f32>,
    pub cycles: Option<u32>,
}
#[derive(Clone, Debug, Default)]
pub struct Sensor {
    pub name: String,
    pub celsius: f32,
}
#[derive(Clone, Debug, Default)]
pub struct Sample {
    /// Busy share of all cores, then of each, since the last sample.
    pub cpu: f32,
    pub cores: Vec<f32>,
    /// Clock of each core, and their average, in MHz.
    pub core_clocks: Vec<u32>,
    pub clock: u32,
    /// CPU package temperature in °C.
    pub temperature: Option<f32>,
    pub load: [f32; 3],
    pub uptime: u64,
    pub processes: u32,
    pub threads: u32,
    pub memory_total: u64,
    pub memory_available: u64,
    /// Not even holding cache.
    pub memory_free: u64,
    pub memory_used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// zram: bytes stored, and the memory they take compressed.
    pub zram: Option<(u64, u64)>,
    /// Bytes per second over every listed interface.
    pub received: u64,
    pub sent: u64,
    pub links: Vec<Link>,
    /// Bytes per second over every disk.
    pub read: u64,
    pub written: u64,
    pub devices: Vec<Device>,
    pub mounts: Vec<Mount>,
    pub gpus: Vec<Gpu>,
    pub battery: Option<Battery>,
    pub sensors: Vec<Sensor>,
    /// Empty unless asked for: reading every process is the costly part.
    pub procs: Vec<Proc>,
}

#[derive(Clone, Copy, PartialEq)]
enum Vendor {
    Nvidia,
    Intel,
    Amd,
}
struct Card {
    vendor: Vendor,
    dir: PathBuf,
    /// Residency in a power-saving state, ms, and its last reading (Intel).
    idle: Option<PathBuf>,
    idle_before: u64,
}
/// Keeps the previous counters, so each sample reports rates since the last.
pub struct Sampler {
    at: Instant,
    cpu: Vec<(u64, u64)>,
    procs: HashMap<i32, u64>,
    procs_before: HashMap<i32, u64>,
    net: HashMap<String, (u64, u64)>,
    disk: HashMap<String, (u64, u64)>,
    clocks: Vec<PathBuf>,
    sensors: Vec<(String, PathBuf)>,
    cards: Vec<Card>,
    /// The last NVIDIA reading: nvidia-smi is asked every other sample.
    nvidia: Option<Gpu>,
    ticks: u64,
    tick: f32,
    page: u64,
    text: String,
    path: String,
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
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            at: Instant::now(),
            cpu: vec![],
            procs: HashMap::new(),
            procs_before: HashMap::new(),
            net: HashMap::new(),
            disk: HashMap::new(),
            clocks: (0..cpus)
                .map(|i| format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq").into())
                .collect(),
            sensors: sensors(),
            cards: cards(),
            nvidia: None,
            ticks: 0,
            tick: tick.max(1) as f32,
            page: page.max(1) as u64,
            text: String::with_capacity(8192),
            path: String::with_capacity(64),
        }
    }
}
impl Sampler {
    /// Refills `s`; the first call only sets the counters rates start from.
    /// `procs` reads every process too.
    pub fn sample(&mut self, s: &mut Sample, procs: bool) {
        let elapsed = self.at.elapsed().as_secs_f32().max(0.001);
        self.at = Instant::now();
        self.ticks += 1;
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
            s.cores.clear();
            if times.len() == self.cpu.len() {
                let mut shares = times.iter().zip(&self.cpu).map(|(&n, &p)| busy(n, p));
                s.cpu = shares.next().unwrap_or(0.);
                s.cores.extend(shares);
            } else {
                s.cpu = 0.;
                s.cores.resize(times.len().saturating_sub(1), 0.);
            }
            self.cpu = times;
        }
        s.core_clocks.clear();
        for i in 0..self.clocks.len() {
            let khz = read_number(&self.clocks[i], &mut self.text).unwrap_or(0);
            s.core_clocks.push((khz / 1000) as u32);
        }
        s.clock = (s.core_clocks.iter().map(|&c| u64::from(c)).sum::<u64>()
            / s.core_clocks.len().max(1) as u64) as u32;
        if self.read("/proc/meminfo") {
            let m = meminfo(&self.text);
            let get = |k: &str| m.get(k).copied().unwrap_or(0) * 1024;
            s.memory_total = get("MemTotal");
            s.memory_available = get("MemAvailable");
            s.memory_used = s.memory_total.saturating_sub(s.memory_available);
            s.memory_free = get("MemFree");
            s.swap_total = get("SwapTotal");
            s.swap_used = s.swap_total.saturating_sub(get("SwapFree"));
        }
        s.zram = self.read("/sys/block/zram0/mm_stat").then(|| {
            let mut v = self.text.split_whitespace().map(|n| n.parse().unwrap_or(0));
            let stored = v.next().unwrap_or(0);
            // mem_used_total: compressed data plus the allocator's overhead.
            (stored, v.nth(1).unwrap_or(0))
        });
        if self.read("/proc/loadavg") {
            let mut f = self.text.split_whitespace();
            for slot in &mut s.load {
                *slot = f.next().and_then(|v| v.parse().ok()).unwrap_or(0.);
            }
            // "running/total" scheduling entities: every thread.
            s.threads = f
                .next()
                .and_then(|v| v.split_once('/')?.1.parse().ok())
                .unwrap_or(0);
        }
        if self.read("/proc/uptime") {
            s.uptime = self
                .text
                .split('.')
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
        let rate = |now: u64, before: Option<&(u64, u64)>, pick: fn(&(u64, u64)) -> u64| {
            before.map_or(0, |b| (now.saturating_sub(pick(b)) as f32 / elapsed) as u64)
        };
        // ponytail: the per-interface and per-disk lists allocate their few names
        // each second; reuse them if that ever shows in a profile.
        s.links.clear();
        if self.read("/proc/net/dev") {
            for (name, rx, tx) in net_bytes(&self.text) {
                let before = self.net.get(name);
                s.links.push(Link {
                    name: name.to_string(),
                    received: rate(rx, before, |b| b.0),
                    sent: rate(tx, before, |b| b.1),
                    total_received: rx,
                    total_sent: tx,
                });
            }
            self.net.clear();
            for l in &s.links {
                self.net
                    .insert(l.name.clone(), (l.total_received, l.total_sent));
            }
        }
        s.received = s.links.iter().map(|l| l.received).sum();
        s.sent = s.links.iter().map(|l| l.sent).sum();
        s.devices.clear();
        if self.read("/proc/diskstats") {
            let mut now = HashMap::new();
            for (name, read, written) in disk_bytes(&self.text, |name| {
                !["loop", "ram", "zram", "dm-", "md"]
                    .iter()
                    .any(|p| name.starts_with(p))
                    && fs::exists(format!("/sys/block/{name}")).unwrap_or(false)
            }) {
                let before = self.disk.get(name);
                s.devices.push(Device {
                    name: name.to_string(),
                    read: rate(read, before, |b| b.0),
                    written: rate(written, before, |b| b.1),
                });
                now.insert(name.to_string(), (read, written));
            }
            self.disk = now;
        }
        s.read = s.devices.iter().map(|d| d.read).sum();
        s.written = s.devices.iter().map(|d| d.written).sum();
        s.mounts.clear();
        if self.read("/proc/mounts") {
            let mut devices: Vec<&str> = vec![];
            for (device, path) in mounts(&self.text) {
                if devices.contains(&device) {
                    continue;
                }
                devices.push(device);
                let (total, used) = disk_usage(&path);
                if total > 0 {
                    s.mounts.push(Mount { path, total, used });
                }
            }
        }
        s.sensors.clear();
        for (name, path) in &self.sensors {
            if let Some(v) = read_number(path, &mut self.text) {
                s.sensors.push(Sensor {
                    name: name.clone(),
                    celsius: v as f32 / 1000.,
                });
            }
        }
        s.temperature = s
            .sensors
            .iter()
            .find(|t| t.name == "CPU")
            .map(|t| t.celsius);
        self.gpus(s, elapsed);
        s.battery = battery(Path::new("/sys/class/power_supply"));
        if procs {
            self.procs(&mut s.procs, elapsed);
            s.processes = s.procs.len() as u32;
        } else {
            s.processes = fs::read_dir("/proc").map_or(0, |d| {
                d.flatten()
                    .filter(|e| e.file_name().as_encoded_bytes()[0].is_ascii_digit())
                    .count() as u32
            });
            self.procs.clear();
            s.procs.clear();
        }
    }
    fn gpus(&mut self, s: &mut Sample, elapsed: f32) {
        s.gpus.clear();
        for card in &mut self.cards {
            let device = card.dir.join("device");
            match card.vendor {
                Vendor::Nvidia => {
                    let status = fs::read_to_string(device.join("power/runtime_status"));
                    if status.is_ok_and(|s| s.trim() == "suspended") {
                        let name = self.nvidia.as_ref().map(|g| g.name.clone());
                        s.gpus.push(Gpu {
                            name: name.unwrap_or_else(|| "NVIDIA GPU".into()),
                            asleep: true,
                            ..Gpu::default()
                        });
                        continue;
                    }
                    // nvidia-smi costs ~20 ms: every other sample is plenty.
                    if self.nvidia.is_none() || self.ticks % 2 == 1 {
                        self.nvidia = nvidia_smi().or(self.nvidia.take());
                    }
                    if let Some(g) = &self.nvidia {
                        s.gpus.push(g.clone());
                    }
                }
                Vendor::Intel => {
                    let busy = card.idle.as_ref().and_then(|p| {
                        let idle = read_number(p, &mut self.text)?;
                        let before = std::mem::replace(&mut card.idle_before, idle);
                        (before > 0).then(|| {
                            let idle = idle.saturating_sub(before) as f32 / (elapsed * 1000.);
                            (1. - idle).clamp(0., 1.)
                        })
                    });
                    let first = |names: &[&str], text: &mut String| {
                        names
                            .iter()
                            .find_map(|n| read_number(&card.dir.join(n), text))
                    };
                    let now = first(
                        &["gt/gt0/rps_act_freq_mhz", "gt_act_freq_mhz"],
                        &mut self.text,
                    );
                    let max = first(
                        &["gt/gt0/rps_RP0_freq_mhz", "gt_RP0_freq_mhz"],
                        &mut self.text,
                    )
                    .or_else(|| {
                        read_number(&device.join("tile0/gt0/freq0/rp0_freq"), &mut self.text)
                    });
                    let now = now.or_else(|| {
                        read_number(&device.join("tile0/gt0/freq0/act_freq"), &mut self.text)
                    });
                    s.gpus.push(Gpu {
                        name: "Intel Graphics".into(),
                        busy,
                        clock: now.map(|n| (n as u32, max.unwrap_or(n) as u32)),
                        ..Gpu::default()
                    });
                }
                Vendor::Amd => {
                    let n = |file: &str, text: &mut String| read_number(&device.join(file), text);
                    let hwmon = fs::read_dir(device.join("hwmon"))
                        .ok()
                        .and_then(|mut d| d.next()?.ok())
                        .map(|e| e.path());
                    let h = |file: &str, text: &mut String| {
                        hwmon
                            .as_ref()
                            .and_then(|d| read_number(&d.join(file), text))
                    };
                    s.gpus.push(Gpu {
                        name: "AMD Radeon".into(),
                        busy: n("gpu_busy_percent", &mut self.text).map(|v| v as f32 / 100.),
                        memory: n("mem_info_vram_used", &mut self.text)
                            .zip(n("mem_info_vram_total", &mut self.text)),
                        temperature: h("temp1_input", &mut self.text).map(|v| v as f32 / 1000.),
                        watts: h("power1_average", &mut self.text)
                            .or_else(|| h("power1_input", &mut self.text))
                            .map(|v| v as f32 / 1e6),
                        ..Gpu::default()
                    });
                }
            }
        }
    }
    fn procs(&mut self, out: &mut Vec<Proc>, elapsed: f32) {
        std::mem::swap(&mut self.procs, &mut self.procs_before);
        self.procs.clear();
        let Ok(dir) = fs::read_dir("/proc") else {
            out.clear();
            return;
        };
        // Rows are overwritten in place: their names keep their allocations.
        let mut n = 0;
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                continue;
            };
            self.path.clear();
            let _ = write!(self.path, "/proc/{pid}/stat");
            // Gone between listing and reading: skip it.
            let path = std::mem::take(&mut self.path);
            let read = self.read(&path);
            self.path = path;
            if !read {
                continue;
            }
            let Some(stat) = parse_stat(&self.text) else {
                continue;
            };
            let cpu = self
                .procs_before
                .get(&pid)
                .map(|&before| stat.ticks.saturating_sub(before) as f32 / self.tick / elapsed)
                .unwrap_or(0.);
            self.procs.insert(pid, stat.ticks);
            let uid = entry.metadata().map_or(0, |m| m.uid());
            if n == out.len() {
                out.push(Proc::default());
            }
            let p = &mut out[n];
            n += 1;
            p.name.clear();
            p.name.push_str(stat.name);
            (p.pid, p.ppid, p.uid, p.cpu) = (pid, stat.ppid, uid, cpu);
            (p.memory, p.threads) = (stat.rss * self.page, stat.threads);
        }
        out.truncate(n);
    }
    /// Reads `path` into the reused buffer; false if it could not.
    fn read(&mut self, path: &str) -> bool {
        self.text.clear();
        fs::File::open(path)
            .and_then(|mut f| f.read_to_string(&mut self.text))
            .is_ok()
    }
}
fn read_number(path: &Path, text: &mut String) -> Option<u64> {
    text.clear();
    fs::File::open(path).ok()?.read_to_string(text).ok()?;
    text.trim().parse().ok()
}

/// The CPU's marketing name, read once.
pub fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|t| {
            let line = t.lines().find(|l| l.starts_with("model name"))?;
            Some(
                line.split_once(':')?
                    .1
                    .trim()
                    .replace("(R)", "")
                    .replace("(TM)", ""),
            )
        })
        .unwrap_or_else(|| "CPU".into())
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
/// Bytes received and sent by each interface that has carried any, but loopback
/// and containers' virtual links.
fn net_bytes(text: &str) -> impl Iterator<Item = (&str, u64, u64)> {
    text.lines().skip(2).filter_map(|l| {
        let (name, rest) = l.split_once(':')?;
        let name = name.trim();
        if name == "lo"
            || ["veth", "docker", "br-", "virbr"]
                .iter()
                .any(|p| name.starts_with(p))
        {
            return None;
        }
        let mut v = rest
            .split_whitespace()
            .map(|n| n.parse::<u64>().unwrap_or(0));
        let rx = v.next()?;
        let tx = v.nth(7)?;
        (rx + tx > 0).then_some((name, rx, tx))
    })
}
/// Bytes read and written by each disk `keep` accepts.
fn disk_bytes(text: &str, keep: impl Fn(&str) -> bool) -> impl Iterator<Item = (&str, u64, u64)> {
    text.lines().filter_map(move |l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        let name = *f.get(2)?;
        if !keep(name) {
            return None;
        }
        let sectors = |i: usize| f.get(i)?.parse::<u64>().ok();
        Some((name, sectors(5)? * 512, sectors(9)? * 512))
    })
}
/// (device, mount point) of block-device filesystems, in mount order.
fn mounts(text: &str) -> impl Iterator<Item = (&str, String)> {
    text.lines().filter_map(|l| {
        let mut f = l.split_whitespace();
        let (device, path) = (f.next()?, f.next()?);
        (device.starts_with("/dev/") && !device.starts_with("/dev/loop"))
            .then(|| (device, path.replace("\\040", " ")))
    })
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
    ppid: i32,
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
        ppid: n(4)? as i32,
        ticks: n(14)? + n(15)?,
        threads: n(20)? as u32,
        rss: n(24)?,
    })
}
/// Temperature sensors worth naming, found once: the first input of each
/// hwmon chip, the CPU's package sensor first.
fn sensors() -> Vec<(String, PathBuf)> {
    let Ok(dir) = fs::read_dir("/sys/class/hwmon") else {
        return vec![];
    };
    let mut dirs: Vec<PathBuf> = dir.flatten().map(|e| e.path()).collect();
    dirs.sort();
    let mut found: Vec<(String, PathBuf)> = vec![];
    for dir in dirs {
        let chip = fs::read_to_string(dir.join("name")).unwrap_or_default();
        let name = match chip.trim() {
            "coretemp" | "k10temp" | "zenpower" | "cpu_thermal" => "CPU",
            "nvme" => "SSD",
            "spd5118" | "jc42" => "Memory",
            "amdgpu" | "nouveau" => continue,
            n if n.starts_with("iwlwifi") || n.starts_with("ath") || n.starts_with("mt7") => {
                "Wi-Fi"
            }
            "acpitz" => "System",
            _ => continue,
        };
        let input = dir.join("temp1_input");
        if !input.exists() {
            continue;
        }
        let same = found.iter().filter(|(n, _)| n.starts_with(name)).count();
        let name = if same == 0 {
            name.to_string()
        } else {
            if same == 1
                && let Some(first) = found.iter_mut().find(|(n, _)| n == name)
            {
                first.0 = format!("{name} 1");
            }
            format!("{name} {}", same + 1)
        };
        found.push((name, input));
    }
    found.sort_by_key(|(n, _)| !n.starts_with("CPU"));
    found
}
/// Graphics cards by their driver's vendor.
fn cards() -> Vec<Card> {
    let Ok(dir) = fs::read_dir("/sys/class/drm") else {
        return vec![];
    };
    let mut cards: Vec<Card> = dir
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("card") && !n.contains('-')
        })
        .filter_map(|e| {
            let dir = e.path();
            let vendor = match fs::read_to_string(dir.join("device/vendor")).ok()?.trim() {
                "0x10de" => Vendor::Nvidia,
                "0x8086" => Vendor::Intel,
                "0x1002" => Vendor::Amd,
                _ => return None,
            };
            let idle = [
                "gt/gt0/rc6_residency_ms",
                "power/rc6_residency_ms",
                "device/tile0/gt0/gtidle/idle_residency_ms",
            ]
            .iter()
            .map(|p| dir.join(p))
            .find(|p| p.exists());
            Some(Card {
                vendor,
                dir,
                idle: idle.filter(|_| vendor == Vendor::Intel),
                idle_before: 0,
            })
        })
        .collect();
    // The discrete card leads.
    cards.sort_by_key(|c| c.vendor == Vendor::Intel);
    let mut nvidia = false;
    cards.retain(|c| c.vendor != Vendor::Nvidia || !std::mem::replace(&mut nvidia, true));
    cards
}
/// ponytail: the first NVIDIA card only; zip the CSV lines with the cards for more.
fn nvidia_smi() -> Option<Gpu> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,clocks.gr,clocks.max.gr",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    parse_nvidia(String::from_utf8_lossy(&out.stdout).lines().next()?)
}
fn parse_nvidia(line: &str) -> Option<Gpu> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    let n = |i: usize| f.get(i)?.parse::<f32>().ok();
    let mib = |i: usize| n(i).map(|v| v as u64 * 1_048_576);
    Some(Gpu {
        name: f.first()?.replace("NVIDIA ", "").replace(" Laptop GPU", ""),
        busy: n(1).map(|v| v / 100.),
        memory: mib(2).zip(mib(3)),
        temperature: n(4),
        watts: n(5),
        clock: n(6).zip(n(7)).map(|(a, b)| (a as u32, b as u32)),
        asleep: false,
    })
}
/// The computer's own battery (not a mouse's), from sysfs. Values come in µW,
/// µWh, µA, µAh and µV.
fn battery(supplies: &Path) -> Option<Battery> {
    fs::read_dir(supplies).ok()?.flatten().find_map(|entry| {
        let p = entry.path();
        let read = |file: &str| fs::read_to_string(p.join(file)).unwrap_or_default();
        let num = |file: &str| read(file).trim().parse::<f64>().ok();
        if read("type").trim() != "Battery" || read("scope").trim() == "Device" {
            return None;
        }
        let percent = num("capacity")? as f32;
        let status = read("status").trim().to_string();
        let volts = num("voltage_now").map(|v| v / 1e6);
        let watts = num("power_now")
            .map(|v| v / 1e6)
            .or_else(|| Some(num("current_now")? / 1e6 * volts?))
            .unwrap_or(0.);
        // Energy in Wh, from energy_* or from charge_* times the voltage.
        let wh = |kind: &str| {
            num(&format!("energy_{kind}"))
                .map(|v| v / 1e6)
                .or_else(|| Some(num(&format!("charge_{kind}"))? / 1e6 * volts?))
        };
        let (now, full) = (wh("now"), wh("full"));
        let seconds = (watts > 0.1)
            .then(|| match status.as_str() {
                "Discharging" => Some(now? / watts),
                "Charging" => Some((full? - now?).max(0.) / watts),
                _ => None,
            })
            .flatten()
            .map(|h| (h * 3600.) as u64);
        let design = num("energy_full_design").or_else(|| num("charge_full_design"));
        let full_raw = num("energy_full").or_else(|| num("charge_full"));
        Some(Battery {
            percent,
            status,
            watts: watts as f32,
            seconds,
            health: full_raw
                .zip(design)
                .filter(|(_, d)| *d > 0.)
                .map(|(f, d)| (f / d) as f32),
            cycles: num("cycle_count").filter(|c| *c > 0.).map(|c| c as u32),
        })
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
        assert_eq!((s.ppid, s.ticks, s.threads, s.rss), (1, 42, 7, 250));
    }
    #[test]
    fn counters_parse() {
        let stat = "cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 50 0 50 350 50 0 0 0 0 0\nintr 1\n";
        assert_eq!(cpu_times(stat), vec![(200, 1000), (100, 500)]);
        let dev = "h\nh\n    lo: 9 0 0 0 0 0 0 0 9 0\n  wlan0: 100 0 0 0 0 0 0 0 40 0\n  eth0: 0 0 0 0 0 0 0 0 0 0\n";
        assert_eq!(net_bytes(dev).collect::<Vec<_>>(), vec![("wlan0", 100, 40)]);
        let disks = " 259 0 nvme0n1 1 0 10 0 1 0 4 0\n 259 1 nvme0n1p1 1 0 99 0 1 0 99 0\n";
        assert_eq!(
            disk_bytes(disks, |n| n == "nvme0n1").collect::<Vec<_>>(),
            vec![("nvme0n1", 5120, 2048)]
        );
        let m = "proc /proc proc rw 0 0\n/dev/nvme0n1p2 / btrfs rw 0 0\n/dev/nvme0n1p2 /home btrfs rw 0 0\n/dev/sda1 /mnt/My\\040Disk ext4 rw 0 0\n";
        assert_eq!(
            mounts(m).collect::<Vec<_>>(),
            vec![
                ("/dev/nvme0n1p2", "/".into()),
                ("/dev/nvme0n1p2", "/home".into()),
                ("/dev/sda1", "/mnt/My Disk".into())
            ]
        );
        let g =
            parse_nvidia("NVIDIA GeForce RTX 5070 Laptop GPU, 12, 5, 8151, 50, 3.18, 180, 3090")
                .unwrap();
        assert_eq!(g.name, "GeForce RTX 5070");
        assert_eq!(
            (g.busy, g.clock, g.watts),
            (Some(0.12), Some((180, 3090)), Some(3.18))
        );
        assert_eq!(g.memory, Some((5 * 1_048_576, 8151 * 1_048_576)));
    }
    #[test]
    fn batteries_report_rate_and_time() {
        let dir = std::env::temp_dir().join(format!("nexus-monitor-bat-{}", std::process::id()));
        let b = dir.join("BAT0");
        fs::create_dir_all(&b).unwrap();
        for (f, v) in [
            ("type", "Battery"),
            ("capacity", "50"),
            ("status", "Discharging"),
            ("voltage_now", "15000000"),
            ("current_now", "1000000"),
            ("charge_now", "2000000"),
            ("charge_full", "4000000"),
            ("charge_full_design", "5000000"),
        ] {
            fs::write(b.join(f), v).unwrap();
        }
        let bat = battery(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(bat.watts, 15.);
        // 2 Ah at 15 V is 30 Wh: two hours at 15 W.
        assert_eq!(bat.seconds, Some(7200));
        assert_eq!(bat.health, Some(0.8));
    }
    #[test]
    fn samples_this_machine() {
        let mut sampler = Sampler::default();
        let mut s = Sample::default();
        sampler.sample(&mut s, true);
        std::thread::sleep(std::time::Duration::from_millis(50));
        sampler.sample(&mut s, true);
        assert!(s.memory_total > 0 && !s.cores.is_empty() && !s.procs.is_empty());
        assert!((0. ..=1.).contains(&s.cpu));
        sampler.sample(&mut s, false);
        assert!(s.procs.is_empty() && s.processes > 0);
    }
}
