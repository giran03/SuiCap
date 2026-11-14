//! Updates from GitHub Releases (same scheme as SuiNotch). The latest release must carry the
//! NSIS installer (`SuiCap_<version>_x64-setup.exe`, made by `tauri build`) and a
//! `SHA256SUMS.txt` listing it. The installer is checked against that sum, checked again right
//! before launch, then run silently; it closes this copy and starts the new one.

use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::Serialize;
use serde_json::Value;
use suicap_core::config::UpdateMode;
use tauri::{AppHandle, Emitter, Manager};
use windows::{
    Foundation::Uri,
    Security::Cryptography::{
        Core::{HashAlgorithmNames, HashAlgorithmProvider},
        CryptographicBuffer,
    },
    Storage::Streams::{DataReader, IBuffer},
    Web::Http::{HttpClient, HttpStatusCode},
    Win32::{
        System::SystemInformation::GetTickCount,
        UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
    },
    core::HSTRING,
};

use crate::state::{AppState, Phase};

/// GitHub `owner/name` the releases come from.
pub const REPO: &str = "giran03/SuiCap";
pub const SUPPORT_EMAIL: &str = "support@cafetako.com";
pub const COFFEE_URL: &str = "https://buymeacoffee.com/cafetako";
pub const DEVELOPER: &str = "CafeTako";

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SUMS_NAME: &str = "SHA256SUMS.txt";
const FIRST_CHECK: Duration = Duration::from_secs(90);
/// Automatic installs wait until the PC has been left alone this long.
const IDLE_BEFORE_INSTALL_S: u32 = 10 * 60;
const EVENT: &str = "update";

#[derive(Clone, Debug, PartialEq)]
struct Release {
    version: String,
    setup_url: String,
    setup_name: String,
    sums_url: String,
}

/// An installer whose checksum was verified.
#[derive(Clone, Debug)]
struct Download {
    release: Release,
    path: PathBuf,
    sha256: String,
}

/// What the settings page shows.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available {
        version: String,
    },
    Downloading {
        version: String,
    },
    Ready {
        version: String,
    },
    Installing {
        version: String,
    },
    Failed {
        message: String,
        version: Option<String>,
    },
}

#[derive(Default)]
struct Inner {
    state: State,
    release: Option<Release>,
    download: Option<Download>,
    /// Unix seconds of the last finished check.
    last_checked: Option<u64>,
}

#[derive(Clone, Serialize)]
pub struct Info {
    pub current: &'static str,
    pub state: State,
    pub last_checked: Option<u64>,
}

static INNER: Mutex<Option<Inner>> = Mutex::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);

fn with<T>(f: impl FnOnce(&mut Inner) -> T) -> T {
    let mut guard = INNER.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Inner::default))
}

pub fn info() -> Info {
    with(|i| Info {
        current: VERSION,
        state: i.state.clone(),
        last_checked: i.last_checked,
    })
}

fn set(app: &AppHandle, state: State) {
    let info = with(|i| {
        i.state = state;
        Info {
            current: VERSION,
            state: i.state.clone(),
            last_checked: i.last_checked,
        }
    });
    let _ = app.emit(EVENT, info);
}

struct Busy;
impl Busy {
    fn acquire() -> Option<Self> {
        BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(Self)
    }
}
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

fn mode(app: &AppHandle) -> UpdateMode {
    app.state::<AppState>().settings().update_mode
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ------------------------------------------------------------------ flow

/// Background check: runs once shortly after launch when the mode allows it.
pub fn start(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_CHECK);
        let m = mode(&app);
        if m != UpdateMode::Off
            && let Some(release) = run_check(&app)
            && m == UpdateMode::Auto
            && run_download(&app, &release)
            && wait_until_idle(&app)
        {
            run_install(&app);
        }
    });
}

/// "Check for updates" in Settings: works in every mode.
pub fn check_now(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        run_check(&app);
    });
}

/// Download and verify now; the restart stays the user's call.
pub fn download(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Some(release) = with(|i| i.release.clone()) {
            run_download(&app, &release);
        }
    });
}

