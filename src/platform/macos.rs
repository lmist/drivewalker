use serde::Serialize;
use std::process::Command;

#[derive(Debug, Serialize)]
pub struct OverviewCommand {
    pub command: String,
    pub output: String,
    pub success: bool,
}

/// Read-only APFS and local-snapshot context, without a filesystem tree walk.
pub fn overview() -> Vec<OverviewCommand> {
    [
        ("df", vec!["-h", "/", "/System/Volumes/Data"]),
        ("diskutil", vec!["apfs", "list"]),
        ("tmutil", vec!["listlocalsnapshots", "/"]),
    ]
    .into_iter()
    .map(|(command, args)| {
        let result = Command::new(command).args(&args).output();
        let (success, output) = match result {
            Ok(output) => (
                output.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            ),
            Err(error) => (false, error.to_string()),
        };
        OverviewCommand {
            command: format!("{command} {}", args.join(" ")),
            output,
            success,
        }
    })
    .collect()
}
