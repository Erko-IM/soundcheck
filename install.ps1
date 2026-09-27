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
    $setup = Join-Path ([IO.Path]::GetTempPath()) 'soundcheck-setup.exe'
    Invoke-WebRequest 'https://github.com/Erko-IM/soundcheck/releases/latest/download/soundcheck-setup.exe' -OutFile $setup -UseBasicParsing
    try {
        $run = Start-Process $setup -ArgumentList '/S' -Wait -PassThru
    } finally {
        Remove-Item $setup
    }
    if ($run.ExitCode -ne 0) {
        throw "soundcheck's installer failed with exit code $($run.ExitCode)"
    }
    Write-Host "installed soundcheck, it's in the Start menu"
}
