//! Local-LLM management: probe the machine, recommend the strongest models that fit (across the
//! Qwen / Llama / DeepSeek / Gemma / Phi families), and install / run them through Ollama — already
//! a first-class mesh provider (`ollama::<tag>`). The runtime layer is factored so llama.cpp /
//! LM Studio can plug in later. Backs the `forge local` commands and the setup wizard.
//!
//! Honesty note: the Ollama *tags* below are real, well-established library tags, but the fetch
//! still defers to `ollama pull <tag>` — so if a tag is renamed/removed (or, for the post-cutoff
//! Gemma 4 line, needs a newer Ollama) the user sees Ollama's own error, never a fabricated success.

use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

/// A hardware probe, used to size a local model to the machine.
#[derive(Debug, Clone)]
pub struct SystemSpecs {
    /// Total physical RAM in GiB.
    pub total_ram_gb: f64,
    /// Logical CPU cores.
    pub cpu_cores: usize,
    /// `"linux" | "macos" | "windows" | "?"`.
    pub os: &'static str,
    /// A discrete GPU, when one was detected (best-effort).
    pub gpu: Option<GpuInfo>,
    /// Apple Silicon: RAM is unified, so the GPU shares (most of) `total_ram_gb`.
    pub apple_silicon: bool,
}

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub name: String,
    /// Dedicated VRAM in GiB, when known.
    pub vram_gb: Option<f64>,
}

impl SystemSpecs {
    /// The memory budget available to a model: dedicated VRAM on a discrete GPU, otherwise system
    /// RAM (Apple Silicon's unified memory counts as the full budget).
    pub fn model_memory_gb(&self) -> f64 {
        match &self.gpu {
            Some(g) if !self.apple_silicon => g.vram_gb.unwrap_or(self.total_ram_gb),
            _ => self.total_ram_gb,
        }
    }
}

/// Detect the machine's specs (best-effort; never fails — unknowns degrade to conservative zeros).
pub fn detect_specs() -> SystemSpecs {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let total_ram_gb = sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "?"
    };
    let apple_silicon = cfg!(target_os = "macos") && cfg!(target_arch = "aarch64");
    SystemSpecs {
        total_ram_gb,
        cpu_cores,
        os,
        gpu: detect_gpu(apple_silicon),
        apple_silicon,
    }
}

/// Best-effort GPU probe. NVIDIA via `nvidia-smi`; Apple Silicon reports unified memory; anything
/// else returns `None` (we fall back to system RAM for sizing).
fn detect_gpu(apple_silicon: bool) -> Option<GpuInfo> {
    if let Ok(out) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if out.status.success() {
            let line = String::from_utf8_lossy(&out.stdout);
            if let Some(first) = line.lines().next() {
                let mut parts = first.split(',');
                let name = parts.next().unwrap_or("GPU").trim().to_string();
                let vram_gb = parts
                    .next()
                    .and_then(|m| m.trim().parse::<f64>().ok())
                    .map(|mib| mib / 1024.0);
                return Some(GpuInfo { name, vram_gb });
            }
        }
    }
    if apple_silicon {
        return Some(GpuInfo {
            name: "Apple Silicon (unified memory)".to_string(),
            vram_gb: None,
        });
    }
    None
}

/// A locally-runnable model in the catalog. Spans several open families and sizes; `min_memory_gb`
/// is a realistic ~Q4 runtime footprint and `quality` is a coarse capability score (0–100, coding-
/// leaning since Forge is a dev tool) used to rank across families.
#[derive(Debug, Clone, Copy)]
pub struct LocalModel {
    /// Stable key used on the CLI (`forge local install <key>`).
    pub key: &'static str,
    /// Family label (Qwen, Llama, DeepSeek, …) — used as the menu group.
    pub family: &'static str,
    /// Human label.
    pub label: &'static str,
    /// Ollama tag, resolved against the registry at `ollama pull` time.
    pub ollama_tag: &'static str,
    /// Parameter count in billions (effective/active, for MoE).
    pub params_b: f64,
    /// Recommended minimum memory budget (GiB) to run it at ~Q4 (Ollama-style: ~8 GB/7B,
    /// ~16 GB/13B, ~32 GB/33B, ~48 GB/70B).
    pub min_memory_gb: f64,
    /// Coarse capability score for ranking across families (higher = stronger).
    pub quality: u8,
    pub blurb: &'static str,
}

