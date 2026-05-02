// Indexer no longer calls `rumoca_phase_parse::parse_to_ast` directly.
// Going through `rumoca_session::parsing::parse_files_parallel` routes
// every parse through rumoca's content-hash keyed artifact cache
// (`<workspace>/.cache/rumoca/parsed-files/`). Second indexer runs and
// the workbench's runtime drill-ins share the same cache entries, so
// a file parsed here is instant at runtime and vice versa.
use rumoca_session::parsing::ast::{Causality, ClassDef, ClassType, StoredDefinition, Token, Variability, Annotation, Modification};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Instant;

/// CLI options parsed from `std::env::args()`. Kept tiny on purpose
/// — adding `clap` would pull megabytes of build into a tool whose
/// whole point is to make the workbench start faster.
#[derive(Default)]
struct Options {
    verbose: bool,
    warm: bool,
    warm_only: Option<Vec<String>>,
}

impl Options {
    fn parse() -> Self {
        let mut opts = Self::default();
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "-v" | "--verbose" => opts.verbose = true,
                "--warm" => opts.warm = true,
                "--warm-only" => {
                    let list = iter.next().unwrap_or_else(|| {
                        eprintln!("error: --warm-only requires a comma-separated list of qualified names");
                        std::process::exit(2);
                    });
                    opts.warm_only = Some(
                        list.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                    // Implies --warm so users don't have to pass both.
                    opts.warm = true;
                }
                other => {
                    eprintln!("error: unknown argument `{other}` (use --help for usage)");
                    std::process::exit(2);
                }
            }
        }
        opts
    }
}

/// Lower a parameter's default expression to a short display string
/// suitable for `%paramName` substitution in icon Text primitives.
/// Returns empty for expressions we can't summarise (function calls,
/// arithmetic, etc.) — the substitutor then drops the placeholder
/// rather than printing a confusing partial value.
///
/// - `Terminal{Bool, "true"}`              → `"true"`
/// - `Terminal{UnsignedReal, "100"}`       → `"100"`
/// - `Terminal{String, "Hello"}`           → `"Hello"` (quotes stripped)
/// - `ComponentReference{Foo.Bar.Baz}`     → `"Baz"` (enum-style leaf)
/// - `Unary{op:Minus, rhs:Terminal..}`     → `"-100"`
/// - anything else                          → `""`
fn format_default_expr(expr: &rumoca_session::parsing::ast::Expression) -> String {
    use rumoca_session::parsing::ast::{Expression, OpUnary, TerminalType};
    match expr {
        Expression::Terminal { terminal_type, token } => {
            let raw = token.text.as_ref();
            match terminal_type {
                TerminalType::String => raw.trim_matches('"').to_string(),
                _ => raw.to_string(),
            }
        }
        Expression::ComponentReference(cref) => cref
            .parts
            .last()
            .map(|p| p.ident.text.as_ref().to_string())
            .unwrap_or_default(),
        Expression::Unary { op, rhs } => match (op, rhs.as_ref()) {
            (OpUnary::Minus(_), inner) => {
                let inner = format_default_expr(inner);
                if inner.is_empty() {
                    String::new()
                } else {
                    format!("-{}", inner)
                }
            }
            // `+1` is parsed as Unary{Plus, Terminal "1"}. Without
            // this branch the leading `+` swallowed the whole
            // expression to empty, so MSL params declared as `k1=+1`
            // (Math.Add, Math.Add3) had blank defaults in the index.
            (OpUnary::Plus(_), inner) => {
                let inner = format_default_expr(inner);
                if inner.is_empty() {
                    String::new()
                } else {
                    format!("+{}", inner)
                }
            }
            _ => String::new(),
        },
        Expression::Parenthesized { inner } => format_default_expr(inner),
        // Array literals like `{1}`, `{1, 2, 3}` — render with
        // braces so the Modelica icon text reads natively (matches
        // what OMEdit shows for `qd_max=%qd_max` on KinematicPTP).
        // Multi-dimensional arrays nest the same formatting.
        Expression::Array { elements, .. } => {
            let parts: Vec<String> = elements
                .iter()
                .map(format_default_expr)
                .collect();
            if parts.iter().any(|s| s.is_empty()) {
                String::new()
            } else {
                format!("{{{}}}", parts.join(","))
            }
        }
        _ => String::new(),
    }
}

