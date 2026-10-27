//! `agentmail install --service` / `uninstall --service`: run the daemon
//! as a systemd user service.
//!
//! Install writes the unit from the README (its "Running the daemon under
//! systemd" section, verbatim but for `ExecStart`, which runs this binary),
//! stops any daemon started on demand, then has systemd load the unit,
//! enable it and start it. Uninstall disables and stops the service,
//! removes the unit and has systemd forget it. Both first check that the
//! user's systemd manager is reachable, and otherwise give the steps to do
//! by hand. `AGENTMAIL_SYSTEMCTL` names the `systemctl` to run (tests).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::{Change, Env, backup_path, read_opt};

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
/// else under `~/.config`.
pub fn unit_path(env: &Env) -> PathBuf {
    env.dir_var("XDG_CONFIG_HOME")
        .unwrap_or_else(|| env.home.join(".config"))
        .join("systemd/user")
        .join(format!("{NAME}.service"))
}

/// The change that installs the unit for `bin` (none when it is already
/// there), or, on uninstall, removes it (none when it is not there). A
/// unit that was there before agentmail's is kept as its backup, also once
/// the service is uninstalled.
pub fn plan(bin: &Path, env: &Env, install: bool) -> Result<Vec<Change>> {
    let path = unit_path(env);
    let before = read_opt(&path)?;
    let after = if install { Some(unit(bin)?) } else { None };
    if before == after {
        return Ok(Vec::new());
    }
    Ok(vec![Change {
        path,
        before,
        after,
        record_anew: false,
        unlink: false,
        save: None,
        keep_backup: true,
        restore: false,
    }])
}

/// The commands install (`install`) or uninstall runs after (install) or
/// around (uninstall) changing the unit, as the user would type them. With
/// `restart` (install, replacing a unit systemd may be running), the
/// service is restarted so that it runs the new unit: enabling a running
/// service does not.
pub fn steps(install: bool, restart: bool) -> Vec<String> {
    if install {
        let mut s = vec![
            "agentmail daemon stop".to_string(),
            "systemctl --user daemon-reload".to_string(),
            format!("systemctl --user enable --now {NAME}"),
        ];
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
    let shown = format!("{prog} {}", args.join(" "));
    let out = Command::new(prog)
        .args(args)
        .output()
        .with_context(|| format!("running {shown}"))?;
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
    let steps = steps(install, false)
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

/// Install: stop any daemon running on demand (with `bin daemon stop`, so
/// the one systemd starts takes over), then load, enable and start the
/// unit, restarting it with `restart` (see [`steps`]). Run once its file
/// is written.
pub fn start(bin: &Path, env: &Env, restart: bool) -> Result<()> {
    let bin_s = bin.to_string_lossy();
    run(&bin_s, &["daemon", "stop"])?;
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
