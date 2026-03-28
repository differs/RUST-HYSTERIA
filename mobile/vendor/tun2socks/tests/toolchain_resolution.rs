use std::collections::BTreeMap;

#[path = "../toolchain.rs"]
mod toolchain;

fn env_map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn falls_back_to_android_ndk_when_rustc_linker_is_a_wrapper() {
    let env = env_map(&[
        ("ANDROID_NDK_HOME", "/opt/android/ndk/28.2.13676358"),
        ("ANDROID_API_LEVEL", "28"),
        ("RUSTC_LINKER", "/home/de/.cargo/bin/dx"),
    ]);

    let tools = toolchain::resolve_make_tools("x86_64-linux-android", &env);

    assert_eq!(
        tools.cc.as_deref(),
        Some(
            "/opt/android/ndk/28.2.13676358/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android28-clang",
        )
    );
    assert_eq!(
        tools.ar.as_deref(),
        Some("/opt/android/ndk/28.2.13676358/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar")
    );
}

#[test]
fn resolves_android_toolchain_from_sysroot_rustflags() {
    let env = env_map(&[
        ("ANDROID_API_LEVEL", "28"),
        (
            "CARGO_ENCODED_RUSTFLAGS",
            "-Clink-arg=-Wl,--sysroot=/sdk/ndk/28.2.13676358/toolchains/llvm/prebuilt/linux-x86_64/sysroot",
        ),
    ]);

    let tools = toolchain::resolve_make_tools("x86_64-linux-android", &env);

    assert_eq!(
        tools.cc.as_deref(),
        Some(
            "/sdk/ndk/28.2.13676358/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android28-clang",
        )
    );
    assert_eq!(
        tools.ar.as_deref(),
        Some("/sdk/ndk/28.2.13676358/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar")
    );
}

#[test]
fn normalizes_generic_android_clang_to_target_specific_clang() {
    let env = env_map(&[
        ("ANDROID_API_LEVEL", "28"),
        ("CC_x86_64_linux_android", "/sdk/ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/clang"),
        (
            "CARGO_ENCODED_RUSTFLAGS",
            "-Clink-arg=-Wl,--sysroot=/sdk/ndk/28.2.13676358/toolchains/llvm/prebuilt/linux-x86_64/sysroot",
        ),
    ]);

    let tools = toolchain::resolve_make_tools("x86_64-linux-android", &env);

    assert_eq!(
        tools.cc.as_deref(),
        Some(
            "/sdk/ndk/28.2.13676358/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android28-clang",
        )
    );
}
