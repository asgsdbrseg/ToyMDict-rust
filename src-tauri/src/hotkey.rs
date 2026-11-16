//! 系统级全局快捷键：双击 Ctrl 取词（对齐 Python 版 services/global_hotkey.py）。
//!
//! 工作流程：
//!   1. 用 `WH_KEYBOARD_LL` 低级键盘钩子在后台监听（不抢焦点）；
//!   2. 400ms 内两次「按下-松开」Ctrl 判定为取词手势；
//!   3. 一律模拟 Ctrl+C（带扫描码）把选中文字送入系统剪贴板，读取后还原
//!      用户原有的剪贴板文本 —— 与 Python 版 `global_hotkey.py` 完全一致；
//!      实测「同时启动 Python 版时取词正常」正是因为 Python 版始终走剪贴板；
//!   4. 应用内且剪贴板确实没被更新时，才回退到 `captureInAppSelection`，
//!      由前端读主文档 / 词条 iframe 的选区；
//!   5. 通过 `externalSearchFromHotkey()` 写入搜索框并立即搜索；前端主文档
//!      与词条 iframe 也会自行监听双击 Ctrl，两条通道用时间戳去重。
//!      外部窗口取词时才把窗口带到前台（应用内置前会清掉 WebView2 选区）。
//!
//! 只在 Windows 编译实现；其它平台 `start()` 直接返回 false，不影响启动。
//! 钩子回调里只做无锁的原子判断并 spawn 工作线程，避免阻塞系统钩子链
//! （Windows 对超时钩子会直接卸载）。

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::AppHandle;

// 以下各项只在 Windows 实现里用到；cfg 标注后 Linux 侧单元测试也能零警告编译。
#[cfg(windows)]
use std::sync::atomic::{AtomicPtr, AtomicU64};
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(windows)]
use tauri::Manager;

/// 运行时开关，默认常开（与 Python 版 `enabled=True` 一致）。
static ENABLED: AtomicBool = AtomicBool::new(true);

/// 两次 Ctrl 的间隔阈值（毫秒）。
#[cfg(windows)]
const DOUBLE_CLICK_MS: u64 = 400;
/// 单次取词最大字符数，避免误复制整篇文章塞进搜索框。
const MAX_CAPTURE_CHARS: usize = 200;

