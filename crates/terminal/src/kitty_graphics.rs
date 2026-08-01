//! Kitty graphics protocol (kitty-graphics) and iTerm2 OSC 1337 image display parser.
//!
//! §11.2 终端图形协议支持
//!
//! 支持两种图像传输协议:
//! 1. iTerm2 OSC 1337: `ESC ] 1337 ; File=<params> : <base64_data> ST`
//! 2. Kitty Graphics (DCS): `ESC _ G <params> ; <base64_data> ESC \ `
//!
//! 参考:
//! - iTerm2: https://iterm2.com/documentation-images.html
//! - Kitty:  https://sw.kovidgoyal.net/kitty/graphics-protocol/

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use collections::HashMap;
use gpui::RenderImage;
use image::GenericImageView;
use image::ImageBuffer;

// ──────────────────────────────────────────────
// 图像标识与元数据
// ──────────────────────────────────────────────

/// 缓存中的图像 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageId(pub u64);

/// 协议解析后产生的图像数据
#[derive(Debug, Clone)]
pub struct ParsedImage {
    /// 图像在终端网格中的起始位置 (行, 列)
    pub placement: Option<(usize, usize)>,
    /// 图像像素尺寸
    pub pixel_size: (u32, u32),
    /// 图像在网格中占据的单元格尺寸
    pub cell_size: Option<(usize, usize)>,
    /// 编码后的图像数据 (base64 解码后的原始字节)
    pub data: Vec<u8>,
}

/// 图像显示操作类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageAction {
    /// 发送图像数据
    Send,
    /// 查询图像信息
    Query,
    /// 删除已缓存的图像
    Delete,
}

/// Kitty Graphics 协议参数
#[derive(Debug, Clone)]
pub struct KittyGraphicsParams {
    /// 操作 (a): S=发送, Q=查询, T=传输, D=删除, C=清除
    pub action: ImageAction,
    /// 图像标识符 (t): 0-99, 0 表示自动分配
    pub identifier: u8,
    /// 传输格式 (T): 0=raw, 1=base64
    pub transfer_format: u8,
    /// 放置模式 (p): 0=绝对, 1=相对光标
    pub placement_mode: u8,
    /// 图像行号 (r)
    pub row: Option<u32>,
    /// 图像列号 (c)
    pub column: Option<u32>,
    /// 图像宽度 (z): -1 表示自动计算
    pub width: i32,
    /// 图像高度 (Z): -1 表示自动计算
    pub height: i32,
    /// 缩放 (s): 1=不缩放, 2=保持宽高比, 3=填充
    pub scale: u8,
    /// 图像 ID 引用 (i)
    pub image_id: Option<String>,
    /// 列宽 (w): 单元格宽度
    pub columns: Option<u32>,
    /// 行高 (h): 单元格高度
    pub rows: Option<u32>,
    /// 层 (l): 0=默认, 1=下, 2=上
    pub layer: u8,
}

impl Default for KittyGraphicsParams {
    fn default() -> Self {
        Self {
            action: ImageAction::Send,
            identifier: 0,
            transfer_format: 1,
            placement_mode: 0,
            row: None,
            column: None,
            width: -1,
            height: -1,
            scale: 1,
            image_id: None,
            columns: None,
            rows: None,
            layer: 0,
        }
    }
}

// ──────────────────────────────────────────────
// 每 pane 图像缓存
// ──────────────────────────────────────────────

/// 每 pane 的图像缓存, 按 ID 管理
#[derive(Debug, Clone)]
pub struct PaneImageCache {
    /// 已解码的图像映射
    pub images: HashMap<ImageId, ParsedImage>,
    /// LRU 淘汰顺序
    pub lru_order: VecDeque<ImageId>,
    /// 缓存大小上限 (字节)
    max_size_bytes: usize,
    /// 缓存图像数量上限
    max_images: usize,
    /// 下一个图像 ID
    next_id: u64,
    /// 当前总大小 (字节)
    current_size: usize,
}

