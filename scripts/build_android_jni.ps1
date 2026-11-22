param(
    [string]$NdkPath
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

if ([string]::IsNullOrWhiteSpace($NdkPath)) {
    if ($env:ANDROID_NDK_HOME) {
        $NdkPath = $env:ANDROID_NDK_HOME
    } else {
        $sdkRoot = if ($env:ANDROID_SDK_ROOT) {
            $env:ANDROID_SDK_ROOT
        } elseif ($env:ANDROID_HOME) {
            $env:ANDROID_HOME
        } else {
            Join-Path $env:LOCALAPPDATA "Android\Sdk"
        }
        $ndkRoot = Join-Path $sdkRoot "ndk"
        $latestNdk = Get-ChildItem -LiteralPath $ndkRoot -Directory -ErrorAction Stop |
            Sort-Object Name -Descending |
            Select-Object -First 1
        if (-not $latestNdk) {
            throw "No Android NDK was found under $ndkRoot. Set ANDROID_NDK_HOME or pass -NdkPath."
        }
        $NdkPath = $latestNdk.FullName
    }
}

$compilerBin = Join-Path $NdkPath "toolchains\llvm\prebuilt\windows-x86_64\bin"
if (-not (Test-Path -LiteralPath $compilerBin)) {
    throw "Android NDK Windows toolchain was not found at $compilerBin"
}

$jniManifest = Join-Path $repoRoot "zerolink-android\zerolink-jni\Cargo.toml"
$targets = @(
    @{
        Triple = "aarch64-linux-android"
        Abi = "arm64-v8a"
        LinkerEnv = "CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
        Linker = "aarch64-linux-android24-clang.cmd"
    },
    @{
        Triple = "x86_64-linux-android"
        Abi = "x86_64"
        LinkerEnv = "CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER"
        Linker = "x86_64-linux-android24-clang.cmd"
    }
)

Push-Location $repoRoot
try {
    foreach ($target in $targets) {
        $linkerPath = Join-Path $compilerBin $target.Linker
        if (-not (Test-Path -LiteralPath $linkerPath)) {
            throw "Android NDK linker was not found: $linkerPath"
        }
        Set-Item -Path "Env:$($target.LinkerEnv)" -Value $linkerPath

        Write-Host "Building Rust JNI library for $($target.Triple)..."
        cargo build --manifest-path $jniManifest --release --target $target.Triple --offline
        if ($LASTEXITCODE -ne 0) {
            throw "Cargo failed to build the JNI library for $($target.Triple)."
        }

        $builtLibrary = Join-Path $repoRoot "target\$($target.Triple)\release\libzerolink_jni.so"
        $packagedLibrary = Join-Path $repoRoot "zerolink-android\app\src\main\jniLibs\$($target.Abi)\libzerolink_jni.so"
        if (-not (Test-Path -LiteralPath $builtLibrary)) {
            throw "Cargo did not produce the expected library: $builtLibrary"
        }
        Copy-Item -LiteralPath $builtLibrary -Destination $packagedLibrary -Force
        Write-Host "Updated $packagedLibrary"
    }
} finally {
    Pop-Location
}
