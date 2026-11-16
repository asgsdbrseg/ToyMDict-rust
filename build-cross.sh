#!/usr/bin/env bash
# Linux 上交叉编译 Windows x64 单文件 exe（无需 Windows SDK / MSVC）。
# 依赖：rustup target、mingw-w64、clang/lld（见 README）。
set -euo pipefail
cd "$(dirname "$0")"

export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export WINDRES=x86_64-w64-mingw32-windres

cargo build --target x86_64-pc-windows-gnu --release
cp -f target/x86_64-pc-windows-gnu/release/toymdict.exe ./ToyMDict.exe
echo "完成：./ToyMDict.exe（单文件）"
echo "请把 custom.css 与 variants.json 放在 exe 同目录。"
