# 로컬 대조 시험용 — 이 저장소의 web/ 을 옆 폴더의 Zenyth-web 클론으로 잇는다(디렉터리 정션, 관리자 권한 불필요).
# CI 는 web/ 에 Zenyth-web 을 직접 받으므로 이 스크립트를 쓰지 않는다. web/ 은 .gitignore 에 있다.
#   pwsh -File scripts/link-web.ps1 [-Web <Zenyth-web 경로>]   (기본: ../Zenyth-web)
param([string]$Web = (Join-Path $PSScriptRoot "..\..\Zenyth-web"))
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$link = Join-Path $root "web"
if (-not (Test-Path (Join-Path $Web "package.json"))) {
  Write-Error "Zenyth-web 을 찾지 못했다: $Web — 옆에 클론하거나 -Web 으로 경로를 줄 것."
}
$target = (Resolve-Path $Web).Path
if (Test-Path $link) {
  $item = Get-Item $link -Force
  if ($item.LinkType -eq "Junction" -and $item.Target -contains $target) { Write-Host "이미 이어져 있다: web -> $target"; exit 0 }
  if ($item.LinkType) { $item.Delete() } else { Write-Error "web/ 이 정션이 아닌 실제 폴더다 — 직접 확인 뒤 지울 것: $link" }
}
New-Item -ItemType Junction -Path $link -Target $target | Out-Null
Write-Host "web -> $target"
if (-not (Test-Path (Join-Path $target "node_modules"))) { Write-Host "Zenyth-web 에서 pnpm install 을 먼저 돌릴 것." }
