//! Compares the local Parakeet models (PARLA-37) on the same WAVs, through the
//! same provider code the app uses.
//!
//! ```text
//! cargo run --release --example compare_models -- [options] [WAV ...]
//!   --list FILE     TSV lines: path<TAB>lang<TAB>reference (lang/reference optional)
//!   --fleurs DIR    a FLEURS folder with refs.tsv (file, raw, norm); lang = folder name
//!   --limit N       at most N clips per --fleurs folder
//!   --models LIST   comma list of parakeet-v3,parakeet-ultra (default: both)
//!   --v3 DIR        v3 model folder (default: Parla's models folder)
//!   --ultra DIR     Ultra model folder (default: Parla's models folder)
//!   --runs N        timed runs per clip; ms are medians (default 1)
//!   --no-trim       skip the batch path's silence trim (raw clip to the model)
//!   --tsv FILE      also write one row per model and clip
//!   --quiet         print only the summary
//! ```
//!
//! Per clip it prints the text, the WER against the reference when there is
//! one, ms for the whole clip (the batch path: silence trim, then inference)
//! and ms for its last 4.5 s, which is about what Parla pays at key-up (the
//! last phrase plus context). WER lowercases and drops punctuation; "num" WER
//! also spells out digits in both texts, so "23" and "ventitré" match.
//! Timing needs a quiet machine; accuracy does not.
use parla_lib::{
    clip::{Clip, SAMPLE_RATE},
    models,
    provider::{parakeet, parakeet::Parakeet, SttProvider},
    settings::{LocalModel, Settings},
};
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Instant,
};

const TAIL_SAMPLES: usize = SAMPLE_RATE as usize * 9 / 2;

struct Item {
    path: PathBuf,
    lang: String,
    reference: Option<String>,
}

struct Row {
    text: String,
    ms: f64,
    tail_ms: f64,
    /// (edits, reference words) raw and with numbers spelled out.
    raw: Option<(usize, usize)>,
    num: Option<(usize, usize)>,
    loops: bool,
}

struct Options {
    items: Vec<Item>,
    models: Vec<LocalModel>,
    v3: Option<PathBuf>,
    ultra: Option<PathBuf>,
    runs: usize,
    no_trim: bool,
    tsv: Option<PathBuf>,
    quiet: bool,
}

fn main() {
    let options = match parse(std::env::args().skip(1).collect()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("compare_models: {error}");
            std::process::exit(2);
        }
    };
    if options.items.is_empty() {
        eprintln!("compare_models: no WAVs given (see the file header for usage)");
        std::process::exit(2);
    }
    let clips: Vec<Clip> = options
        .items
        .iter()
        .map(|item| load(&item.path).unwrap_or_else(|e| panic!("{}: {e}", item.path.display())))
        .collect();
    let mut results: Vec<(LocalModel, Vec<Row>)> = Vec::new();
    for &model in &options.models {
        let settings = settings_for(model, &options);
        let dir = models::model_dir(&settings).expect("model folder");
        if !models::available(model, &dir) {
            eprintln!(
                "{}: not downloaded in {}, skipped",
                model.id(),
                dir.display()
            );
            continue;
        }
        // The first inference loads the model and warms the CPU; not timed.
        let started = Instant::now();
        parakeet::wake(&settings).expect("load model");
        eprintln!(
            "{}: loaded in {} ms",
            model.id(),
            started.elapsed().as_millis()
        );
        let rows = options
            .items
            .iter()
            .zip(&clips)
            .map(|(item, clip)| measure(&settings, item, clip, &options))
            .collect();
        results.push((model, rows));
        parakeet::release();
    }
    report(&options, &results);
}

fn settings_for(model: LocalModel, options: &Options) -> Settings {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(|dir| PathBuf::from(dir).join("com.matdac6.parla").join("models"))
        .unwrap_or_default();
    let v3 = options
        .v3
        .clone()
        .unwrap_or_else(|| root.join(models::V3.directory));
    let ultra = options
        .ultra
        .clone()
        .unwrap_or_else(|| root.join(models::ULTRA.directory));
    let mut settings = Settings {
        active_provider: models::ID.into(),
        local_model: model,
        ..Settings::default()
    };
    settings.providers.insert(
        models::ID.into(),
        serde_json::json!({"model_dir": v3, "ultra_model_dir": ultra}),
    );
    settings
}

fn load(path: &Path) -> Result<Clip, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let clip = Clip::from_wav_bytes(&bytes).map_err(|e| e.to_string())?;
    if clip.sample_rate != SAMPLE_RATE {
        return Err(format!("expected 16 kHz, got {} Hz", clip.sample_rate));
    }
    Ok(clip)
}