impl PaneImageCache {
    /// 创建新的图像缓存, 默认上限 10MB / 100 张图像
    pub fn new() -> Self {
        Self {
            images: HashMap::default(),
            lru_order: VecDeque::new(),
            max_size_bytes: 10 * 1024 * 1024, // 10 MB
            max_images: 100,
            next_id: 0,
            current_size: 0,
        }
    }

    /// 设置缓存大小上限
    pub fn set_max_size_bytes(&mut self, bytes: usize) {
        self.max_size_bytes = bytes;
        self.evict_if_needed();
    }

    /// 设置缓存数量上限
    pub fn set_max_images(&mut self, count: usize) {
        self.max_images = count;
        self.evict_if_needed();
    }

    /// 分配新图像 ID
    fn next_image_id(&mut self) -> ImageId {
        let id = self.next_id;
        self.next_id += 1;
        ImageId(id)
    }

    /// 插入图像到缓存
    pub fn insert(&mut self, image: ParsedImage) -> ImageId {
        let id = self.next_image_id();
        self.current_size += image.data.len();
        self.images.insert(id, image);
        self.lru_order.push_front(id);
        self.evict_if_needed();
        id
    }

    /// 访问图像 (更新 LRU 顺序)
    pub fn access(&mut self, id: ImageId) -> Option<&ParsedImage> {
        if self.images.contains_key(&id) {
            self.lru_order.retain(|&x| x != id);
            self.lru_order.push_front(id);
        }
        self.images.get(&id)
    }

    /// 获取图像
    pub fn get(&self, id: ImageId) -> Option<&ParsedImage> {
        self.images.get(&id)
    }

    /// 删除指定图像
    pub fn remove(&mut self, id: ImageId) {
        if let Some(image) = self.images.remove(&id) {
            self.current_size -= image.data.len();
            self.lru_order.retain(|&x| x != id);
        }
    }

    /// 清空缓存
    pub fn clear(&mut self) {
        self.images.clear();
        self.lru_order.clear();
        self.current_size = 0;
    }

    /// LRU 淘汰策略
    fn evict_if_needed(&mut self) {
        while self.current_size > self.max_size_bytes || self.images.len() > self.max_images {
            if let Some(oldest) = self.lru_order.pop_back() {
                if let Some(image) = self.images.remove(&oldest) {
                    self.current_size -= image.data.len();
                }
            } else {
                break;
            }
        }
    }

    /// 获取缓存统计信息
    pub fn stats(&self) -> (usize, usize, usize) {
        (self.images.len(), self.current_size, self.lru_order.len())
    }
}

