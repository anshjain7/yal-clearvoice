# Builds the YAL ClearVoice window (YalClearVoice.exe, next to this script). There's
# no .NET SDK or runtime to install: it targets .NET Framework 4.8, part of Windows
# 10/11, and compiles with the C# compiler that comes with Visual Studio / Build Tools.
$ErrorActionPreference = "Stop"
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$csc = & $vswhere -latest -products * -find "MSBuild\**\Roslyn\csc.exe" | Select-Object -First 1
if (-not $csc) { throw "C# compiler not found: install Visual Studio Build Tools." }
# YAL's icon (the exe's own, and the windows') and logo come from brand\ (make-icons.ps1).
& $csc -nologo -target:winexe -optimize+ -langversion:latest -nullable:enable `
  "-out:$PSScriptRoot\YalClearVoice.exe" "-win32icon:$PSScriptRoot\brand\yal.ico" `
  "-resource:$PSScriptRoot\brand\yal.ico,yal.ico" "-resource:$PSScriptRoot\brand\yal-logo.png,yal-logo.png" `
  -r:System.Windows.Forms.dll -r:System.Drawing.dll -r:System.Core.dll -r:System.Web.Extensions.dll `
  "$PSScriptRoot\Program.cs"
if ($LASTEXITCODE) { throw "UI build failed" }
