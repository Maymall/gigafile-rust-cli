# rgfile installer for Windows: downloads the latest release, verifies its
# SHA-256 against the release's SHA256SUMS, installs it, and adds it to PATH.
#
#   irm https://raw.githubusercontent.com/Maymall/gigafile-rust-cli/main/install.ps1 | iex
#
# Override the install directory with $env:RGFILE_INSTALL_DIR.
$ErrorActionPreference = "Stop"

function Invoke-BoundedDownload {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [Uri]$Uri,

        [Parameter(Mandatory = $true)]
        [string]$Destination,

        [Parameter(Mandatory = $true)]
        [long]$MaxBytes
    )

    if ($Uri.Scheme -cne [Uri]::UriSchemeHttps) {
        throw "refusing to download over a non-HTTPS URL: $Uri"
    }
    if ($MaxBytes -le 0) {
        throw "download size limit must be positive"
    }

    Add-Type -AssemblyName System.Net.Http
    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromMinutes(10)
    $client.DefaultRequestHeaders.UserAgent.ParseAdd("rgfile-installer")
    $request = $null
    $response = $null
    $inputStream = $null
    $outputStream = $null
    try {
        $currentUri = $Uri
        for ($redirects = 0; $redirects -le 5; $redirects++) {
            $request = [System.Net.Http.HttpRequestMessage]::new(
                [System.Net.Http.HttpMethod]::Get,
                $currentUri)
            $response = $client.SendAsync(
                $request,
                [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead
            ).GetAwaiter().GetResult()

            $status = [int]$response.StatusCode
            if (@(301, 302, 303, 307, 308) -notcontains $status) {
                break
            }
            if ($redirects -eq 5) {
                throw "download exceeded the redirect limit: $Uri"
            }
            $location = $response.Headers.Location
            if ($null -eq $location) {
                throw "download redirect did not include a Location header: $currentUri"
            }
            $nextUri = if ($location.IsAbsoluteUri) {
                $location
            } else {
                [Uri]::new($currentUri, $location.OriginalString)
            }
            if ($nextUri.Scheme -cne [Uri]::UriSchemeHttps) {
                throw "download redirected to a non-HTTPS URL: $nextUri"
            }

            $response.Dispose()
            $response = $null
            $request.Dispose()
            $request = $null
            $currentUri = $nextUri
        }

        if (-not $response.IsSuccessStatusCode) {
            throw "download failed with HTTP status $([int]$response.StatusCode): $Uri"
        }
        $contentLength = $response.Content.Headers.ContentLength
        if ($null -ne $contentLength -and $contentLength -gt $MaxBytes) {
            throw "download exceeds the $MaxBytes-byte limit: $Uri"
        }

        $inputStream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $outputStream = [System.IO.File]::Open(
            $Destination,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None)
        $buffer = New-Object byte[] 65536
        [long]$total = 0
        while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            if ($read -gt ($MaxBytes - $total)) {
                throw "download exceeds the $MaxBytes-byte limit: $Uri"
            }
            $outputStream.Write($buffer, 0, $read)
            $total += $read
        }
        $outputStream.Flush($true)
    }
    finally {
        if ($null -ne $outputStream) { $outputStream.Dispose() }
        if ($null -ne $inputStream) { $inputStream.Dispose() }
        if ($null -ne $response) { $response.Dispose() }
        if ($null -ne $request) { $request.Dispose() }
        $client.Dispose()
        $handler.Dispose()
    }
}

function Expand-RgfileBinary {
    [CmdletBinding()]
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

    Add-Type -AssemblyName System.IO.Compression
    $archiveStream = $null
    $zip = $null
    $inputStream = $null
    $outputStream = $null
    try {
        $archiveStream = [System.IO.File]::Open(
            $Archive,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::Read)
        $zip = [System.IO.Compression.ZipArchive]::new(
            $archiveStream,
            [System.IO.Compression.ZipArchiveMode]::Read,
            $false)

        $matches = @($zip.Entries | Where-Object { $_.FullName -ceq $ExpectedMember })
        if ($matches.Count -ne 1) {
            throw "archive must contain exactly one $ExpectedMember entry"
        }
        $entry = $matches[0]
        if ([string]::IsNullOrEmpty($entry.Name)) {
            throw "archive member $ExpectedMember is not a regular file"
        }

        # ZIP stores the Unix file type in the high 16 bits when present.
        # Reject directories, symlinks, hard links, and other special types.
        $unixType = ([int64]$entry.ExternalAttributes -shr 16) -band 0xF000
        if ($unixType -ne 0 -and $unixType -ne 0x8000) {
            throw "archive member $ExpectedMember is not a regular file"
        }
        if ($entry.Length -le 0 -or $entry.Length -gt $MaxBytes) {
            throw "archive binary has an invalid size"
        }

        # Never use the entry's path as a filesystem destination. Stream the
        # one verified entry to a script-chosen CreateNew file instead.
        $inputStream = $entry.Open()
        $outputStream = [System.IO.File]::Open(
            $Destination,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None)
        $buffer = New-Object byte[] 65536
        [long]$total = 0
        while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            if ($read -gt ($MaxBytes - $total)) {
                throw "archive binary exceeds the $MaxBytes-byte limit"
            }
            $outputStream.Write($buffer, 0, $read)
            $total += $read
        }
        if ($total -ne $entry.Length) {
            throw "archive binary length does not match its ZIP metadata"
        }
        $outputStream.Flush($true)
    }
    finally {
        if ($null -ne $outputStream) { $outputStream.Dispose() }
        if ($null -ne $inputStream) { $inputStream.Dispose() }
        if ($null -ne $zip) { $zip.Dispose() }
        if ($null -ne $archiveStream) { $archiveStream.Dispose() }
    }
}