fn measure(settings: &Settings, item: &Item, clip: &Clip, options: &Options) -> Row {
    let audio: Vec<f32> = clip.samples.iter().map(|&s| s as f32 / 32768.0).collect();
    let mut text = String::new();
    let mut full = Vec::new();
    let mut tail = Vec::new();
    let last = &audio[audio.len().saturating_sub(TAIL_SAMPLES)..];
    for run in 0..options.runs.max(1) {
        let started = Instant::now();
        let result = if options.no_trim {
            parakeet::transcribe_samples(settings, &audio)
        } else {
            Parakeet.transcribe(clip, settings)
        }
        .unwrap_or_else(|e| format!("<error: {e}>"));
        full.push(started.elapsed().as_secs_f64() * 1000.0);
        if run == 0 {
            text = result;
        }
        let started = Instant::now();
        let _ = parakeet::transcribe_samples(settings, last);
        tail.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let reference = item.reference.as_deref();
    Row {
        raw: reference.map(|r| wer(&words(r, None), &words(&text, None))),
        num: reference.map(|r| {
            let lang = Some(item.lang.as_str());
            wer(&words(r, lang), &words(&text, lang))
        }),
        loops: has_loop(&words(&text, None)),
        text,
        ms: median(&mut full),
        tail_ms: median(&mut tail),
    }
}

fn report(options: &Options, results: &[(LocalModel, Vec<Row>)]) {
    let mut tsv = String::from("model\tclip\tlang\tms\ttail_ms\twer\twer_num\tloop\ttext\n");
    for (index, item) in options.items.iter().enumerate() {
        let name = item.path.file_name().unwrap_or_default().to_string_lossy();
        if !options.quiet {
            println!("\n== {name} ({})", item.lang);
            if let Some(reference) = &item.reference {
                println!("   reference      {reference}");
            }
        }
        for (model, rows) in results {
            let row = &rows[index];
            if !options.quiet {
                let mut line = format!("   {:<15}", model.id());
                if let (Some(raw), Some(num)) = (row.raw, row.num) {
                    let _ = write!(line, "WER {:5.1}% num {:5.1}%  ", pct(raw), pct(num));
                }
                let _ = write!(line, "{:6.0} ms  tail {:5.0} ms", row.ms, row.tail_ms);
                if row.loops {
                    line.push_str("  LOOP");
                }
                println!("{line}\n   {:<15}{}", "", row.text);
            }
            let rate = |w: Option<(usize, usize)>| w.map(|w| format!("{:.2}", pct(w)));
            let _ = writeln!(
                tsv,
                "{}\t{name}\t{}\t{:.0}\t{:.0}\t{}\t{}\t{}\t{}",
                model.id(),
                item.lang,
                row.ms,
                row.tail_ms,
                rate(row.raw).unwrap_or_default(),
                rate(row.num).unwrap_or_default(),
                u8::from(row.loops),
                row.text.replace(['\t', '\n'], " ")
            );
        }
    }
    let mut langs: Vec<&str> = options.items.iter().map(|i| i.lang.as_str()).collect();
    langs.sort_unstable();
    langs.dedup();
    println!("\n| model | set | clips | WER | WER num | ms p50 | tail p50 | tail p90 | loops |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|");
    for (model, rows) in results {
        for lang in &langs {
            let picked: Vec<&Row> = options
                .items
                .iter()
                .zip(rows)
                .filter(|(item, _)| item.lang == *lang)
                .map(|(_, row)| row)
                .collect();
            let sum = |f: fn(&Row) -> Option<(usize, usize)>| {
                let parts: Vec<_> = picked.iter().filter_map(|r| f(r)).collect();
                (!parts.is_empty()).then(|| {
                    let edits = parts.iter().map(|p| p.0).sum();
                    let total = parts.iter().map(|p| p.1).sum();
                    format!("{:.2}", pct((edits, total)))
                })
            };
            let mut ms: Vec<f64> = picked.iter().map(|r| r.ms).collect();
            let mut tails: Vec<f64> = picked.iter().map(|r| r.tail_ms).collect();
            println!(
                "| {} | {lang} | {} | {} | {} | {:.0} | {:.0} | {:.0} | {} |",
                model.id(),
                picked.len(),
                sum(|r| r.raw).unwrap_or_else(|| "n/a".into()),
                sum(|r| r.num).unwrap_or_else(|| "n/a".into()),
                median(&mut ms),
                median(&mut tails),
                percentile(&mut tails, 0.9),
                picked.iter().filter(|r| r.loops).count()
            );
        }
    }
    if let Some(path) = &options.tsv {
        std::fs::write(path, tsv).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}

fn parse(args: Vec<String>) -> Result<Options, String> {
    let mut options = Options {
        items: Vec::new(),
        models: LocalModel::ALL.to_vec(),
        v3: None,
        ultra: None,
        runs: 1,
        no_trim: false,
        tsv: None,
        quiet: false,
    };
    let mut fleurs = Vec::new();
    let mut limit = usize::MAX;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--list" => {
                let path = PathBuf::from(value()?);
                let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                let base = path.parent().unwrap_or(Path::new("."));
                for line in text
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
                {
                    let mut fields = line.split('\t');
                    let file = fields.next().unwrap_or_default();
                    options.items.push(Item {
                        path: base.join(file),
                        lang: fields.next().unwrap_or("?").to_string(),
                        reference: fields.next().map(str::to_string),
                    });
                }
            }
            "--fleurs" => fleurs.push(PathBuf::from(value()?)),
            "--limit" => limit = value()?.parse().map_err(|_| "bad --limit")?,
            "--runs" => options.runs = value()?.parse().map_err(|_| "bad --runs")?,
            "--v3" => options.v3 = Some(value()?.into()),
            "--ultra" => options.ultra = Some(value()?.into()),
            "--tsv" => options.tsv = Some(value()?.into()),
            "--quiet" => options.quiet = true,
            "--no-trim" => options.no_trim = true,
            "--models" => {
                options.models = value()?
                    .split(',')
                    .map(|id| LocalModel::from_id(id.trim()).ok_or(format!("unknown model {id}")))
                    .collect::<Result<_, _>>()?;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            path => options.items.push(Item {
                path: path.into(),
                lang: "?".into(),
                reference: None,
            }),
        }
    }
    for dir in fleurs {
        let lang = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let refs = std::fs::read_to_string(dir.join("refs.tsv")).map_err(|e| e.to_string())?;
        for line in refs.lines().filter(|l| !l.trim().is_empty()).take(limit) {
            let mut fields = line.split('\t');
            let (Some(file), Some(raw)) = (fields.next(), fields.next()) else {
                continue;
            };
            options.items.push(Item {
                path: dir.join(file),
                lang: lang.clone(),
                reference: Some(raw.to_string()),
            });
        }
    }
    Ok(options)
}

