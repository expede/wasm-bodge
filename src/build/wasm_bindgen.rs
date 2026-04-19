use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::targets::{WasmBindgenTarget, WasmVariant};

/// Build wasm and run wasm-bindgen for all targets.
///
/// When `debug_variant` is true, also builds a parallel debug variant with
/// DWARF preserved. If `debug_profile` is declared in the crate's Cargo.toml
/// (or in its workspace root), a second `cargo build --profile <debug_profile>`
/// drives the debug build. Otherwise wasm-bodge warns and falls back to
/// copying the release wasm, whose DWARF presence depends on the consumer's
/// release profile settings.
pub fn build_wasm(
    crate_path: &Path,
    output_dir: &Path,
    release_profile: &str,
    debug_profile: &str,
    wasm_opt: bool,
    debug_variant: bool,
) -> Result<()> {
    println!("  Building Rust crate (profile: {})...", release_profile);
    cargo_build(crate_path, release_profile)?;
    let release_wasm = wasm_artifact_path(crate_path, release_profile)?;

    let debug_wasm: Option<PathBuf> = if debug_variant {
        if profile_is_declared(crate_path, debug_profile)? {
            println!(
                "  Building Rust crate (profile: {}, for debug variant)...",
                debug_profile
            );
            cargo_build(crate_path, debug_profile)?;
            Some(wasm_artifact_path(crate_path, debug_profile)?)
        } else {
            warn_fallback(debug_profile);
            Some(copy_for_debug_fallback(&release_wasm)?)
        }
    } else {
        None
    };

    if wasm_opt {
        println!("  Running wasm-opt (optimized, strips debug symbols)...");
        run_wasm_opt(&release_wasm)?;
    }

    std::fs::create_dir_all(output_dir)?;

    for target in WasmBindgenTarget::all() {
        run_wasm_bindgen(&release_wasm, output_dir, *target, WasmVariant::Optimized)?;
    }

    if let Some(debug_wasm) = debug_wasm.as_deref() {
        for target in [WasmBindgenTarget::Web, WasmBindgenTarget::Bundler] {
            run_wasm_bindgen(debug_wasm, output_dir, target, WasmVariant::Debug)?;
        }
    }

    Ok(())
}

fn cargo_build(crate_path: &Path, profile: &str) -> Result<()> {
    let profile_arg = if profile == "release" {
        "--release".to_string()
    } else {
        format!("--profile={}", profile)
    };

    let status = Command::new("cargo")
        .args([
            "build",
            "--target",
            "wasm32-unknown-unknown",
            &profile_arg,
            "--manifest-path",
            &crate_path.join("Cargo.toml").to_string_lossy(),
        ])
        .status()
        .context("Failed to run cargo build")?;

    if !status.success() {
        anyhow::bail!("cargo build failed for profile `{}`", profile);
    }
    Ok(())
}

fn wasm_artifact_path(crate_path: &Path, profile: &str) -> Result<PathBuf> {
    let target_dir = find_target_dir(crate_path)?;
    let crate_name = get_crate_name(crate_path)?;
    let wasm_name = crate_name.replace('-', "_");
    let path = target_dir
        .join("wasm32-unknown-unknown")
        .join(profile)
        .join(format!("{}.wasm", wasm_name));

    if !path.exists() {
        anyhow::bail!("Wasm file not found at {:?}", path);
    }
    Ok(path)
}

/// Check whether `Cargo.toml` (or the workspace root's `Cargo.toml`) declares
/// a `[profile.<profile>]` section. We parse both because custom profiles are
/// frequently defined once at the workspace root and inherited by members.
fn profile_is_declared(crate_path: &Path, profile: &str) -> Result<bool> {
    if profile_is_in_manifest(&crate_path.join("Cargo.toml"), profile)? {
        return Ok(true);
    }

    if let Some(workspace_root) = workspace_root(crate_path)?
        && workspace_root != crate_path
        && profile_is_in_manifest(&workspace_root.join("Cargo.toml"), profile)?
    {
        return Ok(true);
    }

    Ok(false)
}

fn profile_is_in_manifest(cargo_toml: &Path, profile: &str) -> Result<bool> {
    if !cargo_toml.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(cargo_toml)
        .with_context(|| format!("Failed to read {}", cargo_toml.display()))?;
    let parsed: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", cargo_toml.display()))?;

    Ok(parsed
        .get("profile")
        .and_then(|v| v.get(profile))
        .is_some())
}

