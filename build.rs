fn main() {
    // Material draws outlined input fields that stand out in light and dark themes.
    println!("cargo:rerun-if-env-changed=SLINT_STYLE");
    let style = std::env::var("SLINT_STYLE").unwrap_or_else(|_| "material".into());
    let config = slint_build::CompilerConfiguration::new().with_style(style);
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("Failed to compile the Slint UI!");
}
