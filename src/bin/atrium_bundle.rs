//! Bundle already-built main/helper executables with the pinned CEF runtime.
fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let release = std::env::args().any(|arg| arg == "--release");
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| root.join("target"));
        let profile = if release { "release" } else { "debug" };
        const IDENTIFIER: &str = "dev.atrium-rs.browser";
        let info = cef::build_util::mac::BundleInfo::new(
            "atrium",
            IDENTIFIER,
            "Atrium",
            "en",
            env!("CARGO_PKG_VERSION").parse()?,
        );
        let output = target.join("bundle");
        std::fs::create_dir_all(&output)?;
        let staging = output.join(format!(".staging-{}", uuid::Uuid::new_v4()));
        let path = cef::build_util::mac::bundle(
            &staging,
            &target.join(profile),
            "atrium",
            "atrium_helper",
            None,
            info,
        )?;
        let plutil = |args: &[&str], plist: &std::path::Path| -> anyhow::Result<()> {
            let status = std::process::Command::new("/usr/bin/plutil")
                .args(args)
                .arg(plist)
                .status()?;
            anyhow::ensure!(status.success(), "plutil {args:?} failed for {}", plist.display());
            Ok(())
        };
        // Every helper needs its own identifier; the bundler copies the main app's.
        for (helper, suffix) in [
            ("atrium Helper.app", "helper"),
            ("atrium Helper (Renderer).app", "helper.renderer"),
            ("atrium Helper (GPU).app", "helper.gpu"),
            ("atrium Helper (Plugin).app", "helper.plugin"),
            ("atrium Helper (Alerts).app", "helper.alerts"),
        ] {
            let plist = path.join("Contents/Frameworks").join(helper).join("Contents/Info.plist");
            plutil(&["-replace", "CFBundleIdentifier", "-string", &format!("{IDENTIFIER}.{suffix}")], &plist)?;
        }
        let resources = path.join("Contents/Resources");
        std::fs::create_dir_all(&resources)?;
        std::fs::write(
            resources.join("crash_reporter.cfg"),
            include_str!("../../assets/crash_reporter.cfg"),
        )?;
        let cef_root = std::env::var_os("CEF_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| target.join("cef"));
        std::fs::copy(
            cef_root.join("CREDITS.html"),
            resources.join("Chromium-CREDITS.html"),
        )?;
        std::fs::write(
            resources.join("THIRD-PARTY-NOTICES.txt"),
            include_str!("../../THIRD-PARTY-NOTICES.txt"),
        )?;
        std::fs::write(
            resources.join("CEF-LICENSE.txt"),
            include_str!("../../docs/licenses/CEF-LICENSE.txt"),
        )?;
        std::fs::write(
            resources.join("Chromium-LICENSE.txt"),
            include_str!("../../docs/licenses/Chromium-LICENSE.txt"),
        )?;
        // Uses ATRIUM_SIGN_IDENTITY when set, otherwise signs ad hoc, and verifies the result.
        let status = std::process::Command::new(root.join("tools/sign-app.sh"))
            .arg(&path)
            .status()?;
        anyhow::ensure!(status.success(), "App signing or signature verification failed");
        let final_path = output.join("atrium.app");
        let backup = output.join(".previous-atrium.app");
        if backup.exists() {
            std::fs::remove_dir_all(&backup)?;
        }
        if final_path.exists() {
            std::fs::rename(&final_path, &backup)?;
        }
        if let Err(error) = std::fs::rename(&path, &final_path) {
            if backup.exists() {
                let _ = std::fs::rename(&backup, &final_path);
            }
            return Err(error.into());
        }
        let _ = std::fs::remove_dir(&staging);
        std::fs::write(output.join("profile.txt"), profile)?;
        println!("{}", final_path.display());
    }
    Ok(())
}
