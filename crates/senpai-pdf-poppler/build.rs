fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    let mut build = cxx_build::bridge("src/ffi.rs");
    build.file("src/shim.cpp").include("src").std("c++17");

    if target == "x86_64-pc-windows-gnu" {
        // Cross-compiling: pkg-config only knows about the host's own
        // (Linux) libraries, so it's useless here. Instead we point
        // directly at the MSYS2-derived mingw64 sysroot fetched by
        // fetch-mingw64-sysroot.sh.
        let sysroot = std::env::var("SENPAI_MINGW_SYSROOT")
            .expect("set SENPAI_MINGW_SYSROOT to the mingw64-sysroot path (see fetch-mingw64-sysroot.sh)");

        // poppler-cpp's own headers.
        build.include(format!("{sysroot}/mingw64/include/poppler/cpp"));

        // Transitive dependency headers poppler-cpp's headers pull in.
        // glib is notoriously split across two include roots: one for
        // the generated platform config header, one for everything else.
        build.include(format!("{sysroot}/mingw64/include"));
        build.include(format!("{sysroot}/mingw64/include/glib-2.0"));
        build.include(format!("{sysroot}/mingw64/lib/glib-2.0/include"));
        build.include(format!("{sysroot}/mingw64/include/cairo"));
        build.include(format!("{sysroot}/mingw64/include/freetype2"));
        build.include(format!("{sysroot}/mingw64/include/harfbuzz"));
        build.include(format!("{sysroot}/mingw64/include/libpng16"));

        println!("cargo:rustc-link-search=native={sysroot}/mingw64/lib");

        // poppler-cpp itself. mingw's import library is named
        // libpoppler-cpp.dll.a -> linked as "poppler-cpp".
        println!("cargo:rustc-link-lib=dylib=poppler-cpp");

        // poppler-cpp's own transitive runtime deps. Listed explicitly
        // because mingw/PE linking (unlike some Unix linker setups)
        // generally will NOT resolve these automatically just because
        // poppler-cpp itself needs them -- each has to be named.
        println!("cargo:rustc-link-lib=dylib=poppler");
        println!("cargo:rustc-link-lib=dylib=cairo");
        println!("cargo:rustc-link-lib=dylib=freetype");
        println!("cargo:rustc-link-lib=dylib=fontconfig");
        println!("cargo:rustc-link-lib=dylib=glib-2.0");
        println!("cargo:rustc-link-lib=dylib=gobject-2.0");
        println!("cargo:rustc-link-lib=dylib=harfbuzz");
        println!("cargo:rustc-link-lib=dylib=png16");
        println!("cargo:rustc-link-lib=dylib=jpeg");
        println!("cargo:rustc-link-lib=dylib=tiff");
        println!("cargo:rustc-link-lib=dylib=lcms2");
        println!("cargo:rustc-link-lib=dylib=openjp2");
        println!("cargo:rustc-link-lib=dylib=z");
    } else {
        // Native (Linux) build: pkg-config works fine here as before.
        let poppler = pkg_config::probe_library("poppler-cpp")
            .expect("poppler-cpp not found via pkg-config; install libpoppler-cpp-dev");
        for path in &poppler.include_paths {
            build.include(path);
        }
    }

    build.compile("senpai_pdf_poppler_shim");

    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/shim.h");
    println!("cargo:rerun-if-changed=src/shim.cpp");
}
