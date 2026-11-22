# Liquid Glass Refraction Core Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the 64-ring radial lens with one CPU rounded-rect SDF warp so corner refraction follows the true surface normal.

**Architecture:** `glass_optics.rs` holds the pure SDF/bevel/Snell math and one resampling pass. `wallpaper::RawGlass` carries the raw crop and warps on demand; the live capture thread warps each new frame, and the view owns a coalescing warp queue that re-warps the newest raw whenever only the shape changed. `glass::lens` paints one already-warped image.

**Tech Stack:** Rust 2024, GPUI (`gpui-kit` 0.8), `windows-capture` 1.5, `image` 0.25. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-22-liquid-glass-refraction-core-design.md`

**Repo conventions:** binary modules in `src/`, library is pure decisions in `rustedge` (`src/lib.rs`). Lints deny `unwrap/expect/panic/indexing_slicing` — use `get(...).copied().unwrap_or(...)`, `let [...] = *chunk else { .. }`, and `chunks_exact_mut`. Tests live in `#[cfg(test)] mod tests` at the bottom of each file. Commit messages are lowercase imperative (`feat:`, `fix:`, `docs:`).

**Deviations from the spec (deliberate):**
- `shell_offset(stage_width, shape_width)` is not introduced; `glass_optics::crop_rects` *is* the shared centering formula, and both the view and `shell_rect` use it.
- `GlassShape::From<island::Morph>` lives in `glass_optics` (not the view) so tests and `glass.rs` share it.
- `CropSpec::crop` already arrives during the pivot task (Task 9) because `GlassBackdrop` needs it there; `CropGeometry`/`shape` split lands in Task 11.

---

## File structure

| File | Responsibility |
| --- | --- |
| `src/glass_optics.rs` (new) | Pure optics: SDF, normal, bevel, Snell LUT, per-pixel displacement, one `warp` resample pass, geometry conversions (`Shell`, `Rect`, `Margins`, `GlassShape`). No GPUI. |
| `src/wallpaper.rs` | `RawGlass` (raw RGBA + geometry + `warp`), `GlassBackdrop` (warped, paintable), `build_raw`, `crop_rect`, `REFRACTION_MARGIN = 48`. |
| `src/live_glass.rs` | `CropSpec { geometry, shape }`, `crop_spec`, `update_crop -> bool`, `Frame { raw, warped, generation, luma }`, `Sink` with a generation counter, `on_frame_arrived` warps with `spec.shape`. |
| `src/island_view/warp_queue.rs` (new) | Pure coalescing queue: pending + in-flight + failed-marker + ticket. No GPUI, fully unit-tested. |
| `src/island_view/mod.rs` | `poll_warp` on the 32 ms loop, warp queue ownership and install rules, `poll_live` frame consumption, `poll_glass` → `build_raw`, `glass_luma` field. |
| `src/island_view/glass.rs` | One-draw `lens` (shell clip + crop natural rect, stale-shape axial scale), rim without dispersion rings. |
| `src/main.rs` | `mod glass_optics;` + doc update. |
| `ARCHITECTURE.md` | Glass pipeline description. |

---

## Chunk 1: pure optics (`glass_optics.rs`)

### Task 1: Module skeleton + rounded-rect SDF

**Files:**
- Create: `src/glass_optics.rs`
- Modify: `src/main.rs` (add `mod glass_optics;` next to `mod live_glass;`)

- [ ] **Step 1: Write the failing tests**

Create `src/glass_optics.rs` with the module doc, the four types, and an empty `sd_round_rect` stub (`pub fn sd_round_rect(_p: (f32, f32), _half: (f32, f32), _radius: f32) -> f32 { 0.0 }`), plus:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert the precondition themselves")]
mod tests {
    use super::*;

    const HALF: (f32, f32) = (210.0, 144.0); // the 420x288 card
    const RADIUS: f32 = 24.0;

    fn near(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-3, "{a} != {b}");
    }

    #[test]
    fn the_shape_takes_only_morph_geometry() {
        let morph = rustedge::island::Morph {
            width: 320.0,
            height: 56.0,
            radius: 28.0,
            shadow: 12.0,
            detail: 0.5,
        };
        let shape = GlassShape::from(morph);
        near(shape.width, 320.0);
        near(shape.height, 56.0);
        near(shape.radius, 28.0);
    }
```
    #[test]
    fn sdf_is_zero_on_the_edge_negative_inside_positive_outside() {
        near(sd_round_rect((0.0, -144.0), HALF, RADIUS), 0.0);
        near(sd_round_rect((210.0, 0.0), HALF, RADIUS), 0.0);
        assert!(sd_round_rect((0.0, -100.0), HALF, RADIUS) < 0.0);
        assert!(sd_round_rect((0.0, -200.0), HALF, RADIUS) > 0.0);
    }

    #[test]
    fn sdf_corner_distance_is_radial_from_the_corner_center() {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let cx = HALF.0 - RADIUS;
        let cy = HALF.1 - RADIUS;
        let on_arc = (cx + RADIUS * s, -(cy + RADIUS * s));
        assert!(sd_round_rect(on_arc, HALF, RADIUS).abs() < 1e-3);
        let out = (on_arc.0 + 8.0 * s, on_arc.1 - 8.0 * s);
        near(sd_round_rect(out, HALF, RADIUS), 8.0);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test glass_optics`
Expected: both tests FAIL by assertion (the stub returns 0.0).

- [ ] **Step 3: Implement**

```rust
//! Pure liquid-glass optics: a rounded-rect SDF, a bevel profile bent by
//! Snell, and one CPU resampling pass that displaces pixels along the SDF
//! normal.
//!
//! `wallpaper::RawGlass` warps here before the image reaches GPUI, so the
//! lens paints once and the UI thread never resamples. Nothing in this
//! module touches GPUI or the window.

/// A logical shell shape: the island's width, height, and corner radius.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlassShape {
    /// Interpolated width, logical pixels.
    pub width: f32,
    /// Interpolated height, logical pixels.
    pub height: f32,
    /// Corner radius, logical pixels.
    pub radius: f32,
}

impl GlassShape {
    /// "No warp baked": `lens` treats a backdrop with this shape as an
    /// unwarped crop.
    pub const ZERO: Self = Self { width: 0.0, height: 0.0, radius: 0.0 };
}

impl From<rustedge::island::Morph> for GlassShape {
    fn from(morph: rustedge::island::Morph) -> Self {
        Self { width: morph.width, height: morph.height, radius: morph.radius }
    }
}

/// A shell rect in image pixels, relative to the crop's top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shell {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub radius: f32,
}

impl Shell {
    fn center(&self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn half(&self) -> (f32, f32) {
        (self.width / 2.0, self.height / 2.0)
    }
}

/// Available sampling distance from the shell edge to the image edge, in
/// image pixels. `warp` derives it from the shell and the image bounds, so a
/// centered capsule may still sample the stage interior beside it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Margins {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// An axis-aligned rect in whatever frame the caller uses: image pixels for
/// crops, logical pixels for the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    /// The all-zero rect, for `CropSpec::EMPTY` and "no warp" sentinels.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
}

/// Signed distance to a rounded rectangle: negative inside, zero on the
/// edge, positive outside.
#[must_use]
pub fn sd_round_rect(p: (f32, f32), half: (f32, f32), radius: f32) -> f32 {
    let qx = p.0.abs() - half.0 + radius;
    let qy = p.1.abs() - half.1 + radius;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - radius
}
```

Register the module in `src/main.rs` next to `mod live_glass;`:

