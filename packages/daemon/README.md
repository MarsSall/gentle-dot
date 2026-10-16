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
