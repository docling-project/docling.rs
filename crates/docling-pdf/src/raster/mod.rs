//! Pure-Rust raster of an *image-only* page — a scan: one or a few `/Image`
//! XObjects blitted onto a white page — reproducing pdfium's bitmap byte for
//! byte, so the OCR path needs no `libpdfium` for the pages it runs on.
//!
//! Phase 2 of retiring pdfium (docs/PDF_CONFORMANCE.md, "Retiring pdfium").
//! The page is accepted only when everything pdfium would do to render it is
//! something this module does identically, and declined (`None`, the caller
//! falls back to pdfium or the docling-parse plugin) otherwise:
//!
//! * the content stream — through Form XObjects, with their `/Matrix` and
//!   `/BBox` — draws nothing but images: `q`/`Q`/`cm`, rectangular `re W n`
//!   clips, ExtGStates that change nothing visible, invisible (`3 Tr`) text,
//!   fill colours in DeviceGray/DeviceRGB (for stencil masks); a painted
//!   path, shading, inline image, non-rectangular clip, visible text, or a
//!   soft mask/blend/alpha declines the page;
//! * each image is 1/2/4/8-bit DeviceGray/CalGray, 8-bit
//!   DeviceRGB/CalRGB/sRGB-ICC, Indexed over one of those, or a 1-bit
//!   `/ImageMask` stencil, without `/SMask`/`/Mask`, in a filter chain this
//!   module decodes (Flate/LZW/RunLength/ASCII with predictors, `DCTDecode`
//!   through [`jpeg`] — at pdfium's reduced DCT scale when the image is at
//!   least twice the bitmap — `CCITTFaxDecode` through [`fax`] and
//!   `JBIG2Decode` through the renderer's decoder, #811); JPX,
//!   CMYK/Lab/Separation and non-sRGB ICC images stay with pdfium;
//! * the image matrix is axis-aligned or a 90° rotation — pdfium's
//!   `CFX_AggImageRenderer` stretch paths, both ported in [`stretch`]; a
//!   general affine placement (its `CFX_ImageTransformer`) declines.
//!
//! The device geometry is pdfium's to the float: the display matrix of
//! `CPDF_Page::GetDisplayMatrix`, the CTM chain of the content parser
//! (`cm` prepends, forms concatenate), `GetUnitRect().GetOuterRect()` for
//! the image's device rectangle, the rectangular-clip shortcut of
//! `CFX_AggDeviceDriver::SetClip_PathFill`, the flips and swapped clip of a
//! 90° placement, and the image's own decode/palette rules of `CPDF_DIB`
//! (`/Decode` and Indexed lookups become the 2ⁿ-entry palette pdfium
//! builds — applied before the stretch for 2–8-bit samples, after it through
//! the 256-step ramp for 1-bit ones — a stencil mask is stretched to 8-bit
//! coverage and merged with the fill colour as `CompositeRow_ByteMask2Rgb`
//! does). The oracle is pdfium itself: the tests compare against
//! `render_with_config` on the scanned fixtures and on synthesized pages
//! covering every image kind, when the library is installed.

pub mod fax;
pub mod filters;
pub use docling_core::jpeg;
#[cfg(test)]
mod jpeg_tests;
pub mod stretch;

use std::collections::HashMap;

use image::RgbImage;
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::pdf_meta::PdfMeta;
use stretch::{Options, Rect, Source};

/// The renderer knob for image-only pages: `DOCLING_RS_SCAN_RASTER`.
/// `rust` (default) takes this module's bitmap whenever the page qualifies;
/// `pdfium` never does — an A/B switch for conformance runs.
pub fn enabled() -> bool {
    !matches!(
        docling_core::env::nonempty("DOCLING_RS_SCAN_RASTER").as_deref(),
        Some("pdfium") | Some("off") | Some("0")
    )
}

/// Render page `index` into a `width` × `height` bitmap the way pdfium's
/// `FPDF_RenderPageBitmap` (white-cleared, `FPDF_ANNOT`) would, if the page
/// is image-only in the sense above. `None` — with the reason under
/// `DOCLING_RS_DEBUG` — when it is not, or something in it is unsupported.
pub fn render(meta: &PdfMeta, index: usize, width: u32, height: u32) -> Option<RgbImage> {
    match render_inner(meta, index, width, height) {
        Ok(img) => Some(img),
        Err(why) => {
            docling_core::debug_log!(
                "docling-pdf: page {}: not rendered by the Rust raster ({why})",
                index + 1
            );
            None
        }
    }
}

/// pdfium's `CFX_Matrix`: `f32`, applied as `(a·x + c·y + e, b·x + d·y + f)`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct M {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl M {
    const ID: M = M {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// `CFX_Matrix::operator*(right)`: `self` first, then `right`.
    fn then(self, r: M) -> M {
        M {
            a: self.a * r.a + self.b * r.c,
            b: self.a * r.b + self.b * r.d,
            c: self.c * r.a + self.d * r.c,
            d: self.c * r.b + self.d * r.d,
            e: self.e * r.a + self.f * r.c + r.e,
            f: self.e * r.b + self.f * r.d + r.f,
        }
    }

    fn apply(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    fn from_array(v: &[f32]) -> Option<M> {
        if v.len() != 6 || v.iter().any(|x| !x.is_finite()) {
            return None;
        }
        Some(M {
            a: v[0],
            b: v[1],
            c: v[2],
            d: v[3],
            e: v[4],
            f: v[5],
        })
    }
}

/// `CFX_FloatRect` after `TransformRect`/`Normalize`: y-up naming, but the
/// values are device coordinates here.
#[derive(Clone, Copy, Debug)]
struct FRect {
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
}

impl FRect {
    fn from_points(pts: &[(f32, f32)]) -> FRect {
        let mut r = FRect {
            left: pts[0].0,
            right: pts[0].0,
            bottom: pts[0].1,
            top: pts[0].1,
        };
        for &(x, y) in &pts[1..] {
            r.left = r.left.min(x);
            r.right = r.right.max(x);
            r.bottom = r.bottom.min(y);
            r.top = r.top.max(y);
        }
        r
    }

    fn normalize(&mut self) {
        if self.left > self.right {
            std::mem::swap(&mut self.left, &mut self.right);
        }
        if self.bottom > self.top {
            std::mem::swap(&mut self.bottom, &mut self.top);
        }
    }

    /// `CFX_FloatRect::Intersect`: an empty intersection is the zero rect.
    fn intersect(&mut self, o: FRect) {
        self.normalize();
        let mut o = o;
        o.normalize();
        self.left = self.left.max(o.left);
        self.bottom = self.bottom.max(o.bottom);
        self.right = self.right.min(o.right);
        self.top = self.top.min(o.top);
        if self.left > self.right || self.bottom > self.top {
            *self = FRect {
                left: 0.0,
                bottom: 0.0,
                right: 0.0,
                top: 0.0,
            };
        }
    }

    /// `GetOuterRect`: floor the low edges, ceil the high ones (y down).
    fn outer(&self) -> Rect {
        let mut r = Rect::new(
            sat(self.left.floor()),
            sat(self.bottom.floor()),
            sat(self.right.ceil()),
            sat(self.top.ceil()),
        );
        r.normalize();
        r
    }
}

fn sat(v: f32) -> i32 {
    if v.is_nan() {
        0
    } else {
        v.clamp(i32::MIN as f32, i32::MAX as f32) as i32
    }
}

/// `FXSYS_roundf(clamp(v, 0, 1) · 255)`: a colour component to a byte the
/// way `CPDF_Color::GetColorRef` and `CPDF_DIB::LoadPalette` do it.
fn to_byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// One image placement in device space.
struct Draw {
    stream: ObjectId,
    /// Image unit square → device.
    matrix: M,
    /// Device clip at the time of `Do` (integer, from the rectangular clips).
    clip: Rect,
    /// The fill colour at `Do` (a stencil mask paints with it); `None` when
    /// it was set through a colour space this module does not convert.
    fill: Option<[u8; 3]>,
}

/// The fill colour space `sc`/`scn` operands are read in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FillCs {
    Gray,
    Rgb,
    Other,
}

/// The graphics state the walk tracks (`CPDF_AllStates` subset).
#[derive(Clone, Copy)]
struct GState {
    ctm: M,
    clip: Rect,
    /// `Tr` — only invisible text (3) is tolerated.
    text_render: i64,
    /// pdfium's `fill_color_ref_` (black by default); `None` once a colour
    /// this module cannot reproduce (CMYK, patterns, …) was set.
    fill: Option<[u8; 3]>,
    fill_cs: FillCs,
}

/// Rendering context of one content stream: the `mtObj2Device` its objects
/// are rendered with (the display matrix for the page, `F * parent` for a
/// form), the resources and the page resources pdfium falls back to.
struct Ctx<'a> {
    doc: &'a Document,
    obj2dev: M,
    res: Option<&'a Dictionary>,
    page_res: Option<&'a Dictionary>,
    device: Rect,
    depth: usize,
}

struct Walker {
    draws: Vec<Draw>,
    ops: usize,
}

const MAX_OPS: usize = 500_000;
const MAX_DEPTH: usize = 12;

fn deref<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    match obj {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(obj),
        o => o,
    }
}

