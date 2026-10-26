use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    size: u64,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Serialize)]
struct LatestRelease {
    version: String,
    download_url: String,
    size: u64,
    asset_name: String,
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("CTRLServers-Installer")
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))
}

fn platform_asset_name() -> &'static str {
    match std::env::consts::OS {
        "windows" => "CTRLServers.exe",
        "macos" => "CTRLServers.dmg",
        _ => "CTRLServers.AppImage",
    }
}

const RANGE_CONNECTIONS: u64 = 8;
const MIN_PARALLEL_BYTES: u64 = 4 * 1024 * 1024;
const WRITE_BUFFER_BYTES: usize = 64 * 1024;
const PROGRESS_INTERVAL_MS: u64 = 100;

fn emit_progress(app: &AppHandle, downloaded: u64, total: u64) {
    let _ = app.emit(
        "download-progress",
        serde_json::json!({ "downloaded": downloaded, "total": total }),
    );
}

#[tauri::command]
async fn get_latest_release() -> Result<LatestRelease, String> {
    let release: GitHubRelease = http_client()?
        .get("https://api.github.com/repos/CTRLServers/app/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Failed to reach GitHub: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub API error: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse release info: {e}"))?;

    let asset_name = platform_asset_name();
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            format!(
                "Release {version} does not include {asset_name}",
                version = release.tag_name
            )
        })?;

    Ok(LatestRelease {
        version: release.tag_name,
        download_url: asset.browser_download_url.clone(),
        size: asset.size,
        asset_name: asset.name.clone(),
    })
}

#[tauri::command]
async fn get_default_install_path() -> Result<String, String> {
    let base = std::env::var("LOCALAPPDATA")
        .map_err(|_| "Could not resolve the local AppData folder".to_string())?;
    Ok(std::path::Path::new(&base)
        .join("Programs")
        .join("CTRLServers")
        .to_string_lossy()
        .into_owned())
}

async fn download_single(
    client: reqwest::Client,
    url: String,
    tmp: std::path::PathBuf,
    counter: Arc<AtomicU64>,
) -> Result<(), String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Download failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "Download failed with HTTP status {status}",
            status = resp.status()
        ));
    }

    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("Failed to create {}: {e}", tmp.display()))?;

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(WRITE_BUFFER_BYTES);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Download error: {e}"))?;
        counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        buf.extend_from_slice(&chunk);
        if buf.len() >= WRITE_BUFFER_BYTES {
            file.write_all(&buf)
                .await
                .map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
            buf.clear();
        }
    }

    if !buf.is_empty() {
        file.write_all(&buf)
            .await
            .map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    }

    file.flush()
        .await
        .map_err(|e| format!("Failed to finalize {}: {e}", tmp.display()))?;
    Ok(())
}

async fn download_range(
    client: reqwest::Client,
    url: String,
    tmp: std::path::PathBuf,
    start: u64,
    end: u64,
    counter: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), String> {
    use futures_util::StreamExt;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let resp = client
        .get(&url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| format!("Download failed: {e}"))?;

    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!(
            "Download failed with HTTP status {status}",
            status = resp.status()
        ));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .await
        .map_err(|e| format!("Failed to open {}: {e}", tmp.display()))?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|e| format!("Failed to seek in {}: {e}", tmp.display()))?;

    let expected = end - start + 1;
    let mut received: u64 = 0;
    let mut buf: Vec<u8> = Vec::with_capacity(WRITE_BUFFER_BYTES);
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Download aborted".to_string());
        }
        let chunk = chunk.map_err(|e| format!("Download error: {e}"))?;
        received += chunk.len() as u64;
        counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        buf.extend_from_slice(&chunk);
        if buf.len() >= WRITE_BUFFER_BYTES {
            file.write_all(&buf)
                .await
                .map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
            buf.clear();
        }
    }

    if received != expected {
        return Err(format!(
            "Incomplete download segment (received {received} of {expected} bytes)"
        ));
    }

    if !buf.is_empty() {
        file.write_all(&buf)
            .await
            .map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    }
    Ok(())
}

