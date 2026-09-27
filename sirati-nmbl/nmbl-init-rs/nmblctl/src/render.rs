//! The `chain` and `status` views: coloured, section-oriented text built from
//! the discovered [`crate::state::System`]. Each returns a `String` the caller
//! pages; keeping them as pure string builders (no I/O of their own beyond what
//! `System` already read) makes them straightforward to eyeball and to smoke-test.

use nmblctl::color::{Palette, Style};

use crate::state::System;

/// Render the whole boot chain as configured.
pub fn chain(sys: &System, p: Palette) -> String {
    let mut out = String::new();
    push_title(&mut out, &p, "NMBL boot chain");

    // Firmware mode + loader.
    section(&mut out, &p, "Firmware & loader");
    if let Some(cfg) = &sys.config {
        // The runtime config does not carry the bootstrapper mode (that is a
        // build-time Nix choice), so infer what we can from the cmdline / ESP.
        let _ = cfg;
    }
    line(&mut out, &p, "firmware", Style::Value, &infer_firmware(sys));
    line(&mut out, &p, "loader", Style::Value, &infer_loader(sys));
    if let Some(src) = &sys.config_source {
        line(
            &mut out,
            &p,
            "config",
            Style::Value,
            &format!("{} ({})", src.display(), config_location(sys)),
        );
    } else {
        line(
            &mut out,
            &p,
            "config",
            Style::Warn,
            "no runtime config found",
        );
    }

    // Signing.
    section(&mut out, &p, "Signing");
    render_signing(&mut out, &p, sys);

    // Generation selection mode.
    section(&mut out, &p, "Generations");
    let mode = if sys.is_signed_erofs() {
        "signed EROFS generations (verified `active` symlink)"
    } else {
        "Nix profile symlinks (stateful/normal store)"
    };
    line(&mut out, &p, "mode", Style::Value, mode);
    render_generations(&mut out, &p, sys);

    // Rescue.
    section(&mut out, &p, "Rescue");
    line(
        &mut out,
        &p,
        "mode",
        Style::Value,
        sys.rescue_mode.as_deref().unwrap_or("unknown"),
    );
    if let Some(cfg) = &sys.config {
        line(
            &mut out,
            &p,
            "automatic",
            if cfg.rescue.automatic {
                Style::Warn
            } else {
                Style::Dim
            },
            &cfg.rescue.automatic.to_string(),
        );
        line(
            &mut out,
            &p,
            "network",
            Style::Dim,
            &cfg.rescue.network.to_string(),
        );
    }

    // TPM / secure-boot.
    section(&mut out, &p, "TPM & secure boot");
    render_tpm_secureboot(&mut out, &p, sys);

    // Stateful.
    section(&mut out, &p, "Stateful");
    render_stateful(&mut out, &p, sys);

    // Persistent default / one-shot.
    section(&mut out, &p, "Operator selections");
    match sys.read_default() {
        Some(sel) => line(
            &mut out,
            &p,
            "persistent default",
            Style::Value,
            &sel.to_string(),
        ),
        None => line(
            &mut out,
            &p,
            "persistent default",
            Style::Dim,
            "(none — active/newest)",
        ),
    }
    match sys.read_one_shot() {
        Some(one) => line(
            &mut out,
            &p,
            "one-shot next boot",
            Style::Warn,
            &format!("generation {}", one.generation),
        ),
        None => line(&mut out, &p, "one-shot next boot", Style::Dim, "(none)"),
    }

    out
}

