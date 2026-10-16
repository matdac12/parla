//! asr-bench <onnx|gguf|stream> <model> <arg> <wav>...
//!   onnx   <model_dir>  -          : Parla's provider path (ParakeetModel Int8, default params)
//!   gguf   <model.gguf> <lang|->   : transcribe-cpp offline run
//!   stream <model.gguf> <att_right>: transcribe-cpp streaming, 100 ms feeds, then finalize
//! TAIL_S=<seconds> keeps only the last N seconds of every clip (Parla's key-up tail).
//! Output: "<file>\t<ms>\t<audio_s>\t<text>[\t<sum_feed_ms>\t<max_feed_ms>\t<first_text_s>]"
//! where <ms> is the key-up cost: the whole run, or finalize() for streams.
use std::{path::Path, time::Instant};

fn read(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).unwrap();
    let s = r.spec();
    assert!(s.sample_rate == 16000 && s.channels == 1, "{path}: need 16 kHz mono");
    let mut pcm: Vec<f32> = match s.sample_format {
        hound::SampleFormat::Int => r.samples::<i16>().map(|x| x.unwrap() as f32 / 32768.0).collect(),
        hound::SampleFormat::Float => r.samples::<f32>().map(|x| x.unwrap()).collect(),
    };
    if let Ok(tail) = std::env::var("TAIL_S") {
        let keep = (tail.parse::<f32>().unwrap() * 16000.0) as usize;
        if pcm.len() > keep {
            pcm.drain(..pcm.len() - keep);
        }
    }
    pcm
}

fn name(path: &str) -> String {
    Path::new(path).file_name().unwrap().to_string_lossy().into_owned()
}

fn clean(text: &str) -> String {
    text.trim().replace(['\t', '\n'], " ")
}

fn onnx(dir: &str, wavs: &[String]) {
    use transcribe_rs::onnx::{parakeet::{ParakeetModel, ParakeetParams}, Quantization};
    let t = Instant::now();
    let mut model = ParakeetModel::load(Path::new(dir), &Quantization::Int8).unwrap();
    eprintln!("load_ms\t{}", t.elapsed().as_millis());
    model.transcribe_with(&[0.0; 8000], &ParakeetParams::default()).unwrap();
    for path in wavs {
        let pcm = read(path);
        let t = Instant::now();
        let text = model.transcribe_with(&pcm, &ParakeetParams::default()).unwrap().text;
        println!("{}\t{}\t{:.2}\t{}", name(path), t.elapsed().as_millis(), pcm.len() as f32 / 16000.0, clean(&text));
    }
}

fn gguf(model_path: &str, arg: &str, wavs: &[String], stream: bool) {
    use transcribe_cpp::{Model, ParakeetStreamOptions, RunOptions, StreamExtension, StreamOptions};
    let t = Instant::now();
    let model = Model::load(model_path).unwrap();
    let mut session = model.session().unwrap();
    eprintln!("load_ms\t{}", t.elapsed().as_millis());
    let run = RunOptions {
        language: (!stream && arg != "-").then(|| arg.to_string()),
        ..Default::default()
    };
    let stream_opts = StreamOptions {
        family: (stream && arg != "-").then(|| {
            StreamExtension::ParakeetStream(ParakeetStreamOptions { att_context_right: Some(arg.parse().unwrap()) })
        }),
        ..Default::default()
    };
    session.run(&[0.0; 8000], &run).unwrap();
    for path in wavs {
        let pcm = read(path);
        let secs = pcm.len() as f32 / 16000.0;
        if !stream {
            let t = Instant::now();
            let text = session.run(&pcm, &run).unwrap().text;
            println!("{}\t{}\t{secs:.2}\t{}", name(path), t.elapsed().as_millis(), clean(&text));
            continue;
        }
        let mut s = session.stream(&run, &stream_opts).unwrap();
        let (mut sum, mut max, mut first) = (0f64, 0f64, -1f32);
        for (i, chunk) in pcm.chunks(1600).enumerate() {
            let t = Instant::now();
            s.feed(chunk).unwrap();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            sum += ms;
            max = max.max(ms);
            if first < 0.0 && !s.text().full.trim().is_empty() {
                first = (i + 1) as f32 * 0.1;
            }
        }
        let t = Instant::now();
        s.finalize().unwrap();
        let fin = t.elapsed().as_millis();
        println!("{}\t{fin}\t{secs:.2}\t{}\t{sum:.0}\t{max:.0}\t{first:.1}", name(path), clean(&s.text().full));
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "onnx" => onnx(&a[2], &a[4..]),
        "gguf" => gguf(&a[2], &a[3], &a[4..], false),
        "stream" => gguf(&a[2], &a[3], &a[4..], true),
        other => panic!("unknown mode {other}"),
    }
}
