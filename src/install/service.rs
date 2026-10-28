//! `agentmail install --service` / `uninstall --service`: run the daemon
//! as a systemd user service.
//!
//! Install writes the unit from docs/daemon.md (its "Running the daemon under
//! systemd" section, verbatim but for `ExecStart`, which runs this binary),
//! has systemd load the unit, enable it and start it, then stops the daemon
//! started on demand, if one runs, so that the service's takes over. Uninstall disables
//! and stops the service, removes the unit and has systemd forget it; with
//! the unit already removed by hand, it cleans up what systemd still has
//! of the service (see [`orphan`]). Neither discards a unit of the user's
//! (see [`plan`]). Both first check that the user's systemd manager is
//! reachable, and otherwise give the steps to do
//! by hand. `AGENTMAIL_SYSTEMCTL` names the `systemctl` to run (tests).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::{Change, Env, backup_path, created_marker_path, read_opt, shell_quote};

/// The unit's name, as `systemctl` takes it.
pub const NAME: &str = "agentmail";

/// docs/daemon.md's unit, `ExecStart` aside (`{exec}`).
const UNIT: &str = "\
[Unit]
Description=agentmail local agent mail daemon
# Tolerate bursts of stops (e.g. several clients upgrading at once) but
# still give up on a daemon that fails at every start; see below.
StartLimitIntervalSec=60s
StartLimitBurst=30

[Service]
ExecStart={exec} daemon run --wait-for-lock
Restart=always
RestartSec=500ms
# Signal only the daemon on stop; see below.
KillMode=mixed

[Install]
WantedBy=default.target
";

/// The variables that move the daemon's data directory and socket away
/// from their defaults.
pub const LOCATION_VARS: [&str; 2] = ["AGENTMAIL_DATA_DIR", "AGENTMAIL_SOCKET"];

/// The variables the daemon's data directory and socket depend on (see
/// [`crate::paths::Paths::from_vars`]).
const PATH_VARS: [&str; 5] = [
    "AGENTMAIL_DATA_DIR",
    "AGENTMAIL_SOCKET",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "HOME",
];

/// The service's environment, as far as the daemon's location goes: the
/// values of `PATH_VARS` its daemon gets (see [`service_env`]).
pub type ServiceEnv = HashMap<String, String>;

/// Of [`LOCATION_VARS`], those set in `env`.
pub fn location_overrides(env: &Env) -> Vec<&'static str> {
    LOCATION_VARS
        .into_iter()
        .filter(|k| env.var(k).is_some())
        .collect()
}

/// Whether two values of a variable the daemon's location depends on name
/// the same location: both unset (or empty), or the same path.
fn same_value(a: Option<&str>, b: Option<&str>) -> bool {
    fn path(v: Option<&str>) -> Option<&Path> {
        v.filter(|v| !v.is_empty()).map(Path::new)
    }
    path(a) == path(b)
}

/// `env`'s value of the variable `k` ([`Env::home`] for `HOME`).
fn here_value<'a>(env: &'a Env, k: &str) -> Option<&'a str> {
    if k == "HOME" {
        env.home.to_str()
    } else {
        env.var(k)
    }
}

/// The environment the systemd user manager passes to the units it starts,
/// the service's daemon among them (`systemctl --user show-environment`,
/// which `import-environment` and `set-environment` change), with `HOME`
/// defaulting to `env`'s. Nothing when the
/// manager cannot be asked, or the value of a variable the daemon's
/// location depends on ([`PATH_VARS`]) cannot be read (one with special
/// characters is shown quoted, `$'...'`). Other variables' quoted values
/// (say, a multi-line one a desktop session imported) are left out: they
/// matter to nothing here, and a line of one may not even be a variable.
fn manager_env(env: &Env) -> Option<ServiceEnv> {
    let out = Command::new(systemctl(env))
        .args(["--user", "show-environment"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut manager = HashMap::new();
    for (k, v) in text.lines().filter_map(|l| l.split_once('=')) {
        if !PATH_VARS.contains(&k) {
            continue;
        }
        if v.starts_with("$'") {
            return None;
        }
        manager.insert(k.to_string(), v.to_string());
    }
    manager
        .entry("HOME".to_string())
        .or_insert(env.home.to_str()?.to_string());
    Some(manager)
}

/// The words of a list property `systemctl show` prints (say,
/// `Environment="A=a b" C=d`): separated by spaces, each bare or
/// double-quoted with `\` escapes. Nothing when it holds anything else
/// (say, a `$'...'` word).
fn show_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if_eq(&' ').is_some() {}
        let Some(first) = chars.next() else {
            return Some(words);
        };
        let mut word = String::new();
        if first == '"' {
            loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => word.push(chars.next()?),
                    ch => word.push(ch),
                }
            }
            if chars.peek().is_some_and(|&c| c != ' ') {
                return None;
            }
        } else {
            word.push(first);
            while let Some(ch) = chars.next_if(|&c| c != ' ') {
                word.push(ch);
            }
            if word.contains(['"', '\'', '\\']) {
                return None;
            }
        }
        words.push(word);
    }
}

/// The variables an environment file (`EnvironmentFile=`) sets, of those
/// in [`PATH_VARS`]: `KEY=VALUE` lines, the value bare or in matching
/// quotes, `#` and `;` lines comments. Nothing when one of those variables
/// is set in a way read differently by systemd (escapes, a value spanning
/// lines), or a line continues the next.
fn env_file_vars(text: &str) -> Option<Vec<(String, String)>> {
    let mut vars = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.ends_with('\\') {
            return None;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        let quoted = v.starts_with(['"', '\'']);
        let closed = quoted && v.len() >= 2 && v.ends_with(&v[..1]);
        if quoted && !closed {
            // A value spanning lines: what follows is not a variable.
            return None;
        }
        if !PATH_VARS.contains(&k) {
            continue;
        }
        let single = closed && v.starts_with('\'');
        let v = if closed { &v[1..v.len() - 1] } else { v };
        let special: &[char] = if single {
            &['\'']
        } else {
            &['"', '\'', '\\', '$']
        };
        if v.contains(special) {
            return None;
        }
        vars.push((k.to_string(), v.to_string()));
    }
    Some(vars)
}

