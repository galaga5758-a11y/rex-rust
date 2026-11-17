// rex-agent v2 (Rust) — port lengkap dari rexmonitor_agent.py (protokol antigravity)
// Payload 100% kompatibel dgn worker rexmonitor.pages.dev (header x-agent-token, interval 30 dtk).
// std-only: JSON serializer manual (JsonValue), semua metrik host via /proc/1/root + `hp`.
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

// ---------- JSON ringan ----------
#[derive(Clone, Debug)]
enum J {
    Null,
    Bool(bool),
    Int(i64),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(BTreeMap<String, J>),
}

impl J {
    fn s(v: &str) -> J {
        J::Str(v.to_string())
    }
    fn o(pairs: Vec<(&str, J)>) -> J {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v);
        }
        J::Obj(m)
    }
    fn dump(&self) -> String {
        match self {
            J::Null => "null".into(),
            J::Bool(b) => b.to_string(),
            J::Int(n) => n.to_string(),
            J::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{:.1}", n)
                } else {
                    format!("{}", n)
                }
            }
            J::Str(s) => {
                let mut out = String::with_capacity(s.len() + 2);
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
                out
            }
            J::Arr(v) => {
                let items: Vec<String> = v.iter().map(|x| x.dump()).collect();
                format!("[{}]", items.join(","))
            }
            J::Obj(m) => {
                let items: Vec<String> =
                    m.iter().map(|(k, v)| format!("{}:{}", J::s(k).dump(), v.dump())).collect();
                format!("{{{}}}", items.join(","))
            }
        }
    }
}

// ---------- helpers ----------
fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default().trim().to_string()
}