```rust
mod glass_optics;
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test glass_optics`
Expected: 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/glass_optics.rs src/main.rs
git commit -m "feat: add the glass optics module with a rounded-rect sdf"
```

### Task 2: SDF normal

**Files:**
- Modify: `src/glass_optics.rs`

- [ ] **Step 1: Write the failing tests** (append to `mod tests`)

```rust
    #[test]
    fn normals_point_straight_out_on_edges_and_diagonally_at_corners() {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let top = sdf_normal((0.0, -144.0), HALF, RADIUS).unwrap();
        near(top.0, 0.0);
        near(top.1, -1.0);
        let right = sdf_normal((210.0, 0.0), HALF, RADIUS).unwrap();
        near(right.0, 1.0);
        near(right.1, 0.0);
        let cx = HALF.0 - RADIUS;
        let cy = HALF.1 - RADIUS;
        let corner = sdf_normal(
            (cx + RADIUS * s, -(cy + RADIUS * s)),
            HALF,
            RADIUS,
        )
        .unwrap();
        near(corner.0, s);
        near(corner.1, -s);
    }

    #[test]
    fn the_top_tangent_keeps_a_vertical_normal() {
        // Regression: the old radial lens tilted this point ~52°.
        let tangent = sdf_normal((HALF.0 - RADIUS, -HALF.1), HALF, RADIUS).unwrap();
        let tilt = tangent.0.abs().atan2(tangent.1.abs()).to_degrees();
        assert!(tilt < 5.0, "tangent normal tilted {tilt}° from vertical");
    }

    #[test]
    fn a_thin_capsule_medial_axis_has_no_normal() {
        let half = (160.0, 6.0);
        assert!(sdf_normal((0.0, 0.0), half, 6.0).is_none());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test glass_optics`
Expected: compile error — `sdf_normal` not found.

- [ ] **Step 3: Implement**

```rust
/// Unit *outward* normal of the SDF at `p`, or `None` on the medial axis
/// where the central difference collapses.
#[must_use]
pub fn sdf_normal(p: (f32, f32), half: (f32, f32), radius: f32) -> Option<(f32, f32)> {
    const EPS: f32 = 1.0;
    let gx = sd_round_rect((p.0 + EPS, p.1), half, radius)
        - sd_round_rect((p.0 - EPS, p.1), half, radius);
    let gy = sd_round_rect((p.0, p.1 + EPS), half, radius)
        - sd_round_rect((p.0, p.1 - EPS), half, radius);
    let len = (gx * gx + gy * gy).sqrt();
    if len < 0.1 {
        None
    } else {
        Some((gx / len, gy / len))
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test glass_optics`
Expected: 6 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/glass_optics.rs
git commit -m "feat: add the sdf outward normal with a medial-axis guard"
```

### Task 3: Bevel width

**Files:**
- Modify: `src/glass_optics.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn bevel_clamps_by_shape_size() {
        let card = GlassShape { width: 420.0, height: 288.0, radius: 24.0 };
        near(bevel_width(card), 36.0);
        let capsule = GlassShape { width: 320.0, height: 56.0, radius: 28.0 };
        assert!((bevel_width(capsule) - 10.08).abs() < 1e-3);
        let thin = GlassShape { width: 100.0, height: 12.0, radius: 6.0 };
        near(bevel_width(thin), 6.0);
        let tiny = GlassShape { width: 8.0, height: 8.0, radius: 4.0 };
        near(bevel_width(tiny), 4.0);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test glass_optics`
Expected: compile error — `bevel_width` not found.

- [ ] **Step 3: Implement**

```rust
/// Bevel band width for one shape, in logical pixels. Grows with the shorter
/// side, clamps to a usable band, and never crosses the midline.
#[must_use]
pub fn bevel_width(shape: GlassShape) -> f32 {
    let min = shape.width.min(shape.height);
    (min * 0.18).clamp(10.0, 36.0).min(min / 2.0)
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test glass_optics`
Expected: 7 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/glass_optics.rs
git commit -m "feat: add the bevel band width"
```

### Task 4: Snell bend profile

**Files:**
- Modify: `src/glass_optics.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn snell_bend_matches_known_slopes() {
        near(snell_bend(0.0), 0.0);
        // slope 1: tan(45° - asin(sin45°/1.5)) ≈ 0.3034
        assert!((snell_bend(1.0) - 0.3034).abs() < 2e-3);
        // a vertical wall bends hardest: sqrt(n²-1)
        near(snell_bend(f32::INFINITY), (1.5f32 * 1.5 - 1.0).sqrt());
    }

    #[test]
    fn bend_runs_from_one_to_zero_and_falls_monotonically() {
        let lut = bend_lut();
        near(lut[0], 1.0);
        near(lut[63], 0.0);
        for pair in lut.windows(2) {
            assert!(pair[0] >= pair[1], "bend must not grow inward: {pair:?}");
        }
    }

    #[test]
    fn corner_term_peaks_at_45_degrees() {
        near(corner_term((0.0, -1.0)), 0.0);
        near(corner_term((1.0, 0.0)), 0.0);
        let s = std::f32::consts::FRAC_1_SQRT_2;
        near(corner_term((s, -s)), 1.0);
        near(corner_term((0.5, -0.866_025_4)), 0.75);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test glass_optics`
Expected: compile error — `snell_bend`, `bend_lut`, `corner_term` not found.

- [ ] **Step 3: Implement**

```rust
/// LUT resolution for the bend profile. 64 entries are far denser than the
/// band reads.
const BEND_STEPS: usize = 64;
/// Superellipse exponent of the bevel height profile.
const BEVEL_POWER: f32 = 4.0;
/// Glass index of refraction, air → glass.
const REFRACTIVE_INDEX: f32 = 1.5;
/// Fraction of the bevel where the inner-edge taper starts.
const TAPER_START: f32 = 0.75;
/// Extra bend at the 45° corner, on top of the base Snell bend.
const CORNER_BOOST: f32 = 0.30;

/// `tan(θi - θt)` for a surface slope `dh/dd`, air → glass. A vertical wall
/// (`INFINITY`) saturates at `sqrt(n²-1)`.
fn snell_bend(slope: f32) -> f32 {
    let theta_i = slope.atan();
    let sin_t = (theta_i.sin() / REFRACTIVE_INDEX).clamp(-1.0, 1.0);
    (theta_i - sin_t.asin()).tan()
}

fn index_limit() -> f32 {
    (REFRACTIVE_INDEX * REFRACTIVE_INDEX - 1.0).sqrt()
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Normalized Snell bend at bevel progress `e` (0 at the silhouette, 1 at
/// the inner edge).
///
/// The height profile is `h(e) = bevel * (1-(1-e)^p)^(1/p)` with the height
/// equal to the bevel width, so `dh/dd` cancels the bevel and the curve is
/// shape-independent. The raw bend is divided by its edge limit and tapered
/// to zero at the inner edge for a C1 handoff to the flat center.
#[must_use]
pub fn bend_at(e: f32) -> f32 {
    let e = e.clamp(0.0, 1.0);
    if e >= 1.0 {
        return 0.0;
    }
    let inward = 1.0 - e;
    // dh/dd = (1-e)^(p-1) * (1-(1-e)^p)^(1/p - 1); e = 0 is a vertical wall.
    let base = 1.0 - inward.powf(BEVEL_POWER);
    let slope = if base <= 0.0 {
        f32::INFINITY
    } else {
        inward.powf(BEVEL_POWER - 1.0) * base.powf(1.0 / BEVEL_POWER - 1.0)
    };
    let bend = snell_bend(slope) / index_limit();
    bend * (1.0 - smoothstep(TAPER_START, 1.0, e))
}

/// The bend profile sampled once per warp pass.
#[must_use]
pub fn bend_lut() -> [f32; BEND_STEPS] {
    let mut lut = [0.0; BEND_STEPS];
    for (step, slot) in lut.iter_mut().enumerate() {
        #[allow(
            clippy::cast_precision_loss,
            reason = "BEND_STEPS is 64; the quotient is exact in f32"
        )]
        let e = step as f32 / (BEND_STEPS - 1) as f32;
        *slot = bend_at(e);
    }
    lut
}

/// Corner emphasis: `sin²(2θ)` — 0 on straight edges, 1 at the 45° point.
#[must_use]
pub fn corner_term(n: (f32, f32)) -> f32 {
    4.0 * n.0 * n.0 * n.1 * n.1
}
```

Also add `pub(crate) const BEND_STEPS: usize = 64;` visibility as shown (`const BEND_STEPS` is fine private; tests use `bend_lut()`). Keep it private.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test glass_optics`
Expected: 10 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/glass_optics.rs
git commit -m "feat: add the snell bend profile and corner emphasis"
```

### Task 5: Per-pixel displacement

**Files:**
- Modify: `src/glass_optics.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    fn card_shell() -> Shell {
        Shell { x: 48.0, y: 48.0, width: 420.0, height: 288.0, radius: 24.0 }
    }

    fn even_margins() -> Margins {
        Margins { left: 48.0, top: 48.0, right: 48.0, bottom: 48.0 }
    }

    fn top_left_45(shell: Shell) -> (f32, f32) {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let cx = shell.x + shell.width / 2.0;
        let cy = shell.y + shell.height / 2.0;
        let kx = cx - shell.width / 2.0 + shell.radius;
        let ky = cy - shell.height / 2.0 + shell.radius;
        (kx - shell.radius * s, ky - shell.radius * s)
    }

    #[test]
    fn the_top_edge_pulls_straight_up_by_the_depth() {
        let shell = card_shell();
        let offset = displacement(&bend_lut(), (shell.x + 210.0, shell.y), shell, 36.0, even_margins())
            .unwrap();
        near(offset.0, 0.0);
        near(offset.1, -(0.8 * 36.0));
    }

    #[test]
    fn the_45_degree_corner_pulls_along_the_normal_and_is_boosted() {
        let shell = card_shell();
        let (bx, by) = displacement(&bend_lut(), top_left_45(shell), shell, 36.0, even_margins())
            .unwrap();
        let len = (bx * bx + by * by).sqrt();
        near(len, 0.8 * 36.0 * 1.3);
        let s = std::f32::consts::FRAC_1_SQRT_2;
        near(bx / len, -s);
        near(by / len, -s);
    }

    #[test]
    fn the_axis_clamp_spares_the_other_component() {
        let shell = card_shell();
        let margins = Margins { left: 0.0, top: 48.0, right: 48.0, bottom: 48.0 };
        let (bx, by) = displacement(&bend_lut(), top_left_45(shell), shell, 36.0, margins).unwrap();
        near(bx, 0.0);
        assert!(by < -20.0, "the vertical bend must survive the x clamp: {by}");
    }

    #[test]
    fn outside_and_center_pixels_are_copies() {
        let shell = card_shell();
        let lut = bend_lut();
        let above = (shell.x + 210.0, shell.y - 10.0);
        assert!(displacement(&lut, above, shell, 36.0, even_margins()).is_none());
        let center = (shell.x + 210.0, shell.y + 144.0);
        assert!(displacement(&lut, center, shell, 36.0, even_margins()).is_none());
    }

    #[test]
    fn the_bend_never_reaches_past_the_margin() {
        let shell = card_shell();
        let lut = bend_lut();
        let (bx, by) = displacement(&lut, top_left_45(shell), shell, 36.0, even_margins()).unwrap();
        assert!(bx.abs() <= 48.0 && by.abs() <= 48.0);
    }

    #[test]
    fn tangent_and_midpoint_bend_alike_and_fall_off_inward() {
        let shell = card_shell();
        let lut = bend_lut();
        let tangent = displacement(&lut, (shell.x + 186.0, shell.y), shell, 36.0, even_margins())
            .unwrap();
        let midpoint = displacement(&lut, (shell.x + 210.0, shell.y), shell, 36.0, even_margins())
            .unwrap();
        let tangent_len = (tangent.0 * tangent.0 + tangent.1 * tangent.1).sqrt();
        let mid_len = (midpoint.0 * midpoint.0 + midpoint.1 * midpoint.1).sqrt();
        assert!((tangent_len - mid_len).abs() < 0.1 * mid_len);
        let inner = displacement(&lut, (shell.x + 210.0, shell.y + 4.0), shell, 36.0, even_margins())
            .unwrap();
        let inner_len = (inner.0 * inner.0 + inner.1 * inner.1).sqrt();
        assert!(inner_len < mid_len, "the bend must fall off inward");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test glass_optics`
Expected: compile error — `displacement` not found.

- [ ] **Step 3: Implement**

```rust
/// Sample offset for one output pixel, in image pixels. `None` copies the
/// source pixel: outside the shell, past the bevel, or on the medial axis.
#[must_use]
pub fn displacement(
    lut: &[f32; BEND_STEPS],
    p: (f32, f32),
    shell: Shell,
    bevel_px: f32,
    margins: Margins,
) -> Option<(f32, f32)> {
    if !(bevel_px > 0.0) {
        return None;
    }
    let center = shell.center();
    let half = shell.half();
    let local = (p.0 - center.0, p.1 - center.1);
    let d = -sd_round_rect(local, half, shell.radius);
    if !(0.0..bevel_px).contains(&d) {
        return None;
    }
    let n = sdf_normal(local, half, shell.radius)?;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "e is clamped to 0..=1 and BEND_STEPS is 64"
    )]
    let step = (e_of(d, bevel_px) * (BEND_STEPS - 1) as f32).round() as usize;
    let depth = 0.8 * bevel_px;
    let bend = lut.get(step).copied().unwrap_or(0.0) * depth
        * (1.0 + CORNER_BOOST * corner_term(n));
    Some((
        (n.0 * bend).clamp(-margins.left, margins.right),
        (n.1 * bend).clamp(-margins.top, margins.bottom),
    ))
}

fn e_of(d: f32, bevel_px: f32) -> f32 {
    (d / bevel_px).clamp(0.0, 1.0)
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test glass_optics`
Expected: 16 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/glass_optics.rs
git commit -m "feat: add the per-pixel snell displacement"
```

### Task 6: Geometry conversions

**Files:**
- Modify: `src/glass_optics.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    fn card_shape() -> GlassShape {
        GlassShape { width: 420.0, height: 288.0, radius: 24.0 }
    }

    fn capsule_shape() -> GlassShape {
        GlassShape { width: 320.0, height: 56.0, radius: 28.0 }
    }

    fn stage_crop_rect() -> Rect {
        Rect { x: -48.0, y: -48.0, width: 516.0, height: 456.0 }
    }

    #[test]
    fn shell_rect_centers_the_card_and_offsets_the_capsule() {
        let shell = shell_rect(420.0, card_shape(), stage_crop_rect(), 1.0);
        near(shell.x, 48.0);
        near(shell.y, 48.0);
        near(shell.width, 420.0);
        near(shell.radius, 24.0);
        let shell = shell_rect(420.0, capsule_shape(), stage_crop_rect(), 1.25);
        near(shell.x, (50.0 + 48.0) * 1.25);
        near(shell.width, 400.0);
    }

    #[test]
    fn an_offscreen_window_shifts_the_shell_negative() {
        // The window hangs 10 physical px off the left edge (scale 1): the
        // crop starts 10 px right of the window origin.
        let crop = Rect { x: 10.0, y: 0.0, width: 458.0, height: 408.0 };
        let shell = shell_rect(420.0, card_shape(), crop, 1.0);
        near(shell.x, -10.0);
        near(shell.y, 0.0);
    }

    #[test]
    fn crop_rects_centers_the_shell_and_returns_the_natural_crop() {
        let (shell, natural) = crop_rects(420.0, card_shape(), stage_crop_rect());
        near(shell.x, 0.0);
        near(shell.y, 0.0);
        near(shell.width, 420.0);
        near(natural.x, -48.0);
        near(natural.width, 516.0);
        let (shell, _) = crop_rects(420.0, capsule_shape(), stage_crop_rect());
        near(shell.x, 50.0);
        near(shell.y, 0.0);
    }

    #[test]
    fn stale_image_bounds_maps_the_baked_shell_onto_the_current_one() {
        let natural = stage_crop_rect();
        let baked = Rect { x: 50.0, y: 0.0, width: 320.0, height: 56.0 };
        let current = Rect { x: 0.0, y: 0.0, width: 420.0, height: 288.0 };
        let image = stale_image_bounds(natural, baked, current);
        let sx = 420.0 / 320.0;
        let sy = 288.0 / 56.0;
        near(image.x, (-48.0 - 50.0) * sx);
        assert!((image.y - (-48.0 * sy)).abs() < 0.01);
        near(image.width, 516.0 * sx);
        assert!((image.height - 456.0 * sy).abs() < 0.01);
        // Same shape: identity.
        let same = stale_image_bounds(natural, current, current);
        near(same.x, natural.x);
        near(same.width, natural.width);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test glass_optics`
Expected: compile error — `shell_rect`, `crop_rects`, `stale_image_bounds` not found.

- [ ] **Step 3: Implement**

```rust
/// The shell rect inside one crop, in image pixels.
///
/// `stage_width` is the unclipped stage width in logical pixels — the same
/// measure `crop_rects` centers against — and `crop` is the crop rect
/// relative to the window's top-left, so a window hanging off the screen
/// yields a negative `shell.x`.
#[must_use]
pub fn shell_rect(stage_width: f32, shape: GlassShape, crop: Rect, image_scale: f32) -> Shell {
    let (shell, _) = crop_rects(stage_width, shape, crop);
    Shell {
        x: (shell.x - crop.x) * image_scale,
        y: (shell.y - crop.y) * image_scale,
        width: shape.width * image_scale,
        height: shape.height * image_scale,
        radius: shape.radius * image_scale,
    }
}

/// The shell rect and the crop's natural rect, in logical pixels relative to
/// the window's top-left. The shell's top sits on the window's top edge and
/// the shell is centered horizontally; `crop` is returned unchanged.
#[must_use]
pub fn crop_rects(stage_width: f32, shape: GlassShape, crop: Rect) -> (Rect, Rect) {
    let offset = (stage_width - shape.width) / 2.0;
    let shell = Rect { x: offset, y: 0.0, width: shape.width, height: shape.height };
    (shell, crop)
}

/// Maps the crop's natural rect from the baked shell's frame onto the current
/// shell's frame, axially. `lens` uses this while a warp for the new shape is
/// still in flight, so the old bevel rides the new edge instead of ghosting
/// inside the panel. Callers must pass a non-degenerate `baked_shell`.
#[must_use]
pub fn stale_image_bounds(natural: Rect, baked_shell: Rect, current_shell: Rect) -> Rect {
    let sx = current_shell.width / baked_shell.width;
    let sy = current_shell.height / baked_shell.height;
    Rect {
        x: current_shell.x + (natural.x - baked_shell.x) * sx,
        y: current_shell.y + (natural.y - baked_shell.y) * sy,
        width: natural.width * sx,
        height: natural.height * sy,
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test glass_optics`
Expected: 20 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/glass_optics.rs
git commit -m "feat: add shell and crop geometry conversions"
```

### Task 7: The warp pass

**Files:**
- Modify: `src/glass_optics.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    fn solid(width: usize, height: usize, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            out.extend_from_slice(&rgba);
        }
        out
    }

    fn pixel(buf: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let base = (y * width + x) * 4;
        [
            buf.get(base).copied().unwrap_or(0),
            buf.get(base + 1).copied().unwrap_or(0),
            buf.get(base + 2).copied().unwrap_or(0),
            buf.get(base + 3).copied().unwrap_or(0),
        ]
    }

    fn small_shell() -> Shell {
        Shell { x: 20.0, y: 20.0, width: 60.0, height: 60.0, radius: 10.0 }
    }

    #[test]
    fn warp_keeps_the_center_and_the_margin_untouched() {
        let src = solid(100, 100, [10, 20, 30, 255]);
        let out = warp(&src, 100, 100, small_shell(), 10.0).unwrap();
        assert_eq!(pixel(&out, 100, 50, 50), [10, 20, 30, 255]);
        assert_eq!(pixel(&out, 100, 2, 2), [10, 20, 30, 255]);
    }

    #[test]
    fn warp_pulls_the_top_edge_from_above() {
        let mut src = solid(100, 100, [0, 0, 255, 255]);
        for row in src.chunks_exact_mut(100 * 4).take(20) {
            for chunk in row.chunks_exact_mut(4) {
                if let [r, g, b, a] = chunk {
                    *r = 255;
                    *g = 0;
                    *b = 0;
                    *a = 255;
                }
            }
        }
        let out = warp(&src, 100, 100, small_shell(), 10.0).unwrap();
        let edge = pixel(&out, 100, 50, 20);
        assert!(edge[0] > 200, "the top edge must sample the red margin: {edge:?}");
        assert!(edge[2] < 100, "the top edge must not stay blue: {edge:?}");
    }

    #[test]
    fn warp_preserves_alpha() {
        let src = solid(100, 100, [10, 20, 30, 0]);
        let out = warp(&src, 100, 100, small_shell(), 10.0).unwrap();
        assert_eq!(pixel(&out, 100, 50, 50)[3], 0);
        assert_eq!(pixel(&out, 100, 50, 20)[3], 0);
    }

    #[test]
    fn warp_rejects_a_size_mismatch_and_zero_bevel_copies() {
        assert!(warp(&[0, 0, 0, 0], 2, 2, small_shell(), 4.0).is_none());
        let src = solid(100, 100, [1, 2, 3, 255]);
        let out = warp(&src, 100, 100, small_shell(), 0.0).unwrap();
        assert_eq!(out, src);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test glass_optics`
Expected: compile error — `warp` not found.

- [ ] **Step 3: Implement**

```rust
/// Fraction of the bend used to spread R/G/B sample positions.
pub const DISPERSION: f32 = 0.03;

/// One CPU resampling pass: the bevel band is displaced along the SDF
/// normal, R/G/B sample slightly different positions for dispersion, and
/// everything else is copied verbatim. Returns `None` when `src` does not
/// match `width * height * 4`.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "pixel counts are far under f32's 24-bit mantissa"
)]
pub fn warp(
    src: &[u8],
    width: usize,
    height: usize,
    shell: Shell,
    bevel_px: f32,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || src.len() != width * height * 4 {
        return None;
    }
    let mut out = src.to_vec();
    if !(bevel_px > 0.0) {
        return Some(out);
    }
    let margins = Margins {
        left: shell.x.max(0.0),
        top: shell.y.max(0.0),
        right: (width as f32 - (shell.x + shell.width)).max(0.0),
        bottom: (height as f32 - (shell.y + shell.height)).max(0.0),
    };
    let lut = bend_lut();
    for (y, row) in out.chunks_exact_mut(width * 4).enumerate() {
        for (x, chunk) in row.chunks_exact_mut(4).enumerate() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "pixel coordinates are far under f32's 24-bit mantissa"
            )]
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            let Some((bx, by)) = displacement(&lut, p, shell, bevel_px, margins) else {
                continue;
            };
            let red = sample_bilinear(src, width, height, p.0 + bx * (1.0 - DISPERSION), p.1 + by * (1.0 - DISPERSION));
            let green = sample_bilinear(src, width, height, p.0 + bx, p.1 + by);
            let blue = sample_bilinear(src, width, height, p.0 + bx * (1.0 + DISPERSION), p.1 + by * (1.0 + DISPERSION));
            if let [r, g, b, a] = chunk {
                *r = red[0];
                *g = green[1];
                *b = blue[2];
                *a = green[3];
            }
        }
    }
    Some(out)
}

