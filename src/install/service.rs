//! `agentmail install --service` / `uninstall --service`: run the daemon
//! as a systemd user service.
//!
//! Install writes the unit from the README (its "Running the daemon under
//! systemd" section, verbatim but for `ExecStart`, which runs this binary),
//! has systemd load the unit, enable it and start it, then stops the daemon
//! started on demand, if one runs, so that the service's takes over. Uninstall disables
//! and stops the service, removes the unit and has systemd forget it; with
//! the unit already removed by hand, it cleans up what systemd still has
//! of the service (see [`orphan`]). Neither discards a unit of the user's
//! (see [`plan`]). Both first check that the user's systemd manager is
//! reachable, and otherwise give the steps to do
//! by hand. `AGENTMAIL_SYSTEMCTL` names the `systemctl` to run (tests).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::{Change, Env, backup_path, created_marker_path, read_opt, shell_quote};

/// The unit's name, as `systemctl` takes it.
pub const NAME: &str = "agentmail";

/// The README's unit, `ExecStart` aside (`{exec}`).
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
/// from their defaults, which the service does not see.
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

/// Of [`LOCATION_VARS`], those set in `env`.
pub fn location_overrides(env: &Env) -> Vec<&'static str> {
    LOCATION_VARS
        .into_iter()
        .filter(|k| env.var(k).is_some())
        .collect()
}

