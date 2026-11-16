@echo off
setlocal
chcp 65001 >nul
cd /d "%~dp0"

where cargo >nul 2>&1
if errorlevel 1 (
    echo [错误] 未找到 cargo，请先安装 Rust 工具链：https://rustup.rs
    exit /b 1
)

echo [1/2] 编译 Release（前端资源编译进 exe，不生成 _internal）...
cargo build --release --manifest-path src-tauri\Cargo.toml
if errorlevel 1 (
    echo [错误] 编译失败
    exit /b 1
)

echo [2/2] 复制到项目根目录...
copy /Y "target\release\toymdict.exe" "ToyMDict.exe" >nul
if errorlevel 1 (
    echo [错误] 未找到 target\release\toymdict.exe
    exit /b 1
)

echo.
echo 完成：ToyMDict.exe（单文件，免安装）
echo 请把 custom.css 与 variants.json 复制到 ToyMDict.exe 同一目录再运行。
echo 首次运行会在同目录生成 dict_groups.json（配置，与 Python 版通用）和 toymdict.log（日志）。
endlocal