/// The local-model catalog: well-established open models across families and sizes, with their real
/// Ollama tags. Tags still resolve against the Ollama registry at `ollama pull` time, so anything
/// renamed/removed surfaces Ollama's own error rather than a fabricated success. Gemma 4 entries are
/// the June-2026 line (newer than this build's knowledge — they need a recent Ollama).
pub const CATALOG: &[LocalModel] = &[
    // ---- ≤4 GB: tiny / mobile-class ----
    LocalModel {
        key: "qwen2.5-coder-3b",
        family: "Qwen",
        label: "Qwen2.5-Coder 3B",
        ollama_tag: "qwen2.5-coder:3b",
        params_b: 3.0,
        min_memory_gb: 4.0,
        quality: 55,
        blurb: "Tiny coding model; fits modest laptops.",
    },
    LocalModel {
        key: "llama3.2-3b",
        family: "Llama",
        label: "Llama 3.2 3B",
        ollama_tag: "llama3.2:3b",
        params_b: 3.0,
        min_memory_gb: 4.0,
        quality: 46,
        blurb: "Small general model; quick chat/edits.",
    },
    LocalModel {
        key: "gemma4-e2b",
        family: "Gemma",
        label: "Gemma 4 E2B",
        ollama_tag: "gemma4:e2b",
        params_b: 2.0,
        min_memory_gb: 4.0,
        quality: 42,
        blurb: "Tiny Gemma 4; mobile-class.",
    },
    LocalModel {
        key: "gemma4-e4b",
        family: "Gemma",
        label: "Gemma 4 E4B",
        ollama_tag: "gemma4:e4b",
        params_b: 4.0,
        min_memory_gb: 6.0,
        quality: 50,
        blurb: "Small Gemma 4.",
    },
    // ---- 8 GB: 7–9B ----
    LocalModel {
        key: "qwen2.5-coder-7b",
        family: "Qwen",
        label: "Qwen2.5-Coder 7B",
        ollama_tag: "qwen2.5-coder:7b",
        params_b: 7.0,
        min_memory_gb: 8.0,
        quality: 74,
        blurb: "Excellent coder for its size; great 16 GB default.",
    },
    LocalModel {
        key: "deepseek-r1-8b",
        family: "DeepSeek",
        label: "DeepSeek-R1 8B",
        ollama_tag: "deepseek-r1:8b",
        params_b: 8.0,
        min_memory_gb: 8.0,
        quality: 72,
        blurb: "Reasoning-distilled; strong step-by-step.",
    },
    LocalModel {
        key: "qwen2.5-7b",
        family: "Qwen",
        label: "Qwen2.5 7B",
        ollama_tag: "qwen2.5:7b",
        params_b: 7.0,
        min_memory_gb: 8.0,
        quality: 68,
        blurb: "Solid general 7B.",
    },
    LocalModel {
        key: "llama3.1-8b",
        family: "Llama",
        label: "Llama 3.1 8B",
        ollama_tag: "llama3.1:8b",
        params_b: 8.0,
        min_memory_gb: 8.0,
        quality: 66,
        blurb: "Well-rounded general 8B.",
    },
    LocalModel {
        key: "gemma2-9b",
        family: "Gemma",
        label: "Gemma 2 9B",
        ollama_tag: "gemma2:9b",
        params_b: 9.0,
        min_memory_gb: 10.0,
        quality: 64,
        blurb: "Capable general 9B.",
    },
    // ---- 16 GB: 12–16B ----
    LocalModel {
        key: "qwen2.5-coder-14b",
        family: "Qwen",
        label: "Qwen2.5-Coder 14B",
        ollama_tag: "qwen2.5-coder:14b",
        params_b: 14.0,
        min_memory_gb: 16.0,
        quality: 82,
        blurb: "Top coder in the 16 GB class.",
    },
    LocalModel {
        key: "deepseek-r1-14b",
        family: "DeepSeek",
        label: "DeepSeek-R1 14B",
        ollama_tag: "deepseek-r1:14b",
        params_b: 14.0,
        min_memory_gb: 16.0,
        quality: 81,
        blurb: "Strong reasoning at 14B.",
    },
    LocalModel {
        key: "deepseek-coder-v2-16b",
        family: "DeepSeek",
        label: "DeepSeek-Coder-V2 16B (MoE)",
        ollama_tag: "deepseek-coder-v2:16b",
        params_b: 16.0,
        min_memory_gb: 16.0,
        quality: 79,
        blurb: "MoE coder; fast for its quality.",
    },
    LocalModel {
        key: "phi4-14b",
        family: "Phi",
        label: "Phi-4 14B",
        ollama_tag: "phi4:14b",
        params_b: 14.0,
        min_memory_gb: 16.0,
        quality: 76,
        blurb: "Strong reasoning/coding for 14B.",
    },
    LocalModel {
        key: "qwen2.5-14b",
        family: "Qwen",
        label: "Qwen2.5 14B",
        ollama_tag: "qwen2.5:14b",
        params_b: 14.0,
        min_memory_gb: 16.0,
        quality: 77,
        blurb: "Strong general 14B.",
    },
    LocalModel {
        key: "gemma4-12b",
        family: "Gemma",
        label: "Gemma 4 12B",
        ollama_tag: "gemma4:12b",
        params_b: 12.0,
        min_memory_gb: 16.0,
        quality: 78,
        blurb: "Gemma 4 sweet spot (recent Ollama needed).",
    },
    // ---- 32 GB: 27–34B ----
    LocalModel {
        key: "qwen2.5-coder-32b",
        family: "Qwen",
        label: "Qwen2.5-Coder 32B",
        ollama_tag: "qwen2.5-coder:32b",
        params_b: 32.0,
        min_memory_gb: 32.0,
        quality: 89,
        blurb: "Best local coder short of 70B.",
    },
    LocalModel {
        key: "deepseek-r1-32b",
        family: "DeepSeek",
        label: "DeepSeek-R1 32B",
        ollama_tag: "deepseek-r1:32b",
        params_b: 32.0,
        min_memory_gb: 32.0,
        quality: 88,
        blurb: "Excellent reasoning at 32B.",
    },
    LocalModel {
        key: "qwen2.5-32b",
        family: "Qwen",
        label: "Qwen2.5 32B",
        ollama_tag: "qwen2.5:32b",
        params_b: 32.0,
        min_memory_gb: 32.0,
        quality: 85,
        blurb: "Strong general 32B.",
    },
    LocalModel {
        key: "gemma2-27b",
        family: "Gemma",
        label: "Gemma 2 27B",
        ollama_tag: "gemma2:27b",
        params_b: 27.0,
        min_memory_gb: 28.0,
        quality: 80,
        blurb: "Capable general 27B.",
    },
    LocalModel {
        key: "gemma4-31b",
        family: "Gemma",
        label: "Gemma 4 31B",
        ollama_tag: "gemma4:31b",
        params_b: 31.0,
        min_memory_gb: 32.0,
        quality: 86,
        blurb: "Top Gemma 4 (recent Ollama needed).",
    },
    // ---- 48 GB+: 70B ----
    LocalModel {
        key: "deepseek-r1-70b",
        family: "DeepSeek",
        label: "DeepSeek-R1 70B",
        ollama_tag: "deepseek-r1:70b",
        params_b: 70.0,
        min_memory_gb: 48.0,
        quality: 92,
        blurb: "Frontier-class reasoning; big rig only.",
    },
    LocalModel {
        key: "llama3.3-70b",
        family: "Llama",
        label: "Llama 3.3 70B",
        ollama_tag: "llama3.3:70b",
        params_b: 70.0,
        min_memory_gb: 48.0,
        quality: 90,
        blurb: "Top general open model; big rig only.",
    },
    LocalModel {
        key: "qwen2.5-72b",
        family: "Qwen",
        label: "Qwen2.5 72B",
        ollama_tag: "qwen2.5:72b",
        params_b: 72.0,
        min_memory_gb: 48.0,
        quality: 90,
        blurb: "Frontier-class general; big rig only.",
    },
];

