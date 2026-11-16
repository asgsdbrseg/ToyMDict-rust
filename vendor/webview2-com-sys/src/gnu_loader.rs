//! ToyMDict 补丁：windows-gnu 目标的 WebView2Loader 动态加载。
//!
//! 上游 `webview2-com-sys` 只对 `target_env = "msvc"` 静态链接
//! `WebView2LoaderStatic.lib`；GNU 工具链无法链接该 MSVC C++ 静态库，
//! 只好退化成依赖外部 `WebView2Loader.dll`，这会破坏「单文件 exe」。
//!
//! 这里把 DLL（来自 WebView2 SDK，允许再分发）直接编进 exe，首次调用时
//! 释放到临时目录并 LoadLibrary/GetProcAddress。exe 的导入表因此不再出现
//! WebView2Loader.dll，也不需要 MSVC 运行库。

use std::ffi::c_void;
use std::sync::OnceLock;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(lib_file_name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, proc_name: *const i8) -> *mut c_void;
}

static MODULE: OnceLock<usize> = OnceLock::new();

fn module() -> *mut c_void {
    let addr = *MODULE.get_or_init(|| {
        let dir = std::env::temp_dir().join("toymdict-webview2-loader");
        let _ = std::fs::create_dir_all(&dir);
        // 按进程 ID 命名，避免多实例同时首次启动时互相覆盖到半截文件，
        // 也避免旧版本残留 DLL 被复用。
        let path = dir.join(format!("WebView2Loader-{}.dll", std::process::id()));
        let _ = std::fs::write(&path, include_bytes!("../x64/WebView2Loader.dll"));
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe { LoadLibraryW(wide.as_ptr()) as usize }
    });
    addr as *mut c_void
}

/// `name` 必须以 NUL 结尾（宏里用 concat! 拼接）。
pub fn resolve(name: &str) -> *mut c_void {
    let h = module();
    if h.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { GetProcAddress(h, name.as_ptr() as *const i8) }
}
