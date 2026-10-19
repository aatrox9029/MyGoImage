# MyGoImage

[繁體中文](README.zh-TW.md) | English

A lightweight Windows image viewer built with Rust and Slint.

## Features

- Browse folders and SD cards; open images with drag and drop or the command line.
- JPEG, PNG, WebP, BMP, TIFF and GIF viewing (first frame/page for GIF/TIFF).
- Thumbnails, zoom, sorting, configurable shortcuts and multi-image viewing.
- View and copy EXIF and PNG text metadata, including ComfyUI prompt/workflow.
- Copy selected images and move unwanted images to the application's trash folder.
- Switch instantly between English and Traditional Chinese.

## Performance

Scaled JPEG decoding, SIMD CPU resizing, background loading and adjacent-image preloading reduce browsing work. SQLite indexing, bounded RAM caches and a persistent thumbnail cache support large folders. Memory and image-size limits are configurable; actual speed depends on image size, storage and hardware.

## Build

Requires Rust stable and Visual Studio Build Tools with the C++ workload and Windows SDK (Windows x64).

```powershell
cargo build --locked --release -p mygoimage
.\target\release\mygoimage.exe "D:\Photos"
```

Checks: `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, and `cargo test --locked --workspace -- --test-threads=1`.

For a portable ZIP, run `python scripts/collect-licenses.py`, `python scripts/fetch-license-texts.py`, then `./scripts/package-windows.ps1`. Packaging uses the Rust version in `scripts/release-toolchain.json` and includes generated dependency license notices.

## License

Project code: [MIT](LICENSE). Dependencies retain their own licenses; Slint is used under its royalty-free desktop license. Generated third-party notices accompany portable packages.
