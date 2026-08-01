use crate::BoardFrame;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine2 {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

/// Authoritative Pixi stage transform captured in ReplayIR.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoardTransform {
    pub zoom: f64,
    pub position: [f64; 2],
    pub pivot: [f64; 2],
}

impl From<&BoardFrame> for BoardTransform {
    fn from(frame: &BoardFrame) -> Self {
        Self {
            zoom: frame.zoom,
            position: [frame.x, frame.y],
            pivot: [frame.pivot_x, frame.pivot_y],
        }
    }
}

impl BoardTransform {
    /// Match Pixi's `position + (local - pivot) * scale` in output pixels.
    pub fn point(self, local: [f64; 2]) -> [f32; 2] {
        [
            (self.position[0] + (local[0] - self.pivot[0]) * self.zoom) as f32,
            (self.position[1] + (local[1] - self.pivot[1]) * self.zoom) as f32,
        ]
    }

    pub fn extent(self, local: [f64; 2]) -> [f32; 2] {
        [(local[0] * self.zoom) as f32, (local[1] * self.zoom) as f32]
    }

    pub fn affine(self) -> Affine2 {
        Affine2 {
            a: self.zoom,
            b: 0.0,
            c: 0.0,
            d: self.zoom,
            tx: self.position[0] - self.pivot[0] * self.zoom,
            ty: self.position[1] - self.pivot[1] * self.zoom,
        }
    }
}

impl Affine2 {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub fn from_components(
        position: [f64; 2],
        scale: [f64; 2],
        rotation: f64,
        pivot: [f64; 2],
    ) -> Self {
        let cosine = rotation.cos();
        let sine = rotation.sin();
        let a = cosine * scale[0];
        let b = sine * scale[0];
        let c = -sine * scale[1];
        let d = cosine * scale[1];
        Self {
            a,
            b,
            c,
            d,
            tx: position[0] - (pivot[0] * a + pivot[1] * c),
            ty: position[1] - (pivot[0] * b + pivot[1] * d),
        }
    }

    pub fn then(self, child: Self) -> Self {
        Self {
            a: self.a * child.a + self.c * child.b,
            b: self.b * child.a + self.d * child.b,
            c: self.a * child.c + self.c * child.d,
            d: self.b * child.c + self.d * child.d,
            tx: self.a * child.tx + self.c * child.ty + self.tx,
            ty: self.b * child.tx + self.d * child.ty + self.ty,
        }
    }

    pub fn point(self, point: [f64; 2]) -> [f64; 2] {
        [
            self.a * point[0] + self.c * point[1] + self.tx,
            self.b * point[0] + self.d * point[1] + self.ty,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::{Affine2, BoardTransform};

    #[test]
    fn maps_canonical_full_board_bounds_to_captured_pixel_bounds() {
        let transform = BoardTransform {
            zoom: 1.5,
            position: [64.0, 32.0],
            pivot: [-50.0, -50.0],
        };
        assert_eq!(transform.point([-50.0, -50.0]), [64.0, 32.0]);
        assert_eq!(transform.point([150.0, 50.0]), [364.0, 182.0]);
        assert_eq!(transform.extent([100.0, 50.0]), [150.0, 75.0]);
    }

    #[test]
    fn composes_nested_nonuniform_rotations_without_losing_shear() {
        let parent = Affine2::from_components(
            [10.0, 20.0],
            [2.0, 1.0],
            std::f64::consts::FRAC_PI_2,
            [0.0, 0.0],
        );
        let child = Affine2::from_components(
            [5.0, 0.0],
            [1.0, 3.0],
            std::f64::consts::FRAC_PI_4,
            [0.0, 0.0],
        );
        let combined = parent.then(child);
        let nested = parent.point(child.point([2.0, 4.0]));
        let direct = combined.point([2.0, 4.0]);
        assert!((nested[0] - direct[0]).abs() < 1e-12);
        assert!((nested[1] - direct[1]).abs() < 1e-12);
        assert_ne!(combined.a * combined.c + combined.b * combined.d, 0.0);
    }
}
