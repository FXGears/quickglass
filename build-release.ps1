# Builds the release binary with a size-optimized standard library.
#
# Rebuilds std from source (nightly -Zbuild-std) with optimize_for_size and the
# immediate-abort panic strategy, which drops panic message formatting and
# unwinding support. Measured: 359,424 -> 227,328 bytes versus a stable build.
#
# Tradeoff: a panic aborts with no message. The app is a GUI-subsystem binary
# with no console, so a panic was never visible to users anyway.
#
# Needs: rustup toolchain nightly + component rust-src
#   rustup toolchain install nightly; rustup component add rust-src --toolchain nightly
#
# Day-to-day development still uses the stable `cargo build --release`.
#
# Output: target\x86_64-pc-windows-msvc\release\quickglass.exe

$ErrorActionPreference = 'Stop'
$env:RUSTFLAGS = '-Zunstable-options -Cpanic=immediate-abort'
try {
    cargo +nightly build --release --target x86_64-pc-windows-msvc `
        '-Zbuild-std=std,panic_abort' '-Zbuild-std-features=optimize_for_size'
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
}
$exe = Get-Item (Join-Path $PSScriptRoot 'target\x86_64-pc-windows-msvc\release\quickglass.exe')
"$($exe.FullName) ($($exe.Length) bytes)"
