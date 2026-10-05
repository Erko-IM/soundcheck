# Installs soundcheck from the latest release, or updates it, in PowerShell:
#   irm https://raw.githubusercontent.com/Erko-IM/soundcheck/main/install.ps1 | iex
#
# Windows marks only what a browser or the like downloads as from the
# internet, so the installer this fetches runs without the SmartScreen
# warning. It installs for you alone, without asking for an administrator,
# into AppData\Local\soundcheck with a Start menu shortcut, closing
# soundcheck first if it's open. Settings, Apps removes it again.
& {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell's progress bar slows its downloads to a crawl.
    $ProgressPreference = 'SilentlyContinue'
    # A folder of its own each time, so an installer from an earlier run that
    # Windows still holds open never stands in the way of this one.
    $dir = Join-Path ([IO.Path]::GetTempPath()) "soundcheck-$([guid]::NewGuid())"
    New-Item -ItemType Directory -Path $dir | Out-Null
    $setup = Join-Path $dir 'soundcheck-setup.exe'
    try {
        Invoke-WebRequest 'https://github.com/Erko-IM/soundcheck/releases/latest/download/soundcheck-setup.exe' -OutFile $setup -UseBasicParsing
        $run = Start-Process $setup -ArgumentList '/S' -Wait -PassThru
        if ($run.ExitCode -ne 0) {
            throw "soundcheck's installer failed with exit code $($run.ExitCode)"
        }
        Write-Host "installed soundcheck, it's in the Start menu"
    } finally {
        # Windows' virus scanner and its check on installers keep a finished
        # installer open for a moment. Still held after ten seconds, it stays
        # in the temp folder rather than fail an install that worked.
        foreach ($attempt in 1..20) {
            try {
                Remove-Item $dir -Recurse -Force
                break
            } catch {
                Start-Sleep -Milliseconds 500
            }
        }
    }
}
