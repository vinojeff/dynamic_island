//! Embeds `assets/app.ico` into the Windows executable.

fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/app.ico");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=app icon was not embedded: {error}");
        }
    }
    println!("cargo:rerun-if-changed=assets/app.ico");
}