/// The environment the systemd user manager passes to the units it starts,
/// the service's daemon among them (`systemctl --user show-environment`),
/// with `HOME` defaulting to `env`'s. Nothing when the
/// manager cannot be asked, or the value of a variable the daemon's
/// location depends on ([`PATH_VARS`]) cannot be read (one with special
/// characters is shown quoted, `$'...'`). Other variables' quoted values
/// (say, a multi-line one a desktop session imported) are left out: they
/// matter to nothing here, and a line of one may not even be a variable.
fn manager_env(env: &Env) -> Option<std::collections::HashMap<String, String>> {
    let out = Command::new(systemctl(env))
        .args(["--user", "show-environment"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut manager = std::collections::HashMap::new();
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

/// The default daemon location here: `env`'s, but for [`LOCATION_VARS`],
/// which the service does not see. `daemon stop` run by install uses it.
pub fn default_paths(env: &Env) -> Option<crate::paths::Paths> {
    crate::paths::Paths::from_vars(|k| {
        if LOCATION_VARS.contains(&k) {
            None
        } else if k == "HOME" {
            Some(env.home.clone().into_os_string())
        } else {
            env.var(k).map(std::ffi::OsString::from)
        }
    })
    .ok()
}

/// Whether a daemon started on demand runs at the default location (see
/// [`default_paths`]): one holds the lock there, and its lock record does
/// not say it is supervised (a daemon from before lock records, which
/// writes none, is taken as on demand). Install stops only such a daemon:
/// a supervised one is the service's own (or another supervisor's), and
/// stopping it would only drop its clients while it is restarted.
pub fn on_demand_daemon(env: &Env) -> bool {
    default_paths(env)
        .is_some_and(|p| p.lock_held() && !p.lock_record().is_some_and(|r| r.supervised))
}

/// The data directory and socket the service's daemon uses, from the
/// systemd user manager's environment (see [`manager_env`]), when they
/// differ from the defaults here, which `daemon stop` (and every client
/// here not told otherwise) uses: then a note saying so. The defaults
/// depend on `XDG_DATA_HOME` and `XDG_RUNTIME_DIR`, which a shell (say, an
/// ssh session without pam_systemd) may set differently from the manager.
/// Nothing when the manager cannot be asked, or a value cannot be read.
pub fn location_mismatch(env: &Env) -> Option<String> {
    let manager = manager_env(env)?;
    let service =
        crate::paths::Paths::from_vars(|k| manager.get(k).map(std::ffi::OsString::from)).ok()?;
    let here = default_paths(env)?;
    let mut diff = Vec::new();
    if service.socket != here.socket {
        diff.push(format!(
            "socket {} (default here: {})",
            service.socket.display(),
            here.socket.display()
        ));
    }
    if service.data_dir != here.data_dir {
        diff.push(format!(
            "data directory {} (default here: {})",
            service.data_dir.display(),
            here.data_dir.display()
        ));
    }
    if diff.is_empty() {
        return None;
    }
    Some(format!(
        "the service gets the {} from the systemd user manager's environment, which sets \
         XDG_RUNTIME_DIR, XDG_DATA_HOME or HOME differently: `daemon stop` and clients run \
         here use a daemon of their own, not the service's. Set the same values here, or in \
         the manager with `systemctl --user set-environment`.",
        diff.join(" and the ")
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
/// [`manager_unit_path`]): why agentmail refuses, since the manager would
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
/// when it must be. Not, say, the README's `%h/.cargo/bin/agentmail`.
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

/// The change that installs the unit for `bin` (none when it is already
/// there), or, on uninstall, removes it (none when it is not there). A
/// unit that was there before agentmail's is kept as its backup, also once
/// the service is uninstalled.
///
/// Only agentmail's own unit is replaced or removed as is: the unit it
/// created (its marker says so; whatever its `ExecStart`), or the one it
/// wrote over a unit of the user's, which is kept as the backup (its
/// `ExecStart` one agentmail writes, see [`written_exec`]). Any other
/// unit is the user's, also one written by hand from the README: install
/// saves it as the backup, replacing any older backup of agentmail's, and
/// uninstall saves it as the backup before removing it when it matches
/// the README's unit (see [`is_generated`]) or agentmail created it (it
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
        // The README's unit, written by hand, or agentmail's with the
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
/// `bin` and the daemon location overrides `unset` (see
/// [`location_overrides`]), which `daemon stop` leaves out so that it stops
/// the daemon at the default location, the one the service replaces. With
/// `restart` (install, changing the unit while the service runs, on the
/// unit replaced or on one removed by hand), the service is restarted so
/// that it runs the new unit: enabling a running service does not (a
/// stopped one `enable --now` starts on the new unit). With `stop_daemon`, install then stops any daemon run
/// on demand: last, so that the service's daemon, already waiting for the
/// lock, takes over as it exits, leaving no time for a client to start
/// another on demand. (Install passes `stop_daemon` only when such a daemon
/// runs; see [`on_demand_daemon`].)
pub fn steps(
    bin: &Path,
    unset: &[&str],
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
            let env_u: String = unset.iter().map(|k| format!("-u {k} ")).collect();
            let env_u = if env_u.is_empty() {
                env_u
            } else {
                format!("env {env_u}")
            };
            s.push(format!(
                "{env_u}{} daemon stop",
                shell_quote(&bin.to_string_lossy())
            ));
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

/// Where systemd loads the service from: `None` when it has no unit file
/// for it (`LoadState=not-found`), else its `FragmentPath` (empty when it
/// has none). An error when systemd cannot be asked.
fn fragment(env: &Env) -> Result<Option<PathBuf>> {
    let out = Command::new(systemctl(env))
        .args(["--user", "show", "-p", "LoadState,FragmentPath", NAME])
        .output()
        .context("asking systemd about the service")?;
    if !out.status.success() {
        bail!("systemctl --user show {NAME} failed ({})", out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let prop = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
            .unwrap_or("")
            .to_string()
    };
    Ok((prop("LoadState") != "not-found").then(|| PathBuf::from(prop("FragmentPath"))))
}

/// Uninstall, with no unit file: what systemd still has of agentmail's
/// service, if anything (see [`Orphan`]), and a note on a service named
/// `agentmail` it leaves alone as not agentmail's. The running service is
/// agentmail's only when systemd has no unit file for it any more, or
/// loads it from [`unit_path`] (not yet reloaded); one loaded from
/// elsewhere (say, `~/.local/share/systemd/user` or a package's) is the
/// user's. Likewise the `default.target.wants` link is agentmail's only
/// when it points to [`unit_path`] (it may dangle there).
pub fn orphan(env: &Env) -> (Option<Orphan>, Option<String>) {
    let unit = unit_path(env);
    let mut left = Vec::new();
    let wants = wants_link(env);
    let link = match std::fs::read_link(&wants) {
        Ok(target) => {
            let target = wants.parent().map_or(target.clone(), |d| d.join(&target));
            if target == unit {
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
            Ok(Some(f)) if f == unit => true,
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
    let steps = steps(bin, &location_overrides(env), install, true, false)
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
/// demand at the default location (with `bin daemon stop`,
/// [`LOCATION_VARS`] left out), so that the service's, waiting for the
/// lock, takes over (see [`steps`] and [`on_demand_daemon`]).
pub fn stop_on_demand(bin: &Path) -> Result<()> {
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "stop"]);
    for k in LOCATION_VARS {
        cmd.env_remove(k);
    }
    run_cmd(cmd, &format!("{} daemon stop", bin.display()))
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
}
