# Regenerates assets/tray-icon.rgba: a 32x32 yin-yang as raw RGBA bytes
# (row-major, 4 bytes per pixel, straight alpha), which the tray code embeds
# with include_bytes!. Run from the repository root:
#   powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1

$size = 32
$ss = 4                      # 4x4 supersampling for anti-aliased edges
$radius = 15.0               # outer radius in pixels (PowerShell names are case-insensitive, so not $R/$r)
$c = $size / 2.0
$dark = @(43, 47, 58)        # #2B2F3A
$light = @(244, 241, 234)    # #F4F1EA

function Sample([double]$x, [double]$y) {
    # Returns $null (transparent), 'd' (dark) or 'l' (light) at a point
    # relative to the centre. y grows downwards.
    $dist = [Math]::Sqrt($x * $x + $y * $y)
    if ($dist -gt $radius) { return $null }
    if ($dist -gt $radius - 1.25) { return 'd' }       # outline ring
    $h = $radius / 2.0
    $up = [Math]::Sqrt($x * $x + ($y + $h) * ($y + $h)) # distance to upper lobe centre
    $dn = [Math]::Sqrt($x * $x + ($y - $h) * ($y - $h)) # distance to lower lobe centre
    $dot = $radius / 6.0
    if ($up -lt $dot) { return 'd' }                   # dark dot in the light lobe
    if ($dn -lt $dot) { return 'l' }                   # light dot in the dark lobe
    if ($up -lt $h) { return 'l' }
    if ($dn -lt $h) { return 'd' }
    if ($x -lt 0) { return 'l' } else { return 'd' }
}

$bytes = New-Object byte[] ($size * $size * 4)
for ($py = 0; $py -lt $size; $py++) {
    for ($px = 0; $px -lt $size; $px++) {
        $sum = @(0.0, 0.0, 0.0); $cover = 0
        for ($sy = 0; $sy -lt $ss; $sy++) {
            for ($sx = 0; $sx -lt $ss; $sx++) {
                $x = $px + ($sx + 0.5) / $ss - $c
                $y = $py + ($sy + 0.5) / $ss - $c
                $s = Sample $x $y
                if ($null -ne $s) {
                    $col = if ($s -eq 'd') { $dark } else { $light }
                    for ($i = 0; $i -lt 3; $i++) { $sum[$i] += $col[$i] }
                    $cover++
                }
            }
        }
        $o = ($py * $size + $px) * 4
        if ($cover -gt 0) {
            for ($i = 0; $i -lt 3; $i++) { $bytes[$o + $i] = [byte][Math]::Round($sum[$i] / $cover) }
            $bytes[$o + 3] = [byte][Math]::Round(255.0 * $cover / ($ss * $ss))
        }
    }
}
[IO.File]::WriteAllBytes((Join-Path $PSScriptRoot 'tray-icon.rgba'), $bytes)
"wrote $($bytes.Length) bytes"
