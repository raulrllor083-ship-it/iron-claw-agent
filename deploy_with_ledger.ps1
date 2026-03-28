# Blue Dragon — Ledger Deploy Script
# This script deploys the updated iron_claw_agent contract to mainnet
# Make sure your Ledger is connected with the NEAR app open before running

Write-Host ""
Write-Host "=========================================" -ForegroundColor Cyan
Write-Host "  BLUE DRAGON — Contract Deployment" -ForegroundColor Cyan
Write-Host "=========================================" -ForegroundColor Cyan
Write-Host ""
Write-Host "Deploying to: blue-dragon-agent.near" -ForegroundColor Yellow
Write-Host "WASM file: ./target/wasm32-unknown-unknown/release/iron_claw_agent.wasm" -ForegroundColor Yellow
Write-Host ""
Write-Host "Make sure your Ledger is:" -ForegroundColor Green
Write-Host "  1. Connected via USB" -ForegroundColor Green
Write-Host "  2. Unlocked" -ForegroundColor Green
Write-Host "  3. NEAR app is open" -ForegroundColor Green
Write-Host ""
Write-Host "Starting deployment..." -ForegroundColor Cyan
Write-Host ""

Set-Location "c:\Users\User\.gemini\antigravity\scratch\iron_claw_agent"

near contract deploy blue-dragon-agent.near use-file ./target/wasm32-unknown-unknown/release/iron_claw_agent.wasm without-init-call network-config mainnet sign-with-ledger send

if ($LASTEXITCODE -eq 0) {
    Write-Host ""
    Write-Host "=========================================" -ForegroundColor Green
    Write-Host "  DEPLOYMENT SUCCESSFUL!" -ForegroundColor Green
    Write-Host "=========================================" -ForegroundColor Green
} else {
    Write-Host ""
    Write-Host "=========================================" -ForegroundColor Red
    Write-Host "  DEPLOYMENT FAILED (exit code: $LASTEXITCODE)" -ForegroundColor Red
    Write-Host "=========================================" -ForegroundColor Red
}

Write-Host ""
Read-Host "Press Enter to close"
