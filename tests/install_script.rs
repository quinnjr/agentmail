//! `install.sh`, the curl-pipe installer.
//!
//! Each test serves a fake release from a temporary directory through
//! `AGENTMAIL_DOWNLOAD_URL=file://...` and runs the script with a private
//! PATH holding only the tools it needs (plus, for some tests, a fake
//! `cargo`). The release's `agentmail` is a stub shell script that records
//! its arguments, so the tests check what the installer downloads, verifies
//! and runs without building a release.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use tempfile::TempDir;

const STUB: &str = r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/calls"
case "$1" in
    --version) echo "agentmail 0.0.0-stub" ;;
esac
exit 0
"#;

/// A `cargo` that records its arguments and "installs" the stub into
/// `--root`.
const FAKE_CARGO: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_CARGO_LOG"
while [ $# -gt 0 ]; do
    [ "$1" = --root ] && root="$2"
    shift
done
mkdir -p "$root/bin"
cp "$FAKE_CARGO_STUB" "$root/bin/agentmail"
"#;

/// What install.sh runs, linked into the private PATH.
const TOOLS: &[&str] = &[
    "sh",
    "uname",
    "mktemp",
    "rm",
    "mkdir",
    "cp",
    "chmod",
    "mv",
    "cut",
    "tar",
    "gzip",
    "sha256sum",
    "curl",
    "cat",
    "dirname",
    "sed",
    "tail",
    "tr",
    "timeout",
    "setsid",
    "sleep",
    "env",
    "stat",
];

fn triple() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => panic!("no prebuilt target for {other}"),
    }
}

fn archive() -> String {
    format!("agentmail-{}.tar.gz", triple())
}

