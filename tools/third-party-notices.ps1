<#
.SYNOPSIS
  Regenerates THIRD-PARTY-NOTICES.md at the repo root.

.DESCRIPTION
  Lists every crate that goes into the Windows (x86_64-pc-windows-gnu) and Linux
  (x86_64-unknown-linux-gnu) builds of plug-n-keep, with its version and licence, grouped by
  licence, then adds each distinct licence text once. Only normal (non-dev, non-build)
  dependencies count, so what is listed is what is compiled into the binary.

  It reads `cargo metadata` and the crates' own licence files from the cargo registry, so run
  `cargo fetch --locked` first on a fresh checkout. Needs cargo on PATH and PowerShell 5.1 or
  later. No other tools. Run it from anywhere:

    powershell -File tools/third-party-notices.ps1

  For an "A OR B" licence the first choice in $Prefer that the crate offers is the one the text
  is included for. For "A AND B" both are included.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$targets = 'x86_64-pc-windows-gnu', 'x86_64-unknown-linux-gnu'
$Prefer = 'MIT', 'Apache-2.0', 'Zlib', 'BSD-3-Clause', 'ISC', 'BSL-1.0'

# ---- crates that go into the builds ------------------------------------------------------
$crates = @{}
foreach ($t in $targets) {
    $json = (& cargo metadata --format-version 1 --locked --filter-platform $t) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed for $t" }
    $m = $json | ConvertFrom-Json
    $byId = @{}
    foreach ($p in $m.packages) { $byId[$p.id] = $p }
    $nodes = @{}
    foreach ($n in $m.resolve.nodes) { $nodes[$n.id] = $n }
    $seen = @{}
    $stack = New-Object System.Collections.Stack
    foreach ($id in $m.workspace_members) { $seen[$id] = $true; $stack.Push($id) }
    while ($stack.Count -gt 0) {
        $id = $stack.Pop()
        foreach ($d in $nodes[$id].deps) {
            # kind $null is a normal dependency; "dev" and "build" ones are not shipped.
            $normal = @($d.dep_kinds | Where-Object { -not $_.kind }).Count -gt 0
            if ($normal -and -not $seen.ContainsKey($d.pkg)) { $seen[$d.pkg] = $true; $stack.Push($d.pkg) }
        }
    }
    foreach ($id in $seen.Keys) {
        if ($m.workspace_members -contains $id) { continue }
        $p = $byId[$id]
        $crates["$($p.name) $($p.version)"] = $p
    }
}

# ---- licence expressions -----------------------------------------------------------------
function Normalize-Expr([string]$e) {
    if (-not $e) { return 'UNSPECIFIED' }
    $e = $e -replace '/', ' OR '
    $e = $e -replace '\s+', ' '
    $e = $e.Trim()
    if ($e -notmatch '[()]' -and $e -notmatch ' AND ') {
        # Same set of choices, same group: "A OR B" and "B OR A".
        $e = ($e -split ' OR ' | Sort-Object) -join ' OR '
    }
    return $e
}

# Picks the licence ids the text is needed for: OR takes the first preferred branch, AND both.
function Resolve-Expr([string[]]$tokens, [ref]$pos) {
    $result = @(); $terms = @()
    while ($pos.Value -lt $tokens.Count) {
        $tok = $tokens[$pos.Value]
        if ($tok -eq ')') { break }
        $pos.Value++
        if ($tok -eq '(') {
            $terms += , @(Resolve-Expr $tokens $pos)
            $pos.Value++   # the ')'
        }
        elseif ($tok -eq 'OR' -or $tok -eq 'AND') { $terms += $tok }
        else { $terms += , @($tok) }
    }
    # terms alternate operand, operator, operand...
    $ops = @($terms | Where-Object { $_ -is [string] })
    $operands = @($terms | Where-Object { $_ -isnot [string] })
    if ($ops -contains 'AND') {
        # AND binds tighter in SPDX; treat a mixed expression as all operands required unless
        # it is a pure OR.
        return @($operands | ForEach-Object { $_ })
    }
    if ($ops -contains 'OR') {
        foreach ($pref in $Prefer) {
            foreach ($o in $operands) { if ($o -contains $pref) { return @($o) } }
        }
        return @($operands[0])
    }
    return @($operands | ForEach-Object { $_ })
}
function Needed-Ids([string]$expr) {
    $tokens = [regex]::Matches($expr, '\(|\)|[^\s()]+') | ForEach-Object { $_.Value }
    $p = 0
    return @(Resolve-Expr $tokens ([ref]$p)) | Select-Object -Unique
}

$groups = $crates.Values | Group-Object { Normalize-Expr $_.license } | Sort-Object Count -Descending
$needed = @{}
foreach ($g in $groups) { foreach ($id in (Needed-Ids $g.Name)) { $needed[$id] = $true } }

# ---- licence texts, taken from the crates' own files -------------------------------------
function Crate-Dir($name) {
    $p = $crates.Values | Where-Object { $_.name -eq $name } | Select-Object -First 1
    if (-not $p) { throw "crate $name is not in the build any more; update the text sources" }
    return Split-Path $p.manifest_path
}
function Read-Text($path) {
    $t = [System.IO.File]::ReadAllText($path) -replace "`r`n", "`n"
    return $t.Trim("`n") + "`n"
}
# id -> @(title, file, note). MIT is the standard text: each crate's own copyright line is in
# its source.
$mitText = @"
MIT License

