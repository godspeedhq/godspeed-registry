//! Where things live in the buckets. The layout is `docs/publishing-design.md` section 2 in the CLI
//! repository; this is the one place it is spelled.

use godspeed_package::{Arch, PackageName, Version, CONTRACT_FILE};

/// The manifest's file name in a package and in storage.
pub const MANIFEST_FILE: &str = "manifest.toml";

/// An immutable published file: `packages/<publisher>/<name>/<version>/<file>`, in the public bucket.
pub fn published(name: &PackageName<'_>, version: Version, file: &str) -> String {
    format!("packages/{}/{}/{version}/{file}", name.publisher(), name.name())
}

/// A package's index: `index/<publisher>/<name>.toml`, in the public bucket.
pub fn index(name: &PackageName<'_>) -> String {
    format!("index/{}/{}.toml", name.publisher(), name.name())
}

/// A file of an upload in progress, in the private staging bucket.
pub fn staged(name: &PackageName<'_>, version: Version, upload: &str, file: &str) -> String {
    format!("{}/{}/{version}/{upload}/{file}", name.publisher(), name.name())
}

/// Every file a package may contain other than its manifest, which is never uploaded separately: it
/// arrives with the reservation. A request naming anything else is refused before any bytes are read.
pub fn is_package_file(file: &str) -> bool {
    file == CONTRACT_FILE || Arch::ALL.iter().any(|a| a.artifact_file() == file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let n = PackageName::parse("alice/hello").unwrap();
        let v = Version::new(1, 0, 0);
        assert_eq!(published(&n, v, "x86_64.elf"), "packages/alice/hello/1.0.0/x86_64.elf");
        assert_eq!(index(&n), "index/alice/hello.toml");
        assert_eq!(
            staged(&n, v, "u1", "contract.toml"),
            "alice/hello/1.0.0/u1/contract.toml"
        );
    }

    #[test]
    fn only_package_files_are_accepted() {
        assert!(is_package_file("contract.toml"));
        assert!(is_package_file("riscv64.elf"));
        for bad in [
            "manifest.toml",
            "../index/alice/hello.toml",
            "x86_64.ELF",
            "notes.txt",
            "",
        ] {
            assert!(!is_package_file(bad), "{bad}");
        }
    }
}
