//! HWiNFO64 watchdog: keeps HWiNFO running and restarts it before the free version's 12 h shared-memory limit.
//!
//! Every `check_interval_s` (and exactly at the due time) it reads HWiNFO64.exe's creation time. At
//! `restart_after_hours` it kills HWiNFO, writes the silent-start keys into HWiNFO64.INI (only a stopped HWiNFO
//! keeps INI edits), relaunches it with `args` and waits for the shared memory. It also restarts early when the
//! shared memory reports "expired", and starts HWiNFO when it isn't running (`keep_running`).
//! Status goes to `<data>/watchdog_status.json` (the viewer shows it), logs to `<data>/logs/watchdog.log`.
//! HWiNFO runs elevated, so silent kill/relaunch needs an elevated watchdog: `install` registers a logon task.

pub mod fps_helper;
pub mod ini;
pub mod process;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::shm::{self, now_epoch, Kind};
use crate::storage::{deep_merge, read_json, write_json};
use process::Proc;

pub const TASK_NAME: &str = "HWiNFO Viewer Watchdog";
const MUTEX_NAME: &str = "Local\\HWiNFO-Viewer-Watchdog"; // same name as the Python watchdog: they exclude each other
const CANDIDATES: &[(&str, &str)] = &[
    ("ProgramFiles", "HWiNFO64\\HWiNFO64.exe"),
    ("ProgramFiles(x86)", "HWiNFO64\\HWiNFO64.exe"),
    ("ProgramFiles", "HWiNFO\\HWiNFO64.exe"),
    ("LOCALAPPDATA", "Programs\\HWiNFO64\\HWiNFO64.exe"),
];

pub fn defaults() -> Value {
    json!({
        "exe": "",
        "args": ["-sensoronly"],
        "ini": "",
        "ini_settings": {"SensorsOnly": "1", "ShowWelcomeAndProgress": "0", "SensorsSM": "1",
                          "MinimalizeSensors": "1", "AutoUpdate": "0"},
        "restart_after_hours": 11.5,
        "check_interval_s": 30,
        "keep_running": true,
        "kill_timeout_s": 10,
        "ready_timeout_s": 90,
        "settle_s": 1.0,
        "presentmon_exe": ""          // empty = tools\PresentMon.exe next to hwviewer.exe (shipped by the installer)
    })
}

/// PresentMon for the FPS helper: watchdog.json `presentmon_exe`, else `tools\PresentMon.exe` beside our exe.
fn presentmon_path(cfg: &Value) -> PathBuf {
    match cfg["presentmon_exe"].as_str().filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => crate::paths::exe_dir().unwrap_or_default().join("tools").join("PresentMon.exe"),
    }
}

pub fn config_path() -> PathBuf {
    crate::paths::data_dir().join("watchdog.json")
}

pub fn status_path() -> PathBuf {
    crate::paths::data_dir().join("watchdog_status.json")
}