Copyright (c) <year> <copyright holders>

Permission is hereby granted, free of charge, to any person obtaining a copy of this software
and associated documentation files (the "Software"), to deal in the Software without
restriction, including without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
"@
$sources = [ordered]@{
    'MIT'            = @{ Text = $mitText; Note = 'The standard text. Each crate''s own copyright line is in its source.' }
    'Apache-2.0'     = @{ Crate = 'log'; File = 'LICENSE-APACHE' }
    'Zlib'           = @{ Crate = 'zlib-rs'; File = 'LICENSE' }
    'BSD-3-Clause'   = @{ Crate = 'moxcms'; File = 'LICENSE.md' }
    'ISC'            = @{ Crate = 'libloading'; File = 'LICENSE' }
    'BSL-1.0'        = @{ Crate = 'error-code'; File = 'LICENSE' }
    'BSD-2-Clause'   = @{ Crate = 'rusty-s3'; File = 'LICENSE' }
    'CDLA-Permissive-2.0' = @{ Crate = 'webpki-roots'; File = 'LICENSE' }
    'Unicode-3.0'    = @{ Crate = 'unicode-ident'; File = 'LICENSE-UNICODE' }
    'OFL-1.1'        = @{ Crate = 'epaint_default_fonts'; File = 'fonts\OFL.txt'; Note = 'Applies to the Noto fonts egui ships.' }
    'Ubuntu-font-1.0' = @{ Crate = 'epaint_default_fonts'; File = 'fonts\UFL.txt'; Note = 'Applies to the Ubuntu font egui ships.' }
    'IJG'            = @{ Crate = 'jpeg-encoder'; File = 'LICENSE-IJG' }
}
# The icon font egui ships is under its own MIT licence, with its own copyright line.
$extra = @{ Crate = 'epaint_default_fonts'; File = 'fonts\emoji-icon-font-mit-license.txt' }

# ---- write the file ----------------------------------------------------------------------
$sb = New-Object System.Text.StringBuilder
function Add([string]$s = '') { [void]$sb.Append($s).Append("`n") }
Add '# Third-party notices'
Add
Add 'Plug-n-Keep is MIT licensed (see [LICENSE](LICENSE)). It is built from the Rust crates listed'
Add 'below, each under its own licence. The list covers what is compiled into the Windows'
Add '(x86_64-pc-windows-gnu) and Linux (x86_64-unknown-linux-gnu) builds. Where a crate offers a'
Add 'choice of licences (`A OR B`), the full text is given for the one it is used under (MIT where'
Add 'offered, else Apache-2.0). On Linux the app also loads libmtp, libimobiledevice, usbmuxd,'
Add 'libgphoto2 and libheif at run time if the system has them; they are not part of the app and'
Add 'are not bundled.'
Add
Add 'Generated by `tools/third-party-notices.ps1` from `cargo metadata`; do not edit by hand.'
Add
Add "$($crates.Count) crates, in $($groups.Count) licence groups."
Add
Add '## Licences used'
Add
Add '| Licence | Crates |'
Add '|---|---|'
$counts = @{}
foreach ($g in $groups) { foreach ($id in (Needed-Ids $g.Name)) { $counts[$id] = [int]$counts[$id] + $g.Count } }
foreach ($id in ($counts.Keys | Sort-Object { -$counts[$_] })) { Add "| $id | $($counts[$id]) |" }
Add
Add '## Crates'
foreach ($g in $groups) {
    Add
    Add "### $($g.Name) ($($g.Count))"
    Add
    Add '| Crate | Version |'
    Add '|---|---|'
    foreach ($p in ($g.Group | Sort-Object name, version)) {
        Add "| [$($p.name)](https://crates.io/crates/$($p.name)) | $($p.version) |"
    }
}
Add
Add '## Licence texts'
foreach ($id in $sources.Keys) {
    if (-not $needed.ContainsKey($id)) { continue }
    $s = $sources[$id]
    Add
    Add "### $id"
    Add
    if ($s.Note) { Add $s.Note; Add }
    if ($s.Text) { $text = $s.Text }
    else {
        $file = Join-Path (Crate-Dir $s.Crate) $s.File
        $text = Read-Text $file
        Add "Text from the licence file of the ``$($s.Crate)`` crate."
        Add
    }
    Add '```text'
    Add ($text.TrimEnd("`n"))
    Add '```'
    if ($id -eq 'OFL-1.1' -or $id -eq 'Ubuntu-font-1.0') { }
}
foreach ($id in $needed.Keys) {
    if (-not $sources.Contains($id)) { Write-Warning "no licence text source for $id" }
}
if ($needed.ContainsKey('OFL-1.1')) {
    Add
    Add '### MIT (emoji icon font)'
    Add
    Add 'The icon font egui ships has its own copyright line. Text from the licence file of the'
    Add '`epaint_default_fonts` crate.'
    Add
    Add '```text'
    Add ((Read-Text (Join-Path (Crate-Dir $extra.Crate) $extra.File)).TrimEnd("`n"))
    Add '```'
}
$out = Join-Path $root 'THIRD-PARTY-NOTICES.md'
[System.IO.File]::WriteAllText($out, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
"Wrote $out ($($crates.Count) crates)"