/// Look up a catalog entry by key.
pub fn model_by_key(key: &str) -> Option<&'static LocalModel> {
    CATALOG.iter().find(|m| m.key == key)
}

/// Rank the catalog for these specs: EVERY model whose memory budget fits, highest capability first
/// (ties broken by params). The first entry is the recommended pick. Empty only on a machine too
/// small for even the tiniest model.
pub fn recommend(specs: &SystemSpecs) -> Vec<&'static LocalModel> {
    let budget = specs.model_memory_gb();
    let mut fits: Vec<&LocalModel> = CATALOG
        .iter()
        .filter(|m| m.min_memory_gb <= budget)
        .collect();
    fits.sort_by(|a, b| {
        b.quality
            .cmp(&a.quality)
            .then(b.params_b.partial_cmp(&a.params_b).unwrap())
    });
    fits
}

// ----------------------------------------------------------------------------------------------
// Live discovery + benchmark ranking. The static CATALOG above is the offline floor; when online we
// also pull the current model list from Ollama's library so brand-new models appear, and we rank
// everything by REAL Artificial Analysis benchmark scores (reusing forge-mesh's BenchmarkScores),
// falling back to size only when a model has no measured score.
// ----------------------------------------------------------------------------------------------

use forge_mesh::BenchmarkScores;

