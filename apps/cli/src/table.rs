//! The terminal table of a report.
//!
//! Sizes use binary units. The table lists the first `top` children of each
//! root, while the report records every child.

use std::io::{self, Write};

use suds_core::development::{CheckoutKind, DevelopmentReport, StoreClass, StoreStatus};
use suds_core::model::{EntryKind, Report, RootResult, RootStatus};

const NOTE: &str = "Each allocated size estimates the disk space of an entry's blocks. \
On APFS, clones, compression, snapshots, and purgeable storage can make the recoverable space \
differ from the allocated size.";

pub fn write(out: &mut impl Write, report: &Report, top: usize) -> io::Result<()> {
    let mut rows = vec![row(&[
        "ROOT",
        "KIND",
        "ALLOCATED",
        "LOGICAL",
        "FILES",
        "DIRECTORIES",
        "SYMLINKS",
        "STATUS",
        "ELAPSED",
    ])];
    rows.extend(report.roots.iter().map(summary_row));
    write_rows(
        out,
        "",
        &rows,
        &[false, false, true, true, true, true, true, false, true],
    )?;

    for root in &report.roots {
        let measured = matches!(root.status, RootStatus::Complete | RootStatus::Partial);
        if !measured && root.errors.is_empty() {
            continue;
        }
        writeln!(out)?;
        writeln!(out, "{}  {}", root.id, escape(&root.path))?;
        if measured {
            write_children(out, root, top)?;
        }
        write_problems(out, root)?;
    }

    writeln!(out)?;
    writeln!(out, "{NOTE}")?;
    Ok(())
}

/// Writes a development report: the total of each class, then each checkout
/// with its build state, then each store.
pub fn write_development(out: &mut impl Write, report: &DevelopmentReport) -> io::Result<()> {
    let totals = &report.totals;
    let mut rows = vec![row(&["CLASS", "COUNT", "ALLOCATED", "LOGICAL"])];
    for (label, total) in [
        ("main checkouts", totals.main_checkouts),
        ("linked checkouts", totals.linked_checkouts),
        ("build state", totals.build_state),
        ("shared stores", totals.shared_stores),
        ("toolchains", totals.toolchains),
    ] {
        rows.push(vec![
            label.to_owned(),
            total.count.to_string(),
            format_bytes(total.allocated_bytes),
            format_bytes(total.logical_bytes),
        ]);
    }
    write_rows(out, "", &rows, &[false, true, true, true])?;

    if !report.checkouts.is_empty() {
        writeln!(out)?;
        let mut rows = vec![row(&["CHECKOUT", "KIND", "OWN", "BUILD STATE", "PATH"])];
        for checkout in &report.checkouts {
            let build: u64 = checkout
                .build_state
                .iter()
                .map(|state| state.measure.allocated_bytes)
                .sum();
            let kind = match checkout.kind {
                CheckoutKind::Main => "main",
                CheckoutKind::Linked => "linked",
            };
            let name = checkout.path.rsplit('/').next().unwrap_or(&checkout.path);
            rows.push(vec![
                escape(name),
                kind.to_owned(),
                format_bytes(checkout.own.allocated_bytes),
                format_bytes(build),
                escape(&checkout.path),
            ]);
        }
        write_rows(out, "", &rows, &[false, false, true, true, false])?;
    }

    if !report.stores.is_empty() {
        writeln!(out)?;
        let mut rows = vec![row(&["STORE", "CLASS", "ALLOCATED", "STATUS", "PATH"])];
        for store in &report.stores {
            let class = match store.class {
                StoreClass::SharedStore => "shared store",
                StoreClass::Toolchain => "toolchain",
            };
            let (status, size) = match store.status {
                StoreStatus::Present => ("present", format_bytes(store.measure.allocated_bytes)),
                StoreStatus::Unrecognized => {
                    ("unrecognized", format_bytes(store.measure.allocated_bytes))
                }
                StoreStatus::Absent => ("absent", "-".to_owned()),
                StoreStatus::Unreadable => ("unreadable", "-".to_owned()),
            };
            rows.push(vec![
                store.id.clone(),
                class.to_owned(),
                size,
                status.to_owned(),
                escape(&store.path),
            ]);
        }
        write_rows(out, "", &rows, &[false, false, true, false, false])?;
    }

    if report.error_count > 0 {
        writeln!(out)?;
        writeln!(
            out,
            "Suds could not read {} entries, so the sizes are lower bounds.",
            report.error_count
        )?;
        for error in &report.errors {
            writeln!(out, "  {}: {}", escape(&error.path), escape(&error.message))?;
        }
    }
    writeln!(out)?;
    writeln!(
        out,
        "A file with several links counts once, toward the first class that holds it. {NOTE}"
    )
}