fn as_dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Dictionary> {
    match deref(doc, obj) {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

fn num(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

fn nums(ops: &[Object]) -> Option<Vec<f32>> {
    ops.iter().map(num).collect()
}

fn name_is(o: Option<&Object>, n: &[u8]) -> bool {
    matches!(o, Some(Object::Name(v)) if v == n)
}

fn bool_or_int_true(doc: &Document, o: Option<&Object>) -> bool {
    match o.map(|o| deref(doc, o)) {
        Some(Object::Boolean(b)) => *b,
        Some(Object::Integer(i)) => *i != 0,
        Some(Object::Real(r)) => *r != 0.0,
        _ => false,
    }
}

/// A resource of `kind`/`name`: the stream's own resources, then the page's
/// (`CPDF_StreamContentParser::FindResourceHolder`).
fn find_resource<'a>(ctx: &Ctx<'a>, kind: &[u8], name: &[u8]) -> Option<&'a Object> {
    [ctx.res, ctx.page_res]
        .into_iter()
        .flatten()
        .find_map(|res| {
            res.get(kind)
                .ok()
                .and_then(|o| as_dict(ctx.doc, o))
                .and_then(|d| d.get(name).ok())
        })
}

/// Is this ExtGState invisible to an opaque image? Anything that could
/// change pixels (alpha, blend, soft mask, transfer, overprint) declines.
fn extgstate_is_neutral(doc: &Document, gs: &Dictionary) -> Result<(), String> {
    for (k, v) in gs.iter() {
        let v = deref(doc, v);
        let ok = match k.as_slice() {
            b"Type" | b"LW" | b"LC" | b"LJ" | b"ML" | b"D" | b"RI" | b"Font" | b"FL" | b"SM"
            | b"SA" | b"OPM" | b"TK" => true,
            b"CA" | b"ca" => num(v) == Some(1.0),
            b"OP" | b"op" => matches!(v, Object::Boolean(false)),
            b"BM" => match v {
                Object::Name(n) => n == b"Normal" || n == b"Compatible",
                Object::Array(a) => a.first().is_some_and(|o| {
                    name_is(Some(o), b"Normal") || name_is(Some(o), b"Compatible")
                }),
                _ => false,
            },
            b"SMask" => name_is(Some(v), b"None"),
            b"AIS" => matches!(v, Object::Boolean(false)),
            _ => false,
        };
        if !ok {
            return Err(format!("ExtGState /{}", String::from_utf8_lossy(k)));
        }
    }
    Ok(())
}

/// The fill colour space a `cs` operand selects, as far as `sc` values can
/// be converted here (`CPDF_StreamContentParser::FindColorSpace` +
/// `CPDF_Color::GetRGB`): the device gray/RGB spaces (unless the resources
/// override them with `/DefaultGray`/`/DefaultRGB`), CalGray (pdfium ignores
/// its gamma), a CalRGB without `/Gamma`/`/Matrix`, the sRGB ICC profile.
fn fill_cs_of(ctx: &Ctx<'_>, name: &[u8]) -> FillCs {
    match name {
        b"DeviceGray" | b"DeviceRGB" => {
            let default: &[u8] = if name == b"DeviceGray" {
                b"DefaultGray"
            } else {
                b"DefaultRGB"
            };
            if find_resource(ctx, b"ColorSpace", default).is_some() {
                return FillCs::Other;
            }
            if name == b"DeviceGray" {
                FillCs::Gray
            } else {
                FillCs::Rgb
            }
        }
        b"DeviceCMYK" | b"Pattern" => FillCs::Other,
        other => {
            let Some(obj) = find_resource(ctx, b"ColorSpace", other) else {
                return FillCs::Other;
            };
            match color_space(ctx.doc, obj, None, None) {
                Ok(Cs::Gray) => FillCs::Gray,
                Ok(Cs::Rgb {
                    cal_rgb_with_transform: false,
                }) => FillCs::Rgb,
                _ => FillCs::Other,
            }
        }
    }
}

impl Walker {
    /// `re x y w h` under `ctm`, then `obj2dev`: pdfium's rectangular clip —
    /// the five path points transformed twice in `f32`, still a rectangle,
    /// intersected with the device and rounded outwards.
    fn rect_clip(&self, ctx: &Ctx<'_>, ctm: M, re: &[f32]) -> Result<Rect, String> {
        let (x, y, w, h) = (re[0], re[1], re[2], re[3]);
        let user = [(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)];
        let mut dev = [(0f32, 0f32); 5];
        for (i, &(px, py)) in user.iter().enumerate() {
            // `CPDF_StreamContentParser::AddPathObject`: the path is stored in
            // content space (CTM applied), then `CFX_Path::GetRect(&mtObj2Device)`.
            let (ux, uy) = ctm.apply(px, py);
            dev[i] = ctx.obj2dev.apply(ux, uy);
            if i > 0 && dev[i].0 != dev[i - 1].0 && dev[i].1 != dev[i - 1].1 {
                return Err("non-rectangular clip".into());
            }
        }
        if dev[0].0 != dev[3].0 && dev[0].1 != dev[3].1 {
            return Err("non-rectangular clip".into());
        }
        let mut r = FRect {
            left: dev[0].0,
            bottom: dev[0].1,
            right: dev[2].0,
            top: dev[2].1,
        };
        r.normalize();
        r.intersect(FRect {
            left: 0.0,
            bottom: 0.0,
            right: ctx.device.right as f32,
            top: ctx.device.bottom as f32,
        });
        Ok(r.outer())
    }

    fn walk<'a>(&mut self, ctx: &Ctx<'a>, content: &[u8], init: GState) -> Result<(), String> {
        if ctx.depth > MAX_DEPTH {
            return Err("form nesting".into());
        }
        let ops = lopdf::content::Content::decode(content).map_err(|e| format!("content: {e}"))?;
        let mut stack: Vec<GState> = Vec::new();
        let mut st = init;
        // The current path, as far as a clip can use it: exactly one `re`.
        let mut path_rect: Option<Vec<f32>> = None;
        let mut path_other = false;
        let mut clip_pending = false;
        let mut compat = 0usize;
        for op in &ops.operations {
            self.ops += 1;
            if self.ops > MAX_OPS {
                return Err("content too long".into());
            }
            let o = op.operator.as_str();
            let args = &op.operands;
            match o {
                "q" => {
                    stack.push(st);
                    if stack.len() > 256 {
                        return Err("q nesting".into());
                    }
                }
                "Q" => {
                    if let Some(s) = stack.pop() {
                        st = s;
                    }
                }
                "cm" => {
                    let v = nums(args).ok_or("cm operands")?;
                    let m = M::from_array(&v).ok_or("cm matrix")?;
                    // `prepend_to_current_transformation_matrix`: new × CTM.
                    st.ctm = m.then(st.ctm);
                }
                "gs" => {
                    let name = match args.first() {
                        Some(Object::Name(n)) => n,
                        _ => return Err("gs operand".into()),
                    };
                    let gs = find_resource(ctx, b"ExtGState", name)
                        .and_then(|o| as_dict(ctx.doc, o))
                        .ok_or("gs resource")?;
                    extgstate_is_neutral(ctx.doc, gs)?;
                }
                // Path construction: only a lone rectangle can become a clip.
                "re" => {
                    let v = nums(args).ok_or("re operands")?;
                    if v.len() != 4 {
                        return Err("re operands".into());
                    }
                    if path_rect.is_some() || path_other {
                        path_other = true;
                    } else {
                        path_rect = Some(v);
                    }
                }
                "m" | "l" | "c" | "v" | "y" | "h" => path_other = true,
                "W" | "W*" => clip_pending = true,
                "n" => {
                    if clip_pending {
                        match (&path_rect, path_other) {
                            (Some(re), false) => {
                                let r = self.rect_clip(ctx, st.ctm, re)?;
                                st.clip.intersect(&r);
                            }
                            _ => return Err("non-rectangular clip".into()),
                        }
                    }
                    path_rect = None;
                    path_other = false;
                    clip_pending = false;
                }
                "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "S" | "s" => {
                    return Err(format!("painted path ({o})"));
                }
                "sh" => return Err("shading".into()),
                "BI" | "ID" | "EI" => return Err("inline image".into()),
                "d0" | "d1" => return Err("type3 glyph".into()),
                // Text: state is fine, showing glyphs is not unless invisible.
                "BT" | "ET" | "Tc" | "Tw" | "Tz" | "TL" | "Tf" | "Ts" | "Td" | "TD" | "Tm"
                | "T*" => {}
                "Tr" => {
                    st.text_render = args.first().and_then(|o| o.as_i64().ok()).unwrap_or(0);
                }
                "Tj" | "TJ" | "'" | "\"" => {
                    if st.text_render != 3 {
                        return Err("visible text".into());
                    }
                }
                // Fill colour (a stencil mask paints with it); pdfium rounds
                // each component to a byte when the colour is set.
                "g" => {
                    if let Some(v) = args.last().and_then(num) {
                        let b = to_byte(v);
                        st.fill = Some([b, b, b]);
                    }
                    st.fill_cs = FillCs::Gray;
                }
                "rg" => {
                    if let Some(v) = nums(args).filter(|v| v.len() >= 3) {
                        let n = v.len();
                        st.fill = Some([to_byte(v[n - 3]), to_byte(v[n - 2]), to_byte(v[n - 1])]);
                    }
                    st.fill_cs = FillCs::Rgb;
                }
                "k" => {
                    // Adobe CMYK → sRGB is a 9⁴-sample table in pdfium.
                    st.fill = None;
                    st.fill_cs = FillCs::Other;
                }
                "cs" => {
                    // `cs` selects the space and resets the colour value but
                    // leaves the cached colour ref alone until `sc`.
                    st.fill_cs = match args.first() {
                        Some(Object::Name(n)) => fill_cs_of(ctx, n),
                        _ => FillCs::Other,
                    };
                }
                "sc" | "scn" => {
                    if args.iter().any(|a| matches!(a, Object::Name(_))) {
                        // A pattern.
                        st.fill = None;
                    } else {
                        let v: Vec<f32> = args.iter().filter_map(num).collect();
                        let v = if v.len() > 4 {
                            v[v.len() - 4..].to_vec()
                        } else {
                            v
                        };
                        match st.fill_cs {
                            FillCs::Gray if !v.is_empty() => {
                                let b = to_byte(v[0]);
                                st.fill = Some([b, b, b]);
                            }
                            FillCs::Rgb if v.len() >= 3 => {
                                st.fill = Some([to_byte(v[0]), to_byte(v[1]), to_byte(v[2])]);
                            }
                            // Too few operands: pdfium leaves the colour alone.
                            FillCs::Gray | FillCs::Rgb => {}
                            FillCs::Other => st.fill = None,
                        }
                    }
                }
                // Stroke colour, line state, marked content, compatibility: no pixels.
                "G" | "RG" | "K" | "CS" | "SC" | "SCN" | "w" | "J" | "j" | "M" | "d" | "ri"
                | "i" | "BMC" | "BDC" | "EMC" | "MP" | "DP" => {}
                "BX" => compat += 1,
                "EX" => compat = compat.saturating_sub(1),
                "Do" => {
                    let name = match args.first() {
                        Some(Object::Name(n)) => n,
                        _ => return Err("Do operand".into()),
                    };
                    let xobj = find_resource(ctx, b"XObject", name).ok_or("Do resource")?;
                    let (id, stream) = match xobj {
                        Object::Reference(id) => match ctx.doc.get_object(*id) {
                            Ok(Object::Stream(s)) => (*id, s),
                            _ => return Err("XObject reference".into()),
                        },
                        // A direct XObject stream has no id to cache under;
                        // pdfium clones inline ones — rare enough to decline.
                        _ => return Err("direct XObject".into()),
                    };
                    let subtype = stream.dict.get(b"Subtype").ok().map(|o| deref(ctx.doc, o));
                    if name_is(subtype, b"Image") {
                        if st.clip.is_empty() {
                            continue;
                        }
                        self.draws.push(Draw {
                            stream: id,
                            matrix: st.ctm.then(ctx.obj2dev),
                            clip: st.clip,
                            fill: st.fill,
                        });
                    } else if name_is(subtype, b"Form") {
                        self.form(ctx, stream, st)?;
                    } else {
                        return Err("XObject subtype".into());
                    }
                }
                _ if compat > 0 => {}
                other => return Err(format!("operator {other}")),
            }
        }
        Ok(())
    }

    /// `Do` on a Form XObject: pdfium parses it with `/Matrix` as the initial
    /// CTM, its `/BBox` as a clip in the form's own space, and renders its
    /// objects with `form_matrix * mtObj2Device` where `form_matrix` is the
    /// CTM at `Do`.
    fn form<'a>(
        &mut self,
        ctx: &Ctx<'a>,
        stream: &'a lopdf::Stream,
        st: GState,
    ) -> Result<(), String> {
        let d = &stream.dict;
        if let Some(group) = d.get(b"Group").ok().and_then(|o| as_dict(ctx.doc, o)) {
            // An isolated or knockout transparency group renders through a
            // separate bitmap (`ProcessTransparency`); not reproduced here.
            if bool_or_int_true(ctx.doc, group.get(b"I").ok())
                || bool_or_int_true(ctx.doc, group.get(b"K").ok())
            {
                return Err("isolated/knockout transparency group".into());
            }
        }
        if d.get(b"OC").is_ok() {
            return Err("optional content".into());
        }
        let form_matrix = match d.get(b"Matrix").ok().map(|o| deref(ctx.doc, o)) {
            Some(Object::Array(a)) => {
                let v: Vec<f32> = a.iter().filter_map(|o| num(deref(ctx.doc, o))).collect();
                M::from_array(&v).ok_or("form /Matrix")?
            }
            _ => M::ID,
        };
        let inner = Ctx {
            doc: ctx.doc,
            obj2dev: st.ctm.then(ctx.obj2dev),
            res: d
                .get(b"Resources")
                .ok()
                .and_then(|o| as_dict(ctx.doc, o))
                .or(ctx.res),
            page_res: ctx.page_res,
            device: ctx.device,
            depth: ctx.depth + 1,
        };
        let mut inner_state = GState {
            ctm: form_matrix,
            ..st
        };
        if let Some(Object::Array(b)) = d.get(b"BBox").ok().map(|o| deref(ctx.doc, o)) {
            let v: Vec<f32> = b.iter().filter_map(|o| num(deref(ctx.doc, o))).collect();
            if v.len() != 4 {
                return Err("form /BBox".into());
            }
            // `CPDF_Path::AppendFloatRect` of the normalized bbox, transformed
            // by the form matrix — i.e. the rectangle `re` would build.
            let (l, r) = (v[0].min(v[2]), v[0].max(v[2]));
            let (bt, tp) = (v[1].min(v[3]), v[1].max(v[3]));
            let clip = self.rect_clip(&inner, form_matrix, &[l, bt, r - l, tp - bt])?;
            inner_state.clip.intersect(&clip);
        }
        if inner_state.clip.is_empty() {
            return Ok(());
        }
        let content = stream
            .decompressed_content()
            .map_err(|e| format!("form content: {e}"))?;
        self.walk(&inner, &content, inner_state)
    }
}

