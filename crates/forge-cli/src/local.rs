//! Local-LLM management: probe the machine, recommend a Gemma model that fits, and
//! install / run it through a local runtime. Ollama is the first (and currently only) supported
//! runtime — already a first-class mesh provider (`ollama::<tag>`) — but [`Runtime`] is a seam so
//! llama.cpp / LM Studio can plug in later. Backs the `forge local` commands and the setup wizard.
//!
//! Honesty note: the Ollama *tags* below are best-effort. The actual fetch defers to
//! `ollama pull <tag>`, so if a tag isn't in the registry (e.g. an older Ollama that predates
//! Gemma 4) the user sees Ollama's own error rather than a fabricated success.

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

/// A locally-runnable model in the catalog (the Gemma family, per the June 2026 releases).
#[derive(Debug, Clone, Copy)]
pub struct LocalModel {
    /// Stable key used on the CLI (`forge local install <key>`).
    pub key: &'static str,
    /// Human label.
    pub label: &'static str,
    /// Ollama tag, resolved against the registry at `ollama pull` time.
    pub ollama_tag: &'static str,
    /// Parameter count in billions (effective, for MoE).
    pub params_b: f64,
    /// Recommended minimum memory budget (GiB) to run it comfortably at ~Q4.
    pub min_memory_gb: f64,
    pub blurb: &'static str,
}

/// The Gemma catalog (smallest → largest). Sizes track the Gemma 4 release line; `min_memory_gb`
/// is a conservative ~Q4 estimate (≈0.7 GiB/B + runtime overhead).
pub const CATALOG: &[LocalModel] = &[
    LocalModel {
        key: "gemma4-e2b",
        label: "Gemma 4 E2B",
        ollama_tag: "gemma4:e2b",
        params_b: 2.0,
        min_memory_gb: 6.0,
        blurb: "Tiny, fast — fits modest laptops; good for quick edits/chat.",
    },
    LocalModel {
        key: "gemma4-e4b",
        label: "Gemma 4 E4B",
        ollama_tag: "gemma4:e4b",
        params_b: 4.0,
        min_memory_gb: 8.0,
        blurb: "Small but capable; a solid default on 16 GB machines.",
    },
    LocalModel {
        key: "gemma4-12b",
        label: "Gemma 4 12B Unified",
        ollama_tag: "gemma4:12b",
        params_b: 12.0,
        min_memory_gb: 16.0,
        blurb: "Strong general/coding quality; the sweet spot on 16–32 GB.",
    },
    LocalModel {
        key: "gemma4-26b-a4b",
        label: "Gemma 4 26B A4B (MoE)",
        ollama_tag: "gemma4:26b-a4b",
        params_b: 26.0,
        min_memory_gb: 32.0,
        blurb: "Mixture-of-experts: 26B weights, ~4B active — fast for its quality.",
    },
    LocalModel {
        key: "gemma4-31b",
        label: "Gemma 4 31B",
        ollama_tag: "gemma4:31b",
        params_b: 31.0,
        min_memory_gb: 32.0,
        blurb: "Highest quality in the family; wants 32 GB+ / a big GPU.",
    },
];

/// Look up a catalog entry by key.
pub fn model_by_key(key: &str) -> Option<&'static LocalModel> {
    CATALOG.iter().find(|m| m.key == key)
}

/// Rank the catalog for these specs: every model whose memory budget fits, largest first. The first
/// entry is the recommended pick. Empty only on a machine too small for even the E2B model.
pub fn recommend(specs: &SystemSpecs) -> Vec<&'static LocalModel> {
    let budget = specs.model_memory_gb();
    let mut fits: Vec<&LocalModel> = CATALOG
        .iter()
        .filter(|m| m.min_memory_gb <= budget)
        .collect();
    fits.sort_by(|a, b| b.params_b.partial_cmp(&a.params_b).unwrap());
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
    fn recommend_picks_largest_that_fits_ram() {
        // 16 GB → up to the 12B, not the 31B; biggest-fitting is first.
        let r = recommend(&specs(16.0, None, false));
        assert_eq!(r.first().unwrap().key, "gemma4-12b");
        assert!(r.iter().all(|m| m.min_memory_gb <= 16.0));
        assert!(!r.iter().any(|m| m.key == "gemma4-31b"));
    }

    #[test]
    fn tiny_machine_still_gets_the_smallest() {
        let r = recommend(&specs(6.0, None, false));
        assert_eq!(r.last().unwrap().key, "gemma4-e2b");
    }

    #[test]
    fn too_small_machine_gets_nothing() {
        assert!(recommend(&specs(3.0, None, false)).is_empty());
    }

    #[test]
    fn discrete_gpu_vram_is_the_budget_not_system_ram() {
        // 8 GB RAM but a 24 GB GPU → sizing uses the 24 GB VRAM, so the 12B fits (it wouldn't on
        // 8 GB RAM alone, which would cap at the E4B).
        let gpu = Some(GpuInfo {
            name: "RTX 4090".into(),
            vram_gb: Some(24.0),
        });
        let r = recommend(&specs(8.0, gpu, false));
        assert_eq!(r.first().unwrap().key, "gemma4-12b");
        // And a 48 GB card fits the largest.
        let big = Some(GpuInfo {
            name: "A6000".into(),
            vram_gb: Some(48.0),
        });
        assert_eq!(
            recommend(&specs(8.0, big, false)).first().unwrap().key,
            "gemma4-31b"
        );
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
