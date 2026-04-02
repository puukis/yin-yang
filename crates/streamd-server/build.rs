use std::env;
use std::path::{Path, PathBuf};

struct NvencHeaderLocation {
    header: PathBuf,
    include_dir: PathBuf,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(have_nvenc)");
    println!("cargo:rerun-if-env-changed=NVENC_HEADER_PATH");
    println!("cargo:rerun-if-env-changed=NVENC_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=NVENC_LIB_DIR");
    println!("cargo:rerun-if-env-changed=CUDA_PATH");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if let Some(location) = locate_nvenc_header(&target_os) {
        generate_nvenc_bindings(&location.header, Some(location.include_dir.as_path()));
    } else {
        println!("cargo:warning=NVENC headers were not found for target {target_os}");
        println!(
            "cargo:warning=Set NVENC_HEADER_PATH, NVENC_INCLUDE_DIR, or restore the vendored nvEncodeAPI.h."
        );
    }
}

fn locate_nvenc_header(target_os: &str) -> Option<NvencHeaderLocation> {
    if let Some(path) = env::var_os("NVENC_HEADER_PATH").map(PathBuf::from) {
        return path.exists().then(|| NvencHeaderLocation {
            include_dir: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            header: path,
        });
    }

    if let Some(dir) = env::var_os("NVENC_INCLUDE_DIR").map(PathBuf::from) {
        if let Some(location) = header_in_include_dir(dir) {
            return Some(location);
        }
    }

    if let Some(location) = header_in_include_dir(vendored_include_dir()) {
        return Some(location);
    }

    match target_os {
        "linux" => header_in_include_dir(PathBuf::from("/usr/local/include")),
        "windows" => env::var_os("CUDA_PATH")
            .map(PathBuf::from)
            .and_then(|cuda| header_in_include_dir(cuda.join("include"))),
        _ => None,
    }
}

fn vendored_include_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("streamd-server crate should live under crates/")
        .join("third_party")
        .join("nv-codec-headers")
        .join("include")
}

fn header_in_include_dir(include_dir: PathBuf) -> Option<NvencHeaderLocation> {
    let direct = include_dir.join("nvEncodeAPI.h");
    if direct.exists() {
        return Some(NvencHeaderLocation {
            header: direct,
            include_dir,
        });
    }

    let nested = include_dir.join("ffnvcodec").join("nvEncodeAPI.h");
    nested.exists().then_some(NvencHeaderLocation {
        header: nested,
        include_dir,
    })
}

fn generate_nvenc_bindings(header: &Path, include_dir: Option<&Path>) {
    println!("cargo:rerun-if-changed={}", header.display());

    let mut builder = bindgen::Builder::default()
        .header(header.to_str().unwrap())
        .allowlist_type("NV_ENC_.*")
        .allowlist_type("NVENCSTATUS")
        .allowlist_type("NV_ENCODE_API_FUNCTION_LIST")
        .allowlist_function("NvEncodeAPICreateInstance")
        .allowlist_var("NV_ENC_.*")
        .prepend_enum_name(false)
        .derive_debug(true)
        .derive_default(true);

    if let Some(include_dir) = include_dir {
        builder = builder.clang_arg(format!("-I{}", include_dir.display()));
    }

    let bindings = builder
        .generate()
        .expect("Unable to generate NVENC bindings");
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("nvenc_bindings.rs");
    bindings
        .write_to_file(&out_path)
        .expect("Couldn't write NVENC bindings");

    println!("cargo:rustc-cfg=have_nvenc");
}
