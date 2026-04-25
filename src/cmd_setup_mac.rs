//! `nperf setup` (macOS only): ad-hoc-codesigns this nperf binary with the
//! `com.apple.security.cs.debugger` entitlement, which is what `task_for_pid`
//! requires for non-hardened processes when not running as root.
//!
//! Adapted from samply/src/mac/codesign_setup.rs (commit
//! 1920bd32c569de5650d1129eb035f43bd28ace27). MIT OR Apache-2.0.

use std::env;
use std::error::Error;
use std::io::{self, Write};
use std::process::Command;

use crate::args;

const ENTITLEMENTS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.cs.debugger</key>
	<true/>
</dict>
</plist>
"#;

pub fn main(args: args::SetupArgs) -> Result<(), Box<dyn Error>> {
    let exe = env::current_exe()?;

    if !args.yes {
        println!(
            r#"
On macOS, attaching to an existing process via task_for_pid requires the
com.apple.security.cs.debugger entitlement. This subcommand will ad-hoc
codesign your nperf binary with that entitlement (signed for your local
machine only -- not redistributable). The following command will run:

    codesign --force --options runtime --sign - \
      --entitlements <tempfile> {}

The entitlements file contains:
{}
Press Enter to continue, or Ctrl-C to cancel."#,
            exe.display(),
            ENTITLEMENTS_XML
        );

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
    }

    // Stage the entitlements XML to a temp file. We avoid a tempfile crate
    // dep by using std::env::temp_dir() and a pid-based filename.
    let mut entitlements_path = env::temp_dir();
    entitlements_path.push(format!("nperf-entitlements-{}.xml", std::process::id()));
    {
        let mut f = std::fs::File::create(&entitlements_path)?;
        f.write_all(ENTITLEMENTS_XML.as_bytes())?;
        f.flush()?;
    }

    let status = Command::new("codesign")
        .arg("--force")
        .arg("--options")
        .arg("runtime")
        .arg("--sign")
        .arg("-")
        .arg("--entitlements")
        .arg(&entitlements_path)
        .arg(&exe)
        .status()?;

    let _ = std::fs::remove_file(&entitlements_path);

    if !status.success() {
        return Err(format!("codesign exited with {}", status).into());
    }

    println!("Code signing successful: {}", exe.display());
    println!(
        "You can now run `nperf record --pid <PID> ...` against most user processes \
         without sudo. Hardened-runtime apps (App Store / system) still need root."
    );
    Ok(())
}