/// The variables of [`PATH_VARS`] the service's unit sets: its
/// `Environment=` lines, drop-ins included (`systemctl --user show -p
/// Environment`, specifiers expanded), then its `EnvironmentFile=`s, which
/// override them (a missing file is skipped: systemd then starts the
/// service without it, if at all). Nothing when systemd cannot be asked or
/// a value cannot be read. For a unit systemd has not loaded, none.
fn unit_env(env: &Env) -> Option<Vec<(String, String)>> {
    let out = Command::new(systemctl(env))
        .args([
            "--user",
            "show",
            "-p",
            "Environment",
            "-p",
            "EnvironmentFiles",
            NAME,
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut vars = Vec::new();
    for words in text.lines().filter_map(|l| l.strip_prefix("Environment=")) {
        for w in show_words(words)? {
            if let Some((k, v)) = w.split_once('=')
                && PATH_VARS.contains(&k)
            {
                vars.push((k.to_string(), v.to_string()));
            }
        }
    }
    for file in text
        .lines()
        .filter_map(|l| l.strip_prefix("EnvironmentFiles="))
    {
        let file = file
            .rsplit_once(" (ignore_errors=")
            .map_or(file, |(f, _)| f);
        match std::fs::read_to_string(file) {
            Ok(t) => vars.extend(env_file_vars(&t)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(vars)
}

/// The environment the service's daemon gets, as far as its location goes
/// (`PATH_VARS`): the systemd user manager's (see `manager_env`), with
/// what the unit and its drop-ins set over it (see `unit_env`). Nothing
/// when either cannot be told.
pub fn service_env(env: &Env) -> Option<ServiceEnv> {
    let mut vars = manager_env(env)?;
    vars.extend(unit_env(env)?);
    Some(vars)
}

/// The daemon location here: `env`'s, with its [`LOCATION_VARS`].
fn here_paths(env: &Env) -> Option<crate::paths::Paths> {
    crate::paths::Paths::from_vars(|k| here_value(env, k).map(std::ffi::OsString::from)).ok()
}

/// The default daemon location here: `env`'s, but for [`LOCATION_VARS`].
pub fn default_paths(env: &Env) -> Option<crate::paths::Paths> {
    crate::paths::Paths::from_vars(|k| {
        if LOCATION_VARS.contains(&k) {
            None
        } else {
            here_value(env, k).map(std::ffi::OsString::from)
        }
    })
    .ok()
}

/// Where the service's daemon runs: from its environment `service` (see
/// [`service_env`]), or, when that cannot be told, the default location
/// here (see [`default_paths`]). Nothing when it cannot be resolved (say,
/// a relative `AGENTMAIL_DATA_DIR`, which the daemon refuses).
pub fn service_paths(env: &Env, service: Option<&ServiceEnv>) -> Option<crate::paths::Paths> {
    match service {
        Some(s) => crate::paths::Paths::from_vars(|k| s.get(k).map(std::ffi::OsString::from)).ok(),
        None => default_paths(env),
    }
}

/// How `daemon stop`, run by install, is told the location `service` (see
/// [`service_paths`]): variables to unset (`None`) or set. Nothing when the
/// location here is that one; [`LOCATION_VARS`] set here unset when the
/// default location here is; else both set to it.
pub type StopEnv = Vec<(&'static str, Option<PathBuf>)>;

/// The [`StopEnv`] for the service's location `service`.
pub fn stop_env(env: &Env, service: &crate::paths::Paths) -> StopEnv {
    if here_paths(env).as_ref() == Some(service) {
        Vec::new()
    } else if default_paths(env).as_ref() == Some(service) {
        unset_env(&location_overrides(env))
    } else {
        vec![
            ("AGENTMAIL_DATA_DIR", Some(service.data_dir.clone())),
            ("AGENTMAIL_SOCKET", Some(service.socket.clone())),
        ]
    }
}

/// The [`StopEnv`] that unsets `vars`.
pub fn unset_env(vars: &[&'static str]) -> StopEnv {
    vars.iter().map(|&k| (k, None)).collect()
}

/// `bin daemon stop` with `stop` (see [`StopEnv`]), as the user would type
/// it.
pub fn stop_command(bin: &Path, stop: &StopEnv) -> String {
    let mut env_part = String::new();
    for (k, v) in stop {
        let arg = match v {
            None => format!("-u {k}"),
            Some(v) => shell_quote(&format!("{k}={}", v.to_string_lossy())),
        };
        env_part.push_str(&arg);
        env_part.push(' ');
    }
    let env_part = if env_part.is_empty() {
        env_part
    } else {
        format!("env {env_part}")
    };
    format!(
        "{env_part}{} daemon stop",
        shell_quote(&bin.to_string_lossy())
    )
}

/// Whether a daemon started on demand runs at `p` (the service's location,
/// see [`service_paths`]): one holds the lock there, and its lock record
/// does not say it is supervised (a daemon from before lock records, which
/// writes none, is taken as on demand). Install stops only such a daemon:
/// a supervised one is the service's own (or another supervisor's), and
/// stopping it would only drop its clients while it is restarted.
///
/// A daemon that has just taken the lock records itself a moment later:
/// until then the lock file holds its predecessor's record, whose process
/// is gone, or nothing (a lock file just created), or, while the record is
/// rewritten, one that does not decode. The record is read as clients read
/// it (see [`crate::client::settled_lock_record`]), and any of those is
/// read again for up to a second before it is believed.
pub fn on_demand_daemon(p: &crate::paths::Paths) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let pause = || std::thread::sleep(std::time::Duration::from_millis(20));
    loop {
        if !p.lock_held() {
            return false;
        }
        let waiting = std::time::Instant::now() < deadline;
        match crate::client::settled_lock_record_blocking(p) {
            None if waiting => pause(),
            None => return true,
            Some(r) if crate::procinfo::proc_id(r.pid).is_none() && waiting => pause(),
            Some(r) => return !r.supervised,
        }
    }
}

/// Of [`LOCATION_VARS`] set here, those the service does not get the same
/// value of (see [`service_env`]; all of them when that is not known).
pub fn overrides_unlike_service(env: &Env, service: Option<&ServiceEnv>) -> Vec<&'static str> {
    location_overrides(env)
        .into_iter()
        .filter(|k| service.is_none_or(|s| !same_value(s.get(*k).map(String::as_str), env.var(k))))
        .collect()
}

/// When the service's daemon runs elsewhere than the daemon clients here
/// use, beyond what the variables `noted` (see
/// [`overrides_unlike_service`], noted on their own) explain: a note
/// saying where, and which variables the service gets differently from
/// this shell. Say, the manager's `XDG_RUNTIME_DIR` (which an ssh session
/// without pam_systemd may not share), or an `AGENTMAIL_DATA_DIR` a
/// drop-in sets and this shell does not. Nothing when the service's
/// environment cannot be told (see [`service_env`]).
pub fn location_mismatch(
    env: &Env,
    service: Option<&ServiceEnv>,
    noted: &[&str],
) -> Option<String> {
    let mut vars = service?.clone();
    // As if the service got the noted variables as set here.
    for k in noted {
        match env.var(k) {
            Some(v) => vars.insert(k.to_string(), v.to_string()),
            None => vars.remove(*k),
        };
    }
    let paths = |vars: &ServiceEnv| {
        crate::paths::Paths::from_vars(|k| vars.get(k).map(std::ffi::OsString::from)).ok()
    };
    let theirs = paths(&vars)?;
    let here = here_paths(env)?;
    let mut diff = Vec::new();
    if theirs.socket != here.socket {
        diff.push(format!(
            "socket {} (here: {})",
            theirs.socket.display(),
            here.socket.display()
        ));
    }
    if theirs.data_dir != here.data_dir {
        diff.push(format!(
            "data directory {} (here: {})",
            theirs.data_dir.display(),
            here.data_dir.display()
        ));
    }
    if diff.is_empty() {
        return None;
    }
    let differing: Vec<&str> = PATH_VARS
        .into_iter()
        .filter(|k| !same_value(vars.get(*k).map(String::as_str), here_value(env, k)))
        .collect();
    // Those that move the location on their own, if any do.
    let moving: Vec<&str> = differing
        .iter()
        .copied()
        .filter(|k| {
            let mut v = vars.clone();
            match here_value(env, k) {
                Some(x) => v.insert(k.to_string(), x.to_string()),
                None => v.remove(*k),
            };
            paths(&v).as_ref() != Some(&theirs)
        })
        .collect();
    let named = if moving.is_empty() { differing } else { moving };
    Some(format!(
        "the service's daemon uses the {}: the service gets {} differently from this shell (from \
         the systemd user manager's environment, or its unit and drop-ins), so `daemon stop` and \
         clients run here use a daemon of their own, not the service's. Set the same values \
         here, or give the service this shell's, in a drop-in (`systemctl --user edit {NAME}`) \
         or with `systemctl --user set-environment`.",
        diff.join(" and the "),
        named.join(", ")
    ))
}

/// The directory user units go in under the config root `XDG_CONFIG_HOME`
/// (`xdg`, when absolute), else `home`'s `.config`.
fn unit_dir(xdg: Option<&str>, home: &Path) -> PathBuf {
    xdg.map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd/user")
}

/// The systemd user manager's unit search path, as it reports it
/// (`systemctl --user show -p UnitPath`, space-separated). The manager
/// builds it from its own process environment (`XDG_CONFIG_HOME`, `HOME`),
/// which `show-environment` does not show: that is the environment it
/// passes to the units it starts. Nothing when the manager cannot be asked
/// or reports no search path.
fn manager_unit_path(env: &Env) -> Option<String> {
    let out = Command::new(systemctl(env))
        .args(["--user", "show", "-p", "UnitPath"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("UnitPath="))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Whether the directory `dir` is on the search path `search` (directories
/// separated by spaces; `dir` may hold spaces too), as is or once symlinks
/// are resolved.
fn on_search_path(search: &str, dir: &Path) -> bool {
    let padded = format!(" {search} ");
    let listed = |d: &Path| {
        d.to_str()
            .is_some_and(|d| padded.contains(&format!(" {d} ")))
    };
    if listed(dir) {
        return true;
    }
    let Ok(canon) = std::fs::canonicalize(dir) else {
        return false;
    };
    listed(&canon)
        || search
            .split(' ')
            .filter_map(|d| std::fs::canonicalize(d).ok())
            .any(|d| d == canon)
}

/// When the systemd user manager does not load user units from the
/// directory the unit goes in here (its own `XDG_CONFIG_HOME`, or `HOME`,
/// differs from this shell's), as its unit search path says (see
/// `manager_unit_path`): why agentmail refuses, since the manager would
/// never see the unit (and `enable` it, which the manager does). Nothing
/// when the directory is on that path, or the manager cannot be asked.
pub fn config_mismatch(env: &Env) -> Option<String> {
    let search = manager_unit_path(env)?;
    let ours = unit_path(env).parent()?.to_path_buf();
    if on_search_path(&search, &ours) {
        return None;
    }
    Some(format!(
        "the systemd user manager does not load user units from {}, where agentmail would put \
         the unit (from XDG_CONFIG_HOME or HOME here), so it would never see it: the manager's \
         own XDG_CONFIG_HOME or HOME differs from this shell's (`systemctl --user show -p \
         UnitPath` lists where it looks). Run this with XDG_CONFIG_HOME and HOME set as the \
         manager has them.",
        ours.display()
    ))
}

/// The binary the unit runs: this one, by the path it was invoked as
/// (`argv0`, taken from the working directory or found on `PATH`) when
/// that names this same file (so a symlink, say to a versioned install,
/// stays a symlink), else by the path the system reports
/// (`std::env::current_exe`, symlinks resolved). Fails when this binary
/// was removed or replaced while running.
pub fn binary(env: &Env, argv0: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the agentmail binary")?;
    if exe.to_string_lossy().ends_with(" (deleted)") || !exe.is_file() {
        bail!(
            "the running agentmail binary {} was removed or replaced; run the installed one",
            exe.display()
        );
    }
    let canon = std::fs::canonicalize(&exe).ok();
    let same = |p: &PathBuf| canon.is_some() && std::fs::canonicalize(p).ok() == canon;
    Ok(argv0
        .and_then(|a| invoked(env, Path::new(a)))
        .filter(|p| same(p))
        .unwrap_or(exe))
}

/// The absolute path `argv0` runs: taken from the working directory when it
/// holds a `/`, else the first match on `PATH`. `None` when it cannot be
/// told without resolving symlinks (a `..` in it).
fn invoked(env: &Env, argv0: &Path) -> Option<PathBuf> {
    if argv0.as_os_str().is_empty() {
        return None;
    }
    let p = if argv0.as_os_str().as_encoded_bytes().contains(&b'/') {
        env.cwd.join(argv0)
    } else {
        env.var("PATH")?
            .split(':')
            .map(Path::new)
            .filter(|d| d.is_absolute())
            .map(|d| d.join(argv0))
            .find(|p| p.is_file())?
    };
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    // Drops any `.` component.
    Some(p.components().collect())
}

/// Whether `text` is the unit agentmail writes, for any binary.
pub fn is_generated(text: &str) -> bool {
    generated_exec(text).is_some()
}

/// The `ExecStart` binary of `text`, when it is the unit agentmail writes
/// (see [`is_generated`]).
fn generated_exec(text: &str) -> Option<&str> {
    let (head, tail) = UNIT.split_once("{exec}").expect("the unit has {exec}");
    text.strip_prefix(head)
        .and_then(|t| t.strip_suffix(tail))
        .filter(|exec| !exec.is_empty() && !exec.contains('\n'))
}

/// Whether `exec` is an `ExecStart` binary agentmail writes (see
/// [`exec_arg`]): an absolute path with every `%` doubled, quoted only
/// when it must be. Not, say, docs/daemon.md's `%h/.cargo/bin/agentmail`.
fn written_exec(exec: &str) -> bool {
    let unquoted = match exec.strip_prefix('"').and_then(|e| e.strip_suffix('"')) {
        Some(inner) => {
            let mut out = String::new();
            let mut chars = inner.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => match chars.next() {
                        Some(e @ ('\\' | '"')) => out.push(e),
                        _ => return false,
                    },
                    '"' => return false,
                    ch => out.push(ch),
                }
            }
            out
        }
        None => exec.to_string(),
    };
    let mut path = String::new();
    let mut chars = unquoted.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' && chars.next() != Some('%') {
            return false;
        }
        path.push(ch);
    }
    exec_arg(Path::new(&path)).is_ok_and(|e| e == exec)
}

/// The unit that runs `bin` (an absolute path) as the daemon.
pub fn unit(bin: &Path) -> Result<String> {
    Ok(UNIT.replace("{exec}", &exec_arg(bin)?))
}

/// `bin` as the first word of a systemd command line: `%` (specifiers)
/// doubled, and double-quoted, with `\` and `"` escaped, when it holds
/// anything but plain path characters. `$` is left as is: systemd expands
/// variables only in the arguments after the executable, so it would not
/// turn a doubled `$$` in the path back into `$` (checked with
/// `systemd-analyze verify`).
fn exec_arg(bin: &Path) -> Result<String> {
    if !bin.is_absolute() {
        bail!(
            "the agentmail binary path {} is not absolute",
            bin.display()
        );
    }
    let s = bin
        .to_str()
        .ok_or_else(|| anyhow!("the agentmail binary path {} is not UTF-8", bin.display()))?;
    if s.chars().any(char::is_control) {
        bail!(
            "the agentmail binary path {} holds a control character, which a systemd unit cannot",
            s.escape_debug()
        );
    }
    let s = s.replace('%', "%%");
    let plain = s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%,=$".contains(&b));
    Ok(if plain {
        s
    } else {
        format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
    })
}

/// Where the unit goes: `$XDG_CONFIG_HOME/systemd/user/agentmail.service`,
/// else under `~/.config`. A relative `XDG_CONFIG_HOME` is ignored, as the
/// XDG spec says and systemd does.
pub fn unit_path(env: &Env) -> PathBuf {
    unit_dir(env.var("XDG_CONFIG_HOME"), &env.home).join(format!("{NAME}.service"))
}

/// The drop-in directory of the unit at `path` (`agentmail.service.d`,
/// where `systemctl --user edit` puts `override.conf`). Install and
/// uninstall never touch it, so the user's settings there outlive both.
pub fn dropin_dir(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".d");
    path.with_file_name(name)
}

/// The settings of the unit `text` (one install replaces) that agentmail's
/// unit for `bin` lacks, `ExecStart` aside: say, an `Environment=` line,
/// which the service no longer gets once it is replaced.
pub fn dropped_settings(text: &str, bin: &Path) -> Vec<String> {
    let ours = unit(bin).unwrap_or_default();
    let ours: Vec<&str> = ours.lines().map(str::trim).collect();
    text.lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with(['#', ';', '['])
                && !l.starts_with("ExecStart=")
                && !ours.contains(l)
        })
        .map(str::to_string)
        .collect()
}

/// `k=v` as an `Environment=` line of a unit: quoted, with `\` and `"`
/// escaped and `%` (specifiers) doubled.
pub fn environment_line(k: &str, v: &str) -> String {
    let kv = format!("{k}={v}")
        .replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("Environment=\"{kv}\"")
}

/// The change that installs the unit for `bin` (none when it is already
/// there), or, on uninstall, removes it (none when it is not there). A
/// unit that was there before agentmail's is kept as its backup, also once
/// the service is uninstalled.
///
/// Only agentmail's own unit is replaced or removed as is: the unit it
/// created (its marker says so; whatever its `ExecStart`), or the one it
/// wrote over a unit of the user's, which is kept as the backup (its
/// `ExecStart` one agentmail writes, see `written_exec`). Any other
/// unit is the user's, also one written by hand from docs/daemon.md: install
/// saves it as the backup, replacing any older backup of agentmail's, and
/// uninstall saves it as the backup before removing it when it matches
/// docs/daemon.md's unit (see [`is_generated`]) or agentmail created it (it
/// was then edited). Both refuse when a different earlier unit is kept as
/// the backup, and uninstall refuses to remove any other unit.
pub fn plan(bin: &Path, env: &Env, install: bool) -> Result<Vec<Change>> {
    let path = unit_path(env);
    let before = read_opt(&path)?;
    let after = if install { Some(unit(bin)?) } else { None };
    if before == after {
        return Ok(Vec::new());
    }
    let mut c = Change {
        path,
        before,
        after,
        record_anew: false,
        unlink: false,
        save: None,
        keep_backup: true,
        restore: false,
    };
    let Some(text) = c.before.as_deref() else {
        return Ok(vec![c]);
    };
    let generated = generated_exec(text);
    let created = std::fs::symlink_metadata(created_marker_path(&c.path)).is_ok();
    if generated.is_some() && created {
        return Ok(vec![c]);
    }
    if install && created {
        // agentmail's unit, edited by the user beyond its `ExecStart`:
        // replacing it would silently drop what the edits configure (say,
        // the `Environment=` that moves the daemon's data directory).
        bail!(
            "{} is the unit agentmail installed, edited since; installing would drop these \
             edits, so agentmail leaves the unit and nothing was changed:\n{}\
             Keep such settings in a drop-in, which install and uninstall never touch: run \
             `systemctl --user edit {NAME}` (it saves {}), put them there, then remove the \
             unit (rm {}) and run `agentmail install --service` again.",
            c.path.display(),
            super::diff(&c),
            dropin_dir(&c.path).join("override.conf").display(),
            shell_quote(&c.path.to_string_lossy())
        );
    }
    let bak = backup_path(&c.path);
    let backup = match std::fs::read(&bak) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", bak.display()))),
    };
    let saved = backup.as_deref() == Some(text.as_bytes());
    if backup.is_some() && !saved {
        if generated.is_some_and(written_exec) {
            // agentmail's unit, written over the user's kept as the backup.
            return Ok(vec![c]);
        }
        if install {
            bail!(
                "{} is not the unit agentmail writes, and {} already keeps a different earlier \
                 unit; agentmail replaces neither. Move one of them away and run the command \
                 again. Nothing was changed.",
                c.path.display(),
                bak.display()
            );
        }
    }
    if install {
        // The user's unit is the original to keep, whatever an earlier
        // install recorded.
        c.record_anew = true;
    } else if backup.is_none() && (generated.is_some() || created) {
        // docs/daemon.md's unit, written by hand, or agentmail's with the
        // user's edits: removed, but kept.
        c.save = Some(bak);
    } else if !saved {
        bail!(
            "{} is not the unit agentmail installs (it was edited, or agentmail did not write \
             it), so agentmail leaves it; nothing was changed. To remove it anyway, run:\n{}",
            c.path.display(),
            removal_steps(&c.path)
        );
    }
    Ok(vec![c])
}