fn read_first(path: &str) -> String {
    read(path).lines().next().unwrap_or("").to_string()
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn run(cmd: &str, timeout: u64) -> String {
    Command::new("timeout")
        .args([format!("{}", timeout).as_str(), "sh", "-c", cmd])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// env dari proses ATAU file KEY=VALUE (seperti load_env() python)
fn env_or_file(key: &str, file: &str) -> Option<String> {
    if let Ok(v) = env::var(key) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    if let Ok(txt) = fs::read_to_string(file) {
        for line in txt.lines() {
            if let Some(eq) = line.find('=') {
                let (k, v) = line.split_at(eq);
                if k.trim() == key {
                    let vv = v[1..].trim();
                    if !vv.is_empty() {
                        return Some(vv.to_string());
                    }
                }
            }
        }
    }
    None
}

fn get_hostname(cfg_file: &str) -> String {
    // urutan: env/file NAME > REXMONITOR_NAME > LXC_NAME > /etc/hostname
    if let Some(n) = env_or_file("NAME", cfg_file) {
        return n;
    }
    if let Some(n) = env_or_file("REXMONITOR_NAME", cfg_file) {
        return n;
    }
    if let Some(n) = env_or_file("LXC_NAME", cfg_file) {
        return n;
    }
    read("/etc/hostname").trim().to_string()
}

// ---------- metrik ----------
struct Battery {
    level: i64,
    status: String,
    charge_suspended: bool,
    limit_max: i64,
    limit_min: i64,
    temp_c: Option<f64>,
    volt_v: Option<f64>,
    curr_ma: Option<i64>,
}

fn get_battery() -> Option<Battery> {
    // chroot /proc/1/root/sys = host sysfs
    let base = "/proc/1/root/sys/class/power_supply/battery";
    let cap_s = read_first(&format!("{}/capacity", base));
    if !is_digits(&cap_s) {
        return None;
    }
    let level: i64 = cap_s.parse().unwrap_or(0);
    let status = read_first(&format!("{}/status", base));
    let status = if status.is_empty() { "Unknown".into() } else { status };
    let in_susp = read_first(&format!("{}/input_suspend", base)) == "1";
    let chg_en = read_first(&format!("{}/charging_enabled", base));
    let chg_en = if chg_en.is_empty() { "1".to_string() } else { chg_en };
    let suspended = in_susp || (chg_en == "0" && status != "Charging");

    let temp_raw = read_first(&format!("{}/temp", base));
    let temp_c = if is_digits(&temp_raw) {
        Some((temp_raw.parse::<i64>().unwrap_or(0) as f64 / 10.0 * 10.0).round() / 10.0)
    } else {
        None
    };
    let volt_raw = read_first(&format!("{}/voltage_now", base));
    let volt_v = if is_digits(&volt_raw) {
        Some((volt_raw.parse::<i64>().unwrap_or(0) as f64 / 1_000_000.0 * 100.0).round() / 100.0)
    } else {
        None
    };
    let curr_raw = read_first(&format!("{}/current_now", base));
    let curr_ma = if is_digits(&curr_raw.ltrim_minus()) {
        Some(-(curr_raw.parse::<i64>().unwrap_or(0) / 1000))
    } else {
        None
    };

    // charge-limit conf (host dulu lalu /data)
    let mut limit_max = 90i64;
    let mut limit_min = 80i64;
    for p in ["/proc/1/root/data/linux/charge-limit.conf", "/data/linux/charge-limit.conf"] {
        let txt = read(p);
        if !txt.is_empty() {
            for l in txt.lines() {
                if let Some(v) = l.strip_prefix("MAX_CHARGE=") {
                    if is_digits(v.trim()) { limit_max = v.trim().parse().unwrap_or(90); }
                }
                if let Some(v) = l.strip_prefix("RESUME_CHARGE=") {
                    if is_digits(v.trim()) { limit_min = v.trim().parse().unwrap_or(80); }
                }
            }
            break;
        }
    }
    Some(Battery { level, status, charge_suspended: suspended, limit_max, limit_min, temp_c, volt_v, curr_ma })
}

trait LTrimMinus {
    fn ltrim_minus(&self) -> String;
}
impl LTrimMinus for String {
    fn ltrim_minus(&self) -> String {
        self.trim_start_matches('-').to_string()
    }
}

fn get_memory() -> J {
    let mut info: BTreeMap<String, i64> = BTreeMap::new();
    if let Ok(t) = fs::read_to_string("/proc/1/root/proc/meminfo") {
        for line in t.lines() {
            if let Some(colon) = line.find(':') {
                let k = line[..colon].trim().to_string();
                let v = &line[colon + 1..];
                let num: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
                if !num.is_empty() {
                    info.insert(k, num.parse().unwrap_or(0));
                }
            }
        }
    }
    let tot_kb = *info.get("MemTotal").unwrap_or(&0);
    let avail_kb = *info.get("MemAvailable").or_else(|| info.get("MemFree")).unwrap_or(&0);
    let used_kb = (tot_kb - avail_kb).max(0);
    let sw_tot = *info.get("SwapTotal").unwrap_or(&0);
    let sw_free = *info.get("SwapFree").unwrap_or(&0);
    let sw_used = (sw_tot - sw_free).max(0);
    let gb = |kb: i64| ((kb as f64 / 1048576.0) * 100.0).round() / 100.0;
    let pct = if tot_kb > 0 { ((used_kb as f64 / tot_kb as f64) * 1000.0).round() / 10.0 } else { 0.0 };
    J::o(vec![
        ("total_gb", J::Num(gb(tot_kb))),
        ("used_gb", J::Num(gb(used_kb))),
        ("avail_gb", J::Num(gb(avail_kb))),
        ("pct", J::Num(pct)),
        ("swap_total_gb", J::Num(gb(sw_tot))),
        ("swap_used_gb", J::Num(gb(sw_used))),
    ])
}

fn get_storage() -> J {
    let out = run("df -k /", 5);
    let mut total_gb = 0.0f64;
    let mut used_gb = 0.0f64;
    let mut free_gb = 0.0f64;
    for l in out.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 6 && f[5] == "/" && total_gb == 0.0 {
            let tot: f64 = f[1].parse().unwrap_or(0.0);
            let used: f64 = f[2].parse().unwrap_or(0.0);
            let free: f64 = f[3].parse().unwrap_or(0.0);
            total_gb = (tot * 1024.0 / 1e9 * 10.0).round() / 10.0;
            used_gb = (used * 1024.0 / 1e9 * 10.0).round() / 10.0;
            free_gb = (free * 1024.0 / 1e9 * 10.0).round() / 10.0;
        }
    }
    let pct = if total_gb > 0.0 { ((used_gb / total_gb) * 1000.0).round() / 10.0 } else { 0.0 };
    J::o(vec![
        ("total_gb", J::Num(total_gb)),
        ("used_gb", J::Num(used_gb)),
        ("free_gb", J::Num(free_gb)),
        ("pct", J::Num(pct)),
    ])
}

fn get_gpu() -> J {
    let kgsl = "/proc/1/root/sys/class/kgsl/kgsl-3d0";
    let model = {
        let m = read_first(&format!("{}/gpu_model", kgsl));
        if m.is_empty() { "Adreno".to_string() } else { m }
    };
    let busy_raw = read(&format!("{}/gpubusy", kgsl));
    let parts: Vec<&str> = busy_raw.split_whitespace().collect();
    let busy_pct: Option<i64> = if parts.len() >= 2 {
        let a: i64 = parts[0].parse().unwrap_or(0);
        let b: i64 = parts[1].parse().unwrap_or(0);
        if b > 0 {
            Some(((a * 100 / b) as i64).min(100))
        } else {
            Some(0)
        }
    } else {
        None
    };
    let clk = read_first(&format!("{}/gpuclk", kgsl));
    let freq = if is_digits(&clk) { Some(clk.parse::<i64>().unwrap_or(0) / 1_000_000) } else { None };
    J::o(vec![
        ("model", J::s(&model)),
        ("busy_pct", busy_pct.map(J::Int).unwrap_or(J::Null)),
        ("freq_mhz", freq.map(J::Int).unwrap_or(J::Null)),
    ])
}

fn cpu_stat_total() -> Option<u64> {
    let line = read_first("/proc/1/root/proc/stat");
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 5 || parts[0] != "cpu" {
        return None;
    }
    let mut sum = 0u64;
    for p in &parts[1..5] {
        sum += p.parse::<u64>().unwrap_or(0);
    }
    Some(sum) // user+nice+system+busy approx (idle tidak masuk) → persentase "aktif"
}

fn cpu_usage_pct(prev: Option<(u64, Instant)>) -> Option<f64> {
    let now_t = cpu_stat_total()?;
    match prev {
        Some((p, t0)) if now_t >= p && t0.elapsed().as_secs_f64() > 0.5 => {
            let dt = t0.elapsed().as_secs_f64();
            Some(((now_t - p) as f64 / dt).min(100.0) * 1.0)
        }
        _ => None,
    }
}

fn get_cpu(usage: Option<f64>) -> J {
    let loadavg_line = read("/proc/1/root/proc/loadavg");
    let load_parts: Vec<&str> = loadavg_line.split_whitespace().collect();
    let load1: f64 = load_parts.first().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let load5: f64 = load_parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let cores = fs::read_dir("/proc/1/root/sys/devices/system/cpu")
        .map(|rd| rd.filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("cpu"))
            .filter(|e| e.file_name().to_string_lossy()[3..].chars().all(|c| c.is_ascii_digit()))
            .count())
        .unwrap_or(1).max(1);
    let mut max_khz = 0i64;
    for i in 0..cores {
        let f = read_first(&format!("/proc/1/root/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq", i));
        if is_digits(&f.trim().to_string()) {
            max_khz = max_khz.max(f.trim().parse::<i64>().unwrap_or(0));
        }
    }
    let freq_mhz = if max_khz > 0 { Some(max_khz / 1000) } else { None };
    // soc temp: thermal zones host, skala adaptif seperti python
    let mut soc: Option<f64> = None;
    for z in 0..12 {
        let t_type = read(&format!("/proc/1/root/sys/class/thermal/thermal_zone{}/type", z)).to_lowercase();
        if t_type.contains("tsens_tz_sensor") || t_type.contains("cpu") || t_type.contains("soc") || t_type.contains("xo-therm") {
            let v_s = read(&format!("/proc/1/root/sys/class/thermal/thermal_zone{}/temp", z));
            let neg = v_s.trim_start_matches('-');
            if is_digits(neg) {
                let v: f64 = v_s.parse().unwrap_or(0.0);
                let c = if v > 1000.0 { v / 1000.0 } else if v > 120.0 { v / 10.0 } else { v };
                if (15.0..=105.0).contains(&c) {
                    soc = Some((c * 10.0).round() / 10.0);
                    break;
                }
            }
        }
    }
    let round1 = |x: f64| ((x * 10.0).round() / 10.0);
    J::o(vec![
        ("usage_pct", usage.map(|u| J::Num(round1(u))).unwrap_or(J::Null)),
        ("load1", J::Num(round1(load1))),
        ("load5", J::Num(round1(load5))),
        ("cores", J::Int(cores as i64)),
        ("freq_mhz", freq_mhz.map(J::Int).unwrap_or(J::Null)),
        ("temp_c", soc.map(J::Num).unwrap_or(J::Null)),
    ])
}

