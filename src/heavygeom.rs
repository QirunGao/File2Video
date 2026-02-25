use super::geom::Geom;

#[derive(Clone, Copy, Debug)]
pub(crate) struct GeomEstimate {
    pub(crate) geom: Geom,
    pub(crate) best_score: f64,
    pub(crate) second_best_score: f64,
    pub(crate) score_gap: f64,
    pub(crate) confidence: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LocatorThresholds {
    pub(crate) y: f64,
    pub(crate) u: f64,
    pub(crate) v: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GeometryCorrector {
    pub(crate) locator_shift_search_px: i32,
    pub(crate) scale_candidates: [f64; 5],
    pub(crate) robust_delta_y: f64,
    pub(crate) robust_delta_uv: f64,
    pub(crate) refine_rounds: usize,
    pub(crate) refine_shift_step_px: f64,
    pub(crate) refine_scale_step: f64,
}

impl Default for GeometryCorrector {
    fn default() -> Self {
        Self {
            locator_shift_search_px: 6,
            scale_candidates: [0.94, 0.97, 1.00, 1.03, 1.06],
            robust_delta_y: 30.0,
            robust_delta_uv: 24.0,
            refine_rounds: 3,
            refine_shift_step_px: 1.0,
            refine_scale_step: 0.01,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GeomFusionState {
    pub(crate) ema_geom: Geom,
    pub(crate) has_state: bool,
    pub(crate) low_conf_streak: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GeomSearchHint {
    pub(crate) geom: Geom,
    pub(crate) low_conf_streak: usize,
}

impl Default for GeomFusionState {
    fn default() -> Self {
        Self { ema_geom: Geom::identity(), has_state: false, low_conf_streak: 0 }
    }
}

impl GeomFusionState {
    pub(crate) fn hint(&self) -> Option<GeomSearchHint> {
        if self.has_state {
            Some(GeomSearchHint { geom: self.ema_geom, low_conf_streak: self.low_conf_streak })
        } else {
            None
        }
    }

    pub(crate) fn fuse(&mut self, est: GeomEstimate) -> GeomEstimate {
        let mut fused = est.geom;
        if self.has_state {
            let alpha = (0.15 + 0.70 * est.confidence).clamp(0.1, 0.9);
            fused.dx = self.ema_geom.dx * (1.0 - alpha) + est.geom.dx * alpha;
            fused.dy = self.ema_geom.dy * (1.0 - alpha) + est.geom.dy * alpha;
            fused.sx = self.ema_geom.sx * (1.0 - alpha) + est.geom.sx * alpha;
            fused.sy = self.ema_geom.sy * (1.0 - alpha) + est.geom.sy * alpha;
        }
        self.ema_geom = fused;
        self.has_state = true;
        self.low_conf_streak = if est.confidence < 0.02 { self.low_conf_streak.saturating_add(1) } else { 0 };
        GeomEstimate { geom: fused, ..est }
    }
}

fn avg2(a: f64, b: f64) -> f64 {
    (a + b) * 0.5
}

fn huber_loss(diff: f64, delta: f64) -> f64 {
    let ad = diff.abs();
    if ad <= delta {
        0.5 * ad * ad
    } else {
        delta * (ad - 0.5 * delta)
    }
}

fn score_triplet(obs: (f64, f64, f64), exp: (f64, f64, f64), dy: f64, duv: f64) -> f64 {
    huber_loss(obs.0 - exp.0, dy) + 0.7 * huber_loss(obs.1 - exp.1, duv) + 0.7 * huber_loss(obs.2 - exp.2, duv)
}

fn trimmed_mean(vals: &mut [f64]) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    let trim = if n >= 8 { 2 } else if n >= 4 { 1 } else { 0 };
    let slice = &vals[trim..(n - trim)];
    slice.iter().sum::<f64>() / (slice.len() as f64)
}

#[derive(Clone, Copy, Debug, Default)]
struct RunningStats {
    n: usize,
    mean: f64,
    m2: f64,
}

impl RunningStats {
    fn push(&mut self, x: f64) {
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
    }

    fn variance(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.m2 / self.n as f64 }
    }
}

#[derive(Default)]
struct ScoreScratch {
    ys: Vec<f64>,
    us: Vec<f64>,
    vs: Vec<f64>,
}

impl ScoreScratch {
    fn ensure_capacity(&mut self, n: usize) {
        if self.ys.capacity() < n {
            self.ys.reserve(n - self.ys.capacity());
        }
        if self.us.capacity() < n {
            self.us.reserve(n - self.us.capacity());
        }
        if self.vs.capacity() < n {
            self.vs.reserve(n - self.vs.capacity());
        }
    }

    fn reset(&mut self) {
        self.ys.clear();
        self.us.clear();
        self.vs.clear();
    }
}

impl GeometryCorrector {
    fn corner_positions(
        &self,
        bx_n: usize,
        by_n: usize,
        locator_size: usize,
    ) -> [(usize, usize); 4] {
        let ls = locator_size.clamp(1, bx_n.min(by_n));
        [
            (0, 0),
            (bx_n.saturating_sub(ls), 0),
            (0, by_n.saturating_sub(ls)),
            (bx_n.saturating_sub(ls), by_n.saturating_sub(ls)),
        ]
    }

    fn score_geom_bounded<F>(
        &self,
        bx_n: usize,
        by_n: usize,
        locator_size: usize,
        expected_corners: [(f64, f64, f64); 4],
        geom: Geom,
        cutoff: f64,
        scratch: &mut ScoreScratch,
        sample_block: &mut F,
    ) -> f64
    where
        F: FnMut(usize, usize, Geom) -> (f64, f64, f64),
    {
        let ls = locator_size.clamp(1, bx_n.min(by_n));
        scratch.ensure_capacity(ls * ls);
        let corners = self.corner_positions(bx_n, by_n, locator_size);
        let mut score = 0.0f64;
        for (ci, &(bx0, by0)) in corners.iter().enumerate() {
            let exp = expected_corners[ci];
            scratch.reset();
            let mut y_stats = RunningStats::default();
            let mut u_stats = RunningStats::default();
            let mut v_stats = RunningStats::default();
            for yb in 0..ls {
                for xb in 0..ls {
                    let obs = sample_block(bx0 + xb, by0 + yb, geom);
                    scratch.ys.push(obs.0);
                    scratch.us.push(obs.1);
                    scratch.vs.push(obs.2);
                    y_stats.push(obs.0);
                    u_stats.push(obs.1);
                    v_stats.push(obs.2);
                }
            }
            let obs_agg = (
                trimmed_mean(&mut scratch.ys),
                trimmed_mean(&mut scratch.us),
                trimmed_mean(&mut scratch.vs),
            );
            score += score_triplet(obs_agg, exp, self.robust_delta_y, self.robust_delta_uv);
            let y_var = y_stats.variance();
            let u_var = u_stats.variance();
            let v_var = v_stats.variance();
            score += 0.02 * huber_loss(y_var.sqrt(), self.robust_delta_y);
            score += 0.01 * huber_loss(u_var.sqrt(), self.robust_delta_uv);
            score += 0.01 * huber_loss(v_var.sqrt(), self.robust_delta_uv);
            if score >= cutoff {
                return score;
            }
        }
        score
    }

    fn score_geom<F>(
        &self,
        bx_n: usize,
        by_n: usize,
        locator_size: usize,
        expected_corners: [(f64, f64, f64); 4],
        geom: Geom,
        scratch: &mut ScoreScratch,
        sample_block: &mut F,
    ) -> f64
    where
        F: FnMut(usize, usize, Geom) -> (f64, f64, f64),
    {
        self.score_geom_bounded(bx_n, by_n, locator_size, expected_corners, geom, f64::INFINITY, scratch, sample_block)
    }

    fn refine_geom<F>(
        &self,
        bx_n: usize,
        by_n: usize,
        locator_size: usize,
        expected_corners: [(f64, f64, f64); 4],
        mut best: Geom,
        mut best_score: f64,
        scratch: &mut ScoreScratch,
        sample_block: &mut F,
    ) -> (Geom, f64)
    where
        F: FnMut(usize, usize, Geom) -> (f64, f64, f64),
    {
        let mut shift_step = self.refine_shift_step_px.max(0.25);
        let mut scale_step = self.refine_scale_step.max(0.0025);
        for _ in 0..self.refine_rounds.max(1) {
            let mut improved = false;
            let candidates = [
                Geom { dx: best.dx + shift_step, ..best },
                Geom { dx: best.dx - shift_step, ..best },
                Geom { dy: best.dy + shift_step, ..best },
                Geom { dy: best.dy - shift_step, ..best },
                Geom { sx: (best.sx + scale_step).clamp(0.85, 1.15), ..best },
                Geom { sx: (best.sx - scale_step).clamp(0.85, 1.15), ..best },
                Geom { sy: (best.sy + scale_step).clamp(0.85, 1.15), ..best },
                Geom { sy: (best.sy - scale_step).clamp(0.85, 1.15), ..best },
            ];
            for cand in candidates {
                let s = self.score_geom(bx_n, by_n, locator_size, expected_corners, cand, scratch, sample_block);
                if s < best_score {
                    best = cand;
                    best_score = s;
                    improved = true;
                }
            }
            if !improved {
                shift_step *= 0.5;
                scale_step *= 0.5;
            }
        }
        (best, best_score)
    }

    fn search_candidates_once<F>(
        &self,
        bx_n: usize,
        by_n: usize,
        locator_size: usize,
        expected_corners: [(f64, f64, f64); 4],
        scale_candidates: &[f64],
        hint_dx: f64,
        hint_dy: f64,
        shift_px: i32,
        aniso_eps: &[f64],
        best: &mut Geom,
        best_score: &mut f64,
        second_best: &mut f64,
        scratch: &mut ScoreScratch,
        sample_block: &mut F,
    )
    where
        F: FnMut(usize, usize, Geom) -> (f64, f64, f64),
    {
        let shift_min = -(shift_px as f64);
        let shift_max = shift_px as f64;
        for &sxy in scale_candidates {
            let mut seeded_pairs = [(sxy, sxy); 5];
            let mut pair_count = 1usize;
            for (i, &eps) in aniso_eps.iter().take(2).enumerate() {
                seeded_pairs[1 + i * 2] = ((sxy * (1.0 + eps)).clamp(0.85, 1.15), (sxy * (1.0 - eps)).clamp(0.85, 1.15));
                seeded_pairs[2 + i * 2] = ((sxy * (1.0 - eps)).clamp(0.85, 1.15), (sxy * (1.0 + eps)).clamp(0.85, 1.15));
                pair_count = 3 + i * 2;
            }
            for &(sx, sy) in seeded_pairs[..pair_count].iter() {
                for dyi in -shift_px..=shift_px {
                    for dxi in -shift_px..=shift_px {
                        let geom = Geom {
                            dx: (hint_dx + dxi as f64).clamp(hint_dx + shift_min, hint_dx + shift_max),
                            dy: (hint_dy + dyi as f64).clamp(hint_dy + shift_min, hint_dy + shift_max),
                            sx,
                            sy,
                        };
                        let s = self.score_geom_bounded(
                            bx_n,
                            by_n,
                            locator_size,
                            expected_corners,
                            geom,
                            *best_score,
                            scratch,
                            sample_block,
                        );
                        if s < *best_score {
                            *second_best = *best_score;
                            *best_score = s;
                            *best = geom;
                        } else if s < *second_best {
                            *second_best = s;
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn estimate_global_shift_scored<F>(
        &self,
        bx_n: usize,
        by_n: usize,
        locator_size: usize,
        expected_corners: [(f64, f64, f64); 4],
        hint: Option<GeomSearchHint>,
        mut sample_block: F,
    ) -> GeomEstimate
    where
        F: FnMut(usize, usize, Geom) -> (f64, f64, f64),
    {
        let mut best = Geom::identity();
        let mut best_score = f64::INFINITY;
        let mut second_best = f64::INFINITY;
        let mut scratch = ScoreScratch::default();
        let aniso_eps = [0.01, 0.02];
        let mut scale_candidates = self.scale_candidates.to_vec();
        if let Some(h) = hint.map(|h| h.geom) {
            for d in [-0.02, -0.01, 0.0, 0.01, 0.02] {
                let s = ((h.sx + h.sy) * 0.5 + d).clamp(0.85, 1.15);
                scale_candidates.push(s);
            }
        }
        scale_candidates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        scale_candidates.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

        let hint_geom = hint.map(|h| h.geom);
        let hint_dx = hint_geom.map(|h| h.dx).unwrap_or(0.0);
        let hint_dy = hint_geom.map(|h| h.dy).unwrap_or(0.0);
        let first_shift_px = if hint_geom.is_some() {
            self.locator_shift_search_px.min(3)
        } else {
            self.locator_shift_search_px
        };
        self.search_candidates_once(
            bx_n,
            by_n,
            locator_size,
            expected_corners,
            &scale_candidates,
            hint_dx,
            hint_dy,
            first_shift_px,
            &aniso_eps,
            &mut best,
            &mut best_score,
            &mut second_best,
            &mut scratch,
            &mut sample_block,
        );

        let (mut best, _best_score_after_refine) = self.refine_geom(
            bx_n,
            by_n,
            locator_size,
            expected_corners,
            best,
            best_score,
            &mut scratch,
            &mut sample_block,
        );
        best_score = self.score_geom(bx_n, by_n, locator_size, expected_corners, best, &mut scratch, &mut sample_block);

        let preliminary_gap = (second_best - best_score).max(0.0);
        let preliminary_conf = if !best_score.is_finite() || !second_best.is_finite() {
            0.0
        } else {
            (preliminary_gap / (best_score.abs() + 1.0)).clamp(0.0, 1.0)
        };
        let allow_wide_retry = match hint {
            None => true,
            Some(h) => h.low_conf_streak >= 2,
        };
        if allow_wide_retry && preliminary_conf < 0.02 {
            self.search_candidates_once(
                bx_n,
                by_n,
                locator_size,
                expected_corners,
                &scale_candidates,
                hint_dx,
                hint_dy,
                (self.locator_shift_search_px * 2).max(self.locator_shift_search_px + 2),
                &[],
                &mut best,
                &mut best_score,
                &mut second_best,
                &mut scratch,
                &mut sample_block,
            );
            let (b2, _s2) = self.refine_geom(
                bx_n,
                by_n,
                locator_size,
                expected_corners,
                best,
                best_score,
                &mut scratch,
                &mut sample_block,
            );
            best = b2;
            best_score = self.score_geom(bx_n, by_n, locator_size, expected_corners, best, &mut scratch, &mut sample_block);
        }

        let gap = (second_best - best_score).max(0.0);
        let confidence = if !best_score.is_finite() || !second_best.is_finite() {
            0.0
        } else {
            (gap / (best_score.abs() + 1.0)).clamp(0.0, 1.0)
        };
        GeomEstimate { geom: best, best_score, second_best_score: second_best, score_gap: gap, confidence }
    }

    pub(crate) fn locator_thresholds_from_known_corners<F>(
        &self,
        bx_n: usize,
        by_n: usize,
        locator_size: usize,
        geom: Geom,
        mut sample_corner_median: F,
    ) -> LocatorThresholds
    where
        F: FnMut(usize, usize, Geom) -> (f64, f64, f64),
    {
        let corners = self.corner_positions(bx_n, by_n, locator_size);
        let mut c = [(0.0, 0.0, 0.0); 4];
        for (i, &(bx, by)) in corners.iter().enumerate() {
            c[i] = sample_corner_median(bx, by, geom);
        }
        LocatorThresholds {
            y: avg2(c[0].0, c[2].0) * 0.5 + avg2(c[1].0, c[3].0) * 0.5,
            u: avg2(c[0].1, c[1].1) * 0.5 + avg2(c[2].1, c[3].1) * 0.5,
            v: avg2(c[0].2, c[3].2) * 0.5 + avg2(c[1].2, c[2].2) * 0.5,
        }
    }
}