/// The commands that remove the unit at `path` by hand, with the files
/// agentmail keeps next to it: its marker (which a later install or
/// uninstall would trust) and, if any, the unit that was there before
/// agentmail's, kept as its backup.
fn removal_steps(path: &Path) -> String {
    let q = |p: &Path| shell_quote(&p.to_string_lossy());
    format!(
        "  systemctl --user disable --now {NAME}\n  rm {}\n  rm -f {}\n  systemctl --user \
         daemon-reload\nA unit kept as {} is the one that was there before agentmail's: to \
         restore it, move it back to {} before the daemon-reload; otherwise remove it too.\n",
        q(path),
        q(&created_marker_path(path)),
        backup_path(path).display(),
        path.display()
    )
}

/// The commands install (`install`) or uninstall runs after (install) or
/// around (uninstall) changing the unit, as the user would type them, for
/// `bin`, with `daemon stop` told the service's location by `stop` (see
/// [`stop_env`]), so that it stops the daemon there, the one the service
/// replaces. With
/// `restart` (install, the service running on anything but the unit as
/// install leaves it: the unit replaced, one removed by hand, or one a
/// failed install left it on; see [`runs_stale`]), the service is
/// restarted so that it runs the new unit: enabling a running service does
/// not (a stopped one `enable --now` starts on the new unit). With `stop_daemon`, install then stops any daemon run
/// on demand: last, so that the service's daemon, already waiting for the
/// lock, takes over as it exits, leaving no time for a client to start
/// another on demand. (Install passes `stop_daemon` only when such a daemon
/// runs; see [`on_demand_daemon`].)
pub fn steps(
    bin: &Path,
    stop: &StopEnv,
    install: bool,
    stop_daemon: bool,
    restart: bool,
) -> Vec<String> {
    if install {
        let mut s = vec![
            "systemctl --user daemon-reload".to_string(),
            format!("systemctl --user enable --now {NAME}"),
        ];
        if restart {
            s.push(format!("systemctl --user try-restart {NAME}"));
        }
        if stop_daemon {
            s.push(stop_command(bin, stop));
        }
        s
    } else {
        vec![
            format!("systemctl --user disable --now {NAME}"),
            "systemctl --user daemon-reload".to_string(),
        ]
    }
}

