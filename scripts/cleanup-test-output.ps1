[CmdletBinding()]
param(
    [switch]$WhatIf
)

$ErrorActionPreference = 'Stop'

$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$testOutput = (Resolve-Path -LiteralPath (Join-Path $projectRoot 'test-output')).Path

# Preserve binaries used as named baselines or measured artifacts in reports.
$preservedRelativePaths = @(
    'optimization-completion-20261018\before.exe'
    'optimization-jpeg-header-20261018\profile-before.exe'
    'optimization-jpeg-header-20261018\profile-final.exe'
    'optimization-next-20261018\before.exe'
    'optimization-next-20261018\delivery.exe'
    'optimization-next-20261018\release.exe'
    'optimization-r02-20261018\before.exe'
    'perf-next\before.exe'
    'perf-next\after.exe'
    'simd-integration-20261018\before.exe'
    'simd-integration-20261018\after.exe'
    'targeted-20261018\before.exe'
    'targeted-20261018\after.exe'
)

$preserved = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
foreach ($relativePath in $preservedRelativePaths) {
    [void]$preserved.Add($relativePath.Replace('/', '\'))
}

# Keep one known representative for each commonly repeated photo fixture set.
$preferredPhotoDirectories = @(
    'photo-before\photos'
    'browser-features-1792264058373742200\photos'
    'image-size-settings-1792263389677165900\photos'
    'stage-profile-20261018-c\photos'
    'resize-compare-20261018-a\photos'
    'optimization-jpeg-header-20261018\paired-final\photos'
    'viewer-before\photos'
)
$preferredPhotos = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
foreach ($relativePath in $preferredPhotoDirectories) {
    [void]$preferredPhotos.Add($relativePath.Replace('/', '\'))
}

function Get-DirectoryContentHash {
    param([Parameter(Mandatory = $true)][string]$Path)

    $root = (Resolve-Path -LiteralPath $Path).Path
    $manifest = [System.Collections.Generic.List[string]]::new()
    foreach ($item in (Get-ChildItem -LiteralPath $root -File -Recurse -Force | Sort-Object FullName)) {
        $relativePath = $item.FullName.Substring($root.Length).TrimStart('\')
        $fileHash = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash
        $manifest.Add("$relativePath|$($item.Length)|$fileHash")
    }

    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes([string]::Join("`n", $manifest))
        return [System.BitConverter]::ToString($sha256.ComputeHash($bytes)).Replace('-', '')
    }
    finally {
        $sha256.Dispose()
    }
}

$photoDirectoryRecords = @(
    Get-ChildItem -LiteralPath $testOutput -Directory -Recurse -Force |
        Where-Object { $_.Name -eq 'photos' -and -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) } |
        ForEach-Object {
            $relativePath = $_.FullName.Substring($testOutput.Length).TrimStart('\')
            $files = @(Get-ChildItem -LiteralPath $_.FullName -File -Recurse -Force)
            if ($files.Count -gt 0) {
                [pscustomobject]@{
                    Directory = $_.FullName
                    RelativePath = $relativePath
                    ContentHash = (Get-DirectoryContentHash -Path $_.FullName)
                    Bytes = ($files | Measure-Object -Property Length -Sum).Sum
                }
            }
        }
)

$duplicatePhotoDirectories = @(
    foreach ($group in ($photoDirectoryRecords | Group-Object -Property ContentHash)) {
        if ($group.Count -gt 1) {
            $preferred = @($group.Group | Where-Object { $preferredPhotos.Contains($_.RelativePath) }) | Select-Object -First 1
            if ($preferred) {
                $keeper = $preferred
            }
            else {
                $keeper = $group.Group | Sort-Object -Property RelativePath | Select-Object -First 1
            }

            $group.Group | Where-Object { $_.Directory -ne $keeper.Directory }
        }
    }
)

$candidates = @(
    Get-ChildItem -LiteralPath $testOutput -Recurse -File -Filter '*.exe' -Force |
        Where-Object {
            $relativePath = $_.FullName.Substring($testOutput.Length).TrimStart('\')
            -not $preserved.Contains($relativePath)
        } |
        Sort-Object FullName
)

if ($candidates.Count -eq 0 -and $duplicatePhotoDirectories.Count -eq 0) {
    Write-Host 'No redundant EXE files or duplicate photos folders were found.'
    exit 0
}

$exeBytes = ($candidates | Measure-Object -Property Length -Sum).Sum
$photoBytes = ($duplicatePhotoDirectories | Measure-Object -Property Bytes -Sum).Sum
$totalBytes = $exeBytes + $photoBytes
Write-Host "EXE files to remove: $($candidates.Count) ($([math]::Round($exeBytes / 1MB, 1)) MB)"
$candidates | ForEach-Object {
    $relativePath = $_.FullName.Substring($testOutput.Length).TrimStart('\')
    Write-Host ('  {0}  ({1:N1} MB)' -f $relativePath, ($_.Length / 1MB))
}
Write-Host "Duplicate photos folders to remove: $($duplicatePhotoDirectories.Count) ($([math]::Round($photoBytes / 1MB, 1)) MB)"
$duplicatePhotoDirectories | ForEach-Object {
    Write-Host ('  {0}  ({1:N1} MB)' -f $_.RelativePath, ($_.Bytes / 1MB))
}

if ($WhatIf) {
    Write-Host 'WhatIf mode: preview only; no files were deleted.'
    exit 0
}

$confirmation = Read-Host 'Type DELETE to confirm removal; any other input cancels'
if ($confirmation -cne 'DELETE') {
    Write-Host 'Cancelled; no files were deleted.'
    exit 0
}

# Resolve each file again and ensure it remains inside test-output before deletion.
foreach ($file in $candidates) {
    $resolvedPath = (Resolve-Path -LiteralPath $file.FullName).Path
    if (-not $resolvedPath.StartsWith($testOutput + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to delete a path outside test-output: $resolvedPath"
    }

    $relativePath = $resolvedPath.Substring($testOutput.Length).TrimStart('\')
    if ($preserved.Contains($relativePath)) {
        throw "Refusing to delete a preserved file: $relativePath"
    }

    Remove-Item -LiteralPath $resolvedPath -Force
}

# Recheck duplicate content and path boundaries before removing each redundant folder.
foreach ($directory in $duplicatePhotoDirectories) {
    $resolvedPath = (Resolve-Path -LiteralPath $directory.Directory).Path
    if (-not $resolvedPath.StartsWith($testOutput + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to delete a path outside test-output: $resolvedPath"
    }

    if ((Get-DirectoryContentHash -Path $resolvedPath) -ne $directory.ContentHash) {
        throw "Folder contents changed after preview; refusing to delete: $resolvedPath"
    }

    Remove-Item -LiteralPath $resolvedPath -Recurse -Force
}

Write-Host "Done: removed $($candidates.Count) EXE files and $($duplicatePhotoDirectories.Count) duplicate photos folders; freed about $([math]::Round($totalBytes / 1MB, 1)) MB."