#[tauri::command]
async fn install_app(
    app: AppHandle,
    download_url: String,
    install_dir: String,
    asset_name: String,
) -> Result<String, String> {
    let install_dir = install_dir.trim();
    if install_dir.is_empty() {
        return Err("Install path cannot be empty".to_string());
    }

    let dir = std::path::PathBuf::from(install_dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("Failed to create install folder: {e}"))?;

    let dest = dir.join(&asset_name);
    let tmp = dir.join(format!("{asset_name}.download"));

    let client = http_client()?;

    let probe = client
        .get(&download_url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|e| format!("Download failed: {e}"))?;

    if !probe.status().is_success() {
        return Err(format!(
            "Download failed with HTTP status {status}",
            status = probe.status()
        ));
    }

    let range_total = probe
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit_once('/'))
        .and_then(|(_, total)| total.trim().parse::<u64>().ok());
    let ranges_supported =
        probe.status() == reqwest::StatusCode::PARTIAL_CONTENT && range_total.is_some();
    let total = range_total
        .or_else(|| probe.content_length())
        .unwrap_or(0);
    let final_url = probe.url().to_string();
    drop(probe);

    let use_parallel = ranges_supported && total >= MIN_PARALLEL_BYTES;

    let downloaded = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let ticker = {
        let app = app.clone();
        let downloaded = downloaded.clone();
        let done = done.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(PROGRESS_INTERVAL_MS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if done.load(Ordering::Relaxed) {
                    break;
                }
                emit_progress(&app, downloaded.load(Ordering::Relaxed), total);
            }
        })
    };

    let result: Result<(), String> = async {
        if use_parallel {
            let file = tokio::fs::File::create(&tmp)
                .await
                .map_err(|e| format!("Failed to create {asset_name}: {e}"))?;
            file.set_len(total)
                .await
                .map_err(|e| format!("Failed to prepare {asset_name}: {e}"))?;
            drop(file);

            let cancelled = Arc::new(AtomicBool::new(false));
            let per = total / RANGE_CONNECTIONS;
            let mut handles = Vec::with_capacity(RANGE_CONNECTIONS as usize);
            for i in 0..RANGE_CONNECTIONS {
                let start = i * per;
                let end = if i == RANGE_CONNECTIONS - 1 {
                    total - 1
                } else {
                    (i + 1) * per - 1
                };
                handles.push(tokio::spawn(download_range(
                    client.clone(),
                    final_url.clone(),
                    tmp.clone(),
                    start,
                    end,
                    downloaded.clone(),
                    cancelled.clone(),
                )));
            }

            let mut first_error: Option<String> = None;
            for handle in handles {
                match handle.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        cancelled.store(true, Ordering::Relaxed);
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                    Err(e) => {
                        cancelled.store(true, Ordering::Relaxed);
                        if first_error.is_none() {
                            first_error = Some(format!("Download task failed: {e}"));
                        }
                    }
                }
            }

            match first_error {
                Some(e) => Err(e),
                None => Ok(()),
            }
        } else {
            download_single(client.clone(), final_url.clone(), tmp.clone(), downloaded.clone())
                .await
        }
    }
    .await;

    done.store(true, Ordering::Relaxed);
    let _ = ticker.await;
    emit_progress(&app, downloaded.load(Ordering::Relaxed), total);

    result?;

    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|e| format!("Failed to move {asset_name} into place: {e}"))?;

    Ok(dest.to_string_lossy().into_owned())
}

#[tauri::command]
fn create_desktop_shortcut(exe_path: String) -> Result<String, String> {
    #[cfg(windows)]
    {
        use windows::core::{Interface, PCWSTR};
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

        let desktop = dirs::desktop_dir()
            .ok_or_else(|| "Could not locate the Desktop folder".to_string())?;
        let lnk_path = desktop.join("CTRLServers.lnk");
        let workdir = std::path::Path::new(&exe_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let lnk_str = lnk_path.to_string_lossy().into_owned();
        let lnk_w: Vec<u16> = lnk_str.encode_utf16().chain(std::iter::once(0)).collect();
        let exe_w: Vec<u16> = exe_path.encode_utf16().chain(std::iter::once(0)).collect();
        let work_w: Vec<u16> = workdir.encode_utf16().chain(std::iter::once(0)).collect();

        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .map_err(|e| format!("COM initialization failed: {e}"))?;

            let result = (|| -> Result<(), String> {
                let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("Failed to create shortcut object: {e}"))?;
                link.SetPath(PCWSTR(exe_w.as_ptr()))
                    .map_err(|e| format!("Failed to set shortcut target: {e}"))?;
                link.SetWorkingDirectory(PCWSTR(work_w.as_ptr()))
                    .map_err(|e| format!("Failed to set shortcut folder: {e}"))?;
                let persist: IPersistFile = link
                    .cast()
                    .map_err(|e| format!("Failed to query shortcut file interface: {e}"))?;
                persist
                    .Save(PCWSTR(lnk_w.as_ptr()), true)
                    .map_err(|e| format!("Failed to save shortcut: {e}"))
            })();

            CoUninitialize();
            result?;
        }

        Ok(lnk_str)
    }
    #[cfg(not(windows))]
    {
        let _ = exe_path;
        Err("Desktop shortcuts are currently only supported on Windows".to_string())
    }
}

#[tauri::command]
fn set_autostart(enable: bool, exe_path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
        use winreg::RegKey;

        const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if enable {
            let (key, _) = hkcu
                .create_subkey_with_flags(RUN_KEY, KEY_WRITE)
                .map_err(|e| format!("Failed to open the auto-start registry key: {e}"))?;
            key.set_value("CTRLServers", &format!("\"{exe_path}\""))
                .map_err(|e| format!("Failed to enable auto-start: {e}"))?;
        } else if let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_WRITE) {
            let _ = key.delete_value("CTRLServers");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (enable, exe_path);
        Err("Auto-start is currently only supported on Windows".to_string())
    }
}

#[tauri::command]
fn launch_app(exe_path: String) -> Result<(), String> {
    let dir = std::path::Path::new(&exe_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

        std::process::Command::new(&exe_path)
            .current_dir(&dir)
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|e| format!("Failed to launch CTRLServers: {e}"))?;
    }

    #[cfg(not(windows))]
    {
        std::process::Command::new(&exe_path)
            .current_dir(&dir)
            .spawn()
            .map_err(|e| format!("Failed to launch CTRLServers: {e}"))?;
    }

    Ok(())
}

#[tauri::command]
fn open_install_folder(exe_path: String) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", exe_path))
        .spawn()
        .map_err(|e| format!("Failed to open the install folder: {e}"))?;
    Ok(())
}

#[tauri::command]
fn exit_installer(app: AppHandle) {
    app.exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            get_latest_release,
            get_default_install_path,
            install_app,
            create_desktop_shortcut,
            set_autostart,
            launch_app,
            open_install_folder,
            exit_installer,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
