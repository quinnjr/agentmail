//! `install.sh`, the curl-pipe installer.
//!
//! Each test serves a fake release from a temporary directory through
//! `AGENTMAIL_DOWNLOAD_URL=file://...`. The release's `agentmail` is a stub
//! shell script that records its arguments, so the tests check what the
//! installer downloads, verifies and runs without building a release.

use std::path::{Path, PathBuf};
use std::process::Output;

use tempfile::TempDir;

const STUB: &str = r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/calls"
case "$1" in
    --version) echo "agentmail 0.0.0-stub" ;;
    daemon) [ "$2" = status ] && exit 1 ;;
esac
exit 0
"#;

fn triple() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => panic!("no prebuilt target for {other}"),
    }
}

struct Release {
    dir: TempDir,
}

impl Release {
    /// A release served at `<dir>/rel`, as `latest` and as `v0.1.0`.
    fn new() -> Release {
        let dir = TempDir::new().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let stub = pkg.join("agentmail");
        std::fs::write(&stub, STUB).unwrap();
        run("chmod", &["755", stub.to_str().unwrap()], dir.path());
        let archive = format!("agentmail-{}.tar.gz", triple());
        run(
            "tar",
            &["czf", &archive, "-C", "pkg", "agentmail"],
            dir.path(),
        );
        let sum = run("sha256sum", &[&archive], dir.path());
        std::fs::write(dir.path().join(format!("{archive}.sha256")), sum.stdout).unwrap();
        for sub in ["rel/latest/download", "rel/download/v0.1.0"] {
            let to = dir.path().join(sub);
            std::fs::create_dir_all(&to).unwrap();
            for f in [archive.clone(), format!("{archive}.sha256")] {
                std::fs::copy(dir.path().join(&f), to.join(&f)).unwrap();
            }
        }
        Release { dir }
    }

    fn url(&self) -> String {
        format!("file://{}", self.dir.path().join("rel").display())
    }

    fn bin_dir(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.bin_dir().join("calls")).unwrap_or_default()
    }

    /// Runs install.sh with `args`, the release as its download source, a
    /// private HOME and a PATH without cargo.
    fn install(&self, args: &[&str]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
        std::process::Command::new("sh")
            .arg(script)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir.path().join("home"))
            .env("AGENTMAIL_DOWNLOAD_URL", self.url())
            .env("AGENTMAIL_INSTALL_DIR", self.bin_dir())
            .output()
            .unwrap()
    }
}

fn run(cmd: &str, args: &[&str], dir: &Path) -> Output {
    let out = std::process::Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{cmd} {args:?}: {out:?}");
    out
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn installs_the_latest_release_into_the_bin_dir() {
    let r = Release::new();
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("/latest/download/"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("installed agentmail 0.0.0-stub"));
    assert!(stderr(&out).contains("is not on your PATH"));
    let bin = r.bin_dir().join("agentmail");
    let mode = std::os::unix::fs::PermissionsExt::mode(&bin.metadata().unwrap().permissions());
    assert_eq!(mode & 0o777, 0o755);
    assert!(!r.bin_dir().join("agentmail.new").exists());
}

#[test]
fn a_version_without_v_downloads_that_tag() {
    let r = Release::new();
    let out = r.install(&["--version", "0.1.0"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("/download/v0.1.0/"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_checksum_mismatch_installs_nothing() {
    let r = Release::new();
    let sum = r
        .dir
        .path()
        .join("rel/latest/download")
        .join(format!("agentmail-{}.tar.gz.sha256", triple()));
    std::fs::write(&sum, "deadbeef  x\n").unwrap();
    let out = r.install(&[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("checksum mismatch"),
        "{}",
        stderr(&out)
    );
    assert!(!r.bin_dir().join("agentmail").exists());
}

#[test]
fn a_missing_checksum_installs_nothing() {
    let r = Release::new();
    let sum = r
        .dir
        .path()
        .join("rel/latest/download")
        .join(format!("agentmail-{}.tar.gz.sha256", triple()));
    std::fs::remove_file(sum).unwrap();
    let out = r.install(&[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no checksum"), "{}", stderr(&out));
    assert!(!r.bin_dir().join("agentmail").exists());
}

#[test]
fn a_missing_release_without_cargo_says_how_to_get_rust() {
    let r = Release::new();
    let out = r.install(&["--version", "v9.9.9"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("cargo is not installed"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("https://rustup.rs"));
}

#[test]
fn harness_and_service_run_agentmail_install() {
    let r = Release::new();
    let out = r.install(&["--harness", "opencode", "--harness=kiro", "--service"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let calls = r.calls();
    assert!(calls.contains("install --harness opencode\n"), "{calls}");
    assert!(calls.contains("install --harness kiro\n"), "{calls}");
    assert!(calls.contains("install --service\n"), "{calls}");
}

#[test]
fn harness_auto_with_no_harness_on_path_installs_none() {
    let r = Release::new();
    let out = r.install(&["--harness", "auto"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("no claude, opencode or kiro-cli on PATH"));
    assert!(!r.calls().contains("install --harness"), "{}", r.calls());
}

#[test]
fn unknown_options_and_harnesses_are_refused_before_downloading() {
    let r = Release::new();
    for args in [
        &["--harness", "vscode"][..],
        &["--frobnicate"][..],
        &["--version"][..],
    ] {
        let out = r.install(args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            !stderr(&out).contains("agentmail: downloading"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn help_prints_usage() {
    let r = Release::new();
    let out = r.install(&["--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--harness <name>"));
}
