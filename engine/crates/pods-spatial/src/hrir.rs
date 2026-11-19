//! Head-related impulse responses, read from a SOFA file through libmysofa.
//!
//! The renderer needs one HRIR pair per whole degree of azimuth on the
//! horizontal plane. They are all computed once at start (the same
//! `mysofa_getfilter_float` interpolation the PipeWire sofa plugin uses), so
//! the real-time thread never calls into libmysofa. The diffuse-field
//! equalisation ([`crate::eq`]) also reads every measurement of the file
//! ([`Measurements`]).

use std::ffi::{CString, c_char, c_float, c_int, c_uint, c_void};
use std::path::Path;
use std::ptr::NonNull;

use crate::Error;
use crate::conf::RADIUS_M;

// The public structures of mysofa.h (stable since libmysofa 0.6): only read,
// to reach every measurement of the file after `mysofa_open` resampled and
// normalised them. Their sizes are checked against each other before use.
#[repr(C)]
struct MysofaArray {
    values: *mut c_float,
    elements: c_uint,
    attributes: *mut c_void,
}

#[repr(C)]
struct MysofaHrtf {
    i: c_uint,
    c: c_uint,
    r: c_uint,
    e: c_uint,
    n: c_uint,
    m: c_uint,
    listener_position: MysofaArray,
    receiver_position: MysofaArray,
    source_position: MysofaArray,
    emitter_position: MysofaArray,
    listener_up: MysofaArray,
    listener_view: MysofaArray,
    data_ir: MysofaArray,
    data_sampling_rate: MysofaArray,
    data_delay: MysofaArray,
    attributes: *mut c_void,
    variables: *mut c_void,
}

#[repr(C)]
struct MysofaEasy {
    hrtf: *mut MysofaHrtf,
    lookup: *mut c_void,
    neighborhood: *mut c_void,
    fir: *mut c_float,
}

#[link(name = "mysofa")]
unsafe extern "C" {
    fn mysofa_open(
        filename: *const c_char,
        samplerate: c_float,
        filterlength: *mut c_int,
        err: *mut c_int,
    ) -> *mut MysofaEasy;
    fn mysofa_getfilter_float(
        easy: *mut MysofaEasy,
        x: c_float,
        y: c_float,
        z: c_float,
        ir_left: *mut c_float,
        ir_right: *mut c_float,
        delay_left: *mut c_float,
        delay_right: *mut c_float,
    );
    fn mysofa_s2c(values: *mut c_float);
    fn mysofa_close(easy: *mut MysofaEasy);
}

/// Number of azimuths in a set: one per whole degree.
pub const AZIMUTHS: usize = 360;

/// Ear index in a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ear {
    Left = 0,
    Right = 1,
}

/// How the renderer handles the bass of an equalised set: below
/// `crossover_hz` the stereo input goes straight to the ears (delayed by
/// `dry_delay` samples to stay in phase with the binaural band), above it
/// the binaural rendering; 4th-order Linkwitz-Riley filters between the two.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BassPath {
    pub crossover_hz: f32,
    pub dry_delay: usize,
}

/// One HRIR pair per whole degree of azimuth, elevation 0.
#[derive(Debug, Clone)]
pub struct HrirSet {
    rate: u32,
    len: usize,
    /// `AZIMUTHS * 2 * len` samples: azimuth-major, then ear.
    irs: Vec<f32>,
    /// `None`: full-band binaural (the raw set, or a synthetic one).
    bass: Option<BassPath>,
}

/// An open SOFA file (libmysofa "easy" handle), closed on drop.
struct SofaFile {
    easy: NonNull<MysofaEasy>,
    filter_len: usize,
    rate: u32,
}

