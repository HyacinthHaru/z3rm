use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use gpui::{
    AtlasKey, AtlasTile, AtlasTextureId, AtlasTextureKind, Background, BackgroundTag, Bounds,
    DevicePixels, Hsla, MonochromeSprite, Path, PlatformAtlas, PlatformHeadlessRenderer, Point,
    PolychromeSprite, PrimitiveBatch, Quad, ScaledPixels, Scene, Size, SubpixelSprite, TileId,
    Underline,
};
use image::{Rgba, RgbaImage};

/// Deterministic software renderer used by Linux visual regression tests.
///
/// It intentionally covers the scene primitives that make up GPUI chrome and
/// text layout. Text and image sprites are rendered as colored tiles rather
/// than delegated to a display server, which keeps screenshot baselines stable
/// in CI and still exercises scene construction, ordering, clipping, and atlas
/// allocation.
pub(crate) struct SoftwareHeadlessRenderer {
    atlas: Arc<SoftwareAtlas>,
}

impl SoftwareHeadlessRenderer {
    pub(crate) fn new() -> Self {
        Self {
            atlas: Arc::new(SoftwareAtlas::default()),
        }
    }

    fn render(&self, scene: &Scene, size: Size<DevicePixels>) -> RgbaImage {
        let width = size.width.0.max(1) as u32;
        let height = size.height.0.max(1) as u32;
        let mut image = RgbaImage::new(width, height);

        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::Shadows(range) => {
                    for shadow in &scene.shadows[range] {
                        let color = hsla_to_rgba(shadow.color);
                        fill_bounds(&mut image, shadow.bounds, color);
                    }
                }
                PrimitiveBatch::Quads(range) => {
                    for quad in &scene.quads[range] {
                        draw_quad(&mut image, quad);
                    }
                }
                PrimitiveBatch::Paths(range) => {
                    for path in &scene.paths[range] {
                        draw_path(&mut image, path);
                    }
                }
                PrimitiveBatch::Underlines(range) => {
                    for underline in &scene.underlines[range] {
                        draw_underline(&mut image, underline);
                    }
                }
                PrimitiveBatch::MonochromeSprites { range, .. } => {
                    for sprite in &scene.monochrome_sprites[range] {
                        draw_monochrome_sprite(&mut image, sprite, &self.atlas);
                    }
                }
                PrimitiveBatch::SubpixelSprites { range, .. } => {
                    for sprite in &scene.subpixel_sprites[range] {
                        draw_subpixel_sprite(&mut image, sprite, &self.atlas);
                    }
                }
                PrimitiveBatch::PolychromeSprites { range, .. } => {
                    for sprite in &scene.polychrome_sprites[range] {
                        draw_polychrome_sprite(&mut image, sprite, &self.atlas);
                    }
                }
                PrimitiveBatch::Surfaces(range) => {
                    for surface in &scene.surfaces[range] {
                        fill_bounds(
                            &mut image,
                            surface.bounds,
                            Rgba([0, 0, 0, 0]),
                        );
                    }
                }
            }
        }

        image
    }
}

impl PlatformHeadlessRenderer for SoftwareHeadlessRenderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> Result<RgbaImage> {
        Ok(self.render(scene, size))
    }

    fn render_scene(&mut self, _scene: &Scene, _size: Size<DevicePixels>) -> Result<()> {
        Ok(())
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }
}

#[derive(Default)]
struct SoftwareAtlas {
    next_tile_id: Mutex<u32>,
    tiles: Mutex<HashMap<AtlasKey, AtlasTile>>,
    pixels: Mutex<HashMap<u32, StoredTile>>,
}

#[derive(Clone)]
struct StoredTile {
    size: Size<DevicePixels>,
    kind: AtlasTextureKind,
    bytes: Vec<u8>,
}

impl SoftwareAtlas {
    fn tile_pixels(&self, tile_id: TileId) -> Option<StoredTile> {
        self.pixels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&tile_id.0)
            .cloned()
    }
}

impl PlatformAtlas for SoftwareAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        if let Some(tile) = self
            .tiles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .copied()
        {
            return Ok(Some(tile));
        }

        let Some((size, bytes)) = build()? else {
            return Ok(None);
        };
        let kind = key.texture_kind();
        let mut next_tile_id = self
            .next_tile_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *next_tile_id = next_tile_id.saturating_add(1).max(1);
        let tile = AtlasTile {
            texture_id: AtlasTextureId {
                index: kind as u32,
                kind,
            },
            tile_id: TileId(*next_tile_id),
            padding: 0,
            bounds: Bounds {
                origin: Point::default(),
                size,
            },
        };
        drop(next_tile_id);

        self.tiles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.clone(), tile);
        self.pixels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                tile.tile_id.0,
                StoredTile {
                    size,
                    kind,
                    bytes: bytes.into_owned(),
                },
            );
        Ok(Some(tile))
    }

    fn remove(&self, key: &AtlasKey) {
        let tile = self
            .tiles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(key);
        if let Some(tile) = tile {
            self.pixels
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&tile.tile_id.0);
        }
    }
}

