fn main() {
    // Bundle the same viewer the server serves, so remote windows load locally.
    let src = "../../web/index.html";
    println!("cargo:rerun-if-changed={src}");
    std::fs::copy(src, "../ui/viewer.html").expect("copy web viewer into desktop/ui");
    tauri_build::build()
}
