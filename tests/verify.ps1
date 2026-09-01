param(
    [ValidateSet('Fast', 'Full')]
    [string]$Mode = 'Fast'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

function Assert-NativeSuccess([string]$Step) {
    if ($LASTEXITCODE -ne 0) {
        throw "$Step failed with exit code $LASTEXITCODE"
    }
}

Push-Location $root
try {
    cargo fmt --all -- --check
    Assert-NativeSuccess 'cargo fmt'
    cargo clippy --workspace --all-targets -- -D warnings
    Assert-NativeSuccess 'cargo clippy'
    cargo test --workspace
    Assert-NativeSuccess 'cargo test'
    Push-Location frontend
    try {
        npm run typecheck
        Assert-NativeSuccess 'frontend typecheck'
        npm run format:check
        Assert-NativeSuccess 'frontend format check'
        npm run lint
        Assert-NativeSuccess 'frontend lint'
        npm test
        Assert-NativeSuccess 'frontend unit tests'
        if ($Mode -eq 'Full') {
            npm run build
            Assert-NativeSuccess 'frontend build'
        }
    } finally { Pop-Location }
    if ($Mode -eq 'Full') {
        $buildCommit = (git rev-parse --short=12 HEAD).Trim()
        Assert-NativeSuccess 'release commit lookup'
        docker build --file docker/Dockerfile --target release-files `
            --build-arg "PROXY_TESTER_BUILD_COMMIT=$buildCommit" `
            --tag proxy-tester-release-verify:local .
        Assert-NativeSuccess 'Docker musl release build'
        docker compose -f docker/compose.yaml config | Out-Null
        Assert-NativeSuccess 'development compose validation'
        docker compose -f docker/compose.yaml -f docker/compose.managed-direct.yaml config | Out-Null
        Assert-NativeSuccess 'managed-direct compose validation'
        Push-Location frontend
        try {
            $vite = Start-Process -FilePath (Get-Command node).Source `
                -ArgumentList @('./node_modules/vite/bin/vite.js', '--host', '127.0.0.1', '--port', '18080', '--strictPort') `
                -WorkingDirectory (Get-Location) -WindowStyle Hidden -PassThru
            try {
                $ready = $false
                for ($attempt = 0; $attempt -lt 100; $attempt++) {
                    if ($vite.HasExited) { throw "Vite exited with code $($vite.ExitCode)" }
                    try {
                        $response = Invoke-WebRequest -UseBasicParsing http://127.0.0.1:18080 -TimeoutSec 1
                        if ($response.StatusCode -eq 200) { $ready = $true; break }
                    } catch { Start-Sleep -Milliseconds 200 }
                }
                if (-not $ready) { throw 'Vite did not become ready on port 18080' }
                npm run test:e2e
                Assert-NativeSuccess 'Playwright tests'
            } finally {
                if ($vite -and -not $vite.HasExited) { Stop-Process -Id $vite.Id -Force }
            }
        } finally { Pop-Location }
    }
} finally { Pop-Location }