fn median(values: &mut [f64]) -> f64 {
    percentile(values, 0.5)
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * p).round() as usize]
}

fn pct((edits, total): (usize, usize)) -> f64 {
    if total == 0 {
        0.0
    } else {
        edits as f64 * 100.0 / total as f64
    }
}

/// Lowercase words without punctuation; apostrophes split ("l'orario" is two
/// words). With a language, digit runs are spelled out in that language, and
/// thousands separators ("1.250", "1,250") are dropped first.
fn words(text: &str, lang: Option<&str>) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let digit = |i: usize| chars.get(i).is_some_and(char::is_ascii_digit);
    let mut cleaned = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        let separator = lang.is_some()
            && matches!(c, '.' | ',')
            && i > 0
            && digit(i - 1)
            && (1..=3).all(|k| digit(i + k))
            && !digit(i + 4);
        if separator {
            continue;
        }
        if c.is_alphanumeric() {
            cleaned.extend(c.to_lowercase());
        } else {
            cleaned.push(' ');
        }
    }
    let mut out = Vec::new();
    for word in cleaned.split_whitespace() {
        match (lang, word.parse::<u64>()) {
            (Some(lang), Ok(n)) if word.len() <= 9 => {
                out.extend(spell(n, lang).split_whitespace().map(str::to_string));
            }
            _ => out.push(word.to_string()),
        }
    }
    out
}

fn spell(n: u64, lang: &str) -> String {
    match lang {
        "it" | "mixed" => italian(n),
        "en" => english(n),
        _ => n.to_string(),
    }
}