/// Defaults overlaid with watchdog.json; writes an editable copy with every option on first run.
pub fn load_config(path: &Path) -> Value {
    let mut cfg = defaults();
    match read_json(path) {
        Some(user @ Value::Object(_)) => deep_merge(&mut cfg, &user),
        Some(_) => {}
        None if !path.exists() => {
            let _ = write_json(path, &defaults());
        }
        None => {}
    }
    cfg
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub struct Watchdog {
    cfg: Value,
    status_path: PathBuf,
    exe: String,
    exe_name: String,
    limit: f64,
    interval: f64,
    pub stop: Arc<AtomicBool>,
    state: Map<String, Value>,
    first_seen: HashMap<u32, f64>,
    expired_since: Option<f64>,
}

impl Watchdog {
    pub fn new(cfg: Value, status_path: PathBuf) -> Self {
        let prev = read_json(&status_path).unwrap_or(Value::Null);
        let cfg_exe = cfg["exe"].as_str().unwrap_or("").to_string();
        let exe = if cfg_exe.is_empty() { prev["exe"].as_str().unwrap_or("").to_string() } else { cfg_exe.clone() };
        let exe_name = Path::new(if cfg_exe.is_empty() { "HWiNFO64.exe" } else { &cfg_exe })
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "HWiNFO64.exe".into());
        let mut state = Map::new();
        state.insert("pid".into(), json!(std::process::id()));
        state.insert("state".into(), json!("starting"));
        state.insert("exe".into(), json!(exe));
        state.insert("restarts".into(), prev.get("restarts").cloned().unwrap_or(json!(0)));
        state.insert("last_restart".into(), prev.get("last_restart").cloned().unwrap_or(Value::Null));
        for k in ["hwinfo_pid", "hwinfo_started", "restart_at"] {
            state.insert(k.into(), Value::Null);
        }
        state.insert("message".into(), json!(""));
        Watchdog {
            limit: cfg["restart_after_hours"].as_f64().unwrap_or(11.5) * 3600.0,
            interval: cfg["check_interval_s"].as_f64().unwrap_or(30.0).max(5.0),
            cfg,
            status_path,
            exe,
            exe_name,
            stop: Arc::new(AtomicBool::new(false)),
            state,
            first_seen: HashMap::new(),
            expired_since: None,
        }
    }

    fn num(&self, k: &str, default: f64) -> f64 {
        self.cfg[k].as_f64().unwrap_or(default)
    }

    fn args(&self) -> Vec<String> {
        self.cfg["args"].as_array().into_iter().flatten().map(as_text).collect()
    }

    /// Sleep up to `secs`, waking early when stopped. Returns false if stopped.
    fn nap(&self, secs: f64) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
        while Instant::now() < end {
            if self.stop.load(Ordering::SeqCst) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100).min(end.saturating_duration_since(Instant::now())));
        }
        !self.stop.load(Ordering::SeqCst)
    }

    pub fn publish(&mut self, kv: &[(&str, Value)]) {
        for (k, v) in kv {
            self.state.insert(k.to_string(), v.clone());
        }
        self.state.insert("updated".into(), json!(now_epoch()));
        if let Err(e) = write_json(&self.status_path, &Value::Object(self.state.clone())) {
            warn!("could not write status: {e}");
        }
    }

    fn resolve_exe(&self) -> Result<PathBuf, String> {
        let mut cands = vec![PathBuf::from(&self.exe)];
        for (var, rest) in CANDIDATES {
            if let Some(base) = std::env::var_os(var) {
                cands.push(PathBuf::from(base).join(rest));
            }
        }
        cands
            .into_iter()
            .find(|c| !c.as_os_str().is_empty() && c.is_file())
            .ok_or_else(|| format!("{} not found - set \"exe\" in {}", self.exe_name, config_path().display()))
    }

    fn ini_path(&self, exe: &Path) -> PathBuf {
        match self.cfg["ini"].as_str().filter(|s| !s.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => exe.with_file_name("HWiNFO64.INI"),
        }
    }

    pub fn running(&mut self) -> Result<Vec<Proc>, String> {
        let mut procs: Vec<Proc> = process::find_pids(&self.exe_name)?
            .into_iter()
            .map(|pid| {
                // couldn't open it (not elevated?) - fall back to when we first saw it
                process::proc_info(pid).unwrap_or_else(|| Proc {
                    pid,
                    created: *self.first_seen.entry(pid).or_insert_with(now_epoch),
                    exe: String::new(),
                })
            })
            .collect();
        procs.sort_by(|a, b| a.created.total_cmp(&b.created));
        Ok(procs)
    }

    /// Wait until HWiNFO's shared memory is populated. Returns seconds taken, or None on timeout/stop.
    fn wait_ready(&self, timeout: f64) -> Option<f64> {
        let t0 = Instant::now();
        let mut last = None;
        while t0.elapsed().as_secs_f64() < timeout {
            match shm::read_raw() {
                Ok(_) => return Some(t0.elapsed().as_secs_f64()),
                Err(e) => last = Some(e),
            }
            if !self.nap(0.5) {
                return None;
            }
        }
        if let Some(e) = last {
            warn!("shared memory not ready after {timeout:.0} s: {e}");
        }
        None
    }

    pub fn start_hwinfo(&mut self) -> Result<Option<f64>, String> {
        let exe = self.resolve_exe()?;
        self.exe = exe.to_string_lossy().to_string();
        let keys: Vec<(String, String)> =
            self.cfg["ini_settings"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), as_text(v))).collect();
        if !keys.is_empty() {
            let ini = self.ini_path(&exe);
            match ini::update_ini(&ini, &keys, "Settings") {
                Ok(true) => info!("updated {}: {:?}", ini.display(), keys),
                Ok(false) => {}
                Err(e) => warn!("could not update {} ({e}) - HWiNFO may show its start-up dialogs", ini.display()),
            }
        }
        let args = self.args();
        info!("starting {} {}", exe.display(), args.join(" "));
        let exe_s = self.exe.clone();
        self.publish(&[("state", json!("restarting")), ("exe", json!(exe_s)), ("message", json!("starting HWiNFO"))]);
        process::launch(&exe, &args)?;
        let took = self.wait_ready(self.num("ready_timeout_s", 90.0));
        if let Some(t) = took {
            info!("shared memory is up after {t:.1} s");
        }
        Ok(took)
    }

    pub fn cycle(&mut self, p: &Proc, why: &str) -> Result<(), String> {
        info!("restarting HWiNFO (PID {}, up {:.2} h): {why}", p.pid, (now_epoch() - p.created) / 3600.0);
        self.publish(&[("state", json!("restarting")), ("message", json!(why))]);
        let t0 = Instant::now();
        if !process::terminate(p, self.num("kill_timeout_s", 10.0)) {
            return Err(format!("could not terminate HWiNFO (PID {}) - is the watchdog elevated?", p.pid));
        }
        self.nap(self.num("settle_s", 1.0));
        let took = self.start_hwinfo()?;
        let restarts = self.state.get("restarts").and_then(Value::as_u64).unwrap_or(0) + 1;
        self.publish(&[("restarts", json!(restarts)), ("last_restart", json!(now_epoch()))]);
        if took.is_some() {
            info!("restart #{restarts} complete, data gap {:.1} s", t0.elapsed().as_secs_f64());
        }
        Ok(())
    }

    /// One check. Returns how long to sleep before the next one.
    pub fn step(&mut self) -> Result<f64, String> {
        let procs = self.running()?;
        let Some(p) = procs.into_iter().next() else {
            self.expired_since = None;
            let other = read_json(&self.status_path).unwrap_or(Value::Null);
            if other["pid"].as_u64() != Some(std::process::id() as u64)
                && other["state"] == "restarting"
                && now_epoch() - other["updated"].as_f64().unwrap_or(0.0) < self.num("ready_timeout_s", 90.0) + 30.0
            {
                return Ok(5.0); // a one-shot `restart-now` is mid-restart: don't launch a second copy
            }
            if !self.cfg["keep_running"].as_bool().unwrap_or(true) {
                self.publish(&[
                    ("state", json!("waiting")),
                    ("hwinfo_pid", Value::Null),
                    ("restart_at", Value::Null),
                    ("message", json!("HWiNFO is not running")),
                ]);
                return Ok(self.interval);
            }
            info!("HWiNFO is not running");
            self.start_hwinfo()?;
            return Ok(2.0); // pick up the new process quickly
        };
        if !p.exe.is_empty() {
            self.exe = p.exe.clone();
        }
        let restart_at = p.created + self.limit;
        let now = now_epoch();
        if now >= restart_at {
            self.cycle(&p, &format!("uptime limit ({:.1} h) reached", self.limit / 3600.0))?;
            return Ok(2.0);
        }
        match shm::read_raw() {
            Err(e) if e.kind == Kind::Expired => {
                let since = *self.expired_since.get_or_insert(now);
                if now - since > 20.0 {
                    self.cycle(&p, "shared memory expired early")?;
                    return Ok(2.0);
                }
            }
            _ => self.expired_since = None,
        }
        let exe = self.exe.clone();
        self.publish(&[
            ("state", json!("monitoring")),
            ("exe", json!(exe)),
            ("hwinfo_pid", json!(p.pid)),
            ("hwinfo_started", json!(p.created)),
            ("restart_at", json!(restart_at)),
            ("message", json!("")),
        ]);
        Ok(1.0_f64.max(self.interval.min(restart_at - now)))
    }

    pub fn run(&mut self) {
        info!(
            "watchdog started (PID {}, elevated={}, restart after {:.2} h)",
            std::process::id(),
            process::is_admin(),
            self.limit / 3600.0
        );
        if !process::is_admin() {
            warn!(
                "not elevated: killing/starting HWiNFO will fail or show UAC prompts - use `hwviewer watchdog install`"
            );
        }
        // FPS for the OSD when RTSS isn't running (PresentMon needs the admin rights this process has)
        let port = fps_helper::spawn(presentmon_path(&self.cfg));
        self.publish(&[("fps_port", json!(port))]);
        let mut backoff = 0.0_f64;
        while !self.stop.load(Ordering::SeqCst) {
            // a panic in one check must not end the watchdog
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.step()))
                .unwrap_or_else(|_| Err("internal error (panic) in watchdog step".into()));
            let wait = match r {
                Ok(w) => {
                    backoff = 0.0;
                    w
                }
                Err(e) => {
                    backoff = 300.0_f64.min(5.0_f64.max(backoff * 2.0));
                    error!("{e} (retrying in {backoff:.0} s)");
                    self.publish(&[("state", json!("error")), ("message", json!(e))]);
                    backoff
                }
            };
            self.nap(wait);
        }
        self.publish(&[("state", json!("stopped")), ("message", json!(""))]);
        info!("watchdog stopped");
    }
}