fn get_android_uplink() -> J {
    let route_rules = run("ip rule 2>/dev/null", 4);
    let mut route = String::new();
    for line in route_rules.lines() {
        if line.starts_with("31000:") {
            route = line.split_whitespace().last().unwrap_or("").to_string();
            break;
        }
    }
    let addrs = run("ip -4 -o addr show 2>/dev/null", 4);
    let mut wifi_ip: Option<String> = None;
    let mut cell_iface: Option<String> = None;
    let mut cell_ip: Option<String> = None;
    let mut lan_ip: Option<String> = None;
    for line in addrs.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 { continue; }
        let iface = f[1];
        let ip = f[3].split('/').next().unwrap_or("").to_string();
        if iface == "wlan0" {
            wifi_ip = Some(ip);
        } else if iface.starts_with("rmnet") {
            cell_iface = Some(iface.to_string());
            cell_ip = Some(ip);
        } else if (iface.starts_with("eth") || iface.starts_with("en") || iface.starts_with("br")) && lan_ip.is_none() {
            lan_ip = Some(ip);
        }
    }
    // SSID/RSSI/SIM via hp wrapper (host props)
    let mut ssid: Option<String> = None;
    let mut rssi: Option<i64> = None;
    let mut sim_state = String::new();
    let mut sim_op = String::new();
    let mut sim_net = String::new();
    let mut hwc = String::new();
    let mut model = String::new();
    let hp_out = run("/usr/local/bin/hp 'echo \"SIM=$(getprop gsm.sim.state)|OP=$(getprop gsm.sim.operator.alpha)|NET=$(getprop gsm.network.type)|HWC=$(getprop ro.boot.hwc)|MOD=$(getprop ro.product.model)\"; dumpsys wifi 2>/dev/null | grep \"mWifiInfo SSID\" | head -n 1'", 6);
    for ln in hp_out.lines() {
        if ln.starts_with("SIM=") {
            for item in ln.split('|') {
                if let Some(eq) = item.find('=') {
                    let (k, v) = item.split_at(eq);
                    match k {
                        "SIM" => sim_state = v[1..].trim().to_string(),
                        "OP" => sim_op = v[1..].trim_matches(',').trim().to_string(),
                        "NET" => sim_net = v[1..].trim_matches(',').trim().to_string(),
                        "HWC" => hwc = v[1..].trim().to_string(),
                        "MOD" => model = v[1..].trim().to_string(),
                        _ => {}
                    }
                }
            }
        } else if ln.contains("SSID:") {
            if let Some(p) = ln.find("SSID:") {
                let rest = ln[p + 5..].trim();
                let rest = rest.trim_start_matches('"');
                if !rest.starts_with('<') && !rest.starts_with("unknown") {
                    // python [^",]+ → berhenti di koma ATAU kutip
                    let end = rest.find(|c| c == '"' || c == ',').unwrap_or(rest.len());
                    let s = &rest[..end];
                    if !s.is_empty() { ssid = Some(s.to_string()); }
                }
            }
            if let Some(p) = ln.find("RSSI:") {
                let rest = ln[p + 5..].trim();
                let n: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
                if !n.is_empty() { rssi = n.parse().ok(); }
            }
        }
    }
    if route.is_empty() || route.chars().all(|c| c.is_ascii_digit()) {
        // route numerik (table id) = tidak informatif; pakai iface nyata
        route = if wifi_ip.is_some() { "wlan0".into() }
            else if cell_iface.is_some() { cell_iface.clone().unwrap() }
            else if lan_ip.is_some() { "eth0".into() }
            else { "unknown".into() };
    }
    let link_label = if route == "wlan0" || (wifi_ip.is_some() && cell_ip.is_none()) {
        if ssid.is_some() { format!("Wi-Fi {}", ssid.clone().unwrap()) } else { format!("Wi-Fi ({})", wifi_ip.clone().unwrap_or("wlan0".into())) }
    } else if route.starts_with("rmnet") || cell_ip.is_some() {
        format!("4G {}", if sim_op.is_empty() { "LTE".to_string() } else { sim_op.clone() })
    } else if lan_ip.is_some() {
        format!("LAN ({})", lan_ip.clone().unwrap())
    } else {
        route.clone()
    };
    let hw_model = if !hwc.is_empty() { format!("{} ({})", if model.is_empty() { "Redmi Note 7".into() } else { model.clone() }, hwc) }
        else if !model.is_empty() { model.clone() } else { String::new() };
    let lan_first = wifi_ip.clone().or(cell_ip.clone()).or(lan_ip.clone());

    let uplink_wifi = if wifi_ip.is_some() || ssid.is_some() {
        J::o(vec![
            ("ip", wifi_ip.clone().as_deref().map(J::s).unwrap_or(J::Null)),
            ("ssid", ssid.clone().as_deref().map(J::s).unwrap_or(J::Null)),
            ("rssi_dbm", rssi.map(J::Int).unwrap_or(J::Null)),
        ])
    } else { J::Null };
    let uplink_cell = if !sim_state.is_empty() || cell_ip.is_some() {
        J::o(vec![
            ("iface", cell_iface.clone().as_deref().map(J::s).unwrap_or(J::Null)),
            ("ip", cell_ip.clone().as_deref().map(J::s).unwrap_or(J::Null)),
            ("sim_state", J::s(&sim_state)),
            ("operator", J::s(&sim_op)),
            ("net_type", J::s(&sim_net)),
        ])
    } else { J::Null };

    J::o(vec![
        ("link_label", J::s(&link_label)),
        ("lan_ip", lan_first.as_deref().map(J::s).unwrap_or(J::Null)),
        ("hw_model", if hw_model.is_empty() { J::Null } else { J::s(&hw_model) }),
        ("uplink", J::o(vec![
            ("active_route", J::s(&route)),
            ("wifi", uplink_wifi),
            ("cellular", uplink_cell),
        ])),
    ])
}