impl Default for PaneImageCache {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────
// OSC 1337 (iTerm2) 解析器
// ──────────────────────────────────────────────

/// 解析 iTerm2 OSC 1337 参数
///
/// OSC 1337 格式:
/// `ESC ] 1337 ; File=<params> : <base64_data> ST`
///
/// <params> 为分号分隔的键值对:
/// - `name=<path>`: 文件路径 (用于获取尺寸信息)
/// - `size=<bytes>`: 文件大小
/// - `z=<compression>`: 压缩级别 (0=无, 1=lzma, 2=zlib)
/// - `inline=1`: 内联数据 (base64 编码)
/// - `id=<id>`: 图像标识符
/// - `w=<width>`, `h=<height>`: 显示尺寸
/// - `s=<scale>`: 缩放模式
fn parse_osc1337_params(params_str: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for part in params_str.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

/// 解析 OSC 1337 序列
///
/// 参数: OSC 1337 的完整 payload, 格式为 `File=<params>:<base64_data>`
///
/// §11.2 iTerm2 OSC 1337 协议解析
pub fn parse_osc1337(payload: &str) -> Option<ParsedImage> {
    // 解析参数部分
    let params_end = payload.find(':')?;
    let params_str = &payload[..params_end];

    // 验证以 File= 开头
    if !params_str.starts_with("File=") {
        return None;
    }

    let file_params = &params_str[5..];
    let param_map = parse_osc1337_params(file_params);

    // 检查是否内联数据
    let inline = param_map.get("inline").and_then(|v| v.parse::<u8>().ok());
    if inline != Some(1) {
        // 非内联模式, 跳过
        return None;
    }

    // 提取 base64 数据
    let base64_data = &payload[params_end + 1..];
    let encoded_data = base64_data.trim();
    if encoded_data.is_empty() {
        return None;
    }

    let raw_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded_data).ok()?;
    if raw_bytes.is_empty() {
        return None;
    }

    // 解析图像尺寸
    let pixel_size = decode_image_size(&raw_bytes).unwrap_or((raw_bytes.len() as u32, 1));

    Some(ParsedImage {
        placement: None,
        pixel_size,
        cell_size: None,
        data: raw_bytes,
    })
}

// ──────────────────────────────────────────────
// Kitty Graphics (DCS) 解析器
// ──────────────────────────────────────────────

/// 解析 Kitty Graphics DCS 参数字符串
///
/// DCS 格式: `ESC _ G <params> ; <base64_data> ESC \ `
///
/// <params> 为逗号分隔的键值对:
/// - `a=<action>`: 操作 (S=发送, Q=查询, T=传输, D=删除, C=清除)
/// - `t=<identifier>`: 图像标识符 (0-99)
/// - `T=<format>`: 传输格式 (0=raw, 1=base64)
/// - `p=<placement>`: 放置模式
/// - `r=<row>`, `c=<column>`: 位置
/// - `z=<width>`, `Z=<height>`: 像素尺寸
/// - `s=<scale>`: 缩放
/// - `i=<id>`: 图像 ID
/// - `w=<columns>`, `h=<rows>`: 单元格尺寸
/// - `l=<layer>`: 图层
///
/// §11.2 Kitty Graphics 协议解析
fn parse_kitty_params(params_str: &str) -> KittyGraphicsParams {
    let mut params = KittyGraphicsParams::default();

    for part in params_str.split(',') {
        let part = part.trim();
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "a" => {
                    params.action = match value.chars().next() {
                        Some('S') | Some('s') => ImageAction::Send,
                        Some('Q') | Some('q') => ImageAction::Query,
                        Some('D') | Some('d') => ImageAction::Delete,
                        _ => ImageAction::Send,
                    };
                }
                "t" => {
                    params.identifier = value.parse().unwrap_or(0);
                }
                "T" => {
                    params.transfer_format = value.parse().unwrap_or(1);
                }
                "p" => {
                    params.placement_mode = value.parse().unwrap_or(0);
                }
                "r" => {
                    params.row = value.parse().ok();
                }
                "c" => {
                    params.column = value.parse().ok();
                }
                "z" => {
                    params.width = value.parse().unwrap_or(-1);
                }
                "Z" => {
                    params.height = value.parse().unwrap_or(-1);
                }
                "s" => {
                    params.scale = value.parse().unwrap_or(1);
                }
                "i" => {
                    params.image_id = Some(value.to_string());
                }
                "w" => {
                    params.columns = value.parse().ok();
                }
                "h" => {
                    params.rows = value.parse().ok();
                }
                "l" => {
                    params.layer = value.parse().unwrap_or(0);
                }
                _ => {}
            }
        }
    }

    params
}