/// "Install and restart" in Settings.
pub fn install(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let ready = with(|i| i.download.as_ref().is_some_and(|d| d.path.is_file()));
        if ready || with(|i| i.release.clone()).is_some_and(|r| run_download(&app, &r)) {
            run_install(&app);
        }
    });
}

fn run_check(app: &AppHandle) -> Option<Release> {
    let _busy = Busy::acquire()?;
    set(app, State::Checking);
    let result = latest();
    with(|i| i.last_checked = Some(now_secs()));
    match result {
        Ok(Some(r)) if is_newer(&r.version, VERSION) => {
            // A verified download of this very version stays Ready.
            let ready = with(|i| i.download.as_ref().is_some_and(|d| d.release == r && d.path.is_file()));
            with(|i| i.release = Some(r.clone()));
            set(
                app,
                if ready {
                    State::Ready {
                        version: r.version.clone(),
                    }
                } else {
                    State::Available {
                        version: r.version.clone(),
                    }
                },
            );
            Some(r)
        }
        Ok(_) => {
            set(app, State::UpToDate);
            None
        }
        Err(message) => {
            eprintln!("update check failed: {message}");
            set(app, State::Failed { message, version: None });
            None
        }
    }
}

/// True when a verified installer for `release` is on disk afterwards.
fn run_download(app: &AppHandle, release: &Release) -> bool {
    if with(|i| {
        i.download
            .as_ref()
            .is_some_and(|d| d.release == *release && d.path.is_file())
    }) {
        set(
            app,
            State::Ready {
                version: release.version.clone(),
            },
        );
        return true;
    }
    let Some(_busy) = Busy::acquire() else { return false };
    set(
        app,
        State::Downloading {
            version: release.version.clone(),
        },
    );
    match download_verified(release) {
        Ok(d) => {
            with(|i| i.download = Some(d));
            set(
                app,
                State::Ready {
                    version: release.version.clone(),
                },
            );
            true
        }
        Err(message) => {
            eprintln!("update download failed: {message}");
            set(
                app,
                State::Failed {
                    message,
                    version: Some(release.version.clone()),
                },
            );
            false
        }
    }
}

/// Stops capture so saves finish their files, runs the installer, and brings capture back if it didn't replace us.
fn run_install(app: &AppHandle) {
    let Some(download) = with(|i| i.download.clone()) else {
        return;
    };
    let Some(_busy) = Busy::acquire() else { return };
    let version = download.release.version.clone();
    if let Err(message) = verify_download(&download) {
        with(|i| i.download = None);
        set(
            app,
            State::Failed {
                message,
                version: Some(version),
            },
        );
        return;
    }
    set(
        app,
        State::Installing {
            version: version.clone(),
        },
    );
    let state = app.state::<AppState>();
    let was_armed = state.status().phase == Phase::Armed;
    state.disarm(app);
    // `/S` silent, `/UPDATE` makes the Tauri installer close the running app and reopen the new one.
    let outcome = std::process::Command::new(&download.path)
        .args(["/S", "/UPDATE"])
        .spawn()
        .map_err(|_| "Couldn't start the installer".to_string())
        .and_then(|mut c| c.wait().map_err(|_| "Couldn't read the installer result".to_string()))
        .and_then(|exit| {
            exit.success()
                .then_some(())
                .ok_or_else(|| "Installation did not complete. Please try again.".to_string())
        });
    // Still running here means the installer failed (a successful one ends this process).
    if let Err(message) = outcome {
        eprintln!("update install failed: {message}");
        with(|i| i.download = None);
        set(
            app,
            State::Failed {
                message,
                version: Some(version),
            },
        );
    } else {
        set(app, State::UpToDate);
    }
    if was_armed {
        state.arm(app);
    }
}

