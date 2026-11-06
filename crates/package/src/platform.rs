//! The platform name of this host in the catalog and index style.

// Rust guideline compliant 2026-11-06

/// Operating system names of [`std::env::consts::OS`] the platform names cover.
const OS_LINUX: &str = "linux";
const OS_MACOS: &str = "macos";

/// Vendor and system part of a Linux platform name.
const LINUX_PLATFORM_SYSTEM: &str = "unknown-linux";

/// C library part of a Linux platform name.
const LINUX_GLIBC: &str = "gnu";
const LINUX_MUSL: &str = "musl";

/// Vendor and system part of a macOS platform name.
const MACOS_PLATFORM_SYSTEM: &str = "apple-darwin";

/// The platform name of this host in the catalog's target-triple style, such
/// as `x86_64-unknown-linux-gnu`, or `None` on a platform no catalog or index
/// names.
#[must_use]
pub fn host_platform() -> Option<String> {
    platform_name(
        std::env::consts::ARCH,
        std::env::consts::OS,
        cfg!(target_env = "musl"),
    )
}

/// Builds the platform name from the architecture and operating system names
/// of [`std::env::consts`] and whether the C library is musl.
fn platform_name(arch: &str, os: &str, musl: bool) -> Option<String> {
    match os {
        OS_LINUX => {
            let libc = if musl { LINUX_MUSL } else { LINUX_GLIBC };
            Some(format!("{arch}-{LINUX_PLATFORM_SYSTEM}-{libc}"))
        }
        OS_MACOS => Some(format!("{arch}-{MACOS_PLATFORM_SYSTEM}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_names_follow_the_catalog_style() {
        assert_eq!(
            platform_name("x86_64", "linux", false).as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            platform_name("x86_64", "linux", true).as_deref(),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            platform_name("aarch64", "linux", false).as_deref(),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            platform_name("aarch64", "macos", false).as_deref(),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(platform_name("x86_64", "windows", false), None);
    }
}