#[cfg(windows)]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 纯逻辑：给定上一次按下时间与当前时间，判定是否触发双击，并返回新的 last 值。
/// 抽成无副作用函数以便单元测试（不依赖 Win32 / 全局状态）。
fn evaluate_double_press(last_ms: u64, now: u64, interval_ms: u64) -> (bool, u64) {
    // 先判断 now >= last_ms：系统时钟被往回调时不能把它当成「刚按过第二次」，
    // 否则相减下溢（saturating_sub 得 0）会误触发一次取词。
    if last_ms != 0 && now >= last_ms && now - last_ms <= interval_ms {
        (true, 0)
    } else {
        (false, now)
    }
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(value: bool) {
    ENABLED.store(value, Ordering::Relaxed);
}

// ------------------------------------------------------------------ Windows

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{
        GetLastError, GlobalFree, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM,
    };
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, MapVirtualKeyW, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VK_CONTROL, VK_LCONTROL,
        VK_MENU, VK_RCONTROL,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW,
        GetWindowThreadProcessId, SetForegroundWindow, SetWindowsHookExW, ShowWindow,
        TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, SW_RESTORE,
        WH_KEYBOARD_LL, WM_KEYUP, WM_SYSKEYUP,
    };

    static APP: OnceLock<AppHandle> = OnceLock::new();
    static HOOK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static LAST_CTRL_MS: AtomicU64 = AtomicU64::new(0);
    static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
    static CAPTURING: AtomicBool = AtomicBool::new(false);
    static RUNNING: AtomicBool = AtomicBool::new(false);

    const VK_C: u8 = b'C';

    fn log_line(msg: &str) {
        use std::sync::Arc;
        if let Some(app) = APP.get() {
            if let Some(state) = app.try_state::<Arc<crate::state::AppState>>() {
                state.log(msg);
            }
        }
    }

    /// 前台窗口是否属于本进程（含主窗口、词条子窗口、DevTools 窗口）。
    /// 用进程 ID 而不是逐个比对 HWND，能覆盖 WebView2 的子窗口层级。
    fn foreground_is_own_process() -> bool {
        unsafe {
            let fg = GetForegroundWindow();
            if fg.is_null() {
                return false;
            }
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(fg, &mut pid);
            pid != 0 && pid == std::process::id()
        }
    }

    /// keybd_event 的 bScan。Chromium / WebView2 依赖扫描码识别快捷键，
    /// 传 0 的注入按键常被直接忽略，这也是「应用内选词无效」的根因之一。
    fn scan_code(vk: u8) -> u8 {
        unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u8 }
    }

    pub fn start(app: AppHandle) -> bool {
        let _ = APP.set(app);
        std::thread::Builder::new()
            .name("toymdict-global-hotkey".into())
            .spawn(|| unsafe { install_hook() })
            .is_ok()
    }

    unsafe fn install_hook() {
        // WH_KEYBOARD_LL 的回调不注入其它进程，hmod 传 NULL 即可（pynput、
        // rdev 等成熟实现都这么做）。之前传 EXE 模块句柄，在部分 Windows
        // 版本上会让全局钩子静默失效，连外部窗口取词也一起失灵。
        // 线程 ID 传 0 表示全局钩子。
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0);
        if hook.is_null() {
            log_line(&format!(
                "安装全局键盘钩子失败（GetLastError={}），双击 Ctrl 取词不可用",
                GetLastError()
            ));
            return;
        }
        HOOK.store(hook, Ordering::SeqCst);
        log_line("已启用系统级全局快捷键：双击 Ctrl 取词");

        // 低级钩子要求安装线程持有消息循环。GetMessageW 返回 -1 只表示取消息
        // 出错，旧实现用 > 0 判断会在第一次出错时就退出循环、把钩子卸载掉。
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else if r == 0 {
                break; // WM_QUIT
            } else {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        UnhookWindowsHookEx(hook);
        HOOK.store(std::ptr::null_mut(), Ordering::SeqCst);
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
            let injected = (kb.flags & LLKHF_INJECTED) != 0;
            let vk = kb.vkCode as u16;
            let is_ctrl = vk == VK_CONTROL || vk == VK_LCONTROL || vk == VK_RCONTROL;
            let is_up = wparam == WM_KEYUP as usize || wparam == WM_SYSKEYUP as usize;

            // 忽略模拟按键（自身 Ctrl+C）、非 Ctrl 键与关闭状态。
            if is_ctrl && !injected && !CAPTURING.load(Ordering::SeqCst) && ENABLED.load(Ordering::Relaxed) {
                if is_up {
                    // 必须等到松开才允许下一次计数，过滤系统自动重复。
                    CTRL_DOWN.store(false, Ordering::Relaxed);
                } else if !CTRL_DOWN.swap(true, Ordering::Relaxed) {
                    let last = LAST_CTRL_MS.load(Ordering::Relaxed);
                    let (fire, next) = evaluate_double_press(last, now_ms(), DOUBLE_CLICK_MS);
                    LAST_CTRL_MS.store(next, Ordering::Relaxed);
                    if fire {
                        if let Some(app) = APP.get().cloned() {
                            std::thread::spawn(move || capture_and_search(app));
                        }
                    }
                }
            }
        }
        // hhk 在文档中标明被忽略，传空句柄即可。
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    fn capture_and_search(app: AppHandle) {
        if RUNNING.swap(true, Ordering::SeqCst) {
            return; // 上一次取词尚未结束，避免并发重入
        }

        // 无论焦点在哪个窗口，都先按 Python 版 `global_hotkey.py` 的做法模拟
        // Ctrl+C 读剪贴板。用户实测「同时启动 Python 版时取词正常」，正是因为
        // Python 版始终走剪贴板；本版之前在「前台窗口属于本进程」时短路到
        // captureInAppSelection，而那条前端通道在真实 WebView2 里没生效。
        let own = foreground_is_own_process();
        let old = read_clipboard();
        simulate_copy();
        let new = read_clipboard();
        // 还原用户原来的剪贴板；只在原本就是纯文本时还原，避免破坏图片/文件等内容。
        if let Some(ref text) = old {
            write_clipboard(text);
        }

        // 剪贴板确实被 Ctrl+C 更新 => 注入按键生效，应用内也走这条可靠路径。
        let changed = match (&new, &old) {
            (Some(n), Some(o)) => n != o && !n.trim().is_empty(),
            (Some(n), None) => !n.trim().is_empty(),
            _ => false,
        };
        if own && !changed {
            // 应用内且剪贴板没变化（WebView2 忽略注入按键 / 用户没选中），
            // 才回退到前端选区通道读主文档与词条 iframe 的选区。
            log_line("全局取词：应用内剪贴板未更新，回退前端选区通道");
            crate::call_js(&app, "captureInAppSelection", vec![]);
            RUNNING.store(false, Ordering::SeqCst);
            return;
        }

        // 对齐 Python 版：优先用复制后的内容；复制没改变剪贴板时回退到复制前。
        let candidate = match (&new, &old) {
            (Some(n), _) if !n.trim().is_empty() => n.clone(),
            (_, Some(o)) if !o.trim().is_empty() => o.clone(),
            _ => {
                RUNNING.store(false, Ordering::SeqCst);
                return;
            }
        };
        // 词典取词通常是一行短文本，取首行并限制长度，防止误复制整篇内容。
        let text: String = candidate
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(MAX_CAPTURE_CHARS)
            .collect();
        if text.is_empty() {
            RUNNING.store(false, Ordering::SeqCst);
            return;
        }

        // 应用内窗口本就在前台，置前反而会清掉 WebView2 选区；只有从别的
        // 窗口取词时才需要把结果窗口带到前台。
        if !own {
            bring_to_front(&app);
        }
        log_line(&format!("全局取词: {}", text));
        // 前端主文档 / 词条 iframe 也会本地监听双击 Ctrl 自行取词，这里用带
        // 去重的入口，避免双通道把同一个词搜索两次。
        crate::call_js(&app, "externalSearchFromHotkey", vec![serde_json::json!(text)]);
        RUNNING.store(false, Ordering::SeqCst);
    }

    /// 向当前聚焦窗口发送 Ctrl+C。全程不抢焦点，否则会复制到本程序自己的窗口。
    fn simulate_copy() {
        CAPTURING.store(true, Ordering::SeqCst);
        unsafe {
            let (ctrl, c) = (VK_CONTROL as u8, VK_C);
            let (ctrl_scan, c_scan) = (scan_code(ctrl), scan_code(c));
            keybd_event(ctrl, ctrl_scan, 0, 0);
            keybd_event(c, c_scan, 0, 0);
            keybd_event(c, c_scan, KEYEVENTF_KEYUP, 0);
            keybd_event(ctrl, ctrl_scan, KEYEVENTF_KEYUP, 0);
        }
        // 等目标应用把内容写进剪贴板。
        std::thread::sleep(Duration::from_millis(200));
        CAPTURING.store(false, Ordering::SeqCst);
        // 模拟按键的按下/松开被钩子忽略，手动复位状态，避免卡住后续检测。
        CTRL_DOWN.store(false, Ordering::Relaxed);
        LAST_CTRL_MS.store(0, Ordering::Relaxed);
    }

    fn open_clipboard() -> bool {
        for _ in 0..3 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        false
    }

    fn read_clipboard() -> Option<String> {
        if !open_clipboard() {
            return None;
        }
        let mut out = None;
        unsafe {
            let h = GetClipboardData(CF_UNICODETEXT as u32);
            if !h.is_null() {
                let p = GlobalLock(h) as *const u16;
                if !p.is_null() {
                    let mut len = 0usize;
                    while *p.add(len) != 0 && len < 1_000_000 {
                        len += 1;
                    }
                    out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
                    GlobalUnlock(h);
                }
            }
            CloseClipboard();
        }
        out
    }

    fn write_clipboard(text: &str) -> bool {
        if !open_clipboard() {
            return false;
        }
        let mut ok = false;
        unsafe {
            let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let h: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
            if !h.is_null() {
                let p = GlobalLock(h) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
                    GlobalUnlock(h);
                    if EmptyClipboard() != 0
                        && !SetClipboardData(CF_UNICODETEXT as u32, h).is_null()
                    {
                        ok = true;
                    }
                }
                // SetClipboardData 成功后内存归剪贴板所有，不能 GlobalFree。
                if !ok {
                    GlobalFree(h);
                }
            }
            CloseClipboard();
        }
        ok
    }

    fn bring_to_front(app: &AppHandle) {
        if let Some(w) = app.get_webview_window("main") {
            unsafe {
                // Windows 前台锁定：轻按一次 ALT 让本进程获得设置前台窗口的资格。
                let alt = VK_MENU as u8;
                let alt_scan = scan_code(alt);
                keybd_event(alt, alt_scan, 0, 0);
                keybd_event(alt, alt_scan, KEYEVENTF_KEYUP, 0);
                if let Ok(h) = w.hwnd() {
                    // tauri 用 `windows` crate 的 HWND(pub *mut c_void)，
                    // windows-sys 的 HWND 就是 *mut c_void，取 .0 即可互转。
                    let hwnd: HWND = h.0;
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
            }
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

#[cfg(windows)]
pub fn start(app: AppHandle) -> bool {
    win::start(app)
}

#[cfg(not(windows))]
pub fn start(_app: AppHandle) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 双击判定_间隔内触发并清零() {
        assert_eq!(evaluate_double_press(0, 1000, 400), (false, 1000));
        assert_eq!(evaluate_double_press(1000, 1300, 400), (true, 0));
        assert_eq!(evaluate_double_press(1000, 1400, 400), (true, 0));
    }

    #[test]
    fn 双击判定_超时不触发() {
        assert_eq!(evaluate_double_press(1000, 1401, 400), (false, 1401));
        assert_eq!(evaluate_double_press(1000, 5000, 400), (false, 5000));
    }

    #[test]
    fn 双击判定_触发后必须重新计时() {
        let (fire, last) = evaluate_double_press(1000, 1200, 400);
        assert!(fire);
        assert_eq!(evaluate_double_press(last, 1300, 400), (false, 1300));
        assert_eq!(evaluate_double_press(1300, 1500, 400), (true, 0));
    }

    #[test]
    fn 双击判定_时间回拨不崩溃() {
        // 系统时钟被调整时不能因减法下溢 panic。
        assert_eq!(evaluate_double_press(5000, 100, 400), (false, 100));
    }

    #[test]
    fn 开关默认开启且可切换() {
        let old = is_enabled();
        set_enabled(false);
        assert!(!is_enabled());
        set_enabled(true);
        assert!(is_enabled());
        set_enabled(old);
    }

    #[test]
    fn 取词文本清洗_首行与长度限制() {
        let raw = "  南朝宋\n第二行  ";
        let text: String = raw
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(MAX_CAPTURE_CHARS)
            .collect();
        assert_eq!(text, "南朝宋");
        let long: String = "字".repeat(500);
        assert_eq!(long.chars().take(MAX_CAPTURE_CHARS).count(), MAX_CAPTURE_CHARS);
    }
}
