//! Code generator for embedded-bitmap-fonts
//!
//! This tool processes BDF font files and generates Rust source code containing
//! the font bitmap data and metadata.

use anyhow::{Context, Result, bail};
use bdf2::Font;
use bitvec::prelude::*;
use clap::Parser;
use log::{info, warn};
use simple_logger::SimpleLogger;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

/// Character ranges to include in the font
/// Covers ASCII printable and Latin-1 supplement
const CHAR_RANGES: &[(char, char)] = &[
    // ASCII printable
    (' ', '~'),
    // Latin-1 supplement (useful accented chars)
    ('\u{A1}', '\u{FF}'),
];

/// Minimum rows for bitmap layout optimization
const MIN_ROWS: usize = 4;
/// Maximum rows for bitmap layout optimization  
const MAX_ROWS: usize = 20;

#[derive(Parser)]
#[command(name = "codegen")]
#[command(about = "Generate Rust code from BDF font files")]
struct Args {
    /// Input directory containing BDF files
    #[arg(short, long)]
    input: PathBuf,

    /// Output directory for generated Rust files
    #[arg(short, long)]
    output: PathBuf,

    /// Font family name (e.g., "tamzen", "cherry")
    #[arg(short, long)]
    family: String,

    /// Filter pattern for BDF files (e.g., "Tamzen" to only include Tamzen*.bdf)
    #[arg(short = 'P', long)]
    pattern: Option<String>,

    /// Exclude pattern for BDF files (e.g., "Powerline" to exclude Powerline variants)
    #[arg(short = 'X', long)]
    exclude: Option<String>,
}

/// Represents a processed font ready for code generation
struct ProcessedFont {
    name: String,
    width: usize,
    height: usize,
    bold: bool,
    bitmap: Vec<u8>,
    img_width: usize,
    #[allow(dead_code)]
    img_height: usize,
}

/// Helper to calculate ceiling division
fn ceiling_div(a: usize, b: usize) -> usize {
    (a + b - 1) / b
}

fn main() -> Result<()> {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()?;

    let args = Args::parse();

    info!("Processing fonts from: {:?}", args.input);
    info!("Output directory: {:?}", args.output);
    info!("Font family: {}", args.family);

    // Collect all BDF files
    let bdf_files: Vec<PathBuf> = WalkDir::new(&args.input)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            if !path.extension().map_or(false, |ext| ext == "bdf") {
                return false;
            }
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Apply include pattern
            if let Some(ref pattern) = args.pattern {
                if !filename.contains(pattern) {
                    return false;
                }
            }

            // Apply exclude pattern
            if let Some(ref exclude) = args.exclude {
                if filename.contains(exclude) {
                    return false;
                }
            }

            true
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    if bdf_files.is_empty() {
        bail!("No BDF files found in {:?}", args.input);
    }

    info!("Found {} BDF files", bdf_files.len());

    // Process each font
    let mut fonts = Vec::new();
    for path in bdf_files {
        match process_bdf_file(&path) {
            Ok(font) => {
                info!(
                    "Processed: {} ({}x{}, {})",
                    font.name,
                    font.width,
                    font.height,
                    if font.bold { "bold" } else { "regular" }
                );
                fonts.push(font);
            }
            Err(e) => {
                warn!("Failed to process {:?}: {}", path, e);
            }
        }
    }

    if fonts.is_empty() {
        bail!("No fonts were successfully processed");
    }

    // Deduplicate fonts by (width, height, bold) - keep the first one
    let mut seen: HashMap<(usize, usize, bool), usize> = HashMap::new();
    let mut deduped_fonts = Vec::new();
    for font in fonts {
        let key = (font.width, font.height, font.bold);
        if !seen.contains_key(&key) {
            seen.insert(key, deduped_fonts.len());
            deduped_fonts.push(font);
        }
    }

    info!("After deduplication: {} unique fonts", deduped_fonts.len());

    // Sort fonts by size and style
    deduped_fonts.sort_by(|a, b| (a.width, a.height, a.bold).cmp(&(b.width, b.height, b.bold)));

    // Generate output
    fs::create_dir_all(&args.output)?;
    let output_path = args.output.join(format!("{}.rs", args.family));
    generate_rust_code(&deduped_fonts, &args.family, &output_path)?;

    info!("Generated: {:?}", output_path);

    Ok(())
}

