# Security

FerroType types into other programs and can keep what you copy, so security problems in it
matter. Thank you for taking the time to report one.

## Reporting a vulnerability

Please report privately through GitHub: open the repository's **Security** tab and choose
**Report a vulnerability**. Don't open a public issue for anything exploitable.

Include the FerroType version (Settings → About), your Windows version, and steps or a proof of
concept. You'll get an acknowledgement, and a fix or a decision will be published as a GitHub
security advisory. Credit is given unless you'd rather not be named.

## Supported versions

Only the latest release gets security fixes.

## What FerroType does and doesn't do

- **Types only what you give it, only when you start it.** Typing starts from the Start button,
  a history entry's play button or your start hotkey, never on its own.
- **Uses Windows' intended APIs, no hooks.** Keystrokes go through `SendInput`; hotkeys are
  registered with `RegisterHotKey`; copies are noticed with `AddClipboardFormatListener`. No
  keyboard or mouse hook is installed, nothing is injected into other processes, and keys you
  type yourself are never seen.
- **Clipboard: text only, private copies skipped.** Only `CF_UNICODETEXT` is read. By default
  nothing at all is read from a copy carrying the marks password managers use for passwords
  (`ExcludeClipboardContentFromMonitorProcessing` or `Clipboard Viewer Ignore`); a setting turns
  that off. `CanIncludeInClipboardHistory` = 0 alone isn't treated as private: Remote Desktop and
  Hyper-V's VM Connect put it on every copy out of a session. **From clipboard** reads whatever
  text is there and adds it to the history, since you asked for it. Copied files and images are
  never touched. Every read is size-limited and bounds-checked.
- **Saved data is encrypted.** The history file is encrypted with DPAPI for the current Windows
  user, with FerroType's own entropy, and written atomically. In the default (strict) mode only
  pinned entries are saved. An unreadable file is set aside, never parsed as something else; the
  parser rejects malformed input without panicking (tested against every truncation and
  single-byte change).
- **Optional security-key lock.** With a FIDO2 security key registered, the saved history is
  also encrypted with a random 256-bit data key (AES-256-GCM, the file's header as associated
  data, a fresh nonce on every save). The data key is stored once per registered key, wrapped
  with AES-256-GCM under HKDF-SHA256 of that key's `hmac-secret` output for the vault's random
  salt, bound to the credential id. Keys are reached only through Windows' WebAuthn API
  (`webauthn.dll`), which shows its own PIN and touch prompts, so FerroType never sees the PIN; user
  verification is required, and credentials aren't discoverable (nothing is stored on the key).
  Key material is wiped from memory when dropped. It protects the file at rest; while FerroType
  is running and unlocked, the entries are in its memory as usual. Lose every registered key and
  the locked entries can't be recovered.
- **One FerroType at a time.** A named mutex in the session (`Local\`) stops a second copy; it
  asks the running one to show its window and exits. Two copies would otherwise each save the
  history file over the other's. Separately, a FerroType that hasn't unlocked a security-key
  locked file never overwrites or removes it.
- **Installing stays within your account.** Install copies the exe to
  `%LOCALAPPDATA%\Programs\FerroType` and adds a Start menu shortcut and an entry under
  `HKEY_CURRENT_USER\…\Uninstall`; nothing machine-wide. When the installed copy uninstalls
  itself, a temporary copy in `%TEMP%` deletes it after it exits, then is deleted by Windows
  (it's opened `FILE_FLAG_DELETE_ON_CLOSE`).
- **No network access, no telemetry, no update checks.**
- **No administrator rights.** The manifest asks for `asInvoker`.
- **Hardened build.** Control Flow Guard, CET shadow-stack compatibility, ASLR/DEP, imports
  from System32 only, a hardened heap, and at startup: System32-only DLL search, no remote or
  low-integrity images, no legacy injection points. Releases are built by CI from the tagged
  commit, reproducibly (the same toolchain gives the same exe), with a signed build-provenance
  attestation.