impl SofaFile {
    /// Opens a SOFA file, resampled to `rate` and loudness-normalised by
    /// libmysofa (exactly like the PipeWire sofa plugin).
    fn open(path: &Path, rate: u32) -> Result<Self, Error> {
        let cpath = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| Error::Sofa("SOFA path contains a NUL byte".into()))?;
        let mut filter_len: c_int = 0;
        let mut err: c_int = 0;
        // SAFETY: valid C string, out-pointers to locals.
        let easy = unsafe { mysofa_open(cpath.as_ptr(), rate as c_float, &mut filter_len, &mut err) };
        let easy = NonNull::new(easy)
            .ok_or_else(|| Error::Sofa(format!("cannot open {} (libmysofa error {err})", path.display())))?;
        let file = Self { easy, filter_len: filter_len.max(0) as usize, rate };
        if file.filter_len == 0 {
            return Err(Error::Sofa(format!("{}: empty impulse responses", path.display())));
        }
        Ok(file)
    }

    /// The pair for one direction (interpolated by libmysofa), with the
    /// SOFA delays as whole samples.
    fn pair(&self, az_deg: f32, el_deg: f32) -> (Vec<f32>, Vec<f32>, usize, usize) {
        let n = self.filter_len;
        let mut c = [az_deg as c_float, el_deg as c_float, RADIUS_M];
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        let (mut dl, mut dr) = (0.0f32, 0.0f32);
        // SAFETY: `c` holds 3 floats; l and r hold filter_len floats each;
        // the handle is live.
        unsafe {
            mysofa_s2c(c.as_mut_ptr());
            mysofa_getfilter_float(
                self.easy.as_ptr(),
                c[0],
                c[1],
                c[2],
                l.as_mut_ptr(),
                r.as_mut_ptr(),
                &mut dl,
                &mut dr,
            );
        }
        // SOFA delays are in seconds; KEMAR has none (the ITD is inside
        // the IRs) but other files may. Applied as whole samples.
        let rate = self.rate as f32;
        let to_samples = |d: f32| if d.is_finite() && d > 0.0 { (d * rate).round() as usize } else { 0 };
        (l, r, to_samples(dl).min(4096), to_samples(dr).min(4096))
    }

    /// Every measurement of the file, as resampled and normalised by
    /// `mysofa_open` (the same scale as [`SofaFile::pair`]).
    fn measurements(&self) -> Result<Measurements, Error> {
        let bad = |what: &str| Error::Sofa(format!("unexpected SOFA layout: {what}"));
        // SAFETY: the handle is live; mysofa_open filled `hrtf` and left it
        // in cartesian coordinates. Every size is checked before a slice
        // is made from it.
        unsafe {
            let hrtf = (*self.easy.as_ptr()).hrtf;
            if hrtf.is_null() {
                return Err(bad("no HRTF"));
            }
            let h = &*hrtf;
            let (m, r, n, c) = (h.m as usize, h.r as usize, h.n as usize, h.c as usize);
            if r != 2 || c != 3 || n != self.filter_len || m == 0 {
                return Err(bad(&format!("M={m} R={r} N={n} C={c}")));
            }
            let ir = &h.data_ir;
            let pos = &h.source_position;
            if ir.values.is_null() || ir.elements as usize != m * r * n {
                return Err(bad("DataIR size"));
            }
            if pos.values.is_null() || pos.elements as usize != m * c {
                return Err(bad("SourcePosition size"));
            }
            let irs = std::slice::from_raw_parts(ir.values, m * r * n).to_vec();
            let xyz = std::slice::from_raw_parts(pos.values, m * c);
            let dirs = xyz
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| {
                    let norm = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
                    if norm > 0.0 { [p[0] / norm, p[1] / norm, p[2] / norm] } else { [1.0, 0.0, 0.0] }
                })
                .collect();
            Ok(Measurements { rate: self.rate, len: n, dirs, irs })
        }
    }
}

impl Drop for SofaFile {
    fn drop(&mut self) {
        // SAFETY: returned by mysofa_open, not used afterwards.
        unsafe { mysofa_close(self.easy.as_ptr()) };
    }
}

/// Every measurement of a SOFA file: one direction and one pair of impulse
/// responses each, used for the diffuse-field average.
#[derive(Debug, Clone)]
pub struct Measurements {
    pub rate: u32,
    /// Length of every impulse response.
    pub len: usize,
    /// Unit vectors in SOFA axes (x ahead, y left, z up).
    pub dirs: Vec<[f32; 3]>,
    /// `dirs.len() * 2 * len` samples: measurement-major, then ear.
    pub irs: Vec<f32>,
}

impl Measurements {
    pub fn ir(&self, i: usize, ear: Ear) -> &[f32] {
        let base = (i * 2 + ear as usize) * self.len;
        &self.irs[base..base + self.len]
    }
}