fn process_bdf_file(path: &Path) -> Result<ProcessedFont> {
    let filename = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("Invalid filename")?;

    // Read and parse BDF file
    let content = fs::read(path)?;
    let font = bdf2::read(&content[..]).map_err(|e| anyhow::anyhow!("BDF parse error: {:?}", e))?;

    // Get font metrics from the font itself
    let bounds = font.bounds();
    let width = bounds.width as usize;
    let height = bounds.height as usize;

    // Determine if bold from filename
    let lower = filename.to_lowercase();
    let bold = lower.ends_with('b') || lower.contains("-b") || lower.contains("bold");

    // Calculate total characters
    let char_count: usize = CHAR_RANGES
        .iter()
        .map(|(start, end)| *end as usize - *start as usize + 1)
        .sum();

    // Find optimal bitmap layout
    let (rows, per_line, img_width) = optimize_layout(char_count, width, height);

    // Build bitmap
    let bitmap = build_bitmap(&font, width, height, per_line, rows, img_width)?;

    Ok(ProcessedFont {
        name: filename.to_string(),
        width,
        height,
        bold,
        bitmap: bitmap.data,
        img_width: bitmap.width,
        img_height: bitmap.height,
    })
}

fn optimize_layout(char_count: usize, width: usize, height: usize) -> (usize, usize, usize) {
    let mut best_rows = 1;
    let mut best_per_line = char_count;
    let mut min_size = usize::MAX;

    for rows in MIN_ROWS..=MAX_ROWS {
        let per_line = ceiling_div(char_count, rows);
        let min_width = per_line * width;
        // Align to 32-bit boundary for efficient access
        let img_width = ceiling_div(min_width, 32) * 32;
        let img_height = rows * height;
        let size = ceiling_div(img_width * img_height, 8);

        if size < min_size || (size == min_size && rows > best_rows) {
            min_size = size;
            best_rows = rows;
            best_per_line = img_width / width;
        }
    }

    let img_width = best_per_line * width;
    (best_rows, best_per_line, img_width)
}

