//! NV12 帧的几何变换：旋转、裁切、镜像、翻转。
//!
//! # NV12 的布局约束（这是本模块全部难度的来源）
//!
//! NV12 是 YUV 4:2:0 的半平面格式：
//!
//! `@text
//!   Y 平面：width × height 字节，每像素 1 个亮度
//!   UV 平面：(width/2) × (height/2) 对，每对 2 字节（U 然后 V），
//!            一对覆盖 2×2 个像素
//! `@
//!
//! 三条硬约束：
//!
//! 1. **宽高都必须是偶数**。奇数尺寸没有合法的 UV 平面。
//! 2. **裁切的偏移必须是偶数**。从奇数 x 开始裁，色度采样会对不齐，
//!    结果是一半像素的 U/V 取错了位置。
//! 3. **旋转 90°/270° 会交换宽高**。旋转后 UV 平面的「行宽」也随之改变，
//!    不能沿用原来的 stride。
//!
//! 这三条如果只靠注释约定，调用方一定会踩。因此这里用类型把它们钉住：
//! [`Geometry::new`] 拒绝奇数尺寸，[`Crop::new`] 拒绝奇数偏移，
//! 且所有输出都走同一个 [`Frame`] 类型。

/// 一帧 NV12 的几何描述（宽高都是偶数）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    width: u32,
    height: u32,
}

impl Geometry {
    /// 构造。宽高必须非零且为偶数 —— 奇数尺寸没有合法的 UV 平面。
    pub fn new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return None;
        }
        Some(Self { width, height })
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }

    /// 一帧的字节数
    pub fn frame_len(self) -> usize {
        let y = self.width as usize * self.height as usize;
        y + y / 2
    }

    /// 旋转 90° 的整数倍后得到的几何
    ///
    /// 90° 与 270° 会交换宽高；180° 与 0° 不变。
    pub fn rotated(self, rotation: Rotation) -> Self {
        match rotation {
            Rotation::None | Rotation::Deg180 => self,
            Rotation::Deg90 | Rotation::Deg270 => Self {
                width: self.height,
                height: self.width,
            },
        }
    }

    /// 这块几何里是否容得下这个裁切矩形
    pub fn contains(self, crop: Crop) -> bool {
        crop.x as u64 + crop.width as u64 <= self.width as u64
            && crop.y as u64 + crop.height as u64 <= self.height as u64
    }
}

/// 顺时针旋转角度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    None,
    Deg90,
    Deg180,
    Deg270,
}

impl Rotation {
    /// 解析命令行/配置里的角度。只接受 0/90/180/270。
    pub fn parse(degrees: u32) -> Option<Self> {
        match degrees % 360 {
            0 => Some(Self::None),
            90 => Some(Self::Deg90),
            180 => Some(Self::Deg180),
            270 => Some(Self::Deg270),
            _ => None,
        }
    }

    pub fn degrees(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Deg90 => 90,
            Self::Deg180 => 180,
            Self::Deg270 => 270,
        }
    }
}

/// 裁切矩形。x/y 必须是偶数（色度对齐），宽高必须是偶数且非零。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Crop {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        if x % 2 != 0 || y % 2 != 0 || width % 2 != 0 || height % 2 != 0 {
            return None;
        }
        Some(Self {
            x,
            y,
            width,
            height,
        })
    }

    /// 整帧（不裁切）
    pub fn full(geometry: Geometry) -> Self {
        Self {
            x: 0,
            y: 0,
            width: geometry.width,
            height: geometry.height,
        }
    }
}

/// 一帧完整的变换管线参数
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transform {
    /// 先裁切（在原坐标系里）
    pub crop: Option<Crop>,
    /// 再旋转
    pub rotation: Rotation,
    /// 再水平镜像
    pub mirror: bool,
    /// 再垂直翻转
    pub flip: bool,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            crop: None,
            rotation: Rotation::None,
            mirror: false,
            flip: false,
        }
    }
}

impl Transform {
    /// 是否什么都不做（可以走快速路径直接跳过整帧）
    pub fn is_identity(self) -> bool {
        self.crop.is_none() && self.rotation == Rotation::None && !self.mirror && !self.flip
    }