impl HrirSet {
    /// Loads a SOFA file as it is (the "raw" tone): the HRIRs as measured,
    /// resampled and loudness-normalised by libmysofa, exactly like the
    /// PipeWire sofa plugin.
    pub fn load_sofa(path: &Path, rate: u32) -> Result<Self, Error> {
        Ok(Self::from_file(&SofaFile::open(path, rate)?))
    }

    /// Loads a SOFA file and every one of its measurements (for the
    /// diffuse-field equalisation, see [`crate::eq`]).
    pub fn load_sofa_with_measurements(path: &Path, rate: u32) -> Result<(Self, Measurements), Error> {
        let file = SofaFile::open(path, rate)?;
        Ok((Self::from_file(&file), file.measurements()?))
    }

    fn from_file(file: &SofaFile) -> Self {
        let n = file.filter_len;
        let pairs: Vec<_> = (0..AZIMUTHS).map(|az| file.pair(az as f32, 0.0)).collect();
        let max_delay = pairs.iter().map(|p| p.2.max(p.3)).max().unwrap_or(0);
        let len = n + max_delay;
        let mut irs = vec![0.0f32; AZIMUTHS * 2 * len];
        for (az, (l, r, dl, dr)) in pairs.into_iter().enumerate() {
            let base = az * 2 * len;
            irs[base + dl..base + dl + n].copy_from_slice(&l);
            irs[base + len + dr..base + len + dr + n].copy_from_slice(&r);
        }
        Self { rate: file.rate, len, irs, bass: None }
    }

    pub(crate) fn from_parts(rate: u32, len: usize, irs: Vec<f32>, bass: Option<BassPath>) -> Self {
        assert_eq!(irs.len(), AZIMUTHS * 2 * len);
        Self { rate, len, irs, bass }
    }

    /// Builds a set from a function (tests, synthetic HRIRs).
    pub fn from_fn(rate: u32, len: usize, mut f: impl FnMut(usize, Ear) -> Vec<f32>) -> Self {
        let mut irs = vec![0.0f32; AZIMUTHS * 2 * len];
        for az in 0..AZIMUTHS {
            for ear in [Ear::Left, Ear::Right] {
                let ir = f(az, ear);
                let n = ir.len().min(len);
                let base = (az * 2 + ear as usize) * len;
                irs[base..base + n].copy_from_slice(&ir[..n]);
            }
        }
        Self { rate, len, irs, bass: None }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Length of every impulse response, in samples.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The dry bass path of an equalised set (see [`crate::eq`]).
    pub fn bass(&self) -> Option<BassPath> {
        self.bass
    }

    /// The impulse response for a whole-degree azimuth (wrapped) and an ear.
    pub fn ir(&self, az_deg: usize, ear: Ear) -> &[f32] {
        let base = ((az_deg % AZIMUTHS) * 2 + ear as usize) * self.len;
        &self.irs[base..base + self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conf::DEFAULT_SOFA;

    fn energy(x: &[f32]) -> f64 {
        x.iter().map(|&v| (v as f64) * (v as f64)).sum()
    }

    #[test]
    fn kemar_follows_the_angle_convention() {
        let path = Path::new(DEFAULT_SOFA);
        if !path.exists() {
            eprintln!("skipped: {DEFAULT_SOFA} not installed");
            return;
        }
        let set = HrirSet::load_sofa(path, 48_000).unwrap();
        // The bench measured 558 taps for KEMAR at 48 kHz.
        assert!((500..700).contains(&set.len()), "len {}", set.len());
        // 90° is on the left: the left ear is much louder.
        let l90 = energy(set.ir(90, Ear::Left));
        let r90 = energy(set.ir(90, Ear::Right));
        assert!(10.0 * (l90 / r90).log10() > 8.0);
        // 270° mirrors it.
        let l270 = energy(set.ir(270, Ear::Left));
        let r270 = energy(set.ir(270, Ear::Right));
        assert!(10.0 * (r270 / l270).log10() > 8.0);
        // 30°: left louder, as measured on the bench (8.4 dB).
        let d30 = 10.0 * (energy(set.ir(30, Ear::Left)) / energy(set.ir(30, Ear::Right))).log10();
        assert!((4.0..14.0).contains(&d30), "30° ILD {d30} dB");
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(HrirSet::load_sofa(Path::new("/nonexistent.sofa"), 48_000).is_err());
    }
}