fn draw_quad(image: &mut RgbaImage, quad: &Quad) {
    let color = background_color(quad.background, quad.bounds);
    fill_bounds(image, quad.bounds, color);
    let border_color = hsla_to_rgba(quad.border_color);
    let bounds = quad.bounds;
    let edges = quad.border_widths;
    fill_edge(image, bounds, edges.top, Edge::Top, border_color);
    fill_edge(image, bounds, edges.right, Edge::Right, border_color);
    fill_edge(image, bounds, edges.bottom, Edge::Bottom, border_color);
    fill_edge(image, bounds, edges.left, Edge::Left, border_color);
}

fn draw_path(image: &mut RgbaImage, path: &Path<ScaledPixels>) {
    let color = background_color(path.color, path.bounds);
    for triangle in path.vertices.chunks_exact(3) {
        let [a, b, c] = triangle else {
            continue;
        };
        let min_x = a
            .xy_position
            .x
            .0
            .min(b.xy_position.x.0)
            .min(c.xy_position.x.0);
        let max_x = a
            .xy_position
            .x
            .0
            .max(b.xy_position.x.0)
            .max(c.xy_position.x.0);
        let min_y = a
            .xy_position
            .y
            .0
            .min(b.xy_position.y.0)
            .min(c.xy_position.y.0);
        let max_y = a
            .xy_position
            .y
            .0
            .max(b.xy_position.y.0)
            .max(c.xy_position.y.0);
        fill_bounds(
            image,
            Bounds {
                origin: Point {
                    x: ScaledPixels(min_x),
                    y: ScaledPixels(min_y),
                },
                size: Size {
                    width: ScaledPixels(max_x - min_x),
                    height: ScaledPixels(max_y - min_y),
                },
            },
            color,
        );
    }
}

fn draw_underline(image: &mut RgbaImage, underline: &Underline) {
    let mut bounds = underline.bounds;
    bounds.origin.y.0 += (bounds.size.height.0 - underline.thickness.0).max(0.0);
    bounds.size.height.0 = underline.thickness.0.max(1.0);
    fill_bounds(image, bounds, hsla_to_rgba(underline.color));
}
fn draw_monochrome_sprite(
    image: &mut RgbaImage,
    sprite: &MonochromeSprite,
    atlas: &SoftwareAtlas,
) {
    draw_coverage_sprite(image, sprite.bounds, sprite.color, sprite.tile.tile_id, atlas);
}

fn draw_subpixel_sprite(
    image: &mut RgbaImage,
    sprite: &SubpixelSprite,
    atlas: &SoftwareAtlas,
) {
    draw_coverage_sprite(image, sprite.bounds, sprite.color, sprite.tile.tile_id, atlas);
}

fn draw_polychrome_sprite(
    image: &mut RgbaImage,
    sprite: &PolychromeSprite,
    atlas: &SoftwareAtlas,
) {
    let Some(tile) = atlas.tile_pixels(sprite.tile.tile_id) else {
        return;
    };
    draw_sprite_pixels(image, sprite.bounds, &tile, None);
}

fn draw_coverage_sprite(
    image: &mut RgbaImage,
    bounds: Bounds<ScaledPixels>,
    color: Hsla,
    tile_id: TileId,
    atlas: &SoftwareAtlas,
) {
    let Some(tile) = atlas.tile_pixels(tile_id) else {
        return;
    };
    draw_sprite_pixels(image, bounds, &tile, Some(hsla_to_rgba(color)));
}

