use anyhow::{Result, anyhow};
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapEncoder, BitmapPixelFormat};
use windows::Storage::Streams::InMemoryRandomAccessStream;

/// Кодирует BGRA-пиксели в PNG-байты в памяти.
pub fn encode_png(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let stream = InMemoryRandomAccessStream::new()
        .map_err(|e| anyhow!("Stream: {e}"))?;

    let encoder_id = BitmapEncoder::PngEncoderId()
        .map_err(|e| anyhow!("PngEncoderId: {e}"))?;

    let encoder = BitmapEncoder::CreateAsync(encoder_id, &stream)
        .map_err(|e| anyhow!("Encoder: {e}"))?
        .get()
        .map_err(|e| anyhow!("Encoder.get: {e}"))?;

    encoder
        .SetPixelData(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            width,
            height,
            96.0,
            96.0,
            bgra,
        )
        .map_err(|e| anyhow!("SetPixelData: {e}"))?;

    encoder
        .FlushAsync()
        .map_err(|e| anyhow!("Flush: {e}"))?
        .get()
        .map_err(|e| anyhow!("Flush.get: {e}"))?;

    let size = stream.Size().map_err(|e| anyhow!("Size: {e}"))? as usize;
    stream.Seek(0).map_err(|e| anyhow!("Seek: {e}"))?;

    let reader = windows::Storage::Streams::DataReader::CreateDataReader(
        &stream.GetInputStreamAt(0).map_err(|e| anyhow!("GetInput: {e}"))?,
    )
    .map_err(|e| anyhow!("DataReader: {e}"))?;

    reader
        .LoadAsync(size as u32)
        .map_err(|e| anyhow!("Load: {e}"))?
        .get()
        .map_err(|e| anyhow!("Load.get: {e}"))?;

    let mut buf = vec![0u8; size];
    reader
        .ReadBytes(&mut buf)
        .map_err(|e| anyhow!("ReadBytes: {e}"))?;

    Ok(buf)
}

/// Сохраняет BGRA-пиксели как PNG в указанную папку.
/// Возвращает полный путь сохранённого файла.
pub fn save_png(bgra: &[u8], width: u32, height: u32, folder: &str) -> Result<String> {
    if folder.is_empty() {
        return Err(anyhow!("Папка для скриншотов не задана"));
    }

    let _ = std::fs::create_dir_all(folder);

    let now = chrono::Local::now();
    let filename = now.format("screenshot_%Y%m%d_%H%M%S.png").to_string();
    let path = std::path::Path::new(folder).join(&filename);
    let path_str = path.to_string_lossy().to_string();

    let buf = encode_png(bgra, width, height)?;
    std::fs::write(&path, &buf)
        .map_err(|e| anyhow!("Write file: {e}"))?;

    Ok(path_str)
}

/// Сохраняет BGRA-пиксели как PNG по указанному полному пути.
pub fn save_png_to_file(bgra: &[u8], width: u32, height: u32, file_path: &str) -> Result<()> {
    let buf = encode_png(bgra, width, height)?;
    std::fs::write(file_path, &buf)
        .map_err(|e| anyhow!("Write file: {e}"))?;
    Ok(())
}