    /// 输入几何 → 输出几何
    pub fn output_geometry(self, input: Geometry) -> Option<Geometry> {
        let cropped = match self.crop {
            Some(crop) => {
                if !input.contains(crop) {
                    return None;
                }
                Geometry::new(crop.width, crop.height)?
            }
            None => input,
        };
        Some(cropped.rotated(self.rotation))
    }
}

/// 一帧 NV12 的拥有型容器
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub geometry: Geometry,
    pub data: Vec<u8>,
}

impl Frame {
    /// 从已有的字节建帧；长度不符时返回 None（不静默补零 —— 那会让下游
    /// 看到一块看起来正常、实际错位的画面）。
    pub fn from_bytes(geometry: Geometry, data: Vec<u8>) -> Option<Self> {
        if data.len() < geometry.frame_len() {
            return None;
        }
        Some(Self { geometry, data })
    }

    /// 造一帧测试用的图：Y 平面按 (x*3 + y*7) % 251 填值，UV 按位置填
    ///
    /// 生产代码不用它。放在这里是因为**测试需要能一眼看出每个像素来自哪里**，
    /// 否则「旋转对了」只能靠再写一遍同样的旋转来验证 —— 那是循环论证。
    pub fn test_pattern(geometry: Geometry) -> Self {
        let mut data = vec![0u8; geometry.frame_len()];
        let width = geometry.width() as usize;
        let height = geometry.height() as usize;
        for y in 0..height {
            for x in 0..width {
                data[y * width + x] = ((x * 3 + y * 7) % 251) as u8;
            }
        }
        let y_len = width * height;
        for pair in 0..(y_len / 4) {
            let index = y_len + pair * 2;
            data[index] = (pair % 200) as u8 + 16;
            data[index + 1] = (pair % 240) as u8 + 8;
        }
        Self { geometry, data }
    }

    /// Y 平面上 (x, y) 的亮度
    pub fn luma(&self, x: u32, y: u32) -> Option<u8> {
        if x >= self.geometry.width() || y >= self.geometry.height() {
            return None;
        }
        let index = y as usize * self.geometry.width() as usize + x as usize;
        self.data.get(index).copied()
    }
}

/// 执行变换，返回新帧
pub fn apply(input: &Frame, transform: Transform) -> Option<Frame> {
    let geometry = transform.output_geometry(input.geometry)?;
    let mut output = Frame {
        geometry,
        data: vec![0u8; geometry.frame_len()],
    };
    apply_into(input, transform, &mut output)?;
    Some(output)
}

/// 就地风格：把结果写进 output（尺寸必须已经匹配）。
///
/// 分开一个 _into 版本是为了在热路径上避免每帧分配 —— 手机链路是 60fps，
/// 1080p 一帧 3 MB，每秒分配 180 MB 会让分配器成为热点。
pub fn apply_into(input: &Frame, transform: Transform, output: &mut Frame) -> Option<()> {
    let expected = transform.output_geometry(input.geometry)?;
    if output.geometry != expected || output.data.len() < expected.frame_len() {
        return None;
    }

    // 1. 裁切（在原坐标系里，按 Y 平面坐标逐行拷贝）
    let crop = transform.crop.unwrap_or_else(|| Crop::full(input.geometry));
    if crop.width % 2 != 0 || crop.height % 2 != 0 || crop.x % 2 != 0 || crop.y % 2 != 0 {
        return None;
    }
    if !input.geometry.contains(crop) {
        return None;
    }

    let cropped = Geometry::new(crop.width, crop.height)?;
    // 关键：这里必须用**裁切后**的宽度做 stride，不能用 output 的宽度。
    // 90°/270° 旋转会交换宽高，output.geometry.width() 此时是旋转后的宽度，
    // 拿它当 stride 会把行写错位（越大越离谱，小图上直接越界 panic）。
    let stride = cropped.width() as usize;
    fill_cropped_y(input, crop, &mut output.data, stride);
    fill_cropped_uv(input, crop, &mut output.data, stride);

    // 2. 旋转（在裁切后的尺寸上做）
    if transform.rotation != Rotation::None {
        rotate_in_place(output, cropped, transform.rotation)?;
    }

    // 3. 镜像 / 翻转（在旋转后的尺寸上做）
    if transform.mirror {
        mirror_in_place(output);
    }
    if transform.flip {
        flip_in_place(output);
    }
    Some(())
}

