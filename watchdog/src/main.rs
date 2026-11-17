
// rex-watchdog — Rust port dari battery-watchdog.sh + charge-limit 80/40 + battery-saver stages
// Loop 30 dtk: cek baterai, jalankan charge-limit, stage hemat, shutdown bersih, recovery.
use std::env;
use std::fs;
use std::process::{Command, exit};
use std::thread;
use std::time::Duration;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default().trim().to_string()
}
fn read_first(path: &str) -> String {
    read(path).lines().next().unwrap_or("").to_string()
}
fn digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}
fn host(cmd: &str) -> String {
    Command::new("timeout").args(["8", "sh", "-c", cmd]).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

struct Conf { max_charge: i64, resume_charge: i64 }
fn load_conf() -> Conf {
    let mut c = Conf { max_charge: 80, resume_charge: 40 };
    for p in ["/proc/1/root/data/linux/charge-limit.conf", "/data/linux/charge-limit.conf"] {
        let txt = read(p);
        if txt.is_empty() { continue; }
        for l in txt.lines() {
            if let Some(v) = l.strip_prefix("MAX_CHARGE=") { if digits(v.trim()) { c.max_charge = v.trim().parse().unwrap_or(80); } }
            if let Some(v) = l.strip_prefix("RESUME_CHARGE=") { if digits(v.trim()) { c.resume_charge = v.trim().parse().unwrap_or(40); } }
        }
        break;
    }
    c
}

fn sysfs_write(path: &str, val: &str) {
    let _ = fs::write(path, val);
}

fn main() {
    let once = env::args().any(|a| a == "--check");
    let conf = load_conf();
    let state_file = "/opt/rex-head/data/battery-saver.state";
    let log_file = "/opt/rex-head/data/battery-saver-rust.log";

    let log = |msg: &str| {
        let line = format!("{} | {}\n", chrono_like_now(), msg);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_file) {
            let _ = f.write_all(line.as_bytes());
        }
    };

    let base = "/proc/1/root/sys/class/power_supply/battery";
    let cap = read_first(&format!("{}/capacity", base)).parse::<i64>().unwrap_or(0);
    let mut st_level = 0i64;
    if let Ok(t) = fs::read_to_string(state_file) {
        for l in t.lines() {
            if let Some(v) = l.strip_prefix("level=") { st_level = v.trim().parse().unwrap_or(0); }
        }
    }

    if once {
        println!("cap={} state_level={} max={} resume={}", cap, st_level, conf.max_charge, conf.resume_charge);
        return;
    }

    log("rex-watchdog rust aktif");
    loop {
        let cap = read_first(&format!("{}/capacity", base)).parse::<i64>().unwrap_or(0);
        let status = read_first(&format!("{}/status", base));
        let charging = status.eq_ignore_ascii_case("charging") || status.eq_ignore_ascii_case("full");

        // ==== CHARGE LIMIT (max 80 / resume 40) ====
        if cap >= conf.max_charge && charging {
            sysfs_write(&format!("{}/input_suspend", base), "1");
            sysfs_write(&format!("{}/charging_enabled", base), "0");
            log(&format!("limit {}% tercapai — charge ditahan", cap));
        } else if cap <= conf.resume_charge && !charging {
            sysfs_write(&format!("{}/input_suspend", base), "0");
            sysfs_write(&format!("{}/charging_enabled", base), "1");
            log(&format!("resume {}% — charge dinyalakan", cap));
        }

        // ==== SAVER STAGES ====
        let mut stage = 0i64;
        if let Ok(t) = fs::read_to_string(state_file) {
            for l in t.lines() {
                if let Some(v) = l.strip_prefix("stage=") { stage = v.trim().parse().unwrap_or(0); }
            }
        }
        let new_stage = if cap <= 3 { 4 } else if cap <= 5 { 3 } else if cap <= 10 { 2 } else if cap <= 15 { 1 } else { 0 };
        if new_stage > stage {
            match new_stage {
                1 => { log(&format!("stage1 hemat @{}%", cap)); host("am broadcast -a android.intent.action.SCREEN_OFF >/dev/null 2>&1"); }
                2 => { log(&format!("stage2 stop layanan berat @{}%", cap)); host("pm2 stop hermes >/dev/null 2>&1; pkill -f head.py >/dev/null 2>&1"); }
                3 => { log(&format!("stage3 sleep satu2 @{}%", cap)); let _ = fs::write("/opt/hermes-zero-ui/.run/sleep-guard", "1"); host("pm2 stop all >/dev/null 2>&1; sync; sync"); }
                4 => { log(&format!("stage4 SHUTDOWN bersih @{}%", cap)); host("pm2 save >/dev/null 2>&1"); host("sync; sync"); host("su -c 'reboot -p' 2>/dev/null || reboot -p 2>/dev/null || /system_ext/bin/su -c 'reboot -p'"); }
                _ => {}
            }
            let _ = fs::write(state_file, format!("level={} stage={}", cap, new_stage));
        } else if new_stage == 0 && stage > 0 && charging {
            // recovery saat charger pasang
            let _ = fs::remove_file("/opt/hermes-zero-ui/.run/sleep-guard");
            host("pm2 resurrect >/dev/null 2>&1");
            log(&format!("recovery @{}% charging", cap));
            let _ = fs::write(state_file, format!("level={} stage=0", cap));
        }

        thread::sleep(Duration::from_secs(30));
    }
}

fn chrono_like_now() -> String {
    // timestamp lokal sederhana
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    // UTC+7 WIB
    let t = secs + 7 * 3600;
    let d = t % 86400;
    format!("{:02}:{:02}:{:02} WIB", d / 3600, (d % 3600) / 60, d % 60)
}
