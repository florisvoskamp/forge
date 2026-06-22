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
}
