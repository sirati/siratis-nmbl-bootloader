//! Mock scenarios for the UI preview: fake generations, boot phases, errors,
//! rescue and LUKS prompts. Each scenario builds a real `nmbl_init::ui::App`
//! in the state NMBL would be in, so the preview draws through NMBL's own view
//! code. None of this is reachable from any production binary.

use std::path::PathBuf;

use nmbl_init::generations::Generation;
use nmbl_init::ui::app::{App, BootStatusData, EmergencyChoice, EmergencyItem, ModalKind, Screen};

/// Marker string the `nmbl-ui-preview-absent` flake check greps for in the
/// production binaries. It must only ever exist in this crate.
pub const PREVIEW_MARKER: &str = "NMBL-UI-PREVIEW-MOCK-BACKEND-7f3c";

/// One selectable scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Selector,
    SelectorCountdown,
    BootStatus,
    LuksPrompt,
    LuksVerifying,
    WrongPassword,
    Emergency,
    RescueConfirm,
    Error,
}

pub const ALL: [Scenario; 9] = [
    Scenario::Selector,
    Scenario::SelectorCountdown,
    Scenario::BootStatus,
    Scenario::LuksPrompt,
    Scenario::LuksVerifying,
    Scenario::WrongPassword,
    Scenario::Emergency,
    Scenario::RescueConfirm,
    Scenario::Error,
];

impl Scenario {
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Selector => "selector",
            Scenario::SelectorCountdown => "selector-countdown",
            Scenario::BootStatus => "boot-status",
            Scenario::LuksPrompt => "luks",
            Scenario::LuksVerifying => "luks-verifying",
            Scenario::WrongPassword => "wrong-password",
            Scenario::Emergency => "emergency",
            Scenario::RescueConfirm => "rescue-confirm",
            Scenario::Error => "error",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        ALL.iter().copied().find(|s| s.name() == name)
    }
}

/// Fake generations shown by the selector scenarios.
pub fn fake_generations() -> Vec<Generation> {
    [
        (42u32, "26.05.20260920 (Yarara)"),
        (41, "26.05.20260915"),
        (40, "26.05.20260901"),
        (37, "25.11.20260801"),
    ]
    .into_iter()
    .map(|(number, label)| Generation {
        number,
        profile_link: PathBuf::from(format!("/nix/var/nix/profiles/system-{number}-link")),
        toplevel: PathBuf::from(format!("/nix/store/fake-nixos-system-{number}")),
        kernel: PathBuf::from("/nix/store/fake-linux/bzImage"),
        initrd: PathBuf::from("/nix/store/fake-initrd/initrd"),
        init_path: PathBuf::from(format!("/nix/var/nix/profiles/system-{number}-link/init")),
        kernel_params: vec!["console=ttyS0".into(), "quiet".into(), "loglevel=4".into()],
        label: label.to_string(),
    })
    .collect()
}

fn fake_log() -> Vec<String> {
    [
        "phase 0.5: mounting boot fs /dev/disk/by-partlabel/disk-main-ESP at /mnt/boot",
        "phase 1: mount pseudo-filesystems",
        "phase 2a: load early kernel modules",
        "phase 2b: load explicit kernel modules",
        "phase 3: storage activations",
        "waiting for /dev/mapper/cryptroot (2s/30s)",
        "signature verified: generation 42 kernel+initrd OK (enforce)",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Build the App for `scenario`. `tick` animates spinners.
pub fn build<'a>(scenario: Scenario, generations: &'a [Generation], tick: u8) -> App<'a> {
    let mut app = App::new(generations);
    app.show_kernel_params = true;
    match scenario {
        Scenario::Selector => {}
        Scenario::SelectorCountdown => app.countdown_remaining_secs = Some(3),
        Scenario::BootStatus => {
            app.screen = Screen::BootStatus(BootStatusData {
                phase: "phase 3: storage activations".into(),
                log_lines: fake_log(),
                spinner_frame: tick,
            });
        }
        Scenario::LuksPrompt | Scenario::LuksVerifying => {
            app.screen = Screen::Passphrase {
                prompt_label: "Unlock cryptroot (/dev/disk/by-partlabel/disk-main-luks)".into(),
                buffer: "hunter2".to_string().into(),
                cursor: 7,
                verifying: scenario == Scenario::LuksVerifying,
                spinner_frame: tick,
                select_generation: false,
            };
        }
        Scenario::WrongPassword => {
            app.modal = Some(ModalKind::Buttons {
                title: "Wrong passphrase".into(),
                message: "cryptsetup could not unlock cryptroot (attempt 2 of 3).".into(),
                labels: vec!["Retry".into(), "Shell".into(), "Reboot".into()],
                selected: 0,
                hint: "left/right choose  Enter confirm".into(),
            });
        }
        Scenario::Emergency => {
            app.screen = Screen::Emergency {
                message: "Likely cause: the root device never appeared.\n\nBoot failed. The chain \
                          of errors is:\n\nmounting / failed\n  device /dev/mapper/cryptroot did \
                          not appear within 30s\n\nChoose what to do next."
                    .into(),
                items: vec![
                    EmergencyItem {
                        label: "Reboot",
                        choice: EmergencyChoice::Reboot,
                    },
                    EmergencyItem {
                        label: "Pretty Shell",
                        choice: EmergencyChoice::PrettyShell,
                    },
                ],
                selected: 0,
                chosen: None,
            };
        }
        Scenario::RescueConfirm => {
            app.modal = Some(ModalKind::Confirm {
                title: "Enter rescue".into(),
                message: "Stateful recovery exhausted after 5 attempts.\nEnter the signed \
                          rescue system (TPM will be capped)?"
                    .into(),
                yes_label: "Rescue".into(),
                no_label: "Menu".into(),
                yes_selected: true,
                hint: "left/right choose  Enter confirm".into(),
            });
        }
        Scenario::Error => {
            app.modal = Some(ModalKind::Error {
                title: "Signature verification failed".into(),
                message: "generation 42 kernel: no trusted key verified the sidecar \
                          (domain nmbl:gen-kernel:v1). Refusing to boot."
                    .into(),
                hint: "Enter dismiss".into(),
            });
        }
    }
    app
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_name_round_trips() {
        for s in ALL {
            assert_eq!(Scenario::from_name(s.name()), Some(s));
        }
    }

    #[test]
    fn every_scenario_renders_through_nmbl_views() {
        let gens = fake_generations();
        for s in ALL {
            let app = build(s, &gens, 0);
            let backend = ratatui::backend::TestBackend::new(100, 30);
            let mut term = ratatui::Terminal::new(backend).unwrap_or_else(|_| panic!("term"));
            term.draw(|f| nmbl_init::ui::render_app(f, &app))
                .unwrap_or_else(|_| panic!("draw {}", s.name()));
        }
    }
}
