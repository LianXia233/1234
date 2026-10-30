macro_rules! platforms {
    ($($($platform:expr);* => $module:ident),*) => {
        $(
            #[cfg(any($(target_os = $platform),*))]
            #[cfg_attr(not(any($(target_os = $platform),*)), allow(dead_code))]
            mod $module;

            #[cfg(any($(target_os = $platform),*))]
            pub use self::$module::*;

            // [acgdev patch] libc 0.2.18x 起把 ENOATTR 限定到 BSD/Apple 平台，
            // Linux 与 Android 上改用 ENODATA（两者取值相同，均为 61）。
            // 上游 xattr 0.2.3 未跟进，导致 zed-async-tar（gpui 依赖）编译失败。
            #[cfg(any($(target_os = $platform),*))]
            pub const ENOATTR: ::libc::c_int = {
                #[cfg(any(target_os = "linux", target_os = "android"))]
                {
                    ::libc::ENODATA
                }
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                {
                    ::libc::ENOATTR
                }
            };
        )*

        #[cfg(all(feature = "unsupported", not(any($($(target_os = $platform),*),*))))]
        #[cfg_attr(any($($(target_os = $platform),*),*), allow(dead_code))]
        mod unsupported;

        #[cfg(all(feature = "unsupported", not(any($($(target_os = $platform),*),*))))]
        pub use self::unsupported::*;
        #[cfg(all(feature = "unsupported", not(any($($(target_os = $platform),*),*))))]
        pub const ENOATTR: ::libc::c_int = 0;


        /// A constant indicating whether or not the target platform is supported.
        ///
        /// To make programmer's lives easier, this library builds on all platforms.
        /// However, all function calls on unsupported platforms will return
        /// `io::Error`s.
        ///
        /// Note: If you would like compilation to simply fail on unsupported platforms,
        /// turn of the `unsupported` feature.
        pub const SUPPORTED_PLATFORM: bool = cfg!(any($($(target_os = $platform),*),*));
    }
}

platforms! {
    "android"; "linux"; "macos" => linux_macos,
    "freebsd"; "netbsd" => bsd
}
