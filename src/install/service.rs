//! `agentmail install --service` / `uninstall --service`: run the daemon
//! as a systemd user service.
//!
//! Install writes the unit from the README (its "Running the daemon under
//! systemd" section, verbatim but for `ExecStart`, which runs this binary),
//! stops any daemon started on demand, then has systemd load the unit,
//! enable it and start it. Uninstall disables and stops the service,
//! removes the unit and has systemd forget it. Neither discards a unit of
//! the user's (see [`plan`]). Both first check that the
//! user's systemd manager is reachable, and otherwise give the steps to do
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

/// Of [`LOCATION_VARS`], those set in `env`.
pub fn location_overrides(env: &Env) -> Vec<&'static str> {
    LOCATION_VARS
        .into_iter()
        .filter(|k| env.var(k).is_some())
        .collect()
}

/// The data directory and socket the service's daemon uses, from the
/// systemd user manager's environment (`systemctl --user show-environment`,
/// whose `HOME` defaults to `env`'s), when they differ from the defaults
/// here, which `daemon stop` (and every client here not told otherwise)
/// uses: then a note saying so. The defaults depend on `XDG_DATA_HOME` and
/// `XDG_RUNTIME_DIR`, which a shell (say, an ssh session without
/// pam_systemd) may set differently from the manager. Nothing when the
/// manager cannot be asked, or a value cannot be read.
pub fn location_mismatch(env: &Env) -> Option<String> {
    let out = Command::new(systemctl(env))
        .args(["--user", "show-environment"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut manager: std::collections::HashMap<&str, &str> =
        text.lines().filter_map(|l| l.split_once('=')).collect();
    // A value with special characters is shown quoted (`$'...'`).
    if manager.values().any(|v| v.starts_with("$'")) {
        return None;
    }
    let home = env.home.to_str()?;
    manager.entry("HOME").or_insert(home);
    let service =
        crate::paths::Paths::from_vars(|k| manager.get(k).map(std::ffi::OsString::from)).ok()?;
    let here = crate::paths::Paths::from_vars(|k| {
        if LOCATION_VARS.contains(&k) {
            None
        } else if k == "HOME" {
            Some(env.home.clone().into_os_string())
        } else {
            env.var(k).map(std::ffi::OsString::from)
        }
    })
    .ok()?;
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
    let (head, tail) = UNIT.split_once("{exec}").expect("the unit has {exec}");
    text.strip_prefix(head)
        .and_then(|t| t.strip_suffix(tail))
        .is_some_and(|exec| !exec.is_empty() && !exec.contains('\n'))
}

/// The unit that runs `bin` (an absolute path) as the daemon.
pub fn unit(bin: &Path) -> Result<String> {
    Ok(UNIT.replace("{exec}", &exec_arg(bin)?))
}

/// `bin` as the first word of a systemd command line: `%` (specifiers) and
/// `$` (variables) doubled, and double-quoted, with `\` and `"` escaped,
/// when it holds anything but plain path characters.
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
    let s = s.replace('%', "%%").replace('$', "$$");
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
    env.var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| env.home.join(".config"))
        .join("systemd/user")
        .join(format!("{NAME}.service"))
}

/// The change that installs the unit for `bin` (none when it is already
/// there), or, on uninstall, removes it (none when it is not there). A
/// unit that was there before agentmail's is kept as its backup, also once
/// the service is uninstalled.
///
/// Only agentmail's unit is replaced or removed as is: one that is not
/// (see [`is_generated`]) is the user's. Install saves it as the backup,
/// replacing any older backup of agentmail's, but refuses when a
/// different earlier unit is kept there. Uninstall saves an edited unit
/// agentmail created (its marker says so) as the backup, and refuses to
/// remove any other unit that is not agentmail's.
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
    let Some(text) = c.before.as_deref().filter(|t| !is_generated(t)) else {
        return Ok(vec![c]);
    };
    let bak = backup_path(&c.path);
    let backup = match std::fs::read(&bak) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", bak.display()))),
    };
    let saved = backup.as_deref() == Some(text.as_bytes());
    if install {
        if backup.is_some() && !saved {
            bail!(
                "{} is not the unit agentmail writes, and {} already keeps a different earlier \
                 unit; agentmail replaces neither. Move one of them away and run the command \
                 again. Nothing was changed.",
                c.path.display(),
                bak.display()
            );
        }
        // The user's unit is the original to keep, whatever an earlier
        // install recorded.
        c.record_anew = true;
    } else if backup.is_none() && std::fs::symlink_metadata(created_marker_path(&c.path)).is_ok() {
        // agentmail's unit with the user's edits.
        c.save = Some(bak);
    } else if !saved {
        bail!(
            "{} is not the unit agentmail installs (it was edited, or agentmail did not write \
             it), so agentmail leaves it; nothing was changed. To remove it anyway, run:\n  \
             systemctl --user disable --now {NAME}\n  rm {}\n  systemctl --user daemon-reload",
            c.path.display(),
            shell_quote(&c.path.to_string_lossy())
        );
    }
    Ok(vec![c])
}

/// The commands install (`install`) or uninstall runs after (install) or
/// around (uninstall) changing the unit, as the user would type them, for
/// `bin` and the daemon location overrides `unset` (see
/// [`location_overrides`]), which `daemon stop` leaves out so that it stops
/// the daemon at the default location, the one the service replaces. With
/// `stop_daemon`, install first stops any daemon run on demand. With
/// `restart` (install, replacing a unit systemd may be running), the
/// service is restarted so that it runs the new unit: enabling a running
/// service does not.
pub fn steps(
    bin: &Path,
    unset: &[&str],
    install: bool,
    stop_daemon: bool,
    restart: bool,
) -> Vec<String> {
    if install {
        let mut s = Vec::new();
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
        s.push("systemctl --user daemon-reload".to_string());
        s.push(format!("systemctl --user enable --now {NAME}"));
        if restart {
            s.push(format!("systemctl --user try-restart {NAME}"));
        }
        s
    } else {
        vec![
            format!("systemctl --user disable --now {NAME}"),
            "systemctl --user daemon-reload".to_string(),
        ]
    }
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
                return Ok(());
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
    bail!(
        "the systemd user manager is not available (`{prog} --user is-system-running`: {state}); \
         nothing was changed. {}",
        manual()
    )
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
            "To remove the service by hand, run:\n  systemctl --user disable --now {NAME}\n  rm {}\n  systemctl --user daemon-reload\n",
            super::shell_quote(&path.to_string_lossy())
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

/// Install: with `stop_daemon`, stop any daemon running on demand at the
/// default location (with `bin daemon stop`, [`LOCATION_VARS`] left out,
/// so the one systemd starts takes over), then load, enable and start the
/// unit, restarting it with `restart` (see [`steps`]). Run once its file
/// is written.
pub fn start(bin: &Path, env: &Env, stop_daemon: bool, restart: bool) -> Result<()> {
    if stop_daemon {
        let mut cmd = Command::new(bin);
        cmd.args(["daemon", "stop"]);
        for k in LOCATION_VARS {
            cmd.env_remove(k);
        }
        run_cmd(cmd, &format!("{} daemon stop", bin.display()))?;
    }
    user_systemctl(env, &["daemon-reload"])?;
    user_systemctl(env, &["enable", "--now", NAME])?;
    if restart {
        user_systemctl(env, &["try-restart", NAME])?;
    }
    Ok(())
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
}
