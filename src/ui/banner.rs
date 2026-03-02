//! ASCII banner printing used at program startup.

use colored::*;

pub fn print_banner() {
    println!(
        "{}",
        r#"
    ╔═══════════════════════════════════╗
    ║   TransferPlan - Rust v2.1        ║
    ║    (copy_file_range Edition)      ║
    ╚═══════════════════════════════════╝
    "#
            .cyan()
            .bold()
    );
}
