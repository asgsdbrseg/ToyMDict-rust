#[allow(
    non_snake_case,
    non_upper_case_globals,
    non_camel_case_types,
    dead_code,
    clippy::all
)]
pub mod Microsoft {
    pub mod Web {
        pub mod WebView2 {
            pub mod Win32 {
                mod windows_core {
                    // MSVC：沿用上游的静态链接方式（WebView2LoaderStatic.lib）。
                    #[cfg(target_env = "msvc")]
                    macro_rules! link_webview2 {
                        ($library:literal $abi:literal fn $($function:tt)*) => (
                            #[link(name = "WebView2LoaderStatic", kind = "static")]
                            extern $abi {
                                pub fn $($function)*;
                            }
                        )
                    }

                    // ToyMDict 补丁（windows-gnu）：不产生 DLL 导入依赖，改为
                    // 从内嵌资源释放 WebView2Loader.dll 后动态解析函数地址。
                    #[cfg(not(target_env = "msvc"))]
                    macro_rules! link_webview2 {
                        ($library:literal $abi:literal fn $name:ident($($arg:ident : $ty:ty),* $(,)?) -> $ret:ty) => (
                            #[allow(non_snake_case)]
                            unsafe fn $name($($arg : $ty),*) -> $ret {
                                type FnTy = unsafe extern "system" fn($($ty),*) -> $ret;
                                let p = crate::gnu_loader::resolve(
                                    concat!(stringify!($name), "\0"));
                                if p.is_null() {
                                    return ::windows_core::HRESULT(0x80004005u32 as i32);
                                }
                                let f: FnTy = ::core::mem::transmute(p);
                                f($($arg),*)
                            }
                        )
                    }

                    pub(crate) use ::windows_core::*;
                    pub(crate) use link_webview2 as link;
                }

                include!("bindings.rs");
            }
        }
    }
}

#[cfg(all(windows, not(target_env = "msvc")))]
pub mod gnu_loader;

pub mod declared_interfaces;

#[cfg(test)]
mod test {
    use windows_core::w;

    use crate::Microsoft::Web::WebView2::Win32::*;

    #[test]
    fn compare_eq() {
        let mut result = 1;
        unsafe { CompareBrowserVersions(w!("1.0.0"), w!("1.0.0"), &mut result) }.unwrap();
        assert_eq!(0, result);
    }

    #[test]
    fn compare_lt() {
        let mut result = 0;
        unsafe { CompareBrowserVersions(w!("1.0.0"), w!("1.0.1"), &mut result) }.unwrap();
        assert_eq!(-1, result);
    }

    #[test]
    fn compare_gt() {
        let mut result = 0;
        unsafe { CompareBrowserVersions(w!("2.0.0"), w!("1.0.1"), &mut result) }.unwrap();
        assert_eq!(1, result);
    }
}
