fn main() {
    if cfg!(feature = "tcmalloc") {
        let tcmlloc_lib_path = std::env::var("TCMALLOC_LIB_PATH").unwrap_or_else(|_| "/usr/lib".to_string());
        println!("cargo:rustc-link-search=native={}", tcmlloc_lib_path);
    }
    
}