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
    let path = match std::env::consts::OS {
        "windows" => {
            let base = std::env::var("LOCALAPPDATA")
                .map_err(|_| "Could not resolve the local AppData folder".to_string())?;
            std::path::Path::new(&base)
                .join("Programs")
                .join("CTRLServers")
        }
        "macos" => dirs::home_dir()
            .ok_or_else(|| "Could not resolve the home folder".to_string())?
            .join("Applications"),
        _ => dirs::data_dir()
            .ok_or_else(|| "Could not resolve the data folder".to_string())?
            .join("CTRLServers"),
    };
    Ok(path.to_string_lossy().into_owned())
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

    #[cfg(target_os = "macos")]
    {
        let tmp_clone = tmp.clone();
        let dir_clone = dir.clone();
        let app_path = tokio::task::spawn_blocking(move || install_dmg(&tmp_clone, &dir_clone))
            .await
            .map_err(|e| format!("Installer task failed: {e}"))??;
        let _ = tokio::fs::remove_file(&tmp).await;
        Ok(app_path.to_string_lossy().into_owned())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let dest = dir.join(&asset_name);
        tokio::fs::rename(&tmp, &dest)
            .await
            .map_err(|e| format!("Failed to move {asset_name} into place: {e}"))?;

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                .await
                .map_err(|e| format!("Failed to mark {asset_name} executable: {e}"))?;
        }

        Ok(dest.to_string_lossy().into_owned())
    }
}