/// What systemd still has of the service once its unit file is gone (the
/// user removed it by hand): the service running, and the link `enable`
/// made in `default.target.wants`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub active: bool,
    pub link: Option<PathBuf>,
}

/// The link `systemctl --user enable` makes for the unit (its
/// `WantedBy=default.target`).
pub fn wants_link(env: &Env) -> PathBuf {
    let unit = unit_path(env);
    unit.parent()
        .expect("the unit path has a directory")
        .join("default.target.wants")
        .join(format!("{NAME}.service"))
}

/// `systemctl --user show` of the service with `args` (say, `-p` and the
/// properties wanted). An error when systemd cannot be asked.
fn show(env: &Env, args: &[&str]) -> Result<String> {
    let out = Command::new(systemctl(env))
        .args(["--user", "show"])
        .args(args)
        .arg(NAME)
        .output()
        .context("asking systemd about the service")?;
    if !out.status.success() {
        bail!("systemctl --user show {NAME} failed ({})", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The value of the property `k` in `systemctl show` output `text` (empty
/// when it is not there).
fn prop<'a>(text: &'a str, k: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
        .unwrap_or("")
}

/// Whether `a` and `b` name the same file, as they are or once symlinks
/// are resolved: in the whole path, or (the file itself may be gone, as a
/// unit removed by hand is) in its directory.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(x), Ok(y)) = (std::fs::canonicalize(a), std::fs::canonicalize(b))
        && x == y
    {
        return true;
    }
    let in_dir = |p: &Path| {
        Some(
            std::fs::canonicalize(p.parent()?)
                .ok()?
                .join(p.file_name()?),
        )
    };
    matches!((in_dir(a), in_dir(b)), (Some(x), Some(y)) if x == y)
}

/// Where systemd loads the service from: `None` when it has no unit file
/// for it (`LoadState=not-found`), else its `FragmentPath` (empty when it
/// has none). An error when systemd cannot be asked.
fn fragment(env: &Env) -> Result<Option<PathBuf>> {
    let text = show(env, &["-p", "LoadState,FragmentPath"])?;
    Ok((prop(&text, "LoadState") != "not-found")
        .then(|| PathBuf::from(prop(&text, "FragmentPath"))))
}

/// Install, with the service running and its unit file already as
/// agentmail would write it for `bin`: whether the running service is
/// still on something else, so that it must be restarted all the same
/// (say, a previous install wrote the unit over docs/daemon.md's, then its
/// `daemon-reload`, `enable` or `try-restart` failed). It is when systemd
/// has no unit file for it loaded (`LoadState=not-found`), has not
/// reloaded the file since it changed (`NeedDaemonReload=yes`), loads it
/// from another file (`FragmentPath`, compared with symlinks resolved), or
/// runs another binary (the loaded `ExecStart`'s `path=`), or when its
/// main process started before the unit file was last written
/// (`ExecMainStartTimestamp`, which catches a reload that ran but a restart
/// that did not). Asked before the unit is reloaded; whatever systemd
/// cannot tell counts for nothing, as when it cannot be asked at all.
pub fn runs_stale(env: &Env, bin: &Path) -> bool {
    let unit = unit_path(env);
    let Ok(text) = show(
        env,
        &["-p", "LoadState,FragmentPath,NeedDaemonReload,ExecStart"],
    ) else {
        return false;
    };
    let load = prop(&text, "LoadState");
    if load == "not-found" || prop(&text, "NeedDaemonReload") == "yes" {
        return true;
    }
    if load == "loaded" && !same_file(Path::new(prop(&text, "FragmentPath")), &unit) {
        return true;
    }
    let exec = prop(&text, "ExecStart");
    if let Some(path) = exec
        .split_once("path=")
        .and_then(|(_, rest)| rest.split(" ;").next())
        .filter(|p| !p.is_empty())
        && Path::new(path) != bin
    {
        return true;
    }
    started_before_written(env, &unit)
}

/// Whether the service's main process started before the unit file at
/// `unit` was last written (to the second; `systemctl show
/// --timestamp=unix`, systemd 248 and later). False when either is not
/// known.
fn started_before_written(env: &Env, unit: &Path) -> bool {
    let Ok(text) = show(env, &["--timestamp=unix", "-p", "ExecMainStartTimestamp"]) else {
        return false;
    };
    let Some(started) = prop(&text, "ExecMainStartTimestamp")
        .strip_prefix('@')
        .and_then(|t| t.split('.').next())
        .and_then(|t| t.parse::<u64>().ok())
    else {
        return false;
    };
    std::fs::metadata(unit)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|written| started < written.as_secs())
}

/// Uninstall, with no unit file: what systemd still has of agentmail's
/// service, if anything (see [`Orphan`]), and a note on a service named
/// `agentmail` it leaves alone as not agentmail's. The running service is
/// agentmail's only when systemd has no unit file for it any more, or
/// loads it from [`unit_path`] (not yet reloaded); one loaded from
/// elsewhere (say, `~/.local/share/systemd/user` or a package's) is the
/// user's. Likewise the `default.target.wants` link is agentmail's only
/// when it points to [`unit_path`] (it may dangle there). Both compare
/// with symlinks resolved, in the unit's directory too (see
/// `same_file`), as the unit search path check does.
pub fn orphan(env: &Env) -> (Option<Orphan>, Option<String>) {
    let unit = unit_path(env);
    let mut left = Vec::new();
    let wants = wants_link(env);
    let link = match std::fs::read_link(&wants) {
        Ok(target) => {
            let target = wants.parent().map_or(target.clone(), |d| d.join(&target));
            if same_file(&target, &unit) {
                Some(wants)
            } else {
                left.push(format!(
                    "the link {} points to {}, not agentmail's unit",
                    wants.display(),
                    target.display()
                ));
                None
            }
        }
        Err(_) => None,
    };
    let active = is_active(env)
        && match fragment(env) {
            Ok(None) => true,
            Ok(Some(f)) if same_file(&f, &unit) => true,
            Ok(Some(f)) => {
                left.push(format!(
                    "the running service {NAME} is loaded from {}, not from agentmail's unit",
                    if f.as_os_str().is_empty() {
                        "no unit file agentmail knows".to_string()
                    } else {
                        f.display().to_string()
                    }
                ));
                false
            }
            Err(e) => {
                left.push(format!(
                    "the service {NAME} is running, but whether it is agentmail's could not be \
                     told ({e:#})"
                ));
                false
            }
        };
    let note = (!left.is_empty()).then(|| {
        format!(
            "{}; agentmail did not install it, so it is left alone.",
            left.join(", and ")
        )
    });
    (
        (active || link.is_some()).then_some(Orphan { active, link }),
        note,
    )
}

/// The commands that clean up `o`, as the user would type them.
/// (`disable` cannot: with the unit file gone it fails, "Unit ... does not
/// exist", so the link is removed directly.)
pub fn orphan_steps(o: &Orphan) -> Vec<String> {
    let mut s = Vec::new();
    if o.active {
        s.push(format!("systemctl --user stop {NAME}"));
    }
    if let Some(l) = &o.link {
        s.push(format!("rm {}", shell_quote(&l.to_string_lossy())));
    }
    s.push("systemctl --user daemon-reload".to_string());
    s
}

/// Clean up `o` (see [`orphan_steps`]).
pub fn clean_orphan(env: &Env, o: &Orphan) -> Result<()> {
    if o.active {
        user_systemctl(env, &["stop", NAME])?;
    }
    if let Some(l) = &o.link {
        match std::fs::remove_file(l) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("removing {}", l.display())));
            }
        }
        // `systemctl disable` drops the directory once empty; so do we.
        if let Some(d) = l.parent() {
            let _ = std::fs::remove_dir(d);
        }
    }
    user_systemctl(env, &["daemon-reload"])
}

