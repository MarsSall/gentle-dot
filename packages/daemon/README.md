# Gentle Dot daemon

## Windows connector secret storage

The desktop app serves Windows connector secrets through the existing private app
channel using current-user DPAPI. Ciphertext lives under the Windows Local AppData
known folder, in the fixed `dev.gentleman.gentle-dot.connectors` namespace. Storage
requires a local fixed NTFS volume and privately owned, protected directories/files;
unsafe existing permissions or reparse points are rejected, not repaired. Unix
Keychain and Secret Service behavior is unchanged.

This namespace is user-wide, not isolated by `GENTLE_DOT_DATA_DIR`. A temporary
GUI profile therefore does not isolate credentials. Other processes running as the
same user remain trusted. There is no plaintext/memory fallback or automatic
migration from the former Windows in-memory store. Existing signed connector state
whose old in-memory integrity key was lost is not silently trusted or re-signed.

Writes flush an encrypted temporary file before one native replacement request.
Failures before publication preserve the previous value. An uncertain publication
result is reported without automatic retry or deletion; there is no power-loss
or unconditional rollback guarantee. Retained temporary files are not swept.
DPAPI's native length limits still apply; no Credential Manager-sized cap is imposed.

Native storage tests are ignored by default and use fresh synthetic namespaces.
Run only explicitly selected tests, never a blanket ignored-test invocation. They
may retain their directories, lock files, and unsafe fixtures; no production
namespace cleanup is implied. Isolated Windows tests have passed for cross-process
lock contention, hardlink rejection, root-symlink rejection, and a submitted rename
failure that retains its pending ciphertext. These cases do not establish universal
rollback, crash durability, every reparse location, or protection from same-user processes.

## Production Windows stop addon (S8, S10, S11)

Windows startup requires `native/windows-stop.node` and a private launch record at
`<dataDir>/daemon.pid`. There is no implicit unmanaged Windows bypass. Unix startup,
signals and app-channel EOF retain their existing behavior and do not load this addon.

The Rust launcher must write the L98 record before resuming Node (T1c):

```text
gentle-dot-stop-v1
<pid>
<creation-FILETIME-decimal>
<32-lowercase-hex-launch-nonce>
<64-lowercase-hex-capability>
```

The record is bounded to 256 bytes. Native code validates current-process identity
and private file provenance read-only; TypeScript does not repair or independently
accept the record. Do not put its capability in command arguments, environment,
logs or public HTTP endpoints. The native pipe is derived from the launch nonce.
This minimal stop transport does not replace the dedicated privileged app channel
or establish descendant exclusion for that channel.

The listener is constructed and validates the exact-launch record before fd3
adoption. Every currently supported Rust Windows launch requires the dedicated
app channel; successful record validation selects that existing managed route,
not proof that fd3 is present and not privileged-channel authentication. The CLI
then explicitly constructs `WindowsAppChannel` on the main startup thread before
`startDaemon`, `privateMemory`, early PowerShell discovery, engine startup or MCP
server startup can create descendants. Missing/invalid fd3 or adoption failure
refuses startup and independently drains the stop listener; there is no fallback
to a running channel-less managed daemon. Direct channel-less `startDaemon` remains
unchanged; unmanaged Windows CLI startup remains refused as above.

Production construction binds actual main-thread/Node IPC facts and asserts an
exclusive fd3 startup reservation. Production construction rejects injected native
loaders; only the default artifact path followed by successful native one-shot
adoption/status validation admits a Windows app channel into daemon startup.
The portable injected-owner factory never grants production admission. Native ownership
validation makes the original and duplicate handles non-inheritable and closes
the host CRT fd3; JS exposes no descriptor or handle aliases. This is the production
exclusion mechanism under the supported Node 24.14.0 spawn behavior, **not runtime
child-object inspection or observed Windows exclusion proof**. The existing Unix
inherited-socket acquisition and child checks remain unchanged, including fd3
stdio exclusion. Neither the stop record, a pipe name nor fd3 absence authenticates
the app: the inherited connection remains its sole authority.

