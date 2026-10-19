#[path = "build/windows_icon.rs"]
mod windows_icon;

fn main() {
    windows_icon::embed();
    slint_build::compile("../../ui/main.slint").unwrap();
}
