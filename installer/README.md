# YAL ClearVoice installer

A single per-machine, elevated, x64 WiX MSI that installs the app, the audio
service, the engine + model, and the virtual-audio driver (one driver, both
endpoints). No runtime prerequisites: the engine links the CRT statically and the
UI targets .NET Framework 4.8, which is part of Windows, so there's no VC++ redist or
.NET runtime to chase.

> **Status: source only.** The MSI can't be built or tested in the engine's
> sandbox (no WiX toolset, no clean VM, and it needs already-built, signed
> binaries). This is the authoring plus the build recipe to run on Windows.

## Files

| File | Role |
|---|---|
| `Package.wxs` | The MSI: files, service, registry, shortcut, driver custom actions, upgrade. |
| `yalcv-devnode/devnode.cpp` | Tiny SetupAPI helper: create the root devnode + bind the driver, or remove it. |
| `build.ps1` | Stages the binaries and runs `wix build` + signing. |

## Build inputs (must exist and be signed first)

- `clearvoice.exe` — engine (`cargo build --release`, static CRT).
- `YalClearVoice.exe` — UI (`../ui/build.ps1`; .NET Framework 4.8).
- `yalcv-supervisor.exe` — the audio service. **Pending** — not yet built; the MSI
  references it, so build it before packaging or the service won't be included.
- `yalcv-devnode.exe` — built from `yalcv-devnode/devnode.cpp` by `build.ps1`.
- Signed `yalcv.sys`, `yalcv.inf`, `yalcv.cat` — see `../driver` (attestation for
  pilot, WHQL for production). The MSI ships them already signed.

## Prerequisites on the build machine

- WiX v4+ CLI: `dotnet tool install --global wix`
- .NET SDK (only for the WiX CLI), Visual Studio Build Tools (`cl.exe`, `csc.exe`), the
  Rust MSVC toolchain, and a Developer PowerShell.
- A code-signing cert for the staged exes and the MSI.

## Build

```powershell
.\build.ps1 -DriverDir C:\path\to\signed-driver -Version 0.1.0
```

## What the MSI does

- **Install:** lay files to `%ProgramFiles%\YAL ClearVoice\` → create the devnode and
  bind the driver (helper, deferred, as SYSTEM) → register + start the service →
  write HKLM config → add the per-user autostart and a Start Menu shortcut.
- **Upgrade:** installs the new version, then removes the old; the devnode is
  **preserved** (the remove action is skipped while `UPGRADINGPRODUCTCODE` is set),
  so the endpoint IDs — and each agent's saved Genesys device — survive. The driver
  binds in place (the helper is idempotent).
- **Uninstall:** stop + remove the service, remove the devnode, remove files,
  registry, shortcut.
- **Rollback:** a failed driver install triggers the rollback action that removes a
  freshly created devnode; MSI rolls back files, service, and registry itself.
- **Error handling:** launch conditions (64-bit Windows, elevation) block a bad
  target up front; the driver custom action returns non-zero on failure, which
  fails the transaction and triggers rollback.

## Test on a clean VM (can't be done here)

1. Clean Windows 10 22H2 / 11 x64 VM, driver trusted (production signing, or
   test-signing for a pilot build).
2. `msiexec /i YalClearVoice-0.1.0.msi /l*v install.log` — confirm: service running,
   "YAL ClearVoice Microphone" + "YAL ClearVoice Speaker" in Sound settings, UI
   starts at logon, shortcut present.
3. Upgrade: install a higher `-Version` build; confirm the endpoints keep their IDs
   (device instance unchanged in Device Manager).
4. `msiexec /x YalClearVoice-0.1.0.msi /l*v uninstall.log` — confirm service, devnode,
   files, and registry are gone.
5. Force a mid-install failure (e.g. an unsigned driver) and confirm rollback leaves
   nothing behind.

## Known limitations (deliberately minimal)

- The DriverStore package may linger after uninstall (the devnode is removed; the
  staged `.inf` is inert). Add `pnputil /delete-driver <oemNN.inf>` if full cleanup
  is required.
- The pipe/driver security use default ACLs; harden with explicit SDDL before shipping.
- Windows 10 1809+ is enforced by the driver package, not an MSI launch condition
  (MSI can't read the build number reliably); document it as a deployment gate.
