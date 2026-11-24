//! Draws the menu.

use cairo::{Context, Format, ImageSurface};

pub struct Image {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub data: Vec<u8>,
}

const CELL: f64 = 38.0;
const PAD: f64 = 5.0;
const HEIGHT: f64 = 60.0;

/// Logical size of a menu with `n` variants.
pub fn size(n: usize) -> (i32, i32) {
    ((CELL * n as f64 + 2.0 * PAD) as i32, HEIGHT as i32)
}

pub fn draw(variants: &[char], selected: Option<usize>, scale: i32) -> Result<Image, cairo::Error> {
    let (w, h) = size(variants.len());
    let (width, height) = (w * scale, h * scale);
    let w = w as f64;
    let mut surface = ImageSurface::create(Format::ARgb32, width, height)?;
    {
        let cr = Context::new(&surface)?;
        cr.scale(scale as f64, scale as f64);
        rounded(&cr, 0.5, 0.5, w - 1.0, HEIGHT - 1.0, 10.0);
        cr.set_source_rgb(0.13, 0.13, 0.15);
        cr.fill_preserve()?;
        cr.set_source_rgb(0.36, 0.36, 0.40);
        cr.set_line_width(1.0);
        cr.stroke()?;
        for (i, v) in variants.iter().enumerate() {
            let x = PAD + CELL * i as f64;
            let active = selected == Some(i);
            if active {
                rounded(&cr, x + 1.0, PAD, CELL - 2.0, HEIGHT - 2.0 * PAD, 7.0);
                cr.set_source_rgb(0.15, 0.42, 0.88);
                cr.fill()?;
            }
            cr.set_source_rgb(1.0, 1.0, 1.0);
            text(&cr, &v.to_string(), "Sans 17", x, 5.0);
            let grey = if active { 0.95 } else { 0.66 };
            cr.set_source_rgb(grey, grey, grey);
            text(&cr, &(i + 1).to_string(), "Sans 9", x, 37.0);
        }
    }
    surface.flush();
    let stride = surface.stride();
    let data = surface.data().expect("context dropped above").to_vec();
    Ok(Image { width, height, stride, data })
}

fn text(cr: &Context, s: &str, font: &str, x: f64, y: f64) {
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&pango::FontDescription::from_string(font)));
    layout.set_text(s);
    cr.move_to(x + (CELL - layout.pixel_size().0 as f64) / 2.0, y);
    pangocairo::functions::show_layout(cr, &layout);
}

fn rounded(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::FRAC_PI_2 as Q;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -Q, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, Q);
    cr.arc(x + r, y + h - r, r, Q, 2.0 * Q);
    cr.arc(x + r, y + r, r, 2.0 * Q, 3.0 * Q);
    cr.close_path();
}
