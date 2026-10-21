# Builds YalClearVoice.msi. Run from a Developer PowerShell (for cl.exe) on Windows
# with: the WiX v4+ CLI (`dotnet tool install --global wix`), Visual Studio Build
# Tools + the Rust MSVC toolchain, and the SIGNED driver files. Nothing here is built or signed
# in the engine's sandbox - this is the recipe to run on a real build machine.
#
#   .\build.ps1 -DriverDir C:\signed-driver -Version 0.1.0
param(
  [Parameter(Mandatory = $true)][string]$DriverDir,   # signed yalcv.sys/.inf/.cat
  [string]$Version = "0.1.0",
  [string]$Stage = "$PSScriptRoot\stage"
)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent   # the clearvoice crate root

New-Item -ItemType Directory -Force -Path $Stage | Out-Null

# 1. Engine (static CRT, so the MSI needs no VC++ redistributable).
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --release --manifest-path "$root\Cargo.toml" --bin clearvoice
Copy-Item "$root\target\release\clearvoice.exe" $Stage -Force
Copy-Item "$root\models\DeepFilterNet3_onnx.tar.gz" $Stage -Force

# 2. UI (.NET Framework 4.8, part of Windows, so the MSI needs no .NET runtime).
& "$root\ui\build.ps1"
Copy-Item "$root\ui\YalClearVoice.exe" $Stage -Force

# 3. Devnode helper.
cl /nologo /EHsc /W4 /DUNICODE /D_UNICODE /O2 `
  "$PSScriptRoot\yalcv-devnode\devnode.cpp" setupapi.lib newdev.lib `
  /Fe:"$Stage\yalcv-devnode.exe" /link /SUBSYSTEM:CONSOLE
Remove-Item devnode.obj -ErrorAction SilentlyContinue

# 4. Supervisor service (pending artifact): copy if present, else warn.
if (Test-Path "$root\target\release\yalcv-supervisor.exe") {
  Copy-Item "$root\target\release\yalcv-supervisor.exe" $Stage -Force
} else {
  Write-Warning "yalcv-supervisor.exe not found - the service won't be in the MSI until it's built."
}

# Sign the staged binaries (skip if no cert configured).
# signtool sign /fd sha256 /a "$Stage\clearvoice.exe" "$Stage\YalClearVoice.exe" "$Stage\yalcv-devnode.exe" "$Stage\yalcv-supervisor.exe"

# 5. Build the MSI.
wix build "$PSScriptRoot\Package.wxs" -arch x64 `
  -d "StageDir=$Stage" -d "DriverDir=$DriverDir" `
  -o "$PSScriptRoot\YalClearVoice-$Version.msi"

# 6. Sign the MSI (required for distribution).
# signtool sign /fd sha256 /a "$PSScriptRoot\YalClearVoice-$Version.msi"

Write-Host "Built $PSScriptRoot\YalClearVoice-$Version.msi"