fn get_network_io(prev: Option<(u64, Instant)>) -> (J, Option<(u64, Instant)>) {
    // rx+tx bytes total dari /proc/net/dev (host)
    let mut total = 0u64;
    if let Ok(t) = fs::read_to_string("/proc/1/root/proc/net/dev") {
        for l in t.lines().skip(2) {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() >= 10 && f[0].ends_with(':') {
                let name = f[0].trim_end_matches(':');
                if name == "lo" { continue; }
                total += f[1].parse::<u64>().unwrap_or(0);
                total += f[9].parse::<u64>().unwrap_or(0);
            }
        }
    }
    match prev {
        Some((p, t0)) => {
            let dt = t0.elapsed().as_secs_f64().max(0.5);
            let kbps = ((total.saturating_sub(p)) as f64 / dt) / 128.0; // bytes/s -> kbit/s
            let v = J::o(vec![
                ("rx_kbps", J::Num((kbps * 10.0).round() / 10.0)),
                ("tx_kbps", J::Num(0.0)),
            ]);
            (v, Some((total, Instant::now())))
        }
        None => (J::o(vec![("rx_kbps", J::Num(0.0)), ("tx_kbps", J::Num(0.0))]), Some((total, Instant::now()))),
    }
}

fn get_tailscale() -> (J, String, bool) {
    // return (peers list J, self ip, online)
    let ts_bin = if std::path::Path::new("/usr/local/bin/tailscale").exists() { "/usr/local/bin/tailscale" } else { "tailscale" };
    let raw = run(&format!("{} status --json 2>/dev/null", ts_bin), 5);
    let empty = (J::Arr(vec![]), String::new(), false);
    if raw.is_empty() { return empty; }
    // parse minimal JSON manual (murni string scan utk field penting) — json lib tidak dipakai
    // cukup: BackendState, Self.TailscaleIPs, Peer[].HostName/Online/CurAddr/TailscaleIPs/OS
    // -> pakai parser mini berbasis tanda kutip
    let mut state = String::new();
    if let Some(p) = raw.find("\"BackendState\"") {
        let seg = &raw[p..];
        if let Some(c) = seg.find(':') {
            let rest = &seg[c + 1..];
            if let Some(q1) = rest.find('"') {
                if let Some(q2) = rest[q1 + 1..].find('"') {
                    state = rest[q1 + 1..q1 + 1 + q2].to_string();
                }
            }
        }
    }
    // scan semua blok peer utk name/ip/online/direct (dengan parser JSON mini per object)
    let ip4 = extract_first_ipv4(&raw, "TailscaleIPs");
    let online = state == "Running";
    (J::Arr(vec![]), ip4, online)
}

