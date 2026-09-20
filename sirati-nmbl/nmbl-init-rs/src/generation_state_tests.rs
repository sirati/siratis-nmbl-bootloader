use std::fs;
use std::io;
use std::os::unix::fs::symlink;

use super::*;

fn generation(root: &Path, byte: u8) -> io::Result<String> {
    let id = format!("{byte:02x}").repeat(64);
    let dir = root.join("generations").join(&id);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("nix.erofs"), [byte])?;
    fs::write(dir.join("nix.erofs.sig"), [byte])?;
    Ok(id)
}

fn link(root: &Path, name: &str, id: &str) -> io::Result<()> {
    symlink(format!("generations/{id}"), root.join(name))
}

#[test]
fn first_attempt_then_success_blesses_generation()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let id = generation(temp.path(), 1)?;
    link(temp.path(), "active", &id)?;
    link(temp.path(), "pending", &id)?;
    assert_eq!(
        prepare_boot(temp.path(), true, true)?,
        BootStateOutcome::Proceed
    );
    mark_success(temp.path())?;
    assert_eq!(required_id(temp.path(), "tested")?, id);
    assert!(!temp.path().join("attempted").exists());
    assert!(!temp.path().join("pending").exists());
    Ok(())
}

#[test]
fn failed_pending_generation_rolls_back_to_tested()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let good = generation(temp.path(), 2)?;
    let pending = generation(temp.path(), 3)?;
    link(temp.path(), "active", &pending)?;
    link(temp.path(), "pending", &pending)?;
    link(temp.path(), "attempted", &pending)?;
    link(temp.path(), "tested", &good)?;
    assert_eq!(
        prepare_boot(temp.path(), true, true)?,
        BootStateOutcome::RolledBack
    );
    assert_eq!(required_id(temp.path(), "active")?, good);
    assert!(temp.path().join("rollback-event").is_file());
    Ok(())
}

#[test]
fn failed_tested_generation_requests_rescue() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let temp = tempfile::tempdir()?;
    let id = generation(temp.path(), 4)?;
    link(temp.path(), "active", &id)?;
    link(temp.path(), "tested", &id)?;
    link(temp.path(), "attempted", &id)?;
    assert_eq!(
        prepare_boot(temp.path(), true, true)?,
        BootStateOutcome::Rescue
    );
    Ok(())
}

#[test]
fn unsafe_or_incomplete_links_fail_closed() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    symlink("../../etc", temp.path().join("active"))?;
    assert!(prepare_boot(temp.path(), true, true).is_err());
    Ok(())
}
