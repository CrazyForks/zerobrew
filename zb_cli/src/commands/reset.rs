use console::style;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::init::{InitError, run_init};

pub fn execute(root: &Path, prefix: &Path, yes: bool) -> Result<(), zb_core::Error> {
    if !root.exists() && !prefix.exists() {
        println!("Nothing to reset - directories do not exist.");
        return Ok(());
    }

    if !yes {
        println!(
            "{} This will delete all zerobrew data at:",
            style("Warning:").yellow().bold()
        );
        println!("      • {}", root.display());
        println!("      • {}", prefix.display());
        print!("Continue? [y/N] ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    for dir in [root, prefix] {
        if !dir.exists() {
            continue;
        }

        println!(
            "{} Clearing {}...",
            style("==>").cyan().bold(),
            dir.display()
        );

        // Instead of removing the directory entirely (which would require sudo to recreate),
        // just remove its contents. This avoids needing sudo when run_init recreates subdirs.
        let mut failed = false;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let result = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if result.is_err() {
                    failed = true;
                    break;
                }
            }
        } else {
            failed = true;
        }

        // Only fall back to sudo if we couldn't clear contents AND stdout is a terminal
        if failed {
            if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                eprintln!(
                    "{} Failed to clear {} (permission denied, non-interactive mode)",
                    style("error:").red().bold(),
                    dir.display()
                );
                std::process::exit(1);
            }

            // Interactive mode: fall back to sudo for the entire directory
            let status = Command::new("sudo")
                .args(["rm", "-rf", &dir.to_string_lossy()])
                .status();

            if status.is_err() || !status.unwrap().success() {
                eprintln!(
                    "{} Failed to remove {}",
                    style("error:").red().bold(),
                    dir.display()
                );
                std::process::exit(1);
            }
        }
    }

    // Pass false for no_modify_shell since this is a re-initialization
    run_init(root, prefix, false).map_err(|e| match e {
        InitError::Message(msg) => zb_core::Error::StoreCorruption { message: msg },
    })?;

    println!(
        "{} Reset complete. Ready for cold install.",
        style("==>").cyan().bold()
    );

    Ok(())
}
