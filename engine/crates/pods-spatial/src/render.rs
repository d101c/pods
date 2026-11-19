//! Click-free binaural renderer for the head-tracked mode.
//!
//! Two virtual speakers (left input at +30° − yaw, right input at −30° − yaw)
//! are convolved with the KEMAR HRIRs by FFT, overlap-save style: every block
//! is computed from the last `ir_len − 1` input samples plus the new ones.
//! That input history is shared by every filter, so when the angle changes
//! the old and the new convolutions both see the full past ("warm"
//! convolvers) and a one-block crossfade between them leaves no transient.
//! The PipeWire sofa plugin, by contrast, starts its new convolver with an
//! empty history, which is what makes it click on every update.
//!
//! With an equalised set (see [`crate::eq`]) the bass is not rendered: below
//! the crossover the stereo input goes straight to the ears, and 4th-order
//! Linkwitz-Riley filters join it to the binaural band.
//!
//! The renderer adds no latency: each call outputs exactly the samples it
//! received, convolved. It never allocates after [`Renderer::new`], so it can
//! run in the real-time thread. Both modes use it: Fixed simply never
//! changes the angle.

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::angle::{SPEAKER_SPREAD_DEG, YawGate, degree_index};
use crate::hrir::{AZIMUTHS, BassPath, Ear, HrirSet};
use crate::xover::Lr4;

/// Largest number of samples rendered by one FFT pass (a PipeWire quantum
/// of 1024 is one pass; larger quanta are split, smaller ones take one).
pub const BLOCK: usize = 1024;

/// The HRIR spectra for every whole degree, precomputed once.
pub struct SpectraTable {
    rate: u32,
    n_fft: usize,
    ir_len: usize,
    bins: usize,
    /// `AZIMUTHS * 2 * bins`, azimuth-major then ear; already scaled by 1/n_fft.
    data: Vec<Complex32>,
    bass: Option<BassPath>,
}

impl SpectraTable {
    pub fn new(set: &HrirSet) -> Self {
        let ir_len = set.len().max(1);
        let n_fft = (BLOCK + ir_len - 1).next_power_of_two();
        let bins = n_fft / 2 + 1;
        let fwd = RealFftPlanner::<f32>::new().plan_fft_forward(n_fft);
        let mut time = fwd.make_input_vec();
        let mut spec = fwd.make_output_vec();
        let mut data = Vec::with_capacity(AZIMUTHS * 2 * bins);
        let scale = 1.0 / n_fft as f32;
        for az in 0..AZIMUTHS {
            for ear in [Ear::Left, Ear::Right] {
                time.fill(0.0);
                for (t, &v) in time.iter_mut().zip(set.ir(az, ear)) {
                    *t = v * scale;
                }
                fwd.process(&mut time, &mut spec).expect("FFT sizes are consistent");
                data.extend_from_slice(&spec);
            }
        }
        Self { rate: set.rate(), n_fft, ir_len, bins, data, bass: set.bass() }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn ir_len(&self) -> usize {
        self.ir_len
    }

    pub fn fft_len(&self) -> usize {
        self.n_fft
    }

    /// The dry bass path, for an equalised set.
    pub fn bass(&self) -> Option<BassPath> {
        self.bass
    }

    fn spectrum(&self, az: usize, ear: usize) -> &[Complex32] {
        let base = (az * 2 + ear) * self.bins;
        &self.data[base..base + self.bins]
    }
}

/// Speaker azimuth indices (left speaker, right speaker) for a whole-degree yaw.
fn speakers_for(yaw_deg: i32) -> (usize, usize) {
    let yaw = yaw_deg as f32;
    (degree_index(SPEAKER_SPREAD_DEG - yaw), degree_index(-SPEAKER_SPREAD_DEG - yaw))
}

/// The dry bass path: delay line, low-pass on the input, high-pass on the
/// binaural output, per channel.
struct Bass {
    lp: [Lr4; 2],
    hp: [Lr4; 2],
    /// `dry_delay` samples per channel (empty: no delay).
    line: [Vec<f32>; 2],
    pos: usize,
}

impl Bass {
    fn new(path: BassPath, rate: u32) -> Self {
        let (fc, rate) = (path.crossover_hz as f64, rate as f64);
        Self {
            lp: [Lr4::lowpass(fc, rate); 2],
            hp: [Lr4::highpass(fc, rate); 2],
            line: [vec![0.0; path.dry_delay], vec![0.0; path.dry_delay]],
            pos: 0,
        }
    }

