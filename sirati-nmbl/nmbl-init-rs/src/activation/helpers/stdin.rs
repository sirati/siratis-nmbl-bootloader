//! Stdin / passphrase collection for `luks-password` activations.

use zeroize::Zeroizing;

use crate::activation::PasswordSupplier;
use crate::config::{Activation, ActivationKind};
use crate::error::{NmblError, Result};
use crate::ui::console::Console;

use super::kind_label;

/// `None` for every kind except `LuksPassword`, where we prompt and
/// return the raw passphrase bytes. We do NOT append a newline: the
/// cryptsetup argv uses `--key-file=-`, which reads stdin verbatim as
/// binary key data (no stripping). Appending `\n` would turn a 4-byte
/// passphrase "test" into the 5-byte key "test\n", which doesn't match
/// the stored LUKS header digest.
pub(crate) async fn collect_stdin(
    activation: &Activation,
    console: &mut dyn Console,
    supplier: Option<&mut dyn PasswordSupplier>,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    if activation.kind != ActivationKind::LuksPassword {
        return Ok(None);
    }

    let Some(supplier) = supplier else {
        return Err(NmblError::Activation {
            kind: kind_label(activation.kind).to_string(),
            source: Box::new(NmblError::ConfigInvalid {
                reason: "luks-password activation requires a TUI to prompt for the \
                         passphrase, but no PasswordSupplier was provided"
                    .to_string(),
                context: format!(
                    "activation {} ({})",
                    kind_label(activation.kind),
                    activation.description
                ),
            }),
        });
    };

    let label = activation
        .prompt_label
        .as_deref()
        .unwrap_or("Enter passphrase");
    let secret = supplier.prompt(console, label).await?;
    let mut buf = Zeroizing::new(Vec::with_capacity(secret.len()));
    buf.extend_from_slice(secret.as_bytes());
    Ok(Some(buf))
}