fn print_help() {
    println!("msl_indexer — index MSL components and (optionally) warm rumoca compile caches");
    println!();
    println!("USAGE:");
    println!("  msl_indexer [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("  -v, --verbose         Per-file logging during the scan pass");
    println!("      --warm            After indexing, full-compile a default list of common");
    println!("                        MSL examples to warm rumoca's semantic-summary cache.");
    println!("                        First workbench compile of those examples then becomes");
    println!("                        a cache hit (ms instead of minutes).");
    println!("      --warm-only LIST  Comma-separated qualified names to warm instead of the");
    println!("                        default list. Implies --warm.");
    println!("                        e.g. --warm-only Modelica.Blocks.Examples.PID_Controller");
    println!("  -h, --help            Show this help");
    println!();
    println!("OUTPUT:");
    println!("  msl_index.json (next to the MSL source root) — read by the workbench at startup");
    println!("  ~/Documents/luncosim-workspace/.cache/rumoca/parsed-files/ — populated as a side");
    println!("    effect of the scan pass; --warm additionally populates semantic-summaries/");
}

/// Default warm list — the examples users hit most often when they
/// open the Welcome page. Compiling these once after a cache wipe
/// makes the first workbench interaction with each one fast.
/// Add new entries here as we discover other common landings.
const DEFAULT_WARM_EXAMPLES: &[&str] = &[
    "Modelica.Blocks.Examples.PID_Controller",
    "Modelica.Blocks.Examples.Filter",
    "Modelica.Mechanics.Rotational.Examples.First",
    "Modelica.Mechanics.Translational.Examples.Damper",
    "Modelica.Electrical.Analog.Examples.ChuaCircuit",
    "Modelica.Electrical.Analog.Examples.RLCircuit",
    "Modelica.Thermal.HeatTransfer.Examples.TwoMasses",
];

// ---------------------------------------------------------------------------
// Fallback strategy for ports without a Placement annotation
// ---------------------------------------------------------------------------

/// How to assign a diagram position to a connector that carries no
/// `annotation(Placement(...))` declaration.
///
/// # Why this exists
/// The Modelica Language Specification (§18.6) defines the *format* of the
/// Placement annotation but **does not specify any default layout** when it is
/// absent. Quote: "The Placement annotation ... is used to define the placement
/// of the component in the diagram layer."  No default is stated — tools are free
/// to do whatever they want.
///
/// In practice, every MSL connector declares an explicit Placement, so this
/// fallback only fires for:
///   - User-defined components that have no graphical layer at all
///   - Third-party libraries with incomplete annotations
///   - Components whose Placement the rumoca parser cannot yet extract
///
/// # Rationale for `SideByCausality` as the active default
/// Scanning the MSL reveals an informal but consistent convention:
///   - causal `input`  connectors sit at (-100..110, ~0)  → left side
///   - causal `output` connectors sit at (+100..110, ~0)  → right side
///   - acausal connectors in `extends OnePort` / `TwoPort` follow the same
///     left/right pattern: `p` left, `n` right
/// This is **not a standard** — it is an observed pattern that produces
/// sensible schematics for the vast majority of library components.
///
/// Change `PLACEMENT_FALLBACK` below to switch strategy without touching logic.
#[derive(Clone, Copy)]
enum PortPlacementFallback {
    /// inputs → left (-100, 0), outputs → right (+100, 0),
    /// acausal connectors alternate left/right/top/bottom by insertion order.
    /// Mirrors informal MSL convention. **Not a Modelica standard.**
    SideByCausality,
    /// Every un-annotated port gets center (0, 0).
    /// Use this when you want missing annotations to be visually obvious
    /// (ports pile up in the middle, easy to spot).
    AllCenter,
    /// All un-annotated ports stacked on the left side, evenly spaced.
    AllLeft,
}

/// Active fallback strategy — the only line you need to edit to change behaviour.
const PLACEMENT_FALLBACK: PortPlacementFallback = PortPlacementFallback::SideByCausality;

fn fallback_port_position(causality: &Causality, port_index: usize) -> (f32, f32) {
    match PLACEMENT_FALLBACK {
        PortPlacementFallback::SideByCausality => match causality {
            Causality::Input(_)  => (-100.0, 0.0),
            Causality::Output(_) => (100.0, 0.0),
            _ => match port_index % 4 {
                0 => (-100.0, 0.0),
                1 => (100.0, 0.0),
                2 => (0.0, 100.0),
                _ => (0.0, -100.0),
            },
        },
        PortPlacementFallback::AllCenter => (0.0, 0.0),
        PortPlacementFallback::AllLeft => {
            let y = 50.0 - port_index as f32 * 20.0;
            (-100.0, y)
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PortDef {
    name: String,
    connector_type: String,
    msl_path: String,
    is_flow: bool,
    /// Port position in Modelica diagram coordinates (-100..100).
    /// x < 0 = left side, x > 0 = right side, y > 0 = top, y < 0 = bottom.
    /// (0, 0) means no annotation was found and position is unknown.
    x: f32,
    y: f32,
    /// Port size in the parent class's icon coords (placement extent
    /// width/height). Used by the canvas to scale the connector
    /// class's authored Icon to OMEdit-equivalent size. Defaults to
    /// 20×20 (matches the most common MSL placement) when no
    /// Placement annotation was found.
    #[serde(default = "default_port_size")]
    size_x: f32,
    #[serde(default = "default_port_size")]
    size_y: f32,
    /// Rotation from `Placement(transformation(rotation=...))` on the
    /// port declaration. Plumbed to the canvas so connector icons
    /// land oriented (e.g. PI's bottom `u_m` input has rotation=270
    /// so the triangle points up).
    #[serde(default)]
    rotation_deg: f32,
}

fn default_port_size() -> f32 { 20.0 }

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ParamDef {
    name: String,
    param_type: String,
    default: String,
    unit: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct MSLComponentDef {
    name: String,
    msl_path: String,
    category: String,
    display_name: String,
    description: Option<String>,
    /// Short `"…"` string written after the class name in Modelica
    /// source, cleaned of quotes. Distinct from `description` (which
    /// historically stored the `{:?}` Debug form for compatibility);
    /// UI code should prefer this field.
    #[serde(default)]
    short_description: Option<String>,
    /// First plain-text paragraph of
    /// `annotation(Documentation(info="…"))`, HTML-stripped. `None`
    /// when the class has no Documentation annotation (rare for
    /// `Examples.*` classes). The Welcome / MSL Library browser
    /// uses this for richer card copy.
    #[serde(default)]
    documentation_info: Option<String>,
    /// True when `msl_path` contains `.Examples.` — MSL convention
    /// for runnable learning material. Cheap flag so the browser
    /// doesn't have to re-check the path everywhere.
    #[serde(default)]
    is_example: bool,
    /// Second-level MSL package name for navigation grouping —
    /// `Modelica.Electrical.Analog.Examples.*` → `"Electrical"`.
    /// Empty for non-MSL classes. Drives the domain-chip filter.
    #[serde(default)]
    domain: String,
    /// Kind of class: "model", "block", "connector", "record", "type",
    /// "package", "function", "class", "operator". Lower-case to
    /// match Modelica source keywords.
    #[serde(default)]
    class_kind: String,
    icon_text: Option<String>,
    /// Parsed `Icon(graphics={...})` annotation — already merged
    /// across the `extends` chain via `extract_icon_inherited`. This
    /// is the only icon source the runtime uses; the regex-on-Debug
    /// SVG generator that used to live here was retired (it produced
    /// spurious primitives because Debug-string regexing can't tell
    /// graphics primitives apart reliably).
    #[serde(default)]
    icon_graphics: Option<lunco_modelica::annotations::Icon>,
    #[serde(default)]
    diagram_graphics: Option<lunco_modelica::annotations::Diagram>,
    ports: Vec<PortDef>,
    parameters: Vec<ParamDef>,
}

/// True when the top-level class `name` is actually the package
/// declared by the containing folder — i.e. the `package.mo` file
/// declares `package <FolderName> … end <FolderName>` per MLS.
///
/// Without this check, a naïve `"{current_path}.{name}"` join for
/// `Modelica/Blocks/package.mo` produces `Modelica.Blocks.Blocks`
/// instead of `Modelica.Blocks`. Nested classes then compound:
/// `Modelica.Blocks.Blocks.Examples.BooleanNetwork1`.
///
/// Two cases qualify:
///  1. `name == "package"` — legacy / hand-written files that
///     literally named the class `package`.
///  2. `is_package_file` AND the leaf segment of `current_path`
///     matches `name` — the MSL-typical case.
fn is_top_level_self_ref(name: &str, current_path: &str, is_package_file: bool) -> bool {
    if name == "package" {
        return true;
    }
    if is_package_file {
        if let Some(leaf) = current_path.rsplit('.').next() {
            return leaf == name;
        }
    }
    false
}

struct MSLIndexer {
    classes: HashMap<String, ClassDef>,
    /// Per-class first-paragraph plain-text from
    /// `annotation(Documentation(info="…"))`. Keyed by the simple
    /// class name (not fully-qualified) — good enough at MSL scale
    /// because `Examples.*` class names are unique within a file
    /// and the browser looks it up from the `short_name`. Populated
    /// by `extract_documentation_infos` during `scan_dir` while the
    /// `.mo` source is still in memory.
    doc_infos: HashMap<String, String>,
    /// Per-file logging during scan_dir when true; otherwise a tick
    /// every couple seconds with running counters.
    verbose: bool,
    files_scanned: usize,
    bytes_scanned: usize,
    scan_started: Option<Instant>,
    last_progress_print: Option<Instant>,
    /// Bundle of every parsed `.mo` collected during the scan. Written
    /// at the end of `main()` to `.cache/msl/parsed-msl.bin` so the
    /// workbench can install pre-parsed `StoredDefinition`s in ~1s
    /// via `Session::replace_parsed_source_set` — mirrors the wasm
    /// runtime's `parsed-*.bin.zst` strategy on native.
    parsed_bundle: Vec<(String, StoredDefinition)>,
}

/// Scan a Modelica source buffer and map each class's simple name to
/// the **plain-text first paragraph** of its
/// `annotation(Documentation(info="…"))`, if any.
///
/// Strategy: stack-match `model|block|…|function NAME` openers against
/// `end NAME;` tokens to build class byte-ranges, then for every
/// `Documentation(info="…")` pick the **innermost** enclosing range.
/// This handles nested classes (MSL's `protected model Internal …`
/// inside a larger example) correctly.
///
/// After matching, strip HTML tags and common entities, collapse
/// whitespace, and keep only the first paragraph (`</p>` boundary,
/// falling back to a double-newline). Dropping the rest means the
/// index stays small (~200 examples × < 200 chars each).
fn extract_documentation_infos(source: &str) -> HashMap<String, String> {
    // Openers we care about. `operator` covers `operator record` /
    // `operator function` (MLS §14.4) and `type` covers typedefs that
    // occasionally carry their own Documentation block.
    let opener_re = regex::Regex::new(
        r"(?m)\b(?:partial\s+)?(?:model|block|class|connector|record|package|function|type|operator)\s+(\w+)\b",
    )
    .expect("opener regex");
    let end_re = regex::Regex::new(r"(?m)\bend\s+(\w+)\s*;").expect("end regex");
    // Greedy-aware info capture. Modelica strings can contain escaped
    // quotes (`\"`); the `(?:[^"\\]|\\.)*` alternation handles that.
    let doc_re = regex::Regex::new(
        r#"(?s)Documentation\s*\(\s*info\s*=\s*"((?:[^"\\]|\\.)*)""#,
    )
    .expect("doc regex");

    #[derive(Debug)]
    enum Ev {
        Open(String, usize),
        End(String, usize),
    }
    let mut events: Vec<Ev> = Vec::new();
    for m in opener_re.captures_iter(source) {
        events.push(Ev::Open(
            m.get(1).unwrap().as_str().to_string(),
            m.get(0).unwrap().start(),
        ));
    }
    for m in end_re.captures_iter(source) {
        events.push(Ev::End(
            m.get(1).unwrap().as_str().to_string(),
            m.get(0).unwrap().start(),
        ));
    }
    events.sort_by_key(|e| match e {
        Ev::Open(_, p) | Ev::End(_, p) => *p,
    });

    struct Range {
        name: String,
        start: usize,
        end: usize,
    }
    let mut ranges: Vec<Range> = Vec::new();
    let mut stack: Vec<(String, usize)> = Vec::new();
    for e in events {
        match e {
            Ev::Open(n, p) => stack.push((n, p)),
            Ev::End(n, p) => {
                // Match against the nearest open with the same name —
                // tolerant of MLS-legal re-openings of identically-named
                // nested classes inside sibling branches.
                if let Some(idx) = stack.iter().rposition(|(sn, _)| sn == &n) {
                    let (name, start) = stack.remove(idx);
                    ranges.push(Range { name, start, end: p });
                }
            }
        }
    }

    let mut out: HashMap<String, String> = HashMap::new();
    for caps in doc_re.captures_iter(source) {
        let pos = caps.get(0).unwrap().start();
        let raw = caps.get(1).unwrap().as_str();
        // Innermost range containing the Documentation opener.
        let inner = ranges
            .iter()
            .filter(|r| r.start <= pos && pos <= r.end)
            .min_by_key(|r| r.end.saturating_sub(r.start));
        if let Some(r) = inner {
            // Keep the FIRST Documentation per class — MSL sometimes
            // nests `Documentation` inside per-component annotations
            // (rare) and we want the class-level one, which comes
            // first in source order within the class body.
            out.entry(r.name.clone())
                .or_insert_with(|| clean_info_text(raw));
        }
    }
    out
}

/// Turn a raw Modelica `info="…"` string into UI-ready plain text.
/// Unescapes Modelica string escapes, strips HTML tags and common
/// entities, collapses whitespace, and keeps only the first
/// paragraph (so a multi-screen MSL doc fits in a card tagline).
fn clean_info_text(raw: &str) -> String {
    // Modelica string escapes we actually see in MSL.
    let mut s = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('"') => s.push('"'),
                Some('\\') => s.push('\\'),
                Some(other) => {
                    s.push('\\');
                    s.push(other);
                }
                None => s.push('\\'),
            }
        } else {
            s.push(c);
        }
    }

    // First-paragraph boundary: `</p>` is the MSL convention; fall
    // back to a blank line so prose-only info strings still split.
    let lower = s.to_ascii_lowercase();
    if let Some(idx) = lower.find("</p>") {
        s.truncate(idx);
    } else if let Some(idx) = s.find("\n\n") {
        s.truncate(idx);
    }

    // Strip tags + entities. Regex cost here is tiny (called once
    // per class at index time, never at runtime).
    let tag_re = regex::Regex::new(r"<[^>]*>").expect("tag regex");
    let no_tags = tag_re.replace_all(&s, " ");
    let decoded = no_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    let ws_re = regex::Regex::new(r"\s+").expect("ws regex");
    ws_re.replace_all(&decoded, " ").trim().to_string()
}

/// Top-level MSL domain for grouping (`Modelica.Electrical.Analog.*`
/// → `Electrical`). Returns empty string for classes outside the
/// `Modelica.*` tree, which keeps third-party libraries from
/// polluting the browser chips.
fn msl_domain(full_name: &str) -> String {
    let mut parts = full_name.split('.');
    if parts.next() == Some("Modelica") {
        parts.next().unwrap_or("").to_string()
    } else {
        String::new()
    }
}

fn class_kind_str(kind: &ClassType) -> &'static str {
    match kind {
        ClassType::Model => "model",
        ClassType::Class => "class",
        ClassType::Block => "block",
        ClassType::Connector => "connector",
        ClassType::Record => "record",
        ClassType::Type => "type",
        ClassType::Package => "package",
        ClassType::Function => "function",
        ClassType::Operator => "operator",
    }
}

/// Join a class's `description: Vec<Token>` tokens into a single
/// string and strip the surrounding `"…"` quotes. Modelica parses
/// the description as a sequence of concatenated string literals so
/// authors can split long descriptions across lines with `+`; we
/// just join and clean up.
fn clean_short_description(tokens: &[Token]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let mut s = String::new();
    for tok in tokens {
        let t = tok.text.trim();
        let t = t.strip_prefix('"').unwrap_or(t);
        let t = t.strip_suffix('"').unwrap_or(t);
        if !t.is_empty() {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(t);
        }
    }
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

impl MSLIndexer {
    fn new() -> Self {
        Self {
            classes: HashMap::new(),
            doc_infos: HashMap::new(),
            verbose: false,
            files_scanned: 0,
            bytes_scanned: 0,
            scan_started: None,
            last_progress_print: None,
            parsed_bundle: Vec::with_capacity(2700),
        }
    }

    fn scan_dir(&mut self, dir: &Path, package_prefix: &str) {
        if self.scan_started.is_none() {
            self.scan_started = Some(Instant::now());
            self.last_progress_print = Some(Instant::now());
        }
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let folder_name = path.file_name().unwrap().to_str().unwrap();
                    let new_prefix = if package_prefix.is_empty() {
                        folder_name.to_string()
                    } else {
                        format!("{}.{}", package_prefix, folder_name)
                    };
                    self.scan_dir(&path, &new_prefix);
                } else if path.extension().map_or(false, |ext| ext == "mo") {
                    if let Ok(source) = fs::read_to_string(&path) {
                        self.files_scanned += 1;
                        self.bytes_scanned += source.len();
                        // Verbose: one line per file as it's parsed.
                        // Quiet: a tick every 2s with running counters
                        // so the user sees liveness without 2.5k log
                        // lines.
                        if self.verbose {
                            let kb = source.len() as f64 / 1024.0;
                            println!(
                                "[scan] {} ({:.1} KB)",
                                path.strip_prefix(dir.ancestors().last().unwrap_or(dir))
                                    .unwrap_or(&path)
                                    .display(),
                                kb,
                            );
                        } else if let Some(last) = self.last_progress_print {
                            if last.elapsed() >= std::time::Duration::from_secs(2) {
                                let elapsed = self
                                    .scan_started
                                    .map(|t| t.elapsed().as_secs_f64())
                                    .unwrap_or(0.0);
                                let mb = self.bytes_scanned as f64 / (1024.0 * 1024.0);
                                println!(
                                    "[scan] {} files, {:.1} MB, {:.1}s elapsed (current: {})",
                                    self.files_scanned,
                                    mb,
                                    elapsed,
                                    package_prefix,
                                );
                                self.last_progress_print = Some(Instant::now());
                            }
                        }
                        let file_name = path
                            .file_name()
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_string();
                        self.ingest_file(&path, &source, &file_name, package_prefix);
                    }
                }
            }
        }
    }

    /// Parse and index a single `.mo` file. Extracted from the file
    /// branch of [`scan_dir`] so we can also ingest top-level companion
    /// files (e.g. `Complex.mo`) that live next to `Modelica/` rather
    /// than inside it.
    fn ingest_file(
        &mut self,
        path: &Path,
        source: &str,
        file_name: &str,
        package_prefix: &str,
    ) {
        // `package.mo` declares `package <FolderName> …
        // end <FolderName>` per MLS — the class inside IS the package,
        // so we must collapse rather than prefix. Track the file role so
        // both the placement mapping below and `add_stored_definition`
        // treat the class name correctly.
        let is_package_file = file_name == "package.mo";
        // Parse through rumoca-session's cache. A content-hash-matching
        // entry at `.cache/rumoca/parsed-files/` deserialises from
        // bincode in ~ms; a miss pays the full rumoca parse once and
        // writes the bincode so the NEXT indexer run and the workbench's
        // first drill-in are both instant. `parse_files_parallel` with
        // one path is the public entry point that exercises the cache;
        // rayon overhead is negligible for length-1.
        let ast_opt =
            rumoca_session::parsing::parse_files_parallel(&[path.to_path_buf()])
                .ok()
                .and_then(|mut pairs| pairs.pop().map(|(_, ast)| ast));
        if let Some(ast) = ast_opt {
            for (k, v) in extract_documentation_infos(source) {
                self.doc_infos.entry(k).or_insert(v);
            }
            self.parsed_bundle
                .push((path.to_string_lossy().to_string(), ast.clone()));
            self.add_stored_definition(ast, package_prefix, is_package_file);
        }
    }

    /// Top-level companion-file shorthand: load a flat `.mo` at the
    /// MSL cache root with no package prefix. Used for `Complex.mo`
    /// and similar siblings of the main `Modelica/` tree.
    fn ingest_root_file(&mut self, path: &Path, source: &str) {
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        self.ingest_file(path, source, &file_name, "");
    }

    fn add_stored_definition(
        &mut self,
        ast: StoredDefinition,
        current_path: &str,
        is_package_file: bool,
    ) {
        for (name, class) in ast.classes {
            let full_name = if is_top_level_self_ref(&name, current_path, is_package_file) {
                current_path.to_string()
            } else if current_path.is_empty() {
                name.to_string()
            } else {
                format!("{}.{}", current_path, name)
            };
            self.add_class(class, &full_name);
        }
    }

    fn add_class(&mut self, class: ClassDef, full_name: &str) {
        for (nested_name, nested_class) in class.classes.clone() {
            self.add_class(nested_class, &format!("{}.{}", full_name, nested_name));
        }
        self.classes.insert(full_name.to_string(), class);
    }

    fn resolve_inheritance(&self, class_name: &str, ports: &mut Vec<PortDef>, params: &mut Vec<ParamDef>, visited: &mut HashSet<String>) {
        if visited.contains(class_name) { return; }
        visited.insert(class_name.to_string());

        if let Some(class) = self.classes.get(class_name) {
            // 1. Resolve base classes first (extends)
            for ext in &class.extends {
                let base_short_name = ext.base_name.name.iter().map(|s| s.text.to_string()).collect::<Vec<String>>().join(".");
                
                // Heuristic for Modelica name resolution
                let mut resolved_base = None;
                let mut current_scope = class_name.to_string();
                while !current_scope.is_empty() {
                    let candidate = if current_scope.contains('.') {
                        format!("{}.{}", current_scope.rsplitn(2, '.').nth(1).unwrap_or(""), base_short_name)
                    } else {
                        base_short_name.clone()
                    };

                    if self.classes.contains_key(&candidate) {
                        resolved_base = Some(candidate);
                        break;
                    }

                    if current_scope.contains('.') {
                        current_scope = current_scope.rsplitn(2, '.').nth(1).unwrap().to_string();
                    } else {
                        current_scope.clear();
                    }
                }

                // Try absolute if not found
                if resolved_base.is_none() {
                    if self.classes.contains_key(&base_short_name) {
                        resolved_base = Some(base_short_name);
                    } else if self.classes.contains_key(&format!("Modelica.{}", base_short_name)) {
                        resolved_base = Some(format!("Modelica.{}", base_short_name));
                    }
                }

                if let Some(base) = resolved_base {
                    self.resolve_inheritance(&base, ports, params, visited);
                }
            }

            // 2. Add local components
            for comp in class.components.values() {
                if matches!(comp.variability, Variability::Parameter(_)) {
                    if !params.iter().any(|p| p.name == comp.name) {
                        // Format the default value for `%paramName`
                        // text substitution at render time. Prefer
                        // the explicit binding (`= expr`); fall back
                        // to `start=` modification (`parameter Real
                        // R(start=1)`) when no binding is present.
                        // Numeric and string literals show as-written;
                        // enum refs collapse to the leaf name (matches
                        // OMEdit); array literals render `{a,b,c}`.
                        let default = comp
                            .binding
                            .as_ref()
                            .map(format_default_expr)
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| {
                                // `comp.start: Expression` — Empty when no
                                // explicit start was given. format_default_expr
                                // returns "" for `Empty` so this is safe.
                                format_default_expr(&comp.start)
                            });
                        // TODO: resolve `unit` from the type definition.
                        // For `parameter SI.Torque tau_constant` the
                        // authoritative unit lives on `Modelica.Units.SI.Torque`
                        // as `type Torque = Real(unit="N.m")`. Resolve
                        // `comp.type_name` through the scope chain +
                        // imports, walk the `extends Real(unit=...)`
                        // modification, and store the result here so
                        // the canvas substitution (currently using a
                        // hand-maintained table in
                        // `canvas_diagram::si_unit_suffix`) can read
                        // `p.unit` directly. Until then `unit` is None
                        // and user-defined SI types lose their suffix.
                        params.push(ParamDef {
                            name: comp.name.clone(),
                            param_type: comp.type_name.to_string(),
                            default,
                            unit: None,
                        });
                    }
                }

                let type_str = comp.type_name.to_string();
                let lower = type_str.to_lowercase();
                
                let is_port = lower.contains("pin") || 
                              lower.contains("flange") || 
                              lower.contains("port") || 
                              lower.contains("input") || 
                              lower.contains("output");
                
                let has_causality = matches!(comp.causality, Causality::Input(_)) || 
                                    matches!(comp.causality, Causality::Output(_));

                if is_port || has_causality {
                    // Skip conditional connectors (e.g. `BooleanInput
                    // reset if use_reset` on Continuous.Integrator).
                    // They're declared in the type's interface but
                    // *not instantiated* unless the condition is true.
                    // Including them in the index made every Integrator
                    // instance render extra port dots for ports that
                    // aren't actually present in this instance.
                    //
                    // We're conservative — `condition.is_some()` is
                    // enough; we don't try to evaluate the condition.
                    // Worst case: a connector that's always-on via
                    // `if true` gets dropped, which is fine for the
                    // index (the user can still wire it; the dot just
                    // won't pre-render).
                    //
                    // TODO: per-instance conditional resolution.
                    // -----------------------------------------------
                    // The current uniform skip is correct for the
                    // common "default-off MSL conditional" case but
                    // creates a UX gap when a user *enables* the
                    // conditional on a specific instance (e.g.
                    // `Integrator integrator(use_reset=true)`):
                    // simulation works, but the canvas never renders
                    // the `reset` dot, so the user can't drag a wire
                    // to it in the diagram editor.
                    //
                    // The fix is a 3-step upgrade:
                    //   1. Index the conditional ports too — add
                    //      `PortDef.conditional: Option<String>`
                    //      storing the condition expression source
                    //      (e.g. `"use_reset"`).
                    //   2. In the canvas projector, for each
                    //      conditional port consult the *instance's*
                    //      modifications (Integrator(use_reset=true))
                    //      with the class's parameter default as
                    //      fallback. Decide render-vs-skip per
                    //      instance.
                    //   3. Render conditionally-on ports in a slightly
                    //      different style (dashed outline) so users
                    //      see "this port only exists because the
                    //      parameter is on."
                    //
                    // Most MSL conditions are plain boolean parameter
                    // refs (`use_reset`, `useSupport`, `useHeatPort`),
                    // so a 90%-coverage implementation is small.
                    if comp.condition.is_some() {
                        continue;
                    }
                    // Skip protected components — they're internal to
                    // the model (e.g. Integrator's `local_reset` /
                    // `local_set`) and shouldn't render as external
                    // ports. OMEdit / Dymola don't draw them either.
                    if comp.is_protected {
                        continue;
                    }

                    if !ports.iter().any(|p| p.name == comp.name) {
                        // Read the placement straight from rumoca's
                        // typed annotation tree — same code path the
                        // workbench uses at runtime
                        // (`crate::annotations::extract_placement`).
                        // Replaces the prior text-regex scan that
                        // (1) couldn't pull `origin=` when authored
                        // before `extent=`, (2) silently dropped
                        // placements declared in nested-class scopes,
                        // and (3) was MSL-specific by virtue of being
                        // unable to handle parser variations from
                        // other Modelica libraries. Going through
                        // rumoca means any library rumoca can parse
                        // also gets correctly-positioned ports.
                        let placement = lunco_modelica::annotations::extract_placement(
                            &comp.annotation,
                        );
                        let (x, y) = placement
                            .as_ref()
                            .map(|p| {
                                let extent = &p.transformation.extent;
                                let cx = (extent.p1.x + extent.p2.x) / 2.0
                                    + p.transformation.origin.x;
                                let cy = (extent.p1.y + extent.p2.y) / 2.0
                                    + p.transformation.origin.y;
                                (cx as f32, cy as f32)
                            })
                            .unwrap_or_else(|| {
                                fallback_port_position(&comp.causality, ports.len())
                            });
                        let (size_x, size_y) = placement
                            .as_ref()
                            .map(|p| {
                                let e = &p.transformation.extent;
                                ((e.p2.x - e.p1.x).abs() as f32, (e.p2.y - e.p1.y).abs() as f32)
                            })
                            .unwrap_or((20.0, 20.0));
                        let rotation_deg = placement
                            .as_ref()
                            .map(|p| p.transformation.rotation as f32)
                            .unwrap_or(0.0);

                        // Resolve `type_str` to a fully-qualified path so
                        // runtime callers (canvas port-icon renderer,
                        // wire-color resolver) can look the connector
                        // class up directly via `class_cache`. Without
                        // this, `parameter RealInput u` writes
                        // `msl_path = "RealInput"` and downstream
                        // resolution fails.
                        //
                        // Mirrors the scope-chain walk used above for
                        // `extends` resolution: starting from the
                        // declaring class's package, peel one segment
                        // at a time and check `self.classes`.
                        let mut resolved_path = type_str.clone();
                        if !self.classes.contains_key(&resolved_path) {
                            let mut current_scope = class_name.to_string();
                            while !current_scope.is_empty() {
                                let candidate = if current_scope.contains('.') {
                                    format!(
                                        "{}.{}",
                                        current_scope.rsplitn(2, '.').nth(1).unwrap_or(""),
                                        type_str,
                                    )
                                } else {
                                    type_str.clone()
                                };
                                if self.classes.contains_key(&candidate) {
                                    resolved_path = candidate;
                                    break;
                                }
                                if current_scope.contains('.') {
                                    current_scope = current_scope
                                        .rsplitn(2, '.')
                                        .nth(1)
                                        .unwrap()
                                        .to_string();
                                } else {
                                    current_scope.clear();
                                }
                            }
                        }
                        ports.push(PortDef {
                            name: comp.name.clone(),
                            connector_type: type_str.clone(),
                            msl_path: resolved_path,
                            is_flow: is_port,
                            x,
                            y,
                            size_x,
                            size_y,
                            rotation_deg,
                        });
                    }
                }
            }
        }
    }


    fn index_all(&self) -> Vec<MSLComponentDef> {
        use std::sync::Arc;
        let mut all_comps = Vec::new();

        for (full_name, class) in &self.classes {
            if matches!(
                class.class_type,
                ClassType::Model | ClassType::Block | ClassType::Connector
            ) {
                let mut ports = Vec::new();
                let mut parameters = Vec::new();
                let mut visited = HashSet::new();

                self.resolve_inheritance(full_name, &mut ports, &mut parameters, &mut visited);

                let short_name = full_name.rsplit('.').next().unwrap_or(full_name).to_string();
                let category = full_name.rsplitn(2, '.').nth(1).unwrap_or("").replace('.', "/");

                // Walk the `extends` chain via the typed extractor so
                // inherited icons (PartialValve → ValveCompressible)
                // are merged into one `Icon` graphics list. Resolver
                // searches the indexer's full-qualified-name map first,
                // then falls back to a leaf-name suffix scan to handle
                // bare `extends Foo` where `Foo` lives in the same
                // package (MLS §5 scope-chain — `extract_icon_inherited`
                // builds the candidate list, we just resolve names).
                let resolver_classes = &self.classes;
                let mut resolver = |name: &str| -> Option<Arc<ClassDef>> {
                    if let Some(c) = resolver_classes.get(name) {
                        return Some(Arc::new(c.clone()));
                    }
                    let leaf = name.rsplit('.').next().unwrap_or(name);
                    let suffix = format!(".{leaf}");
                    resolver_classes
                        .iter()
                        .find(|(k, _)| k.ends_with(&suffix) || k.as_str() == leaf)
                        .map(|(_, v)| Arc::new(v.clone()))
                };
                let mut icon_visited = HashSet::new();
                let icon_graphics = lunco_modelica::annotations::extract_icon_inherited(
                    full_name,
                    class,
                    &mut resolver,
                    &mut icon_visited,
                );
                // Diagram annotation — used when a connector instance
                // is rendered on a parent's diagram (carries the
                // `%name` Text label and the larger filled triangle
                // graphic that MSL signal connectors use only in the
                // diagram view, not as port markers).
                let diagram_graphics = lunco_modelica::annotations::extract_diagram(
                    &class.annotation,
                );

                // Tiny `%name` / `textString="..."` extraction kept
                // for the palette text fallback when a class has no
                // graphics primitives at all.
                let ann_str = format!("{:?}", class.annotation);
                let mut icon_text = None;
                if let Some(caps) = regex::Regex::new("textString=\"([^\"]+)\"").unwrap().captures(&ann_str) {
                    icon_text = Some(caps.get(1).unwrap().as_str().to_string());
                }

                let short_description = clean_short_description(&class.description);
                let documentation_info = self.doc_infos.get(&short_name).cloned();
                let is_example = full_name.contains(".Examples.");
                let domain = msl_domain(full_name);
                let class_kind = class_kind_str(&class.class_type).to_string();

                all_comps.push(MSLComponentDef {
                    name: short_name.clone(),
                    msl_path: full_name.clone(),
                    category,
                    display_name: format!("📦 {}", short_name),
                    // Legacy Debug-formatted field. Kept for any caller
                    // still reading `description`; new code should use
                    // `short_description` which carries the cleaned
                    // string.
                    description: Some(format!("{:?}", class.description)),
                    short_description,
                    documentation_info,
                    is_example,
                    domain,
                    class_kind,
                    icon_text,
                    icon_graphics,
                    diagram_graphics,
                    ports,
                    parameters,
                });
            }
        }
        all_comps
    }
}

