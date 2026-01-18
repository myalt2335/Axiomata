param(
    [switch]$NoReboot,
    [switch]$NoShutdown
)

$ErrorActionPreference = "Stop"

Write-Host "Building OS image..."
cargo run -q -p os

$UEFI_IMG = Get-ChildItem -Recurse -Filter uefi.img `
    "$PSScriptRoot\target\debug\build" |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1

if (-not $UEFI_IMG) {
    Write-Error "uefi.img not found in target directory."
    exit 1
}

$UEFI_IMG_PATH = $UEFI_IMG.FullName
Write-Host "UEFI image: $UEFI_IMG_PATH"

$FsImgDir = Join-Path $PSScriptRoot "build"
$FsImgPath = Join-Path $FsImgDir "fs.img"
$TargetFsSize = 512MB
if (-not (Test-Path $FsImgDir)) {
    New-Item -ItemType Directory -Force -Path $FsImgDir | Out-Null
}
if (-not (Test-Path $FsImgPath)) {
    Write-Host "Creating persistent filesystem image..."
    $fs = [System.IO.File]::Open($FsImgPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    $fs.SetLength($TargetFsSize)
    $fs.Close()
} else {
    $fsInfo = Get-Item $FsImgPath
    if ($fsInfo.Length -lt $TargetFsSize) {
        Write-Host "Resizing filesystem image..."
        $fs = [System.IO.File]::Open($FsImgPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
        $fs.SetLength($TargetFsSize)
        $fs.Close()
    }
}

$Qemu = "I intend for this to error. Where do you keep your qemu .exe? Put the link here."
$Ovmf = "Similarly to the line above, where do you keep your edk2-x86_64-code.fd?"

$Args = @(
    "-machine", "type=q35,i8042=on",
    "-m", "512M",
    "-drive", "if=pflash,format=raw,readonly=on,file=$Ovmf",
    "-drive", "format=raw,file=$UEFI_IMG_PATH",
    "-device", "piix3-ide,id=ide",
    "-drive", "if=none,id=fsdisk,format=raw,file=$FsImgPath",
    "-device", "ide-hd,drive=fsdisk,bus=ide.0,unit=0",
    "-rtc",   "base=localtime",
    "-accel", "tcg", # Please, If you can, CHANGE THIS. I myself am stuck with tcg because my qemu copy doesn't support anything better, but if you can use something else and it works, PLEASE USE IT.
    "-cpu",   "max"
)

if ($NoReboot) {
    $Args += "-no-reboot"
}

if ($NoShutdown) {
    $Args += "-no-shutdown"
}

& $Qemu @Args
