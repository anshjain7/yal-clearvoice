# Builds "YAL ClearVoice Setup.exe": one file holding the app (window, engine with
# the C runtime built in, model, settings.ini) and the VB-CABLE driver pack.
# Needs the Rust MSVC toolchain and VS Build Tools (csc), as for the app itself.
#
#   .\build.ps1                       # settings: %LOCALAPPDATA%\YAL ClearVoice\setup-settings.ini,
#                                     # else the installed app's, else the Desktop test folder's;
#                                     # VB-CABLE pack from the Desktop
#   .\build.ps1 -Settings <file> -VbCable <folder> -Out <folder>
#   .\build.ps1 -Check                # also builds setup-check.exe (no admin prompt) for /check
#
# The settings.ini you give is what clients get: with keys in it, anyone holding
# the setup file can extract them. VB-CABLE goes in unmodified; integrating it in
# an installer needs VB-Audio's agreement (its licence says so).
param(
  [string]$Settings = (@("$env:LOCALAPPDATA\YAL ClearVoice\setup-settings.ini",
                         "$env:ProgramFiles\YAL ClearVoice\settings.ini",
                         "$env:USERPROFILE\Desktop\YAL ClearVoice\settings.ini") |
                       Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1),
  [string]$VbCable = "$env:USERPROFILE\Desktop\VBCABLE_Driver_Pack45",
  [string]$Out = "$PSScriptRoot\out",
  [switch]$Check
)
$ErrorActionPreference = "Stop"
$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent   # the clearvoice crate
if (-not $Settings) { throw "No settings.ini found: pass -Settings <file>." }
foreach ($need in "$Settings","$VbCable\VBCABLE_Setup_x64.exe", "$root\models\DeepFilterNet3_onnx.tar.gz") {
  if (-not (Test-Path -LiteralPath $need)) { throw "Missing: $need" }
}

# 1. Engine, with the C runtime linked in so client PCs need no VC++ redistributable.
#    A short target path: long exe paths break the microphone (0x8007007A).
$saved = $env:RUSTFLAGS, $env:CARGO_TARGET_DIR
$env:RUSTFLAGS = "-C target-feature=+crt-static"
$env:CARGO_TARGET_DIR = "$env:TEMP\cvdist"
# cargo reports progress on stderr, which "Stop" would treat as a failure.
$ErrorActionPreference = "Continue"
try {
  cargo +stable-x86_64-pc-windows-msvc build --release --manifest-path "$root\Cargo.toml" --bin yalcv_cable
  if ($LASTEXITCODE) { throw "engine build failed" }
} finally {
  $env:RUSTFLAGS, $env:CARGO_TARGET_DIR = $saved
  $ErrorActionPreference = "Stop"
}

# 2. Window.
& "$root\ui\build.ps1"

# 3. The two payloads, zipped.
$work = "$env:TEMP\cvsetup"
if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
$stage = New-Item -ItemType Directory -Force "$work\payload"
Copy-Item "$root\ui\YalClearVoice.exe" "$stage\YAL ClearVoice.exe"
Copy-Item "$env:TEMP\cvdist\release\yalcv_cable.exe" "$stage\YAL ClearVoice Engine.exe"
Copy-Item "$root\models\DeepFilterNet3_onnx.tar.gz" $stage
Copy-Item -LiteralPath $Settings "$stage\settings.ini"
Add-Type -AssemblyName System.IO.Compression.FileSystem
$opt = [IO.Compression.CompressionLevel]::Optimal
[IO.Compression.ZipFile]::CreateFromDirectory($stage, "$work\payload.zip", $opt, $false)
[IO.Compression.ZipFile]::CreateFromDirectory((Resolve-Path -LiteralPath $VbCable), "$work\vbcable.zip", $opt, $false)

# 4. The setup itself, with both zips embedded.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$csc = & $vswhere -latest -products * -find "MSBuild\**\Roslyn\csc.exe" | Select-Object -First 1
if (-not $csc) { throw "C# compiler not found: install Visual Studio Build Tools." }
New-Item -ItemType Directory -Force $Out | Out-Null
$common = @("-nologo", "-target:winexe", "-platform:x64", "-optimize+", "-langversion:latest", "-nullable:enable",
  "-r:System.Windows.Forms.dll", "-r:System.Drawing.dll", "-r:System.Core.dll", "-r:Microsoft.CSharp.dll",
  "-r:System.IO.Compression.dll", "-r:System.IO.Compression.FileSystem.dll",
  "-resource:$work\payload.zip,payload.zip", "-resource:$work\vbcable.zip,vbcable.zip",
  # YAL's icon (the setup file's own, and its window's) and logo, as the app uses.
  "-win32icon:$root\ui\brand\yal.ico", "-resource:$root\ui\brand\yal.ico,yal.ico",
  "-resource:$root\ui\brand\yal-logo.png,yal-logo.png")
& $csc @common "-win32manifest:$PSScriptRoot\app.manifest" "-out:$Out\YAL ClearVoice Setup.exe" "$PSScriptRoot\Setup.cs"
if ($LASTEXITCODE) { throw "setup build failed" }
if ($Check) {
  # Same program without the admin manifest, so /check runs without a prompt.
  & $csc @common "-out:$Out\setup-check.exe" "$PSScriptRoot\Setup.cs"
  if ($LASTEXITCODE) { throw "check build failed" }
}
# The staged payload holds a copy of settings.ini (keys): don't leave it in Temp.
Remove-Item -LiteralPath $work -Recurse -Force
Get-Item "$Out\YAL ClearVoice Setup.exe" | ForEach-Object { "Built {0} ({1:N1} MB)" -f $_.FullName, ($_.Length / 1MB) }
