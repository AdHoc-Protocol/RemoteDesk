// Pictures of the clipboard: PNG on the wire, pixels in the clipboards of the systems. RGBA, 8 bits a channel, rows from the top.

use std::io::Cursor;

/// PNG of `rgba`, `width` × `height`: compressed fast - it goes once, at once.
pub fn png_of(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(rgba.len() / 4);
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

/// The pixels of a PNG as RGBA: width, height.
pub fn rgba_of(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    buffer.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buffer.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buffer.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None, // expanded by normalize_to_color8
    };
    (rgba.len() == (w * h * 4) as usize).then_some((rgba, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture goes to PNG and back as it was.
    #[test]
    fn a_picture_round_trips() {
        let rgba: Vec<u8> = (0..3 * 2 * 4).map(|i| (i * 11) as u8).collect();
        let png = png_of(&rgba, 3, 2).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        assert_eq!(rgba_of(&png), Some((rgba, 3, 2)));
    }
}
