//! Bundle already-built main/helper executables with the pinned CEF runtime.
#[allow(dead_code)]
#[path = "../brand.rs"]
mod brand;

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let release = std::env::args().any(|arg| arg == "--release");
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| root.join("target"));
        let profile = if release { "release" } else { "debug" };
        const IDENTIFIER: &str = brand::BUNDLE_ID;
        let info = cef::build_util::mac::BundleInfo::new(
            brand::PRODUCT_NAME,
            brand::BUNDLE_ID,
            brand::PRODUCT_NAME,
            "en",
            env!("CARGO_PKG_VERSION").parse()?,
        );
        let output = target.join("bundle");
        std::fs::create_dir_all(&output)?;
        let staging = output.join(format!(".staging-{}", uuid::Uuid::new_v4()));
        let path = cef::build_util::mac::bundle(
            &staging,
            &target.join(profile),
            brand::EXECUTABLE,
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
        let cef_root = cef::sys::get_cef_dir()
            .or_else(|| std::env::var_os("CEF_PATH").map(std::path::PathBuf::from))
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
        let plist = path.join("Contents/Info.plist");
        for (key, what) in [
            ("NSCameraUsageDescription", "camera"),
            ("NSMicrophoneUsageDescription", "microphone"),
            ("NSBluetoothAlwaysUsageDescription", "Bluetooth devices"),
            ("NSWebBrowserPublicKeyCredentialUsageDescription", "passkeys"),
        ] {
            let text = format!("Websites you open in {} can ask to use your {what}.", brand::PRODUCT_NAME);
            let status = std::process::Command::new("/usr/bin/plutil")
                .args(["-replace", key, "-string", &text])
                .arg(&plist)
                .status()?;
            anyhow::ensure!(status.success(), "Could not set {key} in Info.plist");
        }
        // Lets macOS offer Atrium as the default browser and open web links and HTML files with it.
        let info_plist = path.join("Contents/Info.plist");
        for (key, value) in [
            (
                "CFBundleURLTypes",
                r#"[{"CFBundleURLName":"Web site URL","CFBundleTypeRole":"Viewer","CFBundleURLSchemes":["http","https"]}]"#,
            ),
            (
                "CFBundleDocumentTypes",
                r#"[{"CFBundleTypeName":"HTML document","CFBundleTypeRole":"Viewer","LSHandlerRank":"Alternate","LSItemContentTypes":["public.html","public.xhtml"],"CFBundleTypeExtensions":["html","htm","xhtml"]}]"#,
            ),
        ] {
            let _ = std::process::Command::new("/usr/bin/plutil")
                .args(["-remove", key])
                .arg(&info_plist)
                .output();
            let status = std::process::Command::new("/usr/bin/plutil")
                .args(["-insert", key, "-json", value])
                .arg(&info_plist)
                .status()?;
            anyhow::ensure!(status.success(), "Could not add {key} to Info.plist");
        }
        // Uses ATRIUM_SIGN_IDENTITY when set, otherwise signs ad hoc, and verifies the result.
        let status = std::process::Command::new(root.join("tools/sign-app.sh"))
            .arg(&path)
            .status()?;
        anyhow::ensure!(status.success(), "App signing or signature verification failed");
        let final_path = output.join(format!("{}.app", brand::EXECUTABLE));
        let backup = output.join(format!(".previous-{}.app", brand::EXECUTABLE));
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
