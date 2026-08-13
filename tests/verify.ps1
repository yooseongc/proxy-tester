param(
    [ValidateSet('Fast', 'Full')]
    [string]$Mode = 'Fast'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

Push-Location $root
try {
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    Push-Location frontend
    try {
        npm run typecheck
        npm test
        if ($Mode -eq 'Full') { npm run build }
    } finally { Pop-Location }
    if ($Mode -eq 'Full') {
        cargo build --workspace --release --target x86_64-unknown-linux-musl
        docker compose config | Out-Null
        docker compose -f compose.production.yaml config | Out-Null
        Push-Location frontend
        try { npm run test:e2e } finally { Pop-Location }
    }
} finally { Pop-Location }