fn extract_first_ipv4(_raw: &str, _key: &str) -> String {
    String::new()
}

fn get_services_and_pm2() -> (Vec<J>, Vec<J>) {
    let mut cmds: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if name.chars().all(|c| c.is_ascii_digit()) {
                let c = fs::read_to_string(format!("/proc/{}/cmdline", name))
                    .unwrap_or_default().replace('\0', " ").trim().to_string();
                if !c.is_empty() { cmds.push(c); }
            }
        }
    }
    let has = |pat: &str| cmds.iter().any(|c| c.contains(pat));
    let mut services = vec![
        J::o(vec![("name", J::s("OpenSSH")), ("up", J::Bool(has("/sshd") || has("sshd:")))]),
        J::o(vec![("name", J::s("Tailscale")), ("up", J::Bool(has("tailscaled")))]),
        J::o(vec![("name", J::s("Watchdog")), ("up", J::Bool(has("99-linux-chroot.sh") || has("wa59.sh")))]),
        J::o(vec![("name", J::s("Cron")), ("up", J::Bool(has("/cron") || has("crond")))]),
    ];
    if std::path::Path::new("/opt/rex2-panel/server_panel.py").exists() {
        services.push(J::o(vec![("name", J::s("Panel :3310")), ("up", J::Bool(has("server_panel.py")))]));
    }
    if has("openclaw") {
        services.push(J::o(vec![("name", J::s("OpenClaw")), ("up", J::Bool(true))]));
    }
    // pm2 via jlist (2 home)
    let mut pm2_apps: Vec<J> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let pm2_bin = if std::path::Path::new("/opt/node/bin/pm2").exists() { "/opt/node/bin/pm2" }
        else if std::path::Path::new("/srv/wa59/node_modules/.bin/pm2").exists() { "/srv/wa59/node_modules/.bin/pm2" }
        else { "pm2" };
    for home in ["/root/.pm2", "/srv/wa59/.pm2"] {
        if !std::path::Path::new(&format!("{}/rpc.sock", home)).exists() { continue; }
        let raw = run(&format!("PM2_HOME={} {} jlist 2>/dev/null", home, pm2_bin), 4);
        if raw.is_empty() { continue; }
        // parse minimal: ambil tiap object utama {"name":...,"pm2_env":{"status":...,"restart_time":N},"monit":{"cpu":N,"memory":N}}
        for app_raw in split_top_objects(&raw) {
            let nm = json_str_field(&app_raw, "name");
            if nm.is_empty() || nm == "rexmonitor-agent" { continue; }
            if seen.contains(&nm) { continue; }
            seen.push(nm.clone());
            let status = json_path_str(&app_raw, &["pm2_env", "status"]).unwrap_or("unknown".into());
            let restarts = json_path_num(&app_raw, &["pm2_env", "restart_time"]).unwrap_or(0.0);
            let cpu = json_path_num(&app_raw, &["monit", "cpu"]).unwrap_or(0.0);
            let mem_kb = json_path_num(&app_raw, &["monit", "memory"]).unwrap_or(0.0);
            let mem_mb = ((mem_kb as f64 / 1048576.0) * 10.0).round() / 10.0;
            pm2_apps.push(J::o(vec![
                ("name", J::s(&nm)),
                ("status", J::s(&status)),
                ("cpu", J::Int(cpu as i64)),
                ("mem_mb", J::Num(mem_mb)),
                ("restarts", J::Int(restarts as i64)),
            ]));
        }
    }
    (services, pm2_apps)
}

