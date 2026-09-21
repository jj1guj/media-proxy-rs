use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=IMAGEMAGICK_PREFIX");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    if let Some(prefix) = env::var_os("IMAGEMAGICK_PREFIX") {
        probe_static(PathBuf::from(prefix).join("lib/pkgconfig"));
        return;
    }

    let workspace_prefix = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("imagemagick_dep must be inside the workspace")
        .join("target/imagemagick/lib/pkgconfig");
    if workspace_prefix.join("MagickWand.pc").is_file() {
        probe_static(workspace_prefix);
        return;
    }

    if pkg_config::Config::new().probe("MagickWand").is_ok() {
        return;
    }

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        for prefix in [
            "/opt/homebrew/opt/imagemagick",
            "/usr/local/opt/imagemagick",
        ] {
            let lib_dir = Path::new(prefix).join("lib");
            if lib_dir.join("libMagickWand-7.Q16HDRI.dylib").exists() {
                println!("cargo:rustc-link-search=native={}", lib_dir.display());
                println!("cargo:rustc-link-lib=dylib=MagickWand-7.Q16HDRI");
                println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
                return;
            }
        }
    }

    panic!("MagickWand development files were not found");
}

fn probe_static(pkg_config_path: PathBuf) {
    env::set_var("PKG_CONFIG_PATH", pkg_config_path);
    pkg_config::Config::new()
        .statik(true)
        .probe("MagickWand")
        .expect("static MagickWand is required");
}
