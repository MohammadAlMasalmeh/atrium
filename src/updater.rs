//! Signed self-update.
//!
//! The feed is a GitHub "latest release" JSON document (`tag_name`, `body`, `assets`). The
//! release carries `<name>-macos-<arch>.zip` (a `ditto` archive of the app) and `<zip>.sig`, an
//! Ed25519 signature over the version, SHA-256, and size (`update_sig`). A verified update is
//! extracted into `<data dir>/updates/staged` and swapped in on the next launch: the old
//! binary moves the current bundle to `updates/previous`, renames the staged bundle into place,
//! and supervises the new version until it reports that its shell loaded. If it exits or times
//! out first, the previous bundle is restored and the old version keeps running.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use axum::{extract::State, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::App;

const EMBEDDED_KEY: &str = match option_env!("ASIDE_UPDATE_PUBLIC_KEY") {
    Some(key) if !key.is_empty() => key,
    _ => include_str!("../assets/update-public-key.txt"),
};
const DEFAULT_REPO: Option<&str> = match option_env!("ASIDE_UPDATE_REPO") {
    Some(repo) if !repo.is_empty() => Some(repo),
    _ => None,
};
const SUPERVISED_ENV: &str = "ASIDE_UPDATE_SUPERVISED";
const WAIT_ENV: &str = "ASIDE_UPDATE_WAIT_PID";
const MAX_TRIAL_LAUNCHES: u32 = 3;
const MAX_ARCHIVE_BYTES: u64 = 2 << 30;
const AUTO_INTERVAL: Duration = Duration::from_secs(6 * 3600);

static DIR: OnceLock<PathBuf> = OnceLock::new();
static RUNTIME: Mutex<Runtime> = Mutex::new(Runtime { phase: "idle", available: None, error: None, last_check: None });
static BUSY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static RELAUNCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Serialize)]
struct Release {
    version: String,
    notes: String,
    asset: String,
    url: String,
    signature_url: String,
}

struct Runtime {
    phase: &'static str,
    available: Option<Release>,
    error: Option<String>,
    last_check: Option<u64>,
}

#[derive(Default, Serialize, Deserialize, Clone)]
struct Persisted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    staged: Option<Staged>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trial: Option<Trial>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<Outcome>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Staged {
    version: String,
    app: PathBuf,
    target: PathBuf,
}

#[derive(Serialize, Deserialize, Clone)]
struct Trial {
    version: String,
    from: String,
    target: PathBuf,
    previous: PathBuf,
    launches: u32,
}

