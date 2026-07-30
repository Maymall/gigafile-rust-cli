$ErrorActionPreference = "Stop"

$repoDir = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
. (Join-Path $repoDir "install.ps1")

Add-Type -AssemblyName System.IO.Compression

function Add-ZipEntry {
    param(
        [Parameter(Mandatory = $true)]
        [System.IO.Compression.ZipArchive]$Archive,

        [Parameter(Mandatory = $true)]
        [string]$Name,

        [Parameter(Mandatory = $true)]
        [byte[]]$Body,

        [int]$ExternalAttributes = 0
    )

    $entry = $Archive.CreateEntry($Name)
    $entry.ExternalAttributes = $ExternalAttributes
    $stream = $entry.Open()
    try {
        $stream.Write($Body, 0, $Body.Length)
    }
    finally {
        $stream.Dispose()
    }
}

function New-TestArchive {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [string]$Kind,

        [Parameter(Mandatory = $true)]
        [string]$ExpectedMember,

        [string]$UnexpectedMember = "../must-not-be-extracted.exe"
    )

    $file = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None)
    $archive = $null
    try {
        $archive = [System.IO.Compression.ZipArchive]::new(
            $file,
            [System.IO.Compression.ZipArchiveMode]::Create,
            $false)
        $validBody = [Text.Encoding]::UTF8.GetBytes("valid binary")
        switch ($Kind) {
            "valid" {
                Add-ZipEntry -Archive $archive -Name $ExpectedMember -Body $validBody
                Add-ZipEntry `
                    -Archive $archive `
                    -Name $UnexpectedMember `
                    -Body ([Text.Encoding]::UTF8.GetBytes("malicious sibling"))
            }
            "duplicate" {
                Add-ZipEntry -Archive $archive -Name $ExpectedMember -Body $validBody
                Add-ZipEntry `
                    -Archive $archive `
                    -Name $ExpectedMember `
                    -Body ([Text.Encoding]::UTF8.GetBytes("different duplicate"))
            }
            "symlink" {
                $symlinkMode = 0xA1FF -shl 16
                Add-ZipEntry `
                    -Archive $archive `
                    -Name $ExpectedMember `
                    -Body ([Text.Encoding]::UTF8.GetBytes("../../victim.exe")) `
                    -ExternalAttributes $symlinkMode
            }
            "oversized" {
                Add-ZipEntry `
                    -Archive $archive `
                    -Name $ExpectedMember `
                    -Body (New-Object byte[] 1025)
            }
            default {
                throw "unknown archive kind: $Kind"
            }
        }
    }
    finally {
        if ($null -ne $archive) { $archive.Dispose() }
        $file.Dispose()
    }
}

function Assert-ArchiveRejected {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Archive,

        [Parameter(Mandatory = $true)]
        [string]$ExpectedMember,

        [Parameter(Mandatory = $true)]
        [string]$Destination,

        [Parameter(Mandatory = $true)]
        [long]$MaxBytes
    )

    $accepted = $false
    try {
        Expand-RgfileBinary `
            -Archive $Archive `
            -ExpectedMember $ExpectedMember `
            -Destination $Destination `
            -MaxBytes $MaxBytes
        $accepted = $true
    }
    catch {
        Write-Verbose "Expected rejection: $($_.Exception.Message)"
    }
    if ($accepted) {
        throw "archive was unexpectedly accepted: $Archive"
    }
    if (Test-Path -LiteralPath $Destination) {
        throw "rejected archive wrote a destination file: $Destination"
    }
}

$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "rgfile-installer-tests-$([Guid]::NewGuid())"
$escapedName = "rgfile-installer-escaped-$([Guid]::NewGuid()).exe"
$escapedPath = Join-Path (Split-Path -Parent $testRoot) $escapedName
New-Item -ItemType Directory -Path $testRoot | Out-Null
try {
    $member = "rgfile-9.9.9-x86_64-pc-windows-msvc/rgfile.exe"

    $validArchive = Join-Path $testRoot "valid.zip"
    $validBinary = Join-Path $testRoot "valid.exe"
    New-TestArchive `
        -Path $validArchive `
        -Kind "valid" `
        -ExpectedMember $member `
        -UnexpectedMember "../$escapedName"
    Expand-RgfileBinary `
        -Archive $validArchive `
        -ExpectedMember $member `
        -Destination $validBinary `
        -MaxBytes 1024
    if ((Get-Content -LiteralPath $validBinary -Raw) -cne "valid binary") {
        throw "valid archive produced unexpected binary content"
    }
    if (Test-Path -LiteralPath $escapedPath) {
        throw "path traversal entry was extracted"
    }

    foreach ($kind in @("duplicate", "symlink", "oversized")) {
        $archive = Join-Path $testRoot "$kind.zip"
        $destination = Join-Path $testRoot "$kind.exe"
        New-TestArchive -Path $archive -Kind $kind -ExpectedMember $member
        Assert-ArchiveRejected `
            -Archive $archive `
            -ExpectedMember $member `
            -Destination $destination `
            -MaxBytes 1024
    }

    Write-Host "PowerShell installer archive safety tests passed."
}
finally {
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $escapedPath -Force -ErrorAction SilentlyContinue
}
