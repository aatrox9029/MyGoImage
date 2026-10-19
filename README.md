# MyGoImage

[繁體中文](README.zh-TW.md) | English

A lightweight Windows image viewer built with Rust and Slint.
Designed for photography and large image files, with a focus on smooth browsing.

## Features

- View JPEG, PNG, WebP, BMP, TIFF, GIF and PSD images (the first frame of GIFs and the first page of TIFFs).
- Quickly copy files, safely delete images and view multiple images.
- Switch instantly between English and Traditional Chinese.
- Preloading helps keep browsing large images smooth.

## Performance

Optimized for common image formats.
Scaled JPEG decoding, SIMD CPU resizing, background loading and adjacent-image preloading reduce processing during browsing. SQLite indexing, bounded RAM caches and a persistent thumbnail cache support large folders. Memory and image-size limits are configurable. PSD decoding uses small blocks or reusable row buffers to reduce temporary memory. Actual speed depends on image size, storage and hardware.

PSD viewing uses the saved composite image and supports 8/16-bit RGB and grayscale with Raw, PackBits RLE, ZIP and ZIP prediction compression, including RGB/grayscale ICC conversion. 16-bit images are displayed as 8-bit. Save with Photoshop's Maximize Compatibility enabled. PSB, 32-bit, CMYK, Lab, indexed color and files without a valid composite image are unsupported.

## Download and Use

Download from the Releases page and double-click to launch.

## Build (for Developers)

Requires Rust stable and Visual Studio Build Tools with the C++ workload and Windows SDK for Windows x64.

```powershell
cargo build --locked --release -p mygoimage
.\target\release\mygoimage.exe "D:\Photos"
```

Checks: `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, and `cargo test --locked --workspace -- --test-threads=1`.

For a portable ZIP, run `python scripts/collect-licenses.py`, `python scripts/fetch-license-texts.py`, then `./scripts/package-windows.ps1`. Packaging uses the Rust version in `scripts/release-toolchain.json` and includes generated dependency license notices.

## License

The project code is licensed under [MIT](LICENSE). Dependencies retain their own licenses; Slint is used under its royalty-free desktop license. Portable packages include generated third-party license notices.