fn italian(n: u64) -> String {
    const UNITS: [&str; 20] = [
        "zero",
        "uno",
        "due",
        "tre",
        "quattro",
        "cinque",
        "sei",
        "sette",
        "otto",
        "nove",
        "dieci",
        "undici",
        "dodici",
        "tredici",
        "quattordici",
        "quindici",
        "sedici",
        "diciassette",
        "diciotto",
        "diciannove",
    ];
    const TENS: [&str; 10] = [
        "",
        "",
        "venti",
        "trenta",
        "quaranta",
        "cinquanta",
        "sessanta",
        "settanta",
        "ottanta",
        "novanta",
    ];
    fn below_thousand(n: u64) -> String {
        let (hundreds, rest) = (n / 100, n % 100);
        let mut out = match hundreds {
            0 => String::new(),
            1 => "cento".into(),
            h => format!("{}cento", UNITS[h as usize]),
        };
        if rest == 0 {
            return out;
        }
        if rest < 20 {
            out.push_str(UNITS[rest as usize]);
            return out;
        }
        let (tens, unit) = (TENS[(rest / 10) as usize], rest % 10);
        // "ventuno", "trentotto": the tens drop their vowel before 1 and 8.
        let tens = if unit == 1 || unit == 8 {
            &tens[..tens.len() - 1]
        } else {
            tens
        };
        out.push_str(tens);
        match unit {
            0 => {}
            3 => out.push_str("tré"),
            u => out.push_str(UNITS[u as usize]),
        }
        out
    }
    if n == 0 {
        return "zero".into();
    }
    let (millions, thousands, rest) = (n / 1_000_000, n / 1000 % 1000, n % 1000);
    let mut parts = Vec::new();
    match millions {
        0 => {}
        1 => parts.push("un milione".to_string()),
        m => parts.push(format!("{} milioni", below_thousand(m))),
    }
    let mut word = match thousands {
        0 => String::new(),
        1 => "mille".into(),
        t => format!("{}mila", below_thousand(t)),
    };
    word.push_str(&below_thousand(rest));
    if !word.is_empty() {
        parts.push(word);
    }
    parts.join(" ")
}

fn english(n: u64) -> String {
    const UNITS: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 10] = [
        "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    fn below_thousand(n: u64) -> Vec<&'static str> {
        let mut out = Vec::new();
        if n >= 100 {
            out.extend([UNITS[(n / 100) as usize], "hundred"]);
        }
        let rest = n % 100;
        if rest >= 20 {
            out.push(TENS[(rest / 10) as usize]);
            if !rest.is_multiple_of(10) {
                out.push(UNITS[(rest % 10) as usize]);
            }
        } else if rest > 0 {
            out.push(UNITS[rest as usize]);
        }
        out
    }
    if n == 0 {
        return "zero".into();
    }
    let mut out = Vec::new();
    for (scale, name) in [(1_000_000, "million"), (1000, "thousand"), (1, "")] {
        let part = n / scale % 1000;
        if part > 0 {
            out.extend(below_thousand(part));
            if !name.is_empty() {
                out.push(name);
            }
        }
    }
    out.join(" ")
}

/// Word-level Levenshtein distance and the reference length.
fn wer(reference: &[String], hypothesis: &[String]) -> (usize, usize) {
    let mut previous: Vec<usize> = (0..=hypothesis.len()).collect();
    for (i, r) in reference.iter().enumerate() {
        let mut current = vec![i + 1; hypothesis.len() + 1];
        for (j, h) in hypothesis.iter().enumerate() {
            current[j + 1] = (previous[j] + usize::from(r != h))
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        previous = current;
    }
    (previous[hypothesis.len()], reference.len())
}

/// The int8 exports sometimes loop ("one one one one one"); flag 5+ repeats.
fn has_loop(words: &[String]) -> bool {
    words.windows(5).any(|w| w.iter().all(|x| *x == w[0]))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn w(text: &str) -> Vec<String> {
        words(text, None)
    }
    #[test]
    fn wer_counts_word_edits() {
        assert_eq!(wer(&w("a b c d"), &w("a x c")), (2, 4));
        assert_eq!(wer(&w("Ciao, Marco!"), &w("ciao marco")), (0, 2));
        assert_eq!(wer(&w("l'orario"), &w("l orario")), (0, 2));
        assert_eq!(wer(&w(""), &w("extra")), (1, 0));
    }
    #[test]
    fn numbers_are_spelled_out_per_language() {
        assert_eq!(italian(23), "ventitré");
        assert_eq!(italian(1250), "milleduecentocinquanta");
        assert_eq!(italian(21), "ventuno");
        assert_eq!(italian(38), "trentotto");
        assert_eq!(italian(2011), "duemilaundici");
        assert_eq!(italian(2_000_000), "due milioni");
        assert_eq!(english(1537), "one thousand five hundred thirty seven");
        assert_eq!(english(15), "fifteen");
        assert_eq!(
            words("Il totale è 1.250 euro", Some("it")),
            ["il", "totale", "è", "milleduecentocinquanta", "euro"]
        );
        assert_eq!(
            words("802.11 and 1,250", Some("en")),
            [
                "eight", "hundred", "two", "eleven", "and", "one", "thousand", "two", "hundred",
                "fifty"
            ]
        );
        assert_eq!(
            wer(
                &words("giovedì ventitré ottobre", Some("it")),
                &words("giovedì 23 ottobre", Some("it"))
            ),
            (0, 3)
        );
    }
    #[test]
    fn loops_need_five_repeats() {
        assert!(has_loop(&w("eight one one one one one A")));
        assert!(!has_loop(&w("one one one one two")));
    }
}
