$ErrorActionPreference = 'Stop'

$manifest = Get-Content Cargo.toml -Raw
$versionMatch = [regex]::Match($manifest, '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
  throw 'Could not read the zmux package version from Cargo.toml'
}
$zmuxVersion = $versionMatch.Groups[1].Value
$isTagRelease = $env:GITHUB_REF -like 'refs/tags/v*'

$signTool = Get-ChildItem `
  "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" |
  Sort-Object FullName |
  Select-Object -Last 1
if (-not $signTool) { throw 'Could not find signtool.exe in the Windows SDK' }

$signingAvailable = $env:WINDOWS_CERTIFICATE_BASE64 -and $env:WINDOWS_CERTIFICATE_PASSWORD
if ($isTagRelease -and -not $signingAvailable) {
  throw 'Tagged Windows releases require WINDOWS_CERTIFICATE_BASE64 and WINDOWS_CERTIFICATE_PASSWORD'
}

if ($isTagRelease) {
  $certificate = Join-Path $env:RUNNER_TEMP 'zmux-signing.pfx'
  [IO.File]::WriteAllBytes(
    $certificate,
    [Convert]::FromBase64String($env:WINDOWS_CERTIFICATE_BASE64)
  )
  foreach ($executable in @('target/release/zmux.exe', 'target/release/zmux-gui.exe')) {
    & $signTool.FullName sign `
      /fd SHA256 `
      /f $certificate `
      /p $env:WINDOWS_CERTIFICATE_PASSWORD `
      /tr http://timestamp.digicert.com `
      /td SHA256 `
      $executable
    if ($LASTEXITCODE -ne 0) { throw "Signing $executable failed" }
    & $signTool.FullName verify /pa /v $executable
    if ($LASTEXITCODE -ne 0) { throw "Verifying $executable failed" }
  }
  $msiName = 'zmux-windows-x86_64.msi'
} else {
  $msiName = 'zmux-windows-x86_64-unsigned.msi'
}

New-Item -ItemType Directory -Force -Path dist
wix build packaging/windows/zmux.wxs `
  -arch x64 `
  -d "ProductVersion=$zmuxVersion" `
  -d "ProjectDir=$PWD" `
  -d "SourceDir=$PWD\target\release" `
  -o "dist/$msiName"
if ($LASTEXITCODE -ne 0) { throw 'Building the MSI failed' }

$msi = Get-Item "dist/$msiName"
if ($msi.Length -eq 0) { throw 'WiX produced an empty MSI' }
if ($isTagRelease) {
  & $signTool.FullName sign `
    /fd SHA256 `
    /f $certificate `
    /p $env:WINDOWS_CERTIFICATE_PASSWORD `
    /tr http://timestamp.digicert.com `
    /td SHA256 `
    $msi.FullName
  if ($LASTEXITCODE -ne 0) { throw 'Signing the MSI failed' }
  & $signTool.FullName verify /pa /v $msi.FullName
  if ($LASTEXITCODE -ne 0) { throw 'Verifying the MSI failed' }
}

$dumpbinCommand = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
$dumpbinPath = if ($dumpbinCommand) { $dumpbinCommand.Source } else { $null }
if (-not $dumpbinPath) {
  $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
  if (Test-Path $vswhere) {
    $dumpbinPath = & $vswhere `
      -latest `
      -products '*' `
      -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
      -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' |
      Select-Object -Last 1
  }
}
if (-not $dumpbinPath) {
  $dumpbinPath = Get-ChildItem `
    "${env:ProgramFiles}\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" `
    -ErrorAction SilentlyContinue |
    Sort-Object FullName |
    Select-Object -Last 1 |
    ForEach-Object FullName
}
if (-not $dumpbinPath) { throw 'Could not find dumpbin.exe' }

foreach ($executable in @('target/release/zmux.exe', 'target/release/zmux-gui.exe')) {
  $dependencies = (& $dumpbinPath /DEPENDENTS $executable | Out-String)
  if ($LASTEXITCODE -ne 0) { throw "Inspecting $executable dependencies failed" }
  if ($dependencies -match '(?im)\b(?:vcruntime14|msvcp14|concrt14)[^\s]*\.dll\b') {
    throw "$executable still requires an unbundled VC++ runtime:`n$dependencies"
  }
}

