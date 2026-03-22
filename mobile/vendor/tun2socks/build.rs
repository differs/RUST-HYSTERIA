use std::{env, path::PathBuf, process::Command};

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

fn configure_make(cmd: &mut Command, target: &str) {
    let cc_key = target_var("CC", target);
    let ar_key = target_var("AR", target);
    let linker_key = cargo_target_var("CARGO_TARGET", target) + "_LINKER";

    let cc = env::var(&cc_key).or_else(|_| env::var(&linker_key));
    if let Ok(cc) = cc {
        cmd.arg(format!("CC={cc}"));
        cmd.arg(format!("PP={cc}"));
        if target.contains("android") {
            cmd.arg("CFLAGS=-DFD_SET_DEFINED -DSOCKLEN_T_DEFINED");
        }
        if let Some(parent) = PathBuf::from(&cc).parent() {
            let strip = parent.join("llvm-strip");
            if strip.exists() {
                cmd.arg(format!("STRIP={}", strip.display()));
            }
        }
    }

    if let Ok(ar) = env::var(&ar_key) {
        cmd.arg(format!("AR={ar}"));
    }
}

fn run_make(args: &[&str], target: &str) {
    let mut cmd = Command::new("make");
    cmd.args(args).current_dir("impl");
    configure_make(&mut cmd, target);
    let status = cmd.status().expect("Failed to invoke make");
    assert!(
        status.success(),
        "make {:?} failed with status {:?}",
        args,
        status
    );
}

fn main() {
    let target = env::var("TARGET").expect("TARGET is not set");
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"));
    run_make(&["clean"], &target);
    run_make(&["static", "-j8"], &target);

    println!(
        "cargo:rustc-link-search=native={}",
        manifest_dir.join("impl/bin").display()
    );
    println!("cargo:rustc-link-lib=static=hev-socks5-tunnel");
    println!(
        "cargo:rustc-link-search=native={}",
        manifest_dir
            .join("impl/third-part/hev-task-system/bin")
            .display()
    );
    println!("cargo:rustc-link-lib=static=hev-task-system");
    println!(
        "cargo:rustc-link-search=native={}",
        manifest_dir.join("impl/third-part/lwip/bin").display()
    );
    println!("cargo:rustc-link-lib=static=lwip");
    println!(
        "cargo:rustc-link-search=native={}",
        manifest_dir.join("impl/third-part/yaml/bin").display()
    );
    println!("cargo:rustc-link-lib=static=yaml");
}
