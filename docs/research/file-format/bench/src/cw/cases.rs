//! The M1 cases: generated and imported spans, as sorted Arrow input files plus the text files
//! the queries need (`plans/08-m1-likely-failures.md`, M1.1).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use arrow::array::{Array, FixedSizeBinaryArray, RecordBatch, TimestampNanosecondArray, UInt32Array};
use arrow::compute::{SortColumn, concat_batches, interleave_record_batch, lexsort_to_indices, take_record_batch};
use arrow::datatypes::SchemaRef;
use arrow::error::ArrowError;
use arrow::ipc::reader::FileReader;
use arrow::ipc::writer::FileWriter;
use cetra_core::batch::spans_to_record_batch;
use cetra_core::enums::{PayloadFormat, SpanKind, SpanStatus};
use cetra_core::ids::SpanId;
use cetra_core::span::Span;
use fmt_testdata::datagen;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// Input files per generated case (CW-19).
pub const FILES: usize = 4;
/// Trace ids and span pairs in each case's `cw_traces.txt` and `cw_spans.txt`.
const IDS: usize = 20;
/// Keys per span in the `keys*` cases.
pub const KEYS_PER_SPAN: usize = 20;
/// Distinct values per key in the `keys*` cases: `"v0"` … `"v49"`, JSON-quoted.
pub const KEY_VALUES: usize = 50;
/// Telemetry spans added under each LLM span in `nonllm95`.
pub const TELEMETRY_PER_LLM: usize = 19;
/// Every this-many-th LLM span of `giant` gets a giant input.
pub const GIANT_EVERY: usize = 40;
/// Bytes of a giant input: under the 8 MiB attribute cap of D40.
pub const GIANT_BYTES: usize = 7_500_000;
/// The absent needle: planted nowhere.
pub const ABSENT: &str = "qzxjabsentneedle";
/// Planted needles: (query name, variants used in turn, plant in every n-th input, offset).
pub const PLANTS: [(&str, &[&str], usize, usize); 5] = [
    ("rare", &["zqxjrareneedle"], 10_000, 3),
    ("common", &["commonneedle"], 10, 1),
    ("bug", &["bug", "Bug", "BUG"], 100, 7),
    ("404", &["404"], 100, 57),
    ("sql", &["sql", "Sql", "SQL"], 100, 31),
];
/// What each planted query searches for (`cw_needles.txt`), with `absent` first.
pub const NEEDLES: [(&str, &str); 6] = [
    ("absent", ABSENT),
    ("rare", "zqxjrareneedle"),
    ("common", "commonneedle"),
    ("bug", "bug"),
    ("404", "404"),
    ("sql", "SQL"),
];
/// Phrases of the `intl` text: accents, CJK, emoji, Cyrillic, and case pairs.
pub const INTL_PHRASES: [&str; 10] = [
    "Le café est très chaud", "CAFÉ CRÈME à emporter", "東京の天気は晴れです", "数据库连接错误",
    "🚀 déploiement terminé ✅", "Die Straße ist gesperrt", "STRASSE GESPERRT",
    "Ошибка подключения к базе данных", "naïve résumé façade", "😀😃 emoji 🎉",
];
/// Extra needles of `intl`, appended to `NEEDLES`.
pub const INTL_NEEDLES: [(&str, &str); 5] = [
    ("cafe", "café"), ("tokyo", "東京"), ("rocket", "🚀"), ("strasse", "straße"), ("oshibka", "ошибка"),
];

/// A span's trace id and span id, as bytes.
type Pair = ([u8; 16], [u8; 8]);

/// The text files of a case directory, in memory.
pub struct CaseFiles {
    /// `cw_queries.txt`: the queries, in run order.
    pub queries: Vec<String>,
    /// `cw_traces.txt`.
    pub traces: Vec<[u8; 16]>,
    /// `cw_spans.txt`: (trace id, span id).
    pub spans: Vec<([u8; 16], [u8; 8])>,
    /// (name, needle) as plain text; the file holds the needle in hex.
    pub needles: Vec<(String, String)>,
    /// `cw_attr.txt`: (key, value), for the `keys*` cases only.
    pub attr: Option<(String, String)>,
}

/// `fmtbench cw gen <case> <dir> [--traces N]`: generates a case and writes its directory.
pub fn gen_cmd(args: &[String]) -> crate::R<()> {
    let (case, dir, traces) = match args {
        [case, dir] => (case, dir, None),
        [case, dir, flag, n] if flag == "--traces" => (case, dir, Some(n.parse::<usize>()?)),
        _ => return Err(crate::USAGE.into()),
    };
    let (spans, files) = gen_spans(case, traces)?;
    let llm = spans.iter().filter(|s| s.span_kind == SpanKind::Llm).count();
    println!("{} spans, {:.2}% LLM", spans.len(), 100.0 * llm as f64 / spans.len().max(1) as f64);
    write_case(Path::new(dir), spans, &files)
}