/// Render the current boot status and health.
pub fn status(sys: &System, p: Palette) -> String {
    let mut out = String::new();
    push_title(&mut out, &p, "NMBL boot status");

    // How this boot was selected, from the cmdline markers.
    section(&mut out, &p, "This boot");
    let parsed = nmbl_init::handover::parse_cmdline(&sys.cmdline);
    let selection = if parsed.has_rollback_marker() {
        ("rollback (untested generation failed)", Style::Warn)
    } else if parsed.boot_set_config().is_some() {
        ("boot-set selector", Style::Value)
    } else {
        ("default / operator choice", Style::Good)
    };
    line(&mut out, &p, "selected via", selection.1, selection.0);
    if let Some(active) = sys.active_generation {
        line(
            &mut out,
            &p,
            "active generation",
            Style::Value,
            &active.to_string(),
        );
    }
    line(
        &mut out,
        &p,
        "rollback marker",
        if parsed.has_rollback_marker() {
            Style::Warn
        } else {
            Style::Dim
        },
        &parsed.has_rollback_marker().to_string(),
    );
    if let Some(cfg) = parsed.boot_set_config() {
        line(&mut out, &p, "boot-set config", Style::Value, cfg);
    }

    // NMBL-set cmdline params vs the rest.
    section(&mut out, &p, "Kernel cmdline (NMBL-set)");
    let nmbl: Vec<&str> = parsed.nmbl_params().map(|x| x.raw.as_str()).collect();
    if nmbl.is_empty() {
        line(&mut out, &p, "params", Style::Dim, "(none)");
    } else {
        for param in nmbl {
            out.push_str(&format!("  {}\n", p.paint(Style::Value, param)));
        }
    }

    // Rescue sentinel presence + automatic setting.
    section(&mut out, &p, "Rescue state");
    let sentinel = sys.sentinel_path();
    let present = sentinel.exists();
    line(
        &mut out,
        &p,
        "rescue sentinel",
        if present { Style::Warn } else { Style::Good },
        &format!(
            "{} ({})",
            if present { "PRESENT" } else { "absent" },
            sentinel.display()
        ),
    );
    if let Some(cfg) = &sys.config {
        line(
            &mut out,
            &p,
            "automatic rescue",
            if cfg.rescue.automatic {
                Style::Warn
            } else {
                Style::Dim
            },
            &cfg.rescue.automatic.to_string(),
        );
    }

    // Signed-generation state (tested/pending/attempted) for EROFS hosts.
    if sys.is_signed_erofs() {
        section(&mut out, &p, "Generation state (signed EROFS)");
        render_generation_health(&mut out, &p, sys);
    }

    // Stateful state machine position + success mark, for stateful hosts.
    section(&mut out, &p, "Stateful health");
    render_stateful_health(&mut out, &p, sys);

    // Which units block the success mark (best-effort systemd query).
    section(&mut out, &p, "Success mark");
    render_success_units(&mut out, &p, sys);

    out
}

// ---- section helpers -------------------------------------------------------

fn push_title(out: &mut String, p: &Palette, title: &str) {
    out.push_str(&p.header(&format!("═══ {title} ═══")));
    out.push('\n');
}

fn section(out: &mut String, p: &Palette, name: &str) {
    out.push('\n');
    out.push_str(&p.paint(Style::Header, name));
    out.push('\n');
}

fn line(out: &mut String, p: &Palette, label: &str, style: Style, value: &str) {
    out.push_str(&p.field(label, style, value));
    out.push('\n');
}

fn config_location(sys: &System) -> &'static str {
    match &sys.config_source {
        Some(s) if s.starts_with("/etc/nmbl") => "embedded in initramfs",
        Some(_) => "external on boot volume",
        None => "unknown",
    }
}

fn infer_firmware(sys: &System) -> String {
    // efi-stub / UEFI hosts have an EFI system partition mounted; BIOS/GRUB
    // hosts do not. Best-effort inference from the running system.
    if std::path::Path::new("/sys/firmware/efi").exists() {
        "UEFI".to_string()
    } else {
        "BIOS (legacy)".to_string()
    }
    .to_string()
        + &format!(" [cmdline: {}]", first_console(sys))
}

fn first_console(sys: &System) -> String {
    nmbl_init::handover::parse_cmdline(&sys.cmdline)
        .params
        .into_iter()
        .find(|x| x.key == "console")
        .and_then(|x| x.value)
        .unwrap_or_else(|| "n/a".to_string())
}

fn infer_loader(sys: &System) -> String {
    // GRUB leaves a /boot/grub; efi-stub writes EFI/BOOT/BOOTX64.EFI.
    if std::path::Path::new("/boot/grub/grub.cfg").exists() {
        "GRUB → NMBL".to_string()
    } else if std::path::Path::new("/boot/EFI/BOOT/BOOTX64.EFI").exists() {
        "efi-stub (NMBL UKI at EFI/BOOT/BOOTX64.EFI)".to_string()
    } else {
        let _ = sys;
        "unknown".to_string()
    }
}

