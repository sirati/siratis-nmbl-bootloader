use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Decision, EmergencyChoice, LOG_PAGE, LogSource, Screen};

impl<'a> App<'a> {
    /// Reduce a crossterm KeyEvent into a state mutation. Returns
    /// `true` if the App wants to exit (decision is Some).
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        // Ignore Release/Repeat so a held key doesn't fire repeatedly
        // and a key-up after the decisive Press doesn't re-trigger.
        if key.kind != KeyEventKind::Press {
            return self.decision.is_some();
        }

        // NOTE: operator-presence latching is NOT done here. The central
        // `LatchingConsole` layer (which every input poll flows through)
        // owns setting `self.interaction` on the first input of the
        // session, so by the time a key reaches `on_key` the latch is
        // already set. Keeping it out of here is what makes the early
        // boot-log window correct for free: a key there latches presence
        // even though it never reaches `on_key`.

        // Any keypress cancels the countdown — even one we ignore later.
        self.countdown_remaining_secs = None;

        // Global Ctrl shortcuts, handled before the per-screen dispatch
        // so they work from every screen. Plain `e` / `l` keep their
        // per-screen meanings because these require the CONTROL modifier.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('e') => {
                    // Ask to leave this (remote) session. Local loops
                    // ignore the flag today; just record it.
                    self.exit_session = true;
                    return false;
                }
                KeyCode::Char('l') => {
                    if matches!(self.screen, Screen::Log { .. }) {
                        // Toggle closed: pop back to the stashed screen.
                        if let Some(prev) = self.return_screen.take() {
                            self.screen = *prev;
                        }
                    } else {
                        // Stash the current screen and open the log viewer,
                        // defaulting to NMBL's own boot transcript.
                        self.return_screen = Some(Box::new(std::mem::replace(
                            &mut self.screen,
                            Screen::Log {
                                lines: LogSource::Nmbl.read_snapshot(),
                                offset: 0,
                                source: LogSource::Nmbl,
                            },
                        )));
                    }
                    return false;
                }
                KeyCode::Char('k') => {
                    // Only meaningful inside the log viewer: flip between
                    // NMBL's own log and the kernel ring buffer, re-reading
                    // the newly-selected buffer so it is fresh. The scroll
                    // resets to the top of the new content. A no-op on every
                    // other screen.
                    if let Screen::Log {
                        lines,
                        offset,
                        source,
                    } = &mut self.screen
                    {
                        *source = source.toggled();
                        *lines = source.read_snapshot();
                        *offset = 0;
                    }
                    return false;
                }
                KeyCode::Char('g') => {
                    // Toggle the "Select NixOS Generation" checkbox while
                    // the passphrase modal is active. No-op on every other
                    // screen so Ctrl+G is inert outside the unlock prompt.
                    if let Screen::Passphrase {
                        select_generation, ..
                    } = &mut self.screen
                    {
                        *select_generation = !*select_generation;
                    }
                    return false;
                }
                _ => {}
            }
        }

        match &mut self.screen {
            Screen::Log { offset, .. } => {
                // Esc closes the viewer (Ctrl+L is handled above). Other
                // keys scroll; the renderer clamps the offset so
                // over-scroll here is harmless. No Decision is produced.
                match key.code {
                    KeyCode::Esc => {
                        if let Some(prev) = self.return_screen.take() {
                            self.screen = *prev;
                        }
                    }
                    KeyCode::Up => *offset = offset.saturating_sub(1),
                    KeyCode::Down => *offset = offset.saturating_add(1),
                    KeyCode::PageUp => *offset = offset.saturating_sub(LOG_PAGE),
                    KeyCode::PageDown => *offset = offset.saturating_add(LOG_PAGE),
                    KeyCode::Home => *offset = 0,
                    KeyCode::End => *offset = u16::MAX,
                    _ => {}
                }
                false
            }
            Screen::List => Self::handle_list_key(
                key.code,
                &mut self.selected_index,
                self.generations,
                &mut self.screen,
                &mut self.show_kernel_params,
                &mut self.decision,
            ),
            Screen::Editing { .. } => {
                Self::handle_editing_key(key, &mut self.screen, &mut self.decision)
            }
            Screen::Passphrase { .. } => {
                Self::handle_passphrase_key(key, &mut self.screen, &mut self.decision)
            }
            Screen::Emergency { .. } => Self::handle_emergency_key(key.code, &mut self.screen),
            // BootStatus absorbs keypresses without producing a Decision.
            // The boot-status screen is non-interactive: it shows progress
            // until the caller flips the App to a different screen.
            Screen::BootStatus(_) => false,
            // KeyEcho is driven directly by the diagnostic loop in
            // `crate::ui::key_echo`, which appends to the ring buffers
            // *before* invoking `on_key` for any state mutations. We
            // intentionally never produce a [`Decision`] from this
            // screen: the loop exits on Ctrl+C / Ctrl+Esc detected at
            // the loop level, not via `Decision`.
            Screen::KeyEcho { .. } => false,
        }
    }

    pub(super) fn handle_emergency_key(code: KeyCode, screen: &mut Screen) -> bool {
        let Screen::Emergency {
            items,
            selected,
            chosen,
            ..
        } = screen
        else {
            return false;
        };

        let last_idx = items.len().saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected < last_idx {
                    *selected = selected.saturating_add(1);
                }
                false
            }
            KeyCode::Enter => {
                if let Some(item) = items.get(*selected) {
                    *chosen = Some(item.choice);
                    true
                } else {
                    false
                }
            }
            // Hotkeys: 'r' for reboot, 'p' for Pretty Shell (when
            // compiled in), 's' for the raw shell, 't' for reTry boot,
            // 'v' for Verify kexec readiness. Operators in a boot-
            // failure scenario tend to be muscle-memory typing one of
            // these letters; we commit straight away on the first key.
            KeyCode::Char('r') => {
                *chosen = Some(EmergencyChoice::Reboot);
                true
            }
            #[cfg(feature = "pretty-shell")]
            KeyCode::Char('p') => {
                *chosen = Some(EmergencyChoice::PrettyShell);
                true
            }
            KeyCode::Char('s') => {
                *chosen = Some(EmergencyChoice::RawShell);
                true
            }
            KeyCode::Char('t') => {
                *chosen = Some(EmergencyChoice::RetryBoot);
                true
            }
            KeyCode::Char('v') => {
                *chosen = Some(EmergencyChoice::VerifyKexecReadiness);
                true
            }
            KeyCode::Esc => {
                // Esc is a no-op: it preserves the prior selection so a
                // stray keypress doesn't commit. The caller can decide
                // separately to fall through to the default on timeout.
                false
            }
            _ => false,
        }
    }

    pub(super) fn handle_list_key(
        code: KeyCode,
        selected_index: &mut usize,
        generations: &[crate::generations::Generation],
        screen: &mut Screen,
        show_kernel_params: &mut bool,
        decision: &mut Option<Decision>,
    ) -> bool {
        let last_idx = generations.len().saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected_index = selected_index.saturating_sub(1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected_index < last_idx {
                    *selected_index = selected_index.saturating_add(1);
                }
                false
            }
            KeyCode::Enter => {
                // Guard against an empty list: emitting a Boot
                // decision with index 0 would crash the caller as
                // soon as it tried to look up the generation.
                if generations.is_empty() {
                    return false;
                }
                *decision = Some(Decision::Boot {
                    generation_index: *selected_index,
                    cmdline_override: None,
                });
                true
            }
            KeyCode::Char('e') => {
                let buffer = generations
                    .get(*selected_index)
                    .map(|g| g.kernel_params.join(" "))
                    .unwrap_or_default();
                *screen = Screen::Editing {
                    generation_index: *selected_index,
                    line: crate::ui::editline::EditableLine::with_text(buffer),
                };
                false
            }
            KeyCode::Char('p') => {
                *show_kernel_params = !*show_kernel_params;
                false
            }
            KeyCode::Char('s') => {
                *decision = Some(Decision::Shell);
                true
            }
            KeyCode::Char('q') => {
                *decision = Some(Decision::Reboot);
                true
            }
            _ => false,
        }
    }

    pub(super) fn handle_editing_key(
        key: KeyEvent,
        screen: &mut Screen,
        decision: &mut Option<Decision>,
    ) -> bool {
        let Screen::Editing {
            generation_index,
            line,
        } = screen
        else {
            return false;
        };

        // Enter / Esc are owned by the editor screen, not the line.
        match key.code {
            KeyCode::Enter => {
                *decision = Some(Decision::Boot {
                    generation_index: *generation_index,
                    cmdline_override: Some(line.text().to_owned()),
                });
                return true;
            }
            KeyCode::Esc => {
                *screen = Screen::List;
                return false;
            }
            _ => {}
        }
        // Everything else (insert, Backspace/Delete, cursor motion,
        // Ctrl+A/E/D, word-wise motion) goes through the shared
        // editable-line helper so the cmdline editor and the passphrase
        // prompt behave identically.
        line.handle_key(key);
        false
    }

    pub(super) fn handle_passphrase_key(
        key: KeyEvent,
        screen: &mut Screen,
        decision: &mut Option<Decision>,
    ) -> bool {
        let Screen::Passphrase { buffer, cursor, .. } = screen else {
            return false;
        };

        match key.code {
            KeyCode::Enter => {
                // Caller (the passphrase prompt loop) detects the buffer
                // is ready by polling — we do NOT exit the App here.
                // Signal "consumed" with `true` so the supplier's
                // dispatch loop can return cleanly.
                true
            }
            KeyCode::Esc => {
                *decision = Some(Decision::Shell);
                true
            }
            _ => {
                // Drive the same shared editable-line logic as the
                // cmdline editor. The secret stays in the Zeroizing
                // buffer (which derefs to &mut String); only the
                // renderer masks it. The cursor tracks the real index.
                // `allow_word_motion = false`: word jumps would leak
                // where the spaces sit in the masked secret.
                let (new_cursor, _handled) =
                    crate::ui::editline::handle_key_on(buffer, *cursor, key, false);
                *cursor = new_cursor;
                false
            }
        }
    }
}