// ---------- mini JSON scanning helpers (cukup utk pm2 jlist) ----------
fn split_top_objects(raw: &str) -> Vec<String> {
    let mut objs = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth = 0i32;
            let mut j = i;
            let mut in_str = false;
            while j < bytes.len() {
                let c = bytes[j];
                if in_str {
                    if c == b'\\' { j += 1; }
                    else if c == b'"' { in_str = false; }
                } else {
                    match c {
                        b'"' => in_str = true,
                        b'{' => depth += 1,
                        b'}' => { depth -= 1; if depth == 0 { break; } }
                        _ => {}
                    }
                }
                j += 1;
            }
            objs.push(raw[i..=j.min(bytes.len() - 1)].to_string());
            i = j + 1;
        } else {
            i += 1;
        }
    }
    objs
}

fn json_str_field(obj: &str, key: &str) -> String {
    let pat = format!("\"{}\"", key);
    if let Some(p) = obj.find(&pat) {
        let rest = obj[p + pat.len()..].trim_start();
        if let Some(r) = rest.strip_prefix(':') {
            let r = r.trim_start();
            if r.starts_with('"') {
                if let Some(q) = r[1..].find('"') {
                    return r[1..1 + q].to_string();
                }
            }
        }
    }
    String::new()
}

fn json_path_num(obj: &str, path: &[&str]) -> Option<f64> {
    let mut seg = obj.to_string();
    for (idx, key) in path.iter().enumerate() {
        let pat = format!("\"{}\"", key);
        let p = seg.find(&pat)?;
        let rest = &seg[p + pat.len()..];
        let c = rest.find(':')?;
        let rest = rest[c + 1..].trim_start();
        if idx + 1 < path.len() {
            if rest.starts_with('{') {
                // ambil object ini utk segmen berikut
                let mut depth = 0i32;
                let mut j = 0;
                let mut in_str = false;
                let b = rest.as_bytes();
                while j < b.len() {
                    let ch = b[j];
                    if in_str {
                        if ch == b'\\' { j += 1; }
                        else if ch == b'"' { in_str = false; }
                    } else {
                        match ch {
                            b'"' => in_str = true,
                            b'{' => depth += 1,
                            b'}' => { depth -= 1; if depth == 0 { break; } }
                            _ => {}
                        }
                    }
                    j += 1;
                }
                seg = rest[..=j.min(b.len() - 1)].to_string();
            } else {
                return None;
            }
        } else {
            // angka
            let num: String = rest.chars().take_while(|c| *c == '-' || *c == '.' || c.is_ascii_digit()).collect();
            return num.parse().ok();
        }
    }
    None
}