fn summary_row(root: &RootResult) -> Vec<String> {
    let measured = matches!(root.status, RootStatus::Complete | RootStatus::Partial);
    let value = |text: String| if measured { text } else { "-".to_owned() };
    vec![
        root.id.clone(),
        root.kind.label().to_owned(),
        value(format_bytes(root.measure.allocated_bytes)),
        value(format_bytes(root.measure.logical_bytes)),
        value(root.measure.files.to_string()),
        value(root.measure.directories.to_string()),
        value(root.measure.symlinks.to_string()),
        root.status.label().to_owned(),
        format_elapsed(root.elapsed_ms),
    ]
}

fn write_children(out: &mut impl Write, root: &RootResult, top: usize) -> io::Result<()> {
    if root.children.is_empty() {
        return writeln!(out, "  No entries.");
    }
    let mut rows = vec![row(&[
        "NAME",
        "TYPE",
        "ALLOCATED",
        "LOGICAL",
        "FILES",
        "DIRECTORIES",
    ])];
    rows.extend(root.children.iter().take(top).map(|child| {
        vec![
            escape(&child.name),
            entry_label(child.kind).to_owned(),
            format_bytes(child.measure.allocated_bytes),
            format_bytes(child.measure.logical_bytes),
            child.measure.files.to_string(),
            child.measure.directories.to_string(),
        ]
    }));
    write_rows(out, "  ", &rows, &[false, false, true, true, true, true])?;
    let hidden = root.children.len().saturating_sub(top);
    if hidden > 0 {
        writeln!(
            out,
            "  Omitted entries: {hidden}. The JSON report lists every entry."
        )?;
    }
    Ok(())
}

fn write_problems(out: &mut impl Write, root: &RootResult) -> io::Result<()> {
    if root.skipped_mount_count > 0 {
        writeln!(
            out,
            "  Mount points left unscanned: {}",
            root.skipped_mount_count
        )?;
        for path in &root.skipped_mounts {
            writeln!(out, "    {}", escape(path))?;
        }
    }
    if root.error_count > 0 {
        writeln!(out, "  Traversal errors: {}", root.error_count)?;
        for error in &root.errors {
            writeln!(
                out,
                "    {}: {}",
                escape(&error.path),
                escape(&error.message)
            )?;
        }
    }
    Ok(())
}

pub fn entry_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Directory => "directory",
        EntryKind::File => "file",
        EntryKind::Symlink => "symlink",
        EntryKind::Other => "other",
    }
}

/// Escapes each control character in a filesystem name or message, so that
/// a name cannot move the cursor, clear the terminal, or begin a new row. The
/// JSON report keeps the original text.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|&cell| cell.to_owned()).collect()
}

/// Writes rows as aligned columns. The first row is the header.
fn write_rows(
    out: &mut impl Write,
    indent: &str,
    rows: &[Vec<String>],
    right: &[bool],
) -> io::Result<()> {
    let mut widths = vec![0; right.len()];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for row in rows {
        let mut line = String::from(indent);
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            let pad = widths[index].saturating_sub(cell.chars().count());
            if right[index] {
                line.extend(std::iter::repeat_n(' ', pad));
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.extend(std::iter::repeat_n(' ', pad));
            }
        }
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(())
}

/// Formats a byte count in binary units, with one decimal place for sizes
/// above 1 KiB.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    // Converting the byte count to a float loses precision only beyond 2^53
    // bytes, and the resulting error falls far below the first decimal place.
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn format_elapsed(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds} ms")
    } else {
        format!("{:.1} s", milliseconds as f64 / 1_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{escape, format_bytes};

    #[test]
    fn escapes_control_characters() {
        assert_eq!(escape("plain name"), "plain name");
        assert_eq!(escape("a\u{1b}[2J\nb\tc"), "a\\u{1b}[2J\\nb\\tc");
        assert_eq!(escape("caf\u{e9}"), "caf\u{e9}");
    }

    #[test]
    fn uses_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(format_bytes(u64::MAX), "16.0 EiB");
    }
}