fn main() {
    // Point rumoca at the same on-disk parse cache the workbench
    // uses (`<workspace>/.cache/rumoca`), so a run here warms the
    // cache for the app and vice versa. Same one-liner as
    // `ClassCachePlugin::build` — keeps all tooling cache under
    // one roof. Honors an explicit `RUMOCA_CACHE_DIR` the user set.
    if std::env::var_os("RUMOCA_CACHE_DIR").is_none() {
        let target = lunco_assets::cache_dir().join("rumoca");
        std::env::set_var("RUMOCA_CACHE_DIR", &target);
        println!("[indexer] using rumoca parse cache at {}", target.display());
    }

    let opts = Options::parse();

    let msl_root = lunco_assets::msl_dir();
    let msl_path = msl_root.join("Modelica");
    if !msl_path.exists() {
        println!("[indexer] MSL not found at {:?}", msl_path);
        return;
    }

    let t_total = Instant::now();
    println!("[indexer] scanning MSL at {:?} (verbose={})", msl_path, opts.verbose);

    let mut indexer = MSLIndexer::new();
    indexer.verbose = opts.verbose;
    indexer.scan_dir(&msl_path, "Modelica");
    // Top-level companion libraries that ship alongside `Modelica/` and
    // are required by it — `Complex.mo` is referenced by Modelica.Fluid
    // (medium models) and Modelica.ComplexBlocks; `ModelicaServices/`
    // carries device animation helpers that several MSL examples extend.
    // Without them, every model touching `Modelica.Fluid.*` fails
    // resolution with `base class not found: Complex does not exist`
    // even though the file is on disk — the in-memory bundle simply
    // never had it.
    //
    // Each entry is loaded independently (top-level package_prefix is
    // empty, so the indexer keys flat files by their declared class
    // name and folder packages by their package.mo `within ;` shape).
    for sibling_dir in ["ModelicaServices"] {
        let p = msl_root.join(sibling_dir);
        if p.exists() {
            indexer.scan_dir(&p, sibling_dir);
        } else {
            println!("[indexer] (skipping absent companion `{}`)", sibling_dir);
        }
    }
    for sibling_file in ["Complex.mo"] {
        let p = msl_root.join(sibling_file);
        if p.exists() {
            // scan_dir handles the file branch when called against its
            // parent dir, but here we want a single file. The cleanest
            // re-use is to scan the parent with an empty prefix — but
            // that pulls in *every* root file. Inline the file branch
            // instead.
            if let Ok(source) = fs::read_to_string(&p) {
                indexer.files_scanned += 1;
                indexer.bytes_scanned += source.len();
                indexer.ingest_root_file(&p, &source);
            }
        } else {
            println!("[indexer] (skipping absent companion `{}`)", sibling_file);
        }
    }

    // Additional Modelica libraries downloaded via lunco-assets and
    // surfaced into the workbench alongside the MSL. Each entry is
    // (cache_subdir, top_level_package_dir_inside_it). The cache
    // subdir is what `lunco-assets` writes (e.g. `dest = "thermofluidstream"`
    // in Assets.toml lands the unpacked archive at
    // `<cache>/thermofluidstream/`), and the inner directory is the
    // actual Modelica package root (the GitHub archive layout puts
    // the library one level down).
    //
    // Adding a library here makes its classes visible in the
    // workbench's package browser, drillable from the Twin tree, and
    // resolvable as `Library.Class` from any open document.
    let extra_libraries: &[(&str, &str)] = &[
        ("thermofluidstream", "ThermofluidStream"),
    ];
    for (cache_subdir, package_dir) in extra_libraries {
        let cache_root = lunco_assets::cache_dir().join(cache_subdir);
        let lib_path = cache_root.join(package_dir);
        if lib_path.exists() {
            println!("[indexer] scanning `{}` at {:?}", package_dir, lib_path);
            indexer.scan_dir(&lib_path, package_dir);
        } else {
            println!(
                "[indexer] (skipping `{}` — run `cargo run -p lunco-assets --bin lunco-assets -- download` to fetch)",
                package_dir
            );
        }
    }

    let scan_secs = indexer
        .scan_started
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    let scan_mb = indexer.bytes_scanned as f64 / (1024.0 * 1024.0);
    println!(
        "[indexer] scan done: {} files, {:.1} MB in {:.1}s",
        indexer.files_scanned, scan_mb, scan_secs,
    );

    println!("[indexer] indexing components (resolving inheritance)...");
    let t_index = Instant::now();
    let components = indexer.index_all();
    println!(
        "[indexer] index done: {} components in {:.1}s",
        components.len(),
        t_index.elapsed().as_secs_f64()
    );

    // Bundled examples — small `.mo` files compiled into the workbench
    // binary at runtime via `include_dir!()`. Pre-parse their class
    // hierarchy here so the Package Browser can render them with
    // proper kind badges and expandable inner classes (matches MSL /
    // workspace docs) without paying any parse cost at startup.
    let bundled_trees = scan_bundled_examples();
    println!(
        "[indexer] bundled examples indexed: {} files",
        bundled_trees.len()
    );

    // Local `MslIndex` mirror — wire-compatible with
    // `lunco_modelica::visual_diagram::MslIndex` (serde structurally
    // matches), but built around the indexer's local
    // `MSLComponentDef` so we don't need to share the type across
    // crates. The runtime reader on the other side uses the
    // canonical `MslIndex` and accepts both shapes.
    #[derive(Serialize)]
    struct LocalMslIndex<'a> {
        components: &'a [MSLComponentDef],
        bundled: &'a [lunco_modelica::visual_diagram::BundledFileTree],
    }
    let output_path = lunco_assets::msl_dir().join("msl_index.json");
    let index = LocalMslIndex {
        components: &components,
        bundled: &bundled_trees,
    };
    let json = serde_json::to_string_pretty(&index).unwrap();
    fs::write(&output_path, json).unwrap();
    println!(
        "[indexer] wrote {} components + {} bundled trees → {}",
        components.len(),
        bundled_trees.len(),
        output_path.display()
    );

    // Pre-parsed bundle for the workbench's fast path. Native mirror
    // of the wasm `parsed-*.bin.zst` artifact: bincode-serialised
    // `Vec<(uri, StoredDefinition)>` that the workbench installs
    // directly via `Session::replace_parsed_source_set`, bypassing
    // every per-file cache key concern.
    let bundle_path = lunco_assets::msl_dir().join("parsed-msl.bin");
    let t_bundle = Instant::now();
    match bincode::serialize(&indexer.parsed_bundle) {
        Ok(bytes) => match fs::write(&bundle_path, &bytes) {
            Ok(()) => println!(
                "[indexer] wrote parsed bundle: {} docs, {:.1} MB in {:.1}s → {}",
                indexer.parsed_bundle.len(),
                bytes.len() as f64 / (1024.0 * 1024.0),
                t_bundle.elapsed().as_secs_f64(),
                bundle_path.display()
            ),
            Err(e) => eprintln!(
                "[indexer] WARN: failed to write parsed bundle to {}: {e}",
                bundle_path.display()
            ),
        },
        Err(e) => eprintln!("[indexer] WARN: failed to serialise parsed bundle: {e}"),
    }

    if opts.warm {
        println!();
        warm_compile_pass(&opts);
    }

    println!();
    println!(
        "[indexer] all done in {:.1}s",
        t_total.elapsed().as_secs_f64()
    );
}

