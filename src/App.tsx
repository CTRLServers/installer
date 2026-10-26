import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import logoUrl from "./assets/logo.png";
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/inter/700.css";

type Screen = "welcome" | "installing" | "done" | "error";

interface LatestRelease {
  version: string;
  download_url: string;
  size: number;
  asset_name: string;
}

interface DownloadProgress {
  downloaded: number;
  total: number;
}

function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** i;
  return `${value >= 100 || i === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[i]}`;
}

function errText(err: unknown): string {
  return typeof err === "string" ? err : err instanceof Error ? err.message : String(err);
}

function TitleBar() {
  const win = getCurrentWindow();
  return (
    <div data-tauri-drag-region className="flex h-8 shrink-0 select-none">
      <div data-tauri-drag-region className="flex-1" />
      <div className="flex h-full">
        <button
          type="button"
          title="Minimize"
          onClick={() => void win.minimize()}
          className="flex h-full w-[46px] items-center justify-center text-[#aaa] transition-colors hover:bg-white/8 hover:text-white"
        >
          <svg width="12" height="12" viewBox="0 0 12 12" fill="none">
            <rect x="1" y="5.5" width="10" height="1" fill="currentColor" rx="0.5" />
          </svg>
        </button>
        <button
          type="button"
          title="Close"
          onClick={() => void win.close()}
          className="flex h-full w-[46px] items-center justify-center text-[#aaa] transition-colors hover:bg-[#e81123] hover:text-white"
        >
          <svg width="12" height="12" viewBox="0 0 12 12" fill="none">
            <path d="M2 2l8 8M10 2l-8 8" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
          </svg>
        </button>
      </div>
    </div>
  );
}

function Brand() {
  return (
    <div className="flex items-center gap-3">
      <img src={logoUrl} alt="CTRLServers" className="h-12 w-12 rounded-xl object-cover" />
      <span className="text-[20px] font-bold tracking-[-0.5px]">CTRLServers</span>
    </div>
  );
}