#[cfg(target_os = "macos")]
fn install_dmg(
    dmg: &std::path::Path,
    install_dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    use std::process::Command;

    let mount_dir = std::env::temp_dir().join("ctrlservers-installer-mount");
    let mount_str = mount_dir.to_string_lossy().into_owned();

    let _ = Command::new("hdiutil")
        .arg("detach")
        .arg("-force")
        .arg(&mount_str)
        .output();
    let _ = std::fs::remove_dir_all(&mount_dir);
    std::fs::create_dir_all(&mount_dir)
        .map_err(|e| format!("Failed to create the mount folder: {e}"))?;

    let dmg_str = dmg.to_string_lossy().into_owned();
    let attached = Command::new("hdiutil")
        .arg("attach")
        .arg(&dmg_str)
        .arg("-nobrowse")
        .arg("-readonly")
        .arg("-mountpoint")
        .arg(&mount_str)
        .output()
        .map_err(|e| format!("Failed to mount the installer image: {e}"))?;
    if !attached.status.success() {
        let _ = Command::new("hdiutil")
            .arg("detach")
            .arg("-force")
            .arg(&mount_str)
            .output();
        let _ = std::fs::remove_dir_all(&mount_dir);
        return Err(format!(
            "Failed to mount the installer image: {}",
            String::from_utf8_lossy(&attached.stderr).trim()
        ));
    }

    let copied = (|| -> Result<std::path::PathBuf, String> {
        let entries = std::fs::read_dir(&mount_dir)
            .map_err(|e| format!("Failed to read the mounted image: {e}"))?;
        let mut bundle: Option<std::path::PathBuf> = None;
        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read the mounted image: {e}"))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("app") {
                bundle = Some(path);
                break;
            }
        }
        let bundle = bundle
            .ok_or_else(|| "The downloaded image does not contain CTRLServers.app".to_string())?;
        let file_name = bundle
            .file_name()
            .ok_or_else(|| "Invalid application bundle name".to_string())?
            .to_os_string();
        let dest = install_dir.join(file_name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .map_err(|e| format!("Failed to remove the existing application: {e}"))?;
        }
        let copied = Command::new("cp")
            .arg("-R")
            .arg(&bundle)
            .arg(install_dir)
            .output()
            .map_err(|e| format!("Failed to copy CTRLServers: {e}"))?;
        if !copied.status.success() {
            return Err(format!(
                "Failed to copy CTRLServers into the install folder: {}",
                String::from_utf8_lossy(&copied.stderr).trim()
            ));
        }
        Ok(dest)
    })();

    let _ = Command::new("hdiutil")
        .arg("detach")
        .arg("-force")
        .arg(&mount_str)
        .output();
    let _ = std::fs::remove_dir_all(&mount_dir);

    copied
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

    #[cfg(target_os = "macos")]
    {
        let target = std::path::Path::new(&exe_path);
        let desktop = dirs::desktop_dir()
            .ok_or_else(|| "Could not locate the Desktop folder".to_string())?;
        let link = desktop.join("CTRLServers.app");
        match std::fs::symlink_metadata(&link) {
            Ok(_) => std::fs::remove_file(&link)
                .map_err(|e| format!("Failed to replace the desktop shortcut: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Failed to inspect the desktop shortcut: {e}")),
        }
        std::os::unix::fs::symlink(target, &link)
            .map_err(|e| format!("Failed to create the desktop shortcut: {e}"))?;
        Ok(link.to_string_lossy().into_owned())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let exe = std::path::Path::new(&exe_path);
        let desktop = dirs::desktop_dir()
            .ok_or_else(|| "Could not locate the Desktop folder".to_string())?;
        let shortcut = desktop.join("CTRLServers.desktop");
        let icon = ensure_linux_icon(exe)?;
        write_desktop_file(&shortcut, exe, &icon)?;
        Ok(shortcut.to_string_lossy().into_owned())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = exe_path;
        Err("Desktop shortcuts are not supported on this platform".to_string())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn ensure_linux_icon(exe: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let dir = exe
        .parent()
        .ok_or_else(|| "Could not resolve the install folder".to_string())?;
    let icon = dir.join("ctrlservers.png");
    if !icon.exists() {
        std::fs::write(&icon, include_bytes!("../icons/128x128.png"))
            .map_err(|e| format!("Failed to write the shortcut icon: {e}"))?;
    }
    Ok(icon)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn write_desktop_file(
    path: &std::path::Path,
    exe: &std::path::Path,
    icon: &std::path::Path,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let content = format!(
        "[Desktop Entry]\nType=Application\nName=CTRLServers\nExec=\"{}\"\nIcon={}\nTerminal=false\nCategories=Network;\n",
        exe.display(),
        icon.display()
    );
    std::fs::write(path, content)
        .map_err(|e| format!("Failed to write the desktop shortcut: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("Failed to mark the desktop shortcut executable: {e}"))?;
    Ok(())
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

    #[cfg(target_os = "macos")]
    {
        let home =
            dirs::home_dir().ok_or_else(|| "Could not resolve the home folder".to_string())?;
        let launch_dir = home.join("Library").join("LaunchAgents");
        std::fs::create_dir_all(&launch_dir)
            .map_err(|e| format!("Failed to create the launch agents folder: {e}"))?;
        let plist = launch_dir.join("xyz.ctrlservers.installer.plist");
        if enable {
            let content = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>xyz.ctrlservers.installer</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>-a</string>
        <string>{exe_path}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#,
                exe_path = exe_path
            );
            std::fs::write(&plist, content)
                .map_err(|e| format!("Failed to enable auto-start: {e}"))?;
            let _ = std::process::Command::new("launchctl")
                .arg("load")
                .arg("-w")
                .arg(&plist)
                .output();
        } else {
            let _ = std::process::Command::new("launchctl")
                .arg("unload")
                .arg("-w")
                .arg(&plist)
                .output();
            let _ = std::fs::remove_file(&plist);
        }
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let config = dirs::config_dir()
            .ok_or_else(|| "Could not resolve the config folder".to_string())?;
        let autostart = config.join("autostart");
        std::fs::create_dir_all(&autostart)
            .map_err(|e| format!("Failed to create the autostart folder: {e}"))?;
        let entry = autostart.join("CTRLServers.desktop");
        if enable {
            let exe = std::path::Path::new(&exe_path);
            let icon = ensure_linux_icon(exe)?;
            write_desktop_file(&entry, exe, &icon)?;
        } else {
            let _ = std::fs::remove_file(&entry);
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (enable, exe_path);
        Err("Auto-start is not supported on this platform".to_string())
    }
}

#[tauri::command]
fn launch_app(exe_path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

        let dir = std::path::Path::new(&exe_path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        std::process::Command::new(&exe_path)
            .current_dir(&dir)
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|e| format!("Failed to launch CTRLServers: {e}"))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&exe_path)
            .spawn()
            .map_err(|e| format!("Failed to launch CTRLServers: {e}"))?;
        Ok(())
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let dir = std::path::Path::new(&exe_path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        std::process::Command::new(&exe_path)
            .current_dir(&dir)
            .spawn()
            .map_err(|e| format!("Failed to launch CTRLServers: {e}"))?;
        Ok(())
    }
}

#[tauri::command]
fn open_install_folder(exe_path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", exe_path))
            .spawn()
            .map_err(|e| format!("Failed to open the install folder: {e}"))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&exe_path)
            .spawn()
            .map_err(|e| format!("Failed to open the install folder: {e}"))?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = std::path::Path::new(&exe_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| "Could not resolve the install folder".to_string())?;
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| format!("Failed to open the install folder: {e}"))?;
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = exe_path;
        Err("Opening the install folder is not supported on this platform".to_string())
    }
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
