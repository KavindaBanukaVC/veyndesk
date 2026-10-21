# Veyndesk

**Open-source remote desktop for Windows, macOS and Linux, like AnyDesk.** Open the app, share your ID and password,
and your partner can see and control your screen. It connects directly when the network allows it and falls back to a
relay when it doesn't, so there is no server or port forwarding to set up.

## Download

Get the latest version from **[Releases](../../releases/latest)** · Website and guides: **[veyndesk.github.io](https://veyndesk.github.io)**

| Platform | File |
|---|---|
| **Windows 10 / 11** | `Veyndesk_x.y.z_x64-setup.exe` (installer) or `Veyndesk-windows-portable.zip` |
| **macOS 10.15+** (Apple Silicon and Intel) | `Veyndesk_x.y.z_universal.dmg` |
| **Linux** (Ubuntu, Debian, Mint…) | `veyndesk_x.y.z_amd64.deb` |
| **Linux** (any distribution) | `Veyndesk_x.y.z_amd64.AppImage` |

**macOS first run:** the app isn't notarized yet, so right-click Veyndesk → **Open** → **Open**. Then allow it in
System Settings → Privacy & Security → **Screen Recording** and **Accessibility**, so others can see and control your Mac.

**Linux:** works on X11 sessions (on Ubuntu, choose "Ubuntu on Xorg" at the login screen). Wayland support is planned.

## How to use

1. Open Veyndesk on both computers.
2. Tell your partner **your ID and password** (shown under *This desk*).
3. They type your ID under *Remote desk* → **Connect** → enter the password.
4. You click **Accept**. Done.

For unattended access, set a permanent password in **Settings**. Sessions that use it skip the Accept prompt.

## Features

- **Direct connection (P2P):** connects PC-to-PC using WebRTC.
- **Automatic relay:** used when the network blocks direct connections.
- **Encryption:** direct (P2P) connections are end-to-end encrypted (DTLS). Relay connections use HTTPS through
  Cloudflare, so they are encrypted in transit but **not** end-to-end. Connections by IP address on a local network are
  not encrypted yet. The password is never sent; a challenge–response check proves it. Details: [Security](https://veyndesk.github.io/security/).
- **Accept / Deny prompt:** asks before anyone sees your screen with the one-time password. If you set a permanent
  password for unattended access, sessions using it start **without** a prompt. Wrong-password lockout.
- **Screen sharing:** adaptive quality, sharp text, multi-monitor.
- **Input and clipboard:** mouse, keyboard and clipboard sync.
- **Optional self-hosted server:** web admin panel, audit log, browser access.

## How it works

| Part | Purpose |
|---|---|
| `crates/host` | Screen capture → changed-tile JPEG encoding, input injection, P2P (WebRTC) |
| `crates/server` | Small built-in server (runs inside every app; can also run alone) |
| `crates/protocol` | Wire protocol and password challenge–response |
| `desktop/` | The desktop app for Windows, macOS and Linux (Tauri) |
| `web/` | Viewer and admin pages |

## Build from source

```bash
cargo test --workspace
cd desktop && npx @tauri-apps/cli@2 build    # → installer in desktop/src-tauri/target/release/bundle/nsis
```
Releases are built automatically by GitHub Actions (`.github/workflows/build.yml`) when a `v*` tag is pushed.

## Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org).

- **Committers and reviewers:** [KavindaBanukaVC](https://github.com/KavindaBanukaVC)
- **Approvers:** [KavindaBanukaVC](https://github.com/KavindaBanukaVC)

Only binaries built by this repository's GitHub Actions workflow from its source code are signed.
Every release is approved manually before signing.

## Privacy policy

Veyndesk does not collect analytics or personal data, and has no accounts. To connect two computers,
it uses these services:

| Service | Used for | What is sent |
|---|---|---|
| **ntfy.sh** (public message service) | Finding your partner by ID | A one-time connection offer (network addresses, encryption fingerprint) on a topic derived from the ID |
| **Google / Cloudflare STUN servers** | Opening a direct connection | Standard STUN requests (your public IP address) |
| **Cloudflare quick tunnel** (`cloudflared`, downloaded on first start) | Relay when a direct connection is impossible | Session traffic of relayed sessions only (HTTPS to Cloudflare, not end-to-end) |

Screen content and keystrokes go only to the partner you connected with. Settings (your ID, password hash and
options) are stored only on your computer:

| System | Settings folder |
|---|---|
| Windows | `%APPDATA%\Veyndesk` |
| macOS | `~/Library/Application Support/Veyndesk` |
| Linux | `~/.config/Veyndesk` |

**Uninstall:** Windows: Settings → Apps → Veyndesk → Uninstall. macOS: drag Veyndesk from Applications to the Trash.
Linux: `sudo apt remove veyndesk` (or delete the AppImage). To also remove your ID and settings, delete the folder above.

## License

[GNU AGPL v3.0](LICENSE). Some ideas are inspired by [RustDesk](https://github.com/rustdesk/rustdesk) (AGPL-3.0).
