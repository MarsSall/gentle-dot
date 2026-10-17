# Gentle Dot desktop (macOS, Linux, and Windows development)

The Tauri 2 shell around the Gentle Dot UI: a floating Dot, an expandable panel, a menu bar item, and a global shortcut. The contract between Rust and the UI is §9 of [`docs/design.md`](../../docs/design.md). Linux is covered [below](#linux).

## Requirements

- macOS, Xcode command line tools, Node 24, pnpm.
- Rust stable through rustup. If another `cargo` without `rustc` (for example from Nix) comes first on `PATH`, put rustup first:

  ```sh
  export PATH=/opt/homebrew/opt/rustup/bin:$PATH
  ```

- `src-tauri/.cargo/config.toml` pins the linker and C compiler to Apple's `/usr/bin/cc` for the macOS targets only (`CC_<target>`), because a Nix GCC `cc` on `PATH` cannot link against the macOS SDK. Linux builds use the system `cc`.

## Run

```sh
pnpm --filter @gentle-dot/desktop dev
```

This starts the UI dev server on `http://127.0.0.1:5173` and opens the Dot. Debug builds spawn the daemon with `GENTLE_DOT_ALLOWED_ORIGINS='["http://127.0.0.1:5173","http://localhost:5173"]'`, because the daemon rejects WebSocket origins outside its allowlist and `tauri dev` serves the UI from that origin. Release builds never set it; there the UI loads from `tauri://localhost`, which the daemon always allows. A daemon that was already running when the app started keeps its own allowlist.

## Build

```sh
pnpm --filter @gentle-dot/desktop tauri build --debug --bundles app   # debug .app
pnpm --filter @gentle-dot/desktop build                                # release
```

The bundle is written to `src-tauri/target/{debug,release}/bundle/macos/Gentle Dot.app`. Run the Rust unit tests with `cargo test` inside `src-tauri`.

## The daemon and `PATH`

On launch, the app checks `GET http://127.0.0.1:<port>/health`. If nothing answers, it spawns `<node> <repo>/packages/daemon/src/cli.ts` and writes its output to `~/.gentle-dot/daemon.log`.

Apps opened from Finder get a minimal `PATH`, but the daemon needs `gentle-shell`. So `build.rs` records three values at build time: the absolute `node` path, the daemon script path, and the build `PATH`. The spawned daemon runs with that `PATH`. Build from a shell where `node` and `gentle-shell` resolve, and rebuild if they move.

Runtime overrides:

| Variable | Effect |
|---|---|
| `GENTLE_DOT_NODE` | `node` binary used to start the daemon |
| `GENTLE_DOT_DAEMON_SCRIPT` | Daemon entry point |
| `GENTLE_DOT_DATA_DIR` | Data directory (default `~/.gentle-dot`) |
| `GENTLE_DOT_PORT` | Daemon port (default `4317`, otherwise `port` in `config.json`) |

`shortcut` in `~/.gentle-dot/config.json` changes the global shortcut (default `Alt+Space`). The Dot position is saved in `~/.gentle-dot/desktop.json` as `dot_points`, in macOS points (the screen space every monitor shares, whatever its scale factor). The `dot` key written by older builds was in physical pixels and is ignored, so the Dot starts once more at the right edge of the main display.

## Windows development (native validation pending)

The Windows port keeps the existing React UI, Rust/Tauri native host, and Node/TypeScript daemon.
It is source-development work toward the first native opening, **not a verified Windows release**.
Windows package source wiring is available below; no installer or installed-runtime execution has
been validated.

Startup prerequisites must already be available: Node 24, pnpm and the repository dependencies,
Rust for the Windows MSVC target, Microsoft C++ Build Tools with a Windows SDK, and WebView2.
If any prerequisite is missing, stop rather than assuming an installation is authorized.
From a native Windows development shell with these tools available, the existing source launcher is
`pnpm --filter @gentle-dot/desktop dev`; this command has not yet been validated on Windows.

`build.rs` selects `node.exe` for the Windows target and records the source daemon path and build
PATH. `GENTLE_DOT_NODE` and `GENTLE_DOT_DAEMON_SCRIPT` remain runtime overrides; keep this
checkout available when launching. The data directory still honors `GENTLE_DOT_DATA_DIR` first;
its default is `<HOME>/.gentle-dot`, falling back to `<USERPROFILE>/.gentle-dot` only on Windows
when HOME is absent or empty. Launching writes app/daemon state there; use only an explicitly
authorized development data directory for validation.

Windows-specific adaptations currently select app-managed window placement (not Linux compositor
rules) and existing colored non-template tray glyphs. Native panel transparency retains the original
setting; visual equivalence has not been verified. Voice model commands and connector-secret access
retain their original implementations, rather than being disabled to make the port compile. The
existing unsupported-platform recording behavior and ephemeral secret-store fallback are not complete
Windows equivalents; native voice and secure credential storage still need platform integration.
Computer control remains macOS-only in the existing implementation.
Native IPC, daemon startup/owned shutdown, dependency compatibility, graphical behavior, and
independent security checks still require Windows implementation or verification. sherpa-onnx remains
a dependency: its build script can download native libraries, so even an offline Cargo invocation must
not be treated as permission for a download. The paused synthetic probe is not a passed proof.

### Windows exit diagnostics

On exit, a failed checked Windows daemon stop writes
`gentle-dot: Windows shutdown was not confirmed: <reason>` to the desktop process's stderr.
The reason comes from that stop attempt, not an unrelated earlier startup refusal.
Owned child completion reports `gentle-dot: owned Windows shutdown confirmed (orderly)` or
`gentle-dot: owned Windows shutdown confirmed (exact-Job fallback)`; this is not confirmation
that the later parent-channel close succeeded. Diagnostics omit record nonces and capabilities.

Owned shutdown first attempts authenticated orderly close within the existing five-second grace.
If it cannot complete, the retained exact Job is the only forced-group fallback; a PID, process
name or recovered record never authorizes termination. Root exit and an empty Job must both be
observed, outstanding stop I/O must settle, and exact record retirement must succeed. Forced
exit does not require exit code zero. Automatic replacement also checked-closes the old parent
channel before discarding its owners. Pending parent-channel cancellation has a two-second wait
budget; timeout retains the handle/operations and refuses replacement, rather than claiming EOF.
Stop-connect result delivery settles the native open only: its inert pinned worker lifetime is
still retained, not claimed retired. Unconfirmed stop/close outcomes retain ownership for checked
retry. Restart propagates that attempt's failure without launching another daemon.

These messages do not change the desktop exit code, prevent desktop exit, or retry shutdown.

### Windows recovery after a crash, restart or power loss

Each launch also writes `daemon.owner` beside `daemon.pid`, after the record is created and before
the daemon runs. It names the desktop process that holds the launch's only Job handle and is
bound to the record by its nonce; the daemon and its native addon never read it, so the v1 record
format is unchanged. The Job uses `KILL_ON_JOB_CLOSE`, and Windows closes a process's handles
before marking it exited, so once that exact desktop process has exited the kernel has already
terminated every process in the Job.

On the next start, if the bound owner is proven gone (a matching process that exited, a reused
PID, or OpenProcess error 87 confirmed by a full process snapshot), Dot waits up to ten seconds
for the recorded daemon root, retires only that exact record, writes
`gentle-dot: previous Windows launch retired after its owning app exited` and starts normally.
Recovery never terminates a process. A witnessed launch that is not proven ended is refused with
`Windows previous launch admission refused: ...` and its record is kept: the owning app is still
running, the witness is invalid, unreadable or bound to another record, the owner's state cannot
be verified, or the recorded root is still running ten seconds after its owner exited. Dot does not
fall back to stopping the old daemon in those cases, because a clean root exit alone does not show
that a live owner's Job has no descendants left. Records written before this witness existed keep
the previous admission route and still need separately authorized manual recovery once their
daemon has exited.

Limits: processes started through brokers such as WMI `Win32_Process.Create`, Task Scheduler or COM
are not in the Job, as for forced shutdown. A terminating process may still complete I/O it had
already issued, but cannot run further code. Scoped native Windows shutdown tests and a baseline differential cover
this owned-launch path; they do not certify installer behavior or the full Windows port.
The nine integration checkpoints and earlier paused probes are not release certification.

## Windows packaging (source wiring; native validation pending)

With prerequisites already provisioned on **native Windows x64**, the new entry point consumes an
existing `build/runtime/windows-x64` (it never stages, installs dependencies, or launches the app):

```sh
pnpm package:windows
pnpm package:windows -- --runtime "C:\\path with spaces\\runtime"
pnpm package:windows -- --help
```

The runtime must already contain its `windows-x64` manifest, Node, daemon and dependencies, Windows
stop addon, Gentle AI directory, Engram, `pi.cmd`, and built UI. The checkout must already have the
installed Tauri CLI and `src-tauri/icons/icon.ico`. Missing artifacts or unsupported host/architecture
are rejected before build. This is an existence check, not a fresh signature/digest verification;
staging remains responsible for release pins and dependency/integrity checks.

The command invokes the installed Tauri JavaScript entry through Node without a `.cmd` spawn. Its
Windows-only overlay includes the runtime as the `runtime` resource and uses the staged UI, bypassing
the usual UI build hook. The current explicit build-plan default is **NSIS** (`--bundles nsis`), not a
verified distribution-format decision. WebView2 install mode is `skip`: it must already exist on the
target machine. Tauri/Rust/MSVC and any bundler tools must be provisioned separately; this command
is not evidence that a build can run offline. Installer artifacts, when built, use Tauri's usual
`src-tauri/target/release/bundle/nsis` output directory.

Only this entry point sets the internal compile-time `GENTLE_DOT_WINDOWS_PACKAGED_REQUIRED=1`
marker. Its Windows release binary validates runtime prerequisites before launching the daemon and
reports missing resources instead of silently using build-machine checkout paths. This is **not** a
runtime toggle. Normal checkout release/dev builds and Unix retain their existing fallback behavior;
Cargo tracks marker changes so subsequent ordinary builds do not retain package-required mode.

`GENTLE_DOT_RUNTIME_DIR` explicitly selects another complete runtime directory. The existing Node
and daemon-script overrides remain available: when the packaged runtime is absent, **both** nonempty
`GENTLE_DOT_NODE` and `GENTLE_DOT_DAEMON_SCRIPT` are required to intentionally use an external
runtime. A single override still works with a complete runtime, but does not authorize checkout
fallback if resources are missing. External runtime provisioning is the caller's responsibility.
No Windows packaging, installer execution, GUI launch, credentials, or native runtime proof is
claimed by these source changes and deterministic tests.

## Menu bar

| Item | What it does |
|---|---|
| Open (`⌥ Space`) | Shows the panel next to the Dot. The shortcut toggles it. |
| New conversation | Shows the panel and emits `dot://new-conversation` so the UI starts a new conversation. |
| Open in browser | Opens `http://127.0.0.1:<port>/#token=…` in the default browser. |
| Restart assistant | Restarts the daemon if the app started it. If it was started elsewhere, shows a dialog instead. |
| Launch at login | Turns the login item on or off (macOS LaunchAgent). This menu item is the source of truth; `launchAtLogin` in `config.json` is not read. |
| Quit | Stops the daemon the app started (SIGTERM), then quits. A daemon started elsewhere keeps running. |

The menu bar icon changes with the agent state that the UI reports through `set_dot_state` (ready, working, needs you, unavailable), and the tooltip names it.

## Icons

All icons come from the rose (`docs/brand/`). `scripts/make-icon.mjs` renders them with Playwright's Chromium:

- `src-tauri/icons/tray/<glyph>.png` and `<glyph>@2x.png` (18 and 36 px): the menu bar glyph `rose-glyph.svg` for `ready`, `working` (dashed outer petals), `needs-you` (badge dot), and `unavailable` (dimmed). They are template images, so macOS uses only their alpha and tints them for the menu bar. The app embeds the 36 px files, which macOS draws at 18 pt.
- `src-tauri/icons/source.png` (1024 px): the neon rose `rose-source.png` on a black macOS squircle with a faint pink rim.
- `packages/ui/public/favicon.svg` and `favicon.png`: the glyph in neon pink for the browser tab.

```sh
node scripts/make-icon.mjs
pnpm --filter @gentle-dot/desktop tauri icon src-tauri/icons/source.png -o /tmp/gentle-dot-icons
cp /tmp/gentle-dot-icons/{32x32,64x64,128x128,128x128@2x}.png /tmp/gentle-dot-icons/icon.{icns,png} src-tauri/icons/
```

`node scripts/make-icon.mjs --linux-tray` renders only `src-tauri/icons/tray/linux/<glyph>.png` (32 px): the same four states for Linux trays, which do not tint icons and can be light or dark, so the glyph is light gray over a dark outline, with an amber badge for `needs-you`.

`tauri icon` also writes Android, iOS, and Windows icons, which this macOS app does not use, so they go to a scratch directory and only the files `tauri.conf.json` lists (plus `64x64.png`) are copied.

## Linux

Debian and Ubuntu (GNOME) and Omarchy (Arch with Hyprland) are built from source on the user's machine, because the app records the build machine's `node` and daemon paths (see above). The setup scripts install the system libraries (the only step that uses sudo, after asking), Node 24 under `~/.local/share/gentle-dot` when no Node 24+ is present, pnpm through corepack, and Rust through rustup, then build with `tauri build --no-bundle` and install `~/.local/bin/gentle-dot` and a menu entry:

```sh
scripts/linux/setup-debian.sh    # Debian, Ubuntu (apt)
scripts/linux/setup-arch.sh      # Omarchy, Arch (pacman)
scripts/linux/uninstall.sh       # removes the launcher, menu entry, and private Node; --purge also ~/.gentle-dot
```

The binary is `src-tauri/target/release/gentle-dot`. Tester checklist: [`docs/linux-testing.md`](../../docs/linux-testing.md).

What differs from macOS (decided in `src/platform.rs`, unit-tested on both systems):

| Topic | macOS | GNOME on Wayland (Debian, Ubuntu) | Hyprland (Omarchy) |
|---|---|---|---|
| Display backend | — | XWayland: the app sets `GDK_BACKEND=x11` before GTK starts, when the session is Wayland, the desktop is not Hyprland, `DISPLAY` is set, and the user did not choose a `GDK_BACKEND` | Native Wayland |
| Dot placement, snapping, panel next to the Dot | The app | The app | Hyprland window rules, matched by title (`Gentle Dot`, `Gentle Dot Panel`): [`scripts/linux/hyprland/`](../../scripts/linux/hyprland/) has a `hyprland.lua` form (Omarchy 4) and a `hyprland.conf` form |
| Global shortcut | `Alt+Space` from the app | A desktop custom shortcut running `gentle-dot --toggle` (XWayland only sees keys while an X11 window has focus, and `Alt+Space` is GNOME's window menu) | A Hyprland `bind` running `gentle-dot --toggle` (the app does not register one) |
| Panel background | `HudWindow` vibrancy | Opaque: the panel URL carries `effects=none` | Same as GNOME |
| Tray glyphs | Template images | `icons/tray/linux/` (needs the AppIndicator GNOME extension; Ubuntu ships it on) | Same, shown by Waybar |

`gentle-dot --toggle` reaches the running app through `tauri-plugin-single-instance` (D-Bus on Linux), which toggles the panel; any other second launch shows it. Started with `--toggle` while not running, the app opens the panel once it is up.

macOS-only calls (`accept_first_mouse`, the `HudWindow` effect, the accessory activation policy) are behind `cfg(target_os = "macos")`; the template flag follows `status::is_template`. The `macos-private-api` feature and `macOSPrivateApi: true` stay on every platform: tauri-build checks the `tauri` features in `Cargo.toml` against the merged `tauri.conf.json`, every `tauri` dependency entry including target-specific ones, so splitting them per target fails the build. They only change macOS code; the Linux build in a Debian container compiles with them. `Info.plist` (`LSUIElement`) is read only by the macOS bundle.