/// Clamped bilinear RGBA sample.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped coordinates index the crop; the result rounds back to 0..=255"
)]
fn sample_bilinear(src: &[u8], width: usize, height: usize, x: f32, y: f32) -> [u8; 4] {
    let x = x.clamp(0.0, (width - 1) as f32);
    let y = y.clamp(0.0, (height - 1) as f32);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let at = |px: usize, py: usize| {
        let base = (py * width + px) * 4;
        [
            f32::from(src.get(base).copied().unwrap_or(0)),
            f32::from(src.get(base + 1).copied().unwrap_or(0)),
            f32::from(src.get(base + 2).copied().unwrap_or(0)),
            f32::from(src.get(base + 3).copied().unwrap_or(0)),
        ]
    };
    let (tl, tr, bl, br) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
    let mut out = [0u8; 4];
    for (slot, i) in out.iter_mut().zip(0..4) {
        let top = tl[i] + (tr[i] - tl[i]) * fx;
        let bottom = bl[i] + (br[i] - bl[i]) * fx;
        let value = top + (bottom - top) * fy;
        *slot = value.round().clamp(0.0, 255.0) as u8;
    }
    out
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test glass_optics`
Expected: 24 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/glass_optics.rs
git commit -m "feat: add the cpu warp resampling pass"
```

---

## Chunk 2: wallpaper raw, the pivot, the queue

### Task 8: `RawGlass` + `build_raw`

**Files:**
- Modify: `src/wallpaper.rs`

- [ ] **Step 1: Write the failing tests** (append to the existing `mod tests`)

```rust
    #[test]
    fn the_crop_rect_is_signed_relative_to_the_window() {
        let r = crop_rect(52, 0, 568, 408, (100, 0), 1.0);
        assert!((r.x - -48.0).abs() < 1e-4);
        assert!((r.y - 0.0).abs() < 1e-4);
        assert!((r.width - 516.0).abs() < 1e-4);
        assert!((r.height - 408.0).abs() < 1e-4);
        // A window hanging 10 px off the left edge: the crop starts right of it.
        let r = crop_rect(0, 0, 458, 408, (-10, 0), 1.0);
        assert!((r.x - 10.0).abs() < 1e-4);
    }

    #[test]
    fn raw_glass_warp_carries_geometry_luma_and_generation() {
        let raw = RawGlass {
            rgba: solid_rgba(4, 4, [40, 50, 60, 255]),
            width: 4,
            height: 4,
            crop: Rect { x: -2.0, y: -2.0, width: 8.0, height: 8.0 },
            image_scale: 1.0,
            stage_width: 420.0,
            luma: 0.4,
            generation: 7,
        };
        let shape = GlassShape { width: 2.0, height: 2.0, radius: 1.0 };
        let backdrop = raw.warp(shape, None).unwrap();
        assert_eq!(backdrop.crop, raw.crop);
        assert_eq!(backdrop.shape, shape);
        assert_eq!(backdrop.generation, 7);
        assert!((backdrop.luma - 0.4).abs() < 1e-6);
        let overridden = raw.warp(shape, Some(0.9)).unwrap();
        assert!((overridden.luma - 0.9).abs() < 1e-6);
    }

    fn solid_rgba(width: usize, height: usize, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            out.extend_from_slice(&rgba);
        }
        out
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test wallpaper`
Expected: compile error — `crop_rect`, `RawGlass` not found.

- [ ] **Step 3: Implement**

Add to the imports at the top:

```rust
use crate::glass_optics::{self, GlassShape, Rect};
```

Make the geometry helper and the new types public to the crate, and split `build`:

```rust
/// The crop rect in logical pixels relative to the window's top-left.
///
/// `left/top/right/bottom` are the physical crop bounds and `stage` is the
/// window's physical origin. x/y are negative while the crop reaches past the
/// window, and turn positive once the window hangs off the screen.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "pixel counts are far under f32's 24-bit mantissa"
)]
pub(crate) fn crop_rect(
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    stage: (i32, i32),
    scale: f32,
) -> Rect {
    Rect {
        x: (left as f32 - stage.0 as f32) / scale,
        y: (top as f32 - stage.1 as f32) / scale,
        width: (right - left) as f32 / scale,
        height: (bottom - top) as f32 / scale,
    }
}

/// A decoded, saturated wallpaper crop that has not been refracted yet.
///
/// The view keeps the newest one around so a shape change can re-warp
/// without touching the wallpaper again. Built on the background executor
/// only: a full decode costs hundreds of milliseconds.
pub struct RawGlass {
    /// Saturated RGBA pixels, `width * height * 4`.
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Crop rect in logical pixels relative to the window's top-left.
    pub crop: Rect,
    /// Image pixels per logical pixel: `scale` static, `scale / factor` live.
    pub image_scale: f32,
    /// Unclipped stage width in logical pixels, for centering the shell.
    pub stage_width: f32,
    /// Mean lightness of the crop, 0.0 dark .. 1.0 bright.
    pub luma: f32,
    /// Bumped per static build and per captured live frame.
    pub generation: u64,
}

impl RawGlass {
    /// Refracts the crop for `shape` into a paintable backdrop.
    ///
    /// `luma` overrides the raw lightness — the live path passes its smoothed
    /// value so a re-warp does not snap the scrim.
    #[must_use]
    pub fn warp(&self, shape: GlassShape, luma: Option<f32>) -> Option<GlassBackdrop> {
        let shell = glass_optics::shell_rect(self.stage_width, shape, self.crop, self.image_scale);
        let bevel_px = glass_optics::bevel_width(shape) * self.image_scale;
        let rgba = glass_optics::warp(&self.rgba, self.width, self.height, shell, bevel_px)?;
        let image = render_image_rgba(self.width, self.height, &rgba)?;
        Some(GlassBackdrop {
            image: Arc::new(image),
            luma: luma.unwrap_or(self.luma),
            crop: self.crop,
            shape,
            generation: self.generation,
        })
    }
}
```

Refactor `build` into `build_raw` — same body, but it ends with `RawGlass` instead of `GlassBackdrop` (`build` itself stays for now and is deleted in Task 9):

```rust
#[must_use]
pub fn build_raw(
    display: (u32, u32),
    rect: (i32, i32, u32, u32),
    scale: f32,
) -> Option<RawGlass> {
    // ... identical decode/compose/crop/saturate/luma body as `build` ...
    let crop = crop_rect(left, top, right, bottom, (rect.0, rect.1), scale);
    // Carry `build`'s existing cast `#[allow]` blocks over; `stage_width`
    // below needs the same `cast_precision_loss` allowance.
    #[allow(
        clippy::cast_precision_loss,
        reason = "pixel counts are far under 2^24"
    )]
    let stage_width = rect.2 as f32 / scale;
    Some(RawGlass {
        rgba: crop_raw,
        width: cw,
        height: ch,
        crop,
        image_scale: scale,
        stage_width,
        luma,
        generation: 0,
    })
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test wallpaper`
Expected: PASS (new tests plus the existing ones).

- [ ] **Step 5: Commit**

```bash
git add src/wallpaper.rs
git commit -m "feat: add the raw glass crop and its warp"
```

### Task 9: The pivot — warped backdrops everywhere

This is the one atomic swap: `GlassBackdrop` gains `crop`/`shape`/`generation`, the lens paints once, and everything publishes already-warped (or explicitly unwarped) images. After this task the app shows *flat* crops; Tasks 10–12 wire real warps in.

**Files:**
- Modify: `src/wallpaper.rs` (`GlassBackdrop`, delete `build`, margin 48)
- Modify: `src/live_glass.rs` (`CropSpec.crop`, frame construction)
- Modify: `src/island_view/mod.rs` (`poll_glass` → `build_raw` + placeholder warp)
- Modify: `src/island_view/glass.rs` (one-draw lens, deletions)
- Test: same files

- [ ] **Step 1: `wallpaper.rs` — new `GlassBackdrop`, margin 48, delete `build`**

```rust
/// How much wallpaper the crop keeps around the stage, logical pixels.
///
/// The edge lens samples the desktop *outside* the panel; the margin must
/// exceed the warp's maximal reach (`0.8 * 36 * 1.3 + dispersion ≈ 38.6`).
pub const REFRACTION_MARGIN: f32 = 48.0;