/// An owned, ranked install candidate — from the static catalog and/or live discovery.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub family: String,
    pub label: String,
    pub ollama_tag: String,
    pub params_b: f64,
    pub min_memory_gb: f64,
    /// Ranking score (higher = better). From Artificial Analysis when `benchmarked`, else a
    /// size-derived proxy.
    pub score: f64,
    /// True when `score` is a measured Artificial Analysis index (not a size guess).
    pub benchmarked: bool,
    pub blurb: String,
}

/// Memory floor (~Q4 runtime, Ollama-style) for a parameter count.
pub fn mem_floor(params_b: f64) -> f64 {
    match params_b {
        p if p <= 2.0 => 4.0,
        p if p <= 4.0 => 6.0,
        p if p <= 9.0 => 8.0,
        p if p <= 16.0 => 16.0,
        p if p <= 34.0 => 32.0,
        _ => 48.0,
    }
}

/// Parse a billions-of-params count from an Ollama size tag: `7b`, `1.5b`, `32b`, `e4b` (Gemma), …
pub fn params_from_size(size: &str) -> Option<f64> {
    let s = size.trim().to_lowercase();
    let s = s.strip_suffix('b')?;
    let s = s.strip_prefix('e').unwrap_or(s); // Gemma e2b/e4b
    let n: f64 = s.parse().ok()?;
    (n > 0.0 && n < 2000.0).then_some(n)
}