/// A decoded image in the form pdfium's `CPDF_DIB` would present it to the
/// stretch engine.
struct Decoded {
    width: i32,
    height: i32,
    kind: Kind,
    interpolate: bool,
}

enum Kind {
    /// 1 bpp (`k1bppRgb`), with the two-entry palette pdfium builds for a
    /// non-default `/Decode` or an Indexed space (`None` = the stock
    /// black/white, no palette) — applied through the 256-step ramp after
    /// the stretch.
    Bilevel {
        data: Vec<u8>,
        stride: usize,
        palette: Option<[[u8; 3]; 2]>,
    },
    /// An `/ImageMask` stencil (`k1bppMask`): bit 1 = paint with the fill
    /// colour, stretched to 8-bit coverage.
    Mask { data: Vec<u8>, stride: usize },
    /// 8-bit gray, no palette (`k8bppRgb`).
    Gray8(Vec<u8>),
    /// 8-bit RGB (`kBgr`), or a paletted image already looked up.
    Rgb8(Vec<u8>),
}

/// The image colour spaces this module reproduces.
#[derive(Clone)]
enum Cs {
    Gray,
    Rgb {
        /// A CalRGB carrying `/Gamma` or `/Matrix`: pdfium copies image
        /// samples through untouched but converts a *fill colour* through
        /// XYZ, so it is fine for images and declined for `sc`.
        cal_rgb_with_transform: bool,
    },
    Indexed {
        base: Base,
        hival: i64,
        table: Vec<u8>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Base {
    Gray,
    Rgb,
}

/// Resolve a `/ColorSpace` object (`CPDF_DocPageData::GetColorSpace`).
fn color_space(
    doc: &Document,
    cs: &Object,
    res: Option<&Dictionary>,
    page_res: Option<&Dictionary>,
) -> Result<Cs, String> {
    match deref(doc, cs) {
        Object::Name(n) => match n.as_slice() {
            b"DeviceGray" | b"G" | b"CalGray" => Ok(Cs::Gray),
            b"DeviceRGB" | b"RGB" | b"CalRGB" => Ok(Cs::Rgb {
                cal_rgb_with_transform: false,
            }),
            b"DeviceCMYK" | b"CMYK" | b"Pattern" | b"I" | b"Indexed" => {
                Err(format!("colour space /{}", String::from_utf8_lossy(n)))
            }
            other => {
                // A named resource.
                for r in [res, page_res].into_iter().flatten() {
                    if let Some(o) = r
                        .get(b"ColorSpace")
                        .ok()
                        .and_then(|o| as_dict(doc, o))
                        .and_then(|d| d.get(other).ok())
                    {
                        return color_space(doc, o, None, None);
                    }
                }
                Err(format!("colour space /{}", String::from_utf8_lossy(other)))
            }
        },
        Object::Array(a) => {
            let fam = a.first().map(|o| deref(doc, o));
            match fam {
                Some(Object::Name(n)) if n == b"CalGray" => Ok(Cs::Gray),
                Some(Object::Name(n)) if n == b"CalRGB" => {
                    let d = a.get(1).and_then(|o| as_dict(doc, o));
                    Ok(Cs::Rgb {
                        cal_rgb_with_transform: d
                            .is_some_and(|d| d.get(b"Gamma").is_ok() || d.get(b"Matrix").is_ok()),
                    })
                }
                Some(Object::Name(n)) if n == b"ICCBased" => {
                    let stream = a
                        .get(1)
                        .map(|o| deref(doc, o))
                        .and_then(|o| o.as_stream().ok())
                        .ok_or("ICCBased stream")?;
                    let n = stream
                        .dict
                        .get(b"N")
                        .ok()
                        .and_then(|o| deref(doc, o).as_i64().ok())
                        .unwrap_or(0);
                    // pdfium treats exactly one profile as a plain swap — the
                    // 3144-byte sRGB IEC61966-2.1 — and runs Little-CMS on
                    // every other one.
                    let bytes = stream.decompressed_content().unwrap_or_default();
                    if n == 3
                        && bytes.len() == 3144
                        && bytes.get(400..417) == Some(b"sRGB IEC61966-2.1")
                    {
                        Ok(Cs::Rgb {
                            cal_rgb_with_transform: false,
                        })
                    } else {
                        Err("ICC profile (Little-CMS transform)".into())
                    }
                }
                Some(Object::Name(n)) if n == b"Indexed" || n == b"I" => {
                    if a.len() < 4 {
                        return Err("Indexed array".into());
                    }
                    let base = match color_space(doc, &a[1], res, page_res)? {
                        Cs::Gray => Base::Gray,
                        Cs::Rgb { .. } => Base::Rgb,
                        Cs::Indexed { .. } => return Err("Indexed over Indexed".into()),
                    };
                    // pdfium clamps hival to 0..=255 so out-of-range files load.
                    let hival = deref(doc, &a[2]).as_i64().unwrap_or(0).clamp(0, 255);
                    let table = match deref(doc, &a[3]) {
                        Object::String(s, _) => s.clone(),
                        Object::Stream(s) => s
                            .decompressed_content()
                            .map_err(|e| format!("Indexed lookup: {e}"))?,
                        _ => return Err("Indexed lookup".into()),
                    };
                    Ok(Cs::Indexed { base, hival, table })
                }
                Some(Object::Name(n)) => {
                    Err(format!("colour space /{}", String::from_utf8_lossy(n)))
                }
                _ => Err("colour space".into()),
            }
        }
        _ => Err("colour space".into()),
    }
}

impl Cs {
    /// `CPDF_ColorSpace::GetRGBOrZerosOnError` for one component value
    /// (`GetRGB`, then `FXSYS_roundf(· 255)` as `LoadPalette` does).
    fn rgb_of(&self, value: f32) -> [u8; 3] {
        match self {
            Cs::Gray | Cs::Rgb { .. } => {
                let g = to_byte(value);
                [g, g, g]
            }
            Cs::Indexed { base, hival, table } => {
                // `CPDF_IndexedCS::GetRGB`: truncate, range-check against
                // hival and the table, base component = byte / 255.
                let index = value as i32;
                let n = if *base == Base::Gray { 1 } else { 3 };
                if index < 0 || i64::from(index) > *hival {
                    return [0, 0, 0];
                }
                let start = index as usize * n;
                if start + n > table.len() {
                    return [0, 0, 0];
                }
                let comp = |i: usize| f32::from(table[start + i]) / 255.0;
                match base {
                    Base::Gray => {
                        let g = to_byte(comp(0));
                        [g, g, g]
                    }
                    Base::Rgb => [to_byte(comp(0)), to_byte(comp(1)), to_byte(comp(2))],
                }
            }
        }
    }
}

/// `CPDF_DIB::LoadPalette` for a one-component image of `bits` bits per
/// pixel: the palette pdfium attaches (`None` = it keeps none and the
/// samples are gray values as they are), from the `/Decode` range and the
/// colour space.
fn palette(cs: &Cs, bits: u32, decode: Option<&[f32]>) -> Result<Option<Vec<[u8; 3]>>, String> {
    let max_data = ((1u32 << bits) - 1) as f32;
    // `GetDecodeAndMaskArray`: default range 0..1, or 0..max_data for
    // Indexed; a `/Decode` sets min and step and flags a non-default one.
    let (def_min, def_max) = match cs {
        Cs::Indexed { .. } => (0.0f32, max_data),
        _ => (0.0, 1.0),
    };
    let (dmin, step, default_decode) = match decode {
        Some(d) if d.len() >= 2 => {
            let (min, max) = (d[0], d[1]);
            (
                min,
                (max - min) / max_data,
                def_min == min && def_max == max,
            )
        }
        Some(_) => return Err("Decode array".into()),
        None => (def_min, (def_max - def_min) / max_data, true),
    };
    let stock_gray = matches!(cs, Cs::Gray);
    if bits == 1 {
        if default_decode && stock_gray {
            return Ok(None);
        }
        let c0 = cs.rgb_of(dmin);
        let c1 = match cs {
            Cs::Indexed { hival: 0, .. } => [0, 0, 0],
            _ => cs.rgb_of(dmin + step),
        };
        if c0 == [0, 0, 0] && c1 == [255, 255, 255] {
            return Ok(None);
        }
        return Ok(Some(vec![c0, c1]));
    }
    if bits == 8 && default_decode && stock_gray {
        return Ok(None);
    }
    Ok(Some(
        (0..(1u32 << bits))
            .map(|i| cs.rgb_of(dmin + step * i as f32))
            .collect(),
    ))
}

/// `GetBits8`: the `bpc`-bit sample at `index` of a byte-aligned row.
fn sample(row: &[u8], index: usize, bpc: u32) -> u8 {
    match bpc {
        8 => row[index],
        _ => {
            let bitpos = index * bpc as usize;
            (row[bitpos / 8] >> (8 - bpc as usize - bitpos % 8)) & ((1u8 << bpc) - 1)
        }
    }
}

/// The sample rows a filter chain yields.
enum Rows {
    /// Byte-aligned packed rows of `bpc`-bit samples.
    Packed { data: Vec<u8>, bpc: u32 },
    /// CCITT: pdfium's 32-bit-pitch 1-bit rows, `None` = a zero row.
    Fax(Vec<Option<Vec<u8>>>),
    /// A decoded JPEG (possibly at a reduced scale).
    Jpeg(jpeg::Image),
}

/// Decode one image XObject the way `CPDF_DIB` loads it, or say why not.
/// `device` is the render bitmap size — pdfium decodes a JPEG at a reduced
/// DCT scale when the image is at least twice as large.
fn decode_image(
    ctx: &Ctx<'_>,
    stream: &lopdf::Stream,
    device: (u32, u32),
) -> Result<Decoded, String> {
    let doc = ctx.doc;
    let d = &stream.dict;
    let int = |k: &[u8]| d.get(k).ok().and_then(|o| deref(doc, o).as_i64().ok());
    let width = int(b"Width").or_else(|| int(b"W")).ok_or("Width")?;
    let height = int(b"Height").or_else(|| int(b"H")).ok_or("Height")?;
    if width <= 0 || height <= 0 || width > 1 << 16 || height > 1 << 16 {
        return Err("image size".into());
    }
    if d.get(b"SMask").is_ok() || d.get(b"Mask").is_ok() {
        return Err("soft/colour-key mask".into());
    }
    let is_mask = bool_or_int_true(doc, d.get(b"ImageMask").ok().or_else(|| d.get(b"IM").ok()));
    let interpolate =
        bool_or_int_true(doc, d.get(b"Interpolate").ok().or_else(|| d.get(b"I").ok()));
    let decode: Option<Vec<f32>> = match d
        .get(b"Decode")
        .ok()
        .or_else(|| d.get(b"D").ok())
        .map(|o| deref(doc, o))
    {
        Some(Object::Array(a)) => Some(a.iter().filter_map(|o| num(deref(doc, o))).collect()),
        _ => None,
    };
    let (w, h) = (width as usize, height as usize);

    let chain = filters::filters(doc, d);
    let (data, codec) =
        filters::apply(doc, &stream.content, &chain).map_err(|e| format!("filter {e:?}"))?;

    let rows = match codec {
        Some(codec) if codec.name == "DCTDecode" => {
            if is_mask {
                return Err("DCT image mask".into());
            }
            // pdfium's tip (2025+) asks libjpeg for a `1/2^n` DCT-scaled decode
            // when the image is at least twice the bitmap in both dimensions
            // (`CPDF_DIB::StartLoadDIBBase`, `log2(min(w/W, h/H))` capped at
            // 3); the pinned conformance build decodes at full size and
            // stretches — the oracle test says so, byte for byte — and this
            // raster follows the pinned build. `jpeg::decode` implements the
            // reduced sizes (`jidctred`), so flipping this constant is all a
            // move of the reference needs.
            const DCT_SCALING_LIKE_PDFIUM_TIP: bool = false;
            let (dw, dh) = (i64::from(device.0), i64::from(device.1));
            let mut levels = 0u32;
            if DCT_SCALING_LIKE_PDFIUM_TIP && dw > 0 && dh > 0 {
                let ratio = (width / dw).min(height / dh).max(1);
                levels = (63 - ratio.leading_zeros()).min(3);
            }
            let transform = codec
                .parms
                .as_ref()
                .and_then(|p| p.get(b"ColorTransform").ok())
                .and_then(|o| deref(doc, o).as_i64().ok())
                .unwrap_or(1)
                != 0;
            let img =
                jpeg::decode(&data, transform, 1 << levels).map_err(|e| format!("JPEG {e:?}"))?;
            if img.width != w.div_ceil(1 << levels) || img.height != h.div_ceil(1 << levels) {
                return Err("JPEG size differs from the dictionary".into());
            }
            Rows::Jpeg(img)
        }
        Some(codec) if codec.name == "CCITTFaxDecode" => {
            let p = codec.parms.as_ref();
            let pi = |k: &[u8], default: i64| {
                p.and_then(|p| p.get(k).ok())
                    .and_then(|o| deref(doc, o).as_i64().ok())
                    .unwrap_or(default)
            };
            let pb = |k: &[u8]| p.is_some_and(|p| bool_or_int_true(doc, p.get(k).ok()));
            let mut rows_param = pi(b"Rows", 0);
            if rows_param > i64::from(u16::MAX) {
                rows_param = 0;
            }
            let params = fax::Params {
                k: pi(b"K", 0) as i32,
                end_of_line: pb(b"EndOfLine"),
                byte_align: pb(b"EncodedByteAlign"),
                black_is_1: pb(b"BlackIs1"),
                columns: pi(b"Columns", 1728).max(0) as usize,
                rows: rows_param.max(0) as usize,
            };
            if params.columns == 0 || params.columns > 65535 {
                return Err("CCITT columns".into());
            }
            // `CreateDecoder`: the decoder's rows must be at least as wide
            // as the image's.
            if fax::pitch(params.columns) < w.div_ceil(8) {
                return Err("CCITT columns narrower than the image".into());
            }
            Rows::Fax(fax::decode(&data, &params, h))
        }
        // JBIG2 (#811): the filter's packed 1-bit output, 0 = black, as
        // pdfium's `JBig2` module hands `CPDF_DIB` the bitmap (inverted
        // from JBIG2's 1 = black); `/JBIG2Globals` decoded ahead of it.
        Some(codec) if codec.name == "JBIG2Decode" => {
            let globals = codec
                .parms
                .as_ref()
                .and_then(|p| p.get(b"JBIG2Globals").ok())
                .map(|o| deref(doc, o))
                .and_then(|o| o.as_stream().ok())
                .and_then(|g| {
                    let chain = filters::filters(doc, &g.dict);
                    match filters::apply(doc, &g.content, &chain) {
                        Ok((bytes, None)) => Some(bytes),
                        _ => None,
                    }
                });
            let img = crate::render::jbig2::decode(&data, globals.as_deref(), w, h)
                .map_err(|e| format!("JBIG2 {e}"))?;
            Rows::Packed {
                data: img.data,
                bpc: 1,
            }
        }
        Some(codec) => return Err(format!("codec {}", codec.name)),
        None => {
            let bpc = if is_mask {
                1
            } else {
                int(b"BitsPerComponent")
                    .or_else(|| int(b"BPC"))
                    .unwrap_or(0)
            };
            let bpc = match bpc {
                1 | 2 | 4 | 8 => bpc as u32,
                other => return Err(format!("{other} bits per component")),
            };
            Rows::Packed { data, bpc }
        }
    };

    if is_mask {
        // `k1bppMask`: `default_decode_ = !Decode || Decode[0] == 0` (as an
        // integer); a default-decode row is inverted so that 1 = paint.
        let default_decode = decode
            .as_ref()
            .is_none_or(|d| d.first().is_none_or(|v| *v as i64 == 0));
        let stride = w.div_ceil(8);
        let mut out = vec![0u8; stride * h];
        match rows {
            Rows::Packed { data, .. } => {
                if data.len() < stride * h {
                    return Err("truncated image data".into());
                }
                for (y, row) in out.chunks_exact_mut(stride).enumerate() {
                    for (o, &s) in row.iter_mut().zip(&data[y * stride..y * stride + stride]) {
                        *o = if default_decode { !s } else { s };
                    }
                }
            }
            Rows::Fax(lines) => {
                for (y, row) in out.chunks_exact_mut(stride).enumerate() {
                    if let Some(Some(line)) = lines.get(y) {
                        for (o, &s) in row.iter_mut().zip(&line[..stride]) {
                            *o = if default_decode { !s } else { s };
                        }
                    }
                }
            }
            Rows::Jpeg(_) => return Err("DCT image mask".into()),
        }
        return Ok(Decoded {
            width: width as i32,
            height: height as i32,
            kind: Kind::Mask { data: out, stride },
            interpolate,
        });
    }

    let cs_obj = d
        .get(b"ColorSpace")
        .ok()
        .or_else(|| d.get(b"CS").ok())
        .ok_or("no ColorSpace")?;
    let cs = color_space(doc, cs_obj, ctx.res, ctx.page_res)?;
    let comps = match cs {
        Cs::Rgb { .. } => 3,
        _ => 1,
    };
    let default_rgb = decode
        .as_deref()
        .is_none_or(|dec| dec == [0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);

    let (out_w, out_h, kind) = match rows {
        Rows::Jpeg(img) => {
            let kind = match (img.channels, comps) {
                // 8-bit gray, with a palette when the space or Decode is not
                // the stock one.
                (1, 1) => match palette(&cs, 8, decode.as_deref())? {
                    None => Kind::Gray8(img.data),
                    Some(pal) => {
                        Kind::Rgb8(img.data.iter().flat_map(|&v| pal[usize::from(v)]).collect())
                    }
                },
                (3, 3) => rgb8(img.data, decode.as_deref(), default_rgb),
                _ => return Err("JPEG components vs colour space".into()),
            };
            (img.width as i32, img.height as i32, kind)
        }
        Rows::Fax(lines) => {
            // 1-bit gray: pdfium's rows (1 = white), zero rows where the data
            // ran out, through the 1-bit palette rules.
            if comps != 1 {
                return Err("CCITT with an RGB colour space".into());
            }
            let stride = w.div_ceil(8);
            let mut out = vec![0u8; stride * h];
            for (y, row) in out.chunks_exact_mut(stride).enumerate() {
                if let Some(Some(line)) = lines.get(y) {
                    row.copy_from_slice(&line[..stride]);
                }
            }
            let pal = palette(&cs, 1, decode.as_deref())?.map(|p| [p[0], p[1]]);
            (
                width as i32,
                height as i32,
                Kind::Bilevel {
                    data: out,
                    stride,
                    palette: pal,
                },
            )
        }
        Rows::Packed { data, bpc } => {
            let stride = (w * bpc as usize * comps).div_ceil(8);
            if data.len() < stride * h {
                return Err("truncated image data".into());
            }
            let kind = if comps == 3 {
                if bpc != 8 {
                    return Err(format!("{bpc}-bit RGB"));
                }
                rgb8(data, decode.as_deref(), default_rgb)
            } else if bpc == 1 {
                let pal = palette(&cs, 1, decode.as_deref())?.map(|p| [p[0], p[1]]);
                Kind::Bilevel {
                    data,
                    stride,
                    palette: pal,
                }
            } else {
                // 2/4/8-bit one-component: `TranslateScanline` unpacks the
                // indices, the palette (if any) turns them into colours.
                match palette(&cs, bpc, decode.as_deref())? {
                    None => Kind::Gray8(data),
                    Some(pal) => {
                        let mut out = Vec::with_capacity(w * h * 3);
                        for y in 0..h {
                            let row = &data[y * stride..y * stride + stride];
                            for x in 0..w {
                                out.extend_from_slice(&pal[usize::from(sample(row, x, bpc))]);
                            }
                        }
                        Kind::Rgb8(out)
                    }
                }
            };
            (width as i32, height as i32, kind)
        }
    };
    Ok(Decoded {
        width: out_w,
        height: out_h,
        kind,
        interpolate,
    })
}

/// 8-bit RGB samples: a default `/Decode` passes through; any other range
/// goes through `TranslateScanline24bpp` — `min + step · v` per component,
/// clamped, `· 255` truncated to a byte.
fn rgb8(data: Vec<u8>, decode: Option<&[f32]>, default_rgb: bool) -> Kind {
    if default_rgb {
        return Kind::Rgb8(data);
    }
    let dec = decode.unwrap_or(&[0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
    let tables: Vec<[u8; 256]> = (0..3)
        .map(|c| {
            let (min, max) = (
                dec.get(2 * c).copied().unwrap_or(0.0),
                dec.get(2 * c + 1).copied().unwrap_or(1.0),
            );
            let step = (max - min) / 255.0;
            let mut t = [0u8; 256];
            for (v, slot) in t.iter_mut().enumerate() {
                *slot = ((min + step * v as f32).clamp(0.0, 1.0) * 255.0) as u8;
            }
            t
        })
        .collect();
    let mut out = data;
    for px in out.chunks_exact_mut(3) {
        for (c, v) in px.iter_mut().enumerate() {
            *v = tables[c][usize::from(*v)];
        }
    }
    Kind::Rgb8(out)
}

/// How a stretched sample lands on the canvas.
enum Blend {
    Gray,
    Rgb,
    /// `BuildPaletteFrom1BppSource`: the 256-step ramp between the two
    /// palette colours, integer arithmetic.
    Ramp([[u8; 3]; 2]),
    /// `CompositeRow_ByteMask2Rgb` with the fill colour: coverage `a` blends
    /// `(dest·(255−a) + fill·a) / 255`, zero coverage leaves the pixel.
    Mask([u8; 3]),
}

fn put(canvas: &mut RgbImage, x: usize, y: usize, px: &[u8], blend: &Blend) {
    if x >= canvas.width() as usize || y >= canvas.height() as usize {
        return;
    }
    let p = canvas.get_pixel_mut(x as u32, y as u32);
    match blend {
        Blend::Gray => *p = image::Rgb([px[0], px[0], px[0]]),
        Blend::Rgb => *p = image::Rgb([px[0], px[1], px[2]]),
        Blend::Ramp([p0, p1]) => {
            let v = i32::from(px[0]);
            let ramp = |c: usize| {
                (i32::from(p0[c]) + (i32::from(p1[c]) - i32::from(p0[c])) * v / 255) as u8
            };
            *p = image::Rgb([ramp(0), ramp(1), ramp(2)]);
        }
        Blend::Mask(fill) => {
            let a = i32::from(px[0]);
            if a == 0 {
                return;
            }
            for (d, &f) in p.0.iter_mut().zip(fill) {
                *d = ((i32::from(*d) * (255 - a) + i32::from(f) * a) / 255) as u8;
            }
        }
    }
}

/// `CFX_AggImageRenderer` for one placement: the image rectangle, the clip,
/// the axis-aligned or 90° stretch, and the composite onto `canvas`.
fn draw(
    canvas: &mut RgbImage,
    img: &Decoded,
    m: M,
    clip: Rect,
    fill: Option<[u8; 3]>,
) -> Result<(), String> {
    let unit = [
        m.apply(0.0, 1.0),
        m.apply(0.0, 0.0),
        m.apply(1.0, 1.0),
        m.apply(1.0, 0.0),
    ];
    let image_rect = FRect::from_points(&unit).outer();
    let mut clip_box = clip;
    clip_box.intersect(&image_rect);
    if clip_box.is_empty() {
        return Ok(());
    }
    // `StartDIBBase`: a huge image (> 60 MB of samples) is always bilinear.
    let bytes = i64::from(img.width)
        * i64::from(img.height)
        * match img.kind {
            Kind::Bilevel { .. } | Kind::Mask { .. } => 0,
            Kind::Gray8(_) => 1,
            Kind::Rgb8(_) => 3,
        };
    let options = Options {
        bilinear: img.interpolate || bytes > 60_000_000,
        no_smoothing: false,
    };
    let (source, blend) = match &img.kind {
        Kind::Bilevel {
            data,
            stride,
            palette,
        } => (
            Source::Bilevel {
                data,
                stride: *stride,
            },
            match palette {
                Some(p) => Blend::Ramp(*p),
                None => Blend::Gray,
            },
        ),
        Kind::Mask { data, stride } => {
            // A fill colour this module could not convert declines the page.
            let fill = fill.ok_or("stencil mask fill colour")?;
            (
                Source::Bilevel {
                    data,
                    stride: *stride,
                },
                Blend::Mask(fill),
            )
        }
        Kind::Gray8(v) => (
            Source::Gray8 {
                data: v,
                stride: img.width as usize,
            },
            Blend::Gray,
        ),
        Kind::Rgb8(v) => (
            Source::Rgb8 {
                data: v,
                stride: 3 * img.width as usize,
            },
            Blend::Rgb,
        ),
    };
    let rotated = (m.b.abs() >= 0.5 || m.a == 0.0) || (m.c.abs() >= 0.5 || m.d == 0.0);
    if rotated {
        if !(m.a.abs() < m.b.abs() / 20.0
            && m.d.abs() < m.c.abs() / 20.0
            && m.a.abs() < 0.5
            && m.d.abs() < 0.5)
        {
            return Err("general affine image placement".into());
        }
        let dest_width = image_rect.width();
        let dest_height = image_rect.height();
        let mut bitmap_clip = clip_box;
        bitmap_clip.offset(-image_rect.left, -image_rect.top);
        let flip_x = m.c > 0.0;
        let flip_y = m.b < 0.0;
        let bitmap_clip = bitmap_clip.swapped_clip_box(dest_width, dest_height, flip_x, flip_y);
        let s = stretch::stretch(
            &source,
            img.width,
            img.height,
            dest_height,
            dest_width,
            bitmap_clip,
            options,
        )
        .ok_or("stretch")?;
        // `ComposeScanlineV`: stretched row `line` → device column.
        let (cw, ch) = (clip_box.width() as usize, clip_box.height() as usize);
        if s.height != cw || s.width != ch {
            return Err("swapped clip size".into());
        }
        for line in 0..cw {
            let dx = clip_box.left as usize + if flip_x { cw - line - 1 } else { line };
            for i in 0..ch {
                let dy = if flip_y {
                    clip_box.top as usize + ch - 1 - i
                } else {
                    clip_box.top as usize + i
                };
                put(
                    canvas,
                    dx,
                    dy,
                    &s.data[(line * s.width + i) * s.channels..][..s.channels],
                    &blend,
                );
            }
        }
        return Ok(());
    }
    let mut dest_width = image_rect.width();
    if m.a < 0.0 {
        dest_width = -dest_width;
    }
    let mut dest_height = image_rect.height();
    if m.d > 0.0 {
        dest_height = -dest_height;
    }
    if dest_width == 0 || dest_height == 0 {
        return Ok(());
    }
    let mut bitmap_clip = clip_box;
    bitmap_clip.offset(-image_rect.left, -image_rect.top);
    let s = stretch::stretch(
        &source,
        img.width,
        img.height,
        dest_width,
        dest_height,
        bitmap_clip,
        options,
    )
    .ok_or("stretch")?;
    for row in 0..s.height {
        for col in 0..s.width {
            put(
                canvas,
                clip_box.left as usize + col,
                clip_box.top as usize + row,
                &s.data[(row * s.width + col) * s.channels..][..s.channels],
                &blend,
            );
        }
    }
    Ok(())
}

fn render_inner(meta: &PdfMeta, index: usize, width: u32, height: u32) -> Result<RgbImage, String> {
    if !enabled() {
        return Err("DOCLING_RS_SCAN_RASTER=pdfium".into());
    }
    if width == 0 || height == 0 || width > 1 << 15 || height > 1 << 15 {
        return Err("bitmap size".into());
    }
    let doc = meta.doc();
    let pid = meta.page_id(index).ok_or("page index")?;
    let page = doc
        .get_object(pid)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .ok_or("page dict")?;
    // Annotations: `FPDF_ANNOT` draws appearance streams and pdfium generates
    // some (Square, Text, …) itself; links draw nothing. Widgets are drawn by
    // the form-fill layer pdfium-render also runs.
    if let Some(annots) = page
        .get(b"Annots")
        .ok()
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    {
        for a in annots {
            let Some(ad) = as_dict(doc, a) else { continue };
            let hidden = ad
                .get(b"F")
                .ok()
                .and_then(|o| deref(doc, o).as_i64().ok())
                .is_some_and(|f| f & 2 != 0);
            if hidden {
                continue;
            }
            if !name_is(ad.get(b"Subtype").ok().map(|o| deref(doc, o)), b"Link") {
                return Err("annotation".into());
            }
        }
    }

    // pdfium's page geometry: the display box and `/Rotate` fold into
    // `page_matrix_`, the display matrix maps the (rotated) page size onto
    // the bitmap with y flipped (`CPDF_Page::GetDisplayMatrixForFloatRect`).
    let pb = crate::textparse::page_box(doc, pid);
    let geom = meta.geometry(index).ok_or("geometry")?;
    let (l, b, r, t) = (pb.l, pb.b, pb.l + pb.w, pb.b + pb.h);
    let page_matrix = match geom.rotation {
        90 => M {
            a: 0.0,
            b: -1.0,
            c: 1.0,
            d: 0.0,
            e: -b,
            f: r,
        },
        180 => M {
            a: -1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            e: r,
            f: t,
        },
        270 => M {
            a: 0.0,
            b: 1.0,
            c: -1.0,
            d: 0.0,
            e: t,
            f: -l,
        },
        _ => M {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: -l,
            f: -b,
        },
    };
    let (pw, ph) = (geom.width, geom.height);
    if pw == 0.0 || ph == 0.0 {
        return Err("page size".into());
    }
    let (wf, hf) = (width as f32, height as f32);
    // rotation 0: x0 = 0, y0 = H (FloatRect.top), x1 = 0, y1 = 0, x2 = W, y2 = H.
    let display = M {
        a: wf / pw,
        b: 0.0 / pw,
        c: 0.0 / ph,
        d: (0.0 - hf) / ph,
        e: 0.0,
        f: hf,
    };
    let obj2dev = page_matrix.then(display);
    let device = Rect::new(0, 0, width as i32, height as i32);

    let page_res = crate::textparse::page_res(doc, pid);
    let ctx = Ctx {
        doc,
        obj2dev,
        res: page_res,
        page_res,
        device,
        depth: 0,
    };
    let content = doc.get_page_content(pid);
    let mut walker = Walker {
        draws: Vec::new(),
        ops: 0,
    };
    walker.walk(
        &ctx,
        &content,
        GState {
            ctm: M::ID,
            clip: device,
            text_render: 0,
            fill: Some([0, 0, 0]),
            fill_cs: FillCs::Gray,
        },
    )?;
    if walker.draws.is_empty() {
        return Err("no image on the page".into());
    }

    let mut canvas = RgbImage::from_pixel(width, height, image::Rgb([255, 255, 255]));
    let mut cache: HashMap<ObjectId, Decoded> = HashMap::new();
    for dr in &walker.draws {
        if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(dr.stream) {
            let stream = doc
                .get_object(dr.stream)
                .ok()
                .and_then(|o| o.as_stream().ok())
                .ok_or("image stream")?;
            let decoded = crate::timing::timed("raster.decode", || {
                decode_image(&ctx, stream, (width, height))
            })?;
            e.insert(decoded);
        }
        let img = &cache[&dr.stream];
        crate::timing::timed("raster.stretch", || {
            draw(&mut canvas, img, dr.matrix, dr.clip, dr.fill)
        })?;
    }
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    fn root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn fixture(rel: &str) -> Vec<u8> {
        std::fs::read(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn crate_fixture(rel: &str) -> Vec<u8> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    /// The scanned fixtures this module must take over from pdfium.
    const IMAGE_ONLY: &[&str] = &[
        "tests/data/scanned/sources/ocr_test.pdf",
        "tests/data/scanned/sources/ocr_test_rotated_90.pdf",
        "tests/data/scanned/sources/ocr_test_rotated_180.pdf",
        "tests/data/scanned/sources/ocr_test_rotated_270.pdf",
        "tests/data/scanned/sources/ocr_test_raster.pdf",
        "tests/data/scanned/sources/ocr_test_raster_rot_90.pdf",
        "tests/data/scanned/sources/ocr_test_raster_rot_180.pdf",
        "tests/data/scanned/sources/ocr_test_raster_rot_270.pdf",
        "tests/data/scanned/sources/nemotron_multipage.pdf",
        "tests/data/scanned/sources/scanned_chart_table.pdf",
        "tests/data/pdf/sources/docling-rs-demotion-repro.pdf",
    ];

    /// The two render sizes the pipeline asks for: the OCR/TableFormer bitmap
    /// (1.5 × the 2.0 render scale, rounded) and the layout image (1.5 ×,
    /// ceiled) — the same arithmetic as `pdfium_backend::extract_page`.
    fn sizes(w: f32, h: f32) -> [(u32, u32); 2] {
        [
            (
                (w * 3.0).round().max(1.0) as u32,
                (h * 3.0).round().max(1.0) as u32,
            ),
            (
                (w * 1.5).ceil().max(1.0) as u32,
                (h * 1.5).ceil().max(1.0) as u32,
            ),
        ]
    }

    #[test]
    fn born_digital_pages_are_declined() {
        let meta = PdfMeta::open(&fixture("tests/data/pdf/sources/2206.01062.pdf")).unwrap();
        assert!(render(&meta, 0, 612, 792).is_none());
        // The ICC-profiled scan (Little-CMS in pdfium).
        let meta = PdfMeta::open(&fixture(
            "tests/data/scanned/sources/sample_with_rotation_mismatch.pdf",
        ))
        .unwrap();
        assert!(render(&meta, 0, 842, 595).is_none());
    }

    #[test]
    fn image_only_pages_render() {
        for rel in IMAGE_ONLY {
            let meta = PdfMeta::open(&fixture(rel)).unwrap();
            let g = meta.geometry(0).unwrap();
            let (w, h) = sizes(g.width, g.height)[1];
            let img = render(&meta, 0, w, h).unwrap_or_else(|| panic!("{rel} declined"));
            assert_eq!((img.width(), img.height()), (w, h), "{rel}");
            // A scan is mostly paper: white dominates, but not everything is.
            let dark = img.pixels().filter(|p| p[0] < 128).count();
            assert!(
                dark > 0 && dark < (w * h / 2) as usize,
                "{rel}: {dark} dark px"
            );
        }
    }

    /// Compare this module with pdfium on page `index` of `pdf` at `sizes`,
    /// appending a line per mismatch to `failures`; `false` when pdfium is
    /// not installed. The oracle needs the `pdfium` feature.
    #[cfg(feature = "pdfium")]
    fn oracle(
        label: &str,
        pdf: &[u8],
        index: usize,
        sizes: &[(u32, u32)],
        failures: &mut Vec<String>,
    ) -> bool {
        use pdfium_render::prelude::*;
        let pdfium = match crate::pdfium_backend::bind_for_tests() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("pdfium not installed — skipping the raster oracle ({e:?})");
                return false;
            }
        };
        let meta = PdfMeta::open(pdf).unwrap_or_else(|| panic!("{label}: lopdf cannot open"));
        let doc = pdfium
            .load_pdf_from_byte_slice(pdf, None)
            .unwrap_or_else(|e| panic!("{label}: pdfium {e:?}"));
        let page = doc.pages().get(index as PdfPageIndex).unwrap();
        for &(w, h) in sizes {
            let want = page
                .render_with_config(
                    &PdfRenderConfig::new()
                        .set_target_width(w as i32)
                        .set_target_height(h as i32),
                )
                .unwrap()
                .as_image()
                .into_rgb8();
            let Some(got) = render(&meta, index, w, h) else {
                failures.push(format!("{label} @{w}x{h}: declined"));
                continue;
            };
            if got.as_raw() != want.as_raw() {
                let diff = got
                    .as_raw()
                    .iter()
                    .zip(want.as_raw())
                    .filter(|(a, b)| a != b)
                    .count();
                let max = got
                    .as_raw()
                    .iter()
                    .zip(want.as_raw())
                    .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
                    .max()
                    .unwrap_or(0);
                failures.push(format!(
                    "{label} @{w}x{h}: {diff} of {} bytes differ (max |Δ| {max})",
                    want.len()
                ));
                if std::env::var_os("DOCLING_RS_RASTER_DUMP").is_some() {
                    let stem = label.replace(['/', ' '], "_");
                    let _ = got.save(format!("/tmp/{stem}.{w}.rust.png"));
                    let _ = want.save(format!("/tmp/{stem}.{w}.pdfium.png"));
                }
            }
        }
        true
    }

    /// The oracle on the corpus: pdfium's own bitmap, byte for byte, on every
    /// image-only fixture page at both pipeline sizes. Skipped without
    /// `libpdfium`.
    #[test]
    #[cfg(feature = "pdfium")]
    fn matches_pdfium_on_the_scanned_fixtures() {
        let mut failures = Vec::new();
        let mut pages = 0;
        for rel in IMAGE_ONLY {
            let bytes = fixture(rel);
            let meta = PdfMeta::open(&bytes).unwrap();
            for i in 0..meta.page_count() {
                let g = meta.geometry(i).unwrap();
                if !oracle(rel, &bytes, i, &sizes(g.width, g.height), &mut failures) {
                    return;
                }
                pages += 1;
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert!(pages >= IMAGE_ONLY.len(), "{pages} pages compared");
    }

    /// Build a one-page PDF: `page` points wide/high (with `/Rotate`), the
    /// content stream, and image XObjects by name.
    fn synth_pdf(
        page: (f32, f32),
        rotate: i64,
        content: &str,
        images: Vec<(&str, Stream)>,
    ) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut xobjs = Dictionary::new();
        for (name, stream) in images {
            let id = doc.add_object(Object::Stream(stream));
            xobjs.set(name, Object::Reference(id));
        }
        let res_id = doc.add_object(dictionary! { "XObject" => Object::Dictionary(xobjs) });
        let content_id = doc.add_object(Object::Stream(Stream::new(
            Dictionary::new(),
            content.as_bytes().to_vec(),
        )));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), page.0.into(), page.1.into()],
            "Rotate" => rotate,
            "Contents" => Object::Reference(content_id),
            "Resources" => Object::Reference(res_id),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn image_stream(w: usize, h: usize, extra: Dictionary, data: Vec<u8>) -> Stream {
        let mut d = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
        };
        for (k, v) in extra.into_iter() {
            d.set(k, v);
        }
        Stream::new(d, data)
    }

    fn reals(v: &[f32]) -> Object {
        Object::Array(v.iter().map(|&x| Object::Real(x)).collect())
    }

    /// Full-page placement of `/Im0` on a `w` × `h` point page.
    fn full_page(w: f32, h: f32) -> String {
        format!("q {w} 0 0 {h} 0 0 cm /Im0 Do Q")
    }

    /// The shapes fixture as packed 1-bit rows (1 = white), byte-aligned.
    fn shapes_bits() -> (usize, usize, Vec<u8>) {
        let img = image::open(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/fax/shapes.png"),
        )
        .unwrap()
        .to_luma8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let stride = w.div_ceil(8);
        let mut out = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                if img.get_pixel(x as u32, y as u32)[0] >= 128 {
                    out[y * stride + x / 8] |= 1 << (7 - x % 8);
                }
            }
        }
        (w, h, out)
    }

    /// The JBIG2 fixture's reference bitmap as packed 1-bit rows (1 = white).
    fn jbig2_reference_bits() -> (usize, usize, Vec<u8>) {
        let img = image::load_from_memory(&crate_fixture("tests/data/jbig2/generic_mmr.png"))
            .unwrap()
            .to_luma8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let stride = w.div_ceil(8);
        let mut out = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                if img.get_pixel(x as u32, y as u32)[0] >= 128 {
                    out[y * stride + x / 8] |= 1 << (7 - x % 8);
                }
            }
        }
        (w, h, out)
    }