fn write_exec(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn which(tool: &str) -> PathBuf {
    for dir in ["/usr/bin", "/bin", "/usr/local/bin"] {
        let p = Path::new(dir).join(tool);
        if p.exists() {
            return p;
        }
    }
    panic!("{tool} not found")
}

struct Release {
    dir: TempDir,
}

impl Release {
    /// A release served at `<dir>/rel`, as `latest` and as `v0.1.0`, and a
    /// private PATH directory `<dir>/path` without cargo.
    fn new() -> Release {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("path");
        std::fs::create_dir_all(&path).unwrap();
        for tool in TOOLS {
            std::os::unix::fs::symlink(which(tool), path.join(tool)).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        let r = Release { dir };
        r.publish(STUB);
        r
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// (Re)publishes a release whose binary is `binary`.
    fn publish(&self, binary: &str) {
        let pkg = self.path("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        write_exec(&pkg.join("agentmail"), binary);
        run(
            "tar",
            &["czf", &archive(), "-C", "pkg", "agentmail"],
            self.dir.path(),
        );
        self.write_checksum();
    }

    /// Writes `<archive>.sha256` for the archive as it is now, and copies
    /// both into the release.
    fn write_checksum(&self) {
        let sum = run("sha256sum", &[&archive()], self.dir.path());
        std::fs::write(self.path(&format!("{}.sha256", archive())), sum.stdout).unwrap();
        for sub in ["rel/latest/download", "rel/download/v0.1.0"] {
            let to = self.path(sub);
            std::fs::create_dir_all(&to).unwrap();
            for f in [archive(), format!("{}.sha256", archive())] {
                std::fs::copy(self.path(&f), to.join(&f)).unwrap();
            }
        }
    }

    fn latest(&self, file: &str) -> PathBuf {
        self.path("rel/latest/download").join(file)
    }

    fn bin_dir(&self) -> PathBuf {
        self.path("bin")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.bin_dir().join("calls")).unwrap_or_default()
    }

    /// Puts a fake `cargo` on the private PATH; returns its log file.
    fn add_cargo(&self) -> PathBuf {
        write_exec(&self.path("path/cargo"), FAKE_CARGO);
        write_exec(&self.path("cargo-stub"), STUB);
        self.path("cargo.log")
    }

    /// Makes `curl` answer the releases/latest lookup with a redirect to
    /// `url`, as GitHub does (a file:// release cannot redirect).
    fn latest_redirects_to(&self, url: &str) {
        let curl = self.path("path/curl");
        std::fs::remove_file(&curl).unwrap();
        let real = which("curl");
        write_exec(
            &curl,
            &format!(
                "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = '%{{url_effective}}' ] && {{ echo '{url}'; exit 0; }}; done\nexec {} \"$@\"\n",
                real.display()
            ),
        );
    }

    /// Replaces `curl` with a fake GNU `wget` that serves file:// URLs and
    /// answers the releases/latest lookup the way wget prints a redirect.
    fn use_wget(&self, latest: &str) {
        std::fs::remove_file(self.path("path/curl")).unwrap();
        write_exec(
            &self.path("path/wget"),
            &format!(
                r#"#!/bin/sh
out=""; url=""; probe=""
while [ $# -gt 0 ]; do
    case "$1" in
        -O) out="$2"; shift 2 ;;
        --max-redirect=0) probe=1; shift ;;
        -*) shift ;;
        *) url="$1"; shift ;;
    esac
done
if [ -n "$probe" ]; then
    echo "  HTTP/1.1 302 Found" >&2
    echo "  Location: {latest} [following]" >&2
    exit 8
fi
path="${{url#file://}}"
[ -f "$path" ] || exit 8
cp "$path" "$out"
"#
            ),
        );
    }

    /// Runs install.sh with `args`, the release as its download source and
    /// a private HOME and PATH.
    fn install(&self, args: &[&str]) -> Output {
        self.install_env(args, &[])
    }

    /// `install` with extra (or overriding) environment variables.
    fn install_env(&self, args: &[&str], env: &[(&str, String)]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
        std::process::Command::new(self.path("path/sh"))
            .arg(script)
            .args(args)
            .current_dir(self.dir.path())
            .env_clear()
            .env("PATH", self.path("path"))
            .env("SHELL", self.path("path/sh"))
            .env("HOME", self.path("home"))
            .env(
                "AGENTMAIL_DOWNLOAD_URL",
                format!("file://{}", self.path("rel").display()),
            )
            .env("AGENTMAIL_INSTALL_DIR", self.bin_dir())
            .env("FAKE_CARGO_LOG", self.path("cargo.log"))
            .env("FAKE_CARGO_STUB", self.path("cargo-stub"))
            .envs(env.iter().map(|(k, v)| (k, v)))
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

/// Asserts the run failed with `msg` and left no half-installed binary.
fn assert_fails_with(r: &Release, out: &Output, msg: &str) {
    assert!(!out.status.success(), "{}", stderr(out));
    assert!(
        stderr(out).contains(msg),
        "want {msg:?} in:\n{}",
        stderr(out)
    );
    assert!(!r.bin_dir().join("agentmail.new").exists());
}

#[test]
fn installs_the_latest_release_into_the_bin_dir() {
    let r = Release::new();
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("/latest/download/"), "{err}");
    assert!(err.contains("installed agentmail 0.0.0-stub"), "{err}");
    assert!(err.contains("is not on your PATH"), "{err}");
    let bin = r.bin_dir().join("agentmail");
    assert_eq!(bin.metadata().unwrap().permissions().mode() & 0o777, 0o755);
    assert!(!r.bin_dir().join("agentmail.new").exists());
}

#[test]
fn stops_the_running_daemon_after_installing() {
    let r = Release::new();
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(r.calls().contains("daemon stop\n"), "{}", r.calls());
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
    let sum = format!("{}  {}\n", "0".repeat(64), archive());
    std::fs::write(r.latest(&format!("{}.sha256", archive())), sum).unwrap();
    let out = r.install(&[]);
    assert_fails_with(&r, &out, "checksum mismatch");
    assert!(!r.bin_dir().join("agentmail").exists());
}

#[test]
fn a_missing_checksum_installs_nothing() {
    let r = Release::new();
    std::fs::remove_file(r.latest(&format!("{}.sha256", archive()))).unwrap();
    let out = r.install(&[]);
    assert_fails_with(&r, &out, "no checksum");
    assert!(!r.bin_dir().join("agentmail").exists());
}

#[test]
fn an_empty_checksum_installs_nothing() {
    let r = Release::new();
    std::fs::write(r.latest(&format!("{}.sha256", archive())), "").unwrap();
    let out = r.install(&[]);
    assert_fails_with(&r, &out, "not a SHA-256 hash");
    assert!(!r.bin_dir().join("agentmail").exists());
}

#[test]
fn a_truncated_archive_keeps_the_old_binary() {
    let r = Release::new();
    // A binary big enough that a cut-off archive still unpacks part of it.
    let big = format!("{STUB}# {}\n", "x".repeat(200_000));
    r.publish(&big);
    let full = std::fs::read(r.path(&archive())).unwrap();
    std::fs::write(r.path(&archive()), &full[..full.len() / 2]).unwrap();
    r.write_checksum();
    std::fs::create_dir_all(r.bin_dir()).unwrap();
    std::fs::write(r.bin_dir().join("agentmail"), "old").unwrap();
    let out = r.install(&[]);
    assert_fails_with(&r, &out, "could not unpack");
    assert_eq!(
        std::fs::read_to_string(r.bin_dir().join("agentmail")).unwrap(),
        "old"
    );
}

#[test]
fn a_binary_that_does_not_run_keeps_the_old_one() {
    let r = Release::new();
    r.publish("#!/bin/sh\nexit 1\n");
    std::fs::create_dir_all(r.bin_dir()).unwrap();
    std::fs::write(r.bin_dir().join("agentmail"), "old").unwrap();
    let out = r.install(&[]);
    assert_fails_with(&r, &out, "does not run on this machine");
    assert_eq!(
        std::fs::read_to_string(r.bin_dir().join("agentmail")).unwrap(),
        "old"
    );
}

#[test]
fn no_release_yet_says_so_and_does_not_build() {
    let r = Release::new();
    r.latest_redirects_to(NO_RELEASE);
    let cargo_log = r.add_cargo();
    let out = r.install(&[]);
    assert_fails_with(&r, &out, "no agentmail release has been published yet");
    assert!(!cargo_log.exists(), "cargo ran without --from-source");
}

/// Where GitHub's releases/latest redirects with no release published.
const NO_RELEASE: &str = "https://github.com/quinnjr/agentmail/releases";
/// Where it redirects when v0.1.0 is the latest release.
const LATEST_V010: &str = "https://github.com/quinnjr/agentmail/releases/tag/v0.1.0";

#[test]
fn the_latest_tag_is_resolved_once_and_downloaded() {
    let r = Release::new();
    r.latest_redirects_to(LATEST_V010);
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("/download/v0.1.0/"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn from_source_builds_the_latest_tag() {
    let r = Release::new();
    r.latest_redirects_to(LATEST_V010);
    let cargo_log = r.add_cargo();
    let out = r.install(&["--from-source"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let log = std::fs::read_to_string(cargo_log).unwrap();
    assert!(log.contains(" --tag v0.1.0 "), "{log}");
}

#[test]
fn from_source_refuses_when_the_latest_release_is_unknown() {
    // The file:// release has no redirect, so the lookup cannot tell.
    let r = Release::new();
    let cargo_log = r.add_cargo();
    let out = r.install(&["--from-source"]);
    assert_fails_with(&r, &out, "could not find the latest release");
    assert!(!cargo_log.exists(), "cargo ran without a tag");
}

#[test]
fn an_unknown_latest_release_missing_from_a_mirror_is_an_error() {
    let r = Release::new();
    std::fs::remove_dir_all(r.path("rel/latest")).unwrap();
    let out = r.install(&[]);
    assert_fails_with(&r, &out, "found no agentmail release at");
}

#[test]
fn wget_resolves_the_latest_tag() {
    let r = Release::new();
    r.use_wget(LATEST_V010);
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("/download/v0.1.0/agentmail-"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn wget_sees_that_no_release_exists() {
    let r = Release::new();
    r.use_wget(NO_RELEASE);
    let out = r.install(&[]);
    assert_fails_with(&r, &out, "no agentmail release has been published yet");
}

#[test]
fn a_missing_tag_is_an_error() {
    let r = Release::new();
    let out = r.install(&["--version", "v9.9.9"]);
    assert_fails_with(&r, &out, "release v9.9.9 has no");
}

#[test]
fn from_source_builds_the_requested_tag() {
    let r = Release::new();
    let cargo_log = r.add_cargo();
    let out = r.install(&["--from-source", "--version", "v0.1.0"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let log = std::fs::read_to_string(cargo_log).unwrap();
    assert!(
        log.starts_with(
            "install --locked --git https://github.com/quinnjr/agentmail --tag v0.1.0 --root "
        ),
        "{log}"
    );
    assert!(stderr(&out).contains("installed agentmail 0.0.0-stub"));
    assert!(!stderr(&out).contains("downloading"), "{}", stderr(&out));
}

#[test]
fn from_source_with_no_release_builds_the_development_branch() {
    let r = Release::new();
    r.latest_redirects_to(NO_RELEASE);
    let cargo_log = r.add_cargo();
    let out = r.install(&["--from-source"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let log = std::fs::read_to_string(cargo_log).unwrap();
    assert!(!log.contains("--tag"), "{log}");
    assert!(
        stderr(&out).contains("building the development branch"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn from_source_without_cargo_says_how_to_get_rust() {
    let r = Release::new();
    r.latest_redirects_to(LATEST_V010);
    let out = r.install(&["--from-source"]);
    assert_fails_with(&r, &out, "cargo is not installed");
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
fn harness_auto_finds_harnesses_in_the_usual_places() {
    let r = Release::new();
    // ~/.local/bin is not on the private PATH, as under `wsl.exe -e sh`.
    let local = r.path("home/.local/bin");
    std::fs::create_dir_all(&local).unwrap();
    write_exec(&local.join("opencode"), "#!/bin/sh\n");
    let out = r.install(&["--harness", "auto"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let calls = r.calls();
    assert!(calls.contains("install --harness opencode\n"), "{calls}");
    assert!(!calls.contains("claude-code"), "{calls}");
}

#[test]
fn harness_auto_with_no_harness_installs_none() {
    let r = Release::new();
    let out = r.install(&["--harness", "auto"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("no claude, opencode or kiro-cli on PATH"));
    assert!(!r.calls().contains("install --harness"), "{}", r.calls());
}

#[test]
fn warns_about_another_agentmail_earlier_on_path() {
    let r = Release::new();
    write_exec(&r.path("path/agentmail"), STUB);
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("finds another agentmail first"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_relative_bin_dir_is_made_absolute() {
    let r = Release::new();
    let out = r.install(&["--bin-dir", "rel-bin"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(r.path("rel-bin/agentmail").exists());
    let want = format!(
        "installed agentmail 0.0.0-stub to {}/rel-bin/agentmail",
        r.dir.path().display()
    );
    assert!(stderr(&out).contains(&want), "{}", stderr(&out));
}

#[test]
fn a_trailing_slash_bin_dir_on_path_is_recognised() {
    let r = Release::new();
    let bin = format!("{}/", r.bin_dir().display());
    let path = format!("{}:{}", r.bin_dir().display(), r.path("path").display());
    let out = r.install_env(&["--bin-dir", &bin], &[("PATH", path)]);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(!err.contains("another agentmail"), "{err}");
    assert!(!err.contains("not on your PATH"), "{err}");
    assert!(!err.contains("//agentmail"), "{err}");
}

#[test]
fn under_wsl_the_login_shell_path_counts_and_cannot_hang_the_install() {
    let r = Release::new();
    // A login shell whose profile adds the bin dir to PATH and leaves a
    // background job holding its output open.
    let shell = r.path("login-shell");
    write_exec(
        &shell,
        &format!(
            "#!/bin/sh\nsleep 30 &\nPATH=\"$PATH:{}\"; export PATH\nexec {} \"$@\"\n",
            r.bin_dir().display(),
            which("sh").display()
        ),
    );
    let began = std::time::Instant::now();
    let out = r.install_env(
        &[],
        &[
            ("WSL_DISTRO_NAME", "Test".to_string()),
            ("SHELL", shell.display().to_string()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(began.elapsed() < std::time::Duration::from_secs(20));
    assert!(
        !stderr(&out).contains("not on your PATH"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_tilde_bin_dir_is_under_home() {
    let r = Release::new();
    let out = r.install(&["--bin-dir=~/tools"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(r.path("home/tools/agentmail").exists());
}

#[test]
fn bad_options_are_refused_before_downloading() {
    let r = Release::new();
    for (args, msg) in [
        (&["--harness", "vscode"][..], "unknown harness 'vscode'"),
        (&["--harness", "*"][..], "unknown harness '*'"),
        (&["--frobnicate"][..], "unknown option"),
        (&["--version"][..], "--version needs a value"),
        (&["--version="][..], "--version needs a value"),
        (&["--bin-dir="][..], "--bin-dir needs a value"),
    ] {
        let out = r.install(args);
        assert_fails_with(&r, &out, msg);
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
