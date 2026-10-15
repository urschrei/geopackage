//! Property tests over the envelopes of circular arcs.
//!
//! `arc_envelope` contains its three control points exactly, since they are
//! added to the box as they are. The arc's own extent is checked against arcs
//! built from a known circle: a centre, a radius, a start angle and a sweep.
//! The control points are those of the circle rounded to `f64`, and the
//! function recovers the circle from them, so the comparison with the arc
//! allows a relative error of `1e-9` of the circle's scale. That allowance is
//! the precision of recovering a circle from three rounded points, not a
//! margin for a wrong box: a box that misses a bulge misses it by a
//! proportion of the radius.

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use geopackage_core::curve::{arc_envelope, scan};
use hegel::generators;

/// A circle and a sweep along it, from which arcs are built.
#[derive(Debug, Clone, Copy)]
struct Arc {
    cx: f64,
    cy: f64,
    radius: f64,
    start: f64,
    sweep: f64,
}

impl Arc {
    fn point(&self, fraction: f64) -> [f64; 2] {
        let angle = self.start + self.sweep * fraction;
        [
            self.cx + self.radius * angle.cos(),
            self.cy + self.radius * angle.sin(),
        ]
    }

    /// The arc's exact box: its endpoints and each axis extreme of the circle
    /// that the sweep passes.
    fn exact_box(&self) -> [f64; 4] {
        let mut points = vec![self.point(0.0), self.point(1.0)];
        for k in -8_i32..=8 {
            let axis = f64::from(k) * FRAC_PI_2;
            let fraction = (axis - self.start) / self.sweep;
            if (0.0..=1.0).contains(&fraction) {
                points.push(self.point(fraction));
            }
        }
        bounds(&points)
    }

    /// The circle's scale, which bounds the error of recovering it.
    fn scale(&self) -> f64 {
        self.cx.abs().max(self.cy.abs()) + self.radius
    }
}

fn bounds(points: &[[f64; 2]]) -> [f64; 4] {
    points.iter().fold(
        [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ],
        |[min_x, max_x, min_y, max_y], [x, y]| {
            [min_x.min(*x), max_x.max(*x), min_y.min(*y), max_y.max(*y)]
        },
    )
}

/// Draws an arc: a centre within 1000 of the origin, a radius from 1e-3 to
/// 1e7, any start angle, and a sweep in either direction of between a degree
/// and just under a full turn. With the centre that close, the allowance of
/// `1e-9` of the scale is at most `1e-3` of the radius, far below the bulge of
/// an arc past an axis extreme.
fn draw_arc(tc: &hegel::TestCase) -> Arc {
    let coordinate = || tc.draw(generators::floats::<f64>().min_value(-1e3).max_value(1e3));
    let exponent = tc.draw(generators::integers::<i32>().min_value(-3).max_value(6));
    let mantissa = tc.draw(generators::floats::<f64>().min_value(1.0).max_value(10.0));
    let sweep = tc.draw(
        generators::floats::<f64>()
            .min_value(PI / 180.0)
            .max_value(TAU - PI / 180.0),
    );
    Arc {
        cx: coordinate(),
        cy: coordinate(),
        radius: mantissa * 10_f64.powi(exponent),
        start: tc.draw(generators::floats::<f64>().min_value(-TAU).max_value(TAU)),
        sweep: if tc.draw(generators::booleans()) {
            sweep
        } else {
            -sweep
        },
    }
}

/// Asserts that `actual` is `expected` to within `tolerance` on each edge.
fn assert_box_near(actual: [f64; 4], expected: [f64; 4], tolerance: f64, context: &str) {
    for (a, e) in actual.iter().zip(expected) {
        assert!(
            (a - e).abs() <= tolerance,
            "{context}: {actual:?} is not {expected:?}"
        );
    }
}

