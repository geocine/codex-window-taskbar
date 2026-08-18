# Codex Windows Taskbar

Native Windows taskbar widget for Codex usage.

![Codex taskbar widget showing the 7d window at 52% remaining, resetting in 2 days](docs/taskbar-widget.svg)

## Download

**[Download codex-windows-taskbar.exe](https://github.com/geocine/codex-window-taskbar/releases/latest/download/codex-windows-taskbar.exe)** ([all releases](https://github.com/geocine/codex-window-taskbar/releases))

No installer — run the exe and the widget appears on your taskbar.

## Features

- Remaining usage rows from Codex (`5h` and/or `7d`; single window is centered)
- 10 blue segments per row
- Reset countdowns
- Tray icon tooltip
- Right-click menu for refresh, poll interval, startup, visibility, and exit
- Diagnostics mode

## Build

```powershell
cargo build --release
```

Output:

```text
target\release\codex-windows-taskbar.exe
```

## Run

```powershell
.\target\release\codex-windows-taskbar.exe
```

Diagnostics:

```powershell
.\target\release\codex-windows-taskbar.exe --diagnose
```

Log file:

```text
%TEMP%\codex-windows-taskbar.log
```

Settings:

```text
%APPDATA%\CodexWindowsTaskbar\settings.json
```

## Requirements

- Windows 10 or Windows 11
- Codex installed and authenticated

The app reads Codex credentials from `~/.codex/auth.json`, including supported WSL credential locations.

## Privacy

The app calls ChatGPT/OpenAI usage endpoints using local Codex credentials. It does not send credentials to a separate backend, collect analytics, or upload project files.

## License

MIT. Copyright (c) 2026 geocine.
