// stt-whisper-spike — disposable measurement CLI.
// Subcommands: bench | stream | accuracy | bias  (each prints a RESULTS.md-ready row).
//
// NOT a workspace member. Nothing here is production code.

mod bench;
// stream (Task 3) and wer (Task 4) modules are wired in as those tasks land.

use std::collections::HashMap;
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// A `RESULTS.md`-ready decode: full text + timestamped segments.
pub struct Decode {
    pub text: String,
    /// (start_cs, end_cs, text) per segment — timestamps in centiseconds.
    pub segments: Vec<(i64, i64, String)>,
    pub decode_secs: f64,
}

/// Load a 16 kHz mono WAV as f32 samples in [-1, 1]. Returns (samples, duration_secs).
pub fn load_wav_16k_mono(path: &str) -> (Vec<f32>, f64) {
    let mut reader = hound::WavReader::open(path)
        .unwrap_or_else(|e| panic!("open wav {path}: {e}"));
    let spec = reader.spec();
    if spec.sample_rate != 16_000 {
        eprintln!(
            "WARNING: {path} is {} Hz, not 16000 — whisper expects 16 kHz mono; results suspect.",
            spec.sample_rate
        );
    }
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect(),
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    // Downmix to mono if needed.
    let mono: Vec<f32> = if spec.channels == 1 {
        raw
    } else {
        let ch = spec.channels as usize;
        raw.chunks(ch).map(|c| c.iter().sum::<f32>() / ch as f32).collect()
    };
    let dur = mono.len() as f64 / 16_000.0;
    (mono, dur)
}

/// Build a WhisperContext for a model file (Metal backend via the `metal` cargo feature).
pub fn make_ctx(model: &str) -> WhisperContext {
    WhisperContext::new_with_params(model, WhisperContextParameters::default())
        .unwrap_or_else(|e| panic!("load model {model}: {e}"))
}

/// Default FullParams for a deterministic English decode. `initial_prompt` biases the decoder.
pub fn make_params<'a>(initial_prompt: Option<&'a str>) -> FullParams<'a, 'a> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("en"));
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    params.set_translate(false);
    if let Some(p) = initial_prompt {
        params.set_initial_prompt(p);
    }
    params
}

/// Decode `samples` once, timing the `full()` call. Gathers segments + full text.
pub fn decode(ctx: &WhisperContext, samples: &[f32], initial_prompt: Option<&str>) -> Decode {
    let mut state = ctx.create_state().expect("create state");
    let params = make_params(initial_prompt);
    let t0 = std::time::Instant::now();
    state.full(params, samples).expect("full decode");
    let decode_secs = t0.elapsed().as_secs_f64();

    let n = state.full_n_segments();
    let mut segments = Vec::with_capacity(n as usize);
    let mut text = String::new();
    for i in 0..n {
        if let Some(seg) = state.get_segment(i) {
            let s = seg.to_str_lossy().map(|c| c.into_owned()).unwrap_or_default();
            segments.push((seg.start_timestamp(), seg.end_timestamp(), s.clone()));
            text.push_str(&s);
        }
    }
    Decode { text: text.trim().to_string(), segments, decode_secs }
}

// ---- getrusage: peak resident set size. macOS ru_maxrss is in BYTES (Linux: KiB). ----
#[repr(C)]
#[derive(Clone, Copy)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i32,
}
#[repr(C)]
struct Rusage {
    ru_utime: Timeval,
    ru_stime: Timeval,
    ru_maxrss: i64,
    // Remaining `long` fields (ru_ixrss .. ru_nivcsw) — sized so getrusage cannot overrun.
    _rest: [i64; 14],
}
extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}
const RUSAGE_SELF: i32 = 0;

/// Peak resident set size in MB. On macOS `ru_maxrss` is BYTES (baked in — not a prose note).
pub fn peak_rss_mb() -> f64 {
    let mut u = Rusage {
        ru_utime: Timeval { tv_sec: 0, tv_usec: 0 },
        ru_stime: Timeval { tv_sec: 0, tv_usec: 0 },
        ru_maxrss: 0,
        _rest: [0; 14],
    };
    let rc = unsafe { getrusage(RUSAGE_SELF, &mut u) };
    assert_eq!(rc, 0, "getrusage failed");
    // macOS: ru_maxrss is BYTES (Linux would be KiB — this spike targets macOS)
    u.ru_maxrss as f64 / (1024.0 * 1024.0)
}

/// Derive a short label like "base.en" from a model path.
pub fn model_label(model: &str) -> String {
    let stem = Path::new(model)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(model);
    stem.strip_prefix("ggml-").unwrap_or(stem).to_string()
}

fn parse_flags(args: &[String]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(key) = args[i].strip_prefix("--") {
            let val = args.get(i + 1).cloned().unwrap_or_default();
            m.insert(key.to_string(), val);
            i += 2;
        } else {
            i += 1;
        }
    }
    m
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let flags = parse_flags(&args);
    match sub {
        "bench" => bench::run(&flags),
        // "stream"   => wired in Task 3
        // "accuracy" / "bias" => wired in Task 4
        _ => {
            eprintln!("usage: stt-whisper-spike <bench|stream|accuracy|bias> [flags]");
            eprintln!("  bench    --model M --audio A");
            std::process::exit(2);
        }
    }
}