fn draw_sprite_pixels(
    image: &mut RgbaImage,
    bounds: Bounds<ScaledPixels>,
    tile: &StoredTile,
    tint: Option<Rgba<u8>>,
) {
    let destination_width = bounds.size.width.0.ceil().max(1.0) as u32;
    let destination_height = bounds.size.height.0.ceil().max(1.0) as u32;
    let source_width = tile.size.width.0.max(1) as usize;
    let source_height = tile.size.height.0.max(1) as usize;
    let channels = match tile.kind {
        AtlasTextureKind::Monochrome => 1,
        AtlasTextureKind::Subpixel => 3,
        AtlasTextureKind::Polychrome => 4,
    };
    let expected_bytes = source_width.saturating_mul(source_height).saturating_mul(channels);
    if tile.bytes.len() < expected_bytes {
        if let Some(tint) = tint {
            fill_bounds(image, bounds, tint);
        }
        return;
    }

    for y in 0..destination_height {
        let source_y =
            (y as usize * source_height / destination_height as usize) * source_width;
        for x in 0..destination_width {
            let source_x = x as usize * source_width / destination_width as usize;
            let offset = (source_y + source_x) * channels;
            let sample = match tile.kind {
                AtlasTextureKind::Monochrome => {
                    let coverage = tile.bytes[offset] as f32 / 255.0;
                    tint.map(|color| Rgba([
                        color[0],
                        color[1],
                        color[2],
                        (color[3] as f32 * coverage).round() as u8,
                    ]))
                }
                AtlasTextureKind::Subpixel => tint.map(|color| {
                    let red = tile.bytes[offset] as f32 / 255.0;
                    let green = tile.bytes[offset + 1] as f32 / 255.0;
                    let blue = tile.bytes[offset + 2] as f32 / 255.0;
                    Rgba([
                        (color[0] as f32 * red).round() as u8,
                        (color[1] as f32 * green).round() as u8,
                        (color[2] as f32 * blue).round() as u8,
                        color[3],
                    ])
                }),
                AtlasTextureKind::Polychrome => Some(Rgba([
                    tile.bytes[offset],
                    tile.bytes[offset + 1],
                    tile.bytes[offset + 2],
                    tile.bytes[offset + 3],
                ])),
            };
            if let Some(sample) = sample {
                let image_x = bounds.origin.x.0.floor().max(0.0) as u32 + x;
                let image_y = bounds.origin.y.0.floor().max(0.0) as u32 + y;
                if image_x < image.width() && image_y < image.height() {
                    let pixel = image.get_pixel_mut(image_x, image_y);
                    *pixel = blend(*pixel, sample);
                }
            }
        }
    }
}


#[derive(Clone, Copy)]
enum Edge {
    Top,
    Right,
    Bottom,
    Left,
}

fn fill_edge(
    image: &mut RgbaImage,
    bounds: Bounds<ScaledPixels>,
    width: ScaledPixels,
    edge: Edge,
    color: Rgba<u8>,
) {
    if width.0 <= 0.0 {
        return;
    }
    let mut edge_bounds = bounds;
    match edge {
        Edge::Top => edge_bounds.size.height = ScaledPixels(width.0),
        Edge::Right => {
            edge_bounds.origin.x.0 += (bounds.size.width.0 - width.0).max(0.0);
            edge_bounds.size.width = ScaledPixels(width.0);
        }
        Edge::Bottom => {
            edge_bounds.origin.y.0 += (bounds.size.height.0 - width.0).max(0.0);
            edge_bounds.size.height = ScaledPixels(width.0);
        }
        Edge::Left => edge_bounds.size.width = ScaledPixels(width.0),
    }
    fill_bounds(image, edge_bounds, color);
}

fn fill_bounds(image: &mut RgbaImage, bounds: Bounds<ScaledPixels>, color: Rgba<u8>) {
    let x0 = bounds.origin.x.0.floor().max(0.0) as u32;
    let y0 = bounds.origin.y.0.floor().max(0.0) as u32;
    let x1 = (bounds.origin.x.0 + bounds.size.width.0)
        .ceil()
        .max(0.0) as u32;
    let y1 = (bounds.origin.y.0 + bounds.size.height.0)
        .ceil()
        .max(0.0) as u32;
    let x1 = x1.min(image.width());
    let y1 = y1.min(image.height());

    for y in y0.min(y1)..y1 {
        for x in x0.min(x1)..x1 {
            let pixel = image.get_pixel_mut(x, y);
            *pixel = blend(*pixel, color);
        }
    }
}