/// A wallpaper crop ready to bend. Built by `RawGlass::warp` and swapped
/// into the view as one value.
#[derive(Clone)]
pub struct GlassBackdrop {
    /// The refracted crop, one-to-one with the stage plus a lens margin.
    pub image: Arc<RenderImage>,
    /// Mean lightness of the crop, 0.0 dark .. 1.0 bright. Drives the scrim.
    pub luma: f32,
    /// Crop rect in logical pixels relative to the window's top-left.
    pub crop: Rect,
    /// The logical shape the warp was baked for. `ZERO` means unwarped.
    pub shape: GlassShape,
    /// The `RawGlass` generation this image was built from.
    pub generation: u64,
}
```

Delete `pub fn build(...)` entirely (`build_raw` replaces it; Task 10 rewires the only caller).

- [ ] **Step 2: `live_glass.rs` — crop rect instead of pad**

Add to the imports: `use crate::glass_optics::{GlassShape, Rect};`

```rust
pub(crate) struct CropSpec {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    /// Integer downsampling factor, 1..=3.
    pub factor: u32,
    /// Crop rect in logical px relative to the window's top-left.
    pub crop: Rect,
}

impl CropSpec {
    const EMPTY: Self = Self {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
        factor: 1,
        crop: Rect::ZERO,
    };
}
```

In `crop_spec`, replace the `pad` computation with:

```rust
let crop = wallpaper::crop_rect(left, top, right, bottom, (stage.0, stage.1), scale);
Some(CropSpec { left, top, right, bottom, factor, crop })
```

In `on_frame_arrived`, replace the final construction:

```rust
let generation = self.sink.next_generation();
let image = wallpaper::render_image_rgba(
    usize::try_from(out_width).map_err(|_| LiveError)?,
    usize::try_from(out_height).map_err(|_| LiveError)?,
    &rgba,
)
.ok_or(LiveError)?;
// Unwarped for now: the live warp lands with the CropGeometry split.
self.sink.publish(GlassBackdrop {
    image: Arc::new(image),
    luma,
    crop: spec.crop,
    shape: GlassShape::ZERO,
    generation,
});
```

Give the `Sink` a generation counter:

```rust
#[derive(Default)]
struct Sink {
    latest: Mutex<Option<GlassBackdrop>>,
    next_generation: AtomicU64,
}

