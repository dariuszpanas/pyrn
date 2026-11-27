# Install the standalone pyrn release executable or an explicitly selected Python tool environment.
& {
    $ErrorActionPreference = 'Stop'
    $versionPattern = '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)'

    function Install-PythonPyrn {
        $package = 'pyrn'
        if ($version) { $package = "pyrn==$version" }
        $uvCommand = Get-Command uv -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($uvCommand) {
            $uvExecutable = $uvCommand.Source
        } else {
            if (!$HOME) { throw 'Installing uv requires a user profile; set USERPROFILE or install uv on PATH' }
            $installer = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName() + '.ps1')
            $previousInstall = $env:UV_UNMANAGED_INSTALL
            try {
                Invoke-WebRequest -UseBasicParsing -Uri 'https://astral.sh/uv/install.ps1' -OutFile $installer -TimeoutSec 120
                $env:UV_UNMANAGED_INSTALL = Join-Path $HOME '.local/bin'
                & ([scriptblock]::Create([IO.File]::ReadAllText($installer)))
                $uvExecutable = Join-Path $env:UV_UNMANAGED_INSTALL 'uv.exe'
                if (-not (Test-Path -LiteralPath $uvExecutable -PathType Leaf)) {
                    throw 'uv installation did not produce an executable'
                }
            } finally {
                $env:UV_UNMANAGED_INSTALL = $previousInstall
                Remove-Item -LiteralPath $installer -Force -ErrorAction SilentlyContinue
            }
        }
        & $uvExecutable --no-config tool install --python 3.12 $package
        if ($LASTEXITCODE -ne 0) { throw "uv tool install failed with exit code $LASTEXITCODE" }
        if ($env:PYRN_NO_MODIFY_PATH -ne '1') {
            & $uvExecutable --no-config tool update-shell
            if ($LASTEXITCODE -ne 0) { throw "uv tool update-shell failed with exit code $LASTEXITCODE" }
        }
        Write-Output 'pyrn installed. Open a new terminal and run pyrn --help.'
        Write-Output 'Update: uv tool upgrade pyrn. Remove: uv tool uninstall pyrn.'
    }

    function Get-ReleaseFile([string]$url, [string]$path, [int]$limit) {
        $deadline = [Threading.CancellationTokenSource]::new(120000)
        try {
            for ($redirect = 0; $redirect -le 5; $redirect++) {
                $response = $client.GetAsync($url, [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $deadline.Token).GetAwaiter().GetResult()
                try {
                    $status = [int]$response.StatusCode
                    if ($status -eq 404) { return $false }
                    if ($status -in 301, 302, 303, 307, 308) {
                        if ($loopback -or !$response.Headers.Location) { throw 'Unexpected release redirect' }
                        $next = [Uri]::new([Uri]$url, $response.Headers.Location)
                        if ($next.Scheme -ne 'https' -or $next.UserInfo) { throw 'Unsafe release redirect' }
                        $url = $next.AbsoluteUri
                        continue
                    }
                    if ($status -ne 200) { throw "Release server returned HTTP $status" }
                    if ($response.Content.Headers.ContentLength -gt $limit) { throw 'Release download exceeds size limit' }
                    $source = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
                    $file = [IO.File]::Open($path, 'CreateNew', 'Write', 'None')
                    try {
                        $buffer = [byte[]]::new(65536)
                        while (($count = $source.ReadAsync($buffer, 0, $buffer.Length, $deadline.Token).GetAwaiter().GetResult()) -gt 0) {
                            if ($file.Length + $count -gt $limit) { throw 'Release download exceeds size limit' }
                            $file.Write($buffer, 0, $count)
                        }
                    } finally { $file.Dispose() }
                    return $true
                } finally { $response.Dispose() }
            }
            throw 'Too many release redirects'
        } finally { $deadline.Dispose() }
    }

    function Resolve-LatestVersion([string]$releaseBase) {
        $deadline = [Threading.CancellationTokenSource]::new(30000)
        $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::Head, "$releaseBase/latest")
        try {
            $response = $client.SendAsync($request, [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $deadline.Token).GetAwaiter().GetResult()
            try {
                if ([int]$response.StatusCode -in 301, 302, 303, 307, 308 -and $response.Headers.Location) {
                    $resolved = [Uri]::new([Uri]"$releaseBase/latest", $response.Headers.Location)
                    if (($loopback -or $resolved.Scheme -eq 'https') -and !$resolved.UserInfo -and
                        $resolved.AbsoluteUri -cmatch ("\A" + [Regex]::Escape($releaseBase) + "/tag/v($versionPattern)\z")) {
                        return $Matches[1]
                    }
                }
            } finally { $response.Dispose() }
        } finally {
            $request.Dispose()
            $deadline.Dispose()
        }
        return $null
    }

    function Assert-RegularFile([string]$path) {
        $attrs = [IO.File]::GetAttributes($path)
        if ($attrs -band ([IO.FileAttributes]::ReparsePoint -bor [IO.FileAttributes]::Directory)) {
            throw 'Refusing to replace a linked file or directory'
        }
    }

    $method = $env:PYRN_INSTALL_METHOD
    if (!$method) { $method = 'auto' }
    $version = $env:PYRN_VERSION
    $dir = $env:PYRN_INSTALL_DIR
    for ($i = 0; $i -lt $args.Count; $i++) {
        switch -Regex ([string]$args[$i]) {
            '^--?native$' { $method = 'native' }
            '^--?python$' { $method = 'python' }
            '^--?version$' {
                if (++$i -ge $args.Count -or !$args[$i]) { throw '--version requires X.Y.Z' }
                $version = [string]$args[$i]
            }
            '^(-InstallDir|--install-dir)$' {
                if (++$i -ge $args.Count -or !$args[$i]) { throw '--install-dir requires an absolute path' }
                $dir = [string]$args[$i]
            }
            '^(-h|--?help)$' { 'Usage: install.ps1 [-Native|-Python] [-Version X.Y.Z] [-InstallDir C:\absolute\path]'; return }
            default { throw 'Unknown installer argument; see --help' }
        }
    }
    if ($version -and $version -cnotmatch "\A$versionPattern\z") { throw 'PYRN_VERSION must be a final X.Y.Z release' }
    if ($method -ceq 'python') { Install-PythonPyrn; return }
    if ($method -cnotin 'auto', 'native') { throw 'Unknown PYRN_INSTALL_METHOD' }
    if ([Environment]::OSVersion.Platform -ne 'Win32NT') { throw 'Use install.sh on Linux and macOS' }
    # The OS architecture can differ from the PowerShell process architecture under emulation.
    try { $machine = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() }
    catch { $machine = $env:PROCESSOR_ARCHITEW6432; if (!$machine) { $machine = $env:PROCESSOR_ARCHITECTURE } }
    switch ($machine) {
        { $_ -in 'X64', 'AMD64' } { $arch = 'x86_64' }
        'Arm64' { $arch = 'aarch64' }
        default { throw 'No native release for this architecture; select --python' }
    }
    $base = $env:PYRN_RELEASE_BASE_URL
    if (!$base) { $base = 'https://github.com/dariuszpanas/pyrn/releases' }
    $loopback = $base -cmatch '\Ahttp://127\.0\.0\.1:[0-9]+\z'
    if (!$loopback -and $base -cnotmatch '\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._/-]*)?\z') { throw 'Release URL requires HTTPS or explicit loopback' }
    if (!$dir) {
        if (!$HOME) { throw 'Set --install-dir or PYRN_INSTALL_DIR when a user profile is unavailable' }
        $dir = Join-Path $HOME '.local/bin'
    }
    if ($dir -notmatch '\A([A-Za-z]:[/\\]|\\\\[^\\]+\\[^\\]+)') { throw '--install-dir requires an absolute path' }
    $dir = [IO.Directory]::CreateDirectory($dir).FullName
    $tmp = [IO.Directory]::CreateDirectory((Join-Path $dir ('.pyrn-install.' + [Guid]::NewGuid()))).FullName
    $archive = Join-Path $tmp 'archive.zip'
    $stage = Join-Path $tmp 'pyrn.exe'
    $dest = Join-Path $dir 'pyrn.exe'
    $receipt = Join-Path $dir '.pyrn-native-receipt'
    $stagedReceipt = Join-Path $tmp 'receipt'
    $backup = Join-Path $tmp 'previous-receipt'
    $manifest = Join-Path $tmp 'manifest'
    $lock = Join-Path $dir '.pyrn-native-lock'
    $pendingLock = Join-Path $tmp 'lock'
    $locked = $false
    $client = $null
    try {
        Add-Type -AssemblyName System.Net.Http
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $handler = [Net.Http.HttpClientHandler]::new()
        $handler.AllowAutoRedirect = $false
        $client = [Net.Http.HttpClient]::new($handler)

        $release = $version
        if (!$release) {
            $release = Resolve-LatestVersion $base
            if (!$release) { throw 'Cannot identify the latest release; specify -Version X.Y.Z or select -Python' }
        }
        $asset = "pyrn-$release-$arch-pc-windows-msvc.zip"
        $manifestName = "pyrn-v$release-sha256.txt"
        if (!(Get-ReleaseFile "$base/download/v$release/$manifestName" $manifest 65536)) {
            throw "Native release checksum manifest unavailable for v$release; for a Python installation select -Python"
        }

        $raw = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($manifest))
        $expected = $null
        foreach ($rawLine in $raw.Split([char]10)) {
            $line = $rawLine.TrimEnd([char]13)
            if ($line.Length -eq 0) { continue }
            if ($line -cnotmatch '\A([0-9a-f]{64})  ([^\r\n]+)\z') { throw 'Invalid release checksum manifest' }
            if ($Matches[2] -ceq $asset) {
                if ($expected) { throw "Duplicate checksum entry for $asset" }
                $expected = $Matches[1]
            }
        }
        if (!$expected) { throw "Release checksum manifest missing entry for $asset" }

        if (!(Get-ReleaseFile "$base/download/v$release/$asset" $archive 134217728)) { throw "Native archive download failed ($asset)" }
        $actualArchive = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualArchive -cne $expected) { throw 'Native archive checksum mismatch' }

        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try {
            $entry = $zip.GetEntry('pyrn.exe')
            if (!$entry) { throw 'Release archive missing pyrn.exe' }
            $inStream = $entry.Open()
            $outStream = [IO.File]::Open($stage, 'CreateNew', 'Write', 'None')
            try { $inStream.CopyTo($outStream) }
            finally {
                $outStream.Dispose()
                $inStream.Dispose()
            }
        } finally { $zip.Dispose() }

        $identity = & $stage --version
        if ($LASTEXITCODE -ne 0 -or @($identity).Count -ne 1 -or $identity -cne "pyrn $release") { throw 'Native executable version mismatch' }
        $actualBin = (Get-FileHash -LiteralPath $stage -Algorithm SHA256).Hash.ToLowerInvariant()

        [void][IO.Directory]::CreateDirectory($pendingLock)
        [IO.Directory]::Move($pendingLock, $lock)
        $locked = $true

        $exists = Test-Path -LiteralPath $dest
        if ($exists -or (Test-Path -LiteralPath $receipt)) {
            if (!$exists -or !(Test-Path -LiteralPath $receipt)) { throw 'Existing executable belongs to another installer' }
            Assert-RegularFile $dest
            Assert-RegularFile $receipt
            if ((Get-Item -LiteralPath $receipt).Length -gt 256) { throw 'Invalid install receipt' }
            $record = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($receipt))
            if ($record -cnotmatch "\Apyrn-native-install-v1 $versionPattern\n([0-9a-f]{64})  pyrn\.exe\n\z") { throw 'Invalid install receipt' }
            if ((Get-FileHash -LiteralPath $dest -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Matches[4]) { throw 'Installed executable or receipt has changed' }
        }

        [IO.File]::WriteAllText($stagedReceipt, "pyrn-native-install-v1 $release`n$actualBin  pyrn.exe`n", [Text.Encoding]::ASCII)
        if ($exists) { [IO.File]::Replace($stagedReceipt, $receipt, $backup) }
        else { [IO.File]::Move($stagedReceipt, $receipt) }
        try {
            if ($exists) { [IO.File]::Replace($stage, $dest, (Join-Path $tmp 'previous.exe')) }
            else { [IO.File]::Move($stage, $dest) }
        } catch {
            [IO.File]::Delete($receipt)
            if ($exists) { [IO.File]::Move($backup, $receipt) }
            throw
        }

        Write-Output "Installed pyrn $release at $dest"
        if ($env:PYRN_NO_MODIFY_PATH -ne '1') {
            try {
                $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
                if ($dir -notin ($userPath -split ';')) {
                    [Environment]::SetEnvironmentVariable('Path', ("$dir;$userPath").TrimEnd(';'), 'User')
                    Write-Output 'Open a new terminal to use pyrn.'
                }
            } catch {
                [Console]::Error.WriteLine('Could not update your user PATH. Add the installation directory to PATH manually.')
                $quoted = [System.Management.Automation.Language.CodeGeneration]::EscapeSingleQuotedStringContent($dir)
                Write-Output "To use pyrn in this PowerShell session, run:`n  `$env:Path = '$quoted;' + `$env:Path"
            }
        }
    } finally {
        try {
            if ($client) { $client.Dispose() }
            foreach ($file in $manifest, $archive, $stage, $stagedReceipt, $backup, (Join-Path $tmp 'previous.exe')) { [IO.File]::Delete($file) }
            if ([IO.Directory]::Exists($pendingLock)) { [IO.Directory]::Delete($pendingLock) }
            [IO.Directory]::Delete($tmp)
        } finally {
            if ($locked) { [IO.Directory]::Delete($lock) }
        }
    }
} @args
