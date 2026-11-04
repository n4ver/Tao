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