/// `fmtbench cw import <src-dir> <dir> [name=needle ...]`: turns an existing dataset's
/// `pq_cetra` files into a case. Each Parquet file becomes one input file, batch by batch.
pub fn import_cmd(args: &[String]) -> crate::R<()> {
    let [src, dir, needle_args @ ..] = args else {
        return Err(crate::USAGE.into());
    };
    let (src, dir) = (Path::new(src), Path::new(dir));
    let needles = import_needles(src, needle_args)?;
    let mut parquet: Vec<PathBuf> = fs::read_dir(src.join("pq_cetra"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    parquet.retain(|p| p.extension().is_some_and(|e| e == "parquet"));
    parquet.sort();
    if parquet.is_empty() {
        return Err(format!("no pq_cetra/*.parquet files in {}", src.display()).into());
    }
    fs::create_dir_all(dir.join("input"))?;
    let trace_ids = src.join("trace_ids.txt");
    let traces = fs::read_to_string(&trace_ids)?
        .lines()
        .take(IDS)
        .map(crate::parse_id)
        .collect::<crate::R<Vec<_>>>()?;
    if traces.len() < IDS {
        return Err(format!("{}: {} trace ids, need {IDS}", trace_ids.display(), traces.len()).into());
    }

    // Spans for cw_spans.txt come from the first input file: row i * rows / IDS, moving forward
    // to the next row with a non-null input, and never the same row twice.
    let mut targets = Vec::new();
    let mut picks: Vec<Pair> = Vec::new();
    let mut last: Option<u64> = None;
    for (k, path) in parquet.iter().enumerate() {
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
        if k == 0 {
            let rows = builder.metadata().file_metadata().num_rows() as u64;
            targets = (0..IDS as u64).map(|i| i * rows / IDS as u64).collect();
        }
        let schema = builder.schema().clone();
        let reader = builder.with_batch_size(8192).build()?;
        let out_path = dir.join(format!("input/{k:03}.arrow"));
        let mut out = create_input(&out_path, &schema)?;
        let mut base = 0u64;
        for batch in reader {
            let batch = batch?;
            if k == 0 && picks.len() < IDS {
                pick_spans(&batch, base, &targets, &mut picks, &mut last)?;
            }
            base += batch.num_rows() as u64;
            out.write(&batch)?;
        }
        out.finish()?;
        println!("{}: {base} rows", out_path.display());
    }
    if picks.len() < IDS {
        return Err(format!(
            "{}: {} spans with an input, need {IDS} for cw_spans.txt",
            parquet[0].display(),
            picks.len()
        )
        .into());
    }

    let queries = queries_for("import", &needles);
    let files = CaseFiles { queries, traces, spans: picks, needles, attr: None };
    write_texts(dir, &files)?;
    println!("imported {} files from {}", parquet.len(), src.display());
    Ok(())
}

/// The needles of an import: the `name=needle` arguments when given, else the defaults (with
/// `rare` taken from `needles.txt`, which holds it in hex, when that file exists).
fn import_needles(src: &Path, args: &[String]) -> crate::R<Vec<(String, String)>> {
    if !args.is_empty() {
        let mut out = Vec::new();
        for a in args {
            let Some((name, needle)) = a.split_once('=') else {
                return Err(format!("needle {a:?}: expected name=needle").into());
            };
            out.push((name.to_owned(), needle.to_owned()));
        }
        return Ok(out);
    }
    let mut out: Vec<(String, String)> = [("absent", ABSENT), ("404", "404"), ("bug", "bug"), ("sql", "SQL"), ("the", " the ")]
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    let path = src.join("needles.txt");
    if path.exists() {
        let text = fs::read_to_string(&path)?;
        if let Some(first) = text.lines().next() {
            out.push(("rare".to_owned(), String::from_utf8(crate::profile::unhex(first.trim()))?));
        }
    }
    Ok(out)
}

/// Adds the spans of one batch of the first input file to `picks` (see `import_cmd`).
/// `base` is the batch's first row within the file; `last` is the last picked row.
fn pick_spans(
    batch: &RecordBatch,
    base: u64,
    targets: &[u64],
    picks: &mut Vec<Pair>,
    last: &mut Option<u64>,
) -> crate::R<()> {
    let trace = fixed_binary(batch, "trace_id")?;
    let span = fixed_binary(batch, "span_id")?;
    let input = batch.column_by_name("input").ok_or("input file has no input column")?;
    for r in 0..batch.num_rows() {
        if picks.len() >= IDS {
            break;
        }
        let row = base + r as u64;
        let want = targets[picks.len()].max(last.map_or(0, |l| l + 1));
        if row < want || input.is_null(r) {
            continue;
        }
        let t: [u8; 16] = trace.value(r).try_into()?;
        let s: [u8; 8] = span.value(r).try_into()?;
        picks.push((t, s));
        *last = Some(row);
    }
    Ok(())
}

/// The fixed-size binary column `name` of `batch`.
fn fixed_binary<'a>(batch: &'a RecordBatch, name: &str) -> crate::R<&'a FixedSizeBinaryArray> {
    let column = batch.column_by_name(name).ok_or_else(|| format!("input file has no {name} column"))?;
    column
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .ok_or_else(|| format!("{name} is not fixed-size binary").into())
}

/// Generates a case (`--traces` = `traces` when given): its spans and its text files.
pub fn gen_spans(case: &str, traces: Option<usize>) -> crate::R<(Vec<Span>, CaseFiles)> {
    let (default_traces, keys) = match case {
        "keys100" => (100_000, Some(100)),
        "keys1k" => (100_000, Some(1_000)),
        "keys5k" => (100_000, Some(5_000)),
        "text_unique" | "b64" | "json_random" | "nonllm95" | "intl" => (10_000, None),
        "giant" => (2_000, None),
        _ => {
            return Err(format!(
                "unknown case {case:?}: expected keys100, keys1k, keys5k, text_unique, b64, json_random, nonllm95, giant or intl"
            )
            .into());
        }
    };
    let world = datagen::World::new(7);
    let mut rng = datagen::Rng::new(42);
    let mut spans = Vec::new();
    for _ in 0..traces.unwrap_or(default_traces) {
        let start = spans.len();
        world.trace(&mut rng, &mut spans);
        if keys.is_some() {
            spans[start..].iter_mut().for_each(crate::strip_payloads);
        }
    }

    let mut attr = None;
    let mut giant_pairs = Vec::new();
    if let Some(pool) = keys {
        assign_keys(&mut spans, pool, &mut rng);
        attr = Some(("tool.arg.2".to_owned(), "\"v3\"".to_owned()));
    } else {
        match case {
            "text_unique" => replace_payloads(&mut spans, &mut rng, unique_text),
            "b64" => replace_payloads(&mut spans, &mut rng, b64_text),
            "json_random" => replace_payloads(&mut spans, &mut rng, json_text),
            "intl" => replace_payloads(&mut spans, &mut rng, intl_text),
            "nonllm95" => spans = telemetry(spans, &mut rng),
            "giant" => giant_pairs = giant(&mut spans, &mut rng),
            _ => {}
        }
        plant(&mut spans);
    }

    let mut needles: Vec<(String, String)> = if keys.is_some() {
        Vec::new()
    } else {
        NEEDLES.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect()
    };
    if case == "intl" {
        needles.extend(INTL_NEEDLES.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())));
    }
    let files = CaseFiles {
        queries: queries_for(case, &needles),
        traces: pick_traces(&spans, &mut rng)?,
        spans: pick_pairs(&spans, &giant_pairs, keys.is_some(), &mut rng)?,
        needles,
        attr,
    };
    Ok((spans, files))
}

