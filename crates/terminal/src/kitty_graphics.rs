//! Kitty graphics protocol and iTerm2 OSC 1337 image display support.
//!
//! §11.2 终端图形协议支持
//!
//! 支持两种图像传输协议:
//! 1. iTerm2 OSC 1337: `ESC ] 1337 ; File=<params> : <base64_data> ST`
//! 2. Kitty graphics (APC): `ESC _ G <control data> ; <base64_payload> ESC \`
//!
//! 参考:
//! - iTerm2: <https://iterm2.com/documentation-images.html>
//! - Kitty:  <https://sw.kovidgoyal.net/kitty/graphics-protocol/>
//!
//! vte 的状态机把 APC 字符串整段丢弃, 且不会把未知的 OSC 号码转交给
//! `Handler`, 所以这两个协议都无法通过 alacritty 的 handler 钩子拿到。
//! [`GraphicsScanner`] 因此直接在 PTY 字节流上做一遍轻量扫描, 只挑出图形
//! 序列, 其余字节原样交给 alacritty 解析。

use std::collections::VecDeque;
use std::sync::Arc;

use base64::Engine as _;
use collections::HashMap;
use gpui::RenderImage;
use image::ImageBuffer;

/// 单条 APC/OSC 序列允许缓冲的最大字节数。超过后整条序列被丢弃, 避免
/// 恶意程序用一条永不终止的序列耗尽内存。
pub const MAX_SEQUENCE_BYTES: usize = 16 * 1024 * 1024;

/// 分块传输累积的 base64 文本上限 (跨 `m=1` 块)。
pub const MAX_TRANSFER_BYTES: usize = 32 * 1024 * 1024;

/// 单张图像解码后允许占用的最大字节数 (BGRA)。
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// 单边像素上限, 在分配像素缓冲之前先挡掉明显不合理的尺寸。
pub const MAX_IMAGE_DIMENSION: u32 = 16384;

const DEFAULT_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_CACHE_MAX_IMAGES: usize = 32;

// ──────────────────────────────────────────────
// 错误
// ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphicsError {
    #[error("malformed graphics control data")]
    Malformed,
    #[error("invalid value for key `{key}`")]
    InvalidValue { key: char },
    #[error("payload is not valid base64")]
    InvalidBase64,
    #[error("transfer exceeds the {limit} byte limit")]
    TooLarge { limit: usize },
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("image data could not be decoded")]
    Undecodable,
    #[error("pixel dimensions are missing or do not match the payload size")]
    DimensionMismatch,
}

impl GraphicsError {
    /// Kitty 响应里使用的错误码前缀。
    fn kitty_code(&self) -> &'static str {
        match self {
            GraphicsError::Malformed | GraphicsError::InvalidValue { .. } => "EINVAL",
            GraphicsError::InvalidBase64 | GraphicsError::Undecodable => "EBADIMAGE",
            GraphicsError::TooLarge { .. } => "ENOSPC",
            GraphicsError::Unsupported(_) => "ENOTSUPPORTED",
            GraphicsError::DimensionMismatch => "EINVAL",
        }
    }
}

// ──────────────────────────────────────────────
// Kitty 控制数据
// ──────────────────────────────────────────────

/// `a=` 操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyAction {
    /// `a=t` — 只传输图像数据, 不显示。
    Transmit,
    /// `a=T` — 传输并立即在光标处显示。
    TransmitAndDisplay,
    /// `a=p` — 显示一张已经传输过的图像。
    Put,
    /// `a=d` — 删除图像或其放置。
    Delete,
    /// `a=q` — 查询终端是否支持某种传输方式。
    Query,
    /// `a=f` — 传输动画帧。
    TransmitFrame,
    /// `a=a` — 控制动画播放。
    ControlAnimation,
    /// `a=c` — 合成动画帧。
    ComposeFrames,
}

/// `f=` 像素格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyFormat {
    /// `f=24`
    Rgb,
    /// `f=32`
    Rgba,
    /// `f=100`
    Png,
}

/// `t=` 传输介质。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMedium {
    /// `t=d` — 数据直接内联在转义序列里。
    Direct,
    /// `t=f` — payload 是一个文件路径。
    File,
    /// `t=t` — payload 是一个临时文件路径, 读完后需要删除。
    TemporaryFile,
    /// `t=s` — payload 是一个 POSIX 共享内存对象名。
    SharedMemory,
}

/// `o=` 压缩方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    /// `o=z`
    Zlib,
}

/// 一条 kitty graphics 控制指令。
///
/// 字段名沿用协议语义而不是键名, 键名写在注释里以便对照规范。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCommand {
    /// `a`
    pub action: KittyAction,
    /// `f`
    pub format: KittyFormat,
    /// `t`
    pub medium: TransferMedium,
    /// `o`
    pub compression: Compression,
    /// `m` — 为真表示后面还有分块。
    pub more_chunks: bool,
    /// `i`
    pub image_id: Option<u32>,
    /// `I`
    pub image_number: Option<u32>,
    /// `p`
    pub placement_id: Option<u32>,
    /// `s` — 传输数据的像素宽度 (仅 `f=24`/`f=32` 需要)。
    pub transmit_width: Option<u32>,
    /// `v` — 传输数据的像素高度。
    pub transmit_height: Option<u32>,
    /// `S` — 从文件读取的字节数。
    pub transmit_size: Option<u32>,
    /// `O` — 文件读取偏移。
    pub transmit_offset: Option<u32>,
    /// `x`
    pub source_x: u32,
    /// `y`
    pub source_y: u32,
    /// `w` — 0 表示到右边缘。
    pub source_width: u32,
    /// `h` — 0 表示到下边缘。
    pub source_height: u32,
    /// `X`
    pub cell_offset_x: u32,
    /// `Y`
    pub cell_offset_y: u32,
    /// `c` — 显示占用的列数。
    pub columns: Option<u32>,
    /// `r` — 显示占用的行数。
    pub rows: Option<u32>,
    /// `z`
    pub z_index: i32,
    /// `C` — 非零表示显示后不移动光标。
    pub cursor_movement: u32,
    /// `q` — 1 抑制成功响应, 2 连错误响应也抑制。
    pub quiet: u8,
    /// `d` — 删除目标。
    pub delete_target: char,
    /// `U`
    pub unicode_placeholder: bool,
}

impl Default for KittyCommand {
    fn default() -> Self {
        Self {
            action: KittyAction::Transmit,
            format: KittyFormat::Rgba,
            medium: TransferMedium::Direct,
            compression: Compression::None,
            more_chunks: false,
            image_id: None,
            image_number: None,
            placement_id: None,
            transmit_width: None,
            transmit_height: None,
            transmit_size: None,
            transmit_offset: None,
            source_x: 0,
            source_y: 0,
            source_width: 0,
            source_height: 0,
            cell_offset_x: 0,
            cell_offset_y: 0,
            columns: None,
            rows: None,
            z_index: 0,
            cursor_movement: 0,
            quiet: 0,
            delete_target: 'a',
            unicode_placeholder: false,
        }
    }
}

impl KittyCommand {
    /// 响应只在请求带上 `i=` 或 `I=` 时才发送 (协议要求)。
    fn wants_response(&self) -> bool {
        self.image_id.is_some() || self.image_number.is_some()
    }

    fn response_prefix(&self) -> String {
        let mut prefix = String::new();
        if let Some(image_id) = self.image_id {
            prefix.push_str(&format!("i={image_id}"));
        }
        if let Some(image_number) = self.image_number {
            if !prefix.is_empty() {
                prefix.push(',');
            }
            prefix.push_str(&format!("I={image_number}"));
        }
        if let Some(placement_id) = self.placement_id {
            if !prefix.is_empty() {
                prefix.push(',');
            }
            prefix.push_str(&format!("p={placement_id}"));
        }
        prefix
    }