fn fill_cropped_y(input: &Frame, crop: Crop, data: &mut [u8], out_stride: usize) {
    let in_width = input.geometry.width() as usize;
    let crop_width = crop.width as usize;
    for row in 0..crop.height as usize {
        let source_start = (crop.y as usize + row) * in_width + crop.x as usize;
        let dest_start = row * out_stride;
        data[dest_start..dest_start + crop_width]
            .copy_from_slice(&input.data[source_start..source_start + crop_width]);
    }
}

fn fill_cropped_uv(input: &Frame, crop: Crop, data: &mut [u8], out_stride: usize) {
    let in_width = input.geometry.width() as usize;
    let in_y_len = in_width * input.geometry.height() as usize;
    let out_y_len = out_stride * crop.height as usize;
    // UV 平面里「一行」覆盖 crop.width 个字节（= crop.width/2 对）
    let uv_row_bytes = crop.width as usize;
    for row in 0..(crop.height as usize / 2) {
        let source_row = crop.y as usize / 2 + row;
        let source_start = in_y_len + source_row * in_width + crop.x as usize;
        let dest_start = out_y_len + row * uv_row_bytes;
        data[dest_start..dest_start + uv_row_bytes]
            .copy_from_slice(&input.data[source_start..source_start + uv_row_bytes]);
    }
}

/// 顺时针旋转 90° 的整数倍。data 已经是「裁切后」的紧凑排列，
/// source 是裁切后的几何。
fn rotate_in_place(frame: &mut Frame, source: Geometry, rotation: Rotation) -> Option<()> {
    let target = source.rotated(rotation);
    let mut rotated = vec![0u8; target.frame_len()];

    let sw = source.width() as usize;
    let sh = source.height() as usize;
    let dw = target.width() as usize;

    for y in 0..sh {
        for x in 0..sw {
            let (dx, dy) = match rotation {
                Rotation::None => (x, y),
                Rotation::Deg90 => (sh - 1 - y, x),
                Rotation::Deg180 => (sw - 1 - x, sh - 1 - y),
                Rotation::Deg270 => (y, sw - 1 - x),
            };
            rotated[dy * dw + dx] = frame.data[y * sw + x];
        }
    }

    // UV 平面：一对 UV 覆盖 2×2 像素，所以按「色度块」旋转
    let source_y_len = sw * sh;
    let target_y_len = target.width() as usize * target.height() as usize;
    let cw = sw / 2;
    let ch = sh / 2;
    let dcw = dw / 2;
    for cy in 0..ch {
        for cx in 0..cw {
            let (dcx, dcy) = match rotation {
                Rotation::None => (cx, cy),
                Rotation::Deg90 => (ch - 1 - cy, cx),
                Rotation::Deg180 => (cw - 1 - cx, ch - 1 - cy),
                Rotation::Deg270 => (cy, cw - 1 - cx),
            };
            let source_index = source_y_len + (cy * cw + cx) * 2;
            let dest_index = target_y_len + (dcy * dcw + dcx) * 2;
            rotated[dest_index] = frame.data[source_index];
            rotated[dest_index + 1] = frame.data[source_index + 1];
        }
    }

    *frame = Frame {
        geometry: target,
        data: rotated,
    };
    Some(())
}

fn mirror_in_place(frame: &mut Frame) {
    let width = frame.geometry.width() as usize;
    let height = frame.geometry.height() as usize;
    let y_len = width * height;
    for row in frame.data[..y_len].chunks_exact_mut(width) {
        row.reverse();
    }
    // UV：按「一对」交换，所以每对 2 字节要整体搬
    for row in frame.data[y_len..].chunks_exact_mut(width) {
        let pairs = width / 2;
        for pair in 0..(pairs / 2) {
            let left = pair * 2;
            let right = width - 2 - left;
            row.swap(left, right);
            row.swap(left + 1, right + 1);
        }
    }
}

