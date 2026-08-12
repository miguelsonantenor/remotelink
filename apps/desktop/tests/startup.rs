#[path = "../src/startup.rs"]
mod startup;

use std::path::PathBuf;

#[test]
fn run_command_quotes_exe_and_adds_flag() {
    let p = PathBuf::from(r"C:\Program Files\RemoteLink\remotelink-app.exe");
    assert_eq!(
        startup::run_command_line(&p),
        r#""C:\Program Files\RemoteLink\remotelink-app.exe" --autostart"#
    );
}