The stop callback must deliver exactly 36 bytes, `GDS1` plus the 32-byte capability.
TypeScript rejects other lengths/versions and compares the capability with
`timingSafeEqual`. A valid stop during asynchronous startup is latched. Synchronous
factory delivery is retained until capability assignment and then authenticated:
at most four correctly shaped frames (144 bytes); overflow fails initialization
closed, drains the listener and does not start the daemon. This bounded queue is
for the injected synchronous factory seam, not a substitute for native prefiltering
and authenticated pending-stop delivery.

Requests, signals and daemon closure share one memoized shutdown. App-channel
logical closure immediately refuses approvals, rejects pending requests and
invalidates connector secrets before awaiting transport drain. Daemon shutdown
attempts its other cleanup independently and awaits Windows channel close; it
publishes its close promise before close-event notification to avoid recursive
shutdown. Unexpected native error/EOF logically closes the app link and requests
daemon shutdown without waiting for native disposal first.

Channel cleanup and stop-listener drain are attempted even after an earlier
failure. Startup failure after adoption aborts/closes the adopted channel before
returning and independently drains the listener. Failed cleanup owners are
retained; secondary failures never replace the original startup/daemon error.
Exit 0 requires real daemon close, app-channel close and stop-listener drain
success; `daemon.closed` settlement alone is not success. Both app-channel drain
boundaries require the resolved result to be `{ mode: "drain", drained: true }`.
Adapter close is first-call-wins: a prior successful abort still rejects later
managed drain, rather than certifying delivery or exit 0. Startup-failure abort
cleanup deliberately does not require a drain result. Rejected or pending native
drain never reports success. On failure, owners remain explicitly retained and the
Windows CLI sets `process.exitCode = 1` without forcing `process.exit(1)` or claiming
descendant completion. This does not add a keepalive, retry, forced termination or
record deletion mechanism; the process can exit naturally when its event loop ends.
Native construction errors are reported with a fixed message, without record data.

The native-rooted callback holds only a weak association to the request handler,
created in a separate lexical scope. The returned owner strongly retains that
handler while it is owned, so normal ownership does not lose delivery. The weak
association breaks the native callback → handler → listener cycle; injected weak
reference checks are not evidence of actual GC/finalizer scheduling.

### Explicit build and staging

No install, download, node-gyp or third-party dependency is involved. On Windows,
use an **existing** MSVC/Windows SDK developer environment with `INCLUDE` and `LIB`
configured for the same architecture as Node. Supply the existing matching Node
headers and architecture/version-compatible `node.lib`, and the compiler path:

```powershell
# From packages/daemon; substitute your existing absolute input paths.
node scripts/build-windows-stop.mjs --help
node scripts/build-windows-stop.mjs --compiler "C:\existing\cl.exe" --headers "C:\existing\node\include\node" --node-lib "C:\existing\node.lib"
node scripts/build-runtime.ts --out "C:\existing\runtime-output"
```

The package command is `pnpm run build:windows-stop -- <same arguments>`.
The compiler emits `native/windows-stop.node` and local compiler/linker artifacts.
It must not be cross-built with mismatched Node/import-library architecture.
Windows `build:runtime` refuses a missing addon before bundling and stages it as
`daemon/native/windows-stop.node`, next to `daemon/cli.mjs`. Source execution loads
`packages/daemon/native/windows-stop.node`; staged execution loads the staged copy.
Unix runtime layout remains unchanged. Neither build command installs dependencies.

### Current verification boundary

The Windows app-channel foundation, backend and Node-API adapter have compiled;
addon exports were load-checked, but app-channel adoption and I/O were **not
invoked** in that check. The portable TypeScript adapter tests and this source
integration exercise ordering, fail-closed admission, framing/approval refusal,
close/drain arbitration and EOF-driven lifecycle using injected participants.
They are **not Windows native proof**, do not mutate actual process IPC facts,
and do not inspect actual Windows descendants.

Functional verification of production fd3 adoption, native I/O and descendant
exclusion remains pending, as does end-to-end managed CLI/app shutdown. Native
failure retention, queue pressure, GC/Worker teardown and environment cleanup are
not established by portable orchestration tests. Neither compiled source nor this
integration means the daemon is fully Windows-ready. No paused probe or replacement
harness is introduced. A rejected or pending native drain cannot report successful
shutdown; timeout is not proof that kernel ownership has completed.
