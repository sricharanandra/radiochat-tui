use arboard::{Clipboard, ImageData};
use image::ImageFormat;
use std::io::Cursor;

pub struct ClipboardManager {
    clipboard: Clipboard,
}

impl ClipboardManager {
    pub fn new() -> Result<Self, String> {
        match Clipboard::new() {
            Ok(clipboard) => Ok(Self { clipboard }),
            Err(e) => Err(format!("Failed to initialize clipboard: {}", e)),
        }
    }
    
    /// Copy text to clipboard
    pub fn copy_text(&mut self, text: &str) -> Result<(), String> {
        self.clipboard
            .set_text(text)
            .map_err(|e| format!("Failed to copy text: {}", e))
    }
    
    /// Paste text from clipboard
    pub fn paste_text(&mut self) -> Result<String, String> {
        self.clipboard
            .get_text()
            .map_err(|e| format!("Failed to paste text: {}", e))
    }
    
    /// Check if clipboard contains an image
    pub fn has_image(&mut self) -> bool {
        self.clipboard.get_image().is_ok()
    }
    
    /// Paste image from clipboard as PNG bytes
    #[allow(dead_code)]
    pub fn paste_image(&mut self) -> Result<Vec<u8>, String> {
        let img = self.clipboard
            .get_image()
            .map_err(|e| format!("No image in clipboard: {}", e))?;
        
        // Convert ImageData to PNG bytes
        let rgba = img.bytes.to_vec();
        let width = img.width;
        let height = img.height;
        
        // Create image buffer
        let img_buffer = image::RgbaImage::from_raw(
            width as u32,
            height as u32,
            rgba,
        ).ok_or("Failed to create image buffer")?;
        
        // Encode as PNG
        let mut png_bytes = Vec::new();
        let mut cursor = Cursor::new(&mut png_bytes);
        
        image::write_buffer_with_format(
            &mut cursor,
            &img_buffer,
            width as u32,
            height as u32,
            image::ColorType::Rgba8,
            ImageFormat::Png,
        ).map_err(|e| format!("Failed to encode image: {}", e))?;
        
        Ok(png_bytes)
    }
    
    /// Copy image to clipboard from PNG bytes
    #[allow(dead_code)]
    pub fn copy_image(&mut self, png_bytes: &[u8]) -> Result<(), String> {
        // Decode PNG
        let img = image::load_from_memory_with_format(png_bytes, ImageFormat::Png)
            .map_err(|e| format!("Failed to decode image: {}", e))?;
        
        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        
        let img_data = ImageData {
            width: width as usize,
            height: height as usize,
            bytes: rgba.into_raw().into(),
        };
        
        self.clipboard
            .set_image(img_data)
            .map_err(|e| format!("Failed to copy image: {}", e))
    }
}
