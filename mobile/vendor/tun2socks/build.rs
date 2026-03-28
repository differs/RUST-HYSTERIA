use std::{collections::BTreeMap, env, path::PathBuf, process::Command};

mod toolchain;

fn configure_make(cmd: &mut Command, target: &str) {
    let env_map = env::vars().collect::<BTreeMap<_, _>>();
    let tools = toolchain::resolve_make_tools(target, &env_map);

    if let Some(cc) = tools.cc {
        cmd.arg(format!("CC={cc}"));
        cmd.arg(format!("PP={cc}"));
        if target.contains("android") {
            cmd.arg("CFLAGS=-DFD_SET_DEFINED -DSOCKLEN_T_DEFINED");
        }
    }

    if let Some(ar) = tools.ar {
        cmd.arg(format!("AR={ar}"));
    }

    if let Some(strip) = tools.strip {
        cmd.arg(format!("STRIP={strip}"));
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