function ToggleRow({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (value: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      className="group flex w-full items-center justify-between rounded-[10px] border border-white/8 bg-white/4 px-4 py-3 text-left transition-colors hover:border-white/20"
    >
      <span className="text-sm text-[#aaa] transition-colors group-hover:text-white">{label}</span>
      <span
        className={`flex h-5 w-5 shrink-0 items-center justify-center rounded-md border-2 transition-all ${
          checked
            ? "border-white bg-white"
            : "border-[#555] bg-transparent group-hover:border-[#888]"
        }`}
      >
        {checked && (
          <svg width="11" height="11" viewBox="0 0 12 12" fill="none">
            <path
              d="M2.5 6.5l2.5 2.5 4.5-5"
              stroke="#000"
              strokeWidth="1.8"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
        )}
      </span>
    </button>
  );
}

function ContinueButton({
  onClick,
  disabled = false,
}: {
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className="h-11 w-full rounded-[10px] bg-white text-sm font-medium text-[#0e0e0e] transition-all hover:-translate-y-px hover:bg-[#ddd] hover:shadow-[0_8px_32px_rgba(255,255,255,0.12)] disabled:pointer-events-none disabled:opacity-40"
    >
      Continue
    </button>
  );
}

export default function App() {
  const [screen, setScreen] = useState<Screen>("welcome");
  const [installPath, setInstallPath] = useState("");
  const [desktopShortcut, setDesktopShortcut] = useState(true);
  const [autostart, setAutostart] = useState(false);
  const [runAfter, setRunAfter] = useState(true);
  const [statusText, setStatusText] = useState("Preparing...");
  const [downloaded, setDownloaded] = useState(0);
  const [total, setTotal] = useState(0);
  const [errorText, setErrorText] = useState("");
  const [installedExe, setInstalledExe] = useState("");
  const [copied, setCopied] = useState(false);
  const [speed, setSpeed] = useState(0);
  const speedRef = useRef({ t: 0, bytes: 0, smoothed: 0 });

  useEffect(() => {
    invoke<string>("get_default_install_path")
      .then(setInstallPath)
      .catch((err) => {
        console.error("Failed to get default install path:", err);
        setInstallPath("");
      });
  }, []);

  const percent = total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : 0;

  async function browseFolder() {
    try {
      const selected = await openDialog({
        directory: true,
        title: "Select installation folder",
        defaultPath: installPath || undefined,
      });
      if (typeof selected === "string") setInstallPath(selected);
    } catch {
    }
  }

  async function startInstall() {
    const path = installPath.trim();
    if (!path) return;

    setScreen("installing");
    setErrorText("");
    setDownloaded(0);
    setTotal(0);
    setSpeed(0);
    speedRef.current = { t: 0, bytes: 0, smoothed: 0 };

    const unlisten = await listen<DownloadProgress>("download-progress", (event) => {
      setDownloaded(event.payload.downloaded);
      if (event.payload.total > 0) setTotal(event.payload.total);

      const now = performance.now();
      const s = speedRef.current;
      if (s.t === 0) {
        s.t = now;
        s.bytes = event.payload.downloaded;
      } else if (now - s.t >= 400) {
        const inst = ((event.payload.downloaded - s.bytes) * 1000) / (now - s.t);
        s.smoothed = s.smoothed === 0 ? inst : s.smoothed * 0.65 + inst * 0.35;
        setSpeed(s.smoothed);
        s.t = now;
        s.bytes = event.payload.downloaded;
      }
    });

    try {
      setStatusText("Checking for the latest version...");
      const release = await invoke<LatestRelease>("get_latest_release");

      setStatusText(`Downloading CTRLServers ${release.version}...`);
      setTotal(release.size);
      const exePath = await invoke<string>("install_app", {
        downloadUrl: release.download_url,
        installDir: path,
        assetName: release.asset_name,
      });
      setInstalledExe(exePath);
      setDownloaded(release.size);
      setTotal(release.size);

      if (desktopShortcut) {
        setStatusText("Creating desktop shortcut...");
        await invoke("create_desktop_shortcut", { exePath });
      }
      if (autostart) {
        setStatusText("Adding CTRLServers to auto-start...");
        await invoke("set_autostart", { enable: true, exePath });
      }

      setStatusText("Finishing up...");
      setScreen("done");
    } catch (err) {
      setErrorText(errText(err));
      setScreen("error");
    } finally {
      unlisten();
    }
  }

  async function copyInstallPath() {
    if (!installedExe) return;
    try {
      await writeText(installedExe);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (err) {
      console.error("Failed to copy path:", err);
    }
  }

  function openInstallFolder() {
    if (!installedExe) return;
    void invoke("open_install_folder", { exePath: installedExe }).catch((err) =>
      console.error("Failed to open folder:", err),
    );
  }

  async function finish() {
    try {
      if (runAfter && installedExe) {
        await invoke("launch_app", { exePath: installedExe });
      }
      await invoke("exit_installer");
    } catch (err) {
      setErrorText(errText(err));
      setScreen("error");
    }
  }

  return (
    <div className="flex h-full flex-col bg-[#050505] text-white">
      <TitleBar />
      <div className="flex min-h-0 flex-1 flex-col px-10 pb-10 pt-6">
        <Brand />

        {screen === "welcome" && (
          <div className="flex min-h-0 flex-1 flex-col animate-[fade-in_0.3s_ease_both]">
            <p className="mt-6 text-sm leading-relaxed text-[#aaa]">
              Hello! You are installing CTRLServers. Before we will proceed to the installation, we
              will ask you some questions:
            </p>

            <div className="mt-7 space-y-4">
              <div>
                <label
                  htmlFor="install-path"
                  className="mb-2 block text-[13px] font-medium text-[#aaa]"
                >
                  1. Where we are gonna install the app?
                </label>
                <div className="flex items-center gap-2">
                  <input
                    id="install-path"
                    value={installPath}
                    onChange={(e) => setInstallPath(e.target.value)}
                    spellCheck={false}
                    placeholder="Installation folder..."
                    className="h-11 min-w-0 flex-1 rounded-[10px] border border-white/8 bg-white/6 px-4 text-sm text-white outline-none transition-colors placeholder:text-[#666] focus:border-white/40"
                  />
                  <button
                    type="button"
                    onClick={() => void browseFolder()}
                    title="Browse for folder"
                    className="flex h-11 w-11 shrink-0 items-center justify-center rounded-[10px] border border-white/8 bg-white/6 text-[#aaa] transition-colors hover:border-white/30 hover:text-white"
                  >
                    <svg
                      width="18"
                      height="18"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    >
                      <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
                    </svg>
                  </button>
                </div>
              </div>

              <ToggleRow
                label="2. Create a desktop shortcut?"
                checked={desktopShortcut}
                onChange={setDesktopShortcut}
              />
              <ToggleRow
                label="3. Should we put the app in Auto-start?"
                checked={autostart}
                onChange={setAutostart}
              />
            </div>

            <div className="mt-auto pt-8">
              <ContinueButton onClick={() => void startInstall()} disabled={!installPath.trim()} />
            </div>
          </div>
        )}

        {screen === "installing" && (
          <div className="flex min-h-0 flex-1 flex-col animate-[fade-in_0.3s_ease_both]">
            <h2 className="mt-10 text-lg font-semibold tracking-[-0.3px]">Installing CTRLServers</h2>
            <p className="mt-2 text-sm text-[#aaa]">{statusText}</p>

            <div className="mt-8">
              <div className="h-2 w-full overflow-hidden rounded-full bg-white/8">
                <div
                  className="h-full rounded-full bg-white transition-[width] duration-200 ease-out"
                  style={{ width: `${percent}%` }}
                />
              </div>
              <div className="mt-3 flex items-center justify-between text-[13px] text-[#aaa]">
                <span>
                  {formatBytes(downloaded)} / {formatBytes(total)}
                </span>
                <span className="font-medium text-white">{speed > 0 ? `${formatBytes(speed)}/s` : ""}</span>
                <span className="font-medium text-white">{percent}%</span>
              </div>
            </div>
          </div>
        )}

        {screen === "done" && (
          <div className="flex min-h-0 flex-1 flex-col animate-[fade-in_0.3s_ease_both]">
            <div className="mt-10 flex flex-col items-center text-center">
              <div className="flex h-16 w-16 items-center justify-center rounded-2xl bg-white/6">
                <svg
                  width="30"
                  height="30"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="#22c55e"
                  strokeWidth="2.2"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <path d="M20 6L9 17l-5-5" />
                </svg>
              </div>
              <h2 className="mt-5 text-lg font-semibold tracking-[-0.3px]">
                Thanks for installing CTRLServers!
              </h2>
              <p className="mt-2 text-sm text-[#aaa]">The app has been installed to</p>
              <div className="mt-1 flex w-full items-center justify-center gap-1.5">
                <p className="min-w-0 max-w-[calc(100%-84px)] truncate rounded-lg bg-white/6 px-3 py-1.5 font-mono text-xs text-[#aaa]">
                  {installedExe}
                </p>
                <button
                  type="button"
                  title={copied ? "Copied!" : "Copy path"}
                  onClick={() => void copyInstallPath()}
                  className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-white/8 bg-white/6 text-[#aaa] transition-colors hover:border-white/30 hover:text-white"
                >
                  {copied ? (
                    <svg
                      width="14"
                      height="14"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="#22c55e"
                      strokeWidth="2.4"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    >
                      <path d="M20 6L9 17l-5-5" />
                    </svg>
                  ) : (
                    <svg
                      width="14"
                      height="14"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    >
                      <rect x="9" y="9" width="13" height="13" rx="2" />
                      <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
                    </svg>
                  )}
                </button>
                <button
                  type="button"
                  title="Open install folder"
                  onClick={openInstallFolder}
                  className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-white/8 bg-white/6 text-[#aaa] transition-colors hover:border-white/30 hover:text-white"
                >
                  <svg
                    width="14"
                    height="14"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  >
                    <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
                  </svg>
                </button>
              </div>
            </div>

            <div className="mt-8">
              <ToggleRow label="Run CTRLServers" checked={runAfter} onChange={setRunAfter} />
            </div>

            <div className="mt-auto pt-8">
              <ContinueButton onClick={() => void finish()} />
            </div>
          </div>
        )}

        {screen === "error" && (
          <div className="flex min-h-0 flex-1 flex-col animate-[fade-in_0.3s_ease_both]">
            <h2 className="mt-10 text-lg font-semibold tracking-[-0.3px] text-[#ff4444]">
              Installation failed
            </h2>
            <div className="mt-4 rounded-[10px] border border-[#ff4444]/30 bg-[#ff4444]/10 px-4 py-3 text-[13px] leading-relaxed break-words text-[#ff4444]">
              {errorText}
            </div>

            <div className="mt-auto flex gap-3 pt-8">
              <button
                type="button"
                onClick={() => {
                  setErrorText("");
                  setScreen("welcome");
                }}
                className="h-11 flex-1 rounded-[10px] border border-white/8 bg-white/4 text-sm font-medium text-white transition-colors hover:border-white/25 hover:bg-white/8"
              >
                Back
              </button>
              <button
                type="button"
                onClick={() => void invoke("exit_installer")}
                className="h-11 flex-[2] rounded-[10px] bg-white text-sm font-medium text-[#0e0e0e] transition-all hover:-translate-y-px hover:bg-[#ddd] hover:shadow-[0_8px_32px_rgba(255,255,255,0.12)]"
              >
                Close
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