    /// #811: an image-only page whose scan is a `JBIG2Decode` stream — as a
    /// 1-bit gray image and as a stencil — renders byte for byte like the
    /// same page with the bitmap stored as Flate, which the pdfium oracle
    /// above pins; so a JBIG2 scan takes this path instead of being
    /// declined (and drawn blank by the page renderer, before the fix).
    #[test]
    fn jbig2_scan_matches_its_flate_twin() {
        let (w, h, bits) = jbig2_reference_bits();
        let (wf, hf) = (w as f32, h as f32);
        let jbig2 = crate_fixture("tests/data/jbig2/generic_mmr.jbig2");
        let page =
            |stream: Stream| synth_pdf((wf, hf), 0, &full_page(wf, hf), vec![("Im0", stream)]);
        let gray = |extra: Dictionary, data: Vec<u8>| {
            let mut d = dictionary! { "ColorSpace" => "DeviceGray", "BitsPerComponent" => 1 };
            for (k, v) in extra.into_iter() {
                d.set(k, v);
            }
            image_stream(w, h, d, data)
        };
        let stencil = |extra: Dictionary, data: Vec<u8>| {
            let mut d = dictionary! { "ImageMask" => true };
            for (k, v) in extra.into_iter() {
                d.set(k, v);
            }
            image_stream(w, h, d, data)
        };
        let jb = || dictionary! { "Filter" => "JBIG2Decode" };
        for (label, flate_pdf, jbig2_pdf) in [
            (
                "gray",
                page(gray(Dictionary::new(), bits.clone())),
                page(gray(jb(), jbig2.clone())),
            ),
            (
                "stencil",
                page(stencil(Dictionary::new(), bits.clone())),
                page(stencil(jb(), jbig2.clone())),
            ),
        ] {
            for (dw, dh) in [(w as u32, h as u32), (2 * w as u32, 2 * h as u32)] {
                let flate = render(&PdfMeta::open(&flate_pdf).unwrap(), 0, dw, dh)
                    .unwrap_or_else(|| panic!("{label}: the Flate twin is declined"));
                let jbig2 = render(&PdfMeta::open(&jbig2_pdf).unwrap(), 0, dw, dh)
                    .unwrap_or_else(|| panic!("{label}: the JBIG2 page is declined"));
                assert_eq!(jbig2.as_raw(), flate.as_raw(), "{label} at {dw}x{dh}");
                let black = flate.pixels().filter(|p| p.0 == [0, 0, 0]).count();
                assert!(black > 500, "{label}: {black} black pixels");
            }
        }
    }

