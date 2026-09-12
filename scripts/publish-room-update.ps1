param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^v?\d+\.\d+\.\d+([+-][0-9A-Za-z.-]+)?$')]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$ArtifactPath,

    [string]$Notes = '',
    [string]$PublicBaseUrl = 'https://mag.horuzprod.com/ltk-rooms',
    [string]$VpsHost = 'root@177.153.59.168',
    [string]$RemoteDirectory = '/opt/ltk-room-server/data/updates'
)

$ErrorActionPreference = 'Stop'

$artifact = Get-Item -LiteralPath $ArtifactPath
$signaturePath = "$($artifact.FullName).sig"
if (-not (Test-Path -LiteralPath $signaturePath -PathType Leaf)) {
    throw "Missing Tauri signature: $signaturePath"
}

$safeVersion = $Version.TrimStart('v')
$extension = if ($artifact.Name.EndsWith('.nsis.zip')) { '.nsis.zip' } else { $artifact.Extension }
$releaseName = "ltk-manager_${safeVersion}_windows-x86_64$extension"
$signature = (Get-Content -Raw -LiteralPath $signaturePath).Trim()
if ([string]::IsNullOrWhiteSpace($signature)) {
    throw 'The Tauri signature file is empty.'
}

$staging = Join-Path ([System.IO.Path]::GetTempPath()) ("ltk-update-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $staging | Out-Null
$resolvedStaging = [System.IO.Path]::GetFullPath($staging)
$resolvedTemp = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
if (-not $resolvedStaging.StartsWith($resolvedTemp, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Refusing to use a staging directory outside the system temporary directory.'
}
try {
    $stagedArtifact = Join-Path $staging $releaseName
    Copy-Item -LiteralPath $artifact.FullName -Destination $stagedArtifact

    $manifest = [ordered]@{
        version = $safeVersion
        notes = $Notes
        pub_date = [DateTimeOffset]::UtcNow.ToString('o')
        platforms = [ordered]@{
            'windows-x86_64' = [ordered]@{
                artifact = $releaseName
                signature = $signature
            }
        }
    }
    $manifestPath = Join-Path $staging 'latest.json'
    [System.IO.File]::WriteAllText(
        $manifestPath,
        ($manifest | ConvertTo-Json -Depth 6),
        [System.Text.UTF8Encoding]::new($false)
    )

    # Upload immutable artifact first. The manifest is moved last so clients can never observe a
    # release whose file is not yet available.
    ssh $VpsHost "mkdir -p '$RemoteDirectory'"
    scp $stagedArtifact "${VpsHost}:${RemoteDirectory}/${releaseName}.uploading"
    ssh $VpsHost "mv '${RemoteDirectory}/${releaseName}.uploading' '${RemoteDirectory}/${releaseName}'"
    scp $manifestPath "${VpsHost}:${RemoteDirectory}/latest.json.uploading"
    ssh $VpsHost "mv '${RemoteDirectory}/latest.json.uploading' '${RemoteDirectory}/latest.json'"

    Write-Host "Published signed update $safeVersion to $PublicBaseUrl"
}
finally {
    if (
        (Test-Path -LiteralPath $resolvedStaging) -and
        $resolvedStaging.StartsWith($resolvedTemp, [StringComparison]::OrdinalIgnoreCase)
    ) {
        Remove-Item -LiteralPath $resolvedStaging -Recurse -Force
    }
}
