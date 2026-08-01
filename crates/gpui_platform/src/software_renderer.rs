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