/// The `systemctl` to run: `AGENTMAIL_SYSTEMCTL`, else `systemctl`.
fn systemctl(env: &Env) -> String {
    env.var("AGENTMAIL_SYSTEMCTL")
        .unwrap_or("systemctl")
        .to_string()
}

/// Run `prog args`, failing with its output when it fails.
fn run(prog: &str, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(prog);
    cmd.args(args);
    run_cmd(cmd, &format!("{prog} {}", args.join(" ")))
}

/// Run `cmd` (`shown`), failing with its output when it fails.
fn run_cmd(mut cmd: Command, shown: &str) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {shown}"))?;
    if !out.status.success() {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if msg.is_empty() {
            msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        bail!("{shown} failed ({}): {msg}", out.status);
    }
    Ok(())
}

/// Run `systemctl --user args`.
fn user_systemctl(env: &Env, args: &[&str]) -> Result<()> {
    let mut all = vec!["--user"];
    all.extend_from_slice(args);
    run(&systemctl(env), &all)
}

/// Check that the user's systemd manager can run the service. A manager
/// that is up but `degraded` (some other unit failed; `is-system-running`
/// then exits 1) can; one that is `offline` or out of reach cannot.
/// `manual` is what to do by hand instead.
pub fn check(env: &Env, manual: &dyn Fn() -> String) -> Result<()> {
    match unavailable(env) {
        None => Ok(()),
        Some(why) => bail!("{why}; nothing was changed. {}", manual()),
    }
}

