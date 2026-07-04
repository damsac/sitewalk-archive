// Table 1: feasibility + RTF + peak memory.
//
// RTF is measured on the SECOND decode — the first is a discarded warm-up because Metal
// JIT-compiles its shaders on the first decode, which would inflate small-model timings the
// worst (exactly the model-vs-model comparison the decision leans on).

use crate::{decode, load_wav_16k_mono, make_ctx, model_label, peak_rss_mb};
use std::collections::HashMap;

pub fn run(flags: &HashMap<String, String>) {
    let model = flags.get("model").expect("--model required");
    let audio = flags.get("audio").expect("--audio required");

    let (samples, audio_secs) = load_wav_16k_mono(audio);

    // --- load ---
    let t_load = std::time::Instant::now();
    let ctx = make_ctx(model);
    let load_secs = t_load.elapsed().as_secs_f64();

    // --- warm-up decode (DISCARDED — Metal shader JIT happens here) ---
    let _warm = decode(&ctx, &samples, None);

    // --- timed decode (2nd) ---
    let d = decode(&ctx, &samples, None);
    let rtf = d.decode_secs / audio_secs;

    let peak = peak_rss_mb();
    let label = model_label(model);

    eprintln!(
        "[bench] {label}: load={load_secs:.2}s  decode={:.2}s  audio={audio_secs:.1}s  RTF={rtf:.3}  peakRSS={peak:.0}MB  segments={}",
        d.decode_secs,
        d.segments.len()
    );
    eprintln!("[bench] transcript preview: {}", preview(&d.text, 160));

    // RESULTS.md Table-1 row (Model | Quant | Size(MB) | Load(s) | RTF | PeakRSS(MB) | Backend | Notes)
    let size_mb = std::fs::metadata(model).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    println!(
        "| {label} | (see filename) | {size_mb:.0} | {load_secs:.2} | {rtf:.3} | {peak:.0} | metal | RTF=2nd decode, warm-up discarded; audio {audio_secs:.0}s |"
    );
}

fn preview(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}