fn blend(base: Rgba<u8>, overlay: Rgba<u8>) -> Rgba<u8> {
    let alpha = overlay[3] as f32 / 255.0;
    if alpha >= 1.0 {
        return overlay;
    }
    if alpha <= 0.0 {
        return base;
    }
    let base_alpha = base[3] as f32 / 255.0;
    let output_alpha = alpha + base_alpha * (1.0 - alpha);
    if output_alpha <= 0.0 {
        return Rgba([0, 0, 0, 0]);
    }
    Rgba([
        ((overlay[0] as f32 * alpha + base[0] as f32 * base_alpha * (1.0 - alpha))
            / output_alpha)
            .round() as u8,
        ((overlay[1] as f32 * alpha + base[1] as f32 * base_alpha * (1.0 - alpha))
            / output_alpha)
            .round() as u8,
        ((overlay[2] as f32 * alpha + base[2] as f32 * base_alpha * (1.0 - alpha))
            / output_alpha)
            .round() as u8,
        (output_alpha * 255.0).round() as u8,
    ])
}

fn background_color(background: Background, bounds: Bounds<ScaledPixels>) -> Rgba<u8> {
    match background.tag() {
        BackgroundTag::Solid => hsla_to_rgba(background.solid_color()),
        BackgroundTag::LinearGradient => {
            let [from, to] = background.gradient_stops();
            let angle = background.gradient_angle_or_pattern_height().to_radians();
            let dx = angle.cos();
            let dy = angle.sin();
            let center_x = bounds.origin.x.0 + bounds.size.width.0 / 2.0;
            let center_y = bounds.origin.y.0 + bounds.size.height.0 / 2.0;
            let extent = (bounds.size.width.0.abs() * dx.abs()
                + bounds.size.height.0.abs() * dy.abs())
                .max(1.0);
            let t = ((center_x * dx + center_y * dy) / extent + 0.5).clamp(0.0, 1.0);
            let left = hsla_to_rgba(from.color);
            let right = hsla_to_rgba(to.color);
            interpolate(left, right, t)
        }
        BackgroundTag::PatternSlash | BackgroundTag::Checkerboard => {
            hsla_to_rgba(background.solid_color())
        }
    }
}

fn interpolate(left: Rgba<u8>, right: Rgba<u8>, t: f32) -> Rgba<u8> {
    Rgba([
        lerp(left[0], right[0], t),
        lerp(left[1], right[1], t),
        lerp(left[2], right[2], t),
        lerp(left[3], right[3], t),
    ])
}

fn lerp(left: u8, right: u8, t: f32) -> u8 {
    (left as f32 + (right as f32 - left as f32) * t).round() as u8
}