impl Sink {
    fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::Relaxed)
    }
    // publish/take unchanged except the payload type stays GlassBackdrop
}
```

with `use std::sync::atomic::{AtomicU64, Ordering};` added. In `take_frame`, rebuild the backdrop with the smoothed luma:

```rust
let frame = Arc::new(GlassBackdrop {
    image: Arc::clone(&raw.image),
    luma,
    crop: raw.crop,
    shape: raw.shape,
    generation: raw.generation,
});
```

(the local `raw` here is the taken `GlassBackdrop`; rename for clarity if desired).

- [ ] **Step 3: `island_view/mod.rs` — placeholder warp in `poll_glass`**

Add to the imports: `use crate::glass_optics::GlassShape;`

Replace the build spawn:

```rust
let built = cx
    .background_executor()
    .spawn(async move {
        let screen = wallpaper::screen_size();
        wallpaper::build_raw(screen, rect, scale)
            .and_then(|raw| raw.warp(GlassShape::ZERO, None))
    })
    .await;
```

(The surrounding epoch/fingerprint logic stays as-is; Task 10 replaces it.)

- [ ] **Step 4: `island_view/glass.rs` — one-draw lens**

Module doc: swap the refraction row to “one CPU SDF warp baked into the crop, [`lens`] paints it once”, and the dispersion row to “R/G/B sample offsets inside the warp — [`glass_optics::DISPERSION`]”. The old `[`DISPERSION`]` link dies with the constant in this task. The import block ends up as (`rustedge::island::{self, IslandState}` and the `gpui_kit` list stay as they are):

```rust
use crate::glass_optics::{self, GlassShape, Rect};
use crate::wallpaper;
```

Delete: `LENS_RINGS`, `REFRACTION_PULL`, `refraction_depth`, `refraction_pull`, `lens_bounds`, `shrink_factor`, `inset_shell`, `paint_lens`, `paint_crop`, `bounds_from`, `shell_offset`, `DISPERSION`, and the two dispersion `paint_ring(...)` calls inside `paint_rim`.

Replace `lens` and add the painter:

```rust
/// The refracted wallpaper under the shell, painted once at the root.
///
/// The crop is warped on the producer side (`RawGlass::warp`); this only
/// clips the shell out of the crop's natural rect. Empty when tucked as the
/// line or before the first build lands — the fallback wash in [`layers`]
/// takes over.
pub(super) fn lens(
    shape: island::Morph,
    state: IslandState,
    backdrop: Option<&Arc<wallpaper::GlassBackdrop>>,
) -> gpui_kit::Div {
    let base = div().absolute().inset_0();
    // `chrome::paint` hands the tucked line to `line_glass` below 12px, so the
    // lens must not paint a wallpaper sliver under it during the tuck morph.
    if state == IslandState::Line || shape.height < 12.0 {
        return base;
    }
    let Some(backdrop) = backdrop else {
        return base;
    };
    let backdrop = Arc::clone(backdrop);
    base.child(
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, (): (), window: &mut Window, _: &mut App| {
                paint_backdrop(bounds, shape, &backdrop, window);
            },
        )
        .size_full(),
    )
}

/// One clipped draw: the current shell out of the (possibly stale-scaled)
/// crop rect. A backdrop baked for an older shape is mapped from its own
/// shell onto the current one so the old bevel rides the new edge instead of
/// ghosting inside the panel.
#[allow(
    clippy::cast_precision_loss,
    reason = "geometry floats: the values are window-sized, far under f32's 24-bit mantissa"
)]
fn paint_backdrop(
    bounds: Bounds<Pixels>,
    morph: island::Morph,
    backdrop: &wallpaper::GlassBackdrop,
    window: &mut Window,
) {
    let stage_width = f32::from(bounds.size.width);
    let shape = GlassShape::from(morph);
    let (shell, natural) = glass_optics::crop_rects(stage_width, shape, backdrop.crop);
    let mut image_rect = natural;
    if backdrop.shape != shape && backdrop.shape.width > 0.0 && backdrop.shape.height > 0.0 {
        let baked = glass_optics::crop_rects(stage_width, backdrop.shape, backdrop.crop);
        image_rect = glass_optics::stale_image_bounds(natural, baked.0, shell);
    }
    let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let _painted = window.paint_image(
        to_bounds(shell, origin),
        to_bounds(image_rect, origin),
        Corners::all(px(shape.radius)),
        Arc::clone(&backdrop.image),
        0,
        false,
    );
}

fn to_bounds(rect: Rect, origin: (f32, f32)) -> Bounds<Pixels> {
    Bounds::new(
        point(px(origin.0 + rect.x), px(origin.1 + rect.y)),
        size(px(rect.width.max(0.0)), px(rect.height.max(0.0))),
    )
}
```

Tests: delete `the_lens_pulls_hardest_at_the_edge_and_stops_at_the_depth`, `the_lens_reaches_the_centre_of_the_shorter_side`, `the_lens_rect_samples_outside_the_silhouette`, `shell_offset_centers_the_capsule_and_zeros_the_card`. Trim the `shell_offset(260.0)` assertion out of `rim_alpha_falls_off_inward_and_exactly_bright_at_edge`, and drop `PILL_WIDTH` from the test `use` (it is now unused). Update the `backdrop()` helper:

```rust
fn backdrop() -> Arc<wallpaper::GlassBackdrop> {
    let image = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
        image::RgbaImage::new(2, 2)
    )]));
    Arc::new(wallpaper::GlassBackdrop {
        image,
        luma: 0.5,
        crop: Rect { x: -48.0, y: -48.0, width: 516.0, height: 456.0 },
        shape: GlassShape::ZERO,
        generation: 0,
    })
}
```

- [ ] **Step 5: Fix the live crop tests**

`crop_keeps_the_lens_margin` (48 px margin, factor 1, stage `(100, 0, 420, 360)`):

```rust
let spec = crop_spec((100, 0, 420, 360), 1.0, (1920, 1080)).unwrap();
assert_eq!((spec.left, spec.top, spec.right, spec.bottom), (52, 0, 568, 408));
assert_eq!(spec.factor, 1);
assert!((spec.crop.x - -48.0).abs() < 1e-4);
assert!((spec.crop.y - 0.0).abs() < 1e-4);
assert!((spec.crop.width - 516.0).abs() < 1e-4);
assert!((spec.crop.height - 408.0).abs() < 1e-4);
```

`crop_clamps_at_screen_edges` and `crop_is_a_whole_number_of_blocks` get the same treatment — with `crop` the numbers change meaning, so write them out (margin 48):

```rust
#[test]
fn crop_clamps_at_screen_edges() {
    let left = crop_spec((0, 0, 420, 360), 1.0, (1920, 1080)).unwrap();
    assert_eq!(left.left, 0);
    assert!(left.crop.x.abs() < 1e-6);
    assert!(left.crop.y.abs() < 1e-6);
    assert!(crop_spec((2000, 0, 420, 360), 1.0, (1920, 1080)).is_none());
}

#[test]
fn crop_is_a_whole_number_of_blocks() {
    let spec = crop_spec((90, 0, 1260, 1080), 3.0, (1920, 1080)).unwrap();
    assert_eq!(spec.factor, 3);
    assert_eq!((spec.right - spec.left) % 3, 0);
    assert_eq!((spec.bottom - spec.top) % 3, 0);
    // The stage starts 90 px in, so the crop reaches 30 px left of the window.
    assert!((spec.crop.x - -30.0).abs() < 1e-4);
    assert!((spec.crop.width - 498.0).abs() < 1e-4);
}
```

`crop_keeps_the_lens_margin` (already rewritten above) asserts `(52, 0, 568, 408)` and `crop = (-48, 0, 516, 408)`; `crop_clamps_at_screen_edges` keeps its off-screen `is_none` arm.

- [ ] **Step 6: Run everything**

Run: `cargo test`
Expected: all targets PASS.

Run: `cargo clippy --all-targets`
Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add src/wallpaper.rs src/live_glass.rs src/island_view/mod.rs src/island_view/glass.rs
git commit -m "feat: paint the glass backdrop as one warped image"
```