fn render_signing(out: &mut String, p: &Palette, sys: &System) {
    let Some(cfg) = &sys.config else {
        line(out, p, "signing", Style::Dim, "no config");
        return;
    };
    let s = &cfg.signing;
    line(
        out,
        p,
        "enabled",
        if s.enable { Style::Good } else { Style::Dim },
        &s.enable.to_string(),
    );
    line(
        out,
        p,
        "enforce",
        if s.enforce { Style::Good } else { Style::Warn },
        &s.enforce.to_string(),
    );
    line(out, p, "algorithm", Style::Value, &s.algorithm);
    line(out, p, "sidecar suffix", Style::Dim, &s.sig_path_suffix);
    // Trusted key fingerprints are baked into the binary; list them from the
    // in-binary trust anchor so the operator can compare against their key.
    match nmbl_init::sig::parse_baked_keys() {
        Ok(keys) if !keys.is_empty() => {
            for (i, k) in keys.iter().enumerate() {
                let fp = k.fingerprint();
                let hex = fp.iter().map(|b| format!("{b:02x}")).collect::<String>();
                line(out, p, &format!("trusted key #{i}"), Style::Value, &hex);
            }
        }
        Ok(_) => line(out, p, "trusted keys", Style::Dim, "(none baked in)"),
        Err(_) => line(out, p, "trusted keys", Style::Warn, "(unreadable)"),
    }
}

fn render_generations(out: &mut String, p: &Palette, sys: &System) {
    if sys.generations.is_empty() {
        line(out, p, "installed", Style::Dim, "(none discovered)");
        return;
    }
    for &n in &sys.generations {
        let is_active = sys.active_generation == Some(n);
        let marker = if is_active { " (active)" } else { "" };
        let style = if is_active { Style::Good } else { Style::Dim };
        out.push_str(&format!(
            "  {}{}\n",
            p.paint(style, &format!("generation {n}")),
            p.paint(Style::Good, marker)
        ));
    }
}

fn render_tpm_secureboot(out: &mut String, p: &Palette, sys: &System) {
    let Some(cfg) = &sys.config else {
        line(out, p, "tpm", Style::Dim, "no config");
        return;
    };
    line(
        out,
        p,
        "tpm.measure",
        if cfg.tpm.measure {
            Style::Good
        } else {
            Style::Dim
        },
        &cfg.tpm.measure.to_string(),
    );
    line(
        out,
        p,
        "tpm.pcrIndex",
        Style::Value,
        &cfg.tpm.pcr_index.to_string(),
    );
    line(
        out,
        p,
        "tpm.requireTpm",
        if cfg.tpm.require_tpm {
            Style::Good
        } else {
            Style::Dim
        },
        &cfg.tpm.require_tpm.to_string(),
    );
    line(
        out,
        p,
        "secureBoot.enable",
        if cfg.secure_boot.enable {
            Style::Good
        } else {
            Style::Dim
        },
        &cfg.secure_boot.enable.to_string(),
    );
    if cfg.secure_boot.enable {
        line(
            out,
            p,
            "secureBoot.enforce",
            if cfg.secure_boot.enforce {
                Style::Good
            } else {
                Style::Warn
            },
            &cfg.secure_boot.enforce.to_string(),
        );
        line(
            out,
            p,
            "priority file",
            Style::Dim,
            &cfg.secure_boot.signed_file_path.display().to_string(),
        );
    }
}

fn render_stateful(out: &mut String, p: &Palette, sys: &System) {
    let Some(cfg) = &sys.config else {
        line(out, p, "stateful", Style::Dim, "no config");
        return;
    };
    match &cfg.stateful {
        Some(s) => {
            line(out, p, "enabled", Style::Good, "true");
            line(
                out,
                p,
                "maxRecoveryAttempts",
                Style::Value,
                &s.max_recovery_attempts.to_string(),
            );
            line(out, p, "successTarget", Style::Value, &s.success_target);
            line(
                out,
                p,
                "state dir",
                Style::Dim,
                &sys.state_dir.display().to_string(),
            );
        }
        None => line(out, p, "enabled", Style::Dim, "false"),
    }
}