fn hsla_to_rgba(color: Hsla) -> Rgba<u8> {
    let color: gpui::Rgba = color.into();
    Rgba([
        (color.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.b.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.a.clamp(0.0, 1.0) * 255.0).round() as u8,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AtlasKey, BorderStyle, ContentMask, Corners, Edges, Point, RenderSvgParams, SharedString,
        TransformationMatrix,
    };

    /// Solid sRGB colors that survive the `Hsla -> Rgba -> u8` round trip
    /// exactly, so the assertions below are byte-exact baselines.
    const GRAY: Hsla = Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.25,
        a: 1.0,
    };
    const RED: Hsla = Hsla {
        h: 0.0,
        s: 1.0,
        l: 0.5,
        a: 1.0,
    };
    const GREEN: Hsla = Hsla {
        h: 1.0 / 3.0,
        s: 1.0,
        l: 0.5,
        a: 1.0,
    };

    const GRAY_RGBA: Rgba<u8> = Rgba([64, 64, 64, 255]);
    const RED_RGBA: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const GREEN_RGBA: Rgba<u8> = Rgba([0, 255, 0, 255]);

    fn quad(bounds: Bounds<ScaledPixels>, background: Hsla) -> Quad {
        Quad {
            order: 0,
            bounds,
            content_mask: ContentMask { bounds },
            background: Background::from(background),
            border_color: Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.0,
                a: 0.0,
            },
            corner_radii: Corners::default(),
            border_widths: Edges::default(),
            border_style: BorderStyle::Solid,
        }
    }

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<ScaledPixels> {
        Bounds {
            origin: Point::new(ScaledPixels(x), ScaledPixels(y)),
            size: Size {
                width: ScaledPixels(w),
                height: ScaledPixels(h),
            },
        }
    }

    fn render(renderer: &mut SoftwareHeadlessRenderer, scene: &Scene, width: u32, height: u32) -> RgbaImage {
        renderer
            .render_scene_to_image(
                scene,
                Size {
                    width: DevicePixels(width as i32),
                    height: DevicePixels(height as i32),
                },
            )
            .expect("software render must not fail")
    }

    #[test]
    fn empty_scene_renders_transparent_frame() {
        let mut scene = Scene::default();
        scene.finish();
        let image = render(&mut SoftwareHeadlessRenderer::new(), &scene, 4, 4);
        assert_eq!((image.width(), image.height()), (4, 4));
        assert!(
            image.pixels().all(|pixel| pixel == &Rgba([0, 0, 0, 0])),
            "empty scene must produce a fully transparent, non-blank-in-pixel-count frame"
        );
    }

    #[test]
    fn solid_quad_matches_exact_baseline() {
        let mut scene = Scene::default();
        scene.insert_primitive(quad(bounds(0.0, 0.0, 8.0, 8.0), GRAY));
        scene.finish();
        let image = render(&mut SoftwareHeadlessRenderer::new(), &scene, 8, 8);
        assert!(
            image.pixels().all(|pixel| pixel == &GRAY_RGBA),
            "every pixel must be the quad color"
        );
    }

    #[test]
    fn overlapping_quads_respect_paint_order() {
        let mut scene = Scene::default();
        scene.insert_primitive(quad(bounds(0.0, 0.0, 8.0, 8.0), GRAY));
        scene.insert_primitive(quad(bounds(2.0, 2.0, 4.0, 4.0), GREEN));
        scene.finish();
        let image = render(&mut SoftwareHeadlessRenderer::new(), &scene, 8, 8);
        for y in 0..image.height() {
            for x in 0..image.width() {
                let inside = (2..6).contains(&x) && (2..6).contains(&y);
                let expected = if inside { GREEN_RGBA } else { GRAY_RGBA };
                assert_eq!(
                    image.get_pixel(x, y),
                    &expected,
                    "pixel ({x}, {y}): later quad must paint over the earlier one"
                );
            }
        }
    }

    #[test]
    fn monochrome_sprite_coverage_maps_to_alpha() {
        // A 2x2 glyph-shaped tile: fully covered on the left column, partially
        // covered on the right, empty at the bottom. The renderer must turn
        // coverage into alpha over the sprite's tint, not into a fabricated
        // solid block.
        let mut renderer = SoftwareHeadlessRenderer::new();
        let atlas = renderer.sprite_atlas();
        let key = AtlasKey::Svg(RenderSvgParams {
            path: SharedString::from("software-renderer-test"),
            size: Size {
                width: DevicePixels(2),
                height: DevicePixels(2),
            },
        });
        let tile = atlas
            .get_or_insert_with(&key, &mut || {
                Ok(Some((
                    Size {
                        width: DevicePixels(2),
                        height: DevicePixels(2),
                    },
                    Cow::Owned(vec![255, 127, 0, 0]),
                )))
            })
            .expect("atlas insert must succeed")
            .expect("tile must be built");

        let mut scene = Scene::default();
        scene.insert_primitive(MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: bounds(0.0, 0.0, 2.0, 2.0),
            content_mask: ContentMask {
                bounds: bounds(0.0, 0.0, 2.0, 2.0),
            },
            color: GREEN,
            tile,
            transformation: TransformationMatrix::unit(),
        });
        scene.finish();
        let image = render(&mut renderer, &scene, 2, 2);

        assert_eq!(image.get_pixel(0, 0), &GREEN_RGBA, "full coverage, opaque");
        assert_eq!(
            image.get_pixel(1, 0),
            &Rgba([0, 255, 0, 127]),
            "coverage 127/255 must yield 50% alpha"
        );
        assert_eq!(
            image.get_pixel(0, 1),
            &Rgba([0, 0, 0, 0]),
            "zero coverage must stay transparent"
        );
        assert_eq!(
            image.get_pixel(1, 1),
            &Rgba([0, 0, 0, 0]),
            "zero coverage must stay transparent"
        );
    }

    #[test]
    fn border_edges_paint_over_quad_fill() {
        let mut scene = Scene::default();
        let mut bordered = quad(bounds(0.0, 0.0, 8.0, 8.0), GRAY);
        bordered.border_color = RED;
        bordered.border_widths = Edges {
            top: ScaledPixels(1.0),
            right: ScaledPixels(0.0),
            bottom: ScaledPixels(0.0),
            left: ScaledPixels(0.0),
        };
        scene.insert_primitive(bordered);
        scene.finish();
        let image = render(&mut SoftwareHeadlessRenderer::new(), &scene, 8, 8);
        for x in 0..8 {
            assert_eq!(
                image.get_pixel(x, 0),
                &RED_RGBA,
                "top border row must be the border color"
            );
        }
        for y in 1..8 {
            for x in 0..8 {
                assert_eq!(
                    image.get_pixel(x, y),
                    &GRAY_RGBA,
                    "fill below the border must stay the quad color"
                );
            }
        }
    }
}