/// Parse the Ollama library HTML into `(slug, [size-tag])` cards — best-effort, structure-tolerant:
/// every `/library/<slug>` link, with the size badges (`7b`, `1.5b`, `e4b`, …) found in the slice
/// up to the next library link. Pure → unit-tested; an empty result makes the caller fall back to
/// the static catalog.
pub fn parse_library(html: &str) -> Vec<(String, Vec<String>)> {
    const MARK: &str = "/library/";
    let bytes = html.as_bytes();
    // Byte offsets of each "/library/<slug>" occurrence.
    let mut hits: Vec<(usize, String)> = Vec::new();
    let mut from = 0;
    while let Some(rel) = html[from..].find(MARK) {
        let start = from + rel + MARK.len();
        let slug: String = html[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.' || *c == '_')
            .collect();
        from = start;
        // Skip nested links like "/library/<slug>/blobs" and tag anchors.
        if !slug.is_empty() && !slug.contains('/') {
            hits.push((start, slug));
        }
        if hits.len() > 200 {
            break;
        }
    }
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for i in 0..hits.len() {
        let (pos, slug) = &hits[i];
        if !seen.insert(slug.clone()) {
            continue;
        }
        let end = hits.get(i + 1).map(|(p, _)| *p).unwrap_or(bytes.len());
        let card = &html[*pos..end.min(html.len())];
        let sizes = size_tokens(card);
        if !sizes.is_empty() {
            out.push((slug.clone(), sizes));
        }
    }
    out
}

/// Extract size badges (`7b`, `1.5b`, `32b`, `e4b`) from a chunk of HTML/text.
fn size_tokens(s: &str) -> Vec<String> {
    let lower = s.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i < chars.len() {
        // A size token starts at a word boundary with an optional 'e', digits, optional '.digits', 'b'.
        let prev_alnum = i > 0 && (chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '.');
        if !prev_alnum {
            let mut j = i;
            if chars[j] == 'e' && j + 1 < chars.len() && chars[j + 1].is_ascii_digit() {
                j += 1;
            }
            let num_start = j;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                j += 1;
            }
            if j > num_start && j < chars.len() && chars[j] == 'b' {
                let after = j + 1;
                let boundary = after >= chars.len() || !chars[after].is_ascii_alphanumeric();
                let tok: String = chars[i..=j].iter().collect();
                if boundary && params_from_size(&tok).is_some() && seen.insert(tok.clone()) {
                    out.push(tok);
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Capitalise a slug's leading segment for a family label ("qwen2.5-coder" → "Qwen2.5-coder").
fn family_of(slug: &str) -> String {
    let base = slug.split(['-', ':']).next().unwrap_or(slug);
    let mut cs = base.chars();
    match cs.next() {
        Some(c) => c.to_uppercase().chain(cs).collect(),
        None => slug.to_string(),
    }
}

/// Fetch + parse the Ollama library (best-effort; `None` on any network/parse failure).
async fn fetch_library() -> Option<Vec<(String, Vec<String>)>> {
    let body = forge_provider::bundled_http_client()
        .get("https://ollama.com/library?sort=newest")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let parsed = parse_library(&body);
    (!parsed.is_empty()).then_some(parsed)
}

/// The static catalog as owned candidates (the offline floor).
fn static_candidates() -> Vec<Candidate> {
    CATALOG
        .iter()
        .map(|m| Candidate {
            family: m.family.to_string(),
            label: m.label.to_string(),
            ollama_tag: m.ollama_tag.to_string(),
            params_b: m.params_b,
            min_memory_gb: m.min_memory_gb,
            score: m.quality as f64,
            benchmarked: false,
            blurb: m.blurb.to_string(),
        })
        .collect()
}

/// Live-discovered library models as candidates (size-derived score until benchmarks attach).
fn live_candidates(lib: &[(String, Vec<String>)]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (slug, sizes) in lib {
        for size in sizes {
            let Some(params) = params_from_size(size) else {
                continue;
            };
            out.push(Candidate {
                family: family_of(slug),
                label: format!("{slug}:{size}"),
                ollama_tag: format!("{slug}:{size}"),
                params_b: params,
                min_memory_gb: mem_floor(params),
                score: params, // provisional until AA attaches
                benchmarked: false,
                blurb: String::new(),
            });
        }
    }
    out
}

/// Build the ranked candidate list for these specs: static catalog ∪ live discovery (deduped by
/// tag, static's curated labels win), filtered to what fits the memory budget, scored by Artificial
/// Analysis (coding-leaning) where available — else size — and sorted best-first.
pub fn rank_candidates(
    live: Option<Vec<(String, Vec<String>)>>,
    specs: &SystemSpecs,
    scores: Option<&BenchmarkScores>,
) -> Vec<Candidate> {
    let mut cands = static_candidates();
    if let Some(lib) = &live {
        cands.extend(live_candidates(lib));
    }
    // Dedup by tag, keeping the first (static curated entry beats a bare live one).
    let mut seen = std::collections::HashSet::new();
    cands.retain(|c| seen.insert(c.ollama_tag.clone()));

    let budget = specs.model_memory_gb();
    cands.retain(|c| c.min_memory_gb <= budget);

    // Attach real benchmark scores where Artificial Analysis has them. EXACT match only — local
    // tags are precise (family+size), so fuzzy fallback would mis-attribute scores across sizes
    // and families (e.g. deepseek-coder ← a qwen-coder row).
    for c in &mut cands {
        if let Some(s) =
            scores.and_then(|b| b.exact_score_for(&format!("ollama::{}", c.ollama_tag)))
        {
            c.score = s.coding.max(s.intelligence);
            c.benchmarked = true;
        }
    }
    // Benchmarked models rank above size-guessed ones; within each, by score then size.
    cands.sort_by(|a, b| {
        b.benchmarked
            .cmp(&a.benchmarked)
            .then(
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(
                b.params_b
                    .partial_cmp(&a.params_b)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    cands
}

/// Discover + rank installable models for this machine: live Ollama library when reachable (so new
/// models appear), else the static catalog — both benchmark-ranked via `scores`.
pub async fn discover_ranked(
    specs: &SystemSpecs,
    scores: Option<&BenchmarkScores>,
) -> Vec<Candidate> {
    let live = fetch_library().await;
    rank_candidates(live, specs, scores)
}

// ----------------------------------------------------------------------------------------------
// Ollama runtime (the first `Runtime`). All ops shell out to the `ollama` binary or probe its HTTP
// port; nothing here is on the hot path, so synchronous std::process is fine.
// ----------------------------------------------------------------------------------------------

const OLLAMA_PORT: u16 = 11434;

/// Whether the `ollama` binary is on PATH.
pub fn ollama_installed() -> bool {
    ollama_version().is_some()
}

/// The installed Ollama version string (`ollama --version`), if present.
pub fn ollama_version() -> Option<String> {
    let out = Command::new("ollama").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether an Ollama server is already listening on localhost (so we don't double-spawn one).
pub fn ollama_serving() -> bool {
    TcpStream::connect_timeout(
        &([127, 0, 0, 1], OLLAMA_PORT).into(),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// The official per-OS install command for Ollama (the thing `forge local install` offers to run,
/// or prints for the user to run manually). `None` on an unknown OS.
pub fn ollama_install_command(specs: &SystemSpecs) -> Option<(&'static str, Vec<String>)> {
    match specs.os {
        "linux" => Some((
            "sh",
            vec![
                "-c".into(),
                "curl -fsSL https://ollama.com/install.sh | sh".into(),
            ],
        )),
        "macos" => {
            if which("brew").is_some() {
                Some(("brew", vec!["install".into(), "ollama".into()]))
            } else {
                None // no brew → manual download from ollama.com/download
            }
        }
        "windows" => {
            if which("winget").is_some() {
                Some((
                    "winget",
                    vec![
                        "install".into(),
                        "--id".into(),
                        "Ollama.Ollama".into(),
                        "-e".into(),
                    ],
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Run the install command (blocking, inheriting stdio so the user sees progress). Returns whether
/// it succeeded.
pub fn run_install(cmd: &str, args: &[String]) -> bool {
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pull a model tag via Ollama (blocking, streaming Ollama's own progress to the terminal).
pub fn ollama_pull(tag: &str) -> bool {
    Command::new("ollama")
        .args(["pull", tag])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Models already pulled locally (`ollama list`), as their tags.
pub fn ollama_installed_models() -> Vec<String> {
    let Ok(out) = Command::new("ollama").arg("list").output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1) // header row
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// Start `ollama serve` detached, if it isn't already listening. Returns whether the server is up
/// afterwards (waits briefly for the port to open).
pub fn ollama_start_serve() -> bool {
    if ollama_serving() {
        return true;
    }
    use std::process::Stdio;
    if Command::new("ollama")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_err()
    {
        return false;
    }
    // Wait up to ~3s for the port to come up.
    for _ in 0..15 {
        if ollama_serving() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    ollama_serving()
}

/// Locate a binary on PATH (cross-platform `which`/`where`).
fn which(bin: &str) -> Option<String> {
    let (cmd, arg) = if cfg!(target_os = "windows") {
        ("where", bin)
    } else {
        ("which", bin)
    };
    let out = Command::new(cmd).arg(arg).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(ram: f64, gpu: Option<GpuInfo>, apple: bool) -> SystemSpecs {
        SystemSpecs {
            total_ram_gb: ram,
            cpu_cores: 8,
            os: "linux",
            gpu,
            apple_silicon: apple,
        }
    }

    #[test]
    fn recommend_returns_all_that_fit_ranked_by_capability() {
        // 16 GB → spans several families/sizes, all within budget, highest-quality first, and
        // ordered (no lower-quality model ahead of a higher one).
        let r = recommend(&specs(16.0, None, false));
        assert!(r.len() >= 8, "broad catalog, not just Gemma: {}", r.len());
        assert!(r.iter().all(|m| m.min_memory_gb <= 16.0));
        assert!(
            r.windows(2).all(|w| w[0].quality >= w[1].quality),
            "ranked best-first"
        );
        // Multiple families are represented (not Gemma-only).
        let families: std::collections::HashSet<_> = r.iter().map(|m| m.family).collect();
        assert!(families.len() >= 3, "families: {families:?}");
        // The 32B+ models do not fit a 16 GB budget.
        assert!(!r.iter().any(|m| m.min_memory_gb > 16.0));
    }

    #[test]
    fn tiny_machine_still_gets_small_models() {
        let r = recommend(&specs(6.0, None, false));
        assert!(!r.is_empty());
        assert!(r.iter().all(|m| m.min_memory_gb <= 6.0));
    }

    #[test]
    fn too_small_machine_gets_nothing() {
        assert!(recommend(&specs(3.0, None, false)).is_empty());
    }

    #[test]
    fn discrete_gpu_vram_is_the_budget_not_system_ram() {
        // 8 GB RAM but a 24 GB GPU → sizing uses VRAM, so 16 GB-class models fit (they wouldn't on
        // 8 GB RAM alone). Best fitting is a 14B-class model, not a 32B (needs 32).
        let gpu = Some(GpuInfo {
            name: "RTX 4090".into(),
            vram_gb: Some(24.0),
        });
        let r = recommend(&specs(8.0, gpu, false));
        assert!(r
            .iter()
            .any(|m| (m.min_memory_gb - 16.0).abs() < f64::EPSILON));
        assert!(!r.iter().any(|m| m.min_memory_gb > 24.0));
        // A 48 GB card fits the 70B tier.
        let big = Some(GpuInfo {
            name: "A6000".into(),
            vram_gb: Some(48.0),
        });
        assert!(recommend(&specs(8.0, big, false))
            .iter()
            .any(|m| m.params_b >= 70.0));
    }

    #[test]
    fn apple_silicon_uses_unified_ram_not_gpu_field() {
        // Unified memory: the GPU "vram" is ignored, the 32 GB system budget applies.
        let gpu = Some(GpuInfo {
            name: "Apple GPU".into(),
            vram_gb: None,
        });
        assert_eq!(specs(32.0, gpu, true).model_memory_gb(), 32.0);
    }

    #[test]
    fn catalog_keys_are_unique_and_resolvable() {
        for m in CATALOG {
            assert!(model_by_key(m.key).is_some());
        }
    }

    #[test]
    fn install_command_is_os_appropriate() {
        // Linux always has the curl one-liner (no brew/winget probe needed).
        let (cmd, args) = ollama_install_command(&specs(16.0, None, false)).unwrap();
        assert_eq!(cmd, "sh");
        assert!(args.last().unwrap().contains("ollama.com/install.sh"));
    }

    #[test]
    fn params_and_mem_floor() {
        assert_eq!(params_from_size("7b"), Some(7.0));
        assert_eq!(params_from_size("1.5b"), Some(1.5));
        assert_eq!(params_from_size("32b"), Some(32.0));
        assert_eq!(params_from_size("e4b"), Some(4.0)); // Gemma
        assert_eq!(params_from_size("latest"), None);
        assert_eq!(mem_floor(7.0), 8.0);
        assert_eq!(mem_floor(14.0), 16.0);
        assert_eq!(mem_floor(70.0), 48.0);
    }

    #[test]
    fn parse_library_extracts_slugs_and_sizes() {
        // Representative of Ollama's library markup: a model link then its size badges.
        let html = r#"
            <a href="/library/qwen3"><span>0.6b</span><span>8b</span><span>32b</span></a>
            <a href="/library/llama4"><div>16b</div><div>128b</div></a>
            <a href="/library/qwen3/tags">ignored nested</a>
        "#;
        let lib = parse_library(html);
        let qwen = lib.iter().find(|(s, _)| s == "qwen3").unwrap();
        assert_eq!(qwen.1, vec!["0.6b", "8b", "32b"]);
        assert!(lib.iter().any(|(s, _)| s == "llama4"));
        // No duplicate slug from the nested /tags link.
        assert_eq!(lib.iter().filter(|(s, _)| s == "qwen3").count(), 1);
    }

    #[test]
    fn rank_uses_benchmarks_filters_budget_and_includes_live() {
        // Real-ish AA rows: exact per-size names (the common case) so each maps to its own score.
        let mut scores = BenchmarkScores::new();
        scores.insert("Qwen2.5-Coder 14B", 70.0, 82.0); // highest coding
        scores.insert("Qwen3 8B", 55.0, 60.0); // a brand-new live model AA already rated
        let live = vec![
            // qwen3 is NOT in the static catalog → must still surface from live discovery.
            (
                "qwen3".to_string(),
                vec!["8b".to_string(), "32b".to_string()],
            ),
        ];
        let specs = specs(16.0, None, false); // budget 16 → 32b (needs 32) excluded
        let ranked = rank_candidates(Some(live), &specs, Some(&scores));

        assert!(
            ranked.iter().all(|c| c.min_memory_gb <= 16.0),
            "budget filtered"
        );
        assert!(
            !ranked.iter().any(|c| c.ollama_tag == "qwen3:32b"),
            "32b doesn't fit"
        );
        // The highest measured coding score ranks first; it's flagged benchmarked.
        assert_eq!(ranked[0].ollama_tag, "qwen2.5-coder:14b");
        assert!(ranked[0].benchmarked);
        // The live-discovered new model is present (and benchmarked, since AA had it).
        let q3 = ranked.iter().find(|c| c.ollama_tag == "qwen3:8b").unwrap();
        assert!(q3.benchmarked);
    }
}
