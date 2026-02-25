use super::*;

pub(crate) fn avg_y_block_geom(
    y: &[u8],
    width: usize,
    height: usize,
    block: usize,
    bx: usize,
    by: usize,
    geom: Geom,
) -> f64 {
    let x_center_nom = bx as f64 * block as f64 + (block as f64 * 0.5);
    let y_center_nom = by as f64 * block as f64 + (block as f64 * 0.5);
    let (x_center, y_center) = geom_map_center_px(width, height, x_center_nom, y_center_nom, geom);
    let dx = x_center - x_center_nom;
    let dy = y_center - y_center_nom;
    avg_y_block_shift(y, width, height, block, bx, by, dx, dy)
}

fn geom_map_center_px(width: usize, height: usize, x: f64, y: f64, geom: Geom) -> (f64, f64) {
    geom.affine_for_frame(width, height).map(x, y)
}

fn sample_plane_bilinear(plane: &[u8], width: usize, height: usize, x: f64, y: f64) -> f64 {
    if width == 0 || height == 0 {
        return 0.0;
    }
    let x = x.clamp(0.0, width.saturating_sub(1) as f64);
    let y = y.clamp(0.0, height.saturating_sub(1) as f64);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let tx = x - x0 as f64;
    let ty = y - y0 as f64;
    let p00 = plane[y0 * width + x0] as f64;
    let p10 = plane[y0 * width + x1] as f64;
    let p01 = plane[y1 * width + x0] as f64;
    let p11 = plane[y1 * width + x1] as f64;
    let a = p00 * (1.0 - tx) + p10 * tx;
    let b = p01 * (1.0 - tx) + p11 * tx;
    a * (1.0 - ty) + b * ty
}

#[derive(Clone)]
struct BilinearAxisLut {
    i0: Vec<usize>,
    i1: Vec<usize>,
    t1: Vec<f32>,
}

fn build_bilinear_axis_lut(dst_len: usize, src_len: usize, scale: f64, offset: f64) -> BilinearAxisLut {
    let mut i0 = vec![0usize; dst_len];
    let mut i1 = vec![0usize; dst_len];
    let mut t1 = vec![0.0f32; dst_len];
    if dst_len == 0 || src_len == 0 {
        return BilinearAxisLut { i0, i1, t1 };
    }
    let max_coord = src_len.saturating_sub(1) as f64;
    for d in 0..dst_len {
        let s = (scale * d as f64 + offset).clamp(0.0, max_coord);
        let p0 = s.floor() as usize;
        let p1 = (p0 + 1).min(src_len - 1);
        i0[d] = p0;
        i1[d] = p1;
        t1[d] = (s - p0 as f64) as f32;
    }
    BilinearAxisLut { i0, i1, t1 }
}

fn integral_from_resampled_axis_aligned_u8(
    plane: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    x_scale: f64,
    x_off: f64,
    y_scale: f64,
    y_off: f64,
) -> IntegralImageF32 {
    let x_lut = build_bilinear_axis_lut(dst_w, src_w, x_scale, x_off);
    let y_lut = build_bilinear_axis_lut(dst_h, src_h, y_scale, y_off);
    let stride = dst_w + 1;
    let mut sum = vec![0.0f32; (dst_h + 1) * stride];

    for y in 0..dst_h {
        let sy0 = y_lut.i0[y];
        let sy1 = y_lut.i1[y];
        let ty = y_lut.t1[y];
        let wy0 = 1.0f32 - ty;
        let row0 = &plane[sy0 * src_w..(sy0 + 1) * src_w];
        let row1 = &plane[sy1 * src_w..(sy1 + 1) * src_w];

        let mut row_acc = 0.0f32;
        for x in 0..dst_w {
            let sx0 = x_lut.i0[x];
            let sx1 = x_lut.i1[x];
            let tx = x_lut.t1[x];

            let p00 = row0[sx0] as f32;
            let p10 = row0[sx1] as f32;
            let p01 = row1[sx0] as f32;
            let p11 = row1[sx1] as f32;

            let a = p00 + (p10 - p00) * tx;
            let b = p01 + (p11 - p01) * tx;
            let v = a * wy0 + b * ty;

            row_acc += v;
            let above = sum[y * stride + (x + 1)];
            sum[(y + 1) * stride + (x + 1)] = above + row_acc;
        }
    }

    IntegralImageF32 { width: dst_w, height: dst_h, sum }
}

