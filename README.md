# CTRLServers Installer

Desktop installer for CTRLServers, built with Tauri 2, React, TypeScript and Tailwind CSS.

![How it looks](howitlooks.png)

## Features

- Always installs the latest release from GitHub (version is never hardcoded)
- Fast parallel download using 8 connections
- Installs per user to `%LOCALAPPDATA%\Programs\CTRLServers` — no admin rights needed
- Optional desktop shortcut and auto-start entry
- Copy the install path or open the install folder from the summary screen
- Lightweight NSIS installer (~2.7 MB)

## Development

```bash
npm install
npm run tauri dev
```

## Build

```bash
npm run tauri build
```

The installer is written to `src-tauri/target/release/bundle/nsis/`.