### Task 10: The warp queue + static wiring

**Files:**
- Create: `src/island_view/warp_queue.rs`
- Modify: `src/island_view/mod.rs`
- Modify: `src/main.rs` (`mod island_view;` already declares children in `island_view/mod.rs` — add `mod warp_queue;` inside `src/island_view/mod.rs`)

- [ ] **Step 1: Write the failing queue tests**

Create `src/island_view/warp_queue.rs` with the types and an empty queue, plus:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert the precondition themselves")]
mod tests {
    use super::*;
    use crate::glass_optics::Rect;

    fn raw(generation: u64) -> Arc<RawGlass> {
        Arc::new(RawGlass {
            rgba: Vec::new(),
            width: 0,
            height: 0,
            crop: Rect::ZERO,
            image_scale: 1.0,
            stage_width: 420.0,
            luma: 0.5,
            generation,
        })
    }

    fn shape() -> GlassShape {
        GlassShape { width: 420.0, height: 288.0, radius: 24.0 }
    }

    #[test]
    fn a_duplicate_of_pending_or_in_flight_is_skipped_without_a_new_ticket() {
        let mut queue = WarpQueue::default();
        let first = queue.request(raw(1), shape(), WarpDest::Live).unwrap();
        let ticket = queue.ticket();
        queue.mark_started(&first);
        assert!(queue.request(raw(1), shape(), WarpDest::Live).is_none());
        assert_eq!(queue.ticket(), ticket, "a duplicate must not invalidate the in-flight warp");
    }

    #[test]
    fn a_newer_request_supersedes_the_pending_one() {
        let mut queue = WarpQueue::default();
        let first = queue.request(raw(1), shape(), WarpDest::Live).unwrap();
        queue.mark_started(&first);
        let second = queue.request(raw(1), shape(), WarpDest::Static).unwrap();
        assert!(!queue.finish(&first), "the superseded warp must not install");
        assert_eq!(queue.take_pending().unwrap().ticket, second.ticket);
    }

    #[test]
    fn a_failed_request_is_not_reissued_until_it_changes() {
        let mut queue = WarpQueue::default();
        let request = queue.request(raw(1), shape(), WarpDest::Live).unwrap();
        queue.mark_started(&request);
        assert!(queue.finish(&request));
        queue.record_failed(&request);
        assert!(queue.request(raw(1), shape(), WarpDest::Live).is_none());
        assert!(queue.request(raw(2), shape(), WarpDest::Live).is_some());
        queue.record_success();
        assert!(queue.request(raw(1), shape(), WarpDest::Live).is_some());
    }

    #[test]
    fn clear_pending_drops_only_the_pending_slot() {
        let mut queue = WarpQueue::default();
        let first = queue.request(raw(1), shape(), WarpDest::Live).unwrap();
        queue.mark_started(&first);
        let _second = queue.request(raw(2), shape(), WarpDest::Live).unwrap();
        queue.clear_pending();
        assert!(queue.take_pending().is_none());
        assert!(!queue.finish(&first));
        assert_eq!(queue.in_flight_generation(), None);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test warp_queue`
Expected: compile error.

- [ ] **Step 3: Implement the queue**

```rust
//! The glass warp queue: one in-flight warp, one pending slot, and a
//! deterministic-failure marker, guarded by a ticket.
//!
//! Pure on purpose — the view owns installation and GPUI. The two-slot shape
//! matters: a single slot would let the 32 ms poller invalidate a long warp
//! forever.

use std::sync::Arc;

use crate::glass_optics::GlassShape;
use crate::wallpaper::RawGlass;

/// Which view slot a warp result may install into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WarpDest {
    Live,
    Static,
}

/// One queued warp: a raw crop, the shape to bake, and where it may land.
#[derive(Clone)]
pub(crate) struct WarpRequest {
    pub raw: Arc<RawGlass>,
    pub shape: GlassShape,
    pub dest: WarpDest,
    pub generation: u64,
    pub ticket: u64,
}

impl WarpRequest {
    fn identity(&self) -> (u64, GlassShape, WarpDest) {
        (self.generation, self.shape, self.dest)
    }
}

#[derive(Default)]
pub(crate) struct WarpQueue {
    pending: Option<WarpRequest>,
    in_flight: Option<WarpRequest>,
    failed: Option<(u64, GlassShape, WarpDest)>,
    ticket: u64,
}

impl WarpQueue {
    /// Queues one warp and returns it, or `None` when the same
    /// `(generation, shape, dest)` is already pending, in flight, or known
    /// to fail. A *different* request supersedes the pending slot.
    pub(crate) fn request(
        &mut self,
        raw: Arc<RawGlass>,
        shape: GlassShape,
        dest: WarpDest,
    ) -> Option<WarpRequest> {
        let identity = (raw.generation, shape, dest);
        if self.pending.as_ref().is_some_and(|r| r.identity() == identity) {
            return None;
        }
        if self.in_flight.as_ref().is_some_and(|r| r.identity() == identity) {
            return None;
        }
        if self.failed.as_ref().is_some_and(|f| *f == identity) {
            return None;
        }
        self.ticket = self.ticket.wrapping_add(1);
        let request = WarpRequest { raw, shape, dest, generation: identity.0, ticket: self.ticket };
        self.pending = Some(request.clone());
        Some(request)
    }

    /// Marks a request as started, clearing the pending slot only if it is
    /// still this request.
    pub(crate) fn mark_started(&mut self, request: &WarpRequest) {
        if self.pending.as_ref().is_some_and(|p| p.ticket == request.ticket) {
            self.pending = None;
        }
        self.in_flight = Some(request.clone());
    }

    /// Clears the in-flight slot. Returns `true` when this warp is still the
    /// newest one and may install.
    pub(crate) fn finish(&mut self, request: &WarpRequest) -> bool {
        self.in_flight = None;
        request.ticket == self.ticket
    }

    /// Records a deterministic failure; the poller stops re-requesting it
    /// until the raw generation or the shape changes.
    pub(crate) fn record_failed(&mut self, request: &WarpRequest) {
        self.failed = Some(request.identity());
    }

    /// A success clears the failure marker.
    pub(crate) fn record_success(&mut self) {
        self.failed = None;
    }

    /// Drops the pending slot (source switch). The in-flight warp is left to
    /// its own completion, which clears `in_flight` itself.
    pub(crate) fn clear_pending(&mut self) {
        self.pending = None;
    }

    /// Drops only the pending warps for one destination — used when a static
    /// build fails and its queued re-warp must not run against a dead raw.
    /// An in-flight warp for that destination still finishes; its install is
    /// blocked by the generation check.
    pub(crate) fn clear_dest(&mut self, dest: WarpDest) {
        if self.pending.as_ref().is_some_and(|r| r.dest == dest) {
            self.pending = None;
        }
    }

    /// Takes the pending request for the next start.
    pub(crate) fn take_pending(&mut self) -> Option<WarpRequest> {
        self.pending.take()
    }

    /// Whether a warp is running.
    pub(crate) fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The in-flight generation, for tests.
    #[cfg(test)]
    pub(crate) fn in_flight_generation(&self) -> Option<u64> {
        self.in_flight.as_ref().map(|r| r.generation)
    }