// --------------------------------------------------------------------------- CLI: run / install / status
#[derive(clap::Subcommand, Clone, Copy, PartialEq)]
pub enum WdCmd {
    /// Run in the foreground (default; should be elevated)
    Run,
    /// Register + start the elevated logon task (asks for admin once)
    Install,
    /// Stop and remove the scheduled task (HWiNFO keeps running)
    Uninstall,
    /// Show the task state and the last status
    Status,
    /// Restart HWiNFO once, then exit (asks for admin)
    RestartNow,
}

#[derive(clap::Args, Clone)]
pub struct WdArgs {
    #[command(subcommand)]
    pub cmd: Option<WdCmd>,
    /// watchdog.json to use (default: <data>/watchdog.json)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[arg(long, hide = true, global = true)]
    pub for_user: Option<String>,
    #[arg(long, hide = true, global = true)]
    pub pause: bool,
    /// Tests only: skip the single-instance mutex (e.g. a stand-in run next to the real watchdog)
    #[arg(long, hide = true, global = true)]
    pub allow_multiple: bool,
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn powershell(script: &str) -> (bool, String) {
    use std::os::windows::process::CommandExt;
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .creation_flags(process::CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => (
            o.status.success(),
            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string(),
        ),
        Err(e) => (false, e.to_string()),
    }
}

fn current_user() -> String {
    format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_else(|_| ".".into()),
        std::env::var("USERNAME").unwrap_or_default()
    )
}