/// Automatic installs wait for a quiet PC and never interrupt a manual recording.
fn wait_until_idle(app: &AppHandle) -> bool {
    loop {
        if mode(app) != UpdateMode::Auto {
            return false;
        }
        if idle_seconds() >= IDLE_BEFORE_INSTALL_S && !app.state::<AppState>().status().recording {
            return true;
        }
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn idle_seconds() -> u32 {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid, correctly sized LASTINPUTINFO.
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            return GetTickCount().wrapping_sub(info.dwTime) / 1000;
        }
    }
    0
}

// ------------------------------------------------------------------ network

fn client() -> windows::core::Result<HttpClient> {
    let client = HttpClient::new()?;
    let headers = client.DefaultRequestHeaders()?;
    let _ = headers
        .UserAgent()?
        .TryParseAdd(&HSTRING::from(format!("SuiCap/{VERSION} (update check)")));
    let _ = headers.TryAppendWithoutValidation(&HSTRING::from("Accept"), &HSTRING::from("application/vnd.github+json"));
    Ok(client)
}

fn get_buffer(client: &HttpClient, url: &str) -> Result<IBuffer, String> {
    let uri = Uri::CreateUri(&HSTRING::from(url)).map_err(|e| e.message())?;
    let resp = client
        .GetAsync(&uri)
        .and_then(|op| op.join())
        .map_err(|_| "No internet connection".to_string())?;
    let code = resp.StatusCode().map_err(|e| e.message())?;
    if code == HttpStatusCode::NotFound {
        return Err("No public release found".into());
    }
    if !resp.IsSuccessStatusCode().unwrap_or(false) {
        return Err(format!("Update server replied {}", code.0));
    }
    resp.Content()
        .and_then(|c| c.ReadAsBufferAsync())
        .and_then(|op| op.join())
        .map_err(|e| e.message())
}

fn bytes(buf: &IBuffer) -> Result<Vec<u8>, String> {
    let len = buf.Length().map_err(|e| e.message())? as usize;
    let mut out = vec![0u8; len];
    let reader = DataReader::FromBuffer(buf).map_err(|e| e.message())?;
    reader.ReadBytes(&mut out).map_err(|e| e.message())?;
    Ok(out)
}

fn latest() -> Result<Option<Release>, String> {
    let client = client().map_err(|e| e.message())?;
    let body = bytes(&get_buffer(
        &client,
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
    )?)?;
    let json: Value = serde_json::from_slice(&body).map_err(|_| "Unreadable release data".to_string())?;
    Ok(parse_release(&json))
}