/// Parse every bundled `.mo` (compiled into `lunco_modelica` via
/// `include_dir!`) and produce a [`BundledFileTree`] for each. The
/// runtime Package Browser consumes the result so multi-class
/// bundled files (`AnnotatedRocketStage.mo`'s package + nested
/// models / connectors) render with the same shape MSL files get.
///
/// Pure function over the in-memory `bundled_models()` list — no
/// disk I/O beyond what `include_dir!` already inlined at compile
/// time, so the cost is `n * parse(file)`, ≤ ~10 small files.
fn scan_bundled_examples() -> Vec<lunco_modelica::visual_diagram::BundledFileTree> {
    use lunco_modelica::models::bundled_models;
    use lunco_modelica::visual_diagram::{BundledClassTree, BundledFileTree};

    // `parse_to_syntax(...).best_effort()` is the same path
    // `SyntaxCache::from_source` uses and is what the workspace
    // browser already renders cleanly — it preserves the full
    // nested class list, including `partial connector` siblings
    // that the bare `parse_to_recovered_ast` recovery parser
    // truncates after the first error-ish token.
    bundled_models()
        .into_iter()
        .filter_map(|m| {
            let syntax = rumoca_phase_parse::parse_to_syntax(m.source, m.filename);
            let ast = syntax.best_effort();
            let (top_short, top_class) = ast.classes.iter().next()?;
            Some(BundledFileTree {
                filename: m.filename.to_string(),
                top: bundled_class_tree(top_short, top_class, ""),
            })
        })
        .collect()
}