#[derive(Serialize, Deserialize, Clone)]
struct Outcome {
    result: String,
    version: String,
    from: String,
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Config {
    feed: Option<String>,
    repo: Option<String>,
    auto: Option<bool>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

fn updates(dir: &Path) -> PathBuf {
    dir.join("updates")
}

fn load(dir: &Path) -> Persisted {
    std::fs::read(updates(dir).join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(dir: &Path, state: &Persisted) -> Result<()> {
    let root = updates(dir);
    std::fs::create_dir_all(&root)?;
    let temp = root.join("state.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(temp, root.join("state.json"))?;
    Ok(())
}

fn config(dir: &Path) -> Config {
    std::fs::read(dir.join("update.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn feed_url(config: &Config) -> Option<String> {
    std::env::var("ASIDE_UPDATE_FEED").ok().filter(|f| !f.is_empty())
        .or_else(|| config.feed.clone())
        .or_else(|| config.repo.as_deref().or(DEFAULT_REPO).map(|repo| format!("https://api.github.com/repos/{repo}/releases/latest")))
}

/// Feeds and downloads must use HTTPS, except on loopback for local testing.
fn allowed_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).with_context(|| format!("invalid update URL {raw}"))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if url.scheme() == "https" || (url.scheme() == "http" && loopback) {
        Ok(url)
    } else {
        bail!("update URLs must use HTTPS: {raw}")
    }
}

fn public_key() -> Option<String> {
    crate::update_sig::parse_public_key(EMBEDDED_KEY)
}

/// The running `.app` bundle, if the executable is inside one.
fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app" && exe.parent()?.ends_with("Contents/MacOS")).then(|| app.to_path_buf())
}

fn executable(app: &Path) -> PathBuf {
    let name = std::env::current_exe().ok().and_then(|e| e.file_name().map(std::ffi::OsStr::to_os_string));
    app.join("Contents/MacOS").join(name.unwrap_or_else(|| "aside".into()))
}

fn info(app: &Path, key: &str) -> Option<String> {
    let plist = plist::Value::from_file(app.join("Contents/Info.plist")).ok()?;
    plist.as_dictionary()?.get(key)?.as_string().map(str::to_string)
}

pub fn current_version() -> String {
    bundle_path().and_then(|app| info(&app, "CFBundleVersion")).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

fn newer(candidate: &str, current: &str) -> bool {
    match (semver::Version::parse(candidate), semver::Version::parse(current)) {
        (Ok(candidate), Ok(current)) => candidate > current,
        _ => false,
    }
}

fn arch() -> &'static str {
    if cfg!(target_arch = "aarch64") { "arm64" } else { "x86_64" }
}

fn disabled_reason(dir: &Path) -> Option<String> {
    if public_key().is_none() {
        Some("This build has no update signing key".into())
    } else if bundle_path().is_none() {
        Some("Not running from an app bundle".into())
    } else if feed_url(&config(dir)).is_none() {
        Some("No release feed is configured".into())
    } else {
        None
    }
}

/// Moves a directory, copying with `ditto` when the destination is on another volume.
fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(18) => {
            let partial = to.with_extension("partial");
            let _ = std::fs::remove_dir_all(&partial);
            run("/usr/bin/ditto", &[from.as_os_str(), partial.as_os_str()])?;
            std::fs::rename(&partial, to)?;
            std::fs::remove_dir_all(from)?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("cannot move {} to {}", from.display(), to.display())),
    }
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<String> {
    let output = std::process::Command::new(program).args(args).output()?;
    if !output.status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
}

/// Files this app writes are quarantined (LSFileQuarantineEnabled). Starting quarantined code
/// makes Gatekeeper show a dialog, so a verified update is cleared before it can run.
fn unquarantine(app: &Path) -> Result<()> {
    run("/usr/bin/xattr", &["-d".as_ref(), "-r".as_ref(), "com.apple.quarantine".as_ref(), app.as_os_str()]).map(drop).or_else(|error| {
        let still = run("/usr/bin/xattr", &["-r".as_ref(), "-l".as_ref(), app.as_os_str()]).unwrap_or_default();
        if still.contains("com.apple.quarantine") { Err(error) } else { Ok(()) }
    })
}

fn team_id(app: &Path) -> Option<String> {
    let output = run("/usr/bin/codesign", &["-dv".as_ref(), "--verbose=2".as_ref(), app.as_os_str()]).ok()?;
    output.lines().find_map(|l| l.strip_prefix("TeamIdentifier=")).filter(|t| *t != "not set").map(str::to_string)
}

// ── Launch-time swap and rollback ────────────────────────────────────────────

/// Runs before the browser starts. Applies a staged update or rolls back a failing one.
/// Returns only when this process should continue as the browser.
pub fn on_launch(dir: &Path) {
    let _ = DIR.set(dir.to_path_buf());
    if let Some(pid) = std::env::var(WAIT_ENV).ok().and_then(|p| p.parse::<i32>().ok()) {
        std::env::remove_var(WAIT_ENV);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while unsafe { libc::kill(pid, 0) } == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let Some(app) = bundle_path() else { return };
    let mut state = load(dir);
    let current = current_version();
    if let Some(trial) = state.trial.as_mut().filter(|t| t.target == app && t.version == current) {
        if std::env::var_os(SUPERVISED_ENV).is_some() {
            return;
        }
        trial.launches += 1;
        if trial.launches < MAX_TRIAL_LAUNCHES {
            let _ = save(dir, &state);
            return;
        }
        let reason = format!("{current} did not finish starting after {MAX_TRIAL_LAUNCHES} launches");
        if rollback(dir, &mut state, &reason).is_ok() {
            relaunch_and_exit(&app, false);
        }
        return;
    }
    let Some(staged) = state.staged.take().filter(|s| s.target == app) else { return };
    if !newer(&staged.version, &current) || !staged.app.join("Contents/Info.plist").exists() {
        let _ = std::fs::remove_dir_all(&staged.app);
        let _ = save(dir, &state);
        return;
    }
    let previous = updates(dir).join("previous").join(app.file_name().unwrap_or_default());
    let _ = std::fs::remove_dir_all(&previous);
    if let Err(error) = move_dir(&app, &previous) {
        eprintln!("update: could not move the current app aside: {error:#}");
        return;
    }
    if let Err(error) = move_dir(&staged.app, &app) {
        eprintln!("update: could not install {}: {error:#}", staged.version);
        let _ = move_dir(&previous, &app);
        state.last = Some(Outcome { result: "failed".into(), version: staged.version, from: current, at: now(), reason: Some(format!("{error:#}")) });
        let _ = save(dir, &state);
        return;
    }
    state.trial = Some(Trial { version: staged.version.clone(), from: current.clone(), target: app.clone(), previous, launches: 0 });
    if save(dir, &state).is_err() {
        return;
    }
    eprintln!("update: installed {}; starting it", staged.version);
    supervise(dir, &app, &staged.version);
}

/// Starts the newly installed version and waits until it confirms startup or fails.
fn supervise(dir: &Path, app: &Path, version: &str) {
    let timeout = std::env::var("ASIDE_UPDATE_CONFIRM_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(120u64);
    let exe = executable(app);
    let started = unquarantine(app).and_then(|()| {
        Ok(std::process::Command::new(&exe).args(std::env::args_os().skip(1)).env(SUPERVISED_ENV, "1").spawn()?)
    });
    let reason = match started {
        Err(error) => format!("could not start {version}: {error:#}"),
        Ok(mut child) => {
            let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
            loop {
                let state = load(dir);
                if state.trial.is_none() && state.last.as_ref().is_some_and(|l| l.result == "updated" && l.version == version) {
                    std::process::exit(0);
                }
                match child.try_wait() {
                    Ok(Some(status)) => break format!("{version} exited during startup ({status})"),
                    Err(error) => break format!("lost track of {version}: {error}"),
                    Ok(None) if std::time::Instant::now() > deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break format!("{version} did not finish starting within {timeout} seconds");
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(250)),
                }
            }
        }
    };
    eprintln!("update: {reason}; restoring the previous version");
    let mut state = load(dir);
    if let Err(error) = rollback(dir, &mut state, &reason) {
        eprintln!("update: rollback failed: {error:#}");
        std::process::exit(1);
    }
}

fn rollback(dir: &Path, state: &mut Persisted, reason: &str) -> Result<()> {
    let trial = state.trial.take().context("no update is being tried")?;
    anyhow::ensure!(trial.previous.join("Contents/Info.plist").exists(), "the previous version is missing");
    let failed = updates(dir).join("failed").join(trial.target.file_name().unwrap_or_default());
    let _ = std::fs::remove_dir_all(&failed);
    move_dir(&trial.target, &failed)?;
    move_dir(&trial.previous, &trial.target)?;
    let _ = std::fs::remove_dir_all(&failed);
    state.last = Some(Outcome { result: "rolled_back".into(), version: trial.version, from: trial.from, at: now(), reason: Some(reason.into()) });
    save(dir, state)
}

fn relaunch_and_exit(app: &Path, wait_for_exit: bool) -> ! {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(executable(app));
    command.args(std::env::args_os().skip(1)).env_remove(SUPERVISED_ENV);
    if wait_for_exit {
        let _ = command.env(WAIT_ENV, std::process::id().to_string()).spawn();
        std::process::exit(0);
    }
    let error = command.exec();
    eprintln!("update: could not restart: {error}");
    std::process::exit(1)
}

/// Called once the shell has loaded: the running version is good.
pub fn confirm_started() {
    let (Some(dir), Some(app)) = (DIR.get(), bundle_path()) else { return };
    let mut state = load(dir);
    let current = current_version();
    let Some(trial) = state.trial.clone().filter(|t| t.target == app && t.version == current) else { return };
    state.trial = None;
    state.last = Some(Outcome { result: "updated".into(), version: trial.version, from: trial.from, at: now(), reason: None });
    if let Err(error) = save(dir, &state) {
        eprintln!("update: could not record a successful start: {error:#}");
    }
}

/// Restarts the app after it quits, so a staged update is installed.
pub fn relaunch_requested() -> bool {
    RELAUNCH.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn relaunch() {
    if let Some(app) = bundle_path() {
        relaunch_and_exit(&app, true);
    }
}

// ── Checking, downloading, and staging ───────────────────────────────────────

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("Aside/{}", current_version()))
        .connect_timeout(Duration::from_secs(15))
        .build()?)
}

async fn fetch_release(feed: &str) -> Result<Option<Release>> {
    let url = allowed_url(feed)?;
    let response = client()?.get(url).header("Accept", "application/vnd.github+json").timeout(Duration::from_secs(30)).send().await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let doc: Value = response.error_for_status()?.json().await?;
    if doc["draft"] == true || doc["prerelease"] == true {
        return Ok(None);
    }
    let version = doc["tag_name"].as_str().context("the release has no tag_name")?.trim_start_matches('v').to_string();
    semver::Version::parse(&version).with_context(|| format!("release tag {version} is not a semantic version"))?;
    let assets = doc["assets"].as_array().cloned().unwrap_or_default();
    let suffix = format!("-macos-{}.zip", arch());
    let asset = assets.iter().find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(&suffix)))
        .with_context(|| format!("release {version} has no *{suffix} asset"))?;
    let name = asset["name"].as_str().unwrap_or_default().to_string();
    let signature = assets.iter().find(|a| a["name"].as_str() == Some(&format!("{name}.sig")))
        .with_context(|| format!("release {version} has no {name}.sig signature"))?;
    let link = |a: &Value| a["browser_download_url"].as_str().map(str::to_string).context("asset has no download URL");
    Ok(Some(Release { version, notes: doc["body"].as_str().unwrap_or_default().chars().take(4000).collect(), url: link(asset)?, signature_url: link(signature)?, asset: name }))
}