#[derive(Clone)]
struct IntegralImageF32 {
    width: usize,
    height: usize,
    sum: Vec<f32>, // (height+1) x (width+1)
}

impl IntegralImageF32 {
    fn stride(&self) -> usize {
        self.width + 1
    }

    #[allow(dead_code)]
    fn from_resampled<F>(width: usize, height: usize, mut sample_fn: F) -> Self
    where
        F: FnMut(usize, usize) -> f32,
    {
        let stride = width + 1;
        let mut sum = vec![0.0f32; (height + 1) * stride];
        for y in 0..height {
            let mut row_acc = 0.0f32;
            for x in 0..width {
                row_acc += sample_fn(x, y);
                let above = sum[y * stride + (x + 1)];
                sum[(y + 1) * stride + (x + 1)] = above + row_acc;
            }
        }
        Self { width, height, sum }
    }

    fn rect_sum(&self, x0: usize, y0: usize, w: usize, h: usize) -> f32 {
        if self.width == 0 || self.height == 0 || w == 0 || h == 0 {
            return 0.0;
        }
        let x0 = x0.min(self.width);
        let y0 = y0.min(self.height);
        let x1 = x0.saturating_add(w).min(self.width);
        let y1 = y0.saturating_add(h).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return 0.0;
        }
        let s = self.stride();
        self.sum[y1 * s + x1] - self.sum[y0 * s + x1] - self.sum[y1 * s + x0] + self.sum[y0 * s + x0]
    }

    fn rect_avg(&self, x0: usize, y0: usize, w: usize, h: usize) -> f64 {
        let sw = w.min(self.width.saturating_sub(x0));
        let sh = h.min(self.height.saturating_sub(y0));
        if sw == 0 || sh == 0 {
            return 0.0;
        }
        (self.rect_sum(x0, y0, sw, sh) / (sw * sh) as f32) as f64
    }
}

#[derive(Clone)]
pub(crate) struct AlignedFrameCache {
    y_sum: IntegralImageF32,
    u_sum: IntegralImageF32,
    v_sum: IntegralImageF32,
}

#[derive(Clone)]
pub(crate) struct AlignedLumaCache {
    y_sum: IntegralImageF32,
}

impl AlignedFrameCache {
    pub(crate) fn build(frame: &Yuv420Frame, geom: Geom) -> Self {
        // Current Geom is axis-aligned scale + translation; exploit that to avoid per-pixel matrix multiplies.
        let cx = frame.width as f64 * 0.5;
        let cy = frame.height as f64 * 0.5;
        let x_scale = geom.sx;
        let y_scale = geom.sy;
        let x_off = cx + geom.dx - geom.sx * cx;
        let y_off = cy + geom.dy - geom.sy * cy;

        let y_sum = integral_from_resampled_axis_aligned_u8(
            &frame.y,
            frame.width,
            frame.height,
            frame.width,
            frame.height,
            x_scale,
            x_off,
            y_scale,
            y_off,
        );
        let uv_w = frame.width / 2;
        let uv_h = frame.height / 2;
        let uv_x_off = x_off * 0.5;
        let uv_y_off = y_off * 0.5;
        let u_sum = integral_from_resampled_axis_aligned_u8(
            &frame.u,
            uv_w,
            uv_h,
            uv_w,
            uv_h,
            x_scale,
            uv_x_off,
            y_scale,
            uv_y_off,
        );
        let v_sum = integral_from_resampled_axis_aligned_u8(
            &frame.v,
            uv_w,
            uv_h,
            uv_w,
            uv_h,
            x_scale,
            uv_x_off,
            y_scale,
            uv_y_off,
        );

        Self { y_sum, u_sum, v_sum }
    }

