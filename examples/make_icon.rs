//! Render an icon SVG into the PNG sizes macOS needs for an .icns / asset
//! catalog (run via bundle.sh: `cargo run --example make_icon -- <out dir> [svg]`,
//! default svg: assets/icon.svg).

use resvg::{tiny_skia, usvg};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args().nth(1).ok_or("usage: make_icon <out dir> [svg]")?;
    let svg_path = std::env::args().nth(2).unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon.svg").into());
    std::fs::create_dir_all(&out)?;
    let svg = std::fs::read(&svg_path)?;
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default())?;
    for (base, scale) in [(16, 1), (16, 2), (32, 1), (32, 2), (128, 1), (128, 2), (256, 1), (256, 2), (512, 1), (512, 2)] {
        let px = base * scale;
        let mut pixmap = tiny_skia::Pixmap::new(px, px).ok_or("bad size")?;
        let s = px as f32 / tree.size().width();
        resvg::render(&tree, tiny_skia::Transform::from_scale(s, s), &mut pixmap.as_mut());
        let name = if scale == 1 { format!("icon_{base}x{base}.png") } else { format!("icon_{base}x{base}@2x.png") };
        pixmap.save_png(format!("{out}/{name}"))?;
    }
    Ok(())
}
