//! `nibble keybinds --install / --uninstall`: nibble's zellij keybindings.
//!
//! Alt-a jumps to the agents sidebar and Alt-v splits the focused pane to
//! open vert on its repo. Zellij reads only the first `keybinds` block of
//! `config.kdl`, so the bindings go into the user's existing block as a
//! marker-delimited section. Re-installing replaces that section, and
//! nothing outside the markers is ever edited: uninstall restores the
//! pre-install bytes.

use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

const BEGIN_MARKER: &str = "// >>> managed by nibble (nibble keybinds --install)";
const END_MARKER: &str = "// <<< managed by nibble";

/// The bindings nibble owns, written inside a `keybinds` block.
const MANAGED_BINDS: &str = r#"    shared_except "locked" {
        // Alt a: jump to the agents sidebar, opening it if needed
        bind "Alt a" {
            Run "nibble" "sidebar" "--focus" {
                floating true
                close_on_exit true
            }
        }
        // Alt v: split the focused pane and open vert on its repo
        bind "Alt v" {
            Run "nibble" "vert" {
                direction "right"
                close_on_exit true
            }
        }
    }
"#;

/// Entry point for `nibble keybinds`.
pub(crate) fn cmd_keybinds(install: bool, uninstall: bool) -> Result<()> {
    let path = resolve_config_path(
        std::env::var_os("ZELLIJ_CONFIG_FILE"),
        crate::status::zellij_config_dir(),
    );
    if install {
        install_at(&path)
    } else if uninstall {
        uninstall_at(&path)
    } else {
        println!("nibble keybindings (zellij): Alt a = agents sidebar, Alt v = vert on this repo.");
        println!(
            "  nibble keybinds --install    add them to {}",
            path.display()
        );
        println!("  nibble keybinds --uninstall  remove them");
        Ok(())
    }
}

/// The config file zellij loads: `ZELLIJ_CONFIG_FILE` when set, otherwise
/// `config.kdl` in the config dir.
fn resolve_config_path(file: Option<OsString>, dir: PathBuf) -> PathBuf {
    file.filter(|f| !f.is_empty())
        .map_or_else(|| dir.join("config.kdl"), PathBuf::from)
}

fn read_config(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(Some(body)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

fn install_at(path: &Path) -> Result<()> {
    let existing = read_config(path)?;
    let updated = match install_section(existing.as_deref()) {
        Ok(updated) => updated,
        Err(e) => {
            println!(
                "Add this inside the keybinds block of {}:\n",
                path.display()
            );
            print!("{}", nested_section());
            return Err(e);
        }
    };
    if existing.as_deref() == Some(updated.as_str()) {
        println!(
            "nibble keybindings already up to date in {}.",
            path.display()
        );
        return Ok(());
    }
    write_atomic(path, &updated)?;
    println!(
        "Installed nibble keybindings in {} (Alt a: sidebar, Alt v: vert).",
        path.display()
    );
    Ok(())
}

fn uninstall_at(path: &Path) -> Result<()> {
    let Some(body) = read_config(path)? else {
        println!("No {} — nothing to remove.", path.display());
        return Ok(());
    };
    match remove_section(&body) {
        None => println!("No nibble keybindings in {}.", path.display()),
        // A symlinked config (dotfiles) keeps its link, emptied.
        Some(rest) if rest.is_empty() && !path.is_symlink() => {
            std::fs::remove_file(path)
                .with_context(|| format!("could not remove {}", path.display()))?;
            println!(
                "Removed {} (it only held nibble keybindings).",
                path.display()
            );
        }
        Some(rest) => {
            write_atomic(path, &rest)?;
            println!("Removed nibble keybindings from {}.", path.display());
        }
    }
    Ok(())
}

/// Replace `path` with `body` through a temp file in the same directory, so
/// an interrupted write never leaves a truncated config. Symlinks are
/// followed and the target keeps its permissions.
fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let target = link_target(path);
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("could not create a temp file in {}", dir.display()))?;
    tmp.write_all(body.as_bytes())
        .and_then(|()| tmp.as_file().sync_all())
        .with_context(|| format!("could not write a temp file in {}", dir.display()))?;
    if let Ok(meta) = std::fs::metadata(&target) {
        tmp.as_file()
            .set_permissions(meta.permissions())
            .with_context(|| format!("could not copy permissions of {}", target.display()))?;
    }
    tmp.persist(&target)
        .with_context(|| format!("could not replace {}", target.display()))?;
    Ok(())
}

