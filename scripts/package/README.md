# Runtime staging

`stage-runtime.mjs` prepares the runtime consumed by the platform packager. It is not a cross-builder: native dependencies must be installed on the target platform. Existing macOS and Linux targets are unchanged; Windows currently supports **native x64 only**.

## Prerequisites

- Install repository dependencies beforehand using the project's supported Node and pnpm versions. Staging does not bootstrap pnpm or the native addon toolchain.
- On Windows, provide the existing daemon addon builder's prerequisites explicitly (including its native build tools). Run staging through pnpm, so `npm_execpath` identifies pnpm's JavaScript entry point; this avoids spawning a `.cmd` file or constructing a command-shell string.
- Windows also requires an already provisioned Go **1.25.10 or newer** for gentle-pi 4.0.0's Gentle AI source build. Set `GENTLE_DOT_BUILD_GO` to the absolute existing `go.exe` (for example `C:\Program Files\Go\bin\go.exe`). Staging refuses a missing path before replacing output or downloading anything; it does not install Go. The dependency installer checks the Go version.
- System `tar` must support the selected archive: on Windows this includes the official Node ZIP and `--strip-components=1` (the Windows bsdtar implementation). Availability and extraction behavior still need native verification.
- Network access is required for checksum-verified Node and Engram release downloads and production npm dependencies, including gentle-pi's existing postinstall. These operations require separate authorization; deterministic helper tests do not perform them.

When authorized, the staging entry is:

```text
pnpm package:runtime -- --target windows-x64
```

Optional flags: `--out <dir>` and `--engram-version <x.y.z>`. See the root package scripts for platform packaging commands.

## Effects and layout

Staging writes `.cache/package`, builds the daemon and UI with the existing build commands, installs production daemon dependencies, and writes `build/runtime/<target>` by default. A previous output bearing a `gentle-dot-runtime` manifest is replaced; any other nonempty output is refused. Build commands can also write their normal build outputs. Staging is not an installer and does not open the app.

On Windows, the official Node ZIP's `node.exe` is moved to `runtime/node/bin/node.exe`, and its npm tree to `runtime/node/lib/node_modules/npm`. Relocatable `npm.cmd` and `npx.cmd` wrappers are provided, but staging invokes npm through Node and `npm-cli.js`, not by spawning those wrappers. Engram is staged as `runtime/bin/engram.exe`. The existing daemon build owns native addon staging; gentle-pi's postinstall integrity reporting remains in place.

Installer subprocesses use a scratch HOME and, on Windows, scratch USERPROFILE, LOCALAPPDATA and APPDATA. Windows PATH is semicolon-separated and contains staged Node, Windows system directories and only the explicitly selected Go executable's directory, never the full user PATH; SystemRoot, ComSpec and temporary paths are provided without inheriting user credentials or mutating the parent environment. Unix installer environment rules are unchanged.

Windows Gentle AI postinstall uses a Go checksum-database source build, not a signed Windows release archive. Its report names the source package/tag, module checksum, resulting binary SHA-256 and Go version separately; a binary hash is not an archive signature. Unix release-asset reporting is unchanged.

A Windows source snapshot without a `.git` entry records `daemonCommit: "snapshot-unversioned"` in the runtime manifest. This explicitly states that Git provenance is unavailable; it is not a commit or content-integrity guarantee. Normal Git checkouts, including Unix, retain commit and dirty-state reporting.

## Windows engine/codemode compatibility

The daemon lockfile selects gentle-pi **4.0.0**, coding-agent **1.0.4**, pi-mcp **1.0.4**, and the engine's pi-codemode **1.0.4** edge. The installed engine declares codemode as `^1.0.4`; a fresh flat npm install can therefore select 1.2.0, which was observed to lack `MCP_TYPESCRIPT_PREAMBLE`. Windows runtime builds now add the lockfile-selected codemode as an exact dependency and npm override. Other dependencies and Unix runtime manifests/install behavior are unchanged; this is not a full transitive npm lock.

After the existing dependency install, Windows staging uses the staged Node to resolve codemode from the engine's ESM import context, verifies the installed versions against the runtime manifest, and imports the five declarations used by coding-agent 1.0.4, including `MCP_TYPESCRIPT_PREAMBLE`. Each subprocess has a ten-second bound and an empty environment. The check loads only declarations, not the engine, shell, agent or installer hooks. A mismatch or missing export stops staging before Engram/UI staging and before `MANIFEST.json` is published. The engine, MCP, gentle-pi and codemode versions are recorded in a successful staging manifest.

Local tests exercised the actual installed 1.0.4 declarations via an isolated scratch manifest and links to existing pnpm modules, plus synthetic missing-export and nested-resolution failures. This proves the specific import seam, not a fresh flat npm installation, Windows-native behavior or full engine startup. No 1.2.0 package was fetched. Restage explicitly when separately authorized: the Windows packager consumes existing runtimes and does **not** re-run this check, so an older staged runtime is not retroactively validated. Secret readback and the effects of the earlier provisioning attempt remain unresolved.

## Windows packaging frontend path

Windows packaging consumes an already staged runtime; it does not stage or install one. The generated Tauri overlay spells `build.frontendDist` as a `./`-prefixed directory path relative to `apps/desktop/src-tauri` (the Tauri config directory), not the desktop CLI working directory. Paths with spaces remain JSON arguments; the runtime resource mapping is unchanged.

An absolute Windows drive path can deserialize as a URL such as `c:`, not an embedded directory, and fail when Tauri joins the panel page. A `file:` URL is not a substitute: it does not embed the frontend. Keep the runtime on the same volume/share as the checkout; packaging refuses paths that cannot be expressed relative to `src-tauri` before invoking a build. This policy fixes the packaging input only; native build and installed-app startup still require separate verification.

## Unverified Windows release seam

No existing local evidence confirms Engram's Windows archive format or member layout. Staging therefore requires the release checksum list to identify exactly one `engram_<version>_windows_amd64.zip` or `.tar.gz` asset, verifies its checksum, and requires a regular top-level `engram.exe`. Missing or ambiguous evidence fails explicitly rather than guessing the format. These supported naming/layout candidates are not proof that a release supplies them.

Native Windows staging, addon compilation, postinstall execution, extracted asset layouts and installed-app independence still require separately authorized verification. Local helper tests prove only target gating and archive/path/environment/invocation plans; they do not establish those native results.
