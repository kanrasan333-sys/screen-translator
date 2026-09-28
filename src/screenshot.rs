use anyhow::{Result, anyhow};
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapEncoder, BitmapPixelFormat, SoftwareBitmap,
};
use windows::Storage::Streams::{Buffer, DataReader, DataWriter, InMemoryRandomAccessStream};

/// Decodes an image file (PNG/JPEG/WebP/GIF/BMP) into BGRA pixels.
///
/// Uses the same WinRT imaging stack the encoder does, so it accepts whatever
/// Windows can decode without pulling in a Rust image crate.  The result is the
/// exact shape a clipboard grab produces, so an attached file rides the same
/// path as a pasted screenshot — thumbnail, encode and all.
pub fn decode_to_bgra(path: &str) -> Result<(Vec<u8>, u32, u32)> {
    let bytes = std::fs::read(path).map_err(|e| anyhow!("Read file: {e}"))?;

    // The decoder reads from a WinRT stream, so the file bytes go into one first.
    let stream = InMemoryRandomAccessStream::new().map_err(|e| anyhow!("Stream: {e}"))?;
    let writer = DataWriter::CreateDataWriter(
        &stream
            .GetOutputStreamAt(0)
            .map_err(|e| anyhow!("GetOutput: {e}"))?,
    )
    .map_err(|e| anyhow!("DataWriter: {e}"))?;
    writer
        .WriteBytes(&bytes)
        .map_err(|e| anyhow!("WriteBytes: {e}"))?;
    writer
        .StoreAsync()
        .map_err(|e| anyhow!("Store: {e}"))?
        .get()
        .map_err(|e| anyhow!("Store.get: {e}"))?;
    // Detach so dropping the writer doesn't close the stream out from under the
    // decoder.
    let _ = writer.DetachStream();
    stream.Seek(0).map_err(|e| anyhow!("Seek: {e}"))?;

    let decoder = BitmapDecoder::CreateAsync(&stream)
        .map_err(|e| anyhow!("Decoder: {e}"))?
        .get()
        .map_err(|e| anyhow!("Decoder.get: {e}"))?;
    let bmp = decoder
        .GetSoftwareBitmapAsync()
        .map_err(|e| anyhow!("GetBitmap: {e}"))?
        .get()
        .map_err(|e| anyhow!("GetBitmap.get: {e}"))?;

    // Whatever the file's native format, hand back 32bpp BGRA — the one shape
    // the thumbnail and the PNG encoder both already speak.  This two-arg
    // Convert yields premultiplied alpha for BGRA, which is what the encoder
    // expects downstream.
    let bgra = SoftwareBitmap::Convert(&bmp, BitmapPixelFormat::Bgra8)
        .map_err(|e| anyhow!("Convert: {e}"))?;
    let w = bgra.PixelWidth().map_err(|e| anyhow!("Width: {e}"))? as u32;
    let h = bgra.PixelHeight().map_err(|e| anyhow!("Height: {e}"))? as u32;
    if w == 0 || h == 0 {
        return Err(anyhow!("empty image"));
    }

    let len = w * h * 4;
    let buffer = Buffer::Create(len).map_err(|e| anyhow!("Buffer: {e}"))?;
    bgra.CopyToBuffer(&buffer)
        .map_err(|e| anyhow!("CopyToBuffer: {e}"))?;
    let reader = DataReader::FromBuffer(&buffer).map_err(|e| anyhow!("FromBuffer: {e}"))?;
    let mut out = vec![0u8; len as usize];
    reader
        .ReadBytes(&mut out)
        .map_err(|e| anyhow!("ReadBytes: {e}"))?;

    Ok((out, w, h))
}

/// Кодирует BGRA-пиксели в PNG-байты в памяти.
pub fn encode_png(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let stream = InMemoryRandomAccessStream::new().map_err(|e| anyhow!("Stream: {e}"))?;

    let encoder_id = BitmapEncoder::PngEncoderId().map_err(|e| anyhow!("PngEncoderId: {e}"))?;

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
        &stream
            .GetInputStreamAt(0)
            .map_err(|e| anyhow!("GetInput: {e}"))?,
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
    std::fs::write(&path, &buf).map_err(|e| anyhow!("Write file: {e}"))?;

    Ok(path_str)
}

/// Сохраняет BGRA-пиксели как PNG по указанному полному пути.
pub fn save_png_to_file(bgra: &[u8], width: u32, height: u32, file_path: &str) -> Result<()> {
    let buf = encode_png(bgra, width, height)?;
    std::fs::write(file_path, &buf).map_err(|e| anyhow!("Write file: {e}"))?;
    Ok(())
}

/// Encodes to PNG, shrinking first if the image is larger than `max_edge` on
/// its long side.
///
/// A vision model has a resolution it works at and scales anything bigger down
/// to fit; doing it here instead means the wire carries a few hundred KB
/// rather than a few megabytes for the same picture, and a full-screen grab
/// off a wide monitor is exactly the case that happens.
pub fn encode_png_capped(bgra: &[u8], width: u32, height: u32, max_edge: u32) -> Result<Vec<u8>> {
    if width.max(height) <= max_edge || width == 0 || height == 0 {
        return encode_png(bgra, width, height);
    }
    let (w, h, small) = downscale(bgra, width, height, max_edge);
    encode_png(&small, w, h)
}

/// Box filter: every destination pixel is the average of the source pixels
/// that fall inside it.
///
/// Dropping pixels instead would be faster and would shred small text, which
/// is most of what anyone points a vision model at.
fn downscale(bgra: &[u8], width: u32, height: u32, max_edge: u32) -> (u32, u32, Vec<u8>) {
    let scale = max_edge as f64 / width.max(height) as f64;
    let nw = ((width as f64 * scale).round() as u32).max(1);
    let nh = ((height as f64 * scale).round() as u32).max(1);

    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; nw as usize * nh as usize * 4];

    for y in 0..nh as usize {
        let y0 = y * h / nh as usize;
        let y1 = ((y + 1) * h / nh as usize).max(y0 + 1).min(h);
        for x in 0..nw as usize {
            let x0 = x * w / nw as usize;
            let x1 = ((x + 1) * w / nw as usize).max(x0 + 1).min(w);

            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for sy in y0..y1 {
                let row = sy * w * 4;
                for sx in x0..x1 {
                    let i = row + sx * 4;
                    acc[0] += bgra[i] as u32;
                    acc[1] += bgra[i + 1] as u32;
                    acc[2] += bgra[i + 2] as u32;
                    acc[3] += bgra[i + 3] as u32;
                    n += 1;
                }
            }

            let d = (y * nw as usize + x) * 4;
            for c in 0..4 {
                out[d + c] = (acc[c] / n) as u8;
            }
        }
    }

    (nw, nh, out)
}
