//! CSS timing curves, for tweens that match the Electron app's transitions.

/// `cubic-bezier(x1, y1, x2, y2)` at time `t` (0–1), solved for x by bisection.
pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    let bez = |a: f32, b: f32, u: f32| 3. * a * u * (1. - u) * (1. - u) + 3. * b * u * u * (1. - u) + u * u * u;
    let (mut lo, mut hi) = (0f32, 1f32);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.;
        if bez(x1, x2, mid) < t { lo = mid } else { hi = mid }
    }
    bez(y1, y2, (lo + hi) / 2.)
}

/// CSS `ease`.
pub fn ease(t: f32) -> f32 {
    cubic_bezier(0.25, 0.1, 0.25, 1., t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves() {
        assert!(ease(0.).abs() < 1e-4 && (ease(1.) - 1.).abs() < 1e-4);
        // CSS `ease` at the halfway point is ≈ 0.8024.
        assert!((ease(0.5) - 0.8024).abs() < 2e-3, "{}", ease(0.5));
        // The focus-mode curve shoots out fast and settles.
        assert!(cubic_bezier(0.22, 1., 0.36, 1., 0.3) > 0.8);
    }
}
