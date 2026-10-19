# MyGoImage

繁體中文 | [English](README.md)<br>
<img width="300" height="300" alt="icon" src="https://github.com/user-attachments/assets/6768accb-7d3c-409b-b906-b9a816f03b75" /><br>
以 Rust 與 Slint 開發的輕量 Windows 看圖程式。
專為攝影作品與大型影像檔案打造，著重於流暢的瀏覽體驗。

## 功能

- 支援 JPEG、PNG、WebP、BMP、TIFF、GIF、PSD（GIF 顯示首幀，TIFF 顯示首頁）。
- 快速複製檔案、安全刪除與多圖瀏覽。
- 英文與繁體中文即時切換。
- 透過預載入功能，提升大型圖片的瀏覽流暢度。

## 效能

針對常用圖片格式進行最佳化。
透過 JPEG 縮放解碼、SIMD CPU 縮圖、背景載入與相鄰圖片預載，降低瀏覽時的處理負擔。SQLite 索引、有限容量的 RAM 快取與持久化縮圖快取支援大型資料夾；記憶體及圖片尺寸上限可調整。PSD 使用小區塊或可重用的列緩衝區解碼，減少暫存記憶體。實際速度依圖片大小、儲存裝置與硬體而異。

PSD 顯示檔案中儲存的合成影像，支援 8／16-bit RGB 與灰階影像、Raw、PackBits RLE、ZIP、ZIP prediction 壓縮，以及 RGB／灰階 ICC 色彩轉換。16-bit 影像會轉為 8-bit 顯示。在 Photoshop 儲存時應啟用「最大化相容性」；不支援 PSB、32-bit、CMYK、Lab、索引色及缺少有效合成影像的檔案。

## 下載與使用

從 [Releases 頁面](https://github.com/aatrox9029/MyGoImage/releases/tag/v1.0.0)下載，雙擊即可使用。

## 建置（供開發者使用）

需要 Rust stable，以及包含 C++ 工作負載與 Windows SDK 的 Visual Studio Build Tools（Windows x64）。

```powershell
cargo build --locked --release -p mygoimage
.\target\release\mygoimage.exe "D:\Photos"
```

檢查指令：`cargo fmt --all -- --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked --workspace -- --test-threads=1`。

如需可攜式 ZIP，依序執行 `python scripts/collect-licenses.py`、`python scripts/fetch-license-texts.py`、`./scripts/package-windows.ps1`。封裝使用 `scripts/release-toolchain.json` 指定的 Rust 版本，並附上產生的第三方授權聲明。

## 授權

專案程式碼採用 [MIT](LICENSE)。依賴套件各自適用其授權；Slint 使用免權利金桌面授權。可攜式封裝會附上產生的第三方授權聲明。