    /// The current ticket, for tests.
    #[cfg(test)]
    pub(crate) fn ticket(&self) -> u64 {
        self.ticket
    }
}
```

Declare it in `src/island_view/mod.rs`: `mod warp_queue;` with `use warp_queue::{WarpDest, WarpQueue, WarpRequest};`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test warp_queue`
Expected: 4 tests PASS.

- [ ] **Step 5: Wire the view**

In `IslandView` add fields (initialize `glass_luma: glass::IDLE_LUMA` and the rest `Default::default()` in the constructor):

```rust
/// The newest static crop, kept around for shape-only re-warps.
static_raw: Option<std::sync::Arc<wallpaper::RawGlass>>,
/// The generation `static_raw` was built under.
static_generation: u64,
/// Smoothed backdrop lightness; `Face` reads this directly.
glass_luma: f32,
warps: WarpQueue,
```

`Face` construction: replace
`glass_luma: self.glass.as_ref().map_or(glass::IDLE_LUMA, |backdrop| backdrop.luma)`
with `glass_luma: self.glass_luma`.

Rewrite `poll_glass` (replacing the Task 9 placeholder):

```rust
/// Refreshes the static glass when the wallpaper or the stage moved.
///
/// One fingerprint comparison per 2s tick. A changed fingerprint bumps the
/// epoch, decodes off-thread, and warps through the queue. A failed build
/// still records the fingerprint so a broken wallpaper does not re-decode
/// every poll.
fn poll_glass(&mut self, cx: &mut Context<Self>) {
    // Live owns the glass; one static build still seeds the fallback slot.
    if self.live.is_live() && self.static_glass.is_some() {
        return;
    }
    if self.glass_building {
        return;
    }
    let Some((origin_x, origin_y, width, height, scale)) = placed_stage(cx) else {
        return;
    };
    let rect = (origin_x, origin_y, width, height);
    cx.spawn(async move |view, cx| {
        let snapshot = cx
            .background_executor()
            .spawn(async move { wallpaper::fingerprint_now(rect) })
            .await;
        // Bump the epoch only when the fingerprint truly moved, so a warp is
        // never invalidated by an idle poll.
        let epoch = view
            .update(cx, |view, _| {
                if snapshot == view.glass_fingerprint {
                    return None;
                }
                view.glass_building = true;
                view.glass_epoch = view.glass_epoch.wrapping_add(1);
                Some(view.glass_epoch)
            })
            .ok()
            .flatten();
        let Some(epoch) = epoch else {
            return;
        };
        let built = cx
            .background_executor()
            .spawn(async move { wallpaper::build_raw(wallpaper::screen_size(), rect, scale) })
            .await;
        view.update(cx, |view, cx| {
            if view.glass_epoch != epoch {
                return;
            }
            view.glass_building = false;
            view.glass_fingerprint = snapshot;
            match built {
                Some(mut raw) => {
                    raw.generation = epoch;
                    let raw = std::sync::Arc::new(raw);
                    view.static_raw = Some(std::sync::Arc::clone(&raw));
                    view.static_generation = epoch;
                    let shape = GlassShape::from(view.shown.shape.get());
                    view.request_warp(raw, shape, WarpDest::Static, cx);
                }
                None => {
                    view.static_raw = None;
                    view.static_glass = None;
                    view.static_generation = 0;
                    // A queued static warp would run against a dead raw.
                    view.warps.clear_dest(WarpDest::Static);
                    if !view.live.is_live() {
                        view.glass = None;
                    }
                }
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
}
```

Add the queue methods, `poll_warp`, and the 32 ms hook:

```rust
/// Queues one background warp for `raw`. Duplicates and known failures are
/// dropped by the queue; a busy queue keeps the request pending.
fn request_warp(
    &mut self,
    raw: std::sync::Arc<wallpaper::RawGlass>,
    shape: GlassShape,
    dest: WarpDest,
    cx: &mut Context<Self>,
) {
    let Some(request) = self.warps.request(raw, shape, dest) else {
        return;
    };
    if self.warps.busy() {
        return;
    }
    self.warps.mark_started(&request);
    self.spawn_warp(request, cx);
}

/// Runs one warp on the background executor. `raw` and `shape` are cloned out
/// of the request first so the inner block and the completion handler do not
/// fight over the same move.
fn spawn_warp(&mut self, request: WarpRequest, cx: &mut Context<Self>) {
    let luma = match request.dest {
        WarpDest::Live => Some(self.glass_luma),
        WarpDest::Static => None,
    };
    let raw = std::sync::Arc::clone(&request.raw);
    let shape = request.shape;
    cx.spawn(async move |view, cx| {
        let built = cx
            .background_executor()
            .spawn(async move { raw.warp(shape, luma) })
            .await;
        view.update(cx, |view, cx| view.finish_warp(request, built, cx)).ok();
    })
    .detach();
}

/// Installs a finished warp when it is still the newest one, then starts the
/// next pending request. Failures keep the old image and mark the request so
/// the poller stops re-issuing it.
fn finish_warp(
    &mut self,
    request: WarpRequest,
    built: Option<wallpaper::GlassBackdrop>,
    cx: &mut Context<Self>,
) {
    let fresh = self.warps.finish(&request);
    if fresh {
        match built {
            Some(backdrop) => {
                self.warps.record_success();
                match request.dest {
                    WarpDest::Static if request.generation == self.static_generation => {
                        let show = !self.live.is_live();
                        self.static_glass = Some(std::sync::Arc::new(backdrop.clone()));
                        if show {
                            self.glass_luma = backdrop.luma;
                            self.glass = Some(std::sync::Arc::new(backdrop));
                            cx.notify();
                        }
                    }
                    // Live installation arrives with the live wiring.
                    _ => {}
                }
            }
            None => {
                self.warps.record_failed(&request);
                eprintln!("glass warp failed (generation {})", request.generation);
            }
        }
    }
    self.start_pending_warp(cx);
}

fn start_pending_warp(&mut self, cx: &mut Context<Self>) {
    if self.warps.busy() {
        return;
    }
    let Some(request) = self.warps.take_pending() else {
        return;
    };
    self.warps.mark_started(&request);
    self.spawn_warp(request, cx);
}

/// Re-warps the newest raw whenever only the shape moved — the desktop did
/// not change, so WGC will not send a frame.
fn poll_warp(&mut self, cx: &mut Context<Self>) {
    let shape = GlassShape::from(self.shown.shape.get());
    if self.live.is_live() {
        return; // the live branch lands with the live wiring
    }
    if let Some(raw) = self.static_raw.clone() {
        let stale = self
            .glass
            .as_ref()
            .is_none_or(|g| g.generation != raw.generation || g.shape != shape);
        if stale {
            self.request_warp(raw, shape, WarpDest::Static, cx);
        }
    }
}
```

In `arm`'s 32 ms loop, after `if view.poll_live(cx) { cx.notify(); }` add:

```rust
view.poll_warp(cx);
```

Delete the `glass_epoch()` helper — `poll_glass` now bumps the epoch inline, and the old helper would be dead code.

While `Face` reads `self.glass_luma`, `poll_live`'s frame install must keep it fresh (Task 11 replaces this block wholesale):

```rust
if let Some(frame) = frame {
    self.glass_luma = frame.luma;
    self.glass = Some(frame);
    true
} else {
    false
}
```

Update `restore_static_glass`:

```rust
fn restore_static_glass(&mut self) {
    self.glass = self.static_glass.clone();
    self.glass_luma = self
        .static_glass
        .as_ref()
        .map_or(glass::IDLE_LUMA, |backdrop| backdrop.luma);
    self.glass_fingerprint = None;
    self.glass_epoch = self.glass_epoch.wrapping_add(1);
    self.glass_building = false;
    self.warps.clear_pending();
}
```

In `set_live_glass`'s live-on branch (which bumps the epoch), also `self.warps.clear_pending();`.

- [ ] **Step 6: Run everything**

Run: `cargo test`
Expected: all PASS.

Run: `cargo clippy --all-targets`
Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add src/island_view/warp_queue.rs src/island_view/mod.rs
git commit -m "feat: re-warp the static glass through a coalescing queue"
```

---

## Chunk 3: live wiring, docs, verification

### Task 11: Live frames warp at capture

**Files:**
- Modify: `src/live_glass.rs`
- Modify: `src/island_view/mod.rs`

- [ ] **Step 1: Write the failing tests** (in `live_glass.rs` `mod tests`)

```rust
    fn card_shape() -> GlassShape {
        GlassShape { width: 420.0, height: 288.0, radius: 24.0 }
    }

    #[test]
    fn crop_spec_carries_shape_geometry_and_image_scale() {
        let spec = crop_spec((100, 0, 420, 360), 2.0, (1920, 1080), card_shape()).unwrap();
        assert_eq!(spec.geometry.factor, 2);
        assert!((spec.geometry.image_scale - 1.0).abs() < 1e-6);
        assert!((spec.geometry.stage_width - 210.0).abs() < 1e-4);
        assert_eq!(spec.shape, card_shape());
    }

    #[test]
    fn update_crop_only_reports_geometry_changes() {
        let mut live = LiveGlass::new();
        assert!(live.update_crop((100, 0, 420, 360), 1.0, card_shape()));
        assert!(!live.update_crop((100, 0, 420, 360), 1.0, card_shape()));
        // Shape-only: the raw stays valid, so this is not a geometry change.
        let morphed = GlassShape { width: 320.0, height: 56.0, radius: 28.0 };
        assert!(!live.update_crop((100, 0, 420, 360), 1.0, morphed));
        assert!(live.update_crop((140, 0, 420, 360), 1.0, morphed));
    }
```

The three crop tests from Task 9 move onto `geometry` and the 4-arg `crop_spec` in the same step — they cannot compile against the old signature:

```rust
    #[test]
    fn crop_keeps_the_lens_margin() {
        let spec = crop_spec((100, 0, 420, 360), 1.0, (1920, 1080), card_shape()).unwrap();
        assert_eq!(
            (
                spec.geometry.left,
                spec.geometry.top,
                spec.geometry.right,
                spec.geometry.bottom
            ),
            (52, 0, 568, 408)
        );
        assert_eq!(spec.geometry.factor, 1);
        assert!((spec.geometry.crop.x - -48.0).abs() < 1e-4);
        assert!((spec.geometry.crop.y - 0.0).abs() < 1e-4);
        assert!((spec.geometry.crop.width - 516.0).abs() < 1e-4);
        assert!((spec.geometry.crop.height - 408.0).abs() < 1e-4);
    }

    #[test]
    fn crop_clamps_at_screen_edges() {
        let left = crop_spec((0, 0, 420, 360), 1.0, (1920, 1080), card_shape()).unwrap();
        assert_eq!(left.geometry.left, 0);
        assert!(left.geometry.crop.x.abs() < 1e-6);
        assert!(left.geometry.crop.y.abs() < 1e-6);
        assert!(crop_spec((2000, 0, 420, 360), 1.0, (1920, 1080), card_shape()).is_none());
    }

