//! The content-stream interpreter (ISO 32000-1, 8–9): graphics state, path
//! construction and painting, clipping, text, XObjects, inline images,
//! shadings and patterns, ExtGState, and the widget annotations' appearance
//! streams — drawn onto a tiny-skia canvas in device space.
//!
//! Where docling-parse's renderer makes a choice, this one follows it: a
//! stroke is at least one device pixel wide and a `0 w` hairline is one
//! pixel; a line width scales by `sqrt(|det CTM|)`; ExtGState alpha and
//! blend modes apply, soft masks and transparency groups do not (the group
//! is drawn straight onto the page); dash patterns are drawn (docling-parse
//! drops them); only Widget annotations are rendered, from their `/AP /N`
//! appearance; a glyph with no face at all leaves a thin blue box.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use lopdf::{Dictionary, Document, Object, ObjectId};
use tiny_skia::{
    BlendMode, FillRule, FilterQuality, GradientStop, LineCap, LineJoin, Mask, Paint, Path,
    PathBuilder, Pixmap, PixmapPaint, Point, SpreadMode, Stroke, StrokeDash, Transform,
};

use super::color::{to_u8, CmykCache, ColorSpace};
use super::font::{FontCache, LoadedFont};
use super::function::Function;
use super::geom::{Box2, Mat};
use super::image;
use super::objects::{
    as_dict, as_stream, deref, get, get_bool, get_dict, get_int, get_name, get_num, name, num,
    nums, resource,
};

const MAX_OPS: usize = 2_000_000;
const MAX_DEPTH: usize = 14;

/// A mesh vertex: device point and RGB.
type Vertex = ((f64, f64), [f64; 3]);
/// A flat-shaded mesh triangle (types 4/5): corners and their mean colour.
type Triangle = ([(f64, f64); 3], [f64; 3]);
/// A flat-shaded Coons/tensor patch (types 6/7): its boundary and mean colour.
type Patch = (Vec<(f64, f64)>, [f64; 3]);

#[derive(Clone)]
struct TextState {
    font: Option<Rc<LoadedFont>>,
    size: f64,
    char_spacing: f64,
    word_spacing: f64,
    hscale: f64,
    leading: f64,
    rise: f64,
    render_mode: i64,
}

#[derive(Clone)]
struct GState {
    ctm: Mat,
    fill_cs: ColorSpace,
    stroke_cs: ColorSpace,
    /// Device RGB, `None` = paints nothing (Separation /None, an unresolved pattern).
    fill_rgb: Option<[f64; 3]>,
    stroke_rgb: Option<[f64; 3]>,
    /// A pattern selected with `scn` / `SCN` (`/Pattern` colour space).
    fill_pattern: Option<Object>,
    stroke_pattern: Option<Object>,
    line_width: f64,
    line_cap: LineCap,
    line_join: LineJoin,
    miter_limit: f64,
    dash: Option<(Vec<f64>, f64)>,
    fill_alpha: f64,
    stroke_alpha: f64,
    blend: BlendMode,
    /// The clip: an axis-aligned rectangle (device space) when nothing else
    /// has been intersected, and a coverage mask once a shape has.
    clip_box: Box2,
    clip_mask: Option<Rc<Mask>>,
    text: TextState,
    /// An uncoloured tiling pattern's cell: colour operators are ignored.
    fixed_color: bool,
    /// The constant alpha and blend mode in force at the `Do` of the
    /// enclosing transparency group(s), applied to everything drawn inside
    /// (docling-parse's `enter_transparency_group`: the group is not
    /// composited as a unit, its parameters are pushed down instead).
    group_alpha: f64,
    group_blend: BlendMode,
}

impl GState {
    fn eff_fill_alpha(&self) -> f64 {
        self.fill_alpha * self.group_alpha
    }
    fn eff_stroke_alpha(&self) -> f64 {
        self.stroke_alpha * self.group_alpha
    }
    /// Content that does not blend on its own inherits the group's mode.
    fn eff_blend(&self) -> BlendMode {
        if self.blend == BlendMode::SourceOver {
            self.group_blend
        } else {
            self.blend
        }
    }
}

/// The caches one document's renders share: parsed fonts, the CMYK
/// conversion table and decoded image samples. The pipeline renders every
/// page twice (the layout scale and the OCR scale), so decoding a page's
/// photographs once pays for itself; the store is bounded by
/// [`IMAGE_BUDGET`] bytes and cleared wholesale when it overflows.
pub struct Shared {
    fonts: RefCell<FontCache>,
    cmyk: RefCell<CmykCache>,
    images: RefCell<ImageStore>,
    /// Images drawn as a placeholder (or not drawn) since the last
    /// [`super::Renderer::take_omissions`] (#916).
    omissions: RefCell<Vec<super::Omission>>,
    /// docling-parse's `bitmap_target_pixels_per_unit`: the resolution, in
    /// pixels per PDF unit, a JPEG needs to be decoded at for the page it is
    /// drawn on ([`image::codec_reduction_shift`]). docling renders with
    /// `render_scale` 1.0 and re-renders the same decoders at the model
    /// scales, so 1.0 is the pipeline's value whatever the canvas scale;
    /// `0.0` disables the reduced decode (every image at full size).
    bitmap_hint: f64,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            fonts: RefCell::default(),
            cmyk: RefCell::default(),
            images: RefCell::default(),
            omissions: RefCell::default(),
            bitmap_hint: 1.0,
        }
    }
}

impl Shared {
    /// Caches for a renderer decoding its JPEGs for `bitmap_hint` pixels per
    /// PDF unit (see the field).
    pub fn with_bitmap_hint(bitmap_hint: f64) -> Shared {
        Shared {
            bitmap_hint,
            ..Shared::default()
        }
    }

    /// Drain what the renders since the last call left out.
    pub fn take_omissions(&self) -> Vec<super::Omission> {
        std::mem::take(&mut *self.omissions.borrow_mut())
    }

    fn omit(&self, id: Option<ObjectId>, rect: [f64; 4], reason: String) {
        self.omissions.borrow_mut().push(super::Omission {
            object: id,
            rect,
            reason,
        });
    }

    /// The renderer's default `bitmap_target_pixels_per_unit`.
    pub fn bitmap_hint(&self) -> f64 {
        self.bitmap_hint
    }
}

/// Decoded image samples per source stream (`image::load`'s result), by
/// (object id, reduction shift).
#[derive(Default)]
struct ImageStore {
    by_id: HashMap<(ObjectId, u32), Rc<image::LoadedImage>>,
    bytes: usize,
}

/// The decoded-sample budget of one document's image store (256 MB): a
/// scanned book's pages are ~8 MB each at 300 dpi, so the two-scale render
/// of a page always hits, and a document that overflows merely re-decodes.
const IMAGE_BUDGET: usize = 256 << 20;

impl Shared {
    fn font(&self, doc: &Document, obj: &Object) -> Option<Rc<LoadedFont>> {
        self.fonts.borrow_mut().get(doc, obj)
    }

    /// `image::load` memoised by object id (inline and direct images are
    /// decoded every time).
    fn load_image(
        &self,
        doc: &Document,
        stream: &lopdf::Stream,
        id: Option<ObjectId>,
        res: Option<&Dictionary>,
        reduction_shift: u32,
    ) -> Result<Rc<image::LoadedImage>, String> {
        if let Some(id) = id {
            if let Some(img) = self.images.borrow().by_id.get(&(id, reduction_shift)) {
                return Ok(img.clone());
            }
        }
        let img = Rc::new(image::load(doc, stream, res, reduction_shift)?);
        if let Some(id) = id {
            let mut store = self.images.borrow_mut();
            let bytes = img.bytes();
            if store.bytes + bytes > IMAGE_BUDGET {
                store.by_id.clear();
                store.bytes = 0;
            }
            if bytes <= IMAGE_BUDGET {
                store.bytes += bytes;
                store.by_id.insert((id, reduction_shift), img.clone());
            }
        }
        Ok(img)
    }
}

pub struct Interp<'a> {
    doc: &'a Document,
    canvas: Pixmap,
    /// Device box of the whole canvas.
    device: Box2,
    /// Per-document caches, shared by every render of the document.
    shared: Rc<Shared>,
    /// Images rasterized for this canvas, by (object id, reduction, fill
    /// colour for stencils).
    images: HashMap<(ObjectId, u32, u32, [u8; 3]), Rc<image::DecodedImage>>,
    /// Rectangle clip masks by rounded coordinates (many text objects share one).
    rect_masks: HashMap<[i32; 4], Rc<Mask>>,
    /// Shape clip masks by (path hash, parent mask, rule): a page that sets
    /// the same rounded frame before each of its figures builds it once.
    shape_masks: HashMap<(u64, usize, bool), Rc<Mask>>,
    ops: usize,
    /// Fonts that drew a blue fallback box at least once (warned once).
    warned_no_face: bool,
    /// Type 3 glyph procedures being run, to stop recursion.
    type3_depth: usize,
    /// The page's base CTM: pattern space (8.7.3.1).
    base: Mat,
    /// `bitmap_target_pixels_per_unit` for this render (see
    /// [`Shared::bitmap_hint`]): the model-input renders take docling's 1.0,
    /// a bitmap kept for OCR or handed to a caller takes 0.0 (full size).
    bitmap_hint: f64,
}

/// One painted sub-path being built (user space, transformed at paint time).
struct PathState {
    builder: PathBuilder,
    /// The current point and the sub-path start in user space.
    current: Option<(f64, f64)>,
    start: Option<(f64, f64)>,
    /// `re` operands seen so far, when the path is nothing but rectangles.
    rects: Vec<[f64; 4]>,
    only_rects: bool,
    pending_clip: Option<FillRule>,
    empty: bool,
}

impl PathState {
    fn new() -> PathState {
        PathState {
            builder: PathBuilder::new(),
            current: None,
            start: None,
            rects: Vec::new(),
            only_rects: true,
            pending_clip: None,
            empty: true,
        }
    }
}