/// The queries of a case, in run order (the table of M1.1 § Cases). `import` has the import's
/// queries.
fn queries_for(case: &str, needles: &[(String, String)]) -> Vec<String> {
    let search: Vec<String> = needles.iter().map(|(n, _)| format!("search:{n}")).collect();
    let strs = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>();
    match case {
        "keys100" | "keys1k" | "keys5k" => strs(&["write", "attr_filter", "attr_keys", "trace_light"]),
        "text_unique" | "b64" | "json_random" => {
            let mut q = strs(&["write", "scan_payload"]);
            q.extend(search);
            q.push("span_heavy".to_owned());
            q
        }
        "nonllm95" => {
            let mut q = strs(&["write", "trace_light", "scan_payload"]);
            q.extend(search);
            q
        }
        "giant" => strs(&["write", "span_heavy", "scan_payload", "search:rare", "search:absent"]),
        "import" => {
            let mut q = strs(&["write", "scan_payload", "trace_light", "span_heavy"]);
            q.extend(search);
            q
        }
        // intl
        _ => {
            let mut q = strs(&["write"]);
            q.extend(search);
            q
        }
    }
}

/// `cw_traces.txt`: 20 distinct trace ids, of randomly chosen spans.
fn pick_traces(spans: &[Span], rng: &mut datagen::Rng) -> crate::R<Vec<[u8; 16]>> {
    let distinct: HashSet<[u8; 16]> = spans.iter().map(|s| *s.trace_id.as_bytes()).collect();
    if distinct.len() < IDS {
        return Err(format!("cw_traces.txt: only {} traces, need {IDS}", distinct.len()).into());
    }
    let mut out: Vec<[u8; 16]> = Vec::new();
    while out.len() < IDS {
        let t = *spans[rng.below(spans.len())].trace_id.as_bytes();
        if !out.contains(&t) {
            out.push(t);
        }
    }
    Ok(out)
}