    /// Deterministic sample bytes.
    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut s = seed.wrapping_mul(2654435761).wrapping_add(12345);
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s >> 24) as u8
            })
            .collect()
    }

    /// The synthesized pages: every image kind the module reproduces.
    fn synthesized_cases() -> Vec<(String, Vec<u8>, (f32, f32))> {
        let mut cases: Vec<(String, Vec<u8>, (f32, f32))> = Vec::new();

        // Reduced-scale JPEG decoding: the device is small enough for
        // pdfium's `resolution_levels_to_skip` to be 1, 2 or 3.
        for name in [
            "rgb_420_big",
            "gray_444",
            "rgb_422",
            "rgb_444_progressive",
            "rgb_420_progressive",
            "gray_progressive",
        ] {
            let jpg = crate_fixture(&format!("tests/data/jpeg/{name}.jpg"));
            let info = jpeg::info(&jpg).unwrap();
            let (w, h) = (info.width as f32, info.height as f32);
            let cs = if info.components == 1 {
                "DeviceGray"
            } else {
                "DeviceRGB"
            };
            let stream = image_stream(
                info.width,
                info.height,
                dictionary! { "ColorSpace" => cs, "BitsPerComponent" => 8, "Filter" => "DCTDecode" },
                jpg,
            );
            cases.push((
                format!("jpeg {name}"),
                synth_pdf((w, h), 0, &full_page(w, h), vec![("Im0", stream)]),
                (w, h),
            ));
        }

        // CCITT as 1-bit gray, both bit conventions, and as stencils.
        let (sw, sh, bits) = shapes_bits();
        let (swf, shf) = (sw as f32, sh as f32);
        for (name, k) in [("shapes.g4", -1i64), ("shapes.g3", 0), ("shapes.g3_2d", 4)] {
            let data = crate_fixture(&format!("tests/data/fax/{name}"));
            let parms = |black: bool| {
                dictionary! { "K" => k, "Columns" => sw as i64, "Rows" => sh as i64, "BlackIs1" => black }
            };
            let gray = image_stream(
                sw,
                sh,
                dictionary! {
                    "ColorSpace" => "DeviceGray", "BitsPerComponent" => 1,
                    "Filter" => "CCITTFaxDecode", "DecodeParms" => parms(false),
                },
                data.clone(),
            );
            cases.push((
                format!("ccitt {name} gray"),
                synth_pdf((swf, shf), 0, &full_page(swf, shf), vec![("Im0", gray)]),
                (swf, shf),
            ));
            let inverted = image_stream(
                sw,
                sh,
                dictionary! {
                    "ColorSpace" => "DeviceGray", "BitsPerComponent" => 1,
                    "Filter" => "CCITTFaxDecode", "DecodeParms" => parms(true),
                    "Decode" => reals(&[1.0, 0.0]),
                },
                data.clone(),
            );
            cases.push((
                format!("ccitt {name} blackis1+decode"),
                synth_pdf((swf, shf), 0, &full_page(swf, shf), vec![("Im0", inverted)]),
                (swf, shf),
            ));
            let mask = image_stream(
                sw,
                sh,
                dictionary! { "ImageMask" => true, "Filter" => "CCITTFaxDecode", "DecodeParms" => parms(false) },
                data,
            );
            cases.push((
                format!("ccitt {name} mask"),
                synth_pdf(
                    (swf, shf),
                    0,
                    &format!("0.2 0.5 0.8 rg {}", full_page(swf, shf)),
                    vec![("Im0", mask)],
                ),
                (swf, shf),
            ));
        }

        // Flate stencil masks: default decode (0 = paint), `/Decode [1 0]`,
        // a gray fill through `cs`/`sc`, a clip, two placements, a rotated
        // page, `/Interpolate`.
        let mask_stream = |decode: Option<[f32; 2]>| {
            let mut d = dictionary! { "ImageMask" => true };
            if let Some(dec) = decode {
                d.set("Decode", reals(&dec));
            }
            image_stream(sw, sh, d, bits.clone())
        };
        cases.push((
            "mask default black".into(),
            synth_pdf(
                (swf, shf),
                0,
                &full_page(swf, shf),
                vec![("Im0", mask_stream(None))],
            ),
            (swf, shf),
        ));
        cases.push((
            "mask decode10 rgb".into(),
            synth_pdf(
                (swf, shf),
                0,
                &format!("0.9 0.1 0.3 rg {}", full_page(swf, shf)),
                vec![("Im0", mask_stream(Some([1.0, 0.0])))],
            ),
            (swf, shf),
        ));
        cases.push((
            "mask cs sc clip twice".into(),
            synth_pdf(
                (swf, shf),
                0,
                &format!(
                    "/DeviceGray cs 0.35 sc q 20 10 150 90 re W n {} Q q 60 0 0 40 120 80 cm /Im0 Do Q",
                    full_page(swf, shf)
                ),
                vec![("Im0", mask_stream(None))],
            ),
            (swf, shf),
        ));
        cases.push((
            "mask rotated page".into(),
            synth_pdf(
                (swf, shf),
                90,
                &format!("0 0 1 rg {}", full_page(swf, shf)),
                vec![("Im0", mask_stream(None))],
            ),
            (shf, swf),
        ));
        cases.push((
            "mask interpolate".into(),
            synth_pdf(
                (swf, shf),
                0,
                &full_page(swf, shf),
                vec![(
                    "Im0",
                    image_stream(
                        sw,
                        sh,
                        dictionary! { "ImageMask" => true, "Interpolate" => true },
                        bits.clone(),
                    ),
                )],
            ),
            (swf, shf),
        ));

        // Indexed and low-depth gray.
        let (iw, ih) = (203usize, 131usize);
        let indexed = |base: &str, hival: i64, table: Vec<u8>| -> Object {
            Object::Array(vec![
                "Indexed".into(),
                base.into(),
                hival.into(),
                Object::String(table, lopdf::StringFormat::Hexadecimal),
            ])
        };
        let raw = |label: &str, w: usize, h: usize, dict: Dictionary, data: Vec<u8>| {
            (
                label.to_string(),
                synth_pdf(
                    (w as f32, h as f32),
                    0,
                    &full_page(w as f32, h as f32),
                    vec![("Im0", image_stream(w, h, dict, data))],
                ),
                (w as f32, h as f32),
            )
        };
        cases.push(raw(
            "indexed 4-bit rgb",
            iw,
            ih,
            dictionary! { "ColorSpace" => indexed("DeviceRGB", 15, noise(48, 2)), "BitsPerComponent" => 4 },
            noise(iw.div_ceil(2) * ih, 1),
        ));
        cases.push(raw(
            "indexed 1-bit two colours",
            sw,
            sh,
            dictionary! { "ColorSpace" => indexed("DeviceRGB", 1, vec![200, 30, 30, 20, 40, 220]), "BitsPerComponent" => 1 },
            bits.clone(),
        ));
        cases.push(raw(
            "indexed 8-bit hival 40",
            iw,
            ih,
            dictionary! { "ColorSpace" => indexed("DeviceRGB", 40, noise(123, 3)), "BitsPerComponent" => 8 },
            noise(iw * ih, 4),
        ));
        cases.push(raw(
            "indexed 2-bit gray base",
            iw,
            ih,
            dictionary! { "ColorSpace" => indexed("DeviceGray", 3, vec![10, 90, 170, 250]), "BitsPerComponent" => 2 },
            noise(iw.div_ceil(4) * ih, 5),
        ));
        cases.push(raw(
            "gray 2-bit",
            iw,
            ih,
            dictionary! { "ColorSpace" => "DeviceGray", "BitsPerComponent" => 2 },
            noise(iw.div_ceil(4) * ih, 6),
        ));
        cases.push(raw(
            "gray 4-bit decode10",
            iw,
            ih,
            dictionary! { "ColorSpace" => "DeviceGray", "BitsPerComponent" => 4, "Decode" => reals(&[1.0, 0.0]) },
            noise(iw.div_ceil(2) * ih, 7),
        ));
        cases.push(raw(
            "gray 8-bit decode range",
            iw,
            ih,
            dictionary! { "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8, "Decode" => reals(&[0.2, 0.8]) },
            noise(iw * ih, 8),
        ));
        cases.push(raw(
            "rgb 8-bit inverted decode",
            iw,
            ih,
            dictionary! {
                "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
                "Decode" => reals(&[1.0, 0.0, 1.0, 0.0, 1.0, 0.0]),
            },
            noise(iw * ih * 3, 9),
        ));
        cases.push(raw(
            "calgray 1-bit",
            sw,
            sh,
            dictionary! {
                "ColorSpace" => Object::Array(vec![
                    "CalGray".into(),
                    Object::Dictionary(dictionary! { "WhitePoint" => reals(&[0.9505, 1.0, 1.089]), "Gamma" => 2.2f32 }),
                ]),
                "BitsPerComponent" => 1,
            },
            bits.clone(),
        ));
        cases
    }

    /// Pages synthesized around every image kind the module reproduces,
    /// each compared with pdfium's render at five sizes (down to the
    /// reduced-scale JPEG regimes): CCITT G4/G3 as gray and as stencils,
    /// Flate stencils with fill colours, clips and a rotated page, Indexed
    /// 1/2/4/8-bit, 2/4-bit gray, `/Decode` ranges on gray and RGB. Skipped
    /// without `libpdfium`.
    #[test]
    #[cfg(feature = "pdfium")]
    fn synthesized_pages_match_pdfium() {
        let mut failures = Vec::new();
        let mut compared = 0;
        for (label, pdf, (pw, ph)) in synthesized_cases() {
            let sizes: Vec<(u32, u32)> = [1.0f32, 2.5, 0.49, 0.24, 0.12]
                .iter()
                .map(|s| {
                    (
                        (pw * s).round().max(1.0) as u32,
                        (ph * s).round().max(1.0) as u32,
                    )
                })
                .collect();
            if !oracle(&label, &pdf, 0, &sizes, &mut failures) {
                return;
            }
            compared += sizes.len();
        }
        assert!(
            failures.is_empty(),
            "{}\n({compared} renders compared)",
            failures.join("\n")
        );
        eprintln!("{compared} synthesized renders byte-identical to pdfium");
    }

    /// Without pdfium: every synthesized kind decodes and draws, and a
    /// stencil paints its fill colour.
    #[test]
    fn synthesized_kinds_render_without_pdfium() {
        for (label, pdf, (pw, ph)) in synthesized_cases() {
            let meta = PdfMeta::open(&pdf).unwrap();
            let (w, h) = ((pw * 0.49).round() as u32, (ph * 0.49).round() as u32);
            assert!(
                render(&meta, 0, w, h).is_some(),
                "{label} declined at {w}x{h}"
            );
        }
        let (sw, sh, bits) = shapes_bits();
        let pdf = synth_pdf(
            (sw as f32, sh as f32),
            0,
            &format!("0.2 0.5 0.8 rg {}", full_page(sw as f32, sh as f32)),
            vec![(
                "Im0",
                image_stream(sw, sh, dictionary! { "ImageMask" => true }, bits),
            )],
        );
        let meta = PdfMeta::open(&pdf).unwrap();
        let img = render(&meta, 0, sw as u32, sh as u32).expect("stencil mask renders");
        let painted = img
            .pixels()
            .filter(|p| **p == image::Rgb([51, 128, 204]))
            .count();
        let white = img
            .pixels()
            .filter(|p| **p == image::Rgb([255, 255, 255]))
            .count();
        assert!(
            painted > 100 && white > 1000,
            "painted {painted}, white {white}"
        );
    }

    fn describe(doc: &Document, res: Option<&Dictionary>, content: &[u8], depth: usize) {
        let pad = "  ".repeat(depth);
        let ops = lopdf::content::Content::decode(content).expect("content");
        let mut shown = std::collections::BTreeMap::<String, usize>::new();
        for op in &ops.operations {
            *shown.entry(op.operator.clone()).or_default() += 1;
            if matches!(
                op.operator.as_str(),
                "cm" | "Do" | "re" | "gs" | "Tr" | "W" | "n"
            ) {
                println!("{pad}{} {:?}", op.operator, op.operands);
            }
        }
        println!("{pad}ops: {shown:?}");
        let Some(res) = res else { return };
        let Some(xobjs) = res.get(b"XObject").ok().and_then(|o| as_dict(doc, o)) else {
            return;
        };
        for (name, obj) in xobjs.iter() {
            let Some(stream) = deref(doc, obj).as_stream().ok() else {
                continue;
            };
            let mut d = stream.dict.clone();
            d.remove(b"Length");
            println!("{pad}/{} {:?}", String::from_utf8_lossy(name), d);
            if name_is(d.get(b"Subtype").ok(), b"Form") {
                let data = stream.decompressed_content().unwrap_or_default();
                let fres = d.get(b"Resources").ok().and_then(|o| as_dict(doc, o));
                describe(doc, fres, &data, depth + 1);
            } else if let Ok(Object::Reference(cs)) = d.get(b"ColorSpace") {
                println!("{pad}  ColorSpace -> {:?}", doc.get_object(*cs).ok());
            }
        }
    }

    /// Development aid: dump the content streams and XObjects of the scanned
    /// fixtures (`cargo test -p docling-pdf --lib raster::tests::dump -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn dump() {
        for rel in IMAGE_ONLY {
            let bytes = fixture(rel);
            let Some(doc) = crate::textparse::load_document(&bytes) else {
                continue;
            };
            let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
            pages.sort_by_key(|(n, _)| *n);
            for (n, pid) in pages.into_iter().take(3) {
                println!("=== {rel} p{n}");
                let content = doc.get_page_content(pid);
                let res = crate::textparse::page_res(&doc, pid);
                describe(&doc, res, &content, 0);
            }
        }
    }
}
