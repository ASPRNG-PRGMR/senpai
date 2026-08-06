fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("senpai.ico");
        if let Err(e) = res.compile() {
            eprintln!("warning: failed to embed Windows icon: {e}");
        }
    }
}