/// Why the user's systemd manager cannot run the service (see [`check`]),
/// or nothing when it can.
pub fn unavailable(env: &Env) -> Option<String> {
    let prog = systemctl(env);
    let out = Command::new(&prog)
        .args(["--user", "is-system-running"])
        .output();
    let state = match &out {
        Ok(o) => {
            let state = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let usable = o.status.success()
                || matches!(
                    state.as_str(),
                    "running" | "degraded" | "starting" | "initializing"
                );
            if usable {
                return None;
            }
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            match (state.is_empty(), err.is_empty()) {
                (false, true) => state,
                (true, false) => err,
                (true, true) => format!("exit {}", o.status),
                (false, false) => format!("{state}; {err}"),
            }
        }
        Err(e) => e.to_string(),
    };
    Some(format!(
        "the systemd user manager is not available (`{prog} --user is-system-running`: {state})"
    ))
}

/// What to do by hand for install (`install`) or uninstall, where the
/// systemd user manager is not available to agentmail.
pub fn manual(bin: &Path, env: &Env, install: bool) -> String {
    let path = unit_path(env);
    let steps = steps(
        bin,
        &unset_env(&location_overrides(env)),
        install,
        true,
        false,
    )
    .iter()
    .map(|s| format!("  {s}\n"))
    .collect::<String>();
    if install {
        let unit = unit(bin).unwrap_or_else(|e| format!("({e:#})\n"));
        format!(
            "To run the daemon under systemd by hand, write this unit to {}:\n\n{unit}\nthen run:\n{steps}",
            path.display()
        )
    } else {
        format!(
            "To remove the service by hand, run:\n{}",
            removal_steps(&path)
        )
    }
}

