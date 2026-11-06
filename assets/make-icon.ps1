# Regenerates assets/tray-icon.rgba: the 32x32 tray icon as raw RGBA bytes
# (row-major, 4 bytes per pixel, straight alpha), which the tray code embeds
# with include_bytes!. Run from the repository root:
#   powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1
#
# The icon is pixel art: a little ghost in a dark hat with a talisman emblem
# and a red flower. Edit the grid below; each character is one pixel and maps
# to a colour in $palette ('.' is transparent).

# PowerShell's @{} hashtables ignore case ('h' and 'H' would collide), so
# build a case-sensitive dictionary instead.
$palette = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([StringComparer]::Ordinal)
$palette['.'] = $null
$palette['K'] = @(0x2B, 0x1D, 0x1D)   # outline / hat edge
$palette['H'] = @(0x4A, 0x30, 0x30)   # hat
$palette['h'] = @(0x6E, 0x4A, 0x45)   # hat highlight
$palette['C'] = @(0xEB, 0xDC, 0xC4)   # emblem
$palette['c'] = @(0x3A, 0x2A, 0x2A)   # emblem mark
$palette['R'] = @(0xE0, 0x45, 0x2F)   # flower
$palette['r'] = @(0xB0, 0x2E, 0x20)   # flower shade
$palette['Y'] = @(0xFF, 0xD2, 0x3F)   # flower centre
$palette['W'] = @(0xFF, 0xFF, 0xFF)   # ghost
$palette['G'] = @(0xDA, 0xDC, 0xE6)   # ghost shade
$palette['g'] = @(0x8E, 0x8F, 0xA0)   # ghost outline
$palette['E'] = @(0x2B, 0x1D, 0x1D)   # eyes
$palette['P'] = @(0xF4, 0xA7, 0xA7)   # blush
$palette['M'] = @(0xE4, 0x5A, 0x4C)   # mouth

$grid = @(
    '................................'
    '.............KKKKK..............'
    '............KCCCCCK.............'
    '............KCCcCCK.............'
    '............KCcCcCK.....KKK.....'
    '............KCCcCCK....KRRRK....'
    '.........KKKKCCCCCKKKKKRRYRRK...'
    '........KHHHKKKKKKKHHHHKRRRRK...'
    '.......KHhhHHHHHHHHHHHHKRrRK....'
    '......KHhHHHHHHHHHHHHHHHKKK.....'
    '.....KHHHHHHHHHHHHHHHHHHHHK.....'
    '...KKHHHHHHHHHHHHHHHHHHHHHHKK...'
    '..KHHHhhhhhhhhhhhhhhhhhhHHHHK...'
    '..KKKKgWWWWWWWWWWWWWWWWWWgKKKK..'
    '.....gWWWEEWWWWWWWWWEEWWWWg.....'
    '....gWWWEWWEWWWWWWWEWWEWWWg.....'
    '....gWWWWWWWWWWWWWWWWWWWWWg.....'
    '....gWWPPPWWMMMMMWWWPPPWWWg.....'
    '....gWWPPPWWWMMMWWWWPPPWWWg.....'
    '....gWWWWWWWWWWWWWWWWWWWWWg.....'
    '...gWWWWWWWWWWWWWWWWWWWWWWWg....'
    '.ggWWWWWWWWWWWWWWWWWWWWWWWWWgg..'
    'gWWWWWWWWWWWWWWWWWWWWWWWWWWWWWg.'
    '.gGGWWWWWWWWWWWWWWWWWWWWWWWGGg..'
    '..gggWWWWWWWWWWWWWWWWWWWWWggg...'
    '.....gWWWWWWWWWWWWWWWWWWWg......'
    '......gWWWWWWWWWWWWWWWWGg.......'
    '.......gGWWWWWWWWWWWWGGg........'
    '........ggGWWWWWWWGGgg..........'
    '..........gggggggg..............'
    '................................'
    '................................'
)

$size = 32
if ($grid.Count -ne $size) { throw "grid has $($grid.Count) rows, expected $size" }
$bytes = New-Object byte[] ($size * $size * 4)
for ($y = 0; $y -lt $size; $y++) {
    $row = $grid[$y]
    if ($row.Length -ne $size) { throw "row $y has $($row.Length) pixels, expected $size" }
    for ($x = 0; $x -lt $size; $x++) {
        $key = [string]$row[$x]
        if (-not $palette.ContainsKey($key)) { throw "row $y, column ${x}: unknown pixel '$key'" }
        $col = $palette[$key]
        if ($null -eq $col) { continue }   # transparent: leave as 0,0,0,0
        $o = ($y * $size + $x) * 4
        $bytes[$o] = $col[0]; $bytes[$o + 1] = $col[1]; $bytes[$o + 2] = $col[2]; $bytes[$o + 3] = 255
    }
}
[IO.File]::WriteAllBytes((Join-Path $PSScriptRoot 'tray-icon.rgba'), $bytes)
"wrote $($bytes.Length) bytes"

# --- assets/tao.ico: the same art as the executables' icon (Explorer, Task
# Manager, taskbar), embedded by build.rs via assets/tao.rc. Windows picks
# the closest size, so provide the common ones. Whole multiples of 32 are
# scaled nearest-neighbour to keep the pixel art crisp; 16 and 48 can't be,
# so they are resampled smoothly. Each frame is stored as PNG (supported
# in .ico files since Windows Vista).
Add-Type -AssemblyName System.Drawing
$src = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
for ($y = 0; $y -lt $size; $y++) {
    for ($x = 0; $x -lt $size; $x++) {
        $o = ($y * $size + $x) * 4
        $src.SetPixel($x, $y, [System.Drawing.Color]::FromArgb($bytes[$o + 3], $bytes[$o], $bytes[$o + 1], $bytes[$o + 2]))
    }
}

$frames = @()
foreach ($n in 16, 32, 48, 64, 256) {
    $bmp = New-Object System.Drawing.Bitmap $n, $n, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    if ($n % $size -eq 0) {
        $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
        # Without this, nearest-neighbour scaling is shifted by half a pixel.
        $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
    } else {
        $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    }
    $g.DrawImage($src, 0, 0, $n, $n)
    $g.Dispose()
    $ms = New-Object IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    $frames += , @($n, $ms.ToArray())
}

# ICO layout: ICONDIR (6 bytes), one 16-byte ICONDIRENTRY per frame, then
# the frame data. A width/height byte of 0 means 256.
$out = New-Object IO.MemoryStream
$w = New-Object IO.BinaryWriter $out
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$frames.Count)
$offset = 6 + 16 * $frames.Count
foreach ($f in $frames) {
    $dim = if ($f[0] -ge 256) { 0 } else { $f[0] }
    $w.Write([byte]$dim); $w.Write([byte]$dim)
    $w.Write([byte]0); $w.Write([byte]0)          # palette size, reserved
    $w.Write([uint16]1); $w.Write([uint16]32)     # planes, bits per pixel
    $w.Write([uint32]$f[1].Length); $w.Write([uint32]$offset)
    $offset += $f[1].Length
}
foreach ($f in $frames) { $w.Write([byte[]]$f[1]) }
$w.Flush()
[IO.File]::WriteAllBytes((Join-Path $PSScriptRoot 'tao.ico'), $out.ToArray())
"wrote tao.ico ($($frames.Count) sizes, $($out.Length) bytes)"