/// 解析 Kitty Graphics DCS 序列
///
/// 参数: DCS 的 payload, 格式为 `<params> ; <base64_data>`
///
/// §11.2 Kitty Graphics DCS 协议解析
pub fn parse_kitty_graphics(payload: &str) -> Option<(KittyGraphicsParams, ParsedImage)> {
    // 分割参数和数据 (最后一个分号分隔)
    let semicolon_pos = payload.rfind(';')?;
    let params_str = &payload[..semicolon_pos];
    let base64_data = &payload[semicolon_pos + 1..];

    let params = parse_kitty_params(params_str);

    // 跳过非发送操作
    if params.action != ImageAction::Send {
        return None;
    }

    let encoded_data = base64_data.trim();
    if encoded_data.is_empty() {
        return None;
    }

    // 传输格式 0 = raw, 1 = base64
    let raw_bytes = if params.transfer_format == 0 {
        encoded_data.as_bytes().to_vec()
    } else {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded_data).ok()?
    };

    if raw_bytes.is_empty() {
        return None;
    }

    // 解析图像尺寸
    let pixel_size = decode_image_size(&raw_bytes).unwrap_or((raw_bytes.len() as u32, 1));

    // 计算放置位置
    let placement = if params.row.is_some() || params.column.is_some() {
        Some((
            params.row.unwrap_or(0) as usize,
            params.column.unwrap_or(0) as usize,
        ))
    } else {
        None
    };

    // 计算单元格尺寸
    let cell_size = if params.columns.is_some() || params.rows.is_some() {
        Some((
            params.columns.unwrap_or(1) as usize,
            params.rows.unwrap_or(1) as usize,
        ))
    } else {
        None
    };

    let image = ParsedImage {
        placement,
        pixel_size,
        cell_size,
        data: raw_bytes,
    };

    Some((params, image))
}

// ──────────────────────────────────────────────
// 图像解码辅助函数
// ──────────────────────────────────────────────

/// 尝试解码图像数据并获取尺寸
fn decode_image_size(data: &[u8]) -> Option<(u32, u32)> {
    // 尝试用 image crate 解码
    if let Ok(img) = image::load_from_memory(data) {
        return Some(img.dimensions());
    }

    // 手动检查 PNG 签名
    if data.len() >= 24 && data[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        if width > 0 && height > 0 {
            return Some((width, height));
        }
    }

    // 手动检查 JPEG 签名
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
        // 简化: 对于 JPEG 返回 None, 让调用方处理
    }

    None
}

/// 解码图像数据为 GPUI RenderImage
///
/// §11.2 将原始图像数据转换为 GPUI 渲染格式
pub fn decode_to_render_image(data: &[u8]) -> Option<Arc<RenderImage>> {
    // 使用 image crate 解码, 转换为 RGBA8
    let img = image::load_from_memory(data).ok()?;
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();

    // image crate 的 RgbaImage 是 RGBA 格式, GPUI 的 RenderImage 需要 BGRA
    let bgra: Vec<u8> = rgba
        .as_raw()
        .chunks_exact(4)
        .flat_map(|rgba| [rgba[2], rgba[1], rgba[0], rgba[3]])
        .collect();

    let buffer = ImageBuffer::from_vec(width, height, bgra)?;
    let frame = image::Frame::new(buffer);

    Some(Arc::new(RenderImage::new([frame])))
}

// ──────────────────────────────────────────────
// 协议分派器
// ──────────────────────────────────────────────

/// 图像协议类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphicsProtocol {
    /// iTerm2 OSC 1337
    Osci1337,
    /// Kitty Graphics DCS
    KittyGraphics,
}

/// 解析结果
#[derive(Debug, Clone)]
pub struct ParsedGraphics {
    /// 协议类型
    pub protocol: GraphicsProtocol,
    /// Kitty Graphics 参数 (仅 Kitty)
    pub kitty_params: Option<KittyGraphicsParams>,
    /// 解析后的图像数据
    pub image: ParsedImage,
}

/// 尝试解析图像协议序列
///
/// 根据 payload 格式自动识别协议类型并解析
///
/// §11.2 协议分派入口
pub fn parse_graphics_protocol(
    protocol: GraphicsProtocol,
    payload: &str,
) -> Option<ParsedGraphics> {
    match protocol {
        GraphicsProtocol::Osci1337 => {
            let image = parse_osc1337(payload)?;
            Some(ParsedGraphics {
                protocol: GraphicsProtocol::Osci1337,
                kitty_params: None,
                image,
            })
        }
        GraphicsProtocol::KittyGraphics => {
            let (params, image) = parse_kitty_graphics(payload)?;
            Some(ParsedGraphics {
                protocol: GraphicsProtocol::KittyGraphics,
                kitty_params: Some(params),
                image,
            })
        }
    }
}