/// Where writes to `path` should land: the file its symlinks point to, even
/// when that file does not exist yet, so a dotfiles link is never replaced.
fn link_target(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    // canonicalize fails on a dangling link; walk it by hand (bounded, so a
    // link loop falls back to writing the last hop).
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        let Ok(next) = std::fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(dir) if next.is_relative() => dir.join(next),
            _ => next,
        };
    }
    current
}

/// The section placed inside an existing `keybinds` block.
fn nested_section() -> String {
    format!("    {BEGIN_MARKER}\n{MANAGED_BINDS}    {END_MARKER}\n")
}

/// The section as a whole `keybinds` block, for configs without one. The
/// markers wrap the block so uninstall removes it too.
fn top_level_section() -> String {
    format!("{BEGIN_MARKER}\nkeybinds {{\n{MANAGED_BINDS}}}\n{END_MARKER}\n")
}

fn is_keybinds_opener(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("keybinds")
        .is_some_and(|rest| rest.trim().is_empty() || rest.starts_with([' ', '{', '\t']))
}

/// The config with nibble's section placed first in its `keybinds` block,
/// replacing any earlier copy. `None` means there is no config file yet.
fn install_section(existing: Option<&str>) -> Result<String> {
    let body = existing.map_or_else(String::new, |b| {
        remove_section(b).unwrap_or_else(|| b.to_string())
    });
    if body.is_empty() {
        return Ok(top_level_section());
    }
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    match lines.iter().position(|l| is_keybinds_opener(l)) {
        Some(i) if lines[i].trim_end().ends_with('{') => {
            let mut out = lines[..=i].concat();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&nested_section());
            out.push_str(&lines[i + 1..].concat());
            Ok(out)
        }
        Some(_) => bail!(
            "the keybinds block in config.kdl does not end its first line with '{{'; \
             add the nibble section by hand"
        ),
        // Always one '\n' before the block: it ends an unterminated last
        // line or leaves a blank line, and uninstall drops exactly that one.
        None => Ok(format!("{body}\n{}", top_level_section())),
    }
}