function Invoke-RgfileInstaller {
    $repo = "Maymall/gigafile-rust-cli"
    $installDir = if ($env:RGFILE_INSTALL_DIR) { $env:RGFILE_INSTALL_DIR } else { "$env:LOCALAPPDATA\Programs\rgfile" }
    $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($architecture -ne "AMD64") {
        throw "unsupported Windows architecture: $architecture; build rgfile from source on this platform"
    }
    $target = "x86_64-pc-windows-msvc"
    [long]$maxArchiveBytes = 536870912
    [long]$maxMetadataBytes = 1048576
    [long]$maxBinaryBytes = 268435456

    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) "rgfile-install-$([System.Guid]::NewGuid())"
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $releasePath = Join-Path $tmp "latest.json"
        Invoke-BoundedDownload `
            -Uri "https://api.github.com/repos/$repo/releases/latest" `
            -Destination $releasePath `
            -MaxBytes $maxMetadataBytes
        $release = Get-Content -LiteralPath $releasePath -Raw | ConvertFrom-Json
        $tag = [string]$release.tag_name
        if ($tag -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
            throw "cannot determine the latest version (got tag: $tag)"
        }
        $version = $tag.Substring(1)
        $asset = "rgfile-$version-$target.zip"
        $base = "https://github.com/$repo/releases/download/$tag"

        Write-Host "Downloading rgfile $version for $target ..."
        $assetPath = Join-Path $tmp $asset
        $checksumPath = Join-Path $tmp "SHA256SUMS"
        Invoke-BoundedDownload -Uri "$base/$asset" -Destination $assetPath -MaxBytes $maxArchiveBytes
        Invoke-BoundedDownload -Uri "$base/SHA256SUMS" -Destination $checksumPath -MaxBytes $maxMetadataBytes

        # Match exactly one complete asset field. Treat SHA256SUMS as untrusted input.
        $matchingHashes = @()
        $assetLineCount = 0
        foreach ($line in Get-Content -LiteralPath $checksumPath) {
            $fields = $line.Trim() -split '\s+'
            if ($fields.Count -ge 2 -and $fields[1] -ceq $asset) {
                $assetLineCount++
                if ($fields.Count -eq 2 -and $fields[0] -cmatch '^[0-9A-Fa-f]{64}$') {
                    $matchingHashes += $fields[0].ToLowerInvariant()
                }
            }
        }
        if ($assetLineCount -ne 1 -or $matchingHashes.Count -ne 1) {
            throw "expected exactly one checksum for $asset in SHA256SUMS"
        }
        $expected = $matchingHashes[0]
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $assetPath).Hash.ToLowerInvariant()
        if ($expected -ne $actual) { throw "checksum verification FAILED" }
        Write-Host "Checksum OK."

        $member = "rgfile-$version-$target/rgfile.exe"
        $binary = Join-Path $tmp "rgfile.exe"
        Expand-RgfileBinary `
            -Archive $assetPath `
            -ExpectedMember $member `
            -Destination $binary `
            -MaxBytes $maxBinaryBytes
        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        $destination = Join-Path $installDir "rgfile.exe"
        $staging = Join-Path $installDir ".rgfile-$([System.Guid]::NewGuid()).exe"
        try {
            [System.IO.File]::Copy($binary, $staging, $false)
            $stagedVersion = @(& $staging --version 2>$null)
            if ($LASTEXITCODE -ne 0) {
                throw "staged binary failed its --version check"
            }
            if ($stagedVersion.Count -ne 1 -or
                [string]$stagedVersion[0] -cne "rgfile $version") {
                throw "staged binary reported an unexpected version"
            }

            # Replace the destination with a same-volume, write-through rename.
            # If this fails, the old installation remains in place.
            if (-not ("RgfileInstallerNativeMethods" -as [type])) {
                Add-Type @"
using System;
using System.Runtime.InteropServices;

public static class RgfileInstallerNativeMethods
{
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool MoveFileEx(
        string existingFileName,
        string newFileName,
        uint flags);
}
"@
            }
            $moveFileReplaceExisting = 0x1
            $moveFileWriteThrough = 0x8
            $moved = [RgfileInstallerNativeMethods]::MoveFileEx(
                $staging,
                $destination,
                $moveFileReplaceExisting -bor $moveFileWriteThrough)
            if (-not $moved) {
                $win32Error = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
                throw "cannot replace $destination (Win32 error $win32Error)"
            }
            $staging = $null
        }
        finally {
            if ($staging -and (Test-Path -LiteralPath $staging)) {
                Remove-Item -LiteralPath $staging -Force -ErrorAction SilentlyContinue
            }
        }

        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (($userPath -split ";") -notcontains $installDir) {
            $newUserPath = if ([string]::IsNullOrEmpty($userPath)) {
                $installDir
            } else {
                "$userPath;$installDir"
            }
            [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
            Write-Host "Added $installDir to your user PATH (restart the terminal to pick it up)."
        }
        Write-Host "Installed: $installDir\rgfile.exe ($(& "$installDir\rgfile.exe" --version))"
    }
    finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Dot-sourcing exposes the extraction helper for offline safety tests. Normal
# script execution and `irm ... | iex` both run the installer.
if ($MyInvocation.InvocationName -ne ".") {
    Invoke-RgfileInstaller
}