/// `cw_spans.txt`: the first giant spans (up to 10), then random spans with an input, 20 in all.
/// The `keys*` cases have no payloads, so their pool is every span.
fn pick_pairs(spans: &[Span], giant: &[Pair], keys: bool, rng: &mut datagen::Rng) -> crate::R<Vec<Pair>> {
    let pool: Vec<Pair> = spans
        .iter()
        .filter(|s| keys || s.input.is_some())
        .map(|s| (*s.trace_id.as_bytes(), *s.span_id.as_bytes()))
        .collect();
    if pool.len() < IDS {
        return Err(format!("cw_spans.txt: only {} spans to pick from, need {IDS}", pool.len()).into());
    }
    let mut out: Vec<Pair> = giant.iter().take(10).copied().collect();
    while out.len() < IDS {
        let p = pool[rng.below(pool.len())];
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

/// Writes a generated case into `dir`: the input files, then the text files.
pub fn write_case(dir: &Path, spans: Vec<Span>, files: &CaseFiles) -> crate::R<()> {
    write_inputs(dir, spans)?;
    write_texts(dir, files)
}

/// Writes `input/000.arrow` … `input/003.arrow` (M1.1 § Steps 6): the spans sorted by start time,
/// cut into `FILES` slices, each sorted by (trace_id, start_time) into one batch.
fn write_inputs(dir: &Path, mut spans: Vec<Span>) -> crate::R<()> {
    spans.sort_by_key(|s| s.start_time);
    fs::create_dir_all(dir.join("input"))?;
    let per = spans.len().div_ceil(FILES);
    for i in 0..FILES {
        let lo = (i * per).min(spans.len());
        let hi = ((i + 1) * per).min(spans.len());
        let mut batches = spans[lo..hi]
            .chunks(8192)
            .map(spans_to_record_batch)
            .collect::<Result<Vec<_>, _>>()?;
        if batches.is_empty() {
            batches.push(spans_to_record_batch(&[])?);
        }
        let b = concat_batches(&batches[0].schema(), &batches)?;
        let idx = lexsort_to_indices(
            &[
                SortColumn { values: b.column_by_name("trace_id").unwrap().clone(), options: None },
                SortColumn { values: b.column_by_name("start_time").unwrap().clone(), options: None },
            ],
            None,
        )?;
        let b = take_record_batch(&b, &idx)?;
        let path = dir.join(format!("input/{i:03}.arrow"));
        let mut out = create_input(&path, &b.schema())?;
        out.write(&b)?;
        out.finish()?;
        println!("{}: {} rows", path.display(), b.num_rows());
    }
    Ok(())
}

/// Creates one Arrow IPC input file, ready for batches of `schema`.
fn create_input(path: &Path, schema: &SchemaRef) -> crate::R<FileWriter<BufWriter<File>>> {
    Ok(FileWriter::try_new(BufWriter::new(File::create(path)?), schema)?)
}

/// Writes the text files of a case: queries, traces, spans, needles (when there are any) and
/// the attribute pair (when there is one).
fn write_texts(dir: &Path, files: &CaseFiles) -> crate::R<()> {
    fs::create_dir_all(dir)?;
    write_lines(dir.join("cw_queries.txt"), files.queries.iter().cloned())?;
    write_lines(dir.join("cw_traces.txt"), files.traces.iter().map(|t| crate::hex(t)))?;
    write_lines(
        dir.join("cw_spans.txt"),
        files.spans.iter().map(|(t, s)| format!("{} {}", crate::hex(t), crate::hex(s))),
    )?;
    if !files.needles.is_empty() {
        write_lines(
            dir.join("cw_needles.txt"),
            files.needles.iter().map(|(n, v)| format!("{n}\t{}", crate::hex(v.as_bytes()))),
        )?;
    }
    if let Some((key, value)) = &files.attr {
        write_lines(dir.join("cw_attr.txt"), [format!("{key}\t{value}")])?;
    }
    Ok(())
}

/// Writes one line per item, each ended by a newline.
fn write_lines(path: PathBuf, lines: impl IntoIterator<Item = String>) -> crate::R<()> {
    let mut text = String::new();
    for line in lines {
        text.push_str(&line);
        text.push('\n');
    }
    fs::write(path, text)?;
    Ok(())
}

/// Hex of 32 random digits, taking 16 per `rng.next()`.
fn hex_string(rng: &mut datagen::Rng, digits: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digits);
    let mut word = 0u64;
    for i in 0..digits {
        if i % 16 == 0 {
            word = rng.next();
        }
        out.push(HEX[(word & 15) as usize] as char);
        word >>= 4;
    }
    out
}

/// Standard base64 of `bytes`, with `=` padding.
fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Words of 3 to 10 random letters, separated by spaces, cut to exactly `len` bytes; the first
/// letter of every 12th word is uppercased.
fn unique_text(rng: &mut datagen::Rng, len: usize) -> String {
    let mut s = String::new();
    let mut words = 0usize;
    while s.len() < len {
        if words > 0 {
            s.push(' ');
        }
        for i in 0..rng.range(3, 10) {
            let c = b'a' + rng.below(26) as u8;
            s.push(if i == 0 && (words + 1) % 12 == 0 { c.to_ascii_uppercase() as char } else { c as char });
        }
        words += 1;
    }
    s.truncate(len);
    s
}

/// Base64 of random bytes, cut to exactly `len` characters.
fn b64_text(rng: &mut datagen::Rng, len: usize) -> String {
    let bytes: Vec<u8> = (0..len * 3 / 4 + 3).map(|_| rng.next() as u8).collect();
    let mut s = base64(&bytes);
    s.truncate(len);
    s
}

/// Random JSON, at most `len` bytes: an object of random items, cut at `len` (ASCII, so any
/// byte is a boundary). Below about 60 bytes the result is a cut prefix, not valid JSON.
fn json_text(rng: &mut datagen::Rng, len: usize) -> String {
    let mut s = format!("{{\"id\":\"{}\",\"items\":[", hex_string(rng, 32));
    while s.len() < len {
        let item = format!("{{\"k\":\"{}\",\"v\":{}}},", hex_string(rng, 16), rng.next() % 1_000_000);
        s.push_str(&item);
    }
    if s.ends_with(',') {
        s.pop();
    }
    s.push_str("]}");
    s.truncate(len);
    s
}

/// `INTL_PHRASES` joined by spaces, cut at a char boundary to at most `len` bytes.
fn intl_text(rng: &mut datagen::Rng, len: usize) -> String {
    let mut s = String::new();
    while s.len() < len {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(INTL_PHRASES[rng.below(INTL_PHRASES.len())]);
    }
    let mut cut = len.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

/// Sets `value` as a text payload: the slot, its format and its preview.
fn put_payload(
    value: String,
    slot: &mut Option<String>,
    format: &mut Option<PayloadFormat>,
    preview: &mut Option<String>,
) {
    *preview = Some(datagen::preview(&value));
    *slot = Some(value);
    *format = Some(PayloadFormat::Text);
}

/// Replaces every non-null input and output by `f(rng, its byte length)`.
fn replace_payloads(spans: &mut [Span], rng: &mut datagen::Rng, f: fn(&mut datagen::Rng, usize) -> String) {
    for s in spans.iter_mut() {
        if let Some(len) = s.input.as_ref().map(String::len) {
            let value = f(rng, len);
            put_payload(value, &mut s.input, &mut s.input_format, &mut s.input_preview);
        }
        if let Some(len) = s.output.as_ref().map(String::len) {
            let value = f(rng, len);
            put_payload(value, &mut s.output, &mut s.output_format, &mut s.output_preview);
        }
    }
}

/// Replaces every span's attributes by `KEYS_PER_SPAN` distinct keys of a pool of `pool` keys.
/// Key `i` is `tool.arg.<i>` for even `i` and 32 random hex digits for odd `i`.
fn assign_keys(spans: &mut [Span], pool: usize, rng: &mut datagen::Rng) {
    let mut keys = Vec::with_capacity(pool);
    for i in 0..pool {
        keys.push(if i % 2 == 0 { format!("tool.arg.{i}") } else { hex_string(rng, 32) });
    }
    for s in spans.iter_mut() {
        let mut chosen = BTreeSet::new();
        while chosen.len() < KEYS_PER_SPAN {
            chosen.insert(rng.below(pool));
        }
        s.attributes = chosen
            .into_iter()
            .map(|i| (keys[i].clone(), format!("\"v{}\"", rng.below(KEY_VALUES))))
            .collect::<BTreeMap<String, String>>();
    }
}

/// Adds `TELEMETRY_PER_LLM` non-LLM spans after each LLM span: database and HTTP calls, with
/// no payload, parented to the LLM span.
fn telemetry(spans: Vec<Span>, rng: &mut datagen::Rng) -> Vec<Span> {
    let mut out = Vec::with_capacity(spans.len() * 2);
    for span in spans {
        let mut extra = Vec::new();
        if span.span_kind == SpanKind::Llm {
            for j in 0..TELEMETRY_PER_LLM {
                extra.push(telemetry_span(&span, j, rng));
            }
        }
        out.push(span);
        out.extend(extra);
    }
    out
}

/// The `j`-th telemetry span under `llm`: a copy of it with the fields of M1.1 § Steps 3 set,
/// and every optional payload and metadata field cleared.
fn telemetry_span(llm: &Span, j: usize, rng: &mut datagen::Rng) -> Span {
    let span_id = loop {
        if let Ok(id) = SpanId::from_bytes(rng.next().to_be_bytes()) {
            break id;
        }
    };
    let db = j % 2 == 0;
    let start = llm.start_time + 1_000 * (j as i64 + 1);
    let (key, value) = if db { ("db.system", "\"postgresql\"") } else { ("http.method", "\"GET\"") };
    let mut s = llm.clone();
    s.span_kind = SpanKind::Span;
    s.name = if db { "db.query" } else { "http.get" }.to_owned();
    s.span_id = span_id;
    s.parent_span_id = Some(llm.span_id);
    s.start_time = start;
    s.end_time = start + 500;
    s.attributes = BTreeMap::from([(key.to_owned(), value.to_owned())]);
    s.status = SpanStatus::Unset;
    s.status_message = None;
    s.events = Vec::new();
    s.provider = None;
    s.request_model = None;
    s.response_model = None;
    s.operation = None;
    s.input_tokens = None;
    s.output_tokens = None;
    s.cache_read_tokens = None;
    s.cache_write_tokens = None;
    s.reasoning_tokens = None;
    s.total_tokens = None;
    s.cost_input = None;
    s.cost_output = None;
    s.cost_total = None;
    s.cost_source = None;
    s.ttft_ns = None;
    s.request_params = None;
    s.input = None;
    s.input_format = None;
    s.output = None;
    s.output_format = None;
    s.input_preview = None;
    s.output_preview = None;
    s.system_instructions = None;
    s.tool_definitions = None;
    s
}

/// Gives every `GIANT_EVERY`-th LLM span with an input a giant input (M1.1 § Steps 3). Returns
/// the (trace, span) pairs of those spans, in order.
fn giant(spans: &mut [Span], rng: &mut datagen::Rng) -> Vec<Pair> {
    let mut count = 0usize;
    let mut pairs = Vec::new();
    for s in spans.iter_mut() {
        if s.span_kind != SpanKind::Llm {
            continue;
        }
        let k = count;
        count += 1;
        if k % GIANT_EVERY != 0 {
            continue;
        }
        let Some(old) = s.input.as_deref() else {
            continue;
        };
        let value = giant_text(rng, old, k / GIANT_EVERY);
        put_payload(value, &mut s.input, &mut s.input_format, &mut s.input_preview);
        pairs.push((*s.trace_id.as_bytes(), *s.span_id.as_bytes()));
    }
    pairs
}

/// A giant input of `GIANT_BYTES`: an even `index` repeats `old` (joined by newlines), an odd
/// one, or an empty `old`, is random base64. The repeated text is cut at a char boundary at
/// or below `GIANT_BYTES`.
fn giant_text(rng: &mut datagen::Rng, old: &str, index: usize) -> String {
    if index % 2 == 0 && !old.is_empty() {
        let mut s = old.to_owned();
        while s.len() < GIANT_BYTES {
            s.push('\n');
            s.push_str(old);
        }
        let mut cut = GIANT_BYTES.min(s.len());
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s
    } else {
        b64_text(rng, GIANT_BYTES)
    }
}

/// Prepends each planted needle (`PLANTS`) to the inputs it lands on. Counting the spans with
/// an input from 0 as `k`, a plant lands when `k % every == offset`; several may land on one.
fn plant(spans: &mut [Span]) {
    let mut k = 0usize;
    for s in spans.iter_mut() {
        let Some(input) = s.input.as_mut() else {
            continue;
        };
        for (_, variants, every, offset) in PLANTS {
            if k % every == offset {
                let variant = variants[(k / every) % variants.len()];
                input.insert_str(0, &format!("{variant} "));
            }
        }
        k += 1;
    }
}

/// Bytes of Arrow memory one flush writes (spec 03 § 2, `flush.max_bytes`).
pub const FLUSH_BYTES: usize = 256 << 20;

/// The most files `cw split` cuts a case into.
const MAX_SPLIT: usize = 1_000;

/// `fmtbench cw split <dir> <n>`: rewrites `<dir>/input/` as `n` files, each one time slice of
/// the case, as `n` L0 files (M2.1 § Steps 1 to 4). Removes the outputs of the old cut.
pub fn split_cmd(args: &[String]) -> crate::R<()> {
    let [dir, n] = args else {
        return Err(crate::USAGE.into());
    };
    let dir = Path::new(dir);
    let n: usize = n.parse()?;
    if !(1..=MAX_SPLIT).contains(&n) {
        return Err(format!("split into {n} files: expected 1 to {MAX_SPLIT}").into());
    }
    let batches = read_batches(&input_paths(dir)?)?;
    let slices = split_batches(&batches, n)?;

    // A backup left by an interrupted split holds the old cut: it is never deleted here.
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.file_name().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with("input.old-")) {
            return Err(format!(
                "{}: an earlier split was interrupted; restore it to input/ by hand, then split again",
                path.display()
            )
            .into());
        }
    }

    // Only now is the old cut replaced: the new files are complete before the old ones go.
    let old = dir.join(format!("input.old-{}", std::process::id()));
    fs::rename(dir.join("input"), &old)?;
    fs::create_dir(dir.join("input"))?;
    for (i, slice) in slices.iter().enumerate() {
        let mut out = create_input(&dir.join(format!("input/{i:03}.arrow")), &slice.schema())?;
        out.write(slice)?;
        out.finish()?;
    }
    fs::remove_dir_all(&old)?;

    // The outputs of the old cut no longer match the input.
    for name in ["pq_cetra", "st", "compact-pq_cetra", "compact-st", "flush-pq_cetra", "flush-st"] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
    }
    let flush = dir.join("flush.arrow");
    if flush.exists() {
        fs::remove_file(&flush)?;
    }

    let total: usize = slices.iter().map(RecordBatch::num_rows).sum();
    println!("{total} rows in {} files", slices.len());
    for (i, slice) in slices.iter().enumerate() {
        let starts = start_times(slice)?.values();
        let lo = starts.iter().copied().min().unwrap_or_default();
        let hi = starts.iter().copied().max().unwrap_or_default();
        println!("input/{i:03}.arrow: {} rows, start_time {lo}..{hi}", slice.num_rows());
    }
    Ok(())
}