fn render_generation_health(out: &mut String, p: &Palette, sys: &System) {
    let Some(cfg) = &sys.config else { return };
    let Some(gi) = cfg.generation_image.as_ref() else {
        return;
    };
    // Resolve the state root the same way boot does.
    let root = gi
        .stage1_store
        .as_ref()
        .map(|s| s.mountpoint.join(&s.relative_state_root))
        .unwrap_or_else(|| gi.state_root.clone());
    match nmbl_init::generation_state::inspect_health(&root) {
        Ok(Some(h)) => {
            line(
                out,
                p,
                "active tested",
                if h.active_is_tested {
                    Style::Good
                } else {
                    Style::Warn
                },
                &h.active_is_tested.to_string(),
            );
            line(
                out,
                p,
                "pending untested",
                if h.pending_present {
                    Style::Warn
                } else {
                    Style::Dim
                },
                &h.pending_present.to_string(),
            );
            line(
                out,
                p,
                "attempted unresolved",
                if h.attempted_unresolved {
                    Style::Warn
                } else {
                    Style::Dim
                },
                &h.attempted_unresolved.to_string(),
            );
        }
        Ok(None) => line(out, p, "state", Style::Dim, "(no active selector yet)"),
        Err(e) => line(out, p, "state", Style::Warn, &format!("unreadable: {e}")),
    }
}

fn render_stateful_health(out: &mut String, p: &Palette, sys: &System) {
    let Some(cfg) = &sys.config else {
        line(out, p, "stateful", Style::Dim, "no config");
        return;
    };
    if cfg.stateful.is_none() {
        line(out, p, "stateful", Style::Dim, "not enabled on this host");
        return;
    }
    let path = sys.state_dir.join("state.bin");
    match nmbl_init::state::read(&path) {
        Ok(Some(st)) => {
            line(
                out,
                p,
                "last boot succeeded",
                if st.last_boot_succeeded {
                    Style::Good
                } else {
                    Style::Warn
                },
                &st.last_boot_succeeded.to_string(),
            );
            line(
                out,
                p,
                "recovery attempt",
                Style::Value,
                &st.recovery_attempt.to_string(),
            );
            match st.last_attempted_generation {
                Some(n) => line(
                    out,
                    p,
                    "last attempted gen",
                    Style::Value,
                    &n.get().to_string(),
                ),
                None => line(out, p, "last attempted gen", Style::Dim, "(none)"),
            }
            let good: Vec<String> = st
                .known_good_generations
                .iter()
                .filter_map(|s| s.map(|v| v.get().to_string()))
                .collect();
            line(
                out,
                p,
                "known-good ring",
                Style::Dim,
                &if good.is_empty() {
                    "(empty)".to_string()
                } else {
                    good.join(", ")
                },
            );
        }
        Ok(None) => line(out, p, "state.bin", Style::Dim, "absent or newer-version"),
        Err(e) => line(
            out,
            p,
            "state.bin",
            Style::Warn,
            &format!("unreadable: {e}"),
        ),
    }
}

fn render_success_units(out: &mut String, p: &Palette, sys: &System) {
    // The success mark is set by nmbl-generation-success (EROFS) or
    // nmbl-boot-succeeded (stateful) once boot-complete.target / the success
    // target is reached. Report each unit's state via systemctl is-active, and
    // list what is failing (which blocks the success mark).
    let units = if sys.is_signed_erofs() {
        vec!["nmbl-generation-success.service", "boot-complete.target"]
    } else {
        vec!["nmbl-boot-succeeded.service", "multi-user.target"]
    };
    for unit in units {
        let state = systemctl_is_active(unit).unwrap_or_else(|| "unknown".to_string());
        let style = match state.as_str() {
            "active" => Style::Good,
            "inactive" | "activating" => Style::Warn,
            "failed" => Style::Bad,
            _ => Style::Dim,
        };
        line(out, p, unit, style, &state);
    }
    // The failed units that would block systemd-boot-check-no-failures.
    match systemctl_failed_units() {
        Some(failed) if !failed.is_empty() => {
            line(
                out,
                p,
                "failed units (block success)",
                Style::Bad,
                &failed.join(", "),
            );
        }
        Some(_) => line(out, p, "failed units", Style::Good, "none"),
        None => line(
            out,
            p,
            "failed units",
            Style::Dim,
            "(systemctl unavailable)",
        ),
    }
}

fn systemctl_is_active(unit: &str) -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(["is-active", unit])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn systemctl_failed_units() -> Option<Vec<String>> {
    let out = std::process::Command::new("systemctl")
        .args(["list-units", "--state=failed", "--no-legend", "--plain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect(),
    )
}