fn flip_in_place(frame: &mut Frame) {
    let width = frame.geometry.width() as usize;
    let height = frame.geometry.height() as usize;
    let y_len = width * height;
    for top in 0..(height / 2) {
        let bottom = height - 1 - top;
        let (before, after) = frame.data.split_at_mut(bottom * width);
        before[top * width..top * width + width].swap_with_slice(&mut after[..width]);
    }
    // UV 平面只有 height/2 行
    let uv_rows = height / 2;
    for top in 0..(uv_rows / 2) {
        let bottom = uv_rows - 1 - top;
        let top_start = y_len + top * width;
        let bottom_start = y_len + bottom * width;
        let (before, after) = frame.data.split_at_mut(bottom_start);
        before[top_start..top_start + width].swap_with_slice(&mut after[..width]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(width: u32, height: u32) -> Geometry {
        Geometry::new(width, height).expect("偶数尺寸应被接受")
    }

    #[test]
    fn 奇数尺寸被拒绝() {
        assert!(Geometry::new(1921, 1080).is_none());
        assert!(Geometry::new(1920, 1081).is_none());
        assert!(Geometry::new(0, 1080).is_none());
        assert!(Geometry::new(1920, 1080).is_some());
    }

    #[test]
    fn 帧长度符合_nv12_公式() {
        assert_eq!(g(1920, 1080).frame_len(), 1920 * 1080 * 3 / 2);
        assert_eq!(g(2, 2).frame_len(), 6);
        assert_eq!(g(640, 480).frame_len(), 640 * 480 * 3 / 2);
    }

    #[test]
    fn 旋转角度只接受四档() {
        assert_eq!(Rotation::parse(0), Some(Rotation::None));
        assert_eq!(Rotation::parse(90), Some(Rotation::Deg90));
        assert_eq!(Rotation::parse(180), Some(Rotation::Deg180));
        assert_eq!(Rotation::parse(270), Some(Rotation::Deg270));
        assert_eq!(Rotation::parse(360), Some(Rotation::None));
        assert_eq!(Rotation::parse(45), None);
        assert_eq!(Rotation::parse(91), None);
    }

    #[test]
    fn 九十度旋转交换宽高() {
        let geometry = g(1920, 1080);
        assert_eq!(geometry.rotated(Rotation::Deg90).width(), 1080);
        assert_eq!(geometry.rotated(Rotation::Deg90).height(), 1920);
        assert_eq!(geometry.rotated(Rotation::Deg270).width(), 1080);
        assert_eq!(geometry.rotated(Rotation::Deg180), geometry);
        assert_eq!(geometry.rotated(Rotation::None), geometry);
    }

    #[test]
    fn 裁切拒绝奇数偏移与奇数尺寸() {
        assert!(Crop::new(1, 0, 100, 100).is_none(), "奇数 x");
        assert!(Crop::new(0, 1, 100, 100).is_none(), "奇数 y");
        assert!(Crop::new(0, 0, 101, 100).is_none(), "奇数宽");
        assert!(Crop::new(0, 0, 100, 101).is_none(), "奇数高");
        assert!(Crop::new(0, 0, 0, 100).is_none(), "零宽");
        assert!(Crop::new(2, 4, 100, 100).is_some());
    }

    #[test]
    fn 输出几何把裁切与旋转叠加起来() {
        let input = g(1920, 1080);
        let crop = Crop::new(100, 50, 800, 600).unwrap();
        let transform = Transform {
            crop: Some(crop),
            rotation: Rotation::Deg90,
            ..Default::default()
        };
        let output = transform.output_geometry(input).expect("应能算出输出几何");
        assert_eq!(output.width(), 600);
        assert_eq!(output.height(), 800);
    }

    #[test]
    fn 超出边界的裁切被拒绝() {
        let input = g(1920, 1080);
        let too_wide = Crop::new(1200, 0, 800, 100).unwrap();
        assert!(Transform {
            crop: Some(too_wide),
            ..Default::default()
        }
        .output_geometry(input)
        .is_none());
        let too_tall = Crop::new(0, 1000, 100, 200).unwrap();
        assert!(Transform {
            crop: Some(too_tall),
            ..Default::default()
        }
        .output_geometry(input)
        .is_none());
    }

    #[test]
    fn 恒等变换返回原样的帧() {
        let geometry = g(8, 6);
        let input = Frame::test_pattern(geometry);
        let transform = Transform::default();
        assert!(transform.is_identity());
        let output = apply(&input, transform).expect("恒等变换应成功");
        assert_eq!(output.geometry, geometry);
        assert_eq!(output.data, input.data);
    }

    #[test]
    fn 九十度旋转把左上角搬到右上角() {
        let geometry = g(4, 6);
        let input = Frame::test_pattern(geometry);
        let expected = input.luma(0, 0).unwrap();
        let output = apply(
            &input,
            Transform {
                rotation: Rotation::Deg90,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output.geometry.width(), 6);
        assert_eq!(output.geometry.height(), 4);
        // 顺时针 90°：(x, y) -> (height-1-y, x)；原点 (0,0) -> (5, 0)
        assert_eq!(output.luma(5, 0), Some(expected));
    }

    #[test]
    fn 旋转四次回到原样() {
        let geometry = g(8, 4);
        let input = Frame::test_pattern(geometry);
        let mut current = input.clone();
        for _ in 0..4 {
            current = apply(
                &current,
                Transform {
                    rotation: Rotation::Deg90,
                    ..Default::default()
                },
            )
            .expect("每步都应成功");
        }
        assert_eq!(current.geometry, geometry);
        assert_eq!(current.data, input.data, "四次 90° 应完全复原");
    }

    #[test]
    fn 一百八十度等价于两次九十度() {
        let geometry = g(6, 4);
        let input = Frame::test_pattern(geometry);
        let direct = apply(
            &input,
            Transform {
                rotation: Rotation::Deg180,
                ..Default::default()
            },
        )
        .unwrap();
        let twice = apply(
            &apply(
                &input,
                Transform {
                    rotation: Rotation::Deg90,
                    ..Default::default()
                },
            )
            .unwrap(),
            Transform {
                rotation: Rotation::Deg90,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(direct.data, twice.data);
    }

    #[test]
    fn 二百七十度等价于逆时针九十度() {
        let geometry = g(6, 4);
        let input = Frame::test_pattern(geometry);
        let forward = apply(
            &input,
            Transform {
                rotation: Rotation::Deg270,
                ..Default::default()
            },
        )
        .unwrap();
        let counter = apply(
            &apply(
                &apply(
                    &input,
                    Transform {
                        rotation: Rotation::Deg90,
                        ..Default::default()
                    },
                )
                .unwrap(),
                Transform {
                    rotation: Rotation::Deg90,
                    ..Default::default()
                },
            )
            .unwrap(),
            Transform {
                rotation: Rotation::Deg90,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(forward.data, counter.data);
    }

    #[test]
    fn 镜像两次回到原样() {
        let geometry = g(8, 6);
        let input = Frame::test_pattern(geometry);
        let once = apply(
            &input,
            Transform {
                mirror: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_ne!(once.data, input.data, "镜像应真的改了内容");
        let twice = apply(
            &once,
            Transform {
                mirror: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(twice.data, input.data);
    }

    #[test]
    fn 翻转两次回到原样() {
        let geometry = g(8, 6);
        let input = Frame::test_pattern(geometry);
        let once = apply(
            &input,
            Transform {
                flip: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_ne!(once.data, input.data);
        let twice = apply(
            &once,
            Transform {
                flip: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(twice.data, input.data);
    }

    #[test]
    fn 镜像的是左右而不是上下() {
        let geometry = g(4, 4);
        let mut input = Frame::test_pattern(geometry);
        for value in input.data.iter_mut() {
            *value = 0;
        }
        input.data[0] = 200;
        let output = apply(
            &input,
            Transform {
                mirror: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output.luma(3, 0), Some(200));
        assert_eq!(output.luma(0, 0), Some(0));
    }

    #[test]
    fn 翻转的是上下而不是左右() {
        let geometry = g(4, 4);
        let mut input = Frame::test_pattern(geometry);
        for value in input.data.iter_mut() {
            *value = 0;
        }
        input.data[0] = 200;
        let output = apply(
            &input,
            Transform {
                flip: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output.luma(0, 3), Some(200));
        assert_eq!(output.luma(0, 0), Some(0));
    }

    #[test]
    fn 裁切取出正确的区域() {
        let geometry = g(8, 8);
        let input = Frame::test_pattern(geometry);
        let crop = Crop::new(2, 4, 4, 4).unwrap();
        let output = apply(
            &input,
            Transform {
                crop: Some(crop),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output.geometry.width(), 4);
        assert_eq!(output.geometry.height(), 4);
        assert_eq!(output.luma(0, 0), input.luma(2, 4), "裁切原点是错的");
        assert_eq!(output.luma(3, 3), input.luma(5, 7));
    }

    #[test]
    fn 裁切后旋转的尺寸是裁切尺寸的旋转() {
        let geometry = g(16, 8);
        let input = Frame::test_pattern(geometry);
        let crop = Crop::new(4, 2, 6, 4).unwrap();
        let output = apply(
            &input,
            Transform {
                crop: Some(crop),
                rotation: Rotation::Deg90,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output.geometry.width(), 4);
        assert_eq!(output.geometry.height(), 6);
        assert_eq!(output.data.len(), output.geometry.frame_len());
    }

    #[test]
    fn 输出缓冲区尺寸不符时拒绝而不是写坏内存() {
        // 必须用非正方形：正方形旋转 90° 后几何不变，测不出「尺寸校验」这一条。
        let geometry = g(8, 4);
        let input = Frame::test_pattern(geometry);
        let transform = Transform {
            rotation: Rotation::Deg90,
            ..Default::default()
        };
        let mut wrong = Frame {
            geometry,
            data: vec![0u8; geometry.frame_len()],
        };
        assert!(apply_into(&input, transform, &mut wrong).is_none());
    }

    #[test]
    fn 字节不足的帧被拒绝而不是补零() {
        let geometry = g(8, 8);
        assert!(Frame::from_bytes(geometry, vec![0u8; 10]).is_none());
        assert!(Frame::from_bytes(geometry, vec![0u8; geometry.frame_len()]).is_some());
    }

    #[test]
    fn 所有变换组合都不越界且输出尺寸正确() {
        let geometry = g(16, 12);
        let input = Frame::test_pattern(geometry);
        let crops = [
            None,
            Crop::new(0, 0, 16, 12),
            Crop::new(2, 2, 8, 6),
            Crop::new(4, 0, 12, 8),
        ];
        let rotations = [
            Rotation::None,
            Rotation::Deg90,
            Rotation::Deg180,
            Rotation::Deg270,
        ];
        let mut cases = 0;
        for crop in crops {
            for rotation in rotations {
                for mirror in [false, true] {
                    for flip in [false, true] {
                        let transform = Transform {
                            crop,
                            rotation,
                            mirror,
                            flip,
                        };
                        let output = apply(&input, transform)
                            .unwrap_or_else(|| panic!("组合失败：{transform:?}"));
                        let expected = transform.output_geometry(geometry).expect("输出几何应可算");
                        assert_eq!(output.geometry, expected, "{transform:?}");
                        assert_eq!(output.data.len(), expected.frame_len(), "{transform:?}");
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 4 * 4 * 2 * 2);
    }

    #[test]
    fn 恒等判定只在真的什么都不做时为真() {
        assert!(Transform::default().is_identity());
        for transform in [
            Transform {
                rotation: Rotation::Deg90,
                ..Default::default()
            },
            Transform {
                mirror: true,
                ..Default::default()
            },
            Transform {
                flip: true,
                ..Default::default()
            },
            Transform {
                crop: Crop::new(0, 0, 8, 8),
                ..Default::default()
            },
            Transform {
                rotation: Rotation::Deg180,
                ..Default::default()
            },
        ] {
            assert!(!transform.is_identity(), "{transform:?} 不该被当成恒等");
        }
    }

    #[test]
    fn 一像素高的帧也能旋转() {
        let geometry = g(4, 2);
        let input = Frame::test_pattern(geometry);
        let output = apply(
            &input,
            Transform {
                rotation: Rotation::Deg90,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output.geometry.width(), 2);
        assert_eq!(output.geometry.height(), 4);
    }

    #[test]
    fn 镜加翻等于旋转一百八十度() {
        // 这是个容易写错的地方：镜像 + 翻转是「绕过中心点的点对称」，
        // 与 180° 旋转在像素上完全等价。
        let geometry = g(8, 6);
        let input = Frame::test_pattern(geometry);
        let both = apply(
            &input,
            Transform {
                mirror: true,
                flip: true,
                ..Default::default()
            },
        )
        .unwrap();
        let rotated = apply(
            &input,
            Transform {
                rotation: Rotation::Deg180,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(both.data, rotated.data);
    }
}