fn install(user: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let script = [
        "$ErrorActionPreference = 'Stop'".to_string(),
        // replacing a running task (an upgrade, or the old Python watchdog): stop it first, or the new one is
        // refused by -MultipleInstances IgnoreNew and the old one keeps the single-instance mutex
        format!("Stop-ScheduledTask -TaskName {} -ErrorAction SilentlyContinue", ps_quote(TASK_NAME)),
        format!(
            "$a = New-ScheduledTaskAction -Execute {} -Argument 'watchdog run' -WorkingDirectory {}",
            ps_quote(&exe.to_string_lossy()),
            ps_quote(&dir.to_string_lossy())
        ),
        format!("$t = New-ScheduledTaskTrigger -AtLogOn -User {}", ps_quote(user)),
        "$s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries \
         -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew \
         -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)"
            .to_string(),
        format!("$p = New-ScheduledTaskPrincipal -UserId {} -LogonType Interactive -RunLevel Highest", ps_quote(user)),
        format!(
            "Register-ScheduledTask -TaskName {} -Action $a -Trigger $t -Settings $s -Principal $p -Force | Out-Null",
            ps_quote(TASK_NAME)
        ),
        format!("Start-ScheduledTask -TaskName {}", ps_quote(TASK_NAME)),
    ]
    .join("\n");
    let (ok, out) = powershell(&script);
    if !ok {
        error!("watchdog install failed: {out}");
        return Err(format!("could not register the task:\n{out}"));
    }
    info!("installed scheduled task '{TASK_NAME}' for {user}: \"{}\" watchdog run", exe.display());
    println!("Installed and started scheduled task '{TASK_NAME}' for {user} (runs at logon, elevated).");
    println!("Command: \"{}\" watchdog run", exe.display());
    Ok(())
}

/// Removing a task that isn't there is fine (the uninstaller always calls this).
fn uninstall() -> Result<(), String> {
    let t = ps_quote(TASK_NAME);
    let (ok, out) = powershell(&format!(
        "Stop-ScheduledTask -TaskName {t} -ErrorAction SilentlyContinue; \
         Unregister-ScheduledTask -TaskName {t} -Confirm:$false -ErrorAction SilentlyContinue"
    ));
    if !ok {
        error!("watchdog uninstall failed: {out}");
        return Err(format!("could not remove the task:\n{out}"));
    }
    info!("removed scheduled task '{TASK_NAME}'");
    println!("Removed scheduled task '{TASK_NAME}'.");
    Ok(())
}

