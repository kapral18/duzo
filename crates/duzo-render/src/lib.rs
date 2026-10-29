//! Drawing primitives for [duzo](https://github.com/kapral18/duzo):
//! block bars, a half-block colour canvas, a braille dot canvas, and the
//! sunburst layout that turns sizes into wedges.
//!
//! Nothing here knows about files or terminals — the layout takes sizes and
//! ids, the canvases hand back cells, and the caller decides what to do with
//! them. That keeps the geometry testable without a TTY.

pub mod bar;
pub mod braille;
pub mod canvas;
pub mod palette;
pub mod radial;

pub use braille::BrailleCanvas;
pub use canvas::{Canvas, HalfCell};
pub use palette::Rgb;
pub use radial::{LayoutOptions, RadialNode, RenderOptions, Segment};