fn bundled_class_tree(
    short_name: &str,
    class_def: &ClassDef,
    parent_path: &str,
) -> lunco_modelica::visual_diagram::BundledClassTree {
    use lunco_modelica::visual_diagram::BundledClassTree;
    let qualified = if parent_path.is_empty() {
        short_name.to_string()
    } else {
        format!("{parent_path}.{short_name}")
    };
    let children = class_def
        .classes
        .iter()
        .map(|(child_short, child_def)| {
            bundled_class_tree(child_short, child_def, &qualified)
        })
        .collect();
    BundledClassTree {
        short_name: short_name.to_string(),
        qualified_path: qualified,
        class_kind: bundled_class_kind(&class_def.class_type).to_string(),
        description: class_def
            .description
            .iter()
            .next()
            .map(|t| t.text.as_ref().trim_matches('"').to_string())
            .filter(|s| !s.is_empty()),
        children,
    }
}

fn bundled_class_kind(kind: &ClassType) -> &'static str {
    match kind {
        ClassType::Model => "model",
        ClassType::Block => "block",
        ClassType::Connector => "connector",
        ClassType::Function => "function",
        ClassType::Record => "record",
        ClassType::Type => "type",
        ClassType::Package => "package",
        ClassType::Class => "class",
        ClassType::Operator => "operator",
    }
}