/// (state, command line) of the registered task, in one PowerShell call.
pub fn task_info() -> Option<(String, String)> {
    let (ok, out) = powershell(&format!(
        "$t = Get-ScheduledTask -TaskName {} -ErrorAction Stop; $a = $t.Actions[0]; \
         \"$($t.State)`n$($a.Execute) $($a.Arguments)\"",
        ps_quote(TASK_NAME)
    ));
    if !ok {
        return None;
    }
    let (state, cmd) = out.split_once('\n').unwrap_or((&out, ""));
    Some((state.trim().to_string(), cmd.trim().to_string()))
}

/// The registered task's command line, e.g. to spot a task still pointing at the Python script.
pub fn task_command() -> Option<String> {
    let (ok, out) = powershell(&format!(
        "$a = (Get-ScheduledTask -TaskName {} -ErrorAction Stop).Actions[0]; \"$($a.Execute) $($a.Arguments)\"",
        ps_quote(TASK_NAME)
    ));
    ok.then_some(out)
}

fn status() {
    let (ok, out) =
        powershell(&format!("(Get-ScheduledTask -TaskName {} -ErrorAction Stop).State", ps_quote(TASK_NAME)));
    println!("scheduled task: {}", if ok { out } else { "not installed".into() });
    if let Some(cmd) = task_command() {
        println!("task command:   {cmd}");
    }
    let Some(st) = read_json(&status_path()) else {
        println!("no status yet ({})", status_path().display());
        return;
    };
    let t = |k: &str| st[k].as_f64().map(crate::log::local_time).unwrap_or_else(|| "-".into());
    let msg = st["message"].as_str().filter(|m| !m.is_empty()).map(|m| format!(" - {m}")).unwrap_or_default();
    println!(
        "watchdog: {}{msg} (updated {:.0} s ago)",
        st["state"].as_str().unwrap_or("?"),
        now_epoch() - st["updated"].as_f64().unwrap_or(0.0)
    );
    println!(
        "HWiNFO:   PID {}, started {}",
        st["hwinfo_pid"].as_u64().map(|p| p.to_string()).unwrap_or("-".into()),
        t("hwinfo_started")
    );
    println!(
        "restart:  next {}, last {}, total {}",
        t("restart_at"),
        t("last_restart"),
        st["restarts"].as_u64().unwrap_or(0)
    );
}

fn single_instance() -> Result<windows::Win32::Foundation::HANDLE, String> {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name = process::wide(MUTEX_NAME);
    // SAFETY: NUL-terminated name; the handle is intentionally held for the process lifetime.
    let h = unsafe { CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr())) }.map_err(|e| e.to_string())?;
    // SAFETY: reads the calling thread's last-error value set by CreateMutexW.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err("the watchdog is already running".into());
    }
    Ok(h)
}

pub fn cli(a: WdArgs) -> Result<(), String> {
    let cmd = a.cmd.unwrap_or(WdCmd::Run);
    let r = (|| {
        if cmd == WdCmd::Status {
            status();
            return Ok(());
        }
        if matches!(cmd, WdCmd::Install | WdCmd::Uninstall | WdCmd::RestartNow) && !process::is_admin() {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let sub = match cmd {
                WdCmd::Install => "install",
                WdCmd::Uninstall => "uninstall",
                _ => "restart-now",
            };
            let mut args: Vec<String> =
                vec!["watchdog".into(), sub.into(), "--pause".into(), "--for-user".into(), current_user()];
            if let Some(c) = &a.config {
                args.extend(["--config".into(), c.to_string_lossy().to_string()]);
            }
            process::shell_runas(&exe, &args, exe.parent().unwrap_or(Path::new(".")), 1)?;
            println!("Continuing in the elevated window...");
            return Ok(());
        }
        crate::log::init("watchdog"); // also for install/uninstall: the installer runs them hidden
        match cmd {
            WdCmd::Install => return install(&a.for_user.clone().unwrap_or_else(current_user)),
            WdCmd::Uninstall => return uninstall(),
            _ => {}
        }
        let cfg_path = a.config.clone().unwrap_or_else(config_path);
        let mut wd = Watchdog::new(load_config(&cfg_path), status_path());
        if cmd == WdCmd::RestartNow {
            let procs = wd.running()?;
            return match procs.first() {
                Some(p) => wd.cycle(&p.clone(), "manual restart"),
                None => wd.start_hwinfo().map(|_| ()),
            };
        }
        let _mutex = if a.allow_multiple { None } else { Some(single_instance()?) };
        wd.run();
        Ok(())
    })();
    if a.pause {
        if let Err(e) = &r {
            eprintln!("error: {e}");
        }
        println!("\nPress Enter to close...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    r
}