$guiHeaders = (& $dumpbinPath /HEADERS target/release/zmux-gui.exe | Out-String)
if ($LASTEXITCODE -ne 0) { throw 'Inspecting zmux-gui.exe headers failed' }
if ($guiHeaders -notmatch '(?im)^\s*2\s+subsystem\s+\(Windows GUI\)\s*$') {
  throw "zmux-gui.exe is not a Windows GUI-subsystem executable:`n$guiHeaders"
}
$cliHeaders = (& $dumpbinPath /HEADERS target/release/zmux.exe | Out-String)
if ($LASTEXITCODE -ne 0) { throw 'Inspecting zmux.exe headers failed' }
if ($cliHeaders -notmatch '(?im)^\s*3\s+subsystem\s+\(Windows CUI\)\s*$') {
  throw "zmux.exe is not a Windows console-subsystem executable:`n$cliHeaders"
}

$decompiled = Join-Path $env:RUNNER_TEMP 'zmux-decompiled.wxs'
wix msi decompile $msi.FullName -o $decompiled
if ($LASTEXITCODE -ne 0) { throw 'Decompiling the MSI failed' }
if (-not (Select-String -Path $decompiled -SimpleMatch 'io.github.thinkter.zmux')) {
  throw 'The MSI does not contain zmux AppUserModelID metadata'
}
if (-not (Select-String -Path $decompiled -SimpleMatch 'ARPPRODUCTICON')) {
  throw 'The MSI does not contain Add/Remove Programs icon metadata'
}
if (-not (Select-String -Path $decompiled -SimpleMatch 'ZMuxIcon.ico')) {
  throw 'The MSI does not contain the zmux shortcut icon'
}

$install = Start-Process msiexec.exe `
  -ArgumentList @('/i', $msi.FullName, '/qn', '/norestart') `
  -Wait `
  -PassThru
if ($install.ExitCode -ne 0) { throw "MSI install failed with $($install.ExitCode)" }

$installRoot = Join-Path $env:LOCALAPPDATA 'Programs\zmux'
$installedExecutable = Join-Path $installRoot 'zmux.exe'
$installedGuiExecutable = Join-Path $installRoot 'zmux-gui.exe'
$installedLicense = Join-Path $installRoot 'LICENSE'
$installedShortcut = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\zmux.lnk'
foreach ($path in @($installedExecutable, $installedGuiExecutable, $installedLicense, $installedShortcut)) {
  if (-not (Test-Path $path)) { throw "MSI did not install expected path: $path" }
}
$shortcut = (New-Object -ComObject WScript.Shell).CreateShortcut($installedShortcut)
if ($shortcut.TargetPath -ne $installedGuiExecutable) {
  throw "Start Menu shortcut targets $($shortcut.TargetPath), expected $installedGuiExecutable"
}
Add-Type -AssemblyName System.Drawing
$associatedIcon = [System.Drawing.Icon]::ExtractAssociatedIcon($installedGuiExecutable)
if (-not $associatedIcon -or $associatedIcon.Width -lt 16 -or $associatedIcon.Height -lt 16) {
  throw 'Installed GUI executable does not expose the embedded zmux icon'
}

$stdout = Join-Path $env:RUNNER_TEMP 'zmux-windows-smoke.stdout.log'
$stderr = Join-Path $env:RUNNER_TEMP 'zmux-windows-smoke.stderr.log'
$application = Start-Process $installedGuiExecutable `
  -RedirectStandardOutput $stdout `
  -RedirectStandardError $stderr `
  -PassThru
Start-Sleep -Seconds 5
if ($application.HasExited) {
  Get-Content $stdout, $stderr -ErrorAction SilentlyContinue
  throw "Packaged Windows application exited during launch smoke test with $($application.ExitCode)"
}
Stop-Process -Id $application.Id -Force
