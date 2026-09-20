use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest_dir
        .parent()
        .expect("libvips_dep must be in the repository root");
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target"));
    let prefix = env::var_os("LIBVIPS_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| target_dir.join("libvips"));
    let lib_dir = prefix.join("lib");
    let library_name = match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => "libvips.dylib",
        Ok("linux") => "libvips.so",
        Ok(target) => panic!("unsupported target OS: {target}"),
        Err(_) => panic!("CARGO_CFG_TARGET_OS is not set"),
    };
    let build_script = repo_root.join("crossfiles/build-libvips.sh");

    let status = Command::new("bash")
        .arg(&build_script)
        .env("LIBVIPS_PREFIX", &prefix)
        .status()
        .expect("failed to start crossfiles/build-libvips.sh");
    if !status.success() || !lib_dir.join(library_name).exists() {
        panic!("failed to build local libvips at {}", prefix.display());
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
    if let Some(glib_lib_dir) = env::var_os("GLIB_LIB_DIR") {
        println!(
            "cargo:rustc-link-search=native={}",
            PathBuf::from(glib_lib_dir).display()
        );
    } else {
        add_pkg_config_search_paths(&["glib-2.0", "gobject-2.0"]);
    }

    println!("cargo:rerun-if-env-changed=LIBVIPS_PREFIX");
    println!("cargo:rerun-if-env-changed=GLIB_LIB_DIR");
    println!("cargo:rerun-if-changed={}", build_script.display());
    println!("cargo:rerun-if-changed={}", lib_dir.display());
}

fn add_pkg_config_search_paths(packages: &[&str]) {
    let output = Command::new("pkg-config")
        .arg("--libs-only-L")
        .args(packages)
        .output()
        .expect("pkg-config is required to locate GLib");

    if !output.status.success() {
        panic!(
            "pkg-config could not locate GLib: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    for flag in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        if let Some(path) = flag.strip_prefix("-L") {
            println!(
                "cargo:rustc-link-search=native={}",
                Path::new(path).display()
            );
        }
    }
}