    fn success_response(&self) -> Option<Vec<u8>> {
        if !self.wants_response() || self.quiet >= 1 {
            return None;
        }
        Some(format!("\x1b_G{};OK\x1b\\", self.response_prefix()).into_bytes())
    }

    fn error_response(&self, error: &GraphicsError) -> Option<Vec<u8>> {
        if !self.wants_response() || self.quiet >= 2 {
            return None;
        }
        Some(
            format!(
                "\x1b_G{};{}:{}\x1b\\",
                self.response_prefix(),
                error.kitty_code(),
                error
            )
            .into_bytes(),
        )
    }
}

fn parse_u32(key: char, value: &str) -> Result<u32, GraphicsError> {
    value
        .parse::<u32>()
        .map_err(|_| GraphicsError::InvalidValue { key })
}

/// 解析 kitty graphics 的控制数据 (APC payload 里第一个 `;` 之前的部分)。
///
/// 控制数据是逗号分隔的 `key=value` 列表, key 恒为单个字母。规范要求
/// 未知的 key 被忽略而不是报错, 这样新版本 kitty 加字段时旧终端不会炸。
pub fn parse_kitty_command(control: &str) -> Result<KittyCommand, GraphicsError> {
    let mut command = KittyCommand::default();

    for entry in control.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((key, value)) = entry.split_once('=') else {
            return Err(GraphicsError::Malformed);
        };
        let mut key_chars = key.chars();
        let (Some(key), None) = (key_chars.next(), key_chars.next()) else {
            return Err(GraphicsError::Malformed);
        };

        match key {
            'a' => {
                command.action = match value {
                    "t" => KittyAction::Transmit,
                    "T" => KittyAction::TransmitAndDisplay,
                    "p" => KittyAction::Put,
                    "d" => KittyAction::Delete,
                    "q" => KittyAction::Query,
                    "f" => KittyAction::TransmitFrame,
                    "a" => KittyAction::ControlAnimation,
                    "c" => KittyAction::ComposeFrames,
                    _ => return Err(GraphicsError::InvalidValue { key }),
                };
            }
            'f' => {
                command.format = match value {
                    "24" => KittyFormat::Rgb,
                    "32" => KittyFormat::Rgba,
                    "100" => KittyFormat::Png,
                    _ => return Err(GraphicsError::InvalidValue { key }),
                };
            }
            't' => {
                command.medium = match value {
                    "d" => TransferMedium::Direct,
                    "f" => TransferMedium::File,
                    "t" => TransferMedium::TemporaryFile,
                    "s" => TransferMedium::SharedMemory,
                    _ => return Err(GraphicsError::InvalidValue { key }),
                };
            }
            'o' => {
                command.compression = match value {
                    "z" => Compression::Zlib,
                    _ => return Err(GraphicsError::InvalidValue { key }),
                };
            }
            'm' => command.more_chunks = parse_u32(key, value)? != 0,
            'i' => command.image_id = Some(parse_u32(key, value)?),
            'I' => command.image_number = Some(parse_u32(key, value)?),
            'p' => command.placement_id = Some(parse_u32(key, value)?),
            's' => command.transmit_width = Some(parse_u32(key, value)?),
            'v' => command.transmit_height = Some(parse_u32(key, value)?),
            'S' => command.transmit_size = Some(parse_u32(key, value)?),
            'O' => command.transmit_offset = Some(parse_u32(key, value)?),
            'x' => command.source_x = parse_u32(key, value)?,
            'y' => command.source_y = parse_u32(key, value)?,
            'w' => command.source_width = parse_u32(key, value)?,
            'h' => command.source_height = parse_u32(key, value)?,
            'X' => command.cell_offset_x = parse_u32(key, value)?,
            'Y' => command.cell_offset_y = parse_u32(key, value)?,
            'c' => command.columns = Some(parse_u32(key, value)?),
            'r' => command.rows = Some(parse_u32(key, value)?),
            'z' => {
                command.z_index = value
                    .parse::<i32>()
                    .map_err(|_| GraphicsError::InvalidValue { key })?
            }
            'C' => command.cursor_movement = parse_u32(key, value)?,
            'q' => {
                command.quiet = value
                    .parse::<u8>()
                    .map_err(|_| GraphicsError::InvalidValue { key })?
            }
            'd' => {
                let mut value_chars = value.chars();
                let (Some(target), None) = (value_chars.next(), value_chars.next()) else {
                    return Err(GraphicsError::InvalidValue { key });
                };
                command.delete_target = target;
            }
            'U' => command.unicode_placeholder = parse_u32(key, value)? != 0,
            _ => {}
        }
    }

    Ok(command)
}

// ──────────────────────────────────────────────
// 解码后的图像
// ──────────────────────────────────────────────

/// 已经解码成 GPUI 可直接绘制格式的图像。
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub pixel_size: (u32, u32),
    /// 解码后占用的字节数, 用于缓存记账。
    pub byte_size: usize,
    pub render_image: Arc<RenderImage>,
}

/// iTerm2 的 `width=`/`height=` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDimension {
    /// 由图像自身像素尺寸决定。
    Auto,
    /// 单元格数。
    Cells(u32),
    /// 像素数。
    Pixels(u32),
    /// 相对终端宽/高的百分比。
    Percent(u32),
}

/// 请求把一张图像放到光标处。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRequest {
    /// kitty 的 `i=`。`None` 表示"刚刚传输的那张"。
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
    pub columns: ImageDimension,
    pub rows: ImageDimension,
    pub z_index: i32,
    /// 显示后是否把光标移到图像下方。
    pub move_cursor: bool,
}

/// 删除范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteScope {
    /// `d=a` / `d=A`
    All,
    /// `d=i` / `d=I`
    ImageId(u32),
    /// `d=n` / `d=N`
    ImageNumber(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteRequest {
    pub scope: DeleteScope,
    /// 大写的删除目标同时释放图像数据, 小写只移除放置。
    pub free_data: bool,
}

/// 扫描器交给 [`crate::Terminal`] 执行的动作。
#[derive(Debug, Clone)]
pub enum GraphicsEvent {
    /// 把一张解码好的图像放进 pane 缓存。
    Transmit {
        image_id: Option<u32>,
        image_number: Option<u32>,
        image: DecodedImage,
    },
    Place(PlacementRequest),
    Delete(DeleteRequest),
    /// 需要原样写回 PTY 的协议响应。
    Respond(Vec<u8>),
}

// ──────────────────────────────────────────────
// 图像解码
// ──────────────────────────────────────────────

fn render_image_from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<Arc<RenderImage>> {
    // RenderImage 期望 BGRA 顺序。
    let mut bgra = rgba;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = ImageBuffer::from_vec(width, height, bgra)?;
    Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

fn check_dimensions(width: u32, height: u32) -> Result<usize, GraphicsError> {
    if width == 0 || height == 0 || width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(GraphicsError::DimensionMismatch);
    }
    let byte_size = (width as u64)
        .checked_mul(height as u64)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(GraphicsError::DimensionMismatch)?;
    if byte_size > MAX_IMAGE_BYTES as u64 {
        return Err(GraphicsError::TooLarge {
            limit: MAX_IMAGE_BYTES,
        });
    }
    Ok(byte_size as usize)
}

/// 从任意已知容器格式 (PNG/JPEG/...) 解码。
pub fn decode_encoded_image(data: &[u8]) -> Result<DecodedImage, GraphicsError> {
    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| GraphicsError::Undecodable)?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| GraphicsError::Undecodable)?;
    let byte_size = check_dimensions(width, height)?;

    let decoded = image::load_from_memory(data).map_err(|_| GraphicsError::Undecodable)?;
    let rgba = decoded.into_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let render_image =
        render_image_from_rgba(width, height, rgba.into_raw()).ok_or(GraphicsError::Undecodable)?;

    Ok(DecodedImage {
        pixel_size: (width, height),
        byte_size,
        render_image,
    })
}

