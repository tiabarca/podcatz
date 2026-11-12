# PODCAZ - comprobacion del entorno de compilacion en Windows.
#   .\setup.ps1            solo comprueba
#   .\setup.ps1 -Install   instala con winget lo que falte
param([switch]$Install)

$ok = $true
function Have($cmd) { return [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }
function Step($name, $present, $wingetId, $extra) {
    if ($present) { Write-Host "[OK]    $name" -ForegroundColor Green; return }
    $script:ok = $false
    if ($Install -and (Have winget)) {
        Write-Host "[..]    Instalando $name ..." -ForegroundColor Yellow
        if ($extra) { winget install --id $wingetId -e --accept-source-agreements --accept-package-agreements --override $extra }
        else { winget install --id $wingetId -e --accept-source-agreements --accept-package-agreements }
    } else {
        Write-Host "[FALTA] $name   ->  winget install --id $wingetId -e" -ForegroundColor Red
    }
}

# 1. Rust
Step "Rust (rustup/cargo)" (Have cargo) "Rustlang.Rustup" $null

# 2. Compilador C/C++ de Microsoft (lo necesitan Rust, LAME y el SDK de ASIO)
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vc = $false
if (Test-Path $vswhere) {
    $p = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    $vc = [bool]$p
}
Step "Visual Studio Build Tools (C++)" $vc "Microsoft.VisualStudio.2022.BuildTools" "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --passive --wait"

# 3. LLVM / libclang (solo para ASIO)
$llvmBin = "C:\Program Files\LLVM\bin"
$clang = (Test-Path "$llvmBin\libclang.dll") -or ($env:LIBCLANG_PATH -and (Test-Path "$env:LIBCLANG_PATH\libclang.dll"))
Step "LLVM / libclang (necesario para ASIO)" $clang "LLVM.LLVM" $null
if ((Test-Path "$llvmBin\libclang.dll") -and -not $env:LIBCLANG_PATH) {
    [Environment]::SetEnvironmentVariable("LIBCLANG_PATH", $llvmBin, "User")
    Write-Host "[OK]    LIBCLANG_PATH = $llvmBin (variable de usuario creada)" -ForegroundColor Green
    Write-Host "        Cierra y vuelve a abrir VS Code para que la vea." -ForegroundColor Yellow
}

Write-Host ""
if ($ok) {
    Write-Host "Entorno listo. Compila con:  cargo run --release -p podcaz" -ForegroundColor Green
} elseif ($Install) {
    Write-Host "Instalacion lanzada. Cierra y abre VS Code y vuelve a ejecutar .\setup.ps1" -ForegroundColor Yellow
} else {
    Write-Host "Faltan componentes. Ejecuta  .\setup.ps1 -Install  o instalalos a mano." -ForegroundColor Yellow
    Write-Host "Sin LLVM puedes compilar sin ASIO:" -ForegroundColor Yellow
    Write-Host "  cargo run --release -p podcaz --no-default-features --features mp3"
}