    fn avg_y_block(&self, bx: usize, by: usize) -> f64 {
        self.y_sum.rect_avg(bx * Y_BLOCK, by * Y_BLOCK, Y_BLOCK, Y_BLOCK)
    }

    fn avg_uv_block(&self, plane_sum: &IntegralImageF32, bx: usize, by: usize) -> f64 {
        let c_block = Y_BLOCK / 2;
        plane_sum.rect_avg(bx * c_block, by * c_block, c_block, c_block)
    }

    pub(crate) fn avg_symbol_triplet(&self, bx: usize, by: usize) -> (f64, f64, f64) {
        (
            self.avg_y_block(bx, by),
            self.avg_uv_block(&self.u_sum, bx, by),
            self.avg_uv_block(&self.v_sum, bx, by),
        )
    }
}

impl AlignedLumaCache {
    pub(crate) fn build(frame: &Yuv420Frame, geom: Geom) -> Self {
        let cx = frame.width as f64 * 0.5;
        let cy = frame.height as f64 * 0.5;
        let x_scale = geom.sx;
        let y_scale = geom.sy;
        let x_off = cx + geom.dx - geom.sx * cx;
        let y_off = cy + geom.dy - geom.sy * cy;
        let y_sum = integral_from_resampled_axis_aligned_u8(
            &frame.y,
            frame.width,
            frame.height,
            frame.width,
            frame.height,
            x_scale,
            x_off,
            y_scale,
            y_off,
        );
        Self { y_sum }
    }

    pub(crate) fn avg_y_block(&self, bx: usize, by: usize) -> f64 {
        self.y_sum.rect_avg(bx * Y_BLOCK, by * Y_BLOCK, Y_BLOCK, Y_BLOCK)
    }
}

fn avg_y_block_shift(y: &[u8], width: usize, height: usize, block: usize, bx: usize, by: usize, dx: f64, dy: f64) -> f64 {
    if width < block || height < block {
        return 0.0;
    }
    let x0 = bx * block;
    let y0 = by * block;
    let mut sum = 0.0f64;
    for yy in 0..block {
        for xx in 0..block {
            sum += sample_plane_bilinear(
                y,
                width,
                height,
                x0 as f64 + xx as f64 + dx,
                y0 as f64 + yy as f64 + dy,
            );
        }
    }
    sum / (block * block) as f64
}

pub(crate) fn avg_uv_block_420_geom(
    plane: &[u8],
    width: usize,
    height: usize,
    y_block: usize,
    bx: usize,
    by: usize,
    geom: Geom,
) -> f64 {
    let x_center_nom = bx as f64 * y_block as f64 + (y_block as f64 * 0.5);
    let y_center_nom = by as f64 * y_block as f64 + (y_block as f64 * 0.5);
    let (x_center, y_center) = geom_map_center_px(width, height, x_center_nom, y_center_nom, geom);
    let dx = x_center - x_center_nom;
    let dy = y_center - y_center_nom;
    avg_uv_block_420_shift(plane, width, height, y_block, bx, by, dx, dy)
}

fn avg_uv_block_420_shift(
    plane: &[u8],
    width: usize,
    height: usize,
    y_block: usize,
    bx: usize,
    by: usize,
    dx: f64,
    dy: f64,
) -> f64 {
    let uv_w = width / 2;
    let uv_h = height / 2;
    let c_block = y_block / 2;
    if uv_w < c_block || uv_h < c_block {
        return 0.0;
    }
    let dx_uv = dx * 0.5;
    let dy_uv = dy * 0.5;
    let x0 = (bx * y_block) as f64 * 0.5;
    let y0 = (by * y_block) as f64 * 0.5;
    let mut sum = 0.0f64;
    for yy in 0..c_block {
        for xx in 0..c_block {
            sum += sample_plane_bilinear(
                plane,
                uv_w,
                uv_h,
                x0 + xx as f64 + dx_uv,
                y0 + yy as f64 + dy_uv,
            );
        }
    }
    sum / (c_block * c_block) as f64
}
