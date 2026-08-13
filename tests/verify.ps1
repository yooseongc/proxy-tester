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
        npm test
        Assert-NativeSuccess 'frontend unit tests'
        if ($Mode -eq 'Full') {
            npm run build
            Assert-NativeSuccess 'frontend build'
        }
    } finally { Pop-Location }
    if ($Mode -eq 'Full') {
        cargo build --workspace --release --target x86_64-unknown-linux-musl
        Assert-NativeSuccess 'musl release build'
        docker compose config | Out-Null
        Assert-NativeSuccess 'development compose validation'
        docker compose -f compose.production.yaml config | Out-Null
        Assert-NativeSuccess 'production compose validation'
        Push-Location frontend
        try {
            npm run test:e2e
            Assert-NativeSuccess 'Playwright tests'
        } finally { Pop-Location }
    }
} finally { Pop-Location }