// ──────────────────────────────────────────────
// 测试
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn test_parse_osc1337_params() {
        let params = parse_osc1337_params("name=test.png;inline=1;z=0");
        assert_eq!(params.get("name"), Some(&"test.png".to_string()));
        assert_eq!(params.get("inline"), Some(&"1".to_string()));
        assert_eq!(params.get("z"), Some(&"0".to_string()));
    }

    #[test]
    fn test_parse_osc1337_inline() {
        // 创建一个最小 PNG (1x1 白色像素)
        let png_data = create_test_png();
        let base64 = base64::engine::general_purpose::STANDARD.encode(&png_data);

        let payload = format!("File=name=test.png;inline=1:{base64}");
        let result = parse_osc1337(&payload);
        assert!(result.is_some());

        let image = result.unwrap();
        assert_eq!(image.pixel_size, (1, 1));
        assert_eq!(image.data, png_data);
    }

    #[test]
    fn test_parse_osc1337_no_inline() {
        let payload = "File=name=test.png;z=0:data";
        let result = parse_osc1337(&payload);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_osc1337_not_file() {
        let payload = "Other=data:abc";
        let result = parse_osc1337(&payload);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_kitty_params_send() {
        let params = parse_kitty_params("a=S,t=0,T=1,p=0,r=10,c=5,z=100,Z=200,s=1");
        assert_eq!(params.action, ImageAction::Send);
        assert_eq!(params.identifier, 0);
        assert_eq!(params.transfer_format, 1);
        assert_eq!(params.placement_mode, 0);
        assert_eq!(params.row, Some(10));
        assert_eq!(params.column, Some(5));
        assert_eq!(params.width, 100);
        assert_eq!(params.height, 200);
        assert_eq!(params.scale, 1);
    }

    #[test]
    fn test_parse_kitty_params_delete() {
        let params = parse_kitty_params("a=D,t=5");
        assert_eq!(params.action, ImageAction::Delete);
        assert_eq!(params.identifier, 5);
    }

    #[test]
    fn test_parse_kitty_params_query() {
        let params = parse_kitty_params("a=Q,t=3");
        assert_eq!(params.action, ImageAction::Query);
        assert_eq!(params.identifier, 3);
    }

    #[test]
    fn test_parse_kitty_graphics_send() {
        let png_data = create_test_png();
        let base64 = base64::engine::general_purpose::STANDARD.encode(&png_data);

        let payload = format!("a=S,t=0,T=1;{base64}");
        let result = parse_kitty_graphics(&payload);
        assert!(result.is_some());

        let (params, image) = result.unwrap();
        assert_eq!(params.action, ImageAction::Send);
        assert_eq!(image.pixel_size, (1, 1));
    }

    #[test]
    fn test_parse_kitty_graphics_delete_returns_none() {
        let payload = "a=D,t=0;data";
        let result = parse_kitty_graphics(&payload);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_kitty_graphics_placement() {
        let png_data = create_test_png();
        let base64 = base64::engine::general_purpose::STANDARD.encode(&png_data);

        let payload = format!("a=S,t=0,T=1,p=0,r=10,c=5,z=20,Z=30;{base64}");
        let result = parse_kitty_graphics(&payload).unwrap();
        assert_eq!(result.1.placement, Some((10, 5)));
        assert_eq!(result.1.pixel_size, (1, 1)); // PNG is 1x1
        assert_eq!(result.0.width, 20);
        assert_eq!(result.0.height, 30);
    }

    #[test]
    fn test_parse_kitty_graphics_cell_size() {
        let png_data = create_test_png();
        let base64 = base64::engine::general_purpose::STANDARD.encode(&png_data);

        let payload = format!("a=S,t=0,T=1,w=10,h=5;{base64}");
        let result = parse_kitty_graphics(&payload).unwrap();
        assert_eq!(result.1.cell_size, Some((10, 5)));
    }

    #[test]
    fn test_image_cache_insert_and_get() {
        let mut cache = PaneImageCache::new();
        let image = ParsedImage {
            placement: None,
            pixel_size: (10, 10),
            cell_size: None,
            data: vec![0; 100],
        };
        let id = cache.insert(image);
        let retrieved = cache.get(id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().pixel_size, (10, 10));
    }

    #[test]
    fn test_image_cache_lru_eviction() {
        let mut cache = PaneImageCache::new();
        cache.set_max_images(2);

        let id1 = cache.insert(ParsedImage {
            placement: None,
            pixel_size: (10, 10),
            cell_size: None,
            data: vec![0; 10],
        });
        let id2 = cache.insert(ParsedImage {
            placement: None,
            pixel_size: (20, 20),
            cell_size: None,
            data: vec![1; 10],
        });
        let id3 = cache.insert(ParsedImage {
            placement: None,
            pixel_size: (30, 30),
            cell_size: None,
            data: vec![2; 10],
        });

        // id1 should be evicted (max 2 images)
        assert!(cache.get(id1).is_none());
        assert!(cache.get(id2).is_some());
        assert!(cache.get(id3).is_some());
    }

    #[test]
    fn test_image_cache_size_eviction() {
        let mut cache = PaneImageCache::new();
        cache.set_max_size_bytes(100);

        // 插入 50 字节的图像
        let id1 = cache.insert(ParsedImage {
            placement: None,
            pixel_size: (10, 10),
            cell_size: None,
            data: vec![0; 50],
        });
        // 再插入 60 字节, 总共 110 > 100, 应该淘汰 id1
        let id2 = cache.insert(ParsedImage {
            placement: None,
            pixel_size: (20, 20),
            cell_size: None,
            data: vec![1; 60],
        });

        assert!(cache.get(id1).is_none());
        assert!(cache.get(id2).is_some());
    }

    #[test]
    fn test_image_cache_remove() {
        let mut cache = PaneImageCache::new();
        let image = ParsedImage {
            placement: None,
            pixel_size: (10, 10),
            cell_size: None,
            data: vec![0; 100],
        };
        let id = cache.insert(image);
        assert!(cache.get(id).is_some());

        cache.remove(id);
        assert!(cache.get(id).is_none());
    }

    #[test]
    fn test_image_cache_clear() {
        let mut cache = PaneImageCache::new();
        cache.insert(ParsedImage {
            placement: None,
            pixel_size: (10, 10),
            cell_size: None,
            data: vec![0; 10],
        });
        cache.clear();
        assert_eq!(cache.images.len(), 0);
        assert_eq!(cache.current_size, 0);
    }

    #[test]
    fn test_graphics_protocol_dispatch() {
        let png_data = create_test_png();
        let base64 = base64::engine::general_purpose::STANDARD.encode(&png_data);

        // OSC 1337
        let payload = format!("File=name=test.png;inline=1:{base64}");
        let result = parse_graphics_protocol(GraphicsProtocol::Osci1337, &payload);
        assert!(result.is_some());
        assert_eq!(result.unwrap().protocol, GraphicsProtocol::Osci1337);

        // Kitty Graphics
        let payload = format!("a=S,t=0,T=1;{base64}");
        let result = parse_graphics_protocol(GraphicsProtocol::KittyGraphics, &payload);
        assert!(result.is_some());
        assert_eq!(result.unwrap().protocol, GraphicsProtocol::KittyGraphics);
    }

    /// 创建一个 1x1 白色像素的 PNG 用于测试
    fn create_test_png() -> Vec<u8> {
        let img = image::RgbaImage::new(1, 1);
        let mut png_buf = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut png_buf),
            image::ImageFormat::Png,
        )
        .expect("write test png");
        png_buf
    }
}
