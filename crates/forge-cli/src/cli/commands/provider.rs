use crate::*;
use anyhow::{Context, Result};

/// `forge provider <add|list|remove>`: manage custom OpenAI-compatible providers.
pub(crate) fn provider_cmd(cmd: ProviderCmd) -> Result<()> {
    match cmd {
        ProviderCmd::Add {
            namespace,
            base_url,
            api_key_env,
            free,
            models,
            label,
        } => provider_add(namespace, base_url, api_key_env, free, models, label),
        ProviderCmd::List => {
            provider_list();
            Ok(())
        }
        ProviderCmd::Remove { namespace } => provider_remove(&namespace),
    }
}

fn provider_add(
    namespace: String,
    base_url: String,
    api_key_env: Option<String>,
    free: bool,
    models: Vec<String>,
    label: Option<String>,
) -> Result<()> {
    let cfg = forge_config::CustomProviderConfig {
        namespace: namespace.clone(),
        base_url: base_url.clone(),
        api_key_env: api_key_env.clone(),
        free,
        models,
        label,
    };
    let path = forge_config::add_custom_provider(&cfg)
        .with_context(|| format!("registering custom provider '{namespace}'"))?;
    println!("✓ Registered custom provider '{namespace}' → {base_url}");
    println!("  Written to {}", path.display());
    match &api_key_env {
        Some(env) if !env.trim().is_empty() => println!(
            "  Set the key with `{env}=…` (or `forge auth {namespace}` to store it in the keyring)."
        ),
        _ => println!("  Keyless endpoint — a placeholder token is sent (fine for local servers)."),
    }
    println!("  Its models join discovery + routing on the next session (try `forge models`).");
    Ok(())
}

fn provider_remove(namespace: &str) -> Result<()> {
    if forge_config::custom_provider(namespace).is_some()
        && forge_config::user_custom_providers()
            .iter()
            .all(|p| p.namespace != namespace)
    {
        anyhow::bail!("'{namespace}' is a built-in provider — it can't be removed");
    }
    let removed = forge_config::remove_custom_provider(namespace)
        .with_context(|| format!("removing custom provider '{namespace}'"))?;
    if removed {
        println!("✓ Removed custom provider '{namespace}' (effective next session).");
    } else {
        println!("No runtime-registered provider '{namespace}' — nothing to remove.");
    }
    Ok(())
}

/// Print built-in custom providers, the user's runtime-registered ones, and scaffolded-but-unwired
/// enterprise gateways.
fn provider_list() {
    let user: Vec<String> = forge_config::user_custom_providers()
        .into_iter()
        .map(|p| p.namespace)
        .collect();

    println!("Custom OpenAI-compatible providers (built-in):");
    for cp in forge_config::custom_providers() {
        if user.iter().any(|u| u == cp.namespace) {
            continue; // listed under the user section below
        }
        let key = if forge_config::has_api_key(cp.namespace) {
            "key set"
        } else {
            "no key"
        };
        println!(
            "  • {:<12} {:<40} [{}{}]",
            cp.namespace,
            cp.endpoint,
            if cp.free { "free, " } else { "" },
            key
        );
    }

    if !user.is_empty() {
        println!("\nYour runtime-registered providers (`forge provider add`):");
        for p in forge_config::user_custom_providers() {
            let key = match &p.api_key_env {
                Some(env) if !env.trim().is_empty() => {
                    if forge_config::has_api_key(&p.namespace) {
                        format!("key set ({env})")
                    } else {
                        format!("needs {env}")
                    }
                }
                _ => "keyless".to_string(),
            };
            println!(
                "  • {:<12} {:<40} [{}{}]",
                p.namespace,
                p.base_url,
                if p.free { "free, " } else { "" },
                key
            );
        }
    }

    println!("\nEnterprise gateways (scaffolded, NOT yet wired in this build):");
    for (ns, why) in forge_config::UNWIRED_ENTERPRISE_PROVIDERS {
        println!("  • {ns}: {why}");
    }
}