/// Drive a full rumoca compile of every requested model so that
/// rumoca's semantic-summary cache (under `<cache>/rumoca/source-roots/
/// semantic-summaries/`) is populated. The workbench's first compile
/// of the same model is then a cache hit (ms instead of minutes).
///
/// Sources to compile come from three places, in priority order:
///   1. `--warm-only NAME[,NAME...]` — explicit qualified names or .mo
///      file paths. Anything containing `/`, `\`, or ending in `.mo`
///      is treated as a path; everything else as an MSL qualified name.
///   2. `LUNCOSIM_WARM_DIRS` env var — `:`-separated list of directories
///      to scan for `*.mo` files. Every top-level model in each file
///      is warmed under its `<file_stem_or_package>.<model_name>`
///      qualified path.
///   3. If neither (1) nor (2) yielded anything: the built-in
///      [`DEFAULT_WARM_EXAMPLES`] list of common MSL examples.
///
/// Each compile is gated by [`ModelicaCompiler::compile_loaded`]'s
/// existing 5-second heartbeat (see lib.rs), so even a multi-minute
/// MSL-heavy compile prints proof-of-life every 5s.
fn warm_compile_pass(opts: &Options) {
    println!("[warm] starting compile pass — populating rumoca semantic-summary cache");
    let t_total = Instant::now();

    let mut compiler = lunco_modelica::ModelicaCompiler::new();

    // Resolve work units. Each entry: (display_label, kind). The
    // `WarmKind` enum is declared at module scope so `push_file_units`
    // can refer to it.
    let mut units: Vec<(String, WarmKind)> = Vec::new();

    // (1) --warm-only — mixed paths and qualified names.
    if let Some(list) = &opts.warm_only {
        for item in list {
            if item.contains('/') || item.contains('\\') || item.ends_with(".mo") {
                push_file_units(&std::path::PathBuf::from(item), &mut units);
            } else {
                units.push((item.clone(), WarmKind::MslClass(item.clone())));
            }
        }
    }

    // (2) LUNCOSIM_WARM_DIRS — scan dirs for .mo files.
    if let Some(dirs) = std::env::var_os("LUNCOSIM_WARM_DIRS") {
        let dirs = dirs.to_string_lossy().to_string();
        for dir in dirs.split(':').filter(|s| !s.is_empty()) {
            let path = std::path::PathBuf::from(dir);
            if !path.exists() {
                eprintln!("[warm] LUNCOSIM_WARM_DIRS entry does not exist: {}", path.display());
                continue;
            }
            if path.is_file() {
                push_file_units(&path, &mut units);
            } else if path.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&path) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.extension().map_or(false, |e| e == "mo") {
                            push_file_units(&p, &mut units);
                        }
                    }
                }
            }
        }
    }

    // (3) Default fallback — common MSL examples.
    if units.is_empty() {
        for ex in DEFAULT_WARM_EXAMPLES {
            units.push((ex.to_string(), WarmKind::MslClass(ex.to_string())));
        }
    }

    println!("[warm] {} units to compile", units.len());
    for (i, (label, _)) in units.iter().enumerate() {
        println!("[warm]   {}/{}: {}", i + 1, units.len(), label);
    }
    println!();

    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut total_compile_secs = 0.0f64;

    for (i, (label, kind)) in units.iter().enumerate() {
        println!("[warm] [{}/{}] compiling {} ...", i + 1, units.len(), label);
        let t = Instant::now();
        let result = match kind {
            WarmKind::MslClass(qn) => compiler.compile_msl_class(qn),
            WarmKind::FileWithSource {
                qualified,
                source,
                filename,
            } => compiler.compile_str(qualified, source, filename),
        };
        let secs = t.elapsed().as_secs_f64();
        total_compile_secs += secs;
        match result {
            Ok(_) => {
                println!("[warm] [{}/{}] ✓ {} compiled in {:.1}s", i + 1, units.len(), label, secs);
                succeeded += 1;
            }
            Err(e) => {
                let msg: String = e.chars().take(200).collect();
                println!(
                    "[warm] [{}/{}] ✗ {} FAILED in {:.1}s: {}",
                    i + 1,
                    units.len(),
                    label,
                    secs,
                    msg
                );
                failed += 1;
            }
        }
    }

    println!();
    println!(
        "[warm] done: {} succeeded, {} failed, total compile {:.1}s, wall {:.1}s",
        succeeded,
        failed,
        total_compile_secs,
        t_total.elapsed().as_secs_f64(),
    );
}