fn download_verified(release: &Release) -> Result<Download, String> {
    let client = client().map_err(|e| e.message())?;
    let sums = String::from_utf8(bytes(&get_buffer(&client, &release.sums_url)?)?)
        .map_err(|_| "Unreadable checksum file".to_string())?;
    let expected = expected_sum(&sums, &release.setup_name).ok_or("The release has no checksum for its installer")?;
    let setup = get_buffer(&client, &release.setup_url)?;
    if sha256_hex(&setup).map_err(|e| e.message())? != expected {
        return Err("The download was damaged, please try again".into());
    }
    let dir = std::env::temp_dir().join("SuiCap-update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&release.setup_name);
    std::fs::write(&path, bytes(&setup)?).map_err(|e| e.to_string())?;
    Ok(Download {
        release: release.clone(),
        path,
        sha256: expected,
    })
}

fn verify_download(download: &Download) -> Result<(), String> {
    let data =
        std::fs::read(&download.path).map_err(|_| "The downloaded installer is missing. Download the update again.")?;
    let buffer = CryptographicBuffer::CreateFromByteArray(&data).map_err(|e| e.message())?;
    if sha256_hex(&buffer).map_err(|e| e.message())? != download.sha256 {
        return Err("The installer changed after download. Download the update again.".into());
    }
    Ok(())
}

fn sha256_hex(buf: &IBuffer) -> windows::core::Result<String> {
    let provider = HashAlgorithmProvider::OpenAlgorithm(&HashAlgorithmNames::Sha256()?)?;
    let hash = provider.HashData(buf)?;
    Ok(CryptographicBuffer::EncodeToHexString(&hash)?
        .to_string()
        .to_ascii_lowercase())
}

// ------------------------------------------------------------------ parsing

/// Reads a GitHub "latest release" response; None when it lacks the installer or its sums.
fn parse_release(v: &Value) -> Option<Release> {
    let version = v.get("tag_name")?.as_str()?.trim_start_matches(['v', 'V']).to_string();
    if !version
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    {
        return None;
    }
    parse_version(&version)?;
    let assets = v.get("assets")?.as_array()?;
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    let asset = |pred: &dyn Fn(&str) -> bool| {
        assets.iter().find_map(|a| {
            let name = a.get("name")?.as_str()?;
            let url = a.get("browser_download_url")?.as_str()?;
            (pred(name) && url.starts_with(&prefix)).then(|| (name.to_string(), url.to_string()))
        })
    };
    let expected_name = format!("SuiCap_{version}_x64-setup.exe");
    let (setup_name, setup_url) = asset(&|n| n.eq_ignore_ascii_case(&expected_name))?;
    let (_, sums_url) = asset(&|n| n.eq_ignore_ascii_case(SUMS_NAME))?;
    Some(Release {
        version,
        setup_url,
        setup_name,
        sums_url,
    })
}

/// Finds `name` in a `sha256sum`-style file ("<hex>  <name>" per line).
fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim().trim_start_matches('*');
        (file.eq_ignore_ascii_case(name) && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let core = v.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u32>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release_json(asset: &str, url: &str) -> Value {
        json!({"tag_name":"v1.2.0","assets":[
            {"name":asset,"browser_download_url":url},
            {"name":"SHA256SUMS.txt","browser_download_url":format!("https://github.com/{REPO}/releases/download/v1.2.0/SHA256SUMS.txt")}]})
    }
    fn url(name: &str) -> String {
        format!("https://github.com/{REPO}/releases/download/v1.2.0/{name}")
    }

    #[test]
    fn reads_installer_and_sums() {
        let name = "SuiCap_1.2.0_x64-setup.exe";
        let r = parse_release(&release_json(name, &url(name))).unwrap();
        assert_eq!((r.version.as_str(), r.setup_name.as_str()), ("1.2.0", name));
    }

    #[test]
    fn rejects_wrong_name_foreign_host_and_missing_sums() {
        for name in [
            "../SuiCap_1.2.0_x64-setup.exe",
            "SuiCap_1.1.0_x64-setup.exe",
            "Other.exe",
        ] {
            assert!(parse_release(&release_json(name, &url(name))).is_none());
        }
        let name = "SuiCap_1.2.0_x64-setup.exe";
        assert!(parse_release(&release_json(name, "https://evil.example/setup.exe")).is_none());
        let v = json!({"tag_name":"v1.2.0","assets":[{"name":name,"browser_download_url":url(name)}]});
        assert!(parse_release(&v).is_none());
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("v2.0.0", "1.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
    }

    #[test]
    fn finds_sum_for_file() {
        let h = "956b4afaf1d2bbcc8c80789a4b4f3dae4a3f0cd285be6452a965bc96ca15e234";
        let sums = format!("{h}  a.exe\r\n{}  b.exe\n", "0".repeat(64));
        assert_eq!(expected_sum(&sums, "a.exe").as_deref(), Some(h));
        assert_eq!(
            expected_sum(&format!("{} *a.exe", h.to_uppercase()), "a.exe").as_deref(),
            Some(h)
        );
        assert_eq!(expected_sum("abc  a.exe", "a.exe"), None);
        assert_eq!(expected_sum(&sums, "missing.exe"), None);
    }

    #[test]
    fn hashes_with_sha256() {
        let buf = CryptographicBuffer::ConvertStringToBinary(
            &HSTRING::from("abc"),
            windows::Security::Cryptography::BinaryStringEncoding::Utf8,
        )
        .unwrap();
        assert_eq!(
            sha256_hex(&buf).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