/// 从裸像素数据解码 (`f=24` / `f=32`)。
fn decode_raw_pixels(
    data: &[u8],
    width: u32,
    height: u32,
    format: KittyFormat,
) -> Result<DecodedImage, GraphicsError> {
    let byte_size = check_dimensions(width, height)?;
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or(GraphicsError::DimensionMismatch)?;

    let rgba = match format {
        KittyFormat::Rgba => {
            if data.len() < pixel_count * 4 {
                return Err(GraphicsError::DimensionMismatch);
            }
            data[..pixel_count * 4].to_vec()
        }
        KittyFormat::Rgb => {
            if data.len() < pixel_count * 3 {
                return Err(GraphicsError::DimensionMismatch);
            }
            let mut rgba = Vec::with_capacity(pixel_count * 4);
            for pixel in data[..pixel_count * 3].chunks_exact(3) {
                rgba.extend_from_slice(pixel);
                rgba.push(0xFF);
            }
            rgba
        }
        KittyFormat::Png => return Err(GraphicsError::Undecodable),
    };

    let render_image =
        render_image_from_rgba(width, height, rgba).ok_or(GraphicsError::Undecodable)?;
    Ok(DecodedImage {
        pixel_size: (width, height),
        byte_size,
        render_image,
    })
}

fn decode_base64(encoded: &[u8]) -> Result<Vec<u8>, GraphicsError> {
    // 分块传输允许在任意位置切开 base64 文本, 拼接后可能带有换行/空格。
    let filtered: Vec<u8> = encoded
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(&filtered)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&filtered))
        .map_err(|_| GraphicsError::InvalidBase64)
}

// ──────────────────────────────────────────────
// 分块传输重组
// ──────────────────────────────────────────────

/// kitty 规范规定同一时刻只能有一个分块传输在进行中, 所以这里只保留一个
/// 待完成的槽位。新的分块传输开始时旧的会被丢弃。
#[derive(Debug)]
struct PendingKittyTransfer {
    command: KittyCommand,
    payload: Vec<u8>,
}

#[derive(Debug, Default)]
struct PendingItermTransfer {
    parameters: ItermParameters,
    payload: Vec<u8>,
}

/// 把解析出来的协议序列变成 [`GraphicsEvent`]。
#[derive(Debug, Default)]
pub struct GraphicsAssembler {
    pending_kitty: Option<PendingKittyTransfer>,
    pending_iterm: Option<PendingItermTransfer>,
}