/// Read a `.mo` file and emit one warm unit per top-level class found
/// in it (model / block / package contents). Uses the lenient parser
/// so syntactically-broken files still surface what they can.
///
/// `qualified` follows MLS scoping: a top-level `model Foo` produces
/// `Foo`; a `package Foo { model Bar }` produces `Foo.Bar`.
fn push_file_units(
    path: &std::path::Path,
    units: &mut Vec<(String, WarmKind)>,
) {
    let Ok(source) = std::fs::read_to_string(path) else {
        eprintln!("[warm] read failed: {}", path.display());
        return;
    };
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("model.mo")
        .to_string();
    // Lenient parse to discover top-level classes. Errors don't kill
    // the warm — we still emit any classes the parser could salvage.
    let syntax = rumoca_phase_parse::parse_to_syntax(&source, &filename);
    let ast = syntax.best_effort();
    let mut emitted = 0;
    for (top_name, class_def) in &ast.classes {
        // If the top class is a package, descend one level so we warm
        // the actual models the user runs (the package itself isn't
        // simulable). One level is enough for the bundled assets;
        // deeper nesting would need recursion.
        if matches!(class_def.class_type, ClassType::Package) {
            for (inner_name, _) in &class_def.classes {
                let qualified = format!("{}.{}", top_name, inner_name);
                let label = format!("{} ({})", qualified, path.display());
                units.push((
                    label,
                    WarmKind::FileWithSource {
                        qualified,
                        source: source.clone(),
                        // Match what the workbench passes to
                        // compile_str so cache keys align: the
                        // workbench uses the literal "model.mo".
                        filename: "model.mo".to_string(),
                    },
                ));
                emitted += 1;
            }
        } else {
            let qualified = top_name.clone();
            let label = format!("{} ({})", qualified, path.display());
            units.push((
                label,
                WarmKind::FileWithSource {
                    qualified,
                    source: source.clone(),
                    filename: "model.mo".to_string(),
                },
            ));
            emitted += 1;
        }
    }
    if emitted == 0 {
        eprintln!(
            "[warm] no compilable classes found in {} (parse errors?)",
            path.display()
        );
    }
}

enum WarmKind {
    MslClass(String),
    FileWithSource {
        qualified: String,
        source: String,
        filename: String,
    },
}