/// Whether systemd runs the service now.
pub fn is_active(env: &Env) -> bool {
    Command::new(systemctl(env))
        .args(["--user", "is-active", "--quiet", NAME])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Install, once the unit file is written: load, enable and start the
/// unit, restarting it with `restart` (see [`steps`]).
pub fn start(env: &Env, restart: bool) -> Result<()> {
    user_systemctl(env, &["daemon-reload"])?;
    user_systemctl(env, &["enable", "--now", NAME])?;
    if restart {
        user_systemctl(env, &["try-restart", NAME])?;
    }
    Ok(())
}

/// Install, once the service is started: stop the daemon running on
/// demand at the service's location (with `bin daemon stop`, told that
/// location by `stop`; see [`stop_env`]), so that the service's, waiting
/// for the lock, takes over (see [`steps`] and [`on_demand_daemon`]).
pub fn stop_on_demand(bin: &Path, stop: &StopEnv) -> Result<()> {
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "stop"]);
    for (k, v) in stop {
        match v {
            None => cmd.env_remove(k),
            Some(v) => cmd.env(k, v),
        };
    }
    run_cmd(cmd, &stop_command(bin, stop))
}

/// Uninstall, before the unit file is removed: disable and stop the
/// service.
pub fn stop(env: &Env) -> Result<()> {
    user_systemctl(env, &["disable", "--now", NAME])
}

/// Uninstall, once the unit file is removed: have systemd forget it.
pub fn reload(env: &Env) -> Result<()> {
    user_systemctl(env, &["daemon-reload"])
}

