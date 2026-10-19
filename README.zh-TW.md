# MyGoImage

繁體中文 | [English](README.md)

以 Rust 與 Slint 開發的輕量 Windows 圖片瀏覽程式。
專為高容量影像檔案打造的圖片瀏覽器，著重於高畫質、流暢瀏覽體驗。

## 功能

- 瀏覽資料夾與 SD 卡，支援拖放圖片及命令列開啟。
- 支援 JPEG、PNG、WebP、BMP、TIFF、GIF（GIF／TIFF 顯示首幀／首頁）。
- 縮圖、縮放、排序與多圖瀏覽。
- 複製選取圖片，將不要的圖片移至程式專用垃圾桶。
- 英文與繁體中文即時切換。
- 自訂預載入圖片功能，切換大檔案圖片瀏覽不會卡頓。

## 效能

透過 JPEG 縮放解碼、SIMD CPU 縮圖、背景載入與相鄰圖片預載，降低瀏覽時的處理負擔。SQLite 索引、有限容量的 RAM 快取與持久化縮圖快取支援大型資料夾；記憶體及圖片尺寸上限可調整。實際速度依圖片大小、儲存裝置與硬體而異。

## 下載與使用

在Relese頁面即可下載程式，雙擊使用。

## 建置 (程式設計師用)

需要 Rust stable，以及包含 C++ 工作負載與 Windows SDK 的 Visual Studio Build Tools（Windows x64）。

```powershell
cargo build --locked --release -p mygoimage
.\target\release\mygoimage.exe "D:\Photos"
```

檢查指令：`cargo fmt --all -- --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked --workspace -- --test-threads=1`。

如需可攜式 ZIP，依序執行 `python scripts/collect-licenses.py`、`python scripts/fetch-license-texts.py`、`./scripts/package-windows.ps1`。封裝使用 `scripts/release-toolchain.json` 指定的 Rust 版本，並附上產生的第三方授權聲明。

## 授權

專案程式碼採用 [MIT](LICENSE)。依賴套件各自適用其授權；Slint 使用免權利金桌面授權。可攜式封裝會附上產生的第三方授權聲明。