async fn download(url: &str, to: &Path, limit: u64) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let response = client()?.get(allowed_url(url)?).send().await?.error_for_status()?;
    if response.url().scheme() != "https" {
        allowed_url(response.url().as_str())?;
    }
    let mut file = tokio::fs::File::create(to).await?;
    let mut stream = response.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        if total > limit {
            bail!("download exceeds {limit} bytes");
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

fn set_phase(phase: &'static str, error: Option<String>) {
    let mut runtime = RUNTIME.lock().unwrap();
    runtime.phase = phase;
    runtime.error = error;
}

async fn check(dir: &Path) -> Result<Option<Release>> {
    if let Some(reason) = disabled_reason(dir) {
        bail!(reason);
    }
    set_phase("checking", None);
    let release = fetch_release(&feed_url(&config(dir)).unwrap_or_default()).await;
    let mut runtime = RUNTIME.lock().unwrap();
    runtime.last_check = Some(now());
    match release {
        Ok(release) => {
            runtime.available = release.filter(|r| newer(&r.version, &current_version()));
            runtime.phase = if load(dir).staged.is_some() { "staged" } else { "idle" };
            runtime.error = None;
            Ok(runtime.available.clone())
        }
        Err(error) => {
            runtime.phase = "error";
            runtime.error = Some(format!("{error:#}"));
            Err(error)
        }
    }
}

/// Downloads, verifies, extracts, and stages a release for installation on the next launch.
async fn stage(dir: &Path, release: &Release) -> Result<()> {
    let key = public_key().context("This build has no update signing key")?;
    let app = bundle_path().context("Not running from an app bundle")?;
    let root = updates(dir);
    let work = root.join(format!("download-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&work)?;
    let result = async {
        set_phase("downloading", None);
        let archive = work.join("update.zip");
        let signature_file = work.join("update.zip.sig");
        download(&release.signature_url, &signature_file, 4096).await?;
        download(&release.url, &archive, MAX_ARCHIVE_BYTES).await?;
        set_phase("verifying", None);
        let staged_app = tokio::task::spawn_blocking({
            let (work, root, app, release) = (work.clone(), root.clone(), app.clone(), release.clone());
            move || -> Result<PathBuf> {
                let (sha256, size) = crate::update_sig::digest_file(&archive)?;
                crate::update_sig::verify(&key, &release.version, &sha256, size, &std::fs::read_to_string(&signature_file)?)?;
                let extracted = work.join("extracted");
                run("/usr/bin/ditto", &["-x".as_ref(), "-k".as_ref(), archive.as_os_str(), extracted.as_os_str()])?;
                let apps: Vec<PathBuf> = std::fs::read_dir(&extracted)?.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|e| e == "app")).collect();
                let [new_app] = apps.as_slice() else { bail!("the archive must contain exactly one .app") };
                let version = info(new_app, "CFBundleVersion").unwrap_or_default();
                anyhow::ensure!(version == release.version, "the archive contains version {version}, not {}", release.version);
                anyhow::ensure!(info(new_app, "CFBundleIdentifier") == info(&app, "CFBundleIdentifier"), "the archive contains a different application");
                run("/usr/bin/codesign", &["--verify".as_ref(), "--deep".as_ref(), "--strict".as_ref(), new_app.as_os_str()])
                    .context("the new app's code signature is not valid")?;
                if let Some(team) = team_id(&app) {
                    anyhow::ensure!(team_id(new_app).as_deref() == Some(team.as_str()), "the new app is not signed by team {team}");
                }
                unquarantine(new_app)?;
                let staged = root.join("staged").join(app.file_name().unwrap_or_default());
                let _ = std::fs::remove_dir_all(&staged);
                move_dir(new_app, &staged)?;
                Ok(staged)
            }
        }).await??;
        let mut state = load(dir);
        state.staged = Some(Staged { version: release.version.clone(), app: staged_app, target: app });
        save(dir, &state)
    }.await;
    let _ = std::fs::remove_dir_all(&work);
    match &result {
        Ok(()) => set_phase("staged", None),
        Err(error) => set_phase("error", Some(format!("{error:#}"))),
    }
    result
}

fn status(dir: &Path) -> Value {
    let runtime = RUNTIME.lock().unwrap();
    let state = load(dir);
    let config = config(dir);
    let reason = disabled_reason(dir);
    json!({
        "enabled": reason.is_none(),
        "disabled_reason": reason,
        "current_version": current_version(),
        "app": bundle_path(),
        "feed": feed_url(&config),
        "auto": config.auto.unwrap_or(true),
        "public_key": public_key(),
        "phase": if reason.is_some() { "disabled" } else { runtime.phase },
        "available": runtime.available,
        "staged": state.staged.as_ref().map(|s| json!({ "version": s.version })),
        "last_check": runtime.last_check,
        "error": runtime.error,
        "last": state.last,
        "rollback_available": state.trial.is_none() && updates(dir).join("previous").read_dir().is_ok_and(|mut d| d.next().is_some()),
    })
}

/// Checks the feed and stages a newer version if there is one.
async fn update(app: &App, dir: &Path) -> Result<()> {
    let _busy = BUSY.lock().await;
    if let Some(release) = check(dir).await? {
        if load(dir).staged.as_ref().is_some_and(|s| s.version == release.version) {
            return Ok(());
        }
        stage(dir, &release).await?;
        let _ = app.events.send(json!({ "type": "aside.update.ready", "version": release.version }));
    }
    Ok(())
}

/// Periodic background checks, unless `"auto": false` in `update.json`.
pub fn spawn_auto(app: Arc<App>) {
    let Some(dir) = DIR.get().cloned() else { return };
    if disabled_reason(&dir).is_some() || config(&dir).auto == Some(false) {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            if let Err(error) = update(&app, &dir).await {
                eprintln!("update check: {error:#}");
            }
            tokio::time::sleep(AUTO_INTERVAL).await;
        }
    });
}