/// Whether `path`'s original (a unit there before agentmail's) is kept.
pub fn backup_kept(path: &Path) -> Option<PathBuf> {
    let bak = backup_path(path);
    std::fs::symlink_metadata(&bak).is_ok().then_some(bak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(cwd: &str, path: &str) -> Env {
        Env {
            home: PathBuf::from("/home/u"),
            cwd: PathBuf::from(cwd),
            vars: HashMap::from([("PATH".to_string(), path.to_string())]),
        }
    }

    #[test]
    fn invoked_resolves_argv0() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("agentmail");
        std::fs::write(&bin, "").unwrap();
        let d = dir.path().to_str().unwrap();
        let e = env(d, &format!("relative:/nowhere:{d}"));
        assert_eq!(invoked(&e, Path::new("agentmail")), Some(bin.clone()));
        assert_eq!(invoked(&e, Path::new("./agentmail")), Some(bin.clone()));
        assert_eq!(invoked(&e, Path::new(&bin)), Some(bin.clone()));
        assert_eq!(invoked(&e, Path::new("../x/agentmail")), None);
        assert_eq!(invoked(&e, Path::new("other")), None);
        assert_eq!(invoked(&e, Path::new("")), None);
    }

    #[test]
    fn is_generated_takes_any_exec_start() {
        assert!(is_generated(&unit(Path::new("/a b/agentmail")).unwrap()));
        assert!(is_generated(
            &UNIT.replace("{exec}", "%h/.cargo/bin/agentmail")
        ));
        let edited = unit(Path::new("/x"))
            .unwrap()
            .replace("[Service]\n", "[Service]\nEnvironment=A=1\n");
        assert!(!is_generated(&edited));
        assert!(!is_generated(&UNIT.replace("{exec}", "")));
    }

    #[test]
    fn environment_line_quotes_for_a_unit() {
        assert_eq!(environment_line("A", "/x/y"), "Environment=\"A=/x/y\"");
        assert_eq!(
            environment_line("A", "/a b/%h\\\"q"),
            r#"Environment="A=/a b/%%h\\\"q""#
        );
    }

    #[test]
    fn dropin_dir_is_next_to_the_unit() {
        assert_eq!(
            dropin_dir(Path::new("/c/systemd/user/agentmail.service")),
            Path::new("/c/systemd/user/agentmail.service.d")
        );
    }

    #[test]
    fn dropped_settings_lists_what_agentmail_unit_lacks() {
        let bin = Path::new("/x/agentmail");
        assert!(dropped_settings(&unit(bin).unwrap(), bin).is_empty());
        assert!(dropped_settings(&UNIT.replace("{exec}", "%h/a"), bin).is_empty());
        let mine = "[Service]\n# note\nExecStart=/old daemon run\nEnvironment=A=1\n\
                    Restart=always\nUser=x\n";
        assert_eq!(dropped_settings(mine, bin), ["Environment=A=1", "User=x"]);
    }

    #[test]
    fn written_exec_takes_only_what_agentmail_writes() {
        for bin in [
            "/x/agentmail",
            "/a b/agent\"mail",
            "/p%q/agentmail",
            "/a\\b",
        ] {
            let e = exec_arg(Path::new(bin)).unwrap();
            assert!(written_exec(&e), "{e}");
        }
        for e in [
            "%h/.cargo/bin/agentmail",
            "/x/%h/agentmail",
            "x/agentmail",
            "\"/x/agentmail\"",
            "\"/a b\\x\"",
            "/a b",
            "",
        ] {
            assert!(!written_exec(e), "{e}");
        }
    }

    #[test]
    fn show_words_unquotes_what_systemctl_prints() {
        assert_eq!(show_words(""), Some(vec![]));
        assert_eq!(
            show_words(r#""A=a b" "C=x\"y" D=plain "F=\$HOME""#),
            Some(vec![
                "A=a b".to_string(),
                "C=x\"y".to_string(),
                "D=plain".to_string(),
                "F=$HOME".to_string()
            ])
        );
        assert_eq!(show_words("$'A=\\n'"), None);
        assert_eq!(show_words(r#""A=b"c"#), None);
        assert_eq!(show_words(r#""A=b"#), None);
    }

    #[test]
    fn env_file_vars_reads_only_what_it_can() {
        let text = "# c\n; c\n\nOTHER=\"a\\b\"\nHOME=/h\n AGENTMAIL_DATA_DIR = \"/a b\"\n\
                    XDG_RUNTIME_DIR='/r$x'\n";
        assert_eq!(
            env_file_vars(text),
            Some(vec![
                ("HOME".to_string(), "/h".to_string()),
                ("AGENTMAIL_DATA_DIR".to_string(), "/a b".to_string()),
                ("XDG_RUNTIME_DIR".to_string(), "/r$x".to_string()),
            ])
        );
        assert_eq!(env_file_vars("HOME=/a\\ b\n"), None);
        assert_eq!(env_file_vars("HOME=\"/a\\\"b\"\n"), None);
        assert_eq!(env_file_vars("OTHER=\"a\nHOME=/x\"\n"), None);
        assert_eq!(env_file_vars("OTHER=a \\\nHOME=/x\n"), None);
    }

    /// A lock held with no record yet (a lock file just created), or with
    /// one caught mid-rewrite, is read again until the record is written,
    /// so a supervised daemon that has just taken the lock is not taken for
    /// one on demand.
    #[test]
    fn on_demand_daemon_waits_for_the_record() {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::in_dir(dir.path());
        let rec = |supervised| crate::paths::LockRecord {
            supervised,
            pid: std::process::id(),
            socket: paths.socket.clone(),
        };
        let mut torn = rec(false).encode();
        torn.extend_from_slice(b"tail\n");
        for before in [Vec::new(), torn] {
            std::fs::write(&paths.lock, &before).unwrap();
            let lock = std::fs::File::open(&paths.lock).unwrap();
            lock.try_lock().unwrap();
            let writer = std::thread::spawn({
                let path = paths.lock.clone();
                let bytes = rec(true).encode();
                move || {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                    file.write_all_at(&bytes, 0).unwrap();
                    file.set_len(bytes.len() as u64).unwrap();
                }
            });
            assert!(!on_demand_daemon(&paths), "{before:?}");
            writer.join().unwrap();
            drop(lock);
        }
        // A record that never comes (a daemon from before lock records) is
        // taken as on demand, as is a record saying so.
        std::fs::write(&paths.lock, b"").unwrap();
        let lock = std::fs::File::open(&paths.lock).unwrap();
        lock.try_lock().unwrap();
        assert!(on_demand_daemon(&paths));
        std::fs::write(&paths.lock, rec(false).encode()).unwrap();
        assert!(on_demand_daemon(&paths));
        drop(lock);
        // No lock held: none.
        assert!(!on_demand_daemon(&paths));
    }

    #[test]
    fn stop_command_shows_the_location_given() {
        let bin = Path::new("/b/agentmail");
        assert_eq!(stop_command(bin, &Vec::new()), "/b/agentmail daemon stop");
        assert_eq!(
            stop_command(bin, &unset_env(&LOCATION_VARS)),
            "env -u AGENTMAIL_DATA_DIR -u AGENTMAIL_SOCKET /b/agentmail daemon stop"
        );
        assert_eq!(
            stop_command(
                bin,
                &vec![
                    ("AGENTMAIL_DATA_DIR", Some(PathBuf::from("/a b"))),
                    ("AGENTMAIL_SOCKET", Some(PathBuf::from("/s"))),
                ]
            ),
            "env 'AGENTMAIL_DATA_DIR=/a b' AGENTMAIL_SOCKET=/s /b/agentmail daemon stop"
        );
    }
}