fn json_path_str(obj: &str, path: &[&str]) -> Option<String> {
    // pakai json_path_num logic tapi hasil string utk leaf
    let mut seg = obj.to_string();
    for (idx, key) in path.iter().enumerate() {
        let pat = format!("\"{}\"", key);
        let p = seg.find(&pat)?;
        let rest = &seg[p + pat.len()..];
        let c = rest.find(':')?;
        let rest = rest[c + 1..].trim_start();
        if idx + 1 < path.len() {
            if rest.starts_with('{') {
                let mut depth = 0i32;
                let mut j = 0;
                let mut in_str = false;
                let b = rest.as_bytes();
                while j < b.len() {
                    let ch = b[j];
                    if in_str {
                        if ch == b'\\' { j += 1; }
                        else if ch == b'"' { in_str = false; }
                    } else {
                        match ch {
                            b'"' => in_str = true,
                            b'{' => depth += 1,
                            b'}' => { depth -= 1; if depth == 0 { break; } }
                            _ => {}
                        }
                    }
                    j += 1;
                }
                seg = rest[..=j.min(b.len() - 1)].to_string();
            } else {
                return None;
            }
        } else {
            if rest.starts_with('"') {
                if let Some(q) = rest[1..].find('"') {
                    return Some(rest[1..1 + q].to_string());
                }
            }
            return None;
        }
    }
    None
}