type Shared = State<Arc<App>>;

fn dir() -> PathBuf {
    DIR.get().cloned().unwrap_or_default()
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/update/status", get(|| async { Json(status(&dir())) }))
        .route("/api/update/check", post(|| async {
            let dir = dir();
            let _busy = BUSY.lock().await;
            let result = check(&dir).await;
            let mut body = status(&dir);
            if let Err(error) = result { body["error"] = json!(format!("{error:#}")); }
            Json(body)
        }))
        .route("/api/update/apply", post(|State(app): Shared| async move {
            let dir = dir();
            let result = update(&app, &dir).await;
            let mut body = status(&dir);
            if let Err(error) = result { body["error"] = json!(format!("{error:#}")); }
            Json(body)
        }))
        .route("/api/update/restart", post(|State(app): Shared| async move {
            if load(&dir()).staged.is_none() {
                return Json(json!({ "error": "No update is ready to install" }));
            }
            RELAUNCH.store(true, std::sync::atomic::Ordering::SeqCst);
            let body = json!({ "cmd": "quit" });
            Json(match app.agent.browser.op(|reply| crate::app::Op::Shell { body, reply }).await {
                Ok(()) => json!({ "ok": true }),
                Err(error) => json!({ "error": error.to_string() }),
            })
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_urls() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("1.0.0", "1.0.0-beta.1"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("garbage", "0.1.0"));
        assert!(allowed_url("https://api.github.com/x").is_ok());
        assert!(allowed_url("http://127.0.0.1:7802/feed").is_ok());
        assert!(allowed_url("http://example.com/feed").is_err());
        assert!(allowed_url("file:///tmp/x").is_err());
    }

    #[test]
    fn verified_updates_lose_their_quarantine() {
        let app = std::env::temp_dir().join(format!("aside-quarantine-test-{}/Aside.app", uuid::Uuid::new_v4()));
        let exe = app.join("Contents/MacOS/aside");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"test").unwrap();
        run("/usr/bin/xattr", &["-w".as_ref(), "com.apple.quarantine".as_ref(), "0081;00000000;aside;".as_ref(), exe.as_os_str()]).unwrap();
        assert!(run("/usr/bin/xattr", &["-l".as_ref(), exe.as_os_str()]).unwrap().contains("com.apple.quarantine"));
        unquarantine(&app).unwrap();
        assert!(!run("/usr/bin/xattr", &["-r".as_ref(), "-l".as_ref(), app.as_os_str()]).unwrap().contains("com.apple.quarantine"));
        unquarantine(&app).unwrap();
        std::fs::remove_dir_all(app.parent().unwrap()).unwrap();
    }

    #[test]
    fn swap_and_rollback_restore_the_previous_bundle() {
        let dir = std::env::temp_dir().join(format!("aside-updater-test-{}", uuid::Uuid::new_v4()));
        let target = dir.join("Apps/Aside.app");
        let previous = updates(&dir).join("previous/Aside.app");
        std::fs::create_dir_all(target.join("Contents")).unwrap();
        std::fs::create_dir_all(previous.join("Contents")).unwrap();
        std::fs::write(target.join("Contents/Info.plist"), "new").unwrap();
        std::fs::write(previous.join("Contents/Info.plist"), "old").unwrap();
        let mut state = Persisted { trial: Some(Trial { version: "0.2.0".into(), from: "0.1.0".into(), target: target.clone(), previous: previous.clone(), launches: 0 }), ..Default::default() };
        rollback(&dir, &mut state, "test").unwrap();
        assert_eq!(std::fs::read_to_string(target.join("Contents/Info.plist")).unwrap(), "old");
        assert!(!previous.exists());
        let saved = load(&dir);
        assert!(saved.trial.is_none());
        assert_eq!(saved.last.unwrap().result, "rolled_back");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
