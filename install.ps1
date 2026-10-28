# agentmail installer for Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/quinnjr/agentmail/main/install.ps1 | iex
#
# With options:
#
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/quinnjr/agentmail/main/install.ps1))) -Harness auto
#
# agentmail runs on Linux, so on Windows it lives inside WSL, next to the
# agents that use it: run Claude Code, opencode or kiro-cli in WSL too. This
# script runs install.sh in your default WSL distribution (or -Distro).

param(
    # Release to install, e.g. v0.1.0 (default: latest).
    [string]$Version = $(if ($env:AGENTMAIL_VERSION) { $env:AGENTMAIL_VERSION } else { 'latest' }),
    # Directory inside WSL for the binary (default: ~/.local/bin).
    [string]$BinDir = $env:AGENTMAIL_INSTALL_DIR,
    # Afterwards run `agentmail install --harness <name>` in WSL:
    # claude-code, opencode, kiro, or auto (each one on PATH in WSL).
    [string[]]$Harness = @(),
    # Afterwards run `agentmail install --service` (systemd in WSL).
    [switch]$Service,
    # Build with cargo inside WSL instead of downloading a binary.
    [switch]$FromSource,
    # WSL distribution to install into (default: the default distribution).
    [string]$Distro = $env:AGENTMAIL_WSL_DISTRO
)

function Install-AgentMail {
    $url = if ($env:AGENTMAIL_INSTALL_URL) { $env:AGENTMAIL_INSTALL_URL } else {
        'https://raw.githubusercontent.com/quinnjr/agentmail/main/install.sh'
    }

    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
        Write-Host "agentmail: error: WSL is not installed. agentmail runs inside WSL." -ForegroundColor Red
        Write-Host "Install it with 'wsl --install' (then restart), and run this again."
        return 1
    }

    $wsl = @()
    if ($Distro) { $wsl += @('-d', $Distro) }

    & wsl.exe @wsl -e true 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        $which = if ($Distro) { "the WSL distribution '$Distro'" } else { 'a default WSL distribution' }
        Write-Host "agentmail: error: could not start $which." -ForegroundColor Red
        Write-Host "Install one with 'wsl --install -d Ubuntu', or pick one with -Distro (see 'wsl -l')."
        return 1
    }

    $opts = @()
    if ($Version -and $Version -ne 'latest') { $opts += @('--version', $Version) }
    if ($BinDir) { $opts += @('--bin-dir', $BinDir) }
    foreach ($h in $Harness) { $opts += @('--harness', $h) }
    if ($Service) { $opts += '--service' }
    if ($FromSource) { $opts += '--from-source' }

    # Download install.sh inside WSL and run it there with the options. No
    # double quotes in it, since Windows PowerShell 5.1 does not escape them
    # for wsl.exe; an empty IFS keeps each option whole in the unquoted $@.
    $fetch = 't=$(mktemp) && { curl -fsSL $0 -o $t || wget -qO $t $0; } && { IFS=; set -f; sh $t $@; }; r=$?; rm -f $t; exit $r'
    & wsl.exe @wsl -e sh -c $fetch $url @opts | Out-Host
    if ($LASTEXITCODE -ne 0) {
        Write-Host "agentmail: installation failed (exit $LASTEXITCODE)." -ForegroundColor Red
        return 1
    }

    Write-Host ""
    Write-Host "agentmail is installed in WSL. Run your agents inside WSL (e.g. 'wsl' then 'claude')."
    return 0
}

$agentmailStatus = Install-AgentMail
# Leave nothing behind in the caller's session (iex runs in its scope).
Remove-Item Function:\Install-AgentMail
Remove-Variable Version, BinDir, Harness, Service, FromSource, Distro -ErrorAction SilentlyContinue
if ($PSCommandPath) {
    # Run as a file (powershell -File install.ps1): report through the exit code.
    exit $agentmailStatus
}
# Through iex or a script block, `exit` would close the caller's window.
$global:LASTEXITCODE = $agentmailStatus
Remove-Variable agentmailStatus