/// The box `arc_envelope` returns contains its three control points, for
/// any points, NaN and the infinities included, and is `None` exactly when
/// no control point is finite.
#[hegel::test]
fn arc_envelope_contains_its_control_points(tc: hegel::TestCase) {
    let point = || {
        [
            tc.draw(generators::floats::<f64>()),
            tc.draw(generators::floats::<f64>()),
        ]
    };
    let (p0, p1, p2) = (point(), point(), point());
    let finite: Vec<[f64; 2]> = [p0, p1, p2]
        .into_iter()
        .filter(|[x, y]| x.is_finite() && y.is_finite())
        .collect();
    match arc_envelope(p0, p1, p2) {
        None => assert!(finite.is_empty(), "{p0:?} {p1:?} {p2:?}"),
        Some([min_x, max_x, min_y, max_y]) => {
            assert!(!finite.is_empty());
            for [x, y] in finite {
                assert!(
                    min_x <= x && x <= max_x && min_y <= y && y <= max_y,
                    "({x}, {y}) is outside {:?}",
                    [min_x, max_x, min_y, max_y]
                );
            }
        }
    }
}

/// `arc_envelope` of three points on a circle is the box of the arc from the
/// first through the second to the third: its endpoints and every axis
/// extreme of the circle that the arc passes.
#[hegel::test]
fn arc_envelope_is_the_box_of_the_arc(tc: hegel::TestCase) {
    let arc = draw_arc(&tc);
    let (p0, p1, p2) = (arc.point(0.0), arc.point(0.5), arc.point(1.0));
    let actual = arc_envelope(p0, p1, p2).unwrap_or_else(|| panic!("{arc:?}"));
    let control = bounds(&[p0, p1, p2]);
    let tolerance = 1e-9 * arc.scale();
    tc.event(
        if arc
            .exact_box()
            .iter()
            .zip(control)
            .any(|(e, c)| (e - c).abs() > tolerance)
        {
            "the arc bulges past its control points"
        } else {
            "the control points bound the arc"
        },
    );
    assert_box_near(
        actual,
        arc.exact_box(),
        1e-9 * arc.scale(),
        &format!("{arc:?}"),
    );
}

/// Writes a little-endian `CIRCULARSTRING` body through the points.
fn circular_string(points: &[[f64; 2]]) -> Vec<u8> {
    let mut body = vec![1];
    body.extend_from_slice(&8_u32.to_le_bytes());
    body.extend_from_slice(&u32::try_from(points.len()).unwrap_or(0).to_le_bytes());
    for [x, y] in points {
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
    }
    body
}

/// The envelope `scan` gives a `CIRCULARSTRING` of several arcs is the box of
/// all of them: each arc contributes its bulges, not only its control points.
#[hegel::test]
fn a_circular_string_envelope_covers_every_arc(tc: hegel::TestCase) {
    let arcs = tc.draw(generators::integers::<usize>().min_value(1).max_value(3));
    let first = draw_arc(&tc);
    let mut points = vec![first.point(0.0)];
    let mut boxes = Vec::new();
    let mut scale: f64 = 0.0;
    let mut current = first;
    for _ in 0..arcs {
        points.push(current.point(0.5));
        points.push(current.point(1.0));
        let [min_x, max_x, min_y, max_y] = current.exact_box();
        boxes.push([min_x, min_y]);
        boxes.push([max_x, max_y]);
        scale = scale.max(current.scale());
        // The next arc starts where this one ends, on a circle of its own.
        let next = draw_arc(&tc);
        let end = current.point(1.0);
        current = Arc {
            cx: end[0] - next.radius * next.start.cos(),
            cy: end[1] - next.radius * next.start.sin(),
            ..next
        };
    }
    let scanned = scan(&circular_string(&points)).unwrap();
    let actual = scanned.xy_envelope.unwrap_or_else(|| panic!("{points:?}"));
    assert_box_near(actual, bounds(&boxes), 1e-9 * scale, &format!("{points:?}"));
}
