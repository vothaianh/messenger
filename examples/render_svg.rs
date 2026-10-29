//! Render one SVG to a PNG at its native size (layer images for AppIcon.icon):
//! `cargo run --example render_svg -- <in.svg> <out.png>`.

use resvg::{tiny_skia, usvg};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (input, output) = (args.next().ok_or("usage: render_svg <in.svg> <out.png>")?, args.next().ok_or("missing <out.png>")?);
    let tree = usvg::Tree::from_data(&std::fs::read(input)?, &usvg::Options::default())?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height()).ok_or("bad size")?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    pixmap.save_png(output)?;
    Ok(())
}
