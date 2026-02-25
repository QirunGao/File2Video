use nalgebra::{Matrix3, Vector3};

#[derive(Clone, Debug)]
pub(crate) struct Affine2 {
    mat: Matrix3<f64>,
}

impl Affine2 {
    fn from_rows(m00: f64, m01: f64, m02: f64, m10: f64, m11: f64, m12: f64) -> Self {
        Self {
            mat: Matrix3::new(
                m00, m01, m02,
                m10, m11, m12,
                0.0, 0.0, 1.0,
            ),
        }
    }

    pub(crate) fn map(&self, x: f64, y: f64) -> (f64, f64) {
        let p = self.mat * Vector3::new(x, y, 1.0);
        (p[0], p[1])
    }

    pub(crate) fn invert(&self) -> Option<Self> {
        let inv = self.mat.try_inverse()?;
        Some(Self { mat: inv })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Geom {
    pub(crate) dx: f64,
    pub(crate) dy: f64,
    pub(crate) sx: f64,
    pub(crate) sy: f64,
}

impl Default for Geom {
    fn default() -> Self {
        Self::identity()
    }
}

impl Geom {
    pub(crate) fn identity() -> Self {
        Self {
            dx: 0.0,
            dy: 0.0,
            sx: 1.0,
            sy: 1.0,
        }
    }

    pub(crate) fn affine_for_frame(&self, width: usize, height: usize) -> Affine2 {
        let cx = width as f64 * 0.5;
        let cy = height as f64 * 0.5;
        let a00 = self.sx;
        let a01 = 0.0;
        let a10 = 0.0;
        let a11 = self.sy;
        let tx = cx + self.dx - (a00 * cx + a01 * cy);
        let ty = cy + self.dy - (a10 * cx + a11 * cy);
        let aff = Affine2::from_rows(a00, a01, tx, a10, a11, ty);
        debug_assert!(aff.invert().is_some());
        aff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "left={} right={}", a, b);
    }

    #[test]
    fn affine_inverse_round_trip() {
        let g = Geom {
            dx: 3.25,
            dy: -4.5,
            sx: 1.07,
            sy: 0.93,
        };
        let a = g.affine_for_frame(1920, 1080);
        let ai = a.invert().expect("invertible");
        let pts = [(0.0, 0.0), (123.4, 567.8), (1919.0, 1079.0), (960.0, 540.0)];
        for (x, y) in pts {
            let (u, v) = a.map(x, y);
            let (xr, yr) = ai.map(u, v);
            approx(x, xr);
            approx(y, yr);
        }
    }

    #[test]
    fn anisotropic_scale_is_about_frame_center() {
        let mut g = Geom::identity();
        g.sx = 2.0;
        g.sy = 0.5;
        let a = g.affine_for_frame(100, 80);
        let (cx, cy) = (50.0, 40.0);
        let (x1, y1) = a.map(cx + 10.0, cy);
        let (x2, y2) = a.map(cx, cy + 10.0);
        approx(x1 - cx, 20.0);
        approx(y1 - cy, 0.0);
        approx(x2 - cx, 0.0);
        approx(y2 - cy, 5.0);
    }
}
