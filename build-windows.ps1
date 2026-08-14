$ErrorActionPreference = "Stop"

rustup target add x86_64-pc-windows-msvc
cargo build --release --target x86_64-pc-windows-msvc

$binary = Join-Path $PSScriptRoot "target\x86_64-pc-windows-msvc\release\tempshare.exe"
Write-Host "Built $binary"
Write-Host "Keep the static directory beside the executable when distributing it."