impl<'a> Interp<'a> {
    /// A white canvas of `width` × `height` device pixels.
    pub fn new(
        doc: &'a Document,
        width: u32,
        height: u32,
        shared: Rc<Shared>,
        bitmap_hint: f64,
    ) -> Option<Interp<'a>> {
        let mut canvas = Pixmap::new(width, height)?;
        canvas.fill(tiny_skia::Color::WHITE);
        Some(Interp {
            doc,
            canvas,
            device: Box2::new(0.0, 0.0, f64::from(width), f64::from(height)),
            shared,
            bitmap_hint,
            images: HashMap::new(),
            rect_masks: HashMap::new(),
            shape_masks: HashMap::new(),
            ops: 0,
            warned_no_face: false,
            type3_depth: 0,
            base: Mat::IDENTITY,
        })
    }

    pub fn into_canvas(self) -> Pixmap {
        self.canvas
    }

    fn initial_state(&self, ctm: Mat) -> GState {
        GState {
            ctm,
            fill_cs: ColorSpace::DeviceGray,
            stroke_cs: ColorSpace::DeviceGray,
            fill_rgb: Some([0.0, 0.0, 0.0]),
            stroke_rgb: Some([0.0, 0.0, 0.0]),
            fill_pattern: None,
            stroke_pattern: None,
            line_width: 1.0,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: None,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            blend: BlendMode::SourceOver,
            clip_box: self.device,
            clip_mask: None,
            text: TextState {
                font: None,
                size: 0.0,
                char_spacing: 0.0,
                word_spacing: 0.0,
                hscale: 1.0,
                leading: 0.0,
                rise: 0.0,
                render_mode: 0,
            },
            fixed_color: false,
            group_alpha: 1.0,
            group_blend: BlendMode::SourceOver,
        }
    }

    /// Draw a page's content under `base` (user space → device).
    pub fn run_page(&mut self, content: &[u8], resources: Option<&Dictionary>, base: Mat) {
        self.base = base;
        let st = self.initial_state(base);
        self.run(content, resources, st, 0);
    }

    /// Draw the page's Widget annotations (`/AP /N`, honouring `/AS` and the
    /// Hidden / NoView flags) — what docling-parse renders of the annotations.
    pub fn run_widgets(&mut self, page: &Dictionary, base: Mat) {
        let doc = self.doc;
        let Some(Object::Array(annots)) = get(doc, page, b"Annots") else {
            return;
        };
        for a in annots {
            let Some(ad) = as_dict(doc, a) else { continue };
            if get_name(doc, ad, b"Subtype") != Some(b"Widget") {
                continue;
            }
            let flags = get_int(doc, ad, b"F").unwrap_or(0);
            if flags & 2 != 0 || flags & 32 != 0 {
                continue;
            }
            let Some(rect) = get(doc, ad, b"Rect")
                .and_then(|o| nums(doc, o))
                .filter(|r| r.len() == 4)
            else {
                continue;
            };
            let Some(ap) = get_dict(doc, ad, b"AP") else {
                continue;
            };
            let Some(n) = get(doc, ap, b"N") else {
                continue;
            };
            let stream = match n {
                Object::Stream(s) => Some(s),
                Object::Dictionary(states) => {
                    let as_name = get_name(doc, ad, b"AS");
                    match as_name {
                        Some(st) => states.get(st).ok().and_then(|o| as_stream(doc, o)),
                        None if states.len() == 1 => {
                            states.iter().next().and_then(|(_, o)| as_stream(doc, o))
                        }
                        None => None,
                    }
                }
                _ => None,
            };
            let Some(stream) = stream else { continue };
            // 12.5.5: the form's BBox through its Matrix, fitted to Rect.
            let bbox = get(doc, &stream.dict, b"BBox")
                .and_then(|o| nums(doc, o))
                .filter(|b| b.len() == 4);
            let matrix = get(doc, &stream.dict, b"Matrix")
                .and_then(|o| nums(doc, o))
                .and_then(|v| Mat::from_slice(&v))
                .unwrap_or(Mat::IDENTITY);
            let rx0 = rect[0].min(rect[2]);
            let ry0 = rect[1].min(rect[3]);
            let rx1 = rect[0].max(rect[2]);
            let ry1 = rect[1].max(rect[3]);
            let a = match bbox {
                Some(b) => {
                    let tb = Box2::new(b[0], b[1], b[2], b[3]).transformed(matrix);
                    let sx = if tb.width() > 1e-9 {
                        (rx1 - rx0) / tb.width()
                    } else {
                        1.0
                    };
                    let sy = if tb.height() > 1e-9 {
                        (ry1 - ry0) / tb.height()
                    } else {
                        1.0
                    };
                    Mat::new(sx, 0.0, 0.0, sy, rx0 - tb.x0 * sx, ry0 - tb.y0 * sy)
                }
                None => Mat::IDENTITY,
            };
            let mut st = self.initial_state(a.then(base));
            st.clip_box = self.device;
            self.draw_form(stream, &st, None, 1);
        }
    }

    fn run(&mut self, content: &[u8], resources: Option<&Dictionary>, init: GState, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        // Inline images and `d0`/`d1` go through the pre-pass (lopdf's
        // lexer drops or splits them).
        let prepared = super::prepass::prepare(content, self.doc, resources);
        let Ok(ops) = lopdf::content::Content::decode(&prepared.content) else {
            return;
        };
        let inline = &prepared.inline;
        let doc = self.doc;
        let mut stack: Vec<GState> = Vec::new();
        let mut st = init.clone();
        let mut path = PathState::new();
        // Text object state.
        let mut tm = Mat::IDENTITY;
        let mut tlm = Mat::IDENTITY;
        let mut compat = 0usize;
        for op in &ops.operations {
            self.ops += 1;
            if self.ops > MAX_OPS {
                return;
            }
            let args = &op.operands;
            let n = |i: usize| args.get(i).and_then(num).unwrap_or(0.0);
            match op.operator.as_str() {
                // --- graphics state ------------------------------------------
                "q" => {
                    stack.push(st.clone());
                    if stack.len() > 64 {
                        stack.remove(0);
                    }
                }
                "Q" => {
                    if let Some(s) = stack.pop() {
                        st = s;
                    }
                }
                "cm" => {
                    if args.len() >= 6 {
                        let v: Vec<f64> = (0..6).map(n).collect();
                        if let Some(m) = Mat::from_slice(&v) {
                            st.ctm = m.then(st.ctm);
                        }
                    }
                }
                "w" => st.line_width = n(0).abs(),
                "J" => {
                    st.line_cap = match n(0) as i64 {
                        1 => LineCap::Round,
                        2 => LineCap::Square,
                        _ => LineCap::Butt,
                    }
                }
                "j" => {
                    st.line_join = match n(0) as i64 {
                        1 => LineJoin::Round,
                        2 => LineJoin::Bevel,
                        _ => LineJoin::Miter,
                    }
                }
                "M" => st.miter_limit = n(0).max(1.0),
                "d" => {
                    let arr: Vec<f64> = args
                        .first()
                        .and_then(|o| nums(doc, o))
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|v| v.is_finite() && *v >= 0.0)
                        .collect();
                    st.dash = if arr.is_empty() || arr.iter().all(|v| *v <= 0.0) {
                        None
                    } else {
                        Some((arr, n(1)))
                    };
                }
                "ri" | "i" => {}
                "gs" => {
                    if let Some(gs) = args
                        .first()
                        .and_then(name)
                        .and_then(|nm| resource(doc, resources, b"ExtGState", nm))
                        .and_then(|o| as_dict(doc, o))
                    {
                        self.apply_extgstate(&mut st, gs, resources);
                    }
                }
                // --- path construction ---------------------------------------
                "m" => {
                    if args.len() >= 2 {
                        let (x, y) = (n(0), n(1));
                        path.builder.move_to(x as f32, y as f32);
                        path.current = Some((x, y));
                        path.start = Some((x, y));
                        path.only_rects = false;
                        path.empty = false;
                    }
                }
                "l" => {
                    if args.len() >= 2 {
                        let (x, y) = (n(0), n(1));
                        if path.current.is_none() {
                            path.builder.move_to(x as f32, y as f32);
                            path.start = Some((x, y));
                        } else {
                            path.builder.line_to(x as f32, y as f32);
                        }
                        path.current = Some((x, y));
                        path.only_rects = false;
                        path.empty = false;
                    }
                }
                "c" | "v" | "y" => {
                    let cur = path.current.unwrap_or((n(0), n(1)));
                    let (x1, y1, x2, y2, x3, y3) = match op.operator.as_str() {
                        "c" if args.len() >= 6 => (n(0), n(1), n(2), n(3), n(4), n(5)),
                        "v" if args.len() >= 4 => (cur.0, cur.1, n(0), n(1), n(2), n(3)),
                        "y" if args.len() >= 4 => (n(0), n(1), n(2), n(3), n(2), n(3)),
                        _ => continue,
                    };
                    if path.current.is_none() {
                        path.builder.move_to(cur.0 as f32, cur.1 as f32);
                        path.start = Some(cur);
                    }
                    path.builder.cubic_to(
                        x1 as f32, y1 as f32, x2 as f32, y2 as f32, x3 as f32, y3 as f32,
                    );
                    path.current = Some((x3, y3));
                    path.only_rects = false;
                    path.empty = false;
                }
                "h" => {
                    if path.current.is_some() {
                        path.builder.close();
                        path.current = path.start;
                    }
                }
                "re" => {
                    if args.len() >= 4 {
                        let (x, y, w, h) = (n(0), n(1), n(2), n(3));
                        path.builder.move_to(x as f32, y as f32);
                        path.builder.line_to((x + w) as f32, y as f32);
                        path.builder.line_to((x + w) as f32, (y + h) as f32);
                        path.builder.line_to(x as f32, (y + h) as f32);
                        path.builder.close();
                        path.current = Some((x, y));
                        path.start = Some((x, y));
                        path.rects.push([x, y, w, h]);
                        path.empty = false;
                    }
                }
                // --- path painting -------------------------------------------
                "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "n" => {
                    let o = op.operator.as_str();
                    if matches!(o, "s" | "b" | "b*") && path.current.is_some() {
                        path.builder.close();
                    }
                    let rule = if o.ends_with('*') {
                        FillRule::EvenOdd
                    } else {
                        FillRule::Winding
                    };
                    let fill = matches!(o, "f" | "F" | "f*" | "B" | "B*" | "b" | "b*");
                    let stroke = matches!(o, "S" | "s" | "B" | "B*" | "b" | "b*");
                    let mut finished = std::mem::replace(&mut path, PathState::new());
                    let user_path =
                        std::mem::replace(&mut finished.builder, PathBuilder::new()).finish();
                    if let Some(up) = &user_path {
                        if let Some(dev) = up.clone().transform(st.ctm.to_ts()) {
                            if fill {
                                self.fill_device_path(&dev, rule, &st, resources, false);
                            }
                            if stroke {
                                self.stroke_device_path(&dev, up, &st, resources);
                            }
                        }
                    }
                    if let Some(clip_rule) = finished.pending_clip {
                        crate::timing::timed("render.apply_clip", || {
                            self.apply_clip(&mut st, user_path.as_ref(), &finished, clip_rule)
                        });
                    }
                }
                "W" => path.pending_clip = Some(FillRule::Winding),
                "W*" => path.pending_clip = Some(FillRule::EvenOdd),
                // --- colour ----------------------------------------------------
                "CS" | "cs" => {
                    if st.fixed_color {
                        continue;
                    }
                    let cs = args
                        .first()
                        .and_then(|o| ColorSpace::parse(doc, o, resources))
                        .unwrap_or(ColorSpace::DeviceGray);
                    let initial = cs.initial();
                    let rgb = match cs {
                        ColorSpace::Pattern(_) => None,
                        _ => cs.to_rgb(&initial),
                    };
                    if op.operator == "cs" {
                        st.fill_cs = cs;
                        st.fill_rgb = rgb;
                        st.fill_pattern = None;
                    } else {
                        st.stroke_cs = cs;
                        st.stroke_rgb = rgb;
                        st.stroke_pattern = None;
                    }
                }
                "SC" | "SCN" | "sc" | "scn" => {
                    if st.fixed_color {
                        continue;
                    }
                    let is_fill = op.operator.starts_with('s');
                    let cs = if is_fill { &st.fill_cs } else { &st.stroke_cs };
                    let (rgb, pattern) = match cs {
                        ColorSpace::Pattern(base) => {
                            let pat_name = args.last().and_then(name);
                            let pat = pat_name
                                .and_then(|nm| resource(doc, resources, b"Pattern", nm))
                                .cloned();
                            // An uncoloured pattern's colour operands in the base space.
                            let comps: Vec<f64> = args.iter().filter_map(num).collect();
                            let rgb = match (base, comps.is_empty()) {
                                (Some(b), false) => b.to_rgb(&comps),
                                _ => Some([0.5, 0.5, 0.5]),
                            };
                            (rgb, pat)
                        }
                        cs => {
                            let comps: Vec<f64> = args.iter().filter_map(num).collect();
                            if comps.is_empty() {
                                continue;
                            }
                            (cs.to_rgb(&comps), None)
                        }
                    };
                    if is_fill {
                        st.fill_rgb = rgb;
                        st.fill_pattern = pattern;
                    } else {
                        st.stroke_rgb = rgb;
                        st.stroke_pattern = pattern;
                    }
                }
                "g" | "G" | "rg" | "RG" | "k" | "K" => {
                    if st.fixed_color {
                        continue;
                    }
                    let cs = match op.operator.as_str() {
                        "g" | "G" => ColorSpace::DeviceGray,
                        "rg" | "RG" => ColorSpace::DeviceRGB,
                        _ => ColorSpace::DeviceCMYK,
                    };
                    let comps: Vec<f64> = args.iter().filter_map(num).collect();
                    let rgb = cs.to_rgb(&comps);
                    if op
                        .operator
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase())
                    {
                        st.fill_cs = cs;
                        st.fill_rgb = rgb;
                        st.fill_pattern = None;
                    } else {
                        st.stroke_cs = cs;
                        st.stroke_rgb = rgb;
                        st.stroke_pattern = None;
                    }
                }
                // --- text --------------------------------------------------------
                "BT" => {
                    tm = Mat::IDENTITY;
                    tlm = Mat::IDENTITY;
                }
                "ET" => {}
                "Tc" => st.text.char_spacing = n(0),
                "Tw" => st.text.word_spacing = n(0),
                "Tz" => st.text.hscale = n(0) / 100.0,
                "TL" => st.text.leading = n(0),
                "Ts" => st.text.rise = n(0),
                "Tr" => st.text.render_mode = n(0) as i64,
                "Tf" => {
                    st.text.size = n(1);
                    st.text.font = args
                        .first()
                        .and_then(name)
                        .and_then(|nm| resource(doc, resources, b"Font", nm))
                        .and_then(|o| self.shared.font(doc, o));
                    if st.text.font.is_none() {
                        // A missing font resource: Helvetica-like fallback so the
                        // text still lands on the page.
                        let mut d = Dictionary::new();
                        d.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
                        st.text.font = self.shared.font(doc, &Object::Dictionary(d));
                    }
                }
                "Td" => {
                    tlm = Mat::translate(n(0), n(1)).then(tlm);
                    tm = tlm;
                }
                "TD" => {
                    st.text.leading = -n(1);
                    tlm = Mat::translate(n(0), n(1)).then(tlm);
                    tm = tlm;
                }
                "Tm" => {
                    if args.len() >= 6 {
                        let v: Vec<f64> = (0..6).map(n).collect();
                        if let Some(m) = Mat::from_slice(&v) {
                            tlm = m;
                            tm = tlm;
                        }
                    }
                }
                "T*" => {
                    tlm = Mat::translate(0.0, -st.text.leading).then(tlm);
                    tm = tlm;
                }
                "Tj" | "'" | "\"" => {
                    if op.operator != "Tj" {
                        if op.operator == "\"" && args.len() >= 3 {
                            st.text.word_spacing = n(0);
                            st.text.char_spacing = n(1);
                        }
                        tlm = Mat::translate(0.0, -st.text.leading).then(tlm);
                        tm = tlm;
                    }
                    if let Some(Object::String(s, _)) = args.last() {
                        self.show_text(s, &mut tm, &st, resources, depth);
                    }
                }
                "TJ" => {
                    if let Some(Object::Array(items)) = args.first() {
                        for it in items {
                            match it {
                                Object::String(s, _) => {
                                    self.show_text(s, &mut tm, &st, resources, depth)
                                }
                                o => {
                                    if let Some(adj) = num(o) {
                                        let tx = -adj / 1000.0 * st.text.size * st.text.hscale;
                                        let vertical =
                                            st.text.font.as_ref().is_some_and(|f| f.vertical);
                                        tm = if vertical {
                                            Mat::translate(0.0, -adj / 1000.0 * st.text.size)
                                                .then(tm)
                                        } else {
                                            Mat::translate(tx, 0.0).then(tm)
                                        };
                                    }
                                }
                            }
                        }
                    }
                }
                // Type 3 glyph metrics (rewritten by the pre-pass); a `d1`
                // glyph is a stencil — its colour operators are ignored and
                // the text fill colour paints it.
                "dZero" => {}
                "dOne" => st.fixed_color = true,
                // --- XObjects, images, shadings -----------------------------------
                "Do" => {
                    if let Some(xobj) = args
                        .first()
                        .and_then(name)
                        .and_then(|nm| resource(doc, resources, b"XObject", nm))
                    {
                        let id = match xobj {
                            Object::Reference(id) => Some(*id),
                            _ => None,
                        };
                        if let Some(stream) = as_stream(doc, xobj) {
                            match get_name(doc, &stream.dict, b"Subtype") {
                                Some(b"Image") => crate::timing::timed("render.image", || {
                                    self.draw_image(stream, id, &st, resources)
                                }),
                                Some(b"Form") => self.draw_form(stream, &st, resources, depth + 1),
                                Some(b"PS") => {}
                                _ => {
                                    if stream.dict.has(b"BBox") {
                                        self.draw_form(stream, &st, resources, depth + 1);
                                    } else if stream.dict.has(b"Width") {
                                        self.draw_image(stream, id, &st, resources);
                                    }
                                }
                            }
                        }
                    }
                }
                "BIX" => {
                    // An inline image the pre-pass cut out (`/I<n> BIX`).
                    if let Some(s) = args
                        .first()
                        .and_then(super::prepass::inline_index)
                        .and_then(|n| inline.get(n))
                    {
                        self.draw_image(s, None, &st, resources);
                    }
                }
                "sh" => {
                    if let Some(sh) = args
                        .first()
                        .and_then(name)
                        .and_then(|nm| resource(doc, resources, b"Shading", nm))
                    {
                        let sh_obj = deref(doc, sh).clone();
                        crate::timing::timed("render.sh", || {
                            self.paint_shading(
                                &sh_obj,
                                st.ctm,
                                &st,
                                None,
                                st.eff_fill_alpha(),
                                true,
                            )
                        });
                    }
                }
                "BX" => compat += 1,
                "EX" => compat = compat.saturating_sub(1),
                "BMC" | "BDC" | "EMC" | "MP" | "DP" => {}
                _ => {}
            }
        }
        let _ = compat;
    }

    // ---------------------------------------------------------------------
    // ExtGState
    // ---------------------------------------------------------------------

    fn apply_extgstate(
        &mut self,
        st: &mut GState,
        gs: &Dictionary,
        resources: Option<&Dictionary>,
    ) {
        let doc = self.doc;
        for (k, v) in gs.iter() {
            let v = deref(doc, v);
            match k.as_slice() {
                b"LW" => {
                    if let Some(w) = num(v) {
                        st.line_width = w.abs();
                    }
                }
                b"LC" => {
                    st.line_cap = match num(v).unwrap_or(0.0) as i64 {
                        1 => LineCap::Round,
                        2 => LineCap::Square,
                        _ => LineCap::Butt,
                    }
                }
                b"LJ" => {
                    st.line_join = match num(v).unwrap_or(0.0) as i64 {
                        1 => LineJoin::Round,
                        2 => LineJoin::Bevel,
                        _ => LineJoin::Miter,
                    }
                }
                b"ML" => {
                    if let Some(m) = num(v) {
                        st.miter_limit = m.max(1.0);
                    }
                }
                b"D" => {
                    if let Object::Array(a) = v {
                        let arr: Vec<f64> =
                            a.first().and_then(|o| nums(doc, o)).unwrap_or_default();
                        let phase = a.get(1).and_then(|o| num(deref(doc, o))).unwrap_or(0.0);
                        st.dash = if arr.is_empty() || arr.iter().all(|x| *x <= 0.0) {
                            None
                        } else {
                            Some((arr, phase))
                        };
                    }
                }
                b"CA" => {
                    if let Some(a) = num(v) {
                        st.stroke_alpha = a.clamp(0.0, 1.0);
                    }
                }
                b"ca" => {
                    if let Some(a) = num(v) {
                        st.fill_alpha = a.clamp(0.0, 1.0);
                    }
                }
                b"BM" => {
                    let nm = match v {
                        Object::Name(n) => Some(n.as_slice()),
                        Object::Array(a) => a.first().and_then(name),
                        _ => None,
                    };
                    st.blend = match nm {
                        Some(b"Multiply") => BlendMode::Multiply,
                        Some(b"Screen") => BlendMode::Screen,
                        Some(b"Overlay") => BlendMode::Overlay,
                        Some(b"Darken") => BlendMode::Darken,
                        Some(b"Lighten") => BlendMode::Lighten,
                        Some(b"ColorDodge") => BlendMode::ColorDodge,
                        Some(b"ColorBurn") => BlendMode::ColorBurn,
                        Some(b"HardLight") => BlendMode::HardLight,
                        Some(b"SoftLight") => BlendMode::SoftLight,
                        Some(b"Difference") => BlendMode::Difference,
                        Some(b"Exclusion") => BlendMode::Exclusion,
                        Some(b"Hue") => BlendMode::Hue,
                        Some(b"Saturation") => BlendMode::Saturation,
                        Some(b"Color") => BlendMode::Color,
                        Some(b"Luminosity") => BlendMode::Luminosity,
                        _ => BlendMode::SourceOver,
                    };
                }
                b"Font" => {
                    if let Object::Array(a) = v {
                        if let (Some(f), Some(sz)) =
                            (a.first(), a.get(1).and_then(|o| num(deref(doc, o))))
                        {
                            if let Some(font) = self.shared.font(doc, f) {
                                st.text.font = Some(font);
                                st.text.size = sz;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let _ = resources;
    }

    // ---------------------------------------------------------------------
    // Clipping
    // ---------------------------------------------------------------------

    /// Intersect the clip with the just-painted path.
    fn apply_clip(
        &mut self,
        st: &mut GState,
        user_path: Option<&Path>,
        ps: &PathState,
        rule: FillRule,
    ) {
        // An empty path clips everything.
        let Some(up) = user_path else {
            st.clip_box = Box2::new(0.0, 0.0, 0.0, 0.0);
            return;
        };
        // A single axis-aligned rectangle (the common `re W n`) stays a box.
        if ps.only_rects && ps.rects.len() == 1 && st.ctm.b.abs() < 1e-9 && st.ctm.c.abs() < 1e-9 {
            let r = ps.rects[0];
            let b = Box2::new(r[0], r[1], r[0] + r[2], r[1] + r[3]).transformed(st.ctm);
            st.clip_box = st.clip_box.intersect(b);
            if st.clip_box.is_empty() {
                st.clip_box = Box2::new(0.0, 0.0, 0.0, 0.0);
            }
            return;
        }
        let Some(dev) = up.clone().transform(st.ctm.to_ts()) else {
            return;
        };
        let bb = dev.bounds();
        let pb = Box2::new(
            f64::from(bb.left()),
            f64::from(bb.top()),
            f64::from(bb.right()),
            f64::from(bb.bottom()),
        );
        // A rectangle drawn with m/l/l/l/h (what most producers emit) is a
        // box clip too: every vertex sits on a corner of the bounding box.
        if device_path_is_rect(&dev) {
            st.clip_box = st.clip_box.intersect(pb);
            if st.clip_box.is_empty() {
                st.clip_box = Box2::new(0.0, 0.0, 0.0, 0.0);
            }
            return;
        }
        let new_box = st.clip_box.intersect(pb);
        if new_box.is_empty() {
            st.clip_box = Box2::new(0.0, 0.0, 0.0, 0.0);
            st.clip_mask = None;
            return;
        }
        let parent_ptr = st
            .clip_mask
            .as_ref()
            .map(|m| Rc::as_ptr(m) as usize)
            .unwrap_or(0);
        let key = (
            path_hash(&dev, st.clip_box),
            parent_ptr,
            rule == FillRule::EvenOdd,
        );
        if let Some(m) = self.shape_masks.get(&key) {
            st.clip_mask = Some(m.clone());
            st.clip_box = new_box;
            return;
        }
        let mask = crate::timing::timed("render.clip_mask", || {
            shape_mask(
                self.canvas.width(),
                self.canvas.height(),
                &dev,
                rule,
                new_box,
                st.clip_mask.as_deref(),
            )
        });
        let Some(mask) = mask else { return };
        let mask = Rc::new(mask);
        if self.shape_masks.len() > 256 {
            self.shape_masks.clear();
        }
        self.shape_masks.insert(key, mask.clone());
        st.clip_mask = Some(mask);
        st.clip_box = new_box;
    }

    /// The mask to draw with: the shape mask, or a cached one for the
    /// rectangle clip — none when the clip is the whole canvas, or when
    /// `bounds` (the shape's device box) lies inside the rectangle anyway,
    /// which is nearly every glyph of a clipped text block.
    fn clip_mask_for(&mut self, st: &GState, bounds: Option<tiny_skia::Rect>) -> Option<Rc<Mask>> {
        if let Some(m) = &st.clip_mask {
            return Some(m.clone());
        }
        let cb = st.clip_box;
        if cb.x0 <= 0.0 && cb.y0 <= 0.0 && cb.x1 >= self.device.x1 && cb.y1 >= self.device.y1 {
            return None;
        }
        if let Some(b) = bounds {
            if f64::from(b.left()) >= cb.x0 - 1e-6
                && f64::from(b.top()) >= cb.y0 - 1e-6
                && f64::from(b.right()) <= cb.x1 + 1e-6
                && f64::from(b.bottom()) <= cb.y1 + 1e-6
            {
                return None;
            }
        }
        let key = [
            (cb.x0 * 4.0).round() as i32,
            (cb.y0 * 4.0).round() as i32,
            (cb.x1 * 4.0).round() as i32,
            (cb.y1 * 4.0).round() as i32,
        ];
        if let Some(m) = self.rect_masks.get(&key) {
            return Some(m.clone());
        }
        let mut m = Mask::new(self.canvas.width(), self.canvas.height())?;
        let r = tiny_skia::Rect::from_ltrb(cb.x0 as f32, cb.y0 as f32, cb.x1 as f32, cb.y1 as f32)?;
        m.fill_path(
            &PathBuilder::from_rect(r),
            FillRule::Winding,
            true,
            Transform::identity(),
        );
        let m = Rc::new(m);
        if self.rect_masks.len() > 64 {
            self.rect_masks.clear();
        }
        self.rect_masks.insert(key, m.clone());
        Some(m)
    }

    fn visible(&self, st: &GState, dev_bounds: tiny_skia::Rect) -> bool {
        let b = Box2::new(
            f64::from(dev_bounds.left()),
            f64::from(dev_bounds.top()),
            f64::from(dev_bounds.right()),
            f64::from(dev_bounds.bottom()),
        );
        !st.clip_box.intersect(b).is_empty()
            || (b.width() == 0.0 || b.height() == 0.0) && !st.clip_box.is_empty()
    }

    // ---------------------------------------------------------------------
    // Painting
    // ---------------------------------------------------------------------

    fn paint_for(rgb: [f64; 3], alpha: f64, blend: BlendMode) -> Paint<'static> {
        let mut p = Paint::default();
        p.set_color_rgba8(to_u8(rgb[0]), to_u8(rgb[1]), to_u8(rgb[2]), to_u8(alpha));
        p.anti_alias = true;
        p.blend_mode = blend;
        p
    }

    /// Fill a device-space path with the fill colour or pattern.
    fn fill_device_path(
        &mut self,
        dev: &Path,
        rule: FillRule,
        st: &GState,
        resources: Option<&Dictionary>,
        is_text: bool,
    ) {
        if st.eff_fill_alpha() <= 1.0 / 512.0 || !self.visible(st, dev.bounds()) {
            return;
        }
        if let Some(pat) = st.fill_pattern.clone() {
            crate::timing::timed("render.pattern_fill", || {
                self.fill_with_pattern(dev, rule, &pat, st, resources)
            });
            return;
        }
        let Some(rgb) = st.fill_rgb else { return };
        let paint = Self::paint_for(rgb, st.eff_fill_alpha(), st.eff_blend());
        let mask = self.clip_mask_for(st, Some(dev.bounds()));
        let stage = if is_text {
            "render.glyph_fill"
        } else {
            "render.path_fill"
        };
        crate::timing::timed(stage, || {
            self.canvas
                .fill_path(dev, &paint, rule, Transform::identity(), mask.as_deref())
        });
    }

    fn stroke_of(&self, st: &GState) -> Stroke {
        let scale = st.ctm.mean_scale();
        let width = (st.line_width * scale).max(1.0);
        let dash = st.dash.as_ref().and_then(|(arr, phase)| {
            let mut a: Vec<f32> = arr.iter().map(|v| (v * scale) as f32).collect();
            if a.len() % 2 == 1 {
                let c = a.clone();
                a.extend(c);
            }
            if a.iter().all(|v| *v <= 0.0) || a.iter().any(|v| !v.is_finite()) {
                return None;
            }
            StrokeDash::new(a, (phase * scale) as f32)
        });
        Stroke {
            width: width as f32,
            miter_limit: st.miter_limit as f32,
            line_cap: st.line_cap,
            line_join: st.line_join,
            dash,
        }
    }

    fn stroke_device_path(
        &mut self,
        dev: &Path,
        _user: &Path,
        st: &GState,
        resources: Option<&Dictionary>,
    ) {
        if st.eff_stroke_alpha() <= 1.0 / 512.0 {
            return;
        }
        let stroke = self.stroke_of(st);
        let bb = dev.bounds();
        let grow = stroke.width;
        let Some(bb) = tiny_skia::Rect::from_ltrb(
            bb.left() - grow,
            bb.top() - grow,
            bb.right() + grow,
            bb.bottom() + grow,
        ) else {
            return;
        };
        if !self.visible(st, bb) {
            return;
        }
        if let Some(pat) = st.stroke_pattern.clone() {
            // Stroke with a pattern: the stroked outline, filled by the pattern.
            if let Some(outline) = dev.stroke(&stroke, 1.0) {
                self.fill_with_pattern(&outline, FillRule::Winding, &pat, st, resources);
            }
            return;
        }
        let Some(rgb) = st.stroke_rgb else { return };
        let paint = Self::paint_for(rgb, st.eff_stroke_alpha(), st.eff_blend());
        let mask = self.clip_mask_for(st, Some(bb));
        crate::timing::timed("render.stroke", || {
            self.canvas
                .stroke_path(dev, &paint, &stroke, Transform::identity(), mask.as_deref())
        });
    }

    // ---------------------------------------------------------------------
    // Text
    // ---------------------------------------------------------------------

    fn show_text(
        &mut self,
        bytes: &[u8],
        tm: &mut Mat,
        st: &GState,
        resources: Option<&Dictionary>,
        depth: usize,
    ) {
        let Some(font) = st.text.font.clone() else {
            return;
        };
        let ts = &st.text;
        let mode = ts.render_mode;
        let invisible = mode == 3 || mode == 7;
        let do_fill = matches!(mode, 0 | 2 | 4 | 6);
        let do_stroke = matches!(mode, 1 | 2 | 5 | 6);
        for (code, cid, single_byte) in font.decode(bytes) {
            let w0 = font.advance(code, cid);
            let trm = Mat::new(ts.size * ts.hscale, 0.0, 0.0, ts.size, 0.0, ts.rise)
                .then(*tm)
                .then(st.ctm);
            if !invisible {
                if let Some(t3) = &font.type3 {
                    self.draw_type3_glyph(&font, t3, code, trm, st, resources, depth);
                } else if let Some(glyph) = font.glyph(code, cid) {
                    if let Some(dev) = (*glyph).clone().transform(trm.to_ts()) {
                        if do_fill {
                            self.fill_device_path(&dev, FillRule::Winding, st, resources, true);
                        }
                        if do_stroke {
                            let mut sst = st.clone();
                            // Text stroke width is in user space, not text space.
                            sst.line_width = st.line_width;
                            self.stroke_device_path(&dev, &dev, &sst, resources);
                        }
                    }
                } else if !font.has_program() && (do_fill || do_stroke) {
                    self.draw_missing_glyph_box(w0, trm, st);
                }
            }
            let mut adv = if font.vertical {
                // Vertical writing: advance down by the default 1 em.
                0.0
            } else {
                w0 * ts.size + ts.char_spacing
            };
            if single_byte && code == 32 {
                adv += ts.word_spacing;
            }
            if font.vertical {
                let ty = -(ts.size
                    + ts.char_spacing
                    + if single_byte && code == 32 {
                        ts.word_spacing
                    } else {
                        0.0
                    });
                *tm = Mat::translate(0.0, ty).then(*tm);
            } else {
                *tm = Mat::translate(adv * ts.hscale, 0.0).then(*tm);
            }
        }
    }

    /// docling-parse's fallback for a cell no face could draw: a 0.5 px
    /// outline in `#1070C0` around the glyph box.
    fn draw_missing_glyph_box(&mut self, w0: f64, trm: Mat, st: &GState) {
        if !self.warned_no_face {
            self.warned_no_face = true;
            eprintln!(
                "docling-pdf: no fallback font face on this host — text without an embedded font is drawn as boxes \
                 (install Liberation/DejaVu fonts, or point DOCLING_RS_FONT_DIRS at a font directory)"
            );
        }
        let w = if w0 > 0.0 { w0 } else { 0.5 };
        let pts = [
            trm.apply(0.0, -0.2),
            trm.apply(w, -0.2),
            trm.apply(w, 0.8),
            trm.apply(0.0, 0.8),
        ];
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0 as f32, pts[0].1 as f32);
        for p in &pts[1..] {
            pb.line_to(p.0 as f32, p.1 as f32);
        }
        pb.close();
        let Some(path) = pb.finish() else { return };
        let mut paint = Paint::default();
        paint.set_color_rgba8(0x10, 0x70, 0xC0, 0xFF);
        paint.anti_alias = true;
        let stroke = Stroke {
            width: 0.5,
            ..Stroke::default()
        };
        let mask = self.clip_mask_for(st, Some(path.bounds()));
        self.canvas.stroke_path(
            &path,
            &paint,
            &stroke,
            Transform::identity(),
            mask.as_deref(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_type3_glyph(
        &mut self,
        font: &LoadedFont,
        t3: &super::font::Type3,
        code: u32,
        trm: Mat,
        st: &GState,
        resources: Option<&Dictionary>,
        depth: usize,
    ) {
        if self.type3_depth > 4 || depth > MAX_DEPTH {
            return;
        }
        let name = font_type3_name(font, code);
        let Some(name) = name else { return };
        let Some(id) = t3.char_procs.get(&name) else {
            return;
        };
        let Ok(Object::Stream(proc_stream)) = self.doc.get_object(*id) else {
            return;
        };
        let Some(content) = crate::textparse::content_stream_bytes(proc_stream) else {
            return;
        };
        let mut gst = st.clone();
        gst.ctm = t3.font_matrix.then(trm);
        // A glyph procedure starts with the text's fill colour; `d1` glyphs
        // ignore colour operators (their ink is the fill colour).
        let res = t3.resources.as_ref().or(resources);
        self.type3_depth += 1;
        self.run(&content, res, gst, depth + 1);
        self.type3_depth -= 1;
    }

    // ---------------------------------------------------------------------
    // XObjects
    // ---------------------------------------------------------------------

    fn draw_form(
        &mut self,
        stream: &lopdf::Stream,
        st: &GState,
        parent_res: Option<&Dictionary>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let doc = self.doc;
        let d = &stream.dict;
        let mut gst = st.clone();
        if get_dict(doc, d, b"Group").is_some() {
            // 11.6.6: the group composites as a unit with the alpha and blend
            // mode in force at the `Do`; without a group buffer they are
            // pushed down onto its contents, and an inner `gs` sets alpha
            // relative to them.
            gst.group_alpha = st.eff_fill_alpha();
            gst.group_blend = st.eff_blend();
            gst.fill_alpha = 1.0;
            gst.stroke_alpha = 1.0;
            gst.blend = BlendMode::SourceOver;
        }
        if let Some(m) = get(doc, d, b"Matrix")
            .and_then(|o| nums(doc, o))
            .and_then(|v| Mat::from_slice(&v))
        {
            gst.ctm = m.then(gst.ctm);
        }
        if let Some(b) = get(doc, d, b"BBox")
            .and_then(|o| nums(doc, o))
            .filter(|b| b.len() == 4)
        {
            // Clip to the BBox in form space.
            let mut pb = PathBuilder::new();
            let (x0, y0, x1, y1) = (
                b[0].min(b[2]),
                b[1].min(b[3]),
                b[0].max(b[2]),
                b[1].max(b[3]),
            );
            pb.move_to(x0 as f32, y0 as f32);
            pb.line_to(x1 as f32, y0 as f32);
            pb.line_to(x1 as f32, y1 as f32);
            pb.line_to(x0 as f32, y1 as f32);
            pb.close();
            let ps = PathState {
                builder: PathBuilder::new(),
                current: None,
                start: None,
                rects: vec![[x0, y0, x1 - x0, y1 - y0]],
                only_rects: true,
                pending_clip: None,
                empty: false,
            };
            let up = pb.finish();
            self.apply_clip(&mut gst, up.as_ref(), &ps, FillRule::Winding);
            if gst.clip_box.is_empty() {
                return;
            }
        }
        let res = get_dict(doc, d, b"Resources").or(parent_res);
        let Some(content) = crate::textparse::content_stream_bytes(stream) else {
            return;
        };
        self.run(&content, res, gst, depth);
    }

    fn draw_image(
        &mut self,
        stream: &lopdf::Stream,
        id: Option<ObjectId>,
        st: &GState,
        resources: Option<&Dictionary>,
    ) {
        if st.eff_fill_alpha() <= 1.0 / 512.0 {
            return;
        }
        let doc = self.doc;
        // The unit square's device extent decides the reduction factor.
        let (bx0, by0, bx1, by1) = st.ctm.unit_bbox();
        let dev_box = Box2::new(bx0, by0, bx1, by1);
        if st.clip_box.intersect(dev_box).is_empty() {
            return;
        }
        let dst_w = st.ctm.apply_vec(1.0, 0.0);
        let dst_h = st.ctm.apply_vec(0.0, 1.0);
        let dst_w = dst_w.0.hypot(dst_w.1).max(1.0);
        let dst_h = dst_h.0.hypot(dst_h.1).max(1.0);
        let target = (dst_w.ceil() as u32, dst_h.ceil() as u32);
        let is_mask = get_bool(doc, &stream.dict, b"ImageMask")
            .or_else(|| get_bool(doc, &stream.dict, b"IM"))
            .unwrap_or(false);
        let fill = match (is_mask, st.fill_rgb) {
            (true, Some(rgb)) => [to_u8(rgb[0]), to_u8(rgb[1]), to_u8(rgb[2])],
            (true, None) => return,
            _ => [0, 0, 0],
        };
        // docling-parse decodes a JPEG no larger than the page needs at its
        // bitmap hint (a scan at a quarter for the 72 dpi hint, then blitted
        // up onto this canvas): the drawn extent in PDF units against the
        // declared size picks the reduced inverse DCT.
        let reduction = image::declared_size(doc, &stream.dict)
            .map(|(sw, sh)| {
                let ux = self.base.a.hypot(self.base.b).max(1e-9);
                let uy = self.base.c.hypot(self.base.d).max(1e-9);
                image::codec_reduction_shift(
                    (bx1 - bx0) / ux,
                    (by1 - by0) / uy,
                    sw,
                    sh,
                    self.bitmap_hint,
                )
            })
            .unwrap_or(0);
        let key = id.map(|id| (id, target.0, target.1, fill));
        let img = match key.and_then(|k| self.images.get(&k).cloned()) {
            Some(i) => i,
            None => {
                let rect = [bx0, by0, bx1, by1];
                let mut placeholder = None;
                let decoded = match crate::timing::timed("render.image_decode", || {
                    let loaded = self
                        .shared
                        .load_image(doc, stream, id, resources, reduction)?;
                    placeholder = loaded.placeholder_reason().map(str::to_string);
                    let mut cmyk = self.shared.cmyk.borrow_mut();
                    image::rasterize(&loaded, fill, Some(target), &mut cmyk)
                }) {
                    Ok(d) => Rc::new(d),
                    Err(e) => {
                        docling_core::debug_log!("docling-pdf render: image skipped ({e})");
                        self.shared.omit(id, rect, format!("image not drawn ({e})"));
                        return;
                    }
                };
                if let Some(why) = placeholder {
                    self.shared.omit(id, rect, why);
                }
                if let Some(k) = key {
                    if self.images.len() > 256 {
                        self.images.clear();
                    }
                    self.images.insert(k, decoded.clone());
                }
                decoded
            }
        };
        // Image space (w × h, y down) → unit square → device.
        let (pw, ph) = (
            f64::from(img.pixmap.width()),
            f64::from(img.pixmap.height()),
        );
        docling_core::debug_log!(
            "docling-pdf render: image {:?} reduction 1/{} → pixmap {pw}×{ph} (src {}×{}) for {target:?} px",
            id,
            1u32 << reduction,
            img.src_width,
            img.src_height
        );
        let to_unit = Mat::new(1.0 / pw, 0.0, 0.0, -1.0 / ph, 0.0, 1.0);
        let m = to_unit.then(st.ctm);
        let paint = PixmapPaint {
            opacity: st.eff_fill_alpha() as f32,
            blend_mode: st.eff_blend(),
            quality: if dst_w >= pw && dst_h >= ph && pw * ph < 4.0 {
                FilterQuality::Nearest
            } else {
                FilterQuality::Bilinear
            },
        };
        let img_bounds = tiny_skia::Rect::from_ltrb(
            dev_box.x0 as f32,
            dev_box.y0 as f32,
            dev_box.x1 as f32,
            dev_box.y1 as f32,
        );
        let mask = self.clip_mask_for(st, img_bounds);
        self.canvas.draw_pixmap(
            0,
            0,
            img.pixmap.as_ref(),
            &paint,
            m.to_ts(),
            mask.as_deref(),
        );
    }

    // ---------------------------------------------------------------------
    // Shadings and patterns
    // ---------------------------------------------------------------------

    /// Paint a shading dictionary/stream through `matrix` (shading space →
    /// device), restricted to `area` (a device path) or to the clip.
    fn paint_shading(
        &mut self,
        sh: &Object,
        matrix: Mat,
        st: &GState,
        area: Option<(&Path, FillRule)>,
        alpha: f64,
        background: bool,
    ) {
        let doc = self.doc;
        let Some(d) = as_dict(doc, sh) else { return };
        let stype = get_int(doc, d, b"ShadingType").unwrap_or(0);
        let cs = get(doc, d, b"ColorSpace")
            .and_then(|o| ColorSpace::parse(doc, o, None))
            .unwrap_or(ColorSpace::DeviceRGB);
        let func = d
            .get(b"Function")
            .ok()
            .and_then(|o| Function::parse(doc, o));
        let mask = self.clip_mask_for(st, area.map(|(p, _)| p.bounds()));
        // The region to paint: the given path, else the clip box.
        let region: Path = match area {
            Some((p, _)) => p.clone(),
            None => {
                let cb = st.clip_box;
                let Some(r) = tiny_skia::Rect::from_ltrb(
                    cb.x0 as f32,
                    cb.y0 as f32,
                    cb.x1 as f32,
                    cb.y1 as f32,
                ) else {
                    return;
                };
                PathBuilder::from_rect(r)
            }
        };
        let rule = area.map(|(_, r)| r).unwrap_or(FillRule::Winding);
        let _ = background;
        match stype {
            2 | 3 => {
                let Some(coords) = get(doc, d, b"Coords").and_then(|o| nums(doc, o)) else {
                    return;
                };
                let domain = get(doc, d, b"Domain")
                    .and_then(|o| nums(doc, o))
                    .unwrap_or(vec![0.0, 1.0]);
                let extend = get(doc, d, b"Extend")
                    .and_then(|o| match o {
                        Object::Array(a) => Some(
                            a.iter()
                                .map(|b| matches!(deref(doc, b), Object::Boolean(true)))
                                .collect::<Vec<bool>>(),
                        ),
                        _ => None,
                    })
                    .unwrap_or(vec![false, false]);
                let (e0, e1) = (
                    extend.first().copied().unwrap_or(false),
                    extend.get(1).copied().unwrap_or(false),
                );
                let Some(f) = func.as_ref() else { return };
                let stops = sample_stops(f, &cs, &domain, alpha, e0, e1);
                if stops.len() < 2 {
                    return;
                }
                let shader = if stype == 2 {
                    if coords.len() < 4 {
                        return;
                    }
                    tiny_skia::LinearGradient::new(
                        Point::from_xy(coords[0] as f32, coords[1] as f32),
                        Point::from_xy(coords[2] as f32, coords[3] as f32),
                        stops,
                        SpreadMode::Pad,
                        matrix.to_ts(),
                    )
                } else {
                    if coords.len() < 6 {
                        return;
                    }
                    tiny_skia::RadialGradient::new(
                        Point::from_xy(coords[0] as f32, coords[1] as f32),
                        coords[2].max(0.0) as f32,
                        Point::from_xy(coords[3] as f32, coords[4] as f32),
                        coords[5].max(0.0) as f32,
                        stops,
                        SpreadMode::Pad,
                        matrix.to_ts(),
                    )
                };
                let Some(shader) = shader else { return };
                let paint = Paint {
                    shader,
                    blend_mode: st.eff_blend(),
                    anti_alias: true,
                    force_hq_pipeline: false,
                    colorspace: tiny_skia::ColorSpace::Linear,
                };
                self.canvas.fill_path(
                    &region,
                    &paint,
                    rule,
                    Transform::identity(),
                    mask.as_deref(),
                );
            }
            1 => {
                // Function-based: sample the domain on a grid, draw as an image.
                let domain = get(doc, d, b"Domain")
                    .and_then(|o| nums(doc, o))
                    .unwrap_or(vec![0.0, 1.0, 0.0, 1.0]);
                let fm = get(doc, d, b"Matrix")
                    .and_then(|o| nums(doc, o))
                    .and_then(|v| Mat::from_slice(&v))
                    .unwrap_or(Mat::IDENTITY);
                let Some(f) = func.as_ref() else { return };
                if domain.len() < 4 {
                    return;
                }
                const N: u32 = 64;
                let Some(mut pm) = Pixmap::new(N, N) else {
                    return;
                };
                {
                    let px = pm.pixels_mut();
                    for j in 0..N {
                        for i in 0..N {
                            let x = domain[0]
                                + (domain[1] - domain[0]) * (f64::from(i) + 0.5) / f64::from(N);
                            let y = domain[2]
                                + (domain[3] - domain[2]) * (f64::from(j) + 0.5) / f64::from(N);
                            let out = f.eval(&[x, y]);
                            let rgb = cs.to_rgb(&out).unwrap_or([0.0, 0.0, 0.0]);
                            px[(j * N + i) as usize] = tiny_skia::PremultipliedColorU8::from_rgba(
                                to_u8(rgb[0]),
                                to_u8(rgb[1]),
                                to_u8(rgb[2]),
                                255,
                            )
                            .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
                        }
                    }
                }
                // pixmap (N×N, y down) → domain rect → shading space → device.
                let to_domain = Mat::new(
                    (domain[1] - domain[0]) / f64::from(N),
                    0.0,
                    0.0,
                    (domain[3] - domain[2]) / f64::from(N),
                    domain[0],
                    domain[2],
                );
                let m = to_domain.then(fm).then(matrix);
                let shader = tiny_skia::Pattern::new(
                    pm.as_ref(),
                    SpreadMode::Pad,
                    FilterQuality::Bilinear,
                    alpha as f32,
                    m.to_ts(),
                );
                let paint = Paint {
                    shader,
                    blend_mode: st.eff_blend(),
                    anti_alias: true,
                    force_hq_pipeline: false,
                    colorspace: tiny_skia::ColorSpace::Linear,
                };
                self.canvas.fill_path(
                    &region,
                    &paint,
                    rule,
                    Transform::identity(),
                    mask.as_deref(),
                );
            }
            4..=7 => {
                let Some(stream) = as_stream(doc, sh) else {
                    return;
                };
                let Ok(data) = stream.decompressed_content() else {
                    return;
                };
                let region_mask = if area.is_some() {
                    // Mesh patches are clipped to the fill region through a mask.
                    let mut m = match mask.as_deref() {
                        Some(m) => m.clone(),
                        None => {
                            let Some(m) = Mask::new(self.canvas.width(), self.canvas.height())
                            else {
                                return;
                            };
                            let mut m = m;
                            m.fill_path(
                                &PathBuilder::from_rect(
                                    tiny_skia::Rect::from_ltrb(
                                        0.0,
                                        0.0,
                                        self.device.x1 as f32,
                                        self.device.y1 as f32,
                                    )
                                    .unwrap(),
                                ),
                                FillRule::Winding,
                                true,
                                Transform::identity(),
                            );
                            m
                        }
                    };
                    m.intersect_path(&region, rule, true, Transform::identity());
                    Some(Rc::new(m))
                } else {
                    mask.clone()
                };
                self.paint_mesh(
                    stype,
                    d,
                    &data,
                    &cs,
                    func.as_ref(),
                    matrix,
                    st,
                    region_mask.as_deref(),
                    alpha,
                );
            }
            _ => {}
        }
    }

    /// Mesh shadings (types 4–7): each triangle / patch filled flat with the
    /// mean of its corner colours — the picture the layout model needs.
    #[allow(clippy::too_many_arguments)]
    fn paint_mesh(
        &mut self,
        stype: i64,
        d: &Dictionary,
        data: &[u8],
        cs: &ColorSpace,
        func: Option<&Function>,
        matrix: Mat,
        st: &GState,
        mask: Option<&Mask>,
        alpha: f64,
    ) {
        let doc = self.doc;
        let bpc = get_int(doc, d, b"BitsPerCoordinate")
            .unwrap_or(16)
            .clamp(1, 32) as u32;
        let bpcomp = get_int(doc, d, b"BitsPerComponent")
            .unwrap_or(16)
            .clamp(1, 16) as u32;
        let bpf = get_int(doc, d, b"BitsPerFlag").unwrap_or(8).clamp(2, 8) as u32;
        let decode = get(doc, d, b"Decode")
            .and_then(|o| nums(doc, o))
            .unwrap_or_default();
        let ncomp = if func.is_some() { 1 } else { cs.components() };
        if decode.len() < 4 + 2 * ncomp {
            return;
        }
        let mut reader = BitReader::new(data);
        let read_val = |r: &mut BitReader, bits: u32, dmin: f64, dmax: f64| -> Option<f64> {
            let v = r.read(bits)?;
            let max = if bits >= 32 {
                u32::MAX as f64
            } else {
                ((1u64 << bits) - 1) as f64
            };
            Some(dmin + f64::from(v) * (dmax - dmin) / max)
        };
        let color_of = |comps: &[f64]| -> [f64; 3] {
            let out = match func {
                Some(f) => f.eval(&comps[..1]),
                None => comps.to_vec(),
            };
            cs.to_rgb(&out).unwrap_or([0.0, 0.0, 0.0])
        };
        let mut triangles: Vec<Triangle> = Vec::new();
        let mut patches: Vec<Patch> = Vec::new();
        let read_vertex = |r: &mut BitReader| -> Option<Vertex> {
            let x = read_val(r, bpc, decode[0], decode[1])?;
            let y = read_val(r, bpc, decode[2], decode[3])?;
            let mut comps = Vec::with_capacity(ncomp);
            for c in 0..ncomp {
                comps.push(read_val(r, bpcomp, decode[4 + 2 * c], decode[5 + 2 * c])?);
            }
            Some(((x, y), color_of(&comps)))
        };
        match stype {
            4 => {
                let mut prev: Vec<((f64, f64), [f64; 3])> = Vec::new();
                let mut count = 0;
                while let Some(flag) = reader.read(bpf) {
                    let Some(v) = read_vertex(&mut reader) else {
                        break;
                    };
                    reader.align();
                    if flag == 0 {
                        // Start a new triangle: two more vertices with flags.
                        let mut tri = vec![v];
                        for _ in 0..2 {
                            let _ = reader.read(bpf);
                            let Some(v2) = read_vertex(&mut reader) else {
                                break;
                            };
                            reader.align();
                            tri.push(v2);
                        }
                        if tri.len() == 3 {
                            prev = tri;
                        } else {
                            break;
                        }
                    } else if prev.len() == 3 {
                        if flag == 1 {
                            prev = vec![prev[1], prev[2], v];
                        } else {
                            prev = vec![prev[0], prev[2], v];
                        }
                    } else {
                        break;
                    }
                    let pts = [prev[0].0, prev[1].0, prev[2].0];
                    let col = mean3(&[prev[0].1, prev[1].1, prev[2].1]);
                    triangles.push((pts, col));
                    count += 1;
                    if count > 200_000 {
                        break;
                    }
                }
            }
            5 => {
                let per_row = get_int(doc, d, b"VerticesPerRow").unwrap_or(2).max(2) as usize;
                let mut rows: Vec<Vec<Vertex>> = Vec::new();
                loop {
                    let mut row = Vec::with_capacity(per_row);
                    for _ in 0..per_row {
                        match read_vertex(&mut reader) {
                            Some(v) => row.push(v),
                            None => break,
                        }
                    }
                    if row.len() < per_row {
                        break;
                    }
                    rows.push(row);
                    if rows.len() > 4096 {
                        break;
                    }
                }
                for r in 1..rows.len() {
                    for c in 1..per_row {
                        let (a, b, cc, dd) = (
                            rows[r - 1][c - 1],
                            rows[r - 1][c],
                            rows[r][c - 1],
                            rows[r][c],
                        );
                        triangles.push(([a.0, b.0, cc.0], mean3(&[a.1, b.1, cc.1])));
                        triangles.push(([b.0, dd.0, cc.0], mean3(&[b.1, dd.1, cc.1])));
                    }
                }
            }
            6 | 7 => {
                let npts = if stype == 6 { 12 } else { 16 };
                let mut prev_pts: Vec<(f64, f64)> = Vec::new();
                let mut prev_cols: Vec<[f64; 3]> = Vec::new();
                while let Some(flag) = reader.read(bpf) {
                    let (np, nc) = if flag == 0 { (npts, 4) } else { (npts - 4, 2) };
                    let mut pts = Vec::with_capacity(16);
                    let mut ok = true;
                    for _ in 0..np {
                        match (
                            read_val(&mut reader, bpc, decode[0], decode[1]),
                            read_val(&mut reader, bpc, decode[2], decode[3]),
                        ) {
                            (Some(x), Some(y)) => pts.push((x, y)),
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        break;
                    }
                    let mut cols = Vec::with_capacity(4);
                    for _ in 0..nc {
                        let mut comps = Vec::with_capacity(ncomp);
                        for c in 0..ncomp {
                            match read_val(
                                &mut reader,
                                bpcomp,
                                decode[4 + 2 * c],
                                decode[5 + 2 * c],
                            ) {
                                Some(v) => comps.push(v),
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if !ok {
                            break;
                        }
                        cols.push(color_of(&comps));
                    }
                    if !ok {
                        break;
                    }
                    reader.align();
                    // Shared edge from the previous patch (8.7.4.5.7, Table 85).
                    let (full_pts, full_cols) = if flag == 0 || prev_pts.len() < 12 {
                        if flag != 0 {
                            break;
                        }
                        (pts.clone(), cols.clone())
                    } else {
                        let p = &prev_pts;
                        let edge: [(f64, f64); 4] = match flag {
                            1 => [p[3], p[4], p[5], p[6]],
                            2 => [p[6], p[7], p[8], p[9]],
                            _ => [p[9], p[10], p[11], p[0]],
                        };
                        let pc = &prev_cols;
                        let ec: [[f64; 3]; 2] = match flag {
                            1 => [pc[1], pc[2]],
                            2 => [pc[2], pc[3]],
                            _ => [pc[3], pc[0]],
                        };
                        let mut fp = edge.to_vec();
                        fp.extend(pts.iter().copied());
                        let mut fc = ec.to_vec();
                        fc.extend(cols.iter().copied());
                        (fp, fc)
                    };
                    if full_pts.len() < 12 || full_cols.len() < 4 {
                        break;
                    }
                    patches.push((full_pts[..12].to_vec(), mean4(&full_cols)));
                    prev_pts = full_pts;
                    prev_cols = full_cols;
                    if patches.len() > 50_000 {
                        break;
                    }
                }
            }
            _ => {}
        }
        let ts = matrix.to_ts();
        for (pts, col) in &triangles {
            let mut pb = PathBuilder::new();
            pb.move_to(pts[0].0 as f32, pts[0].1 as f32);
            pb.line_to(pts[1].0 as f32, pts[1].1 as f32);
            pb.line_to(pts[2].0 as f32, pts[2].1 as f32);
            pb.close();
            if let Some(p) = pb.finish().and_then(|p| p.transform(ts)) {
                let mut paint = Self::paint_for(*col, alpha, st.eff_blend());
                paint.anti_alias = false;
                self.canvas
                    .fill_path(&p, &paint, FillRule::Winding, Transform::identity(), mask);
            }
        }
        for (pts, col) in &patches {
            // The boundary: four cubics p0 p1 p2 p3 | p3 p4 p5 p6 | p6 p7 p8 p9 | p9 p10 p11 p0.
            let mut pb = PathBuilder::new();
            pb.move_to(pts[0].0 as f32, pts[0].1 as f32);
            for k in 0..4 {
                let (a, b, c) = (
                    pts[(3 * k + 1) % 12],
                    pts[(3 * k + 2) % 12],
                    pts[(3 * k + 3) % 12],
                );
                pb.cubic_to(
                    a.0 as f32, a.1 as f32, b.0 as f32, b.1 as f32, c.0 as f32, c.1 as f32,
                );
            }
            pb.close();
            if let Some(p) = pb.finish().and_then(|p| p.transform(ts)) {
                let mut paint = Self::paint_for(*col, alpha, st.eff_blend());
                paint.anti_alias = false;
                self.canvas
                    .fill_path(&p, &paint, FillRule::Winding, Transform::identity(), mask);
            }
        }
    }

    /// Fill `dev` with a pattern: a shading pattern paints its shading
    /// through the path; a tiling pattern renders one cell into a pixmap and
    /// repeats it.
    fn fill_with_pattern(
        &mut self,
        dev: &Path,
        rule: FillRule,
        pat: &Object,
        st: &GState,
        resources: Option<&Dictionary>,
    ) {
        let doc = self.doc;
        let Some(pd) = as_dict(doc, pat) else { return };
        let ptype = get_int(doc, pd, b"PatternType").unwrap_or(2);
        let pmatrix = get(doc, pd, b"Matrix")
            .and_then(|o| nums(doc, o))
            .and_then(|v| Mat::from_slice(&v))
            .unwrap_or(Mat::IDENTITY);
        // Pattern space is the default space of the page (or form) the
        // pattern is a resource of: the base CTM recorded for this stream.
        let base = self.pattern_base(st);
        let m = pmatrix.then(base);
        if ptype == 2 {
            let Some(sh) = get(doc, pd, b"Shading") else {
                return;
            };
            let sh = sh.clone();
            self.paint_shading(&sh, m, st, Some((dev, rule)), st.eff_fill_alpha(), false);
            return;
        }
        // Tiling pattern.
        let Some(stream) = as_stream(doc, pat) else {
            return;
        };
        let Some(bbox) = get(doc, pd, b"BBox")
            .and_then(|o| nums(doc, o))
            .filter(|b| b.len() == 4)
        else {
            return;
        };
        let mut xstep = get_num(doc, pd, b"XStep")
            .unwrap_or(bbox[2] - bbox[0])
            .abs();
        let mut ystep = get_num(doc, pd, b"YStep")
            .unwrap_or(bbox[3] - bbox[1])
            .abs();
        if xstep < 1e-9 {
            xstep = (bbox[2] - bbox[0]).abs().max(1e-3);
        }
        if ystep < 1e-9 {
            ystep = (bbox[3] - bbox[1]).abs().max(1e-3);
        }
        let paint_type = get_int(doc, pd, b"PaintType").unwrap_or(1);
        let scale = m.max_scale().max(1e-6);
        let cell_w = (xstep * scale).ceil().clamp(1.0, 2048.0);
        let cell_h = (ystep * scale).ceil().clamp(1.0, 2048.0);
        let s_x = cell_w / xstep;
        let s_y = cell_h / ystep;
        let (x0, y0) = (bbox[0].min(bbox[2]), bbox[1].min(bbox[3]));
        // Pattern space → tile pixmap (y down).
        let to_tile = Mat::new(s_x, 0.0, 0.0, -s_y, -x0 * s_x, (y0 + ystep) * s_y);
        let Some(mut tile) = Pixmap::new(cell_w as u32, cell_h as u32) else {
            return;
        };
        tile.fill(tiny_skia::Color::TRANSPARENT);
        let Some(content) = crate::textparse::content_stream_bytes(stream) else {
            return;
        };
        let res = get_dict(doc, &stream.dict, b"Resources").or(resources);
        // Render the cell with a nested interpreter sharing the font cache.
        let mut inner = Interp {
            doc,
            canvas: tile,
            device: Box2::new(0.0, 0.0, cell_w, cell_h),
            shared: self.shared.clone(),
            images: HashMap::new(),
            rect_masks: HashMap::new(),
            shape_masks: HashMap::new(),
            ops: self.ops,
            warned_no_face: self.warned_no_face,
            type3_depth: self.type3_depth,
            base: to_tile,
            bitmap_hint: self.bitmap_hint,
        };
        let mut gst = inner.initial_state(to_tile);
        if paint_type == 2 {
            gst.fixed_color = true;
            gst.fill_rgb = st.fill_rgb;
            gst.stroke_rgb = st.fill_rgb;
        }
        inner.run(&content, res, gst, MAX_DEPTH - 2);
        self.ops = inner.ops;
        let tile = inner.canvas;
        // tile pixmap → pattern space → device.
        let Some(from_tile) = to_tile.invert() else {
            return;
        };
        let shader_m = from_tile.then(m);
        let shader = tiny_skia::Pattern::new(
            tile.as_ref(),
            SpreadMode::Repeat,
            FilterQuality::Bilinear,
            st.eff_fill_alpha() as f32,
            shader_m.to_ts(),
        );
        let paint = Paint {
            shader,
            blend_mode: st.eff_blend(),
            anti_alias: true,
            force_hq_pipeline: false,
            colorspace: tiny_skia::ColorSpace::Linear,
        };
        let mask = self.clip_mask_for(st, Some(dev.bounds()));
        self.canvas
            .fill_path(dev, &paint, rule, Transform::identity(), mask.as_deref());
    }

    /// The base transform for pattern space: the page's (or the enclosing
    /// form's) default user space. Recorded on the interpreter per stream;
    /// here the page base is used for everything, which is exact for page
    /// content and the common case for forms.
    fn pattern_base(&self, _st: &GState) -> Mat {
        self.base
    }
}

/// The coverage mask of a shape clip: `dev` (already in device space)
/// rasterized into a fresh mask, cut to `keep` (the new clip box) and
/// multiplied by the `parent` mask when the clip nests. Built by hand
/// rather than `Mask::intersect_path`: that rasterizes into a canvas-sized
/// temporary and multiplies the *whole* canvas, after a full-canvas
/// rectangle fill seeded the box clip — three or four passes over 4 MB
/// per clip, and a designer's page sets a few hundred clips. Here the only
/// full-canvas cost is the allocation; every other pass covers the shape's
/// bounding box.
fn shape_mask(
    w: u32,
    h: u32,
    dev: &Path,
    rule: FillRule,
    keep: Box2,
    parent: Option<&Mask>,
) -> Option<Mask> {
    let mut m = Mask::new(w, h)?;
    m.fill_path(dev, rule, true, Transform::identity());
    let bb = dev.bounds();
    let (w, h) = (w as usize, h as usize);
    // The rows and columns the shape may have touched.
    let bx0 = (bb.left().floor().max(0.0) as usize).min(w);
    let by0 = (bb.top().floor().max(0.0) as usize).min(h);
    let bx1 = ((bb.right().ceil().max(0.0) as usize) + 1).min(w);
    let by1 = ((bb.bottom().ceil().max(0.0) as usize) + 1).min(h);
    // The pixels that stay: the shape's box ∩ the clip box. A pixel a box
    // edge cuts through keeps the covered fraction (what the anti-aliased
    // rectangle mask this replaces gave it).
    let kx0 = (keep.x0.floor().max(0.0) as usize).clamp(bx0, bx1);
    let ky0 = (keep.y0.floor().max(0.0) as usize).clamp(by0, by1);
    let kx1 = (keep.x1.ceil().max(0.0) as usize).clamp(kx0, bx1);
    let ky1 = (keep.y1.ceil().max(0.0) as usize).clamp(ky0, by1);
    // Coverage of an edge pixel along one axis: the part of [i, i + 1)
    // inside [lo, hi).
    let edge = |i: usize, lo: f64, hi: f64| -> f64 {
        let (a, b) = (i as f64, i as f64 + 1.0);
        (b.min(hi) - a.max(lo)).clamp(0.0, 1.0)
    };
    let fx: Vec<f64> = (kx0..kx1).map(|x| edge(x, keep.x0, keep.x1)).collect();
    let plain_x = fx.iter().all(|&f| f >= 1.0);
    let data = m.data_mut();
    for y in by0..by1 {
        let row = &mut data[y * w..(y + 1) * w];
        if y < ky0 || y >= ky1 {
            row[bx0..bx1].fill(0);
            continue;
        }
        row[bx0..kx0].fill(0);
        row[kx1..bx1].fill(0);
        let fy = edge(y, keep.y0, keep.y1);
        let prow = parent.map(|p| &p.data()[y * w..(y + 1) * w]);
        if prow.is_none() && plain_x && fy >= 1.0 {
            continue;
        }
        for x in kx0..kx1 {
            let mut c = f64::from(row[x]) * fy * fx[x - kx0];
            if let Some(prow) = prow {
                c = c * f64::from(prow[x]) / 255.0;
            }
            row[x] = c.round().clamp(0.0, 255.0) as u8;
        }
    }
    Some(m)
}

/// A hash of a device path's geometry (and the box it is clipped into).
fn path_hash(p: &Path, clip_box: Box2) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for pt in p.points() {
        pt.x.to_bits().hash(&mut h);
        pt.y.to_bits().hash(&mut h);
    }
    for v in p.verbs() {
        (*v as u8).hash(&mut h);
    }
    for v in [clip_box.x0, clip_box.y0, clip_box.x1, clip_box.y1] {
        v.to_bits().hash(&mut h);
    }
    h.finish()
}

/// One closed sub-path of lines whose every vertex lies on a corner of its
/// bounding box — an axis-aligned rectangle (`get_axis_aligned_clip_rect`).
fn device_path_is_rect(p: &Path) -> bool {
    let b = p.bounds();
    if b.width() <= 1e-3 || b.height() <= 1e-3 {
        return false;
    }
    let mut moves = 0;
    let mut points = 0;
    for seg in p.segments() {
        match seg {
            tiny_skia::PathSegment::MoveTo(pt) => {
                moves += 1;
                if moves > 1 || !on_corner(pt, &b) {
                    return false;
                }
                points += 1;
            }
            tiny_skia::PathSegment::LineTo(pt) => {
                if !on_corner(pt, &b) {
                    return false;
                }
                points += 1;
            }
            tiny_skia::PathSegment::Close => {}
            _ => return false,
        }
    }
    (4..=5).contains(&points)
}

fn on_corner(pt: Point, b: &tiny_skia::Rect) -> bool {
    let eps = 1e-3;
    let on_x = (pt.x - b.left()).abs() <= eps || (pt.x - b.right()).abs() <= eps;
    let on_y = (pt.y - b.top()).abs() <= eps || (pt.y - b.bottom()).abs() <= eps;
    on_x && on_y
}

/// The Type 3 glyph name for a code: through the encoding's `/Differences`.
fn font_type3_name(font: &LoadedFont, code: u32) -> Option<String> {
    font.type3_glyph_name(code)
}

fn mean3(c: &[[f64; 3]; 3]) -> [f64; 3] {
    [
        (c[0][0] + c[1][0] + c[2][0]) / 3.0,
        (c[0][1] + c[1][1] + c[2][1]) / 3.0,
        (c[0][2] + c[1][2] + c[2][2]) / 3.0,
    ]
}

fn mean4(c: &[[f64; 3]]) -> [f64; 3] {
    let n = c.len().max(1) as f64;
    let mut out = [0.0; 3];
    for col in c {
        for k in 0..3 {
            out[k] += col[k] / n;
        }
    }
    out
}

/// Sample a shading function into gradient stops over `[t0, t1]`; a
/// non-extended end gets a transparent guard stop just past it, as
/// docling-parse does (Blend2D's pad mode is the only one that keeps the
/// interior ramp intact).
fn sample_stops(
    f: &Function,
    cs: &ColorSpace,
    domain: &[f64],
    alpha: f64,
    e0: bool,
    e1: bool,
) -> Vec<GradientStop> {
    let (t0, t1) = (
        domain.first().copied().unwrap_or(0.0),
        domain.get(1).copied().unwrap_or(1.0),
    );
    const N: usize = 64;
    let mut stops = Vec::with_capacity(N + 3);
    let a = to_u8(alpha);
    let guard = 1.0 / 1024.0;
    for i in 0..=N {
        let u = i as f64 / N as f64;
        let t = t0 + (t1 - t0) * u;
        let out = f.eval(&[t]);
        let rgb = cs.to_rgb(&out).unwrap_or([0.0, 0.0, 0.0]);
        let color = tiny_skia::Color::from_rgba8(to_u8(rgb[0]), to_u8(rgb[1]), to_u8(rgb[2]), a);
        let pos = if !e0 && i == 0 {
            guard
        } else if !e1 && i == N {
            1.0 - guard
        } else {
            u
        };
        stops.push(GradientStop::new(pos as f32, color));
    }
    if !e0 {
        stops.insert(0, GradientStop::new(0.0, tiny_skia::Color::TRANSPARENT));
    }
    if !e1 {
        stops.push(GradientStop::new(1.0, tiny_skia::Color::TRANSPARENT));
    }
    stops
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, pos: 0 }
    }

    fn read(&mut self, bits: u32) -> Option<u32> {
        if self.pos + bits as usize > self.data.len() * 8 {
            return None;
        }
        let mut v: u64 = 0;
        for _ in 0..bits {
            let byte = self.data[self.pos / 8];
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            v = (v << 1) | u64::from(bit);
            self.pos += 1;
        }
        Some(v as u32)
    }

    fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
}