    #[test]
    fn crop_is_a_whole_number_of_blocks() {
        let spec = crop_spec((90, 0, 1260, 1080), 3.0, (1920, 1080), card_shape()).unwrap();
        assert_eq!(spec.geometry.factor, 3);
        assert_eq!((spec.geometry.right - spec.geometry.left) % 3, 0);
        assert_eq!((spec.geometry.bottom - spec.geometry.top) % 3, 0);
        assert!((spec.geometry.crop.x - -30.0).abs() < 1e-4);
        assert!((spec.geometry.crop.width - 498.0).abs() < 1e-4);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test live_glass`
Expected: compile error — `CropGeometry`, new `crop_spec` signature not found.

- [ ] **Step 3: Implement `live_glass.rs`**

```rust
/// The physical crop plus the geometry the warp and lens need.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CropGeometry {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    /// Integer downsampling factor, 1..=3.
    pub factor: u32,
    /// Crop rect in logical px relative to the window's top-left.
    pub crop: Rect,
    /// Image pixels per logical pixel: `scale / factor`.
    pub image_scale: f32,
    /// Unclipped stage width in logical pixels.
    pub stage_width: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CropSpec {
    pub geometry: CropGeometry,
    pub shape: GlassShape,
}

impl CropSpec {
    const EMPTY: Self = Self {
        geometry: CropGeometry {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
            factor: 1,
            crop: Rect::ZERO,
            image_scale: 1.0,
            stage_width: 0.0,
        },
        shape: GlassShape::ZERO,
    };
}
```

`crop_spec` gains `shape` and computes the new fields (geometry identical to Task 9 plus):

```rust
let image_scale = scale / factor as f32;
let stage_width = stage.2 as f32 / scale;
Some(CropSpec {
    geometry: CropGeometry { left, top, right, bottom, factor, crop, image_scale, stage_width },
    shape,
})
```

`update_crop` returns whether the geometry moved:

```rust
pub(crate) fn update_crop(
    &mut self,
    stage: (i32, i32, u32, u32),
    scale: f32,
    shape: GlassShape,
) -> bool {
    let Some(spec) = crop_spec(stage, scale, wallpaper::screen_size(), shape) else {
        return false;
    };
    let mut slot = match self.spec.lock() {
        Ok(slot) => slot,
        Err(poison) => PoisonError::into_inner(poison),
    };
    let geometry_changed = slot.geometry != spec.geometry;
    if *slot != spec {
        *slot = spec;
    }
    geometry_changed
}
```

Frame plumbing:

```rust
/// One captured frame: the raw crop (kept for shape-only re-warps) and the
/// backdrop the capture thread already warped.
pub(crate) struct Frame {
    pub raw: Arc<RawGlass>,
    pub warped: Option<Arc<GlassBackdrop>>,
    pub generation: u64,
    pub luma: f32,
}
```

`on_frame_arrived` after the existing downsample/saturate/luma:

```rust
let generation = self.sink.next_generation();
let raw = Arc::new(RawGlass {
    rgba,
    width: usize::try_from(out_width).map_err(|_| LiveError)?,
    height: usize::try_from(out_height).map_err(|_| LiveError)?,
    crop: spec.geometry.crop,
    image_scale: spec.geometry.image_scale,
    stage_width: spec.geometry.stage_width,
    luma,
    generation,
});
let warped = raw.warp(spec.shape, None).map(Arc::new);
self.sink.publish(Frame { raw, warped, generation, luma });
```

(`rgba` moves into `RawGlass`; drop the separate `render_image_rgba` call here — `RawGlass::warp` builds the image.)

`Sink` stores `Frame` (`latest: Mutex<Option<Frame>>`); `take_frame` returns `(Option<Frame>, Action)` and applies the smoothed luma to both the frame and the warped backdrop:

```rust
pub(crate) fn take_frame(&mut self, now_ms: u64) -> (Option<Frame>, Action) {
    if !self.running() {
        return (None, Action::None);
    }
    let Some(mut frame) = self.sink.take() else {
        return (None, Action::None);
    };
    #[allow(clippy::cast_precision_loss, reason = "milliseconds fit in f32 for this use")]
    let dt = now_ms.saturating_sub(self.last_frame_ms) as f32;
    self.last_frame_ms = now_ms;
    frame.luma = self.luma.push(frame.luma, dt);
    if let Some(warped) = frame.warped.take() {
        frame.warped = Some(Arc::new(GlassBackdrop {
            image: Arc::clone(&warped.image),
            luma: frame.luma,
            crop: warped.crop,
            shape: warped.shape,
            generation: warped.generation,
        }));
    }
    let action = self.machine.handle(Event::Frame, now_ms);
    (Some(frame), action)
}
```

- [ ] **Step 4: Consume `Frame` in `poll_live`**

In `poll_live`, replace the `update_crop` call — the stale-raw drop happens immediately, before any `poll_warp` tick can pick it up:

```rust
let shape = GlassShape::from(self.shown.shape.get());
if let Some((x, y, width, height, scale)) = placed_stage(cx) {
    if self.live.update_crop((x, y, width, height), scale, shape) {
        // Stale geometry cannot be re-warped: fall back to the wash until a
        // frame with the new crop arrives. Clearing `static_glass` too lets
        // the 2s poll rebuild the fallback for the new rect (poll_glass
        // early-returns while live && static_glass.is_some()).
        self.last_live_raw = None;
        self.live_generation = 0;
        self.glass = None;
        self.static_glass = None;
        self.warps.clear_dest(WarpDest::Live);
    }
}
```

and at the bottom:

```rust
let (frame, action) = self.live.take_frame(now);
self.apply_live(action, cx);
let mut changed = false;
if let Some(frame) = frame {
    self.live_generation = frame.generation;
    self.last_live_raw = Some(frame.raw);
    if self.glass_luma != frame.luma {
        changed = true; // the scrim follows the smoothed luma
    }
    self.glass_luma = frame.luma;
    if let Some(warped) = frame.warped {
        self.glass = Some(warped);
        changed = true;
    }
}
changed
```

Add the fields `last_live_raw: Option<std::sync::Arc<wallpaper::RawGlass>>` and `live_generation: u64` to `IslandView` (default `None`/`0`), and extend `restore_static_glass` with:

```rust
self.last_live_raw = None;
self.live_generation = 0;
```

- [ ] **Step 5: Run everything**

Run: `cargo test`
Expected: all PASS.

Run: `cargo clippy --all-targets`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add src/live_glass.rs src/island_view/mod.rs
git commit -m "feat: warp live glass frames at capture time"
```

### Task 12: Shape-only live re-warps

**Files:**
- Modify: `src/island_view/mod.rs`

- [ ] **Step 1: Install live re-warps in `finish_warp`**

Task 10's `finish_warp` still has the `_ => {}` arm. Replace it with the Live arm — without it, shape-only re-warps would install nothing, `glass.shape` would never match, and `poll_warp` would re-request the same identity forever:

```rust
                    WarpDest::Live
                        if self.live.is_live()
                            && request.generation == self.live_generation =>
                    {
                        self.glass_luma = backdrop.luma;
                        self.glass = Some(std::sync::Arc::new(backdrop));
                        cx.notify();
                    }
```

- [ ] **Step 2: Activate the live branch of `poll_warp`**

Replace the early return in `poll_warp`:

```rust
fn poll_warp(&mut self, cx: &mut Context<Self>) {
    let shape = GlassShape::from(self.shown.shape.get());
    if self.live.is_live() {
        let Some(raw) = self.last_live_raw.clone() else {
            return;
        };
        let stale = self
            .glass
            .as_ref()
            .is_none_or(|g| g.generation != raw.generation || g.shape != shape);
        if stale {
            self.request_warp(raw, shape, WarpDest::Live, cx);
        }
        return;
    }
    if let Some(raw) = self.static_raw.clone() {
        let stale = self
            .glass
            .as_ref()
            .is_none_or(|g| g.generation != raw.generation || g.shape != shape);
        if stale {
            self.request_warp(raw, shape, WarpDest::Static, cx);
        }
    }
}
```

- [ ] **Step 3: Verify the queue rules still hold**

Run: `cargo test warp_queue`
Expected: PASS (unchanged).

- [ ] **Step 4: Commit**

```bash
git add src/island_view/mod.rs
git commit -m "feat: re-warp live glass when only the shape changed"
```

Coverage note: the queue's identity rules (duplicate skip, superseded ticket, failed marker) are unit-tested in `warp_queue.rs`; `finish_warp`'s dest/generation guards run on the GPUI thread and are covered by those tests plus the manual checklist in Task 14. Extracting them into a pure helper would split the install rules from the state they guard, so the plan accepts the gap deliberately.

### Task 13: Docs, cleanup, full verification

**Files:**
- Modify: `src/glass_optics.rs`, `src/wallpaper.rs`, `src/live_glass.rs`, `src/island_view/glass.rs`, `src/main.rs`, `ARCHITECTURE.md`

- [ ] **Step 1: Module docs**

- `glass_optics.rs`: already documented; re-read for accuracy.
- `wallpaper.rs`: the module doc describes the build chain; mention `build_raw` → `RawGlass` → `RawGlass::warp` and that `REFRACTION_MARGIN` must exceed the warp reach (`0.8 * 36 * 1.3 + dispersion ≈ 38.6`, hence 48).
- `live_glass.rs`: the module doc's frame pipeline line becomes “crop → downsample → saturate → luma → **warp** → `Frame { raw, warped }`”.
- `island_view/glass.rs`: the Flutter-vocabulary table maps “refraction” to “one CPU SDF warp baked into the crop, painted once”.
- `main.rs`: the crate doc's pixel-pipeline bullet gains `glass_optics`.

- [ ] **Step 2: `ARCHITECTURE.md`**

Update the glass pipeline section: the WGC thread publishes `Frame { raw, warped }`; the view re-warps the raw on shape-only changes through `warp_queue`; `lens` paints one image; `REFRACTION_MARGIN` is 48. Mirror the same edits in `ARCHITECTURE.zh.md` if the section exists there.

- [ ] **Step 3: Dead-code sweep**

```bash
cargo build --all-targets 2>&1 | grep -i "never used"
cargo clippy --all-targets
```

Remove anything the pivot orphaned (e.g., unused imports in `glass.rs`). Fix all warnings.

- [ ] **Step 4: Full test + fmt**

```bash
cargo fmt
cargo test
cargo clippy --all-targets
```

Expected: fmt clean, all tests PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "docs: describe the warped glass pipeline"
```

### Task 14: Manual visual verification

Run `cargo run --release` and check, in order:

- [ ] **Checkerboard wallpaper** (set a checkerboard desktop image): grid lines bend around all four card corners; the top edge's lines stay vertical through the bend (no diagonal shear); the capsule ends bend along the radius.
- [ ] **Photo wallpaper**: the wallpaper behind the card is crisp at the center; ink over the glass stays readable; the edge band compresses the outside world inward.
- [ ] **Morph pill → card**: no ghost bevel ring inside the panel during the morph; the bevel hugs the new edge within a frame or two of settling.
- [ ] **Screen edges**: with the window dragged flush to the left screen edge, the left edge stays flat while the right edge still bends; no sampling artifacts.
- [ ] **Window off-screen**: drag the window partially off the left edge; the glass stays aligned with the shell (no shift).
- [ ] **Static fallback** (`RUSTEDGE_GLASS=static`): the refracted static wallpaper looks the same as live; toggling back with `RUSTEDGE_GLASS=live` restores live capture.
- [ ] **Perf page**: no visible frame drops from the warp (release build).

File any visual regressions against the spec's “测试/视觉” list before closing out.
