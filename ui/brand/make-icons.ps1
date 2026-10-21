# Makes the YAL artwork the app and the setup use, from yal-logo.svg (YAL's
# official logo, from https://yal.inc/yal-logo.svg):
#   yal-mark.svg   the knot symbol alone (the logo without its letters)
#   yal-logo.png   the full logo, 4x size, transparent: the windows' header
#   yal.ico        the knot at 16-256 px: the exe, shortcut and window icon
# Rendering uses Microsoft Edge (part of Windows) in headless mode.
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
$here = $PSScriptRoot
$edge = @("${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
          "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $edge) { throw "Microsoft Edge not found." }

# 1. The knot: the logo's first path ("Vector"); the letters are Vector_2..5.
$logo = Get-Content "$here\yal-logo.svg" -Raw
$mark = [regex]::Replace($logo, '<path[^>]*id="Vector_\d+"[^>]*/>\s*', '')
# The knot spans x 0-28.5, y 0-28.2: centre it in a 32-unit square, for an even margin.
$mark = $mark -replace 'width="77.9914" height="28.9928" viewBox="0 0 77.9914 28.9928"', 'width="32" height="32" viewBox="-1.75 -1.9 32 32"'
[IO.File]::WriteAllText("$here\yal-mark.svg", $mark, (New-Object Text.UTF8Encoding($false)))

# 2. Render an SVG at w x h onto a transparent PNG.
function Render($svg, $png, $w, $h) {
  $tmp = Join-Path $env:TEMP "yal-render"
  New-Item -ItemType Directory -Force $tmp | Out-Null
  Copy-Item $svg "$tmp\art.svg" -Force
  $html = "<html><body style='margin:0;background:transparent'><img src='art.svg' style='display:block;width:${w}px;height:${h}px'></body></html>"
  [IO.File]::WriteAllText("$tmp\page.html", $html)
  $url = "file:///" + ("$tmp\page.html" -replace '\\', '/')
  Start-Process $edge -Wait -WindowStyle Hidden -ArgumentList '--headless=new', '--disable-gpu', '--hide-scrollbars',
    '--force-device-scale-factor=1', '--default-background-color=00000000', "--window-size=$w,$h",
    "--user-data-dir=$tmp\profile", "--screenshot=`"$png`"", $url
  if (-not (Test-Path $png)) { throw "Edge didn't render $svg" }
}
Render "$here\yal-logo.svg" "$here\yal-logo.png" 312 116
Render "$here\yal-mark.svg" "$env:TEMP\yal-mark-256.png" 256 256

# 3. yal.ico: 16-64 px as 32-bit bitmaps (what every Windows tool reads), 256 as PNG.
$big = [Drawing.Bitmap]::FromFile("$env:TEMP\yal-mark-256.png")
$entries = foreach ($size in 16, 20, 24, 32, 40, 48, 64, 256) {
  $bmp = New-Object Drawing.Bitmap $size, $size, ([Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [Drawing.Graphics]::FromImage($bmp)
  $g.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
  $g.PixelOffsetMode = [Drawing.Drawing2D.PixelOffsetMode]::HighQuality
  $g.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::HighQuality
  $g.DrawImage($big, 0, 0, $size, $size); $g.Dispose()
  $ms = New-Object IO.MemoryStream
  if ($size -eq 256) {
    $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png)
  } else {
    $w = New-Object IO.BinaryWriter $ms
    $w.Write([int]40); $w.Write([int]$size); $w.Write([int]($size * 2)); $w.Write([int16]1); $w.Write([int16]32)
    $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0)
    $data = $bmp.LockBits((New-Object Drawing.Rectangle 0, 0, $size, $size), 'ReadOnly', 'Format32bppArgb')
    $row = New-Object byte[] ($size * 4)
    for ($y = $size - 1; $y -ge 0; $y--) {   # bitmaps run bottom-up
      [Runtime.InteropServices.Marshal]::Copy([IntPtr]($data.Scan0.ToInt64() + $y * $data.Stride), $row, 0, $row.Length)
      $w.Write($row)
    }
    $bmp.UnlockBits($data)
    $w.Write((New-Object byte[] ([Math]::Ceiling($size / 32) * 4 * $size)))   # empty AND mask: alpha does the work
    $w.Flush()
  }
  $bmp.Dispose()
  [pscustomobject]@{ Size = $size; Bytes = $ms.ToArray() }
}
$big.Dispose()
$out = New-Object IO.BinaryWriter ([IO.File]::Create("$here\yal.ico"))
$out.Write([int16]0); $out.Write([int16]1); $out.Write([int16]$entries.Count)
$offset = 6 + 16 * $entries.Count
foreach ($e in $entries) {
  $dim = if ($e.Size -ge 256) { 0 } else { $e.Size }
  $out.Write([byte]$dim); $out.Write([byte]$dim); $out.Write([byte]0); $out.Write([byte]0)
  $out.Write([int16]1); $out.Write([int16]32); $out.Write([int]$e.Bytes.Length); $out.Write([int]$offset)
  $offset += $e.Bytes.Length
}
foreach ($e in $entries) { $out.Write($e.Bytes) }
$out.Close()
"made yal-mark.svg, yal-logo.png, yal.ico ({0} sizes)" -f $entries.Count