struct Bitmap {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

fn build_bitmap(
    font: &Font,
    glyph_width: usize,
    glyph_height: usize,
    per_line: usize,
    _rows: usize,
    img_width: usize,
) -> Result<Bitmap> {
    let mut all_lines: Vec<BitVec<u8, Msb0>> = Vec::new();
    let mut current_row_lines: Vec<BitVec<u8, Msb0>> = (0..glyph_height)
        .map(|_| BitVec::with_capacity(img_width))
        .collect();
    let mut chars_in_row = 0;

    for (start, end) in CHAR_RANGES {
        for ch in *start..=*end {
            add_glyph_to_lines(&mut current_row_lines, font, ch, glyph_width, glyph_height);

            chars_in_row += 1;
            if chars_in_row >= per_line {
                // Pad remaining bits to img_width
                for line in &mut current_row_lines {
                    while line.len() < img_width {
                        line.push(false);
                    }
                }
                all_lines.append(&mut current_row_lines);
                current_row_lines = (0..glyph_height)
                    .map(|_| BitVec::with_capacity(img_width))
                    .collect();
                chars_in_row = 0;
            }
        }
    }

    // Handle remaining characters
    if chars_in_row > 0 {
        for line in &mut current_row_lines {
            while line.len() < img_width {
                line.push(false);
            }
        }
        all_lines.append(&mut current_row_lines);
    }

    // Convert to bytes
    let height = all_lines.len();
    let mut data = Vec::new();
    for line in all_lines {
        data.extend(line.into_vec());
    }

    Ok(Bitmap {
        data,
        width: img_width,
        height,
    })
}

fn add_glyph_to_lines(
    lines: &mut [BitVec<u8, Msb0>],
    font: &Font,
    ch: char,
    glyph_width: usize,
    glyph_height: usize,
) {
    let glyph = font.glyphs().get(&ch);
    let font_bounds = font.bounds();

    for y in 0..glyph_height {
        for x in 0..glyph_width {
            let pixel = if let Some(g) = glyph {
                get_glyph_pixel(g, x, y, &font_bounds)
            } else {
                false
            };
            lines[y].push(pixel);
        }
    }
}

fn get_glyph_pixel(
    glyph: &bdf2::Glyph,
    x: usize,
    y: usize,
    font_bounds: &bdf2::BoundingBox,
) -> bool {
    let glyph_bounds = glyph.bounds();

    // Calculate the offset within the font's coordinate system
    // BDF glyphs can have different bounding boxes from the font
    let x_offset = (glyph_bounds.x - font_bounds.x) as i32;
    let y_offset = (font_bounds.height as i32 - glyph_bounds.height as i32)
        - (glyph_bounds.y - font_bounds.y) as i32;

    let gx = x as i32 - x_offset;
    let gy = y as i32 - y_offset;

    if gx < 0 || gy < 0 || gx >= glyph_bounds.width as i32 || gy >= glyph_bounds.height as i32 {
        return false;
    }

    glyph.get(gx as u32, gy as u32)
}

fn generate_rust_code(fonts: &[ProcessedFont], family: &str, output_path: &Path) -> Result<()> {
    let mut file = File::create(output_path)?;

    // Header
    writeln!(file, "// @generated by embedded-bitmap-fonts-codegen")?;
    writeln!(file, "// Do not edit manually!")?;
    writeln!(file)?;
    writeln!(file, "#![allow(non_upper_case_globals)]")?;
    writeln!(file)?;
    writeln!(file, "//! {} font family", family)?;
    writeln!(file, "//!")?;
    writeln!(
        file,
        "//! This module contains bitmap fonts from the {} family.",
        family
    )?;
    writeln!(file, "//!")?;
    writeln!(file, "//! ## Available Fonts")?;
    writeln!(file, "//!")?;
    writeln!(file, "//! | Font | Size | Style | Flash Size |")?;
    writeln!(file, "//! |------|------|-------|------------|")?;

    for font in fonts {
        let style = if font.bold { "Bold" } else { "Regular" };
        let const_name = make_const_name(font);
        writeln!(
            file,
            "//! | [`{}`] | {}x{} | {} | {} bytes |",
            const_name,
            font.width,
            font.height,
            style,
            font.bitmap.len()
        )?;
    }

    writeln!(file)?;
    writeln!(file, "use crate::BitmapFont;")?;
    writeln!(file, "use core::num::NonZeroU8;")?;
    writeln!(
        file,
        "use embedded_graphics::{{geometry::Size, image::ImageRaw, mono_font::mapping::GlyphMapping}};"
    )?;
    writeln!(file)?;

    // Custom glyph mapping struct
    writeln!(file, "/// Character mapping for this font family")?;
    writeln!(file, "struct Mapping;")?;
    writeln!(file)?;
    writeln!(file, "impl GlyphMapping for Mapping {{")?;
    writeln!(file, "    fn index(&self, c: char) -> usize {{")?;
    writeln!(file, "        match c {{")?;

    let mut offset = 0usize;
    for (start, end) in CHAR_RANGES {
        if offset == 0 {
            writeln!(
                file,
                "            '{}' ..= '{}' => c as usize - '{}' as usize,",
                escape_char(*start),
                escape_char(*end),
                escape_char(*start)
            )?;
        } else {
            writeln!(
                file,
                "            '{}' ..= '{}' => c as usize - '{}' as usize + {},",
                escape_char(*start),
                escape_char(*end),
                escape_char(*start),
                offset
            )?;
        }
        offset += *end as usize - *start as usize + 1;
    }

    writeln!(
        file,
        "            _ => '?' as usize - ' ' as usize, // replacement char"
    )?;
    writeln!(file, "        }}")?;
    writeln!(file, "    }}")?;
    writeln!(file, "}}")?;
    writeln!(file)?;
    writeln!(file, "static GLYPH_MAPPING: Mapping = Mapping;")?;
    writeln!(file)?;

    // NonZeroU8 constant
    writeln!(file, "const ONE: NonZeroU8 = match NonZeroU8::new(1) {{")?;
    writeln!(file, "    Some(one) => one,")?;
    writeln!(file, "    None => unreachable!(),")?;
    writeln!(file, "}};")?;
    writeln!(file)?;

    // Generate each font
    for font in fonts {
        generate_font_constant(&mut file, font)?;
    }

    Ok(())
}

fn escape_char(c: char) -> String {
    if c.is_ascii_graphic() || c == ' ' {
        format!("{}", c)
    } else {
        format!("\\u{{{:04X}}}", c as u32)
    }
}

fn make_const_name(font: &ProcessedFont) -> String {
    let style = if font.bold { "_BOLD" } else { "" };
    format!("FONT_{}x{}{}", font.width, font.height, style)
}

fn generate_font_constant(file: &mut File, font: &ProcessedFont) -> Result<()> {
    let const_name = make_const_name(font);
    let style = if font.bold { "bold" } else { "regular" };

    writeln!(file, "/// {}x{} {} font", font.width, font.height, style)?;
    writeln!(file, "///")?;
    writeln!(file, "/// Flash size: {} bytes", font.bitmap.len())?;
    writeln!(file, "///")?;
    writeln!(
        file,
        "/// Use `.pixel_double()` to get a {}x{} version.",
        font.width * 2,
        font.height * 2
    )?;
    writeln!(
        file,
        "pub static {}: BitmapFont<'static> = BitmapFont {{",
        const_name
    )?;
    writeln!(
        file,
        "    bitmap: ImageRaw::new(&{}_DATA, {}),",
        const_name, font.img_width
    )?;
    writeln!(file, "    glyph_mapping: &GLYPH_MAPPING,")?;
    writeln!(
        file,
        "    size: Size::new({}, {}),",
        font.width, font.height
    )?;
    writeln!(file, "    pixels: ONE,")?;
    writeln!(file, "}};")?;
    writeln!(file)?;

    // Bitmap data
    writeln!(file, "#[rustfmt::skip]")?;
    writeln!(
        file,
        "static {}_DATA: [u8; {}] = [",
        const_name,
        font.bitmap.len()
    )?;

    for chunk in font.bitmap.chunks(16) {
        write!(file, "    ")?;
        for byte in chunk {
            write!(file, "0x{:02X}, ", byte)?;
        }
        writeln!(file)?;
    }

    writeln!(file, "];")?;
    writeln!(file)?;

    Ok(())
}
