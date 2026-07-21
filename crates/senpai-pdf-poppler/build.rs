fn main() {
    let poppler = pkg_config::probe_library("poppler-cpp")
        .expect("poppler-cpp not found via pkg-config; install libpoppler-cpp-dev");

    let mut build = cxx_build::bridge("src/ffi.rs");
    build
        .file("src/shim.cpp")
        .include("src")
        .std("c++17");

    for path in &poppler.include_paths {
        build.include(path);
    }

    build.compile("senpai_pdf_poppler_shim");

    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/shim.h");
    println!("cargo:rerun-if-changed=src/shim.cpp");
}
