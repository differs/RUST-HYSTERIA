use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MakeTools {
    pub cc: Option<String>,
    pub ar: Option<String>,
    pub strip: Option<String>,
}

pub fn resolve_make_tools(target: &str, env: &BTreeMap<String, String>) -> MakeTools {
    let cc_key = target_var("CC", target);
    let ar_key = target_var("AR", target);
    let linker_key = cargo_target_var("CARGO_TARGET", target) + "_LINKER";

    let cc = env
        .get(&cc_key)
        .cloned()
        .or_else(|| env.get(&linker_key).cloned())
        .or_else(|| resolve_rustc_linker(target, env));
    let cc = cc.or_else(|| {
        if target.contains("android") {
            resolve_android_target_linker(target, env)
        } else {
            None
        }
    });
    let cc = normalize_android_cc(target, env, cc);

    let ar = env
        .get(&ar_key)
        .cloned()
        .or_else(|| cc.as_deref().and_then(|cc| derive_llvm_tool(cc, "llvm-ar")))
        .or_else(|| resolve_android_llvm_tool("llvm-ar", env));

    let strip = cc
        .as_deref()
        .and_then(|cc| derive_llvm_tool(cc, "llvm-strip"))
        .or_else(|| resolve_android_llvm_tool("llvm-strip", env));

    MakeTools { cc, ar, strip }
}

fn normalize_android_cc(
    target: &str,
    env: &BTreeMap<String, String>,
    cc: Option<String>,
) -> Option<String> {
    let cc = cc?;
    if !target.contains("android") {
        return Some(cc);
    }

    let compiler_name = Path::new(&cc)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let is_generic_compiler = matches!(compiler_name, "cc" | "gcc" | "clang" | "clang++");
    if is_generic_compiler {
        return resolve_android_target_linker(target, env).or(Some(cc));
    }

    Some(cc)
}

fn resolve_rustc_linker(target: &str, env: &BTreeMap<String, String>) -> Option<String> {
    let linker = env.get("RUSTC_LINKER")?;
    if target.contains("android") {
        if let Some(android_linker) = resolve_android_target_linker(target, env) {
            return Some(android_linker);
        }
    }
    Some(linker.clone())
}

fn resolve_android_target_linker(target: &str, env: &BTreeMap<String, String>) -> Option<String> {
    let toolchain = resolve_android_toolchain_bin(env)?;
    let api_level = env
        .get("ANDROID_API_LEVEL")
        .map(String::as_str)
        .unwrap_or("21");
    let clang_name = match target {
        "aarch64-linux-android" => format!("aarch64-linux-android{api_level}-clang"),
        "armv7-linux-androideabi" => format!("armv7a-linux-androideabi{api_level}-clang"),
        "i686-linux-android" => format!("i686-linux-android{api_level}-clang"),
        "x86_64-linux-android" => format!("x86_64-linux-android{api_level}-clang"),
        _ => return None,
    };
    Some(toolchain.join(clang_name).display().to_string())
}

fn resolve_android_llvm_tool(tool: &str, env: &BTreeMap<String, String>) -> Option<String> {
    Some(resolve_android_toolchain_bin(env)?.join(tool).display().to_string())
}

fn resolve_android_toolchain_bin(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(ndk_home) = env
        .get("ANDROID_NDK_HOME")
        .or_else(|| env.get("ANDROID_NDK_ROOT"))
    {
        return Some(
            Path::new(ndk_home).join("toolchains/llvm/prebuilt/linux-x86_64/bin"),
        );
    }

    let rustflags = env
        .get("CARGO_ENCODED_RUSTFLAGS")
        .or_else(|| env.get("RUSTFLAGS"))?;
    for flag in split_rustflags(rustflags) {
        let sysroot = flag
            .split("--sysroot=")
            .nth(1)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(sysroot) = sysroot {
            let sysroot_path = Path::new(sysroot);
            if let Some(parent) = sysroot_path.parent() {
                return Some(parent.join("bin"));
            }
        }
    }

    None
}

fn split_rustflags(flags: &str) -> Vec<&str> {
    if flags.contains('\u{1f}') {
        flags.split('\u{1f}').filter(|part| !part.is_empty()).collect()
    } else {
        flags.split_whitespace().collect()
    }
}

fn derive_llvm_tool(cc: &str, tool_name: &str) -> Option<String> {
    let parent = PathBuf::from(cc).parent()?.to_path_buf();
    let tool = parent.join(tool_name);
    if tool.exists() {
        return Some(tool.display().to_string());
    }
    None
}

fn target_var(prefix: &str, target: &str) -> String {
    format!("{}_{}", prefix, target.replace('-', "_"))
}

fn cargo_target_var(prefix: &str, target: &str) -> String {
    format!(
        "{}_{}",
        prefix,
        target.to_ascii_uppercase().replace('-', "_")
    )
}