impl GraphicsAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一条完整的 kitty APC payload (已去掉 `ESC _ G` 和 `ESC \`)。
    pub fn accept_kitty(&mut self, body: &[u8], events: &mut Vec<GraphicsEvent>) {
        let (control, payload) = match body.iter().position(|&byte| byte == b';') {
            Some(index) => (&body[..index], &body[index + 1..]),
            None => (body, &body[body.len()..]),
        };

        let Ok(control) = std::str::from_utf8(control) else {
            log::debug!("kitty graphics: control data is not valid utf-8");
            return;
        };

        let command = match parse_kitty_command(control) {
            Ok(command) => command,
            Err(error) => {
                log::debug!("kitty graphics: {error}");
                return;
            }
        };

        match self.assemble_kitty(command, payload) {
            Ok(None) => {}
            Ok(Some(mut produced)) => events.append(&mut produced),
            Err((command, error)) => {
                log::debug!("kitty graphics: {error}");
                if let Some(response) = command.error_response(&error) {
                    events.push(GraphicsEvent::Respond(response));
                }
            }
        }
    }

    fn assemble_kitty(
        &mut self,
        command: KittyCommand,
        payload: &[u8],
    ) -> Result<Option<Vec<GraphicsEvent>>, (KittyCommand, GraphicsError)> {
        // 后续分块只带 `m` 和 `q`, 图像标识在第一块上, 所以要先把分块拼完
        // 再判断动作。
        if let Some(mut pending) = self.pending_kitty.take() {
            if pending.payload.len() + payload.len() > MAX_TRANSFER_BYTES {
                return Err((
                    pending.command,
                    GraphicsError::TooLarge {
                        limit: MAX_TRANSFER_BYTES,
                    },
                ));
            }
            pending.payload.extend_from_slice(payload);
            if command.more_chunks {
                self.pending_kitty = Some(pending);
                return Ok(None);
            }
            return self
                .execute_kitty(pending.command, &pending.payload)
                .map(Some);
        }

        if command.more_chunks {
            if payload.len() > MAX_TRANSFER_BYTES {
                return Err((
                    command,
                    GraphicsError::TooLarge {
                        limit: MAX_TRANSFER_BYTES,
                    },
                ));
            }
            self.pending_kitty = Some(PendingKittyTransfer {
                command,
                payload: payload.to_vec(),
            });
            return Ok(None);
        }

        self.execute_kitty(command, payload).map(Some)
    }

    fn execute_kitty(
        &mut self,
        command: KittyCommand,
        payload: &[u8],
    ) -> Result<Vec<GraphicsEvent>, (KittyCommand, GraphicsError)> {
        let mut events = Vec::new();

        match command.action {
            KittyAction::Delete => {
                let free_data = command.delete_target.is_ascii_uppercase();
                let scope = match command.delete_target.to_ascii_lowercase() {
                    'a' => Some(DeleteScope::All),
                    'i' => command.image_id.map(DeleteScope::ImageId),
                    'n' => command.image_number.map(DeleteScope::ImageNumber),
                    other => {
                        // 位置相关的删除 (`d=c`/`d=p`/`d=x`...) 需要逐格记录
                        // 放置信息, 当前的覆盖层模型没有这份数据, 与其误删
                        // 不如不动。
                        log::debug!("kitty graphics: unsupported delete target `{other}`");
                        None
                    }
                };
                if let Some(scope) = scope {
                    events.push(GraphicsEvent::Delete(DeleteRequest { scope, free_data }));
                }
                if let Some(response) = command.success_response() {
                    events.push(GraphicsEvent::Respond(response));
                }
                return Ok(events);
            }
            KittyAction::Put => {
                events.push(GraphicsEvent::Place(placement_from_kitty(&command)));
                if let Some(response) = command.success_response() {
                    events.push(GraphicsEvent::Respond(response));
                }
                return Ok(events);
            }
            KittyAction::TransmitFrame
            | KittyAction::ControlAnimation
            | KittyAction::ComposeFrames => {
                return Err((command, GraphicsError::Unsupported("animation")));
            }
            KittyAction::Transmit | KittyAction::TransmitAndDisplay | KittyAction::Query => {}
        }

        if command.compression != Compression::None {
            return Err((command, GraphicsError::Unsupported("zlib compression")));
        }
        if command.medium != TransferMedium::Direct {
            // 文件与共享内存传输会让终端按 PTY 里的字符串去读本地路径,
            // 这是一条从子进程到宿主文件系统的旁路, 暂不开放。
            return Err((
                command,
                GraphicsError::Unsupported("non-direct transmission medium"),
            ));
        }

        let data = match decode_base64(payload) {
            Ok(data) => data,
            Err(error) => return Err((command, error)),
        };

        let image = match command.format {
            KittyFormat::Png => decode_encoded_image(&data),
            KittyFormat::Rgb | KittyFormat::Rgba => {
                match (command.transmit_width, command.transmit_height) {
                    (Some(width), Some(height)) => {
                        decode_raw_pixels(&data, width, height, command.format)
                    }
                    _ => Err(GraphicsError::DimensionMismatch),
                }
            }
        };
        let image = match image {
            Ok(image) => image,
            Err(error) => return Err((command, error)),
        };

        if command.action == KittyAction::Query {
            // 查询只确认"这样传输是可行的", 不留下任何图像。
            if let Some(response) = command.success_response() {
                events.push(GraphicsEvent::Respond(response));
            }
            return Ok(events);
        }

        events.push(GraphicsEvent::Transmit {
            image_id: command.image_id,
            image_number: command.image_number,
            image,
        });
        if command.action == KittyAction::TransmitAndDisplay {
            events.push(GraphicsEvent::Place(placement_from_kitty(&command)));
        }
        if let Some(response) = command.success_response() {
            events.push(GraphicsEvent::Respond(response));
        }
        Ok(events)
    }

    /// 处理一条完整的 OSC 1337 payload (已去掉 `ESC ] 1337 ;` 和终止符)。
    pub fn accept_iterm(&mut self, body: &[u8], events: &mut Vec<GraphicsEvent>) {
        if let Some(rest) = strip_prefix_ignore_ascii_case(body, b"File=") {
            let Some((parameters, payload)) = split_iterm_body(rest) else {
                return;
            };
            self.pending_iterm = None;
            self.finish_iterm(parameters, payload, events);
            return;
        }

        if let Some(rest) = strip_prefix_ignore_ascii_case(body, b"MultipartFile=") {
            let parameters = parse_iterm_parameters(rest);
            self.pending_iterm = Some(PendingItermTransfer {
                parameters,
                payload: Vec::new(),
            });
            return;
        }

        if let Some(rest) = strip_prefix_ignore_ascii_case(body, b"FilePart=") {
            let Some(pending) = &mut self.pending_iterm else {
                return;
            };
            if pending.payload.len() + rest.len() > MAX_TRANSFER_BYTES {
                log::debug!("iterm graphics: multipart transfer exceeded the size limit");
                self.pending_iterm = None;
                return;
            }
            pending.payload.extend_from_slice(rest);
            return;
        }

        if body.eq_ignore_ascii_case(b"FileEnd") {
            let Some(pending) = self.pending_iterm.take() else {
                return;
            };
            self.finish_iterm(pending.parameters, &pending.payload, events);
        }
    }

    fn finish_iterm(
        &mut self,
        parameters: ItermParameters,
        payload: &[u8],
        events: &mut Vec<GraphicsEvent>,
    ) {
        if !parameters.inline {
            return;
        }
        let data = match decode_base64(payload) {
            Ok(data) => data,
            Err(error) => {
                log::debug!("iterm graphics: {error}");
                return;
            }
        };
        let image = match decode_encoded_image(&data) {
            Ok(image) => image,
            Err(error) => {
                log::debug!("iterm graphics: {error}");
                return;
            }
        };
        events.push(GraphicsEvent::Transmit {
            image_id: None,
            image_number: None,
            image,
        });
        events.push(GraphicsEvent::Place(PlacementRequest {
            image_id: None,
            placement_id: None,
            columns: parameters.width,
            rows: parameters.height,
            z_index: 0,
            move_cursor: !parameters.do_not_move_cursor,
        }));
    }
}

fn placement_from_kitty(command: &KittyCommand) -> PlacementRequest {
    PlacementRequest {
        image_id: command.image_id,
        placement_id: command.placement_id,
        columns: command
            .columns
            .filter(|columns| *columns > 0)
            .map_or(ImageDimension::Auto, ImageDimension::Cells),
        rows: command
            .rows
            .filter(|rows| *rows > 0)
            .map_or(ImageDimension::Auto, ImageDimension::Cells),
        z_index: command.z_index,
        move_cursor: command.cursor_movement == 0,
    }
}

// ──────────────────────────────────────────────
// iTerm2 OSC 1337 参数
// ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItermParameters {
    pub name: Option<String>,
    pub size: Option<u64>,
    pub width: ImageDimension,
    pub height: ImageDimension,
    pub preserve_aspect_ratio: bool,
    pub inline: bool,
    pub do_not_move_cursor: bool,
}

impl Default for ItermParameters {
    fn default() -> Self {
        Self {
            name: None,
            size: None,
            width: ImageDimension::Auto,
            height: ImageDimension::Auto,
            preserve_aspect_ratio: true,
            inline: false,
            do_not_move_cursor: false,
        }
    }
}

fn parse_iterm_dimension(value: &str) -> ImageDimension {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") || value.is_empty() {
        return ImageDimension::Auto;
    }
    if let Some(pixels) = value
        .strip_suffix("px")
        .or_else(|| value.strip_suffix("Px"))
    {
        return pixels
            .parse::<u32>()
            .map_or(ImageDimension::Auto, ImageDimension::Pixels);
    }
    if let Some(percent) = value.strip_suffix('%') {
        return percent
            .parse::<u32>()
            .map_or(ImageDimension::Auto, ImageDimension::Percent);
    }
    value
        .parse::<u32>()
        .map_or(ImageDimension::Auto, ImageDimension::Cells)
}

pub fn parse_iterm_parameters(parameters: &[u8]) -> ItermParameters {
    let mut result = ItermParameters::default();
    let Ok(parameters) = std::str::from_utf8(parameters) else {
        return result;
    };

    for entry in parameters.split(';') {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("name") {
            result.name = base64::engine::general_purpose::STANDARD
                .decode(value)
                .ok()
                .and_then(|decoded| String::from_utf8(decoded).ok());
        } else if key.eq_ignore_ascii_case("size") {
            result.size = value.parse().ok();
        } else if key.eq_ignore_ascii_case("width") {
            result.width = parse_iterm_dimension(value);
        } else if key.eq_ignore_ascii_case("height") {
            result.height = parse_iterm_dimension(value);
        } else if key.eq_ignore_ascii_case("preserveAspectRatio") {
            result.preserve_aspect_ratio = value != "0";
        } else if key.eq_ignore_ascii_case("inline") {
            result.inline = value != "0";
        } else if key.eq_ignore_ascii_case("doNotMoveCursor") {
            result.do_not_move_cursor = value != "0";
        }
    }

    result
}

fn strip_prefix_ignore_ascii_case<'a>(data: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if data.len() >= prefix.len() && data[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&data[prefix.len()..])
    } else {
        None
    }
}

/// iTerm2 用 `:` 把参数和 base64 数据分开, 但 base64 字母表里没有 `:`,
/// 所以第一个 `:` 就是分隔符。
fn split_iterm_body(body: &[u8]) -> Option<(ItermParameters, &[u8])> {
    let separator = body.iter().position(|&byte| byte == b':')?;
    Some((
        parse_iterm_parameters(&body[..separator]),
        &body[separator + 1..],
    ))
}

// ──────────────────────────────────────────────
// PTY 字节流扫描器
// ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Ground,
    /// 刚读到 `ESC`。
    Escape,
    /// `ESC _ G` 之后的 kitty graphics 正文。
    KittyBody,
    /// kitty 正文里读到 `ESC`, 等待判断是不是 `ESC \`。
    KittyBodyEscape,
    /// `ESC ]` 之后的 OSC 正文, 尚未确定是不是 1337。
    OscBody,
    /// OSC 正文里读到 `ESC`。
    OscBodyEscape,
    /// 与图形无关的字符串序列 (其它 OSC/DCS/SOS/PM/APC), 只需跳到终止符。
    IgnoredString,
    /// 被忽略的字符串里读到 `ESC`。
    IgnoredStringEscape,
}

fn string_escape_state(state: ScanState) -> ScanState {
    match state {
        ScanState::KittyBody => ScanState::KittyBodyEscape,
        ScanState::OscBody => ScanState::OscBodyEscape,
        _ => ScanState::IgnoredStringEscape,
    }
}

fn string_body_state(state: ScanState) -> ScanState {
    match state {
        ScanState::KittyBodyEscape => ScanState::KittyBody,
        ScanState::OscBodyEscape => ScanState::OscBody,
        _ => ScanState::IgnoredString,
    }
}

/// 在 PTY 字节流上就地挑出图形协议序列。
///
/// 扫描器只做观察, 不改写字节流: kitty 的 APC 和 iTerm2 的 OSC 1337 都会被
/// vte 当作未知序列丢弃, 所以让原始字节继续流向 alacritty 是安全的。
#[derive(Debug)]
pub struct GraphicsScanner {
    state: ScanState,
    buffer: Vec<u8>,
    /// 当前序列已经超过 [`MAX_SEQUENCE_BYTES`], 只等终止符然后整条丢弃。
    overflowed: bool,
    assembler: GraphicsAssembler,
}

impl Default for GraphicsScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphicsScanner {
    pub fn new() -> Self {
        Self {
            state: ScanState::Ground,
            buffer: Vec::new(),
            overflowed: false,
            assembler: GraphicsAssembler::new(),
        }
    }

    /// 喂入一段 PTY 字节, 返回本次产生的动作。
    ///
    /// 序列可以横跨多次调用: 解析状态和已收集的正文都留在扫描器里。
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<GraphicsEvent> {
        let mut events = Vec::new();
        let mut index = 0;

        while index < bytes.len() {
            match self.state {
                ScanState::Ground => {
                    // 绝大多数字节都是普通文本, 直接跳到下一个 ESC。
                    match bytes[index..].iter().position(|&byte| byte == 0x1B) {
                        Some(offset) => {
                            index += offset + 1;
                            self.state = ScanState::Escape;
                        }
                        None => break,
                    }
                }
                ScanState::KittyBody | ScanState::OscBody | ScanState::IgnoredString => {
                    let terminator = bytes[index..]
                        .iter()
                        .position(|&byte| byte == 0x1B || byte == 0x07);
                    let end = terminator.map_or(bytes.len(), |offset| index + offset);
                    self.push_slice(&bytes[index..end]);
                    self.discard_if_prefix_mismatches();
                    index = end;

                    if terminator.is_none() {
                        break;
                    }
                    let byte = bytes[index];
                    index += 1;
                    if byte == 0x07 {
                        // BEL 是 OSC 的合法终止符; kitty 的 APC 只用 `ESC \`,
                        // 但 base64 里不会出现 BEL, 所以一并接受更宽容。
                        self.finish_sequence(&mut events);
                    } else {
                        self.state = string_escape_state(self.state);
                    }
                }
                ScanState::Escape => {
                    let byte = bytes[index];
                    index += 1;
                    self.buffer.clear();
                    self.overflowed = false;
                    self.state = match byte {
                        b'_' => ScanState::KittyBody,
                        b']' => ScanState::OscBody,
                        // DCS / SOS / PM 也是字符串序列, 必须跳到终止符,
                        // 否则里面的 `ESC ]` 会被误认成 OSC 起点。
                        b'P' | b'X' | b'^' => ScanState::IgnoredString,
                        0x1B => ScanState::Escape,
                        _ => ScanState::Ground,
                    };
                }
                ScanState::KittyBodyEscape
                | ScanState::OscBodyEscape
                | ScanState::IgnoredStringEscape => {
                    let byte = bytes[index];
                    index += 1;
                    if byte == b'\\' {
                        self.finish_sequence(&mut events);
                    } else {
                        // 落单的 ESC: 它和后面这个字节都属于正文, 放回去继续收集。
                        self.state = string_body_state(self.state);
                        self.push_byte(0x1B);
                        if byte == 0x1B {
                            self.state = string_escape_state(self.state);
                        } else {
                            self.push_byte(byte);
                        }
                    }
                }
            }
        }

        events
    }

    /// 一旦确定当前字符串序列不是图形协议就停止缓冲, 免得 OSC 52 之类的
    /// 大 payload 白白占内存。
    fn discard_if_prefix_mismatches(&mut self) {
        let matches = match self.state {
            ScanState::KittyBody => self.buffer.first().is_none_or(|byte| *byte == b'G'),
            ScanState::OscBody => {
                const PREFIX: &[u8] = b"1337;";
                let length = self.buffer.len().min(PREFIX.len());
                self.buffer[..length] == PREFIX[..length]
            }
            _ => true,
        };
        if !matches {
            self.state = ScanState::IgnoredString;
            self.buffer.clear();
            self.buffer.shrink_to_fit();
        }
    }

    fn push_byte(&mut self, byte: u8) {
        self.push_slice(&[byte]);
    }

    fn push_slice(&mut self, bytes: &[u8]) {
        if self.overflowed || self.state == ScanState::IgnoredString {
            return;
        }
        if self.buffer.len() + bytes.len() > MAX_SEQUENCE_BYTES {
            log::debug!("terminal graphics: dropping sequence over {MAX_SEQUENCE_BYTES} bytes");
            self.overflowed = true;
            self.buffer.clear();
            self.buffer.shrink_to_fit();
            return;
        }
        self.buffer.extend_from_slice(bytes);
    }

    fn finish_sequence(&mut self, events: &mut Vec<GraphicsEvent>) {
        let state = self.state;
        self.state = ScanState::Ground;
        let overflowed = self.overflowed;
        self.overflowed = false;
        let buffer = std::mem::take(&mut self.buffer);

        if overflowed {
            return;
        }

        match state {
            ScanState::KittyBody | ScanState::KittyBodyEscape => {
                if let Some(body) = buffer.strip_prefix(b"G") {
                    self.assembler.accept_kitty(body, events);
                }
            }
            ScanState::OscBody | ScanState::OscBodyEscape => {
                if let Some(body) = buffer.strip_prefix(b"1337;") {
                    self.assembler.accept_iterm(body, events);
                }
            }
            _ => {}
        }
    }
}

// ──────────────────────────────────────────────
// 每 pane 图像缓存
// ──────────────────────────────────────────────

/// 缓存中的图像 ID (终端内部分配, 与协议里的 `i=` 无关)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(pub u64);

#[derive(Debug, Clone)]
pub struct CachedImage {
    pub image: DecodedImage,
    /// 协议里的 `i=`。
    pub client_id: Option<u32>,
    /// 协议里的 `I=`。
    pub image_number: Option<u32>,
}

/// 每 pane 的图像缓存, 按字节数和张数双重限额做 LRU 淘汰。
#[derive(Debug)]
pub struct PaneImageCache {
    images: HashMap<ImageId, CachedImage>,
    by_client_id: HashMap<u32, ImageId>,
    by_image_number: HashMap<u32, ImageId>,
    usage_order: VecDeque<ImageId>,
    max_bytes: usize,
    max_images: usize,
    next_id: u64,
    current_bytes: usize,
    last_transmitted: Option<ImageId>,
    /// 已经从缓存里移除、但 GPUI 图集里可能还留着纹理的图像。
    dropped: Vec<Arc<RenderImage>>,
}

impl PaneImageCache {
    pub fn new() -> Self {
        Self {
            images: HashMap::default(),
            by_client_id: HashMap::default(),
            by_image_number: HashMap::default(),
            usage_order: VecDeque::new(),
            max_bytes: DEFAULT_CACHE_MAX_BYTES,
            max_images: DEFAULT_CACHE_MAX_IMAGES,
            next_id: 0,
            current_bytes: 0,
            last_transmitted: None,
            dropped: Vec::new(),
        }
    }

    /// 取走待释放的图像, 调用方负责 `App::drop_image`。
    pub fn take_dropped_images(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.dropped)
    }

    pub fn set_max_bytes(&mut self, bytes: usize) {
        self.max_bytes = bytes;
        self.evict_if_needed();
    }

    pub fn set_max_images(&mut self, count: usize) {
        self.max_images = count;
        self.evict_if_needed();
    }

    /// 插入一张图像。同一个 `client_id` 再次传输会替换旧的那张。
    pub fn insert(
        &mut self,
        image: DecodedImage,
        client_id: Option<u32>,
        image_number: Option<u32>,
    ) -> ImageId {
        if let Some(client_id) = client_id
            && let Some(previous) = self.by_client_id.get(&client_id).copied()
        {
            self.remove(previous);
        }

        let id = ImageId(self.next_id);
        self.next_id += 1;
        self.current_bytes += image.byte_size;
        if let Some(client_id) = client_id {
            self.by_client_id.insert(client_id, id);
        }
        if let Some(image_number) = image_number {
            self.by_image_number.insert(image_number, id);
        }
        self.images.insert(
            id,
            CachedImage {
                image,
                client_id,
                image_number,
            },
        );
        self.usage_order.push_front(id);
        self.last_transmitted = Some(id);
        self.evict_if_needed();
        id
    }

    pub fn get(&self, id: ImageId) -> Option<&CachedImage> {
        self.images.get(&id)
    }

    pub fn resolve_client_id(&self, client_id: u32) -> Option<ImageId> {
        self.by_client_id.get(&client_id).copied()
    }

    pub fn resolve_image_number(&self, image_number: u32) -> Option<ImageId> {
        self.by_image_number.get(&image_number).copied()
    }

    pub fn last_transmitted(&self) -> Option<ImageId> {
        self.last_transmitted
    }

    /// 标记一次访问, 让图像在 LRU 队列里回到最前面。
    pub fn touch(&mut self, id: ImageId) {
        if self.images.contains_key(&id) {
            self.usage_order.retain(|entry| *entry != id);
            self.usage_order.push_front(id);
        }
    }

    pub fn remove(&mut self, id: ImageId) -> Option<CachedImage> {
        let removed = self.images.remove(&id)?;
        self.current_bytes = self.current_bytes.saturating_sub(removed.image.byte_size);
        self.dropped.push(removed.image.render_image.clone());
        self.usage_order.retain(|entry| *entry != id);
        if let Some(client_id) = removed.client_id {
            self.by_client_id.remove(&client_id);
        }
        if let Some(image_number) = removed.image_number {
            self.by_image_number.remove(&image_number);
        }
        if self.last_transmitted == Some(id) {
            self.last_transmitted = None;
        }
        Some(removed)
    }

    pub fn clear(&mut self) {
        self.dropped.extend(
            self.images
                .values()
                .map(|cached| cached.image.render_image.clone()),
        );
        self.images.clear();
        self.by_client_id.clear();
        self.by_image_number.clear();
        self.usage_order.clear();
        self.current_bytes = 0;
        self.last_transmitted = None;
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub fn byte_size(&self) -> usize {
        self.current_bytes
    }

    fn evict_if_needed(&mut self) {
        while self.current_bytes > self.max_bytes || self.images.len() > self.max_images {
            let Some(oldest) = self.usage_order.pop_back() else {
                break;
            };
            if let Some(removed) = self.images.remove(&oldest) {
                self.current_bytes = self.current_bytes.saturating_sub(removed.image.byte_size);
                self.dropped.push(removed.image.render_image.clone());
                if let Some(client_id) = removed.client_id {
                    self.by_client_id.remove(&client_id);
                }
                if let Some(image_number) = removed.image_number {
                    self.by_image_number.remove(&image_number);
                }
                if self.last_transmitted == Some(oldest) {
                    self.last_transmitted = None;
                }
            }
        }
    }
}

impl Default for PaneImageCache {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────
// 测试
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 一张 `width` x `height` 的纯色 PNG。
    fn test_png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([1, 2, 3, 255]));
        let mut encoded = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .expect("write test png");
        encoded
    }

    fn encode(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    fn kitty_sequence(control: &str, payload: &str) -> Vec<u8> {
        format!("\x1b_G{control};{payload}\x1b\\").into_bytes()
    }

    fn transmitted(events: &[GraphicsEvent]) -> Vec<&DecodedImage> {
        events
            .iter()
            .filter_map(|event| match event {
                GraphicsEvent::Transmit { image, .. } => Some(image),
                _ => None,
            })
            .collect()
    }

    fn placements(events: &[GraphicsEvent]) -> Vec<&PlacementRequest> {
        events
            .iter()
            .filter_map(|event| match event {
                GraphicsEvent::Place(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    fn responses(events: &[GraphicsEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                GraphicsEvent::Respond(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            })
            .collect()
    }

    // ── 控制数据解析 ──

    #[test]
    fn parses_the_real_kitty_key_set() {
        let command =
            parse_kitty_command("a=T,f=100,t=d,i=31,I=7,p=2,s=64,v=48,c=10,r=5,z=-3,q=1,m=1,C=1")
                .expect("parse");
        assert_eq!(command.action, KittyAction::TransmitAndDisplay);
        assert_eq!(command.format, KittyFormat::Png);
        assert_eq!(command.medium, TransferMedium::Direct);
        assert_eq!(command.image_id, Some(31));
        assert_eq!(command.image_number, Some(7));
        assert_eq!(command.placement_id, Some(2));
        assert_eq!(command.transmit_width, Some(64));
        assert_eq!(command.transmit_height, Some(48));
        assert_eq!(command.columns, Some(10));
        assert_eq!(command.rows, Some(5));
        assert_eq!(command.z_index, -3);
        assert_eq!(command.quiet, 1);
        assert!(command.more_chunks);
        assert_eq!(command.cursor_movement, 1);
    }

    #[test]
    fn action_letters_cover_every_documented_value() {
        let cases = [
            ("a=t", KittyAction::Transmit),
            ("a=T", KittyAction::TransmitAndDisplay),
            ("a=p", KittyAction::Put),
            ("a=d", KittyAction::Delete),
            ("a=q", KittyAction::Query),
            ("a=f", KittyAction::TransmitFrame),
            ("a=a", KittyAction::ControlAnimation),
            ("a=c", KittyAction::ComposeFrames),
        ];
        for (control, expected) in cases {
            assert_eq!(
                parse_kitty_command(control).expect("parse").action,
                expected,
                "control data: {control}"
            );
        }
    }

    #[test]
    fn defaults_match_the_specification() {
        let command = parse_kitty_command("").expect("parse");
        assert_eq!(command.action, KittyAction::Transmit);
        assert_eq!(command.format, KittyFormat::Rgba);
        assert_eq!(command.medium, TransferMedium::Direct);
        assert_eq!(command.compression, Compression::None);
        assert!(!command.more_chunks);
        assert_eq!(command.delete_target, 'a');
        assert_eq!(command.quiet, 0);
    }

    #[test]
    fn transfer_medium_letters_are_media_not_identifiers() {
        assert_eq!(
            parse_kitty_command("t=f").expect("parse").medium,
            TransferMedium::File
        );
        assert_eq!(
            parse_kitty_command("t=t").expect("parse").medium,
            TransferMedium::TemporaryFile
        );
        assert_eq!(
            parse_kitty_command("t=s").expect("parse").medium,
            TransferMedium::SharedMemory
        );
    }

    #[test]
    fn unknown_keys_are_ignored_but_bad_values_are_rejected() {
        assert_eq!(
            parse_kitty_command("a=t,ZZ9=1")
                .expect_err("two letter key is malformed")
                .clone(),
            GraphicsError::Malformed
        );
        assert!(parse_kitty_command("a=t,k=99").is_ok());
        assert_eq!(
            parse_kitty_command("a=t,i=notanumber").expect_err("bad number"),
            GraphicsError::InvalidValue { key: 'i' }
        );
        assert_eq!(
            parse_kitty_command("a=z").expect_err("bad action"),
            GraphicsError::InvalidValue { key: 'a' }
        );
        assert_eq!(
            parse_kitty_command("noequals").expect_err("missing ="),
            GraphicsError::Malformed
        );
    }

    // ── 端到端扫描 ──

    #[test]
    fn scans_a_single_chunk_transmit_and_display() {
        let png = test_png(4, 2);
        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(&kitty_sequence("a=T,f=100,t=d", &encode(&png)));

        let images = transmitted(&events);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].pixel_size, (4, 2));
        assert_eq!(placements(&events).len(), 1);
        // 没给 i= / I=, 协议规定不回响应。
        assert!(responses(&events).is_empty());
    }

    #[test]
    fn transmit_only_does_not_place() {
        let png = test_png(2, 2);
        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(&kitty_sequence("a=t,f=100,i=9", &encode(&png)));

        assert_eq!(transmitted(&events).len(), 1);
        assert!(placements(&events).is_empty());
        assert_eq!(responses(&events), vec!["\x1b_Gi=9;OK\x1b\\".to_string()]);
    }

    #[test]
    fn reassembles_a_chunked_transfer() {
        let png = test_png(8, 8);
        let encoded = encode(&png);
        // kitty 把 base64 切成 <= 4096 字节的块, 这里用更小的块保证多轮。
        let chunk_size = 64;
        let chunks: Vec<&str> = encoded
            .as_bytes()
            .chunks(chunk_size)
            .map(|chunk| std::str::from_utf8(chunk).expect("base64 is ascii"))
            .collect();
        assert!(chunks.len() > 2, "test needs a multi chunk payload");

        let mut scanner = GraphicsScanner::new();
        let mut events = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let control = if index == 0 {
                "a=T,f=100,t=d,i=42,m=1".to_string()
            } else if index + 1 == chunks.len() {
                "m=0".to_string()
            } else {
                "m=1".to_string()
            };
            events.extend(scanner.feed(&kitty_sequence(&control, chunk)));
        }

        let images = transmitted(&events);
        assert_eq!(images.len(), 1, "chunks must produce exactly one image");
        assert_eq!(images[0].pixel_size, (8, 8));
        assert_eq!(placements(&events).len(), 1);
        assert_eq!(responses(&events), vec!["\x1b_Gi=42;OK\x1b\\".to_string()]);
    }

    #[test]
    fn chunked_transfer_split_across_reads() {
        let png = test_png(4, 4);
        let sequence = kitty_sequence("a=T,f=100,t=d", &encode(&png));

        let mut scanner = GraphicsScanner::new();
        let mut events = Vec::new();
        // 一次一个字节, 模拟 PTY 把序列切得七零八落。
        for byte in &sequence {
            events.extend(scanner.feed(&[*byte]));
        }
        assert_eq!(transmitted(&events).len(), 1);
    }

    #[test]
    fn raw_rgb_and_rgba_payloads_decode() {
        let mut scanner = GraphicsScanner::new();
        let rgb = vec![0x10u8, 0x20, 0x30, 0x40, 0x50, 0x60];
        let events = scanner.feed(&kitty_sequence("a=T,f=24,s=2,v=1", &encode(&rgb)));
        assert_eq!(transmitted(&events)[0].pixel_size, (2, 1));

        let rgba = vec![0u8; 4 * 3 * 2];
        let events = scanner.feed(&kitty_sequence("a=T,f=32,s=3,v=2", &encode(&rgba)));
        assert_eq!(transmitted(&events)[0].pixel_size, (3, 2));
    }

    #[test]
    fn raw_payload_without_dimensions_is_rejected() {
        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(&kitty_sequence("a=T,f=32,i=3", &encode(&[0u8; 16])));
        assert!(transmitted(&events).is_empty());
        assert_eq!(
            responses(&events),
            vec![
                "\x1b_Gi=3;EINVAL:pixel dimensions are missing or do not match the payload size\x1b\\"
                    .to_string()
            ]
        );
    }

    #[test]
    fn query_action_answers_without_storing_an_image() {
        let png = test_png(1, 1);
        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(&kitty_sequence("a=q,f=100,i=31", &encode(&png)));

        assert!(transmitted(&events).is_empty());
        assert!(placements(&events).is_empty());
        assert_eq!(responses(&events), vec!["\x1b_Gi=31;OK\x1b\\".to_string()]);
    }

    #[test]
    fn put_action_places_without_transmitting() {
        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(&kitty_sequence("a=p,i=7,c=6,r=3", ""));

        assert!(transmitted(&events).is_empty());
        let placements = placements(&events);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].image_id, Some(7));
        assert_eq!(placements[0].columns, ImageDimension::Cells(6));
        assert_eq!(placements[0].rows, ImageDimension::Cells(3));
    }

    #[test]
    fn delete_action_uses_the_image_id_not_the_medium_key() {
        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(&kitty_sequence("a=d,d=i,i=5", ""));
        let deletes: Vec<&DeleteRequest> = events
            .iter()
            .filter_map(|event| match event {
                GraphicsEvent::Delete(request) => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].scope, DeleteScope::ImageId(5));
        assert!(!deletes[0].free_data);

        let events = scanner.feed(&kitty_sequence("a=d,d=A", ""));
        let deletes: Vec<&DeleteRequest> = events
            .iter()
            .filter_map(|event| match event {
                GraphicsEvent::Delete(request) => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(deletes[0].scope, DeleteScope::All);
        assert!(deletes[0].free_data);
    }

    #[test]
    fn unsupported_features_report_errors_instead_of_guessing() {
        let mut scanner = GraphicsScanner::new();

        let events = scanner.feed(&kitty_sequence("a=T,f=100,t=f,i=1", &encode(b"/tmp/x.png")));
        assert!(transmitted(&events).is_empty());
        assert!(responses(&events)[0].contains("ENOTSUPPORTED"));

        let events = scanner.feed(&kitty_sequence("a=T,f=100,o=z,i=2", &encode(b"junk")));
        assert!(transmitted(&events).is_empty());
        assert!(responses(&events)[0].contains("ENOTSUPPORTED"));

        let events = scanner.feed(&kitty_sequence("a=f,i=3", ""));
        assert!(responses(&events)[0].contains("ENOTSUPPORTED"));
    }

    #[test]
    fn quiet_levels_suppress_responses() {
        let mut scanner = GraphicsScanner::new();
        let png = test_png(1, 1);

        let events = scanner.feed(&kitty_sequence("a=t,f=100,i=1,q=1", &encode(&png)));
        assert!(responses(&events).is_empty());

        let events = scanner.feed(&kitty_sequence(
            "a=t,f=100,i=1,q=1",
            &encode(b"not an image"),
        ));
        assert_eq!(responses(&events).len(), 1, "q=1 still reports errors");

        let events = scanner.feed(&kitty_sequence(
            "a=t,f=100,i=1,q=2",
            &encode(b"not an image"),
        ));
        assert!(responses(&events).is_empty());
    }

    #[test]
    fn malformed_payloads_do_not_produce_images() {
        let mut scanner = GraphicsScanner::new();

        // base64 里混进非法字符
        let events = scanner.feed(&kitty_sequence("a=T,f=100", "!!!!not base64!!!!"));
        assert!(transmitted(&events).is_empty());

        // 合法 base64 但不是图像
        let events = scanner.feed(&kitty_sequence("a=T,f=100", &encode(b"hello world")));
        assert!(transmitted(&events).is_empty());

        // 控制数据缺 `;`
        let events = scanner.feed(b"\x1b_Ga=T,f=100\x1b\\");
        assert!(transmitted(&events).is_empty());

        // APC 但不是 kitty graphics
        let events = scanner.feed(b"\x1b_Xsomething\x1b\\");
        assert!(events.is_empty());

        // 扫描器必须回到可用状态
        let png = test_png(1, 1);
        let events = scanner.feed(&kitty_sequence("a=T,f=100", &encode(&png)));
        assert_eq!(transmitted(&events).len(), 1);
    }

    #[test]
    fn oversized_sequences_are_dropped_and_scanning_recovers() {
        let mut scanner = GraphicsScanner::new();
        scanner.feed(b"\x1b_Ga=T,f=100;");
        // 分批喂入超过上限的 payload。
        let filler = vec![b'A'; 1024 * 1024];
        for _ in 0..(MAX_SEQUENCE_BYTES / filler.len() + 2) {
            assert!(scanner.feed(&filler).is_empty());
        }
        assert!(scanner.feed(b"\x1b\\").is_empty());

        let png = test_png(1, 1);
        let events = scanner.feed(&kitty_sequence("a=T,f=100", &encode(&png)));
        assert_eq!(transmitted(&events).len(), 1, "scanner must recover");
    }

    #[test]
    fn oversized_chunked_transfer_is_rejected() {
        let mut scanner = GraphicsScanner::new();
        let chunk = "A".repeat(4096);
        let mut rejected = false;
        for index in 0..(MAX_TRANSFER_BYTES / chunk.len() + 2) {
            let control = if index == 0 {
                "a=T,f=100,i=1,m=1"
            } else {
                "m=1"
            };
            let events = scanner.feed(&kitty_sequence(control, &chunk));
            if events
                .iter()
                .any(|event| matches!(event, GraphicsEvent::Respond(_)))
            {
                rejected = true;
                break;
            }
        }
        assert!(rejected, "the transfer limit must terminate the assembly");
    }

    #[test]
    fn declared_pixel_dimensions_are_bounded() {
        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(&kitty_sequence(
            &format!("a=T,f=32,i=1,s={},v={}", u32::MAX, u32::MAX),
            &encode(&[0u8; 16]),
        ));
        assert!(transmitted(&events).is_empty());
        assert_eq!(responses(&events).len(), 1);
    }

    #[test]
    fn plain_text_and_other_escapes_are_left_alone() {
        let mut scanner = GraphicsScanner::new();
        assert!(scanner.feed(b"hello \x1b[31mworld\x1b[0m\r\n").is_empty());
        assert!(scanner.feed(b"\x1b]0;a title\x07").is_empty());
        assert!(scanner.feed(b"\x1b]52;c;ZGF0YQ==\x1b\\").is_empty());
        assert!(scanner.feed(b"\x1bP+q544e\x1b\\").is_empty());

        // 前面那些序列都没有让扫描器卡住。
        let png = test_png(1, 1);
        let events = scanner.feed(&kitty_sequence("a=T,f=100", &encode(&png)));
        assert_eq!(transmitted(&events).len(), 1);
    }

    #[test]
    fn graphics_sequence_embedded_in_normal_output() {
        let png = test_png(3, 3);
        let mut stream = b"$ icat cat.png\r\n".to_vec();
        stream.extend(kitty_sequence("a=T,f=100", &encode(&png)));
        stream.extend_from_slice(b"\r\n$ ");

        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(&stream);
        assert_eq!(transmitted(&events).len(), 1);
        assert_eq!(placements(&events).len(), 1);
    }

    // ── iTerm2 OSC 1337 ──

    #[test]
    fn parses_iterm_inline_image() {
        let png = test_png(6, 4);
        let sequence = format!(
            "\x1b]1337;File=name={};size={};inline=1;width=10;height=5:{}\x07",
            encode(b"cat.png"),
            png.len(),
            encode(&png)
        );

        let mut scanner = GraphicsScanner::new();
        let events = scanner.feed(sequence.as_bytes());

        assert_eq!(transmitted(&events).len(), 1);
        let placements = placements(&events);
        assert_eq!(placements[0].columns, ImageDimension::Cells(10));
        assert_eq!(placements[0].rows, ImageDimension::Cells(5));
    }

    #[test]
    fn iterm_dimension_units() {
        assert_eq!(parse_iterm_dimension("auto"), ImageDimension::Auto);
        assert_eq!(parse_iterm_dimension("12"), ImageDimension::Cells(12));
        assert_eq!(parse_iterm_dimension("300px"), ImageDimension::Pixels(300));
        assert_eq!(parse_iterm_dimension("50%"), ImageDimension::Percent(50));
        assert_eq!(parse_iterm_dimension("nonsense"), ImageDimension::Auto);
    }

    #[test]
    fn iterm_without_inline_is_a_download_not_a_display() {
        let png = test_png(2, 2);
        let sequence = format!(
            "\x1b]1337;File=name={}:{}\x07",
            encode(b"cat.png"),
            encode(&png)
        );
        let mut scanner = GraphicsScanner::new();
        assert!(scanner.feed(sequence.as_bytes()).is_empty());
    }

    #[test]
    fn iterm_multipart_transfer() {
        let png = test_png(5, 5);
        let encoded = encode(&png);
        let mut scanner = GraphicsScanner::new();
        let mut events = Vec::new();

        events.extend(scanner.feed(b"\x1b]1337;MultipartFile=inline=1;width=4\x07"));
        for chunk in encoded.as_bytes().chunks(32) {
            let part = format!(
                "\x1b]1337;FilePart={}\x07",
                std::str::from_utf8(chunk).expect("base64 is ascii")
            );
            events.extend(scanner.feed(part.as_bytes()));
        }
        events.extend(scanner.feed(b"\x1b]1337;FileEnd\x07"));

        assert_eq!(transmitted(&events).len(), 1);
        assert_eq!(transmitted(&events)[0].pixel_size, (5, 5));
        assert_eq!(placements(&events)[0].columns, ImageDimension::Cells(4));
    }

    #[test]
    fn iterm_sequence_terminated_by_st() {
        let png = test_png(2, 2);
        let sequence = format!("\x1b]1337;File=inline=1:{}\x1b\\", encode(&png));
        let mut scanner = GraphicsScanner::new();
        assert_eq!(transmitted(&scanner.feed(sequence.as_bytes())).len(), 1);
    }

    // ── 缓存 ──

    fn cache_image(bytes: usize) -> DecodedImage {
        DecodedImage {
            pixel_size: (1, 1),
            byte_size: bytes,
            render_image: render_image_from_rgba(1, 1, vec![0, 0, 0, 255])
                .expect("build render image"),
        }
    }

    #[test]
    fn cache_evicts_by_image_count() {
        let mut cache = PaneImageCache::new();
        cache.set_max_images(2);
        let first = cache.insert(cache_image(10), None, None);
        let second = cache.insert(cache_image(10), None, None);
        let third = cache.insert(cache_image(10), None, None);

        assert!(cache.get(first).is_none());
        assert!(cache.get(second).is_some());
        assert!(cache.get(third).is_some());
    }

    #[test]
    fn cache_evicts_by_byte_budget() {
        let mut cache = PaneImageCache::new();
        cache.set_max_bytes(100);
        let first = cache.insert(cache_image(60), None, None);
        let second = cache.insert(cache_image(60), None, None);

        assert!(cache.get(first).is_none());
        assert!(cache.get(second).is_some());
        assert_eq!(cache.byte_size(), 60);
    }

    #[test]
    fn cache_indexes_client_ids_and_numbers() {
        let mut cache = PaneImageCache::new();
        let id = cache.insert(cache_image(10), Some(31), Some(7));
        assert_eq!(cache.resolve_client_id(31), Some(id));
        assert_eq!(cache.resolve_image_number(7), Some(id));
        assert_eq!(cache.last_transmitted(), Some(id));

        // 同一个 client id 再传一次会替换旧图。
        let replacement = cache.insert(cache_image(10), Some(31), None);
        assert_ne!(replacement, id);
        assert_eq!(cache.resolve_client_id(31), Some(replacement));
        assert!(cache.get(id).is_none());
        assert_eq!(cache.len(), 1);

        cache.remove(replacement);
        assert!(cache.resolve_client_id(31).is_none());
        assert!(cache.is_empty());
        assert_eq!(cache.byte_size(), 0);
    }
}