/// The `.arrow` files of `<dir>/input/`, in name order.
pub(crate) fn input_paths(dir: &Path) -> crate::R<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir.join("input"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|p| p.extension().is_some_and(|e| e == "arrow"));
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no input/*.arrow files in {}", dir.display()).into());
    }
    Ok(paths)
}

/// The batches of `paths`, in order.
fn read_batches(paths: &[PathBuf]) -> crate::R<Vec<RecordBatch>> {
    let mut batches = Vec::new();
    for path in paths {
        for batch in FileReader::try_new(File::open(path)?, None)? {
            batches.push(batch?);
        }
    }
    if batches.iter().all(|b| b.num_rows() == 0) {
        return Err("no input rows".into());
    }
    Ok(batches)
}

/// The `start_time` column of `batch`.
fn start_times(batch: &RecordBatch) -> crate::R<&TimestampNanosecondArray> {
    batch
        .column_by_name("start_time")
        .ok_or("input file has no start_time column")?
        .as_any()
        .downcast_ref::<TimestampNanosecondArray>()
        .ok_or_else(|| "start_time is not a nanosecond timestamp".into())
}

/// `batch`'s rows sorted by `(trace_id, start_time)`, as `write_inputs` sorts a case.
fn sorted_by_trace(batch: &RecordBatch) -> crate::R<RecordBatch> {
    let column = |name: &str| -> crate::R<_> {
        Ok(SortColumn {
            values: batch.column_by_name(name).ok_or_else(|| format!("input file has no {name} column"))?.clone(),
            options: None,
        })
    };
    let idx = lexsort_to_indices(&[column("trace_id")?, column("start_time")?], None)?;
    Ok(take_record_batch(batch, &idx)?)
}

/// The rows of `batches` (one schema) cut into `n` slices of consecutive `start_time`, each
/// sorted by `(trace_id, start_time)`: slice i holds the rows of global ranks
/// [len*i/n, len*(i+1)/n) by `start_time` (ties by input order), and empty slices are dropped.
pub fn split_batches(batches: &[RecordBatch], n: usize) -> crate::R<Vec<RecordBatch>> {
    // (start_time, batch index, row index) of every row, in time order.
    let mut rows: Vec<(i64, usize, usize)> = Vec::new();
    for (b, batch) in batches.iter().enumerate() {
        let starts = start_times(batch)?;
        for r in 0..batch.num_rows() {
            rows.push((starts.value(r), b, r));
        }
    }
    if rows.is_empty() {
        return Err("no input rows".into());
    }
    rows.sort_unstable();
    let total = rows.len();
    let refs: Vec<&RecordBatch> = batches.iter().collect();
    let mut out = Vec::new();
    for i in 0..n {
        let (lo, hi) = (total * i / n, total * (i + 1) / n);
        if lo == hi {
            continue;
        }
        let indices: Vec<(usize, usize)> = rows[lo..hi].iter().map(|&(_, b, r)| (b, r)).collect();
        let slice = interleave_record_batch(&refs, &indices).map_err(|e| match e {
            ArrowError::OffsetOverflowError(_) => {
                format!("slice {i} of {n} is too large for one batch: split into more files")
            }
            e => e.to_string(),
        })?;
        out.push(sorted_by_trace(&slice)?);
    }
    Ok(out)
}

/// `fmtbench cw flush-input <dir>`: writes `<dir>/flush.arrow`, one batch of about `FLUSH_BYTES`
/// holding the first rows of the input files in order, sorted by `(trace_id, start_time)`
/// (M2.1 § Step 5).
pub fn flush_input_cmd(args: &[String]) -> crate::R<()> {
    let [dir] = args else {
        return Err(crate::USAGE.into());
    };
    let dir = Path::new(dir);
    let batches = read_batches(&input_paths(dir)?)?;
    let total_rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    // A batch read from IPC reports the whole file body for each of its columns, so the sizes are
    // measured on compacted copies, one batch at a time.
    let mut total_bytes = 0usize;
    for batch in &batches {
        let idx = UInt32Array::from_iter_values(0..u32::try_from(batch.num_rows())?);
        total_bytes += take_record_batch(batch, &idx)?.get_array_memory_size();
    }
    let rows = flush_rows(total_rows, total_bytes, FLUSH_BYTES);

    let mut parts = Vec::new();
    let mut left = rows;
    for batch in &batches {
        if left == 0 {
            break;
        }
        let take = left.min(batch.num_rows());
        parts.push(batch.slice(0, take));
        left -= take;
    }
    let schema = batches[0].schema();
    let flush = sorted_by_trace(&concat_batches(&schema, &parts)?)?;
    let mut out = create_input(&dir.join("flush.arrow"), &schema)?;
    out.write(&flush)?;
    out.finish()?;
    println!("flush.arrow: {} rows, {} bytes in memory", flush.num_rows(), flush.get_array_memory_size());
    Ok(())
}

/// The rows of a flush: `total_rows` scaled by `limit / total_bytes`, rounded up and capped at
/// `total_rows`; all of them when `total_bytes` is 0.
fn flush_rows(total_rows: usize, total_bytes: usize, limit: usize) -> usize {
    if total_bytes == 0 {
        return total_rows;
    }
    let rows = (total_rows as u128 * limit as u128).div_ceil(total_bytes as u128);
    rows.min(total_rows as u128) as usize
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// An empty directory for one test, under the system temp dir.
    fn temp_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cw-test-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The batches of an input file.
    fn read_input(path: &Path) -> Vec<RecordBatch> {
        FileReader::try_new(File::open(path).unwrap(), None)
            .unwrap()
            .map(|b| b.unwrap())
            .collect()
    }

    /// Whether `s` is non-empty lowercase hex.
    fn is_hex(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn base64_matches_known_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, want) in cases {
            assert_eq!(base64(input.as_bytes()), want);
        }
    }

    #[test]
    fn text_generators_hit_their_length() {
        let mut rng = datagen::Rng::new(1);
        for len in [0, 1, 7, 1_000, 100_000] {
            let u = unique_text(&mut rng, len);
            assert_eq!(u.len(), len);
            let b = b64_text(&mut rng, len);
            assert_eq!(b.len(), len);
            assert!(json_text(&mut rng, len).len() <= len);
            assert!(intl_text(&mut rng, len).len() <= len);
        }
    }

    #[test]
    fn unique_text_repeats_no_window() {
        let mut rng = datagen::Rng::new(9);
        let texts: Vec<String> = (0..50).map(|_| unique_text(&mut rng, 4_096)).collect();
        let mut seen: HashMap<&[u8], usize> = HashMap::new();
        for (i, text) in texts.iter().enumerate() {
            for window in text.as_bytes().windows(32) {
                if let Some(&j) = seen.get(window) {
                    assert_eq!(j, i, "a 32-byte window occurs in texts {j} and {i}");
                }
                seen.insert(window, i);
            }
        }
    }

    #[test]
    fn planting_hits_its_rates() {
        let world = datagen::World::new(7);
        let mut rng = datagen::Rng::new(42);
        let mut spans = Vec::new();
        while spans.len() < 20_000 {
            world.trace(&mut rng, &mut spans);
        }
        spans.truncate(20_000);
        for s in spans.iter_mut() {
            s.input = Some("plain words".to_owned());
        }
        plant(&mut spans);
        let count = |needle: &str| spans.iter().filter(|s| s.input.as_deref().unwrap().contains(needle)).count();
        assert_eq!(count("zqxjrareneedle"), 2);
        assert_eq!(count("commonneedle"), 2_000);
        for needle in ["bug", "Bug", "BUG"] {
            let c = count(needle);
            assert!((60..=75).contains(&c), "{needle} planted {c} times, want about 67");
        }
    }

    #[test]
    fn telemetry_makes_95_percent_non_llm() {
        let (spans, _) = gen_spans("nonllm95", Some(200)).unwrap();
        let llm = spans.iter().filter(|s| s.span_kind == SpanKind::Llm).count();
        assert!((llm as f64) / (spans.len() as f64) < 0.06);
        let telemetry: Vec<&Span> = spans.iter().filter(|s| s.name == "db.query" || s.name == "http.get").collect();
        assert!(!telemetry.is_empty());
        for s in telemetry {
            assert!(s.parent_span_id.is_some());
            assert!(s.input.is_none() && s.output.is_none());
        }
    }

    #[test]
    fn keys_cases_have_twenty_distinct_keys() {
        let (spans, _) = gen_spans("keys1k", Some(50)).unwrap();
        let mut used = HashSet::new();
        for s in &spans {
            assert_eq!(s.attributes.len(), KEYS_PER_SPAN);
            for key in s.attributes.keys() {
                used.insert(key.clone());
                match key.strip_prefix("tool.arg.") {
                    Some(n) => {
                        let n: usize = n.parse().unwrap();
                        assert!(n % 2 == 0 && n < 1_000, "{key}");
                    }
                    None => assert!(key.len() == 32 && is_hex(key), "{key}"),
                }
            }
        }
        assert!(used.len() <= 1_000);
    }

    #[test]
    fn giant_case_has_giant_inputs() {
        let (spans, files) = gen_spans("giant", Some(200)).unwrap();
        let max = spans.iter().filter_map(|s| s.input.as_ref().map(String::len)).max().unwrap_or(0);
        assert!(max >= GIANT_BYTES - 3);
        let (trace, span) = files.spans[0];
        let first = spans
            .iter()
            .find(|s| *s.trace_id.as_bytes() == trace && *s.span_id.as_bytes() == span)
            .unwrap();
        assert!(first.input.as_ref().unwrap().len() >= GIANT_BYTES - 3);
    }

    #[test]
    fn gen_writes_a_case_directory() {
        let dir = temp_dir("gen");
        let (spans, files) = gen_spans("text_unique", Some(30)).unwrap();
        let total = spans.len();
        write_case(&dir, spans, &files).unwrap();

        let mut rows = 0;
        for i in 0..FILES {
            let mut prev: Option<([u8; 16], i64)> = None;
            for batch in read_input(&dir.join(format!("input/{i:03}.arrow"))) {
                let traces = fixed_binary(&batch, "trace_id").unwrap();
                let starts = batch
                    .column_by_name("start_time")
                    .unwrap()
                    .as_any()
                    .downcast_ref::<TimestampNanosecondArray>()
                    .unwrap();
                for r in 0..batch.num_rows() {
                    let key = (<[u8; 16]>::try_from(traces.value(r)).unwrap(), starts.value(r));
                    assert!(prev.is_none_or(|p| p <= key), "input {i} is not sorted");
                    prev = Some(key);
                }
                rows += batch.num_rows();
            }
        }
        assert_eq!(rows, total);

        let queries = fs::read_to_string(dir.join("cw_queries.txt")).unwrap();
        assert!(queries.lines().all(|q| !q.is_empty()));
        assert!(queries.contains("search:absent"));
        let traces = fs::read_to_string(dir.join("cw_traces.txt")).unwrap();
        assert_eq!(traces.lines().count(), IDS);
        assert!(traces.lines().all(|t| t.len() == 32 && is_hex(t)));
        let spans_text = fs::read_to_string(dir.join("cw_spans.txt")).unwrap();
        assert_eq!(spans_text.lines().count(), IDS);
        assert!(spans_text.lines().all(|l| {
            let (t, s) = l.split_once(' ').unwrap();
            t.len() == 32 && s.len() == 16 && is_hex(t) && is_hex(s)
        }));
        let needles = fs::read_to_string(dir.join("cw_needles.txt")).unwrap();
        assert!(needles.lines().all(|l| {
            let (name, hex) = l.split_once('\t').unwrap();
            !name.is_empty() && is_hex(hex)
        }));
        assert!(!dir.join("cw_attr.txt").exists());
    }

    #[test]
    fn keys_case_writes_its_attr_file() {
        let dir = temp_dir("keys");
        let (spans, files) = gen_spans("keys100", Some(20)).unwrap();
        write_case(&dir, spans, &files).unwrap();
        assert_eq!(fs::read_to_string(dir.join("cw_attr.txt")).unwrap(), "tool.arg.2\t\"v3\"\n");
        assert!(!dir.join("cw_needles.txt").exists());
    }

    /// Every `(trace_id, span_id)` of `batches`, sorted.
    fn id_pairs(batches: &[RecordBatch]) -> Vec<([u8; 16], [u8; 8])> {
        let mut out = Vec::new();
        for b in batches {
            let traces = fixed_binary(b, "trace_id").unwrap();
            let spans = fixed_binary(b, "span_id").unwrap();
            for r in 0..b.num_rows() {
                out.push((<[u8; 16]>::try_from(traces.value(r)).unwrap(), <[u8; 8]>::try_from(spans.value(r)).unwrap()));
            }
        }
        out.sort();
        out
    }

    /// The `(trace_id, start_time)` keys of `batch`, in row order.
    fn sort_keys(batch: &RecordBatch) -> Vec<([u8; 16], i64)> {
        let traces = fixed_binary(batch, "trace_id").unwrap();
        let starts = start_times(batch).unwrap();
        (0..batch.num_rows())
            .map(|r| (<[u8; 16]>::try_from(traces.value(r)).unwrap(), starts.value(r)))
            .collect()
    }

    /// The text files of a case directory, as strings.
    fn text_files(dir: &Path) -> Vec<String> {
        ["cw_queries.txt", "cw_traces.txt", "cw_spans.txt", "cw_needles.txt"]
            .iter()
            .map(|f| fs::read_to_string(dir.join(f)).unwrap())
            .collect()
    }

    #[test]
    fn split_keeps_every_row_in_time_slices() {
        let dir = temp_dir("split-slices");
        let (spans, files) = gen_spans("text_unique", Some(200)).unwrap();
        write_case(&dir, spans, &files).unwrap();
        let mut batches = Vec::new();
        for i in 0..FILES {
            batches.extend(read_input(&dir.join(format!("input/{i:03}.arrow"))));
        }
        let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
        let want = id_pairs(&batches);
        for n in [1, 3, 10] {
            let slices = split_batches(&batches, n).unwrap();
            let rows: usize = slices.iter().map(RecordBatch::num_rows).sum();
            assert_eq!(rows, total, "n = {n}");
            assert_eq!(id_pairs(&slices), want, "n = {n}");
            let mut prev_max: Option<i64> = None;
            for slice in &slices {
                let keys = sort_keys(slice);
                assert!(keys.windows(2).all(|w| w[0] <= w[1]), "slice is not sorted (n = {n})");
                let starts: Vec<i64> = keys.iter().map(|k| k.1).collect();
                let lo = *starts.iter().min().unwrap();
                let hi = *starts.iter().max().unwrap();
                assert!(prev_max.is_none_or(|p| p <= lo), "slices overlap in time (n = {n})");
                prev_max = Some(hi);
            }
        }
    }

    #[test]
    fn split_drops_empty_slices() {
        let (spans, _) = gen_spans("text_unique", Some(30)).unwrap();
        let batch = spans_to_record_batch(&spans).unwrap().slice(0, 3);
        let slices = split_batches(&[batch], 10).unwrap();
        assert_eq!(slices.len(), 3);
        assert!(slices.iter().all(|s| s.num_rows() == 1));
    }

    #[test]
    fn flush_rows_scales_to_the_limit() {
        assert_eq!(flush_rows(1_000, 4_000, 1_000), 250);
        assert_eq!(flush_rows(1_000, 4_001, 1_000), 250);
        assert_eq!(flush_rows(10, 100, 1_000), 10);
        assert_eq!(flush_rows(10, 0, 1_000), 10);
    }

    #[test]
    fn flush_input_takes_the_first_rows() {
        let dir = temp_dir("flush-input");
        let (spans, files) = gen_spans("text_unique", Some(200)).unwrap();
        let total = spans.len();
        write_case(&dir, spans, &files).unwrap();
        flush_input_cmd(&[dir.display().to_string()]).unwrap();
        let batches = read_input(&dir.join("flush.arrow"));
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), total);
        let keys = sort_keys(&batches[0]);
        assert!(keys.windows(2).all(|w| w[0] <= w[1]), "flush.arrow is not sorted");
    }

    #[test]
    fn split_cmd_rewrites_the_input() {
        let dir = temp_dir("split-cmd");
        let d = dir.display().to_string();
        let (spans, files) = gen_spans("text_unique", Some(100)).unwrap();
        write_case(&dir, spans, &files).unwrap();
        flush_input_cmd(std::slice::from_ref(&d)).unwrap();
        fs::create_dir_all(dir.join("pq_cetra")).unwrap();
        fs::write(dir.join("pq_cetra/000.parquet"), b"old").unwrap();
        let before = text_files(&dir);

        split_cmd(&[d.clone(), "6".to_owned()]).unwrap();

        assert_eq!(fs::read_dir(dir.join("input")).unwrap().count(), 6);
        assert!(!dir.join("pq_cetra").exists());
        assert!(!dir.join("flush.arrow").exists());
        assert_eq!(text_files(&dir), before);
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("input.old-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn split_refuses_after_an_interrupted_split() {
        let dir = temp_dir("split-interrupted");
        let d = dir.display().to_string();
        let (spans, files) = gen_spans("text_unique", Some(100)).unwrap();
        write_case(&dir, spans, &files).unwrap();
        let backup = dir.join("input.old-123");
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("000.arrow"), b"old cut").unwrap();
        let input_before: Vec<PathBuf> = input_paths(&dir).unwrap();

        let err = split_cmd(&[d, "6".to_owned()]).unwrap_err().to_string();

        assert!(err.contains("input.old-123") && err.contains("an earlier split was interrupted"), "{err}");
        assert_eq!(fs::read(backup.join("000.arrow")).unwrap(), b"old cut");
        assert_eq!(input_paths(&dir).unwrap(), input_before);
    }
}