fn workspace_root(crate_path: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version=1",
            "--no-deps",
            "--manifest-path",
            &crate_path.join("Cargo.toml").to_string_lossy(),
        ])
        .output()
        .context("Failed to run cargo metadata")?;

    if !output.status.success() {
        return Ok(None);
    }

    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("Failed to parse cargo metadata output")?;

    Ok(metadata["workspace_root"]
        .as_str()
        .map(PathBuf::from))
}

fn warn_fallback(profile: &str) {
    eprintln!(
        "\
warning: --debug-variant was requested but `[profile.{profile}]` is not
         declared in Cargo.toml (or in the workspace root). Falling back to
         copying the release wasm; DWARF symbols will only be preserved if
         your `[profile.release]` sets `debug = true` (or similar).

         Recommended: add the following to your Cargo.toml (or your
         workspace root's Cargo.toml) to get a proper debug build:

             [profile.{profile}]
             inherits = \"dev\"
             debug = \"full\"
             opt-level = 0
             strip = \"none\"

         Or pass `--debug-profile <name>` to use a different profile.\n",
        profile = profile
    );
}

fn copy_for_debug_fallback(release_wasm: &Path) -> Result<PathBuf> {
    let debug_wasm_dir = release_wasm.parent().unwrap().join("_wasm_bodge_debug");
    std::fs::create_dir_all(&debug_wasm_dir)?;
    let dest = debug_wasm_dir.join(release_wasm.file_name().unwrap());
    std::fs::copy(release_wasm, &dest).context("Failed to copy wasm for debug fallback")?;
    Ok(dest)
}

fn run_wasm_opt(wasm_file: &Path) -> Result<()> {
    let wasm_path = wasm_file.to_string_lossy();
    let status = Command::new("wasm-opt")
        .args(["-O4", "--all-features", "-o", &wasm_path, &wasm_path])
        .status()
        .context("Failed to run wasm-opt. Is it installed? (cargo install wasm-opt)")?;

    if !status.success() {
        anyhow::bail!("wasm-opt failed");
    }
    Ok(())
}

fn run_wasm_bindgen(
    wasm_file: &Path,
    output_dir: &Path,
    target: WasmBindgenTarget,
    variant: WasmVariant,
) -> Result<()> {
    let dir_name = format!("{}{}", target.dir_name(), variant.dir_suffix());
    println!(
        "  Running wasm-bindgen for target '{}' ({})...",
        target,
        if variant.is_debug() {
            "debug"
        } else {
            "optimized"
        }
    );
    let target_dir = output_dir.join(&dir_name);
    std::fs::create_dir_all(&target_dir)?;

    let mut cmd = Command::new("wasm-bindgen");
    cmd.args([
        &wasm_file.to_string_lossy(),
        "--out-dir",
        &target_dir.to_string_lossy(),
        "--target",
        target.as_str(),
        "--weak-refs",
    ]);
    if variant.is_debug() {
        cmd.arg("--keep-debug");
    }
    let status = cmd.status().context("Failed to run wasm-bindgen")?;

    if !status.success() {
        anyhow::bail!("wasm-bindgen failed for target '{}' ({})", target, dir_name);
    }
    Ok(())
}

fn find_target_dir(crate_path: &Path) -> Result<PathBuf> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version=1",
            "--no-deps",
            "--manifest-path",
            &crate_path.join("Cargo.toml").to_string_lossy(),
        ])
        .output()
        .context("Failed to run cargo metadata")?;

    if output.status.success() {
        let metadata: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("Failed to parse cargo metadata")?;

        if let Some(target_dir) = metadata["target_directory"].as_str() {
            return Ok(PathBuf::from(target_dir));
        }
    }

    Ok(crate_path.join("target"))
}

fn get_crate_name(crate_path: &Path) -> Result<String> {
    let cargo_toml_path = crate_path.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path).context("Failed to read Cargo.toml")?;

    let parsed: toml::Value = toml::from_str(&content).context("Failed to parse Cargo.toml")?;

    parsed["package"]["name"]
        .as_str()
        .map(String::from)
        .context("Could not find package name in Cargo.toml")
}
