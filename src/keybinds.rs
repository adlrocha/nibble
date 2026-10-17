//! `nibble keybinds --install / --uninstall`: nibble's zellij keybindings.
//!
//! Alt-a jumps to the agents sidebar and Alt-v splits the focused pane to
//! open vert on its repo. Zellij reads only the first `keybinds` block of
//! `config.kdl`, so the bindings go into the user's existing block as a
//! marker-delimited section. Re-installing replaces that section, and
//! nothing outside the markers is ever edited.

use anyhow::{bail, Context, Result};

const BEGIN_MARKER: &str = "// >>> managed by nibble (nibble keybinds --install)";
const END_MARKER: &str = "// <<< managed by nibble";

/// The bindings nibble owns, written inside the user's `keybinds` block.
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

/// A config holding nothing but nibble's section, as written when the user
/// had no `config.kdl`. Uninstall deletes a file that is reduced to this.
const EMPTY_KEYBINDS: &str = "keybinds {\n}\n";

/// Entry point for `nibble keybinds`.
pub(crate) fn cmd_keybinds(install: bool, uninstall: bool) -> Result<()> {
    let path = crate::status::zellij_config_dir().join("config.kdl");
    let existing = match std::fs::read_to_string(&path) {
        Ok(body) => Some(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };

    if uninstall {
        let Some(body) = existing else {
            println!("No {} — nothing to remove.", path.display());
            return Ok(());
        };
        match remove_section(&body) {
            None => println!("No nibble keybindings in {}.", path.display()),
            Some(rest) if rest == EMPTY_KEYBINDS => {
                std::fs::remove_file(&path)
                    .with_context(|| format!("could not remove {}", path.display()))?;
                println!(
                    "Removed {} (it only held nibble keybindings).",
                    path.display()
                );
            }
            Some(rest) => {
                std::fs::write(&path, rest)
                    .with_context(|| format!("could not write {}", path.display()))?;
                println!("Removed nibble keybindings from {}.", path.display());
            }
        }
        return Ok(());
    }

    if !install {
        println!("nibble keybindings (zellij): Alt a = agents sidebar, Alt v = vert on this repo.");
        println!(
            "  nibble keybinds --install    add them to {}",
            path.display()
        );
        println!("  nibble keybinds --uninstall  remove them");
        return Ok(());
    }

    let updated = match install_section(existing.as_deref()) {
        Ok(updated) => updated,
        Err(e) => {
            println!(
                "Add this inside the keybinds block of {}:\n",
                path.display()
            );
            print!("{}", section());
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
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    std::fs::write(&path, updated)
        .with_context(|| format!("could not write {}", path.display()))?;
    println!(
        "Installed nibble keybindings in {} (Alt a: sidebar, Alt v: vert).",
        path.display()
    );
    Ok(())
}

/// The marker-delimited section, indented for a top-level `keybinds` block.
fn section() -> String {
    format!("    {BEGIN_MARKER}\n{MANAGED_BINDS}    {END_MARKER}\n")
}

/// The config with nibble's section placed first in its `keybinds` block,
/// replacing any earlier copy. `None` means there is no config file yet.
fn install_section(existing: Option<&str>) -> Result<String> {
    let body = existing.map_or_else(String::new, |b| remove_section(b).unwrap_or(b.to_string()));
    let mut lines: Vec<&str> = body.lines().collect();
    let opener = lines.iter().position(|l| {
        let t = l.trim_start();
        t.strip_prefix("keybinds")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '{', '\t']))
    });
    let section = section();
    let mut out = match opener {
        Some(i) if lines[i].trim_end().ends_with('{') => {
            lines.insert(i + 1, section.trim_end_matches('\n'));
            lines.join("\n")
        }
        Some(_) => bail!(
            "the keybinds block in config.kdl does not end its first line with '{{'; \
             add the nibble section by hand"
        ),
        None => {
            let sep = if body.is_empty() || body.ends_with("\n\n") {
                ""
            } else if body.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{body}{sep}keybinds {{\n{section}}}")
        }
    };
    out.push('\n');
    Ok(out)
}

/// The config without nibble's section, or `None` when it has none.
fn remove_section(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.iter().position(|l| l.trim() == BEGIN_MARKER)?;
    let end = start + lines[start..].iter().position(|l| l.trim() == END_MARKER)?;
    let kept: Vec<&str> = lines[..start]
        .iter()
        .chain(&lines[end + 1..])
        .copied()
        .collect();
    let mut out = kept.join("\n");
    if body.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_CONFIG: &str = "theme \"x\"\n\nkeybinds clear-defaults=true {\n    normal {\n        bind \"Alt n\" { NewPane; }\n    }\n}\n";

    #[test]
    fn creates_a_config_when_none_exists() {
        let out = install_section(None).unwrap();
        assert_eq!(out, format!("keybinds {{\n{}}}\n", section()));
        assert_eq!(remove_section(&out).unwrap(), EMPTY_KEYBINDS);
    }

    #[test]
    fn inserts_first_in_the_existing_keybinds_block() {
        let out = install_section(Some(USER_CONFIG)).unwrap();
        let expected = format!(
            "theme \"x\"\n\nkeybinds clear-defaults=true {{\n{}    normal {{\n        bind \"Alt n\" {{ NewPane; }}\n    }}\n}}\n",
            section()
        );
        assert_eq!(out, expected);
        assert_eq!(out.matches("keybinds").count(), 2); // block + marker text
    }

    #[test]
    fn reinstall_is_idempotent() {
        let once = install_section(Some(USER_CONFIG)).unwrap();
        let twice = install_section(Some(&once)).unwrap();
        assert_eq!(once, twice);
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
    fn appends_a_block_when_config_has_no_keybinds() {
        let out = install_section(Some("theme \"x\"\n")).unwrap();
        assert_eq!(
            out,
            format!("theme \"x\"\n\nkeybinds {{\n{}}}\n", section())
        );
        let no_newline = install_section(Some("theme \"x\"")).unwrap();
        assert_eq!(no_newline, out);
        let blank_line = install_section(Some("theme \"x\"\n\n")).unwrap();
        assert_eq!(blank_line, out);
    }

    #[test]
    fn ignores_names_that_only_start_with_keybinds() {
        let out = install_section(Some("keybinds_extra true\n")).unwrap();
        assert!(out.ends_with(&format!("\n\nkeybinds {{\n{}}}\n", section())));
    }

    #[test]
    fn refuses_a_single_line_keybinds_block() {
        let err = install_section(Some("keybinds { normal { } }\n")).unwrap_err();
        assert!(err.to_string().contains("add the nibble section by hand"));
    }

    #[test]
    fn remove_restores_the_original_config() {
        let installed = install_section(Some(USER_CONFIG)).unwrap();
        assert_eq!(remove_section(&installed).unwrap(), USER_CONFIG);
    }

    #[test]
    fn remove_without_section_or_end_marker_is_none() {
        assert!(remove_section(USER_CONFIG).is_none());
        assert!(remove_section(&format!("{BEGIN_MARKER}\nbind x\n")).is_none());
    }

    #[test]
    fn remove_keeps_a_missing_trailing_newline_missing() {
        let body = format!("a\n{BEGIN_MARKER}\n{END_MARKER}\nb");
        assert_eq!(remove_section(&body).unwrap(), "a\nb");
    }
}