/// The config without nibble's section, or `None` when it has none.
fn remove_section(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let start = lines.iter().position(|l| l.trim() == BEGIN_MARKER)?;
    let end = start + lines[start..].iter().position(|l| l.trim() == END_MARKER)?;
    let mut out = lines[..start].concat();
    let tail = lines[end + 1..].concat();
    let top_level = !lines[start].starts_with(char::is_whitespace);
    if top_level && tail.is_empty() {
        // The separator install_section put before an appended block.
        out.pop();
    }
    out.push_str(&tail);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_CONFIG: &str = "theme \"x\"\n\nkeybinds clear-defaults=true {\n    normal {\n        bind \"Alt n\" { NewPane; }\n    }\n}\n";

    fn round_trip(original: &str) {
        let installed = install_section(Some(original)).unwrap();
        assert_eq!(install_section(Some(&installed)).unwrap(), installed);
        assert_eq!(remove_section(&installed).unwrap(), original);
    }

    #[test]
    fn creates_a_config_when_none_exists() {
        let out = install_section(None).unwrap();
        assert_eq!(out, top_level_section());
        assert_eq!(install_section(Some(&out)).unwrap(), out);
        assert_eq!(remove_section(&out).unwrap(), "");
    }

    #[test]
    fn inserts_first_in_the_existing_keybinds_block() {
        let out = install_section(Some(USER_CONFIG)).unwrap();
        let expected = format!(
            "theme \"x\"\n\nkeybinds clear-defaults=true {{\n{}    normal {{\n        bind \"Alt n\" {{ NewPane; }}\n    }}\n}}\n",
            nested_section()
        );
        assert_eq!(out, expected);
        round_trip(USER_CONFIG);
    }

    #[test]
    fn reinstall_replaces_a_stale_section() {
        let stale = USER_CONFIG.replace(
            "keybinds clear-defaults=true {\n",
            &format!(
                "keybinds clear-defaults=true {{\n    {BEGIN_MARKER}\n    bind \"Alt z\" {{ Quit; }}\n    {END_MARKER}\n"
            ),
        );
        let out = install_section(Some(&stale)).unwrap();
        assert!(!out.contains("Alt z"));
        assert_eq!(out, install_section(Some(USER_CONFIG)).unwrap());
    }

    #[test]
    fn appends_a_marked_block_when_config_has_no_keybinds() {
        let out = install_section(Some("theme \"x\"\n")).unwrap();
        assert_eq!(out, format!("theme \"x\"\n\n{}", top_level_section()));
        round_trip("theme \"x\"\n");
        round_trip("theme \"x\"");
        round_trip("theme \"x\"\n\n");
    }

    #[test]
    fn preserves_crlf_and_a_missing_final_newline() {
        round_trip("theme \"x\"\r\nkeybinds {\r\n    normal {\r\n    }\r\n}\r\n");
        round_trip("keybinds {\n    normal {\n    }\n}");
    }

    #[test]
    fn opener_on_the_last_line_still_gets_the_section() {
        let out = install_section(Some("keybinds {")).unwrap();
        assert_eq!(out, format!("keybinds {{\n{}", nested_section()));
    }

    #[test]
    fn ignores_names_that_only_start_with_keybinds() {
        let out = install_section(Some("keybinds_extra true\n")).unwrap();
        assert_eq!(
            out,
            format!("keybinds_extra true\n\n{}", top_level_section())
        );
    }

    #[test]
    fn refuses_a_single_line_keybinds_block() {
        let err = install_section(Some("keybinds { normal { } }\n")).unwrap_err();
        assert!(err.to_string().contains("add the nibble section by hand"));
    }

    #[test]
    fn remove_without_section_or_end_marker_is_none() {
        assert!(remove_section(USER_CONFIG).is_none());
        assert!(remove_section(&format!("{BEGIN_MARKER}\nbind x\n")).is_none());
    }

    #[test]
    fn remove_keeps_content_after_an_appended_block() {
        let body = format!("a\n\n{}b\n", top_level_section());
        assert_eq!(remove_section(&body).unwrap(), "a\n\nb\n");
    }

    #[test]
    fn config_file_env_wins_over_the_config_dir() {
        let dir = PathBuf::from("/cfg");
        assert_eq!(
            resolve_config_path(Some("/other.kdl".into()), dir.clone()),
            PathBuf::from("/other.kdl")
        );
        assert_eq!(
            resolve_config_path(Some(OsString::new()), dir.clone()),
            dir.join("config.kdl")
        );
        assert_eq!(
            resolve_config_path(None, dir.clone()),
            dir.join("config.kdl")
        );
    }

    #[test]
    fn install_and_uninstall_files_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("zellij/config.kdl");

        uninstall_at(&path).unwrap(); // no file: nothing to do
        install_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), top_level_section());
        install_at(&path).unwrap(); // already up to date
        uninstall_at(&path).unwrap();
        assert!(!path.exists());

        std::fs::write(&path, USER_CONFIG).unwrap();
        uninstall_at(&path).unwrap(); // no section: untouched
        install_at(&path).unwrap();
        uninstall_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), USER_CONFIG);
    }

    #[test]
    fn install_refusal_leaves_the_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.kdl");
        std::fs::write(&path, "keybinds { }\n").unwrap();
        assert!(install_at(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keybinds { }\n");
    }

    #[test]
    fn writes_through_a_symlink_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("dotfiles.kdl");
        let link = tmp.path().join("config.kdl");
        std::fs::write(&real, USER_CONFIG).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        install_at(&link).unwrap();
        assert!(link.is_symlink());
        assert!(std::fs::read_to_string(&real)
            .unwrap()
            .contains(BEGIN_MARKER));
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        uninstall_at(&link).unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), USER_CONFIG);
    }

    #[test]
    fn uninstall_empties_a_symlinked_config_instead_of_deleting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("dotfiles.kdl");
        let link = tmp.path().join("config.kdl");
        std::fs::write(&real, top_level_section()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        uninstall_at(&link).unwrap();
        assert!(link.is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "");
    }

    #[test]
    fn dangling_symlink_is_kept_and_its_target_created() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("config.kdl");
        let real = tmp.path().join("dotfiles/zellij.kdl");
        std::os::unix::fs::symlink("dotfiles/zellij.kdl", &link).unwrap();

        install_at(&link).unwrap();
        assert!(link.is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), top_level_section());
        uninstall_at(&link).unwrap();
        assert!(link.is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "");
    }

    #[test]
    fn link_target_follows_chains_and_stops_on_loops() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let missing = tmp.path().join("missing");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&missing, &b).unwrap();
        assert_eq!(link_target(&a), missing);

        let x = tmp.path().join("x");
        let y = tmp.path().join("y");
        std::os::unix::fs::symlink(&y, &x).unwrap();
        std::os::unix::fs::symlink(&x, &y).unwrap();
        let end = link_target(&x);
        assert!(end == x || end == y);
    }

    #[test]
    fn unreadable_config_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where the file should be cannot be read as a string.
        let err = install_at(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("could not read"));
    }
}