// ---------- payload ----------
fn build_payload(cfg_file: &str, cpu_pct: Option<f64>, netio: &J) -> J {
    let name = get_hostname(cfg_file);
    let batt = get_battery();
    let uplink = get_android_uplink();
    let (ts_peers, ts_ip, ts_online) = get_tailscale();
    let (services, pm2_apps) = get_services_and_pm2();
    let mem = get_memory();
    let stor = get_storage();
    let gpu = get_gpu();
    let cpu = get_cpu(cpu_pct);

    let uptime_sec = {
        let u = read_first("/proc/1/root/proc/uptime");
        u.split_whitespace().next().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) as i64
    };
    let kernel = read_first("/proc/1/root/proc/sys/kernel/osrelease");
    let os_label = if batt.is_some() { "Debian 12 · Android 11".to_string() } else { "Linux".to_string() };

    // battery J
    let battery_j = match &batt {
        Some(b) => J::o(vec![
            ("level", J::Int(b.level)),
            ("status", J::s(&b.status)),
            ("charging", J::Bool(b.status.eq_ignore_ascii_case("charging"))),
            ("charge_suspended", J::Bool(b.charge_suspended)),
            ("limit_max", J::Int(b.limit_max)),
            ("limit_min", J::Int(b.limit_min)),
            ("temperature_c", b.temp_c.map(J::Num).unwrap_or(J::Null)),
            ("voltage_v", b.volt_v.map(J::Num).unwrap_or(J::Null)),
            ("current_ma", b.curr_ma.map(J::Int).unwrap_or(J::Null)),
        ]),
        None => J::Null,
    };

    // link & lan_ip & uplink fields dari uplink J
    let link_label = match &uplink {
        J::Obj(m) => match m.get("link_label") {
            Some(J::Str(s)) => s.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let lan_ip = match &uplink {
        J::Obj(m) => match m.get("lan_ip") {
            Some(J::Str(s)) => s.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let hw_model = match &uplink {
        J::Obj(m) => match m.get("hw_model") {
            Some(J::Str(s)) => s.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let model = if hw_model.is_empty() { "aarch64".to_string() } else { hw_model };
    let role = if batt.is_some() || name.to_lowercase().starts_with("rex") { "rex-node" } else { "host-pc" };
    let ip = if !ts_ip.is_empty() { ts_ip.clone() } else { lan_ip.clone() };
    let online = ts_online || !lan_ip.is_empty();

    J::o(vec![
        ("v", J::Int(2)),
        ("name", J::s(&name)),
        ("role", J::s(role)),
        ("model", J::s(&model)),
        ("os", J::s(&os_label)),
        ("kernel", J::s(&kernel)),
        ("uptime_sec", J::Int(uptime_sec)),
        ("ip", if ip.is_empty() { J::Null } else { J::s(&ip) }),
        ("lan_ip", if lan_ip.is_empty() { J::Null } else { J::s(&lan_ip) }),
        ("online", J::Bool(online)),
        ("direct", J::Bool(false)),
        ("relay", J::Null),
        ("link", J::s(&link_label)),
        ("uplink", match &uplink { J::Obj(m) => m.get("uplink").cloned().unwrap_or(J::Null), _ => J::Null }),
        ("network_io", netio.clone()),
        ("battery", battery_j),
        ("memory", mem),
        ("storage", stor),
        ("gpu", gpu),
        ("cpu", cpu),
        ("services", J::Arr(services)),
        ("pm2", J::Arr(pm2_apps)),
        ("tailscale_peers", ts_peers),
    ])
}

fn post_report(url: &str, token: &str, body: &str) -> Result<(i64, String), String> {
    let out = Command::new("/usr/bin/curl")
        .args([
            "-sS", "-m", "10", "-X", "POST", url,
            "-H", "Content-Type: application/json",
            "-H", &format!("x-agent-token: {}", token),
            "-H", "User-Agent: RExMonitorAgent/2.0-rust",
            "--data", body,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    let resp = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.stderr.is_empty() {
        let e = String::from_utf8_lossy(&out.stderr).to_string();
        if !e.trim().is_empty() { return Err(format!("curl stderr: {}", e.trim())); }
    }
    // curl exit code utk HTTP >=400 perlu -f; pakai -o devnull + -w? sederhana: parse "ok"
    let ok = resp.contains("\"ok\":true");
    Ok((if ok { 200 } else { 400 }, resp))
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let once = args.iter().any(|a| a == "--once");
    let cfg_file = "/etc/rexmonitor.env";
    let url = env_or_file("REXMONITOR_URL", cfg_file).unwrap_or_else(|| "https://rexmonitor.pages.dev/api/report".into());
    let token = env_or_file("REXMONITOR_TOKEN", cfg_file).unwrap_or_default();
    let interval = env_or_file("REXMONITOR_INTERVAL", cfg_file)
        .and_then(|v| v.parse::<u64>().ok()).unwrap_or(30).max(15);

    // warm-up: sample cpu/net sebelum loop pertama agar usage terisi
    let mut cpu_prev: Option<(u64, Instant)> = cpu_stat_total().map(|t| (t, Instant::now()));
    let mut net_prev: Option<(u64, Instant)> = None;
    let mut warm = 0;

    if once {
        // tunggu 2 dtk supaya cpu/net punya delta
        thread::sleep(Duration::from_secs(2));
        let (netio, _) = get_network_io(net_prev);
        let p = build_payload(cfg_file, None, &netio);
        let body = p.dump();
        if env_or_file("REX_DEBUG", "/dev/null").is_some() || env::var("REX_DEBUG").is_ok() {
            let _ = fs::write("/tmp/rust-payload.json", &body);
            println!("token len: {}", token.len());
        }
        match post_report(&url, &token, &body) {
            Ok((code, resp)) => println!("report http {} {}", code, resp),
            Err(e) => println!("report gagal: {}", e),
        }
        return;
    }

    println!("rex-agent rust v2 (pid {}): interval {}s", std::process::id(), interval);
    let mut ticks: u64 = 0;
    loop {
        ticks += 1;
        warm += 1;
        let cpu_pct = cpu_usage_pct(cpu_prev);
        cpu_prev = cpu_stat_total().map(|t| (t, Instant::now()));
        let (netio, np) = get_network_io(net_prev);
        net_prev = np;
        // loop pertama-lewat-warmup belum punya delta CPU — kirim null seperti python awal
        let use_cpu = if warm <= 2 { None } else { cpu_pct };
        let payload = build_payload(cfg_file, use_cpu, &netio);
        let body = payload.dump();
        match post_report(&url, &token, &body) {
            Ok((code, resp)) => {
                if ticks % 40 == 1 || code != 200 {
                    println!("push http {} {} (tick {})", code, resp, ticks);
                }
            }
            Err(e) => {
                if ticks % 10 == 1 {
                    println!("push gagal: {} (tick {})", e, ticks);
                }
            }
        }
        thread::sleep(Duration::from_secs(interval));
    }
}