    fn reset(&mut self) {
        for ch in 0..2 {
            self.lp[ch].reset();
            self.hp[ch].reset();
            self.line[ch].fill(0.0);
        }
        self.pos = 0;
    }

    /// `out` holds the binaural signal of `input`'s samples; replaces it
    /// with high-passed binaural + delayed, low-passed dry input.
    fn mix(&mut self, ch: usize, input: &[f32], out: &mut [f32]) -> usize {
        let line = &mut self.line[ch];
        let (lp, hp) = (&mut self.lp[ch], &mut self.hp[ch]);
        let mut pos = self.pos;
        for (&x, y) in input.iter().zip(out.iter_mut()) {
            let dry = if line.is_empty() {
                x
            } else {
                let d = std::mem::replace(&mut line[pos], x);
                pos += 1;
                if pos == line.len() {
                    pos = 0;
                }
                d
            };
            *y = (hp.tick(*y as f64) + lp.tick(dry as f64)) as f32;
        }
        lp.flush_tiny();
        hp.flush_tiny();
        pos
    }
}

/// The stereo → binaural renderer. One instance per PipeWire filter.
pub struct Renderer {
    table: Arc<SpectraTable>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    /// Shared input history, `ir_len − 1` samples per input channel.
    hist: [Vec<f32>; 2],
    time: Vec<f32>,
    spec_in: [Vec<Complex32>; 2],
    acc: Vec<Complex32>,
    scratch_fwd: Vec<Complex32>,
    scratch_inv: Vec<Complex32>,
    new_out: [Vec<f32>; 2],
    old_out: [Vec<f32>; 2],
    gate: YawGate,
    current: (usize, usize),
    chunk: usize,
    transitions: u64,
    bass: Option<Bass>,
}

impl Renderer {
    pub fn new(table: Arc<SpectraTable>) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let n = table.n_fft;
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);
        let p = table.ir_len - 1;
        let chunk = BLOCK.min(n - p);
        let scratch_fwd = fwd.make_scratch_vec();
        let scratch_inv = inv.make_scratch_vec();
        Self {
            hist: [vec![0.0; p], vec![0.0; p]],
            time: vec![0.0; n],
            spec_in: [fwd.make_output_vec(), fwd.make_output_vec()],
            acc: fwd.make_output_vec(),
            scratch_fwd,
            scratch_inv,
            new_out: [vec![0.0; chunk], vec![0.0; chunk]],
            old_out: [vec![0.0; chunk], vec![0.0; chunk]],
            gate: YawGate::default(),
            current: speakers_for(0),
            chunk,
            transitions: 0,
            bass: table.bass.map(|b| Bass::new(b, table.rate)),
            fwd,
            inv,
            table,
        }
    }

    /// Number of angle changes applied so far (each one is a crossfade).
    pub fn transitions(&self) -> u64 {
        self.transitions
    }

    /// The whole-degree yaw being rendered.
    pub fn rendered_yaw(&self) -> i32 {
        self.gate.applied()
    }

    /// Forgets the input history (after a pause in the stream).
    pub fn reset(&mut self) {
        for h in &mut self.hist {
            h.fill(0.0);
        }
        if let Some(b) = &mut self.bass {
            b.reset();
        }
    }

    /// Renders one block. The yaw is sampled once per call (once per PipeWire
    /// cycle): if it left the dead zone, the first FFT pass crossfades from
    /// the old angle to the new one. All slices must have the same length.
    pub fn process(&mut self, yaw_deg: f32, in_l: &[f32], in_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
        let n = in_l.len().min(in_r.len()).min(out_l.len()).min(out_r.len());
        let target = match self.gate.update(yaw_deg) {
            Some(y) => speakers_for(y),
            None => self.current,
        };
        let mut fade_from = (target != self.current).then_some(self.current);
        if fade_from.is_some() {
            self.transitions += 1;
        }
        self.current = target;

        let mut off = 0;
        while off < n {
            let m = (n - off).min(self.chunk);
            self.analyse(&in_l[off..off + m], &in_r[off..off + m]);
            self.synthesise(target, m, false);
            if let Some(old) = fade_from.take() {
                self.synthesise(old, m, true);
                for ear in 0..2 {
                    let (new, old) = (&mut self.new_out[ear], &self.old_out[ear]);
                    for (i, (nv, &ov)) in new[..m].iter_mut().zip(&old[..m]).enumerate() {
                        let w = 0.5 - 0.5 * (std::f32::consts::PI * (i + 1) as f32 / m as f32).cos();
                        *nv = ov + w * (*nv - ov);
                    }
                }
            }
            out_l[off..off + m].copy_from_slice(&self.new_out[0][..m]);
            out_r[off..off + m].copy_from_slice(&self.new_out[1][..m]);
            if let Some(b) = &mut self.bass {
                b.mix(0, &in_l[off..off + m], &mut out_l[off..off + m]);
                b.pos = b.mix(1, &in_r[off..off + m], &mut out_r[off..off + m]);
            }
            off += m;
        }
    }

    /// Forward FFT of [history | new samples] for both inputs, then slides
    /// the shared history.
    fn analyse(&mut self, in_l: &[f32], in_r: &[f32]) {
        let p = self.hist[0].len();
        let m = in_l.len();
        for (ch, input) in [in_l, in_r].into_iter().enumerate() {
            let hist = &mut self.hist[ch];
            self.time[..p].copy_from_slice(hist);
            self.time[p..p + m].copy_from_slice(input);
            self.time[p + m..].fill(0.0);
            self.fwd
                .process_with_scratch(&mut self.time, &mut self.spec_in[ch], &mut self.scratch_fwd)
                .expect("FFT sizes are consistent");
            if m >= p {
                hist.copy_from_slice(&input[m - p..]);
            } else {
                hist.copy_within(m.., 0);
                hist[p - m..].copy_from_slice(input);
            }
        }
    }

    /// Both ears for a pair of speaker azimuths, from the current spectra.
    fn synthesise(&mut self, speakers: (usize, usize), m: usize, into_old: bool) {
        let p = self.hist[0].len();
        for ear in 0..2 {
            let hl = self.table.spectrum(speakers.0, ear);
            let hr = self.table.spectrum(speakers.1, ear);
            for (k, a) in self.acc.iter_mut().enumerate() {
                *a = self.spec_in[0][k] * hl[k] + self.spec_in[1][k] * hr[k];
            }
            // DC and Nyquist are real for real signals; clear rounding noise
            // (realfft rejects a non-zero imaginary part there).
            if let Some(first) = self.acc.first_mut() {
                first.im = 0.0;
            }
            if let Some(last) = self.acc.last_mut() {
                last.im = 0.0;
            }
            self.inv
                .process_with_scratch(&mut self.acc, &mut self.time, &mut self.scratch_inv)
                .expect("FFT sizes are consistent");
            let dst = if into_old { &mut self.old_out[ear] } else { &mut self.new_out[ear] };
            dst[..m].copy_from_slice(&self.time[p..p + m]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random numbers in [−1, 1).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    /// Synthetic HRIRs: broadband, decaying, different for every azimuth
    /// and ear (like real ones), length 300.
    fn synthetic_set() -> HrirSet {
        let mut rng = Lcg(7);
        HrirSet::from_fn(48_000, 300, |az, ear| {
            let onset = 5 + (az % 40) + ear as usize * 3;
            (0..300)
                .map(|i| if i < onset { 0.0 } else { rng.next() * (-((i - onset) as f32) / 60.0).exp() * 0.3 })
                .collect()
        })
    }

    /// y[t] = Σ ir[k]·x[t−k], with x = 0 before `from`.
    fn conv_at(x: &[f32], ir: &[f32], t: usize, from: usize) -> f32 {
        let mut s = 0.0f64;
        for (k, &h) in ir.iter().enumerate() {
            if k > t || t - k < from {
                break;
            }
            s += h as f64 * x[t - k] as f64;
        }
        s as f32
    }

    fn binaural_at(set: &HrirSet, xl: &[f32], xr: &[f32], yaw: i32, ear: Ear, t: usize, from: usize) -> f32 {
        let (a, b) = speakers_for(yaw);
        conv_at(xl, set.ir(a, ear), t, from) + conv_at(xr, set.ir(b, ear), t, from)
    }

    fn signal(n: usize, seed: u64) -> Vec<f32> {
        let mut rng = Lcg(seed);
        (0..n).map(|_| rng.next() * 0.5).collect()
    }

    fn run(r: &mut Renderer, yaw_per_block: &[(usize, f32)], xl: &[f32], xr: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let (mut ol, mut or) = (vec![0.0; xl.len()], vec![0.0; xl.len()]);
        let mut off = 0;
        for &(len, yaw) in yaw_per_block {
            r.process(yaw, &xl[off..off + len], &xr[off..off + len], &mut ol[off..off + len], &mut or[off..off + len]);
            off += len;
        }
        (ol, or)
    }

    #[test]
    fn static_output_is_the_exact_convolution() {
        let set = synthetic_set();
        let table = Arc::new(SpectraTable::new(&set));
        // Odd block sizes, one larger than BLOCK (split in two passes).
        let blocks = [64usize, 1000, 3000, 7, 512, 1024];
        let n: usize = blocks.iter().sum();
        let (xl, xr) = (signal(n, 1), signal(n, 2));
        let mut r = Renderer::new(table);
        let (ol, or) = run(&mut r, &blocks.map(|b| (b, 0.0)), &xl, &xr);
        for t in (0..n).step_by(37) {
            let el = binaural_at(&set, &xl, &xr, 0, Ear::Left, t, 0);
            let er = binaural_at(&set, &xl, &xr, 0, Ear::Right, t, 0);
            assert!((ol[t] - el).abs() < 2e-5, "t={t} {} vs {el}", ol[t]);
            assert!((or[t] - er).abs() < 2e-5, "t={t} {} vs {er}", or[t]);
        }
        assert_eq!(r.transitions(), 0);
    }

    #[test]
    fn angle_change_crossfades_between_warm_convolvers() {
        let set = synthetic_set();
        let table = Arc::new(SpectraTable::new(&set));
        let b = 256usize;
        let n = 6 * b;
        let (xl, xr) = (signal(n, 3), signal(n, 4));
        let mut r = Renderer::new(table);
        // Yaw jumps to 40° at block 3.
        let yaws = [0.0, 0.0, 0.0, 40.0, 40.0, 40.0];
        let (ol, _) = run(&mut r, &yaws.map(|y| (b, y)), &xl, &xr);
        assert_eq!(r.transitions(), 1);
        assert_eq!(r.rendered_yaw(), 40);

        let t0 = 3 * b;
        let mut worst_warm = 0.0f32;
        let mut worst_cold = 0.0f32;
        for i in 0..b {
            let t = t0 + i;
            let w = 0.5 - 0.5 * (std::f32::consts::PI * (i + 1) as f32 / b as f32).cos();
            let old = binaural_at(&set, &xl, &xr, 0, Ear::Left, t, 0);
            let new_warm = binaural_at(&set, &xl, &xr, 40, Ear::Left, t, 0);
            // What the sofa plugin does: the new convolver only knows the
            // input from the switch on.
            let new_cold = binaural_at(&set, &xl, &xr, 40, Ear::Left, t, t0);
            let warm = old + w * (new_warm - old);
            let cold = old + w * (new_cold - old);
            worst_warm = worst_warm.max((ol[t] - warm).abs());
            worst_cold = worst_cold.max((ol[t] - cold).abs());
        }
        assert!(worst_warm < 2e-5, "renderer differs from the warm crossfade by {worst_warm}");
        assert!(worst_cold > 1e-2, "test signal too weak to tell warm from cold ({worst_cold})");
        // After the fade: plain convolution at the new angle.
        for t in (4 * b..n).step_by(11) {
            let e = binaural_at(&set, &xl, &xr, 40, Ear::Left, t, 0);
            assert!((ol[t] - e).abs() < 2e-5);
        }
    }

    #[test]
    fn dead_zone_and_wrap() {
        let set = synthetic_set();
        let mut r = Renderer::new(Arc::new(SpectraTable::new(&set)));
        let x = vec![0.0f32; 128];
        let mut o = (vec![0.0f32; 128], vec![0.0f32; 128]);
        for yaw in [0.3, -0.6, 0.9, -0.99] {
            r.process(yaw, &x, &x, &mut o.0, &mut o.1);
        }
        assert_eq!(r.transitions(), 0);
        r.process(-1.2, &x, &x, &mut o.0, &mut o.1);
        assert_eq!((r.transitions(), r.rendered_yaw()), (1, 359));
        r.process(f32::NAN, &x, &x, &mut o.0, &mut o.1);
        assert_eq!(r.transitions(), 1);
        assert_eq!(speakers_for(0), (30, 330));
        assert_eq!(speakers_for(359), (31, 331));
        assert_eq!(speakers_for(45), (345, 285));
    }

    #[test]
    fn kemar_table_fits_one_block() {
        let path = std::path::Path::new(crate::conf::DEFAULT_SOFA);
        if !path.exists() {
            return;
        }
        let (set, m) = HrirSet::load_sofa_with_measurements(path, 48_000).unwrap();
        let t = SpectraTable::new(&set);
        assert_eq!(t.fft_len(), 2048);
        let r = Renderer::new(Arc::new(t));
        assert_eq!(r.chunk, BLOCK);
        // Equalised (longer HRIRs): still one 2048-point FFT per block.
        let (eq, _) = crate::eq::equalise(&set, &m);
        let t = SpectraTable::new(&eq);
        assert_eq!(t.fft_len(), 2048);
        assert_eq!(Renderer::new(Arc::new(t)).chunk, BLOCK);
    }

    #[test]
    fn bass_path_is_dry_and_joins_the_binaural_band() {
        // Every HRIR is 0.5·δ[n − 40]: a centred input reaches each ear
        // through the binaural band as x[n − 40], and the dry path with the
        // same delay. The output must be LR4-LP(x[n − 40]) + LR4-HP(x[n − 40]):
        // an all-pass, no dip at the crossover.
        let delay = 40;
        let ir: Vec<f32> = (0..64).map(|i| if i == delay { 0.5 } else { 0.0 }).collect();
        let base = HrirSet::from_fn(48_000, 64, |_, _| ir.clone());
        let mut irs = Vec::new();
        for az in 0..AZIMUTHS {
            for ear in [Ear::Left, Ear::Right] {
                irs.extend_from_slice(base.ir(az, ear));
            }
        }
        let bass = BassPath { crossover_hz: 150.0, dry_delay: delay };
        let set = HrirSet::from_parts(48_000, 64, irs, Some(bass));
        let mut r = Renderer::new(Arc::new(SpectraTable::new(&set)));
        let n = 6000;
        let x = signal(n, 9);
        let blocks = [256usize, 1024, 700, 4020];
        let (ol, or) = run(&mut r, &blocks.map(|b| (b, 0.0)), &x, &x);
        let (mut lp, mut hp) = (Lr4::lowpass(150.0, 48_000.0), Lr4::highpass(150.0, 48_000.0));
        let mut worst = 0.0f32;
        for t in 0..n {
            let d = if t >= delay { x[t - delay] as f64 } else { 0.0 };
            let e = (lp.tick(d) + hp.tick(d)) as f32;
            worst = worst.max((ol[t] - e).abs()).max((or[t] - e).abs());
        }
        assert!(worst < 1e-4, "differs from LP + HP by {worst}");
        // Head turns do not touch the dry path: still the same after a reset.
        r.reset();
        let (ol2, _) = run(&mut r, &blocks.map(|b| (b, 0.0)), &x, &x);
        assert_eq!(ol, ol2);
    }

    #[test]
    fn equalised_kemar_speaker_is_flat() {
        let path = std::path::Path::new(crate::conf::DEFAULT_SOFA);
        if !path.exists() {
            return;
        }
        let (set, m) = HrirSet::load_sofa_with_measurements(path, 48_000).unwrap();
        let (eq, _) = crate::eq::equalise(&set, &m);
        let mut r = Renderer::new(Arc::new(SpectraTable::new(&eq)));
        // An impulse on the left input only: the left virtual speaker.
        let n = 16_384;
        let mut x = vec![0.0f32; n];
        x[0] = 1.0;
        let z = vec![0.0f32; n];
        let (ol, or) = run(&mut r, &[(n, 0.0)], &x, &z);
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
        let power = |y: &[f32]| {
            let mut t = y.to_vec();
            let mut s = fft.make_output_vec();
            fft.process(&mut t, &mut s).unwrap();
            s.iter().map(|c| c.norm_sqr() as f64).collect::<Vec<_>>()
        };
        let (pl, pr) = (power(&ol), power(&or));
        let bin = 48_000.0 / n as f64;
        // Plain stereo puts the channel in one ear at 0 dB: the sum of both
        // ears, per octave band, must stay within ±3 dB of it.
        for c in [63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0] {
            let ks = ((c / 2f64.sqrt() / bin) as usize)..((c * 2f64.sqrt() / bin) as usize);
            let p = ks.clone().map(|k| pl[k] + pr[k]).sum::<f64>() / ks.len() as f64;
            let p_db = 10.0 * p.log10();
            assert!(p_db.abs() < 3.0, "{c} Hz octave: {p_db:.2} dB");
        }
    }
}
