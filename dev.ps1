param(
    [Parameter(Position=0)]
    [ValidateSet('test','check','lint','fmt','build','release','demo','loop-demo','web')]
    [string]$Action='test'
)
$ErrorActionPreference='Stop'
Push-Location $PSScriptRoot
try {
    $localCargo=Join-Path $PSScriptRoot '.tools\cargo\bin\cargo.exe'
    if (Test-Path -LiteralPath $localCargo) {
        $env:CARGO_HOME=Join-Path $PSScriptRoot '.tools\cargo'
        $env:RUSTUP_HOME=Join-Path $PSScriptRoot '.tools\rustup'
        $env:PATH="$env:CARGO_HOME\bin;$env:PATH"
    }
    $tmp=Join-Path $PSScriptRoot '.tools\tmp'
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    $env:TEMP=$tmp
    $env:TMP=$tmp
    switch ($Action) {
        'test' { cargo test --all-targets --all-features }
        'check' { cargo check --all-targets --all-features }
        'lint' { cargo clippy --all-targets --all-features -- -D warnings }
        'fmt' { cargo fmt --all --check }
        'build' { cargo build --bins }
        'release' { cargo build --release --bins }
        'web' {
            cargo build --features web --bins
            if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
            & (Join-Path $PSScriptRoot 'target\debug\ithink-web.exe')
        }
        'loop-demo' {
            cargo build --bins
            if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
            & (Join-Path $PSScriptRoot 'target\debug\ithink-loop-demo.exe')
        }
        'demo' {
            cargo build --bins
            if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
            & (Join-Path $PSScriptRoot 'target\debug\ithink-demo.exe')
        }
    }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Pop-Location
}
